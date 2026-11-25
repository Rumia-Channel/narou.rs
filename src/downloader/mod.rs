pub(crate) mod auth;
pub(crate) mod author_listing;
pub mod html;
pub mod http_policy;
#[cfg(feature = "native-runtime")]
pub mod narou_api;
pub mod novel_info;
pub mod persistence;
pub mod preprocess;
#[cfg(feature = "native-runtime")]
pub mod rate_limit;
pub mod section;
pub mod security;
pub mod settings;
pub mod site_setting;
pub mod toc;
pub mod types;
pub mod util;

use std::collections::HashMap;
#[cfg(feature = "native-runtime")]
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{
    DateTime, Datelike, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc,
};
use chrono_tz::Tz;
use regex::Regex;

use self::auth::AuthState;
use self::novel_info::NovelInfo;
use self::persistence::section_filename;
use self::persistence::{PersistenceService, SectionLoadFailure, compute_section_hash};
use crate::application::novel_access::{AccessOrigin, NovelAccess, NovelWriteGuard};

use crate::platform::{CookieStore, LoginGroup};
use self::section::{SectionCache, download_section};
use self::security::is_safe_public_url_syntax;
use self::settings::DownloaderSettings;
use self::site_setting::{WorkTypeSubjects, SiteSetting};
use self::toc::{
    create_short_story_subtitles, fetch_toc, parse_subtitles, parse_subtitles_multipage,
};
use self::util::{
    build_section_url, compile_html_pattern, load_length_limit, mask_spoiler_text,
    parse_two_digit_year_datetime, ruby_filename_special_chars, sanitize_filename_with_limit,
};
use crate::application::messages;
use crate::db::novel_record::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, Clock, HttpClient, NovelId, NovelIdReservation, NovelMutation, NovelObjectKeys,
    NovelRepository, ObjectStore, ProgressReporter, RateLimiter,
};

pub use self::types::{
    ARCHIVE_ROOT_DIR, DownloadResult, NarouApiEntry, RAW_DATA_DIR, SECTION_SAVE_DIR,
    SectionElement, SectionFile, SubtitleInfo, TargetType, TocFile, TocObject, UpdateStatus,
};
pub use self::util::pretreatment_source;

#[cfg(feature = "native-runtime")]
pub(crate) const SECTION_HASH_CACHE_NAME: &str = "section_hash_cache";
const DEFAULT_SITE_TIMEZONE: &str = "Asia/Tokyo";

/// A platform-neutral budget checked before sections and uncached images.
///
/// The index is the first unfinished section, including a section whose body
/// is saved but whose images are still being fetched. Implementations must be
/// cheap; the same index may be checked repeatedly while its images advance.
pub trait SectionBudget: Send {
    /// Worker implementations allocate and checkpoint under one live job-token
    /// transaction. Native allocations retain their owned liveness resource.
    fn allocate_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository) -> crate::platform::PlatformFuture<'a, Result<(NovelIdReservation, bool)>> {
        Box::pin(async move { Ok((novels.allocate_id().await?, false)) })
    }

    /// Rehydrate the exact saved reservation and its owned liveness resource.
    /// Worker implementations bind the current execution owner atomically;
    /// missing or changed proof must fail without allocating a replacement.
    fn restore_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository, id: NovelId, generation: i64) -> crate::platform::PlatformFuture<'a, Result<NovelIdReservation>> {
        novels.resume_id(id, generation)
    }

    /// Runtime-only invocation capability. Native budgets have no job owner.
    fn job_owner(&self) -> Option<Arc<crate::platform::JobExecutionOwner>> { None }

    /// Atomically checkpoint the original epoch and exact create reservation.
    /// True transfers release responsibility to the durable checkpoint owner;
    /// false explicitly denotes a non-durable budget. An error is ambiguous.
    fn set_access_origin<'a>(&'a mut self, _origin: &'a AccessOrigin, _creation_reservation: Option<(NovelId, i64)>) -> crate::platform::PlatformFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn should_yield(&mut self, next_section_index: usize) -> bool;

    /// Check execution ownership at a safe boundary. Native budgets do not
    /// need an external cancellation store.
    fn check_active(&mut self) -> crate::platform::PlatformFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Execution options for a resumable download.
///
/// `resume_from_section` identifies the first section that still needs
/// network work. Completed metadata is restored without rereading bodies;
/// older checkpoints and interrupted checkpoint writes fall back to storage.
pub struct DownloadExecutionOptions<'a> {
    pub force: bool,
    pub budget: Option<&'a mut dyn SectionBudget>,
    pub resume_from_section: Option<usize>,
    /// Original writer token. A canonical-work resume must provide it; a planned
    /// numeric-only creation has not captured its first prefix/epoch yet.
    pub access: Option<NovelWriteGuard>,
    /// Exact durable new-work ID/generation. A resumed creation never allocates
    /// another reservation; the checkpoint owner releases it on terminal exit.
    /// Without an origin, only an early creation with no cursor and no existing
    /// record is admitted. The durable owner validates that planned state.
    pub creation_reservation: Option<(NovelId, i64)>,
}

struct DownloadCreation {
    reservation: NovelIdReservation,
    checkpoint_owned: bool,
    committed: bool,
}

struct DownloadDiscovery {
    url_captures: HashMap<String, String>,
    toc_url: String,
    response: Result<(String, Option<crate::platform::HttpResponse>)>,
    policy: crate::downloader::http_policy::FetchPolicy,
}

/// Whether a release failure only means "the cleanup is still pending".
///
/// An unpublished creation keeps its own namespace gated to its removal
/// owner until the native startup collector finishes the cleanup. That state
/// is durable and owned by the collector, so it never fails the operation
/// that released the reservation.
fn deferred_creation_cleanup(error: &NarouError) -> bool {
    match error {
        NarouError::CreationCleanupPending => true,
        NarouError::Shared(inner) => deferred_creation_cleanup(inner),
        _ => false,
    }
}

async fn checkpoint_download_access(
    budget: &mut Option<&mut dyn SectionBudget>,
    origin: &AccessOrigin,
    creation: Option<&mut DownloadCreation>,
) -> Result<()> {
    let Some(budget) = budget.as_deref_mut() else { return Ok(()); };
    let pair = creation.as_ref().map(|creation| (creation.reservation.id(), creation.reservation.generation()));
    let previous_owner = creation.as_ref().is_some_and(|creation| creation.checkpoint_owned);
    let mut creation = creation;
    // A lost checkpoint response may still have committed ownership. Retain
    // the exact token on error; only an explicit false ACK permits release.
    if let Some(creation) = &mut creation { creation.checkpoint_owned = true; }
    let owned = budget.set_access_origin(origin, pair).await?;
    if let Some(creation) = creation { creation.checkpoint_owned = previous_owner || owned; }
    Ok(())
}

/// Platform services every [`Downloader`] constructor needs. Bundles the
/// injected ports so the constructors take them as one argument.
pub struct DownloaderPlatform {
    pub http: Arc<dyn HttpClient>,
    pub rate_limiter: Arc<dyn RateLimiter>,
    pub novels: Arc<dyn NovelRepository>,
    pub objects: Arc<dyn ObjectStore>,
    pub assets: Arc<dyn AssetStore>,
    pub clock: Arc<dyn Clock>,
}

/// What [`Downloader::process_digest`] needs to drive the interactive digest
/// prompt: the shrunk TOC counts plus the handles its actions touch.
struct DigestContext<'a> {
    write: &'a NovelWriteGuard,
    existing_id: Option<i64>,
    toc_url: &'a str,
    novel_dir: &'a Path,
    title: &'a str,
    latest_story: &'a str,
    old_count: usize,
    latest_count: usize,
}

/// One attempt at the table of contents: the first page, the metadata read
/// from it and the episode list of every page.
struct TocAttempt {
    /// The first page's source.
    source: String,
    /// Metadata read from the first page.
    info: NovelInfo,
    /// 1 = series, 2 = short story.
    novel_type: u8,
    /// Whether the work is finished.
    is_end: bool,
    /// The episodes of every page.
    listing: toc::TocListing,
}

/// One [`Downloader::section_needs_download`] probe: the latest and stored
/// subtitle rows plus the cache/state handles the decision consults.
struct SectionDecision<'a> {
    setting: &'a mut SiteSetting,
    /// The credentials the run may use; a body that needs authentication
    /// switches to the next stored login through it.
    auth: &'a mut AuthState,
    latest: &'a SubtitleInfo,
    old: Option<&'a SubtitleInfo>,
    existing_id: Option<i64>,
    keys: &'a NovelObjectKeys,
    toc_url: &'a str,
    strong_update: bool,
    timezone: SiteTimezone,
    existed: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum SiteTimezone {
    Named(Tz),
    Fixed(FixedOffset),
}

impl SiteTimezone {
    fn resolve_local_datetime(self, dt: NaiveDateTime) -> Option<DateTime<Utc>> {
        match self {
            Self::Named(tz) => match tz.from_local_datetime(&dt) {
                LocalResult::Single(local) | LocalResult::Ambiguous(local, _) => {
                    Some(local.with_timezone(&Utc))
                }
                LocalResult::None => None,
            },
            Self::Fixed(offset) => match offset.from_local_datetime(&dt) {
                LocalResult::Single(local) | LocalResult::Ambiguous(local, _) => {
                    Some(local.with_timezone(&Utc))
                }
                LocalResult::None => None,
            },
        }
    }

    fn ymd(self, dt: DateTime<Utc>) -> String {
        match self {
            Self::Named(tz) => {
                let local = dt.with_timezone(&tz);
                format!("{:04}{:02}{:02}", local.year(), local.month(), local.day())
            }
            Self::Fixed(offset) => {
                let local = dt.with_timezone(&offset);
                format!("{:04}{:02}{:02}", local.year(), local.month(), local.day())
            }
        }
    }
}

pub struct Downloader {
    http: Arc<dyn HttpClient>,
    rate_limiter: Arc<dyn RateLimiter>,
    novels: Arc<dyn NovelRepository>,
    assets: Arc<dyn AssetStore>,
    clock: Arc<dyn Clock>,
    persistence: PersistenceService,
    site_settings: Arc<Vec<SiteSetting>>,
    section_cache: SectionCache,
    section_hash_cache: HashMap<String, HashMap<String, String>>,
    section_hash_cache_dirty: bool,
    /// Stored login cookies. `None` keeps the downloader anonymous (tests and
    /// the Worker); the native constructors inject the inventory-backed store.
    cookies: Option<Arc<dyn CookieStore>>,
    settings: Arc<dyn DownloaderSettings>,
    progress: Option<Box<dyn ProgressReporter>>,
    /// ユーザーに見える行の送り先。`None` は従来動作 (native は
    /// `safe_println` / `safe_stderr_println`、Worker は tracing) に
    /// フォールバックする。`set_message_sink` で差し替える。
    message_sink: Option<Arc<dyn crate::application::messages::MessageSink>>,
    /// Results of URLs that `preprocess:` runs asked for. Lives for one novel
    /// download so repeated references (e.g. one illustration used by many
    /// sections of a series) resolve once.
    preprocess_jobs: super::downloader::preprocess::PreprocessJobs,
}

/// Fallback used when no message sink is installed (native: the original
/// `safe_println`; Worker: the trace log — no terminal on the platform).
#[cfg(feature = "native-runtime")]
fn default_report_line(line: &str) {
    crate::progress::safe_println(line);
}

#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
fn default_report_line(line: &str) {
    tracing::debug!("{line}");
}

/// Fallback used when no message sink is installed (native: the original
/// `safe_stderr_println`; Worker: the trace log).
#[cfg(feature = "native-runtime")]
fn default_report_warn(line: &str) {
    crate::progress::safe_stderr_println(line);
}

#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
fn default_report_warn(line: &str) {
    tracing::warn!("{line}");
}

/// Bold-colored text. Native renders terminal/Web colors; Worker keeps the
/// plain text so messages are not lost.
#[cfg(feature = "native-runtime")]
fn colorize(text: &str, color: &str) -> String {
    crate::termcolor::bold_colored(text, color)
}

#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
fn colorize(text: &str, _color: &str) -> String {
    text.to_string()
}

/// なろう ncode の合成 URL (新規ダウンロードの取得先)。ncode → syosetu URL の
/// 唯一の定義 — `ncode_target_url` と `resolve_target_for_download` の
/// 新規分岐が同じ組み立てを使う。
fn narou_ncode_url(ncode: &str) -> String {
    format!("https://ncode.syosetu.com/{}/", ncode.to_lowercase())
}

fn ncode_target_url(target: &str) -> Option<String> {
    matches!(Downloader::get_target_type(target), TargetType::Ncode)
        .then(|| narou_ncode_url(target))
}

fn story_changed(old_story: &Option<String>, fetched_story: &Option<String>) -> bool {
    match (old_story, fetched_story) {
        (None, None) => false,
        (Some(old), Some(new)) => {
            normalize_story_for_compare(old) != normalize_story_for_compare(new)
        }
        (None, Some(new)) => !normalize_story_for_compare(new).is_empty(),
        (Some(old), None) => !normalize_story_for_compare(old).is_empty(),
    }
}

fn normalize_story_for_compare(story: &str) -> String {
    let br = regex::Regex::new(r"(?i)<br\s*/?>").expect("valid br regex");
    let normalized = br.replace_all(story, "\n");
    normalized
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Over18AccessDecision {
    Allow,
    Prompt,
    Deny,
}

fn requires_over18_confirmation(setting: &SiteSetting, toc_source: &str) -> bool {
    setting.confirm_over18
        || setting
            .over18_pattern()
            .is_some_and(|pattern| pattern.is_match(toc_source))
}

fn over18_access_decision(
    setting: &SiteSetting,
    toc_source: &str,
    stored_over18: Option<bool>,
) -> Over18AccessDecision {
    if !requires_over18_confirmation(setting, toc_source) {
        return Over18AccessDecision::Allow;
    }

    match stored_over18 {
        Some(true) => Over18AccessDecision::Allow,
        Some(false) => Over18AccessDecision::Deny,
        None => Over18AccessDecision::Prompt,
    }
}

async fn toc_title_for_type_inference(
    setting: &SiteSetting,
    toc_source: &str,
    info: &NovelInfo,
) -> String {
    match &info.title {
        Some(title) => title.clone(),
        None => setting.resolve_info_pattern("t", toc_source).await.unwrap_or_default(),
    }
}

fn toc_subtitles_imply_series(
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
    title: &str,
) -> bool {
    let Ok(subtitles) = parse_subtitles(setting, toc_source, url_captures) else {
        return false;
    };
    if subtitles.len() > 1 {
        return true;
    }
    subtitles.into_iter().any(|subtitle| {
        !subtitle.href.is_empty()
            || !subtitle.chapter.is_empty()
            || !subtitle.subchapter.is_empty()
            || subtitle.subtitle != title
    })
}

async fn resolve_novel_type(
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
    info: &NovelInfo,
) -> (u8, bool) {
    let title = toc_title_for_type_inference(setting, toc_source, info).await;
    let toc_implies_series = toc_subtitles_imply_series(setting, toc_source, url_captures, &title);

    if let Some(nt) = info.novel_type {
        if nt == 2 && toc_implies_series {
            return (1u8, false);
        }
        return (nt, info.end.unwrap_or(false));
    }

    if let Some(text) = setting.resolve_info_pattern("nt", toc_source).await {
        let resolved = setting.get_novel_type_from_string(&text);
        if resolved.0 == 2 && toc_implies_series {
            return (1u8, false);
        }
        return resolved;
    }

    if toc_implies_series {
        return (1u8, false);
    }

    (1u8, false)
}

fn section_relative_path(subtitle: &SubtitleInfo) -> String {
    format!("{}/{}", types::SECTION_SAVE_DIR, section_filename(subtitle))
}

fn cache_timestamp(clock: &dyn Clock, save_diff_snapshots: bool) -> Option<String> {
    if !save_diff_snapshots {
        return None;
    }
    Some(
        clock
            .now_utc()
            .with_timezone(&chrono::Local)
            .format("%Y.%m.%d@%H.%M.%S")
            .to_string(),
    )
}

fn sections_latest_update_time_with_timezone(
    subtitles: &[SubtitleInfo],
    key: &str,
    subkey: Option<&str>,
    timezone: SiteTimezone,
) -> Option<DateTime<Utc>> {
    let mut latest: Option<DateTime<Utc>> = None;
    for subtitle in subtitles {
        let value = match key {
            "subupdate" => subtitle.subupdate.as_deref().unwrap_or_else(|| {
                if subkey == Some("subdate") {
                    subtitle.subdate.as_str()
                } else {
                    ""
                }
            }),
            _ => subtitle.subdate.as_str(),
        };
        let Some(parsed) = parse_loose_datetime_with_timezone(value, timezone) else {
            continue;
        };
        if latest.is_none_or(|current| parsed > current) {
            latest = Some(parsed);
        }
    }
    latest
}

pub(crate) fn normalize_narou_datetime(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut skipping_paren = false;

    for ch in value.trim().chars() {
        match ch {
            '(' | '（' => skipping_paren = true,
            ')' | '）' => skipping_paren = false,
            _ if skipping_paren => {}
            '年' => normalized.push('/'),
            '月' => normalized.push('/'),
            '日' => {}
            '時' => normalized.push(':'),
            '分' => normalized.push(':'),
            '秒' => {}
            '\u{00A0}' => normalized.push(' '),
            _ => normalized.push(ch),
        }
    }

    let separator_space = Regex::new(r"\s*([/:])\s*").expect("valid datetime separator regex");
    let normalized = separator_space.replace_all(&normalized, "$1");
    let whitespace = Regex::new(r"\s+").expect("valid whitespace regex");
    whitespace
        .replace_all(normalized.trim().trim_end_matches(':'), " ")
        .trim()
        .to_string()
}

fn parse_loose_datetime_with_timezone(
    value: &str,
    timezone: SiteTimezone,
) -> Option<DateTime<Utc>> {
    let value = normalize_narou_datetime(value);
    if value.is_empty() {
        return None;
    }

    if let Ok(ts) = value.parse::<i64>() {
        return DateTime::from_timestamp(ts, 0);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(&value) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = DateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S%.f %z") {
        return Some(dt.with_timezone(&Utc));
    }

    // 2 桁年 (ノベルアップ＋ の `24/5/29 7:36`) は `%Y` だと西暦 24 年として
    // 読まれるため、先に POSIX `%y` の規則 (Ruby の Date.parse と同じ) で読む。
    // 4 桁の年は `%y` に合致しないので、既存の解釈には触れない。
    if let Some(datetime) = parse_two_digit_year_datetime(&value, timezone) {
        return Some(datetime);
    }

    let formats = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
        "%Y/%m/%d",
    ];

    for fmt in &formats {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&value, fmt) {
            return timezone.resolve_local_datetime(dt);
        }
        if let Ok(date) = NaiveDate::parse_from_str(&value, fmt) {
            return date
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| timezone.resolve_local_datetime(dt));
        }
    }

    None
}

pub(crate) fn site_timezone(timezone: Option<&str>) -> SiteTimezone {
    let configured = timezone
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(local_timezone_setting)
        .unwrap_or_else(|| DEFAULT_SITE_TIMEZONE.to_string());
    parse_site_timezone(&configured).unwrap_or_else(default_site_timezone)
}

/// `time-zone` local setting override (`.narou/local_setting.yaml`).
#[cfg(feature = "native-runtime")]
fn local_timezone_setting() -> Option<String> {
    crate::compat::load_local_setting_string("time-zone")
}

/// Worker builds have no local settings; the site's own timezone default
/// applies.
#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
fn local_timezone_setting() -> Option<String> {
    None
}

fn default_site_timezone() -> SiteTimezone {
    SiteTimezone::Named(chrono_tz::Asia::Tokyo)
}

fn parse_site_timezone(value: &str) -> Option<SiteTimezone> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let upper = value.to_ascii_uppercase();
    let timezone_name = match upper.as_str() {
        "JST" | "ASIA/TOKYO/JST" => "Asia/Tokyo",
        "UTC" | "GMT" | "Z" => "UTC",
        _ => value,
    };
    if let Ok(tz) = timezone_name.parse::<Tz>() {
        return Some(SiteTimezone::Named(tz));
    }

    let (sign, rest) = match value.as_bytes().first().copied() {
        Some(b'+') => (1, &value[1..]),
        Some(b'-') => (-1, &value[1..]),
        _ => return None,
    };
    let compact = rest.replace(':', "");
    let (hours, minutes) = match compact.len() {
        2 => (compact.parse::<i32>().ok()?, 0),
        4 => (
            compact[..2].parse::<i32>().ok()?,
            compact[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60)).map(SiteTimezone::Fixed)
}

pub fn parse_datetime_with_timezone(value: &str, timezone: Option<&str>) -> Option<DateTime<Utc>> {
    parse_loose_datetime_with_timezone(value, site_timezone(timezone))
}

/// Resolve the CLI/saved user-agent, randomizing the value for `auto`/
/// `random`. Native と Worker のどちらも `ua_generator` のランダムな
/// Firefox UA を使う (保存値・明示指定はそのまま優先する)。
pub fn resolve_user_agent(user_agent: Option<&str>, saved_user_agent: Option<String>) -> String {
    let is_auto = |ua: &str| {
        let trimmed = ua.trim();
        trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("auto")
            || trimmed.eq_ignore_ascii_case("random")
    };
    let randomized = || ua_generator::ua::spoof_firefox_ua().to_string();
    match user_agent {
        Some(ua) if is_auto(ua) => randomized(),
        Some(ua) if !ua.trim().is_empty() => ua.to_string(),
        _ => match saved_user_agent {
            Some(ua) if is_auto(&ua) => randomized(),
            Some(ua) if !ua.trim().is_empty() => ua,
            _ => randomized(),
        },
    }
}

#[cfg(test)]
fn date_string_is_newer(latest: &str, old: &str) -> bool {
    date_string_is_newer_with_timezone(latest, old, site_timezone(None))
}

fn date_string_is_newer_with_timezone(latest: &str, old: &str, timezone: SiteTimezone) -> bool {
    match (
        parse_loose_datetime_with_timezone(latest, timezone),
        parse_loose_datetime_with_timezone(old, timezone),
    ) {
        (Some(latest_dt), Some(old_dt)) => latest_dt > old_dt,
        _ => latest > old,
    }
}

#[cfg(test)]
fn date_string_to_ymd(value: &str) -> Option<String> {
    date_string_to_ymd_with_timezone(value, site_timezone(None))
}

fn date_string_to_ymd_with_timezone(value: &str, timezone: SiteTimezone) -> Option<String> {
    let dt = parse_loose_datetime_with_timezone(value, timezone)?;
    Some(timezone.ymd(dt))
}

fn resolve_download_status(
    force: bool,
    updated_count: usize,
    existing_id: Option<i64>,
    title_changed: bool,
    author_changed: bool,
    story_changed: bool,
    sections_deleted: bool,
) -> types::UpdateStatus {
    let has_changes = force
        || updated_count > 0
        || existing_id.is_none()
        || title_changed
        || author_changed
        || story_changed
        || sections_deleted;

    if has_changes {
        types::UpdateStatus::Ok
    } else {
        types::UpdateStatus::None
    }
}

fn should_replace_last_update(status: types::UpdateStatus) -> bool {
    matches!(status, types::UpdateStatus::Ok)
}

fn merge_update_timestamps(
    updated: &mut NovelRecord,
    record: &NovelRecord,
    status: types::UpdateStatus,
) {
    if should_replace_last_update(status) {
        updated.last_update = record.last_update;
        updated.novelupdated_at = record.novelupdated_at.or(updated.novelupdated_at);
        updated.general_lastup = record.general_lastup.or(updated.general_lastup);
    }
}

fn sanitize_site_tags(raw: &str) -> Vec<String> {
    let cleaned = crate::downloader::html::sanitize_text(raw)
        .replace("キーワードが設定されていません", "")
        .replace("キーワード", "");
    let regex_meta = Regex::new(r#"\"?\(\?\.\+\?\)\"?|\(\?<?[^)]*\)"#).expect("valid regex");
    let cleaned = regex_meta.replace_all(&cleaned, "").to_string();
    cleaned
        .split_whitespace()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

/// Merge metadata and all TOC pages in first-seen order without duplicating
/// tag strings or discarding tags that appear only on a later page.
fn collect_site_tags(info_tags: Option<&str>, page_tags: &[String]) -> Vec<String> {
    let mut positions = HashMap::new();
    for source in info_tags.into_iter().chain(page_tags.iter().map(String::as_str)) {
        for tag in sanitize_site_tags(source) {
            if crate::application::attribute_tags::is_attribute_tag(&tag) {
                continue;
            }
            let position = positions.len();
            positions.entry(tag).or_insert(position);
        }
    }
    let mut tags: Vec<_> = positions.into_iter().collect();
    tags.sort_unstable_by_key(|(_, position)| *position);
    tags.into_iter().map(|(tag, _)| tag).collect()
}

fn section_timestamp_ymd(download_time: Option<&str>, timezone: SiteTimezone) -> Option<String> {
    download_time.and_then(|value| date_string_to_ymd_with_timezone(value, timezone))
}

/// narou.rb `HTML#initialize` の既定挿絵パターン。
///
/// サイト定義の `illust_grep_pattern` が `null` (または未定義) のとき、
/// narou.rb の `HTML#set_illust_setting` は上書きしないためこの既定が使われ、
/// 本文中の `<img src="...">` が挿絵注釈になる。取得自体は変換時に行われるが、
/// Rust 版はダウンロード時に前倒ししているので、同じ既定をここでも使う
/// (なろう / R18なろう / カクヨム / Arcadia が該当する)。
pub const DEFAULT_ILLUST_GREP_PATTERN: &str = r#"<img.+?src="(?<src>.+?)".*?>"#;

fn illustration_sources<'a>(
    section: &'a SectionElement,
    raw_html: Option<&'a str>,
) -> impl Iterator<Item = &'a str> {
    [
        section.body.as_str(),
        section.introduction.as_str(),
        section.postscript.as_str(),
    ]
    .into_iter()
    .chain(raw_html)
}

/// 同時に投げる挿絵 `stat` の件数。ストアへの同時要求を絞る (`stat` は
/// メタデータだけを返すので、画像本体の 64MiB 級 buffer は並行に抱えない)。
const ILLUSTRATION_VERIFY_WINDOW: usize = 8;

/// 挿絵の索引にあるファイル名を、保存側と同じキー生成で 1 枚ずつ確認する。
///
/// `stat` だけを使うので、ストアの一覧 (LIST) が prefix の解釈差などで空を
/// 返す環境でも実体を見つけられる。確認は [`ILLUSTRATION_VERIFY_WINDOW`] 件の
/// 並行 `stat` で行い、確認順はファイル名順のまま、失敗したときは最初の
/// 失敗だけを報告する。キー生成や確認に失敗した場合は `None` (呼び出し側は
/// 従来どおり索引だけで判断する)。
async fn verify_illustration_files(
    assets: &dyn AssetStore,
    object_keys: &NovelObjectKeys,
    filenames: &[String],
) -> Option<std::collections::HashSet<String>> {
    use futures::StreamExt;

    let keys: Vec<_> = filenames
        .iter()
        .map(|filename| object_keys.illustration(filename).ok())
        .collect::<Option<Vec<_>>>()?;

    let pending: Vec<_> = keys.iter().map(|key| assets.stat(key)).collect();
    let mut stats = Box::pin(
        futures::stream::iter(pending).buffered(ILLUSTRATION_VERIFY_WINDOW),
    );
    let mut stored = std::collections::HashSet::new();
    let mut index = 0usize;
    while let Some(result) = stats.next().await {
        match result {
            Ok(Some(_)) => {
                stored.insert(filenames[index].clone());
            }
            Ok(None) => {}
            Err(error) => {
                crate::application::debug::emit_error_async(format!(
                    "挿絵: 実体の確認に失敗 ({error})"
                )).await;
                return None;
            }
        }
        index += 1;
    }
    Some(stored)
}

impl Downloader {
    /// Native constructor: loads site definitions from the filesystem and the
    /// section hash cache from the Inventory. Worker builds use
    /// Inject the store that keeps login cookies for authenticated sites.
    pub fn with_cookie_store(mut self, cookies: Arc<dyn CookieStore>) -> Self {
        self.cookies = Some(cookies);
        self
    }

    /// Load the stored login cookie for `host`, when a store is available.
    /// そのサイトに保存されたログインを試行順で返す。
    async fn stored_groups_for(&self, setting: &SiteSetting) -> Vec<LoginGroup> {
        let Some(cookies) = self.cookies.as_ref() else {
            return Vec::new();
        };
        cookies
            .load_groups(&setting.domain)
            .await
            .unwrap_or_default()
    }

    /// [`Self::with_platform_and_storage_and_settings`] with bundled values.
    #[cfg(feature = "native-runtime")]
    pub fn with_platform_and_storage(platform: DownloaderPlatform) -> Result<Self> {
        let site_settings = crate::downloader::site_setting::effective_site_settings_shared();
        let settings = crate::downloader::settings::default_settings();
        let section_hash_cache = settings.load_section_hash_cache();
        Self::with_platform_and_storage_and_settings(platform, site_settings, section_hash_cache)
    }

    /// Build the downloader without loading site definitions or Inventory.
    ///
    /// Worker callers and filesystem-free tests supply both values explicitly.
    pub fn with_platform_and_storage_and_settings(
        platform: DownloaderPlatform,
        site_settings: impl Into<Arc<Vec<SiteSetting>>>,
        section_hash_cache: HashMap<String, HashMap<String, String>>,
    ) -> Result<Self> {
        Self::with_platform_and_storage_and_settings_and_support(
            platform,
            site_settings,
            section_hash_cache,
            crate::downloader::settings::default_settings(),
        )
    }

    /// Full constructor: additionally injects the settings/interactive
    /// capability (the platform default is used when building through the
    /// shorter constructor). Worker callers can supply bundled settings here
    /// without any filesystem access.
    pub fn with_platform_and_storage_and_settings_and_support(
        platform: DownloaderPlatform,
        site_settings: impl Into<Arc<Vec<SiteSetting>>>,
        section_hash_cache: HashMap<String, HashMap<String, String>>,
        settings: Arc<dyn DownloaderSettings>,
    ) -> Result<Self> {
        let DownloaderPlatform {
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            clock,
        } = platform;
        let persistence =
            PersistenceService::with_clock(objects.clone(), clock.clone());

        Ok(Self {
            http,
            rate_limiter,
            novels,
            assets,
            clock,
            persistence,
            site_settings: site_settings.into(),
            section_cache: SectionCache::new(),
            section_hash_cache,
            cookies: None,
            section_hash_cache_dirty: false,
            settings,
            progress: None,
            message_sink: None,
            preprocess_jobs: super::downloader::preprocess::PreprocessJobs::new(),
        })
    }

    pub fn get_target_type(target: &str) -> TargetType {
        if target.starts_with("http://") || target.starts_with("https://") {
            TargetType::Url
        } else if regex::Regex::new(r"(?i)^n\d+[a-z]+$")
            .unwrap()
            .is_match(target)
        {
            TargetType::Ncode
        } else if target.chars().all(|c| c.is_ascii_digit()) {
            TargetType::Id
        } else {
            TargetType::Other
        }
    }

    pub fn find_site_setting(&self, url: &str) -> Option<SiteSetting> {
        self.site_setting_for_url(url).cloned()
    }

    fn site_setting_for_url(&self, url: &str) -> Option<&SiteSetting> {
        self.site_settings.iter().find(|setting| setting.matches_url(url))
    }

    fn site_setting_for_stored_target(&self, record: &NovelRecord, target_type: TargetType) -> Result<&SiteSetting> {
        Self::site_setting_for_stored_target_in(&self.site_settings, record.id, &record.toc_url, target_type)
    }

    fn site_setting_for_stored_target_in<'a>(settings: &'a [SiteSetting], id: i64, toc_url: &str, target_type: TargetType) -> Result<&'a SiteSetting> {
        match target_type {
            TargetType::Id => settings.iter().find(|setting| setting.matches_url(toc_url)).or_else(|| {
                Self::ageauth_redirect_target(toc_url)
                    .and_then(|url| settings.iter().find(|setting| setting.matches_url(&url)))
            }).ok_or_else(|| NarouError::NotFound(format!("Novel not found for ID: {id}"))),
            _ => settings.iter().find(|setting| setting.matches_url(toc_url))
                .ok_or_else(|| NarouError::SiteSetting("No matching site setting".into())),
        }
    }

    fn site_setting_for_new_ncode_in<'a>(settings: &'a [SiteSetting], url: &str, ncode: &str) -> Result<&'a SiteSetting> {
        settings.iter().find(|setting| setting.matches_url(url))
            .ok_or_else(|| NarouError::InvalidTarget(format!("対応外のncodeです({})", ncode.to_lowercase())))
    }

    /// Admission identity follows the effective YAML definition, not an alias host.
    /// Known metadata is just `(novel ID, TOC URL)`; unrelated fields are not decoded.
    /// Resolving this key performs no HTTP requests and does not clone the DSL.
    pub fn site_key_for_known_target(settings: &[SiteSetting], target: &str, known: Option<(i64, &str)>) -> Result<String> {
        let target_type = Self::get_target_type(target);
        if matches!(target_type, TargetType::Url) {
            let setting = settings.iter()
                .find(|setting| setting.matches_series_url(target))
                .or_else(|| settings.iter().find(|setting| setting.matches_url(target)))
                .or_else(|| settings.iter().find(|setting| setting.matches_author_url(target)))
                .ok_or_else(|| NarouError::InvalidTarget(format!("No site setting for URL: {target}")))?;
            return Ok(setting.download_site_family());
        }
        if known.is_none() && matches!(target_type, TargetType::Ncode) {
            return Ok(Self::site_setting_for_new_ncode_in(settings, &narou_ncode_url(target), target)?.download_site_family());
        }
        let (id, toc_url) = known.ok_or_else(|| NarouError::NotFound(format!("Novel not found for ID: {target}")))?;
        Ok(Self::site_setting_for_stored_target_in(settings, id, toc_url, target_type)?.download_site_family())
    }

    /// Admission lookup reuses already-loaded metadata without HTTP or downloader caches.
    pub async fn site_key_for_target(&self, target: &str, known_record: Option<&NovelRecord>) -> Result<String> {
        let target_type = Self::get_target_type(target);
        if matches!(target_type, TargetType::Url)
            || (known_record.is_some() && matches!(target_type, TargetType::Id | TargetType::Ncode))
        {
            return Self::site_key_for_known_target(&self.site_settings, target, known_record.map(|record| (record.id, record.toc_url.as_str())));
        }
        let (_, setting) = self.resolve_target_for_download(target).await?;
        Ok(setting.download_site_family())
    }

    async fn load_novel_info(
        &mut self,
        setting: &SiteSetting,
        toc_source: &str,
        url_captures: &HashMap<String, String>,
        toc_url: &str,
    ) -> Result<NovelInfo> {
        NovelInfo::load(
            self.http.as_ref(),
            self.rate_limiter.as_ref(),
            setting,
            toc_source,
            url_captures,
            toc_url,
            &mut self.preprocess_jobs,
        )
        .await
    }

    pub async fn fetch_latest_status_by_id(
        &mut self,
        id: i64,
    ) -> Result<(
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<i64>,
        Option<bool>,
    )> {
        let record = self
            .novels
            .get_current(id.into())
            .await?
            .ok_or_else(|| NarouError::NotFound(format!("Novel not found for ID: {}", id)))?;
        let (toc_url, ncode) = (record.toc_url, record.ncode);

        let setting = self
            .find_site_setting(&toc_url)
            .ok_or_else(|| NarouError::SiteSetting("No matching site setting".into()))?;

        let mut url_captures = setting.extract_url_captures(&toc_url).unwrap_or_default();
        if let Some(ncode) = ncode {
            url_captures.entry("ncode".to_string()).or_insert(ncode);
        }

        let toc_source = fetch_toc(
            self.http.as_ref(),
            self.rate_limiter.as_ref(),
            &setting,
            &toc_url,
            &mut self.preprocess_jobs,
        )
        .await?;
        let info = self
            .load_novel_info(&setting, &toc_source, &url_captures, &toc_url)
            .await?;

        let (novel_type, inferred_is_end) =
            resolve_novel_type(&setting, &toc_source, &url_captures, &info).await;
        let is_end = Some(inferred_is_end);

        // Ruby: get_general_lastup / get_novelupdated_at fall back to subtitle dates
        // when novel_info_url is unavailable (e.g. Arcadia)
        let mut novelupdated_at = info.novelupdated_at;
        let mut general_lastup = info.general_lastup;
        if novelupdated_at.is_none() || general_lastup.is_none() {
            let site_timezone = setting.site_timezone();
            let subtitles = if novel_type == 2 {
                create_short_story_subtitles(&setting, &toc_source, &info).await.ok()
            } else {
                let title = info.title.as_deref().unwrap_or("");
                parse_subtitles_multipage(toc::MultipageTocRequest {
                    http: self.http.as_ref(),
                    rate_limiter: self.rate_limiter.as_ref(),
                    setting: &setting,
                    toc_source: &toc_source,
                    url_captures: &url_captures,
                    title,
                    progress: self.progress.as_deref(),
                    jobs: &mut self.preprocess_jobs,
                })
                .await
                .ok()
                .map(|listing| listing.subtitles)
            };
            if let Some(subs) = &subtitles {
                if novelupdated_at.is_none() {
                    novelupdated_at = sections_latest_update_time_with_timezone(
                        subs,
                        "subupdate",
                        Some("subdate"),
                        site_timezone,
                    );
                }
                if general_lastup.is_none() {
                    general_lastup = sections_latest_update_time_with_timezone(
                        subs,
                        "subdate",
                        None,
                        site_timezone,
                    );
                }
            }
        }

        Ok((novelupdated_at, general_lastup, info.length, is_end))
    }

    async fn process_digest(&self, digest: DigestContext<'_>) -> Result<bool> {
        let DigestContext {
            write,
            existing_id,
            toc_url,
            novel_dir,
            title,
            latest_story,
            old_count,
            latest_count,
        } = digest;
        #[cfg(feature = "native-runtime")]
        {
            if latest_count >= old_count {
                return Ok(false);
            }

            let mut message = messages::download::digest_detected_prompt(old_count, latest_count);

            let mut auto_choices = crate::compat::load_digest_auto_choices();

            // 進捗バー (CLI) がダイアログ表示を上書きしないよう、選択待ちの
            // 間だけ描画を止める。Web の ProgressReporter は no-op。
            let choose = |auto_choices: &mut Option<std::collections::VecDeque<String>>,
                          message: &str| {
                let mut selected = None;
                if let Some(progress) = self.progress.as_deref() {
                    progress.with_suspended(&mut || {
                        selected = Some(crate::compat::choose_digest_action_with_auto_choices(
                            title,
                            message,
                            auto_choices,
                        ));
                    });
                }
                selected.unwrap_or_else(|| {
                    crate::compat::choose_digest_action_with_auto_choices(
                        title,
                        message,
                        auto_choices,
                    )
                })
            };

            loop {
                match choose(&mut auto_choices, &message) {
                    crate::compat::DigestChoice::Update => return Ok(false),
                    crate::compat::DigestChoice::Cancel => return Ok(true),
                    crate::compat::DigestChoice::CancelAndFreeze => {
                        if let Some(id) = existing_id {
                            crate::compat::set_frozen_state(write, id, Some(true))?;
                        }
                        return Ok(true);
                    }
                    crate::compat::DigestChoice::Backup => {
                        let backup_name = crate::compat::create_backup(write, title)?;
                        self.report_line(&messages::download::backup_created(backup_name)).await;
                    }
                    crate::compat::DigestChoice::ShowStory => {
                        self.report_line(messages::download::story_label()).await;
                        self.report_line(latest_story).await;
                    }
                    crate::compat::DigestChoice::OpenBrowser => {
                        crate::compat::open_browser(toc_url);
                    }
                    crate::compat::DigestChoice::OpenFolder => {
                        crate::compat::open_directory(novel_dir, None);
                    }
                    crate::compat::DigestChoice::Convert => {
                        if let Some(id) = existing_id {
                            crate::compat::convert_existing_novel(
                                write, id, false,
                            ).map_err(NarouError::Platform)?;
                        }
                    }
                }

                if std::io::stdin().is_terminal() {
                    message.clear();
                }
                let _ = std::io::stdout().flush();
            }
        }

        // Worker: no interactive terminal. Return an explicit domain error
        // instead of silently continuing or deleting sections.
        #[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
        {
            let _ = (
                write,
                existing_id,
                toc_url,
                novel_dir,
                title,
                latest_story,
                old_count,
                latest_count,
            );
            Err(NarouError::Unsupported(
                "digest handling requires an interactive terminal; not supported on this platform"
                    .to_string(),
            ))
        }
    }

    /// 小説ディレクトリに実際に保存されている挿絵のファイル名。
    ///
    /// 一覧は `PersistenceService` が持つ (ストアに触るのはそこだけ)。
    /// 上限ページ数を超えた・一覧に失敗した場合は `None` で、呼び出し側は
    /// 従来どおり索引だけで判断する。
    async fn stored_illustration_names(
        &self,
        object_keys: &NovelObjectKeys,
    ) -> Option<std::collections::HashSet<String>> {
        /// 1 ページ 1000 件 × このページ数まで見る。
        const MAX_LIST_PAGES: usize = 10;
        self.persistence
            .list_illustration_names(object_keys, MAX_LIST_PAGES)
            .await
    }

    /// 索引が知っている挿絵の実体を確認する (一覧が使えないときの保険)。
    ///
    /// 上限件数を超える小説は一覧が使える前提に任せる (索引だけで判断する
    /// 従来の挙動へ戻す)。確認自体は [`verify_illustration_files`] が
    /// 最大 [`ILLUSTRATION_VERIFY_WINDOW`] 件の並行 `stat` で行う。
    async fn verify_indexed_illustrations(
        &self,
        object_keys: &NovelObjectKeys,
        illustration_store: &crate::illustration_store::IllustrationIndex,
    ) -> Option<std::collections::HashSet<String>> {
        /// 1 小説につき 1 枚ずつ確認する上限。これを超える小説は一覧が使える
        /// 前提に任せる (索引だけで判断する従来の挙動へ戻す)。
        const MAX_VERIFY: usize = 64;
        let filenames: Vec<String> = illustration_store.filenames().into_iter().collect();
        if filenames.is_empty() || filenames.len() > MAX_VERIFY {
            return None;
        }
        verify_illustration_files(self.assets.as_ref(), object_keys, &filenames).await
    }

    /// 挿絵の取得。`stored_illustrations` は索引が知っている実体名の集合で、
    /// 保存成功した実体も追記し、同じ取得処理内での再取得を防ぐ。
    #[allow(clippy::too_many_arguments)]
    async fn download_illustration(
        &mut self,
        access: &NovelWriteGuard,
        setting: &SiteSetting,
        section: &SectionElement,
        object_keys: &NovelObjectKeys,
        illustration_store: &mut crate::illustration_store::IllustrationIndex,
        mut stored_illustrations: Option<&mut std::collections::HashSet<String>>,
        toc_url: &str,
        raw_html: Option<&str>,
        budget: &mut Option<&mut dyn SectionBudget>,
        section_index: usize,
    ) -> Result<()> {
        // サイト定義がパターンを持たない場合は narou.rb の既定パターンを使う。
        let default_pattern = setting.illust_grep_pattern.is_none();

        // 既定パターンは全サイト共通の定数なので、セクションごとに
        // コンパイルし直さない。
        static DEFAULT_ILLUST_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
            compile_html_pattern(DEFAULT_ILLUST_GREP_PATTERN).expect("valid default pattern")
        });
        let compiled_pattern;
        let re: &Regex = match setting.illust_grep_pattern.as_deref() {
            Some(pattern) => {
                compiled_pattern = compile_html_pattern(pattern).map_err(NarouError::Regex)?;
                &compiled_pattern
            }
            None => &DEFAULT_ILLUST_RE,
        };
        let storage = crate::illustration_store::IllustrationStorageService::new(
            self.assets.clone(),
        )
        .with_dedup(crate::illustration_store::dedup_active());
        // Illustration hosts (i.pximg.net) reject requests without the site's
        // headers, and login-only images need the same cookie jar.
        let illustration_policy = crate::downloader::http_policy::FetchPolicy::for_site(setting);

        // Parsed sections may omit site-specific image markup. Definitions
        // whose fetched page contains only reader controls can opt out.
        let raw_html = raw_html.filter(|_| setting.illust_from_raw.unwrap_or(true));
        for source in illustration_sources(section, raw_html) {
            for caps in re.captures_iter(source) {
                messages::ready().await;
                let Some(url_match) = caps.get(1) else {
                    continue;
                };
                let raw_url = url_match.as_str();
                if raw_url.is_empty() {
                    continue;
                }
                // 既定パターンは本文のあらゆる `<img>` に当たるため、narou.rb
                // `Illustration#scanner` と同じく URL でない注記 (相対パス) は
                // 取得対象にしない。明示パターンはサイト定義が対象を選んでいる
                // ので従来どおり本文基準で解決する。
                if default_pattern
                    && !crate::illustration_store::is_remote_illustration_source(raw_url)
                {
                    continue;
                }
                // `//host/...` (protocol-relative) を本文のスキームで解決し、
                // mitemin は取得用 URL (`viewimagebig` → `viewimage`) へ寄せる
                // (narou.rb `Illustration#transform_mitemin_url` と同じ)。
                let resolved = crate::illustration_store::normalize_illustration_url(
                    &build_section_url(setting, toc_url, raw_url),
                );
                let url = resolved.as_str();
                if self.http.validate_url(url).await.is_err() {
                    self.report_warn(&messages::download::warn_unsafe_illustration_url(url)).await;
                    continue;
                }

                // 索引が知っていても、実体が保存先に無ければ取り直す
                // (ストアの切替・手動削除・別環境での取得で索引だけが残る)。
                let known = illustration_store
                    .filename_for_source(url)
                    .map(str::to_string)
                    .or_else(|| {
                        crate::illustration_store::mitemin_illustration_id(url)
                            .and_then(|id| {
                                illustration_store
                                    .filename_for_mitemin_id(&id)
                                    .map(str::to_string)
                            })
                    });
                if let Some(filename) = known {
                    let stored = stored_illustrations
                        .as_deref()
                        .map(|names| names.contains(&filename))
                        .unwrap_or(true);
                    if stored {
                        crate::application::debug::emit_async(format!(
                            "挿絵: 取得済みのため再取得しません: {url}"
                        )).await;
                        continue;
                    }
                    crate::application::debug::emit_async(format!(
                        "挿絵: 索引にはありますが実体が無いので取り直します: 挿絵/{filename} ({url})"
                    )).await;
                }
                // 保存済み画像は予算を消費せず読み飛ばし、未取得画像の直前で
                // 中断する。索引は呼び出し元が保存し、同じ話の残りから再開する。
                if let Some(budget) = budget.as_deref_mut() {
                    budget.check_active().await?;
                    if budget.should_yield(section_index) {
                        return Err(NarouError::DownloadBudgetExpired {
                            next_section_index: section_index,
                        });
                    }
                }

                match crate::downloader::http_policy::fetch_stream(
                    self.http.as_ref(),
                    self.rate_limiter.as_ref(),
                    url,
                    &illustration_policy,
                )
                .await
                {
                    Ok(response) => {
                        let content_type =
                            response.header("Content-Type").unwrap_or("").to_string();
                        // The storage service stages the input, validates all
                        // frames before streaming APNG output, and publishes
                        // the index only after the hashed object is complete.
                        let ext =
                            crate::illustration_store::illustration_extension_from_content_type(
                                &content_type,
                            )
                            .unwrap_or_else(|| {
                                crate::illustration_store::guessed_extension_from_url(url)
                            });
                        match storage
                            .store_stream(access, object_keys, illustration_store, url, response.body, ext)
                            .await
                        {
                            Err(err) => {
                                crate::application::debug::emit_error_async(format!(
                                    "挿絵の保存に失敗 ({url}): {err}"
                                )).await;
                                return Err(err);
                            }
                            Ok(stored) => {
                                let filename = stored.filename;
                                crate::application::debug::emit_async(format!(
                                    "挿絵を保存しました: {url} → 挿絵/{filename}"
                                )).await;
                                if let Some(names) = stored_illustrations.as_deref_mut() {
                                    names.insert(filename);
                                }
                            }
                        }
                    }
                    Err(err) => {
                        crate::application::debug::emit_error_async(format!(
                            "挿絵の取得に失敗 ({url}): {err}"
                        )).await;
                        self.report_warn(&messages::download::warn_illustration_download(url, err)).await;
                    }
                }
            }
        }

        Ok(())
    }

    pub fn download_target_expander(&self) -> crate::application::download_targets::DownloadTargetExpander<'_> {
        crate::application::download_targets::DownloadTargetExpander::new(
            self.http.as_ref(),
            self.rate_limiter.as_ref(),
            &self.site_settings,
        )
        .with_cookie_store(self.cookies.as_deref())
    }

    pub fn get_novel_data_dir(&self, record: &NovelRecord) -> PathBuf {
        crate::db::novel_dir_for_record(&PathBuf::from(types::ARCHIVE_ROOT_DIR), record)
    }

    /// Capture from this Downloader's injected ports before domain waiting or
    /// source fetch, then retain this exact token through post-fetch metadata.
    pub async fn capture_write_for_record(&self, record: &NovelRecord) -> Result<NovelWriteGuard> {
        let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
        NovelWriteGuard::capture_for(self.persistence.objects(), Some(&self.assets), &keys).await
    }

    fn download_toc_target(setting: &SiteSetting, target: &str, stored_url: Option<&str>) -> (HashMap<String, String>, String) {
        let captures = stored_url
            .and_then(|url| setting.extract_url_captures(url))
            .or_else(|| setting.extract_url_captures(target))
            .or_else(|| ncode_target_url(target).and_then(|url| setting.extract_url_captures(&url)))
            .unwrap_or_default();
        let toc_url = if let Some(url) = stored_url {
            url.to_string()
        } else if captures.is_empty() {
            setting.toc_url()
        } else {
            let mut selects = captures.clone();
            selects.insert("__target_url".to_string(), target.to_string());
            setting.get_toc_url_with_captures(&selects)
        };
        (captures, Self::ageauth_redirect_target(&toc_url).unwrap_or(toc_url))
    }

    pub async fn download_novel(&mut self, target: &str) -> Result<DownloadResult> {
        self.download_novel_with_force(target, false).await
    }

    pub async fn download_novel_with_force(
        &mut self,
        target: &str,
        force: bool,
    ) -> Result<DownloadResult> {
        self.download_novel_with_force_budget(target, force, None)
            .await
    }

    /// Download a novel while yielding only before a new section starts.
    ///
    /// Existing callers keep the unbounded API above. Worker callers provide a
    /// short-lived budget so a timeout cannot cancel an in-flight section
    /// between its fetch and persistence steps.
    pub async fn download_novel_with_force_budget(
        &mut self,
        target: &str,
        force: bool,
        budget: Option<&mut dyn SectionBudget>,
    ) -> Result<DownloadResult> {
        self.download_novel_with_execution_options(
            target,
            DownloadExecutionOptions {
                force,
                budget,
                resume_from_section: None,
                access: None,
                creation_reservation: None,
            },
        )
        .await
    }

    /// Download with an optional durable section resume cursor.
    pub async fn download_novel_with_execution_options(
        &mut self,
        target: &str,
        mut options: DownloadExecutionOptions<'_>,
    ) -> Result<DownloadResult> {
        // Preprocess job results belong to one novel: a new download starts
        // with an empty set so nothing leaks between works.
        self.preprocess_jobs.reset();
        self.section_cache = section::SectionCache::new();
        let (mut existing_id, setting) = self.resolve_target_for_download(target).await?;
        let mut setting = setting.clone();
        if options.access.is_none() && (options.resume_from_section.is_some()
            || (options.creation_reservation.is_some() && existing_id.is_some())) {
            return Err(NarouError::DownloadResumeCorrupt("download checkpoint has no original writer fence".into()));
        }
        if existing_id.is_none() && options.resume_from_section.is_some() && options.creation_reservation.is_none() {
            return Err(NarouError::DownloadResumeCorrupt("new download checkpoint has no original ID reservation".into()));
        }
        // A wholly unresolved alias can discover its canonical URL before
        // first admission. An admitted writer or creation never changes ID,
        // and a same-URL absence remains a create intent across the probe.
        let discovery = if existing_id.is_none() && options.access.is_none() && options.creation_reservation.is_none() {
            let (url_captures, initial_url) = Self::download_toc_target(&setting, target, None);
            let policy = crate::downloader::http_policy::FetchPolicy::for_site(&setting);
            let mut discovery = crate::downloader::http_policy::resolve_final_url_with_body(
                self.http.as_ref(), self.rate_limiter.as_ref(), &initial_url, &policy,
            ).await;
            if let Ok((final_url, _)) = &discovery {
                let ageauth_url = Self::ageauth_redirect_target(final_url);
                let final_url = ageauth_url.as_deref().unwrap_or(final_url);
                if final_url != initial_url {
                    let canonical_url = self.site_setting_for_url(final_url)
                        .and_then(|setting| setting.toc_url_with_url_captures(final_url));
                    let canonical_url = canonical_url.as_deref().unwrap_or(final_url);
                    if canonical_url != initial_url
                        && let Some(record) = self.novels.find_by_toc_url_current(canonical_url).await?
                    {
                        setting = self.site_setting_for_stored_target(&record, TargetType::Url)?.clone();
                        existing_id = Some(record.id);
                        // Canonical admission may select stored login cookies;
                        // its source fetch must use that newly selected scope.
                        if let Ok((_, body)) = &mut discovery { *body = None; }
                    }
                }
            }
            Some(DownloadDiscovery { url_captures, toc_url: initial_url, response: discovery, policy })
        } else { None };
        if options.access.is_none() && let Some(id) = existing_id {
            let record = self.novels.get_current(id.into()).await?.ok_or_else(|| NarouError::NotFound(format!("Novel not found: {id}")))?;
            options.access = Some(self.capture_write_for_record(&record).await?);
        }
        let mut creation = match options.creation_reservation {
            Some((id, generation)) => {
                if existing_id.is_some_and(|existing| existing != id.0) {
                    return Err(NarouError::Platform("novel_busy: creation checkpoint belongs to another registered identity".into()));
                }
                let reservation = match options.budget.as_deref_mut() {
                    Some(budget) => budget.restore_creation_id(self.novels.as_ref(), id, generation).await?,
                    None => self.novels.resume_id(id, generation).await?,
                };
                Some(DownloadCreation { reservation, checkpoint_owned: true, committed: false })
            }
            None if existing_id.is_none() => {
                let (reservation, checkpoint_owned) = match options.budget.as_deref_mut() {
                    Some(budget) => budget.allocate_creation_id(self.novels.as_ref()).await?,
                    None => (self.novels.allocate_id().await?, false),
                };
                Some(DownloadCreation { reservation, checkpoint_owned, committed: false })
            }
            None => None,
        };
        let result = async {
            let job_owner = options.budget.as_deref().and_then(|budget| budget.job_owner());
            if let (Some(write), Some(owner)) = (options.access.as_mut(), job_owner) {
                write.bind_job_owner(owner)?;
            }
            if let (Some(write), Some(creation)) = (options.access.as_mut(), creation.as_ref()) {
                write.retain_reservation(&creation.reservation)?;
            }
            if let Some(write) = options.access.as_ref() {
                checkpoint_download_access(&mut options.budget, &write.origin(), creation.as_mut()).await?;
            }
            self.download_novel_reserved(target, options, existing_id, setting, discovery, creation.as_mut()).await
        }.await;
        if let Some(creation) = creation {
            if creation.committed {
                creation.reservation.adopt();
            } else if !creation.checkpoint_owned&&let Err(release_error)=self.novels.release_id(creation.reservation).await{
                // A deferred cleanup keeps the unpublished namespace gated to
                // its own removal owner until the startup collector finishes
                // it. That pending state never fails the download: a canceled
                // new work is a normal skip, and a failed download keeps its
                // own error.
                if !deferred_creation_cleanup(&release_error){
                    return Err(match result{
                        Err(error)=>NarouError::Platform(format!("{error}; 小説IDの予約を解放できませんでした: {release_error}")),
                        Ok(_)=>release_error,
                    });
                }
            }
        }
        result
    }

    async fn download_novel_reserved(
        &mut self,
        target: &str,
        mut options: DownloadExecutionOptions<'_>,
        existing_id: Option<i64>,
        mut setting: SiteSetting,
        discovery: Option<DownloadDiscovery>,
        mut creation: Option<&mut DownloadCreation>,
    ) -> Result<DownloadResult> {
        let force = options.force;
        let resume_from_section = options.resume_from_section;
        let mut budget = options.budget.take();
        let provisional_id = existing_id.or_else(|| creation.as_ref().map(|creation| creation.reservation.id().0))
            .ok_or_else(|| NarouError::Platform("new download has no owned ID reservation".into()))?;

        let db_toc_url = if let Some(id) = existing_id {
            self.novels
                .get_current(id.into())
                .await?
                .map(|record| record.toc_url)
        } else {
            None
        };

        let ((url_captures, toc_url), discovery) = match discovery {
            Some(discovery) => {
                let target = if existing_id.is_none() {
                    (discovery.url_captures, discovery.toc_url)
                } else {
                    Self::download_toc_target(&setting, target, db_toc_url.as_deref())
                };
                (target, Some((discovery.response, discovery.policy)))
            }
            None => (Self::download_toc_target(&setting, target, db_toc_url.as_deref()), None),
        };
        // Login fallback: a novel already flagged as needing authentication
        // sends the stored cookie from the first request; every other novel
        // stays anonymous until a fetch actually fails.
        let mut configured_cookie = setting.cookie.clone();
        // 認証状態: サイトの設定 cookie と、保存済みログインのうち今有効なもの。
        // 候補を切り替えるたびに素の cookie へ戻すので、アカウントの cookie が
        // 混ざらない。
        let mut auth = AuthState::new(configured_cookie.clone());
        if let Some(id) = existing_id {
            let record = self.novels.get_current(id.into()).await?;
            let requires = record.as_ref().is_some_and(|record| record.requires_login);
            if requires {
                // その小説で以前成功したログインがあればそれを、無ければ
                // 一覧の先頭を最初のリクエストから送る。
                let groups = self.stored_groups_for(&setting).await;
                auth.set_groups(groups);
                let chosen = record
                    .as_ref()
                    .and_then(|record| record.login_session.as_deref())
                    .and_then(|id| auth.index_of(id))
                    .or_else(|| auth.next_unused());
                if let Some(index) = chosen {
                    auth.apply(&mut setting, index);
                }
            }
        }
        let mut login_site_changed = false;
        // Prefetched input and its response-cookie jar are one fetch context.
        // Rebuilding a policy here loses CSRF cookies needed by YAML form jobs.
        let (probe_policy, discovered) = match discovery {
            Some((response, policy)) => (policy, response),
            None => {
                let policy = crate::downloader::http_policy::FetchPolicy::for_site(&setting);
                let response = crate::downloader::http_policy::resolve_final_url_with_body(
                    self.http.as_ref(), self.rate_limiter.as_ref(), &toc_url, &policy,
                ).await;
                (policy, response)
            }
        };
        let (resolved_toc_url, prefetched_toc, redirected) = match discovered {
                Ok((final_url, _body)) if final_url != toc_url => {
                    let final_url = Self::ageauth_redirect_target(&final_url).unwrap_or(final_url);
                    if let Some(mut final_setting) = self.find_site_setting(&final_url) {
                        configured_cookie = final_setting.cookie.clone();
                        if final_setting.domain == setting.domain {
                            final_setting.cookie = setting.cookie.clone();
                            final_setting.login_cookie_hosts = setting.login_cookie_hosts.clone();
                        } else {
                            // 別サイトへ移ったので、認証状態もそのサイトの
                            // cookie で作り直す。
                            login_site_changed = true;
                            auth = AuthState::new(configured_cookie.clone());
                        }
                        setting = final_setting;
                        (
                            setting
                                .toc_url_with_url_captures(&final_url)
                                .unwrap_or(final_url),
                            None,
                            true,
                        )
                    } else {
                        // 移動先がどのサイト定義にも一致しないページ (暁の
                        // /novels/adult_check のような中継ゲート) は TOC として
                        // 採用しない。採用すると壁への認証リトライが作品ではなく
                        // ゲート URL へ送られ、構造的に必ず失敗する。元の
                        // toc_url を保持すれば、リトライは作品 URL へ認証付きで
                        // 届き、壁は login_pattern として従来どおり処理される。
                        (toc_url, None, false)
                    }
                }
                Ok((final_url, body)) => (final_url, body, false),
                Err(_) => (toc_url, None, false),
            };
        if redirected
            && let Some(claimed) = self.novels.find_by_toc_url_current(&resolved_toc_url).await?
            && claimed.id != provisional_id
        {
            return Err(NarouError::Platform(format!("novel_busy: redirect belongs to another admitted identity: {}", claimed.id)));
        }
        let toc_url = resolved_toc_url;
        let url_captures = setting
            .extract_url_captures(&toc_url)
            .unwrap_or(url_captures);
        let toc_source = match prefetched_toc {
            // The redirect probe already fetched the final URL with the same
            // cookie scope; reuse the body instead of a second GET.
            Some(response) => {
                let response =
                    crate::downloader::http_policy::ensure_success_response(&toc_url, response);
                match response {
                    Ok(response) => {
                        let mut body = crate::downloader::http_policy::decode_with_encoding(
                            &response.body,
                            Some(setting.encoding()),
                        );
                        // The redirect probe already spent the GET, so run the
                        // definition (and any requests it makes) here rather
                        // than re-fetching the same URL through `fetch_toc`.
                        crate::downloader::util::pretreatment_source_with_jobs(
                            crate::downloader::util::PretreatmentRequest {
                                http: self.http.as_ref(),
                                rate_limiter: self.rate_limiter.as_ref(),
                                policy: &probe_policy,
                                src: &mut body,
                                encoding: setting.encoding(),
                                setting: Some(&setting),
                                jobs: &mut self.preprocess_jobs,
                                url: &toc_url,
                            },
                        )
                        .await?;
                        if let Some(re) = setting.compiled_error_message_pattern()
                            && re.is_match(&body)
                        {
                            Err(NarouError::NotFound("Novel deleted or private".into()))
                        } else {
                            Ok(body)
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            None => {
                fetch_toc(
                    self.http.as_ref(),
                    self.rate_limiter.as_ref(),
                    &setting,
                    &toc_url,
                    &mut self.preprocess_jobs,
                )
                .await
            }
        };
        // ログイン壁 (サイトのログインページやブラウザ確認の応答) が残った
        // ときは、保存済みログインを順に試す。保存された cookie が無いサイトは
        // 従来どおり匿名のまま失敗する。
        // 目次を読む間は失効したトークンの更新を一度だけ試す (更新した
        // トークンでも壁が残るなら、保存済みログインを従来どおり試す)。
        auth.reset_token_refresh();
        let toc_source = self
            .recover_toc_page(&mut setting, &mut auth, &toc_url, toc_source)
            .await;
        let toc_source = match toc_source {
            Ok(source) => source,
            Err(NarouError::NotFound(_)) if existing_id.is_some() => {
                let id = existing_id.unwrap();
                return self.unavailable_result(options.access.as_ref(), id).await;
            }
            Err(err) => return Err(err),
        };
        let toc_preview_info = NovelInfo::from_toc_source(&setting, &toc_source).await;
        match over18_access_decision(
            &setting,
            &toc_source,
            self.settings.global_setting_optional_bool("over18"),
        ) {
            Over18AccessDecision::Allow => {}
            Over18AccessDecision::Prompt => {
                if !self
                    .settings
                    .confirm("年齢認証：あなたは18歳以上ですか", false, false)?
                {
                    return Ok(DownloadResult {
                        id: provisional_id,
                        title: toc_preview_info.title.clone().unwrap_or_default(),
                        author: toc_preview_info.author.clone().unwrap_or_default(),
                        novel_dir: PathBuf::new(),
                        new_novel: existing_id.is_none(),
                        new_arrivals: false,
                        new_arrival_subtitles: Vec::new(),
                        updated_count: 0,
                        total_count: 0,
                        status: UpdateStatus::Canceled,
                        title_changed: false,
                        author_changed: false,
                        story_changed: false,
                        sections_deleted: false,
                    });
                }
                self.settings.save_global_setting_bool("over18", true)?;
            }
            Over18AccessDecision::Deny => {
                return Ok(DownloadResult {
                    id: provisional_id,
                    title: toc_preview_info.title.clone().unwrap_or_default(),
                    author: toc_preview_info.author.clone().unwrap_or_default(),
                    novel_dir: PathBuf::new(),
                    new_novel: existing_id.is_none(),
                    new_arrivals: false,
                    new_arrival_subtitles: Vec::new(),
                    updated_count: 0,
                    total_count: 0,
                    status: UpdateStatus::Canceled,
                    title_changed: false,
                    author_changed: false,
                    story_changed: false,
                    sections_deleted: false,
                });
            }
        }

        let existing_record = match existing_id {
            Some(eid) => self.novels.get_current(eid.into()).await?,
            None => None,
        };

        // 目次と各ページの話数一覧を読む。壁や欠けた一覧になったときは、保存済み
        // ログインを順に試して最初の一ページから読み直すので、欠けた一覧を
        // 成功として保存しない。
        let attempt = match self
            .recover_toc_listing(&mut setting, &mut auth, &toc_url, &url_captures, toc_source)
            .await
        {
            Ok(attempt) => attempt,
            Err(NarouError::NotFound(_)) if existing_id.is_some() => {
                let id = existing_id.unwrap();
                return self.unavailable_result(options.access.as_ref(), id).await;
            }
            Err(error) => return Err(error),
        };
        let TocAttempt {
            source: toc_source,
            info,
            novel_type,
            is_end,
            listing,
        } = attempt;
        let toc::TocListing {
            mut subtitles,
            partial: partial_listing,
            tag_sources: page_tags,
            ..
        } = listing;
        let author = info.author.clone().unwrap_or_default();

        let fetched_raw_title = info.title.clone().unwrap_or_default();
        let raw_title = if fetched_raw_title.is_empty() {
            existing_record
                .as_ref()
                .map(|record| record.raw_title().to_string())
                .unwrap_or_default()
        } else {
            fetched_raw_title
        };
        // A supplied writer on an existing record already knows its durable
        // prefix, so re-acknowledge canonical ownership before the first
        // writer-dependent read; a mid-download replacement must not be
        // blessed by this guard and an abort must leave both ACKs persisted.
        let mut reacked = false;
        if let Some(record) = existing_record.as_ref()
            && let Some(write) = options.access.as_ref()
        {
            let record_keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            if write.prefix() != &crate::platform::ObjectPrefix::from(record_keys.prefix().clone()) {
                return Err(NarouError::Platform("download writer belongs to another novel".into()));
            }
            checkpoint_download_access(&mut budget, &write.origin(), creation.as_deref_mut()).await?;
            reacked = true;
        }
        #[cfg(feature = "native-runtime")]
        let title_ini = if let Some(record) = existing_record.as_ref() {
            let write = options.access.as_ref().ok_or_else(|| NarouError::Platform("existing title settings have no original writer".into()))?;
            let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
            let key = keys.setting();
            let mut settings_access = NovelAccess::from_writer(write.clone(), "download-title-settings").await?;
            let result: Result<_> = async {
                match settings_access.read()?.read_small(&key).await? {
                    Some(bytes) => {
                        let text = String::from_utf8(bytes).map_err(|error| NarouError::Platform(format!("setting.ini is not UTF-8: {error}")))?;
                        Ok(crate::converter::ini::IniData::load(&text))
                    }
                    None => Ok(crate::converter::ini::IniData::new()),
                }
            }.await;
            let released = settings_access.release().await;
            Some(result.and_then(|ini| released.map(|()| ini))?)
        } else { None };
        #[cfg(not(feature = "native-runtime"))]
        let title_ini = None;
        let strip_title_prefix =
            self.settings
                .strip_title_prefix_for(provisional_id, &raw_title, &author, title_ini.as_ref())?;
        let track_raw_title = strip_title_prefix
            || existing_record
                .as_ref()
                .is_some_and(NovelRecord::has_raw_title);
        let title = crate::title::project_title(&raw_title, strip_title_prefix);
        let use_subdirectory = self.download_use_subdirectory(existing_record.as_ref());
        let ncode = self
            .extract_ncode(&setting, &toc_source).await
            .or_else(|| url_captures.get("ncode").cloned());
        let file_title = self.compute_file_title(
            &ncode,
            &title,
            setting.append_title_to_folder_name,
            existing_record.as_ref(),
        );
        // Ruby の sitename/get_file_title と同じく、登録済みの保存先は
        // サイト定義や取得メタデータが変わっても維持する。保存先を先に
        // 変えると旧目次が見えず、話数減少・キャンセルを検出できない。
        let sitename = existing_record
            .as_ref()
            .map(|record| record.sitename.clone())
            .filter(|name| !name.is_empty())
            .or_else(|| info.sitename.clone())
            .unwrap_or_else(|| setting.sitename.clone());

        let object_keys = NovelObjectKeys::new(&sitename, &file_title, use_subdirectory)
            .map_err(|error| NarouError::Platform(error.to_string()))?;
        let novel_dir = self.compute_novel_dir(&sitename, &file_title, use_subdirectory);
        if existing_id.is_none() {
            let by_url = self.novels.find_by_toc_url_current(&toc_url).await?;
            let by_prefix = self.novels.find_by_storage_prefix_current(object_keys.prefix()).await?;
            for claimed in [by_url, by_prefix].into_iter().flatten() {
                if claimed.id != provisional_id {
                    return Err(NarouError::Platform(format!("novel_busy: novel identity was registered during download: {}", claimed.id)));
                }
            }
        }
        // Existing works retain the epoch from first canonical admission.
        // A new prefix becomes knowable only after resolving the source TOC.
        let mut write = match options.access.take() {
            Some(write) => write,
            None => NovelWriteGuard::capture_for(self.persistence.objects(), Some(&self.assets), &object_keys).await?,
        };
        if let Some(owner) = budget.as_deref().and_then(|budget| budget.job_owner()) {
            write.bind_job_owner(owner)?;
        }
        if let Some(creation) = creation.as_ref() {
            write.retain_reservation(&creation.reservation)?;
        }
        if write.prefix() != &crate::platform::ObjectPrefix::from(object_keys.prefix().clone()) {
            return Err(NarouError::Platform("download writer belongs to another novel".into()));
        }
        if !reacked {
            checkpoint_download_access(&mut budget, &write.origin(), creation.as_deref_mut()).await?;
        }
        if let Some(creation) = creation.as_deref_mut() {
            self.novels.reserve_create(write.fence(), &creation.reservation, &toc_url).await?;
            creation.committed = existing_id.is_some();
        } else if existing_id.is_none() {
            return Err(NarouError::Platform("new download lost its ID reservation".into()));
        }
        let mut access = NovelAccess::from_writer(write.clone(), "download").await?;
        let operation: Result<DownloadResult> = async {
        let old_toc = self
            .persistence
            .load_toc(access.ensure_read().await?, &object_keys)
            .await?;
        let old_subtitles: HashMap<String, &SubtitleInfo> = old_toc
            .as_ref()
            .map(|t| t.subtitles.iter().map(|s| (s.index.clone(), s)).collect())
            .unwrap_or_default();

        let old_title = old_toc.as_ref().map(|t| t.title.clone());
        let old_author = old_toc.as_ref().map(|t| t.author.clone());
        let old_story = old_toc.as_ref().and_then(|t| t.story.clone());
        let old_section_count = old_toc.as_ref().map(|t| t.subtitles.len()).unwrap_or(0);
        let accessible_section_count = subtitles.len();

        let mut preserved_indices = std::collections::HashSet::new();
        if partial_listing && setting.preserve_missing_sections && !subtitles.is_empty()
            && subtitles.iter().all(|latest| old_subtitles.get(&latest.index)
                .is_none_or(|old| old.href == latest.href))
        {
            let omitted = {
                let visible: std::collections::HashSet<&str> =
                    subtitles.iter().map(|subtitle| subtitle.index.as_str()).collect();
                old_subtitles.values().copied()
                    .filter(|old| !visible.contains(old.index.as_str())).collect::<Vec<_>>()
            };
            if !omitted.is_empty() {
                for old in omitted {
                    preserved_indices.insert(old.index.as_str());
                    subtitles.push(old.clone());
                }
                if subtitles.iter().any(|subtitle| subtitle.index.is_empty()
                    || !subtitle.index.bytes().all(|byte| byte.is_ascii_digit()))
                {
                    return Err(NarouError::SiteSetting(
                        "preserve_missing_sections requires stable numeric section indices".into(),
                    ));
                }
                subtitles.sort_unstable_by(|left, right| {
                    let left = left.index.trim_start_matches('0');
                    let right = right.index.trim_start_matches('0');
                    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                });
            }
        }

        if partial_listing || subtitles.is_empty() {
            let omits_saved = if old_section_count > 0 {
                let visible: std::collections::HashSet<&str> =
                    subtitles.iter().map(|subtitle| subtitle.index.as_str()).collect();
                old_subtitles.keys().any(|index| !visible.contains(index.as_str()))
            } else {
                false
            };
            if omits_saved || subtitles.is_empty() {
                self.report_warn(if omits_saved {
                    messages::download::partial_listing_preserved()
                } else {
                    messages::download::no_accessible_sections()
                }).await;
                return Ok(DownloadResult {
                    id: provisional_id,
                    title: old_title.unwrap_or(title),
                    author: old_author.unwrap_or(author),
                    novel_dir,
                    new_novel: existing_id.is_none(),
                    new_arrivals: false,
                    new_arrival_subtitles: Vec::new(),
                    updated_count: 0,
                    total_count: old_section_count,
                    status: UpdateStatus::Canceled,
                    title_changed: false,
                    author_changed: false,
                    story_changed: false,
                    sections_deleted: false,
                });
            }
            self.report_warn(&messages::download::partial_listing(accessible_section_count)).await;
        }

        #[cfg(feature = "native-runtime")]
        if let Some(existing) = existing_record.as_ref() {
            crate::title::rename_projected_outputs(&write, &novel_dir, &author, &existing.title, &title)?;
        }

        let fetched_story = info.story.clone();
        let digest_story = old_story
            .clone()
            .or_else(|| fetched_story.clone())
            .unwrap_or_default();
        if !force && old_section_count > subtitles.len() {
            let title_for_digest = if title.is_empty() {
                old_title.clone().unwrap_or_default()
            } else {
                title.clone()
            };
            if self
                .process_digest(DigestContext {
                    write: &write,
                    existing_id,
                    toc_url: &toc_url,
                    novel_dir: &novel_dir,
                    title: &title_for_digest,
                    latest_story: &digest_story,
                    old_count: old_section_count,
                    latest_count: subtitles.len(),
                })
                .await?
            {
                return Ok(DownloadResult {
                    id: provisional_id,
                    title: title_for_digest,
                    author: if author.is_empty() {
                        old_author.clone().unwrap_or_default()
                    } else {
                        author.clone()
                    },
                    novel_dir,
                    new_novel: existing_id.is_none(),
                    new_arrivals: false,
                    new_arrival_subtitles: Vec::new(),
                    updated_count: 0,
                    total_count: subtitles.len(),
                    status: UpdateStatus::Canceled,
                    title_changed: false,
                    author_changed: false,
                    story_changed: false,
                    sections_deleted: true,
                });
            }
        }

        // サイト定義に `illust_grep_pattern` が無い場合は narou.rb の既定
        // パターンで取得するので、どのサイトでも索引を読む。
        let mut illustration_store = self
            .persistence
            .load_cache(access.ensure_read().await?, &object_keys)
            .await?
            .unwrap_or_default();
        if setting.illust_grep_pattern.is_none() {
            // 既定パターンは本文の `<img>` 全部に当たるため、セクションごとでは
            // なく小説 1 件につき 1 行だけ出す (24 話で 24 行出していた)。
            crate::application::debug::emit_async(format!(
                "挿絵: サイト定義に illust_grep_pattern が無いので既定の <img> パターンで取得します ({})",
                setting.domain
            )).await;
        }
        // 索引が空なら突き合わせる相手が無いので一覧は取らない (小説 1 件に
        // つき 1〜2 サブリクエスト増えるため、必要なときだけ)。
        // The store listing/stat enumeration below does not consume the read
        // lease; release it so a slow S3 LIST cannot leave an idle lease to
        // expire and stall a writer drain waiting on this novel's readers.
        let _ = access.release_read().await;
        let mut stored_illustrations = if illustration_store.source_count() > 0 {
            let names = match self.stored_illustration_names(&object_keys).await {
                // 一覧が取れて空でなければそれを信じる (通常経路)。
                Some(names) if !names.is_empty() => Some(names),
                // 一覧が空・失敗のときは索引のファイルを 1 枚ずつ確認する。
                // LIST が prefix の解釈差で空を返す環境でも「実体がある」と
                // 判定でき、本当に無いときだけ取り直せる。
                _ => {
                    self.verify_indexed_illustrations(&object_keys, &illustration_store)
                        .await
                }
            };
            if crate::application::debug::enabled() {
                crate::application::debug::emit_async(format!(
                    "挿絵: 保存済み {} 件 / 索引 {} 件 ({})",
                    names.as_ref().map(|names| names.len()).unwrap_or(0),
                    illustration_store.source_count(),
                    object_keys.prefix().as_ref(),
                )).await;
            }
            names
        } else {
            None
        };

        let resume_from = resume_from_section.unwrap_or(0);
        if resume_from > subtitles.len() {
            return Err(NarouError::DownloadResumeCorrupt(format!(
                "resume section {resume_from} exceeds TOC length {}",
                subtitles.len()
            )));
        }
        if resume_from == 0 && (budget.is_some() || resume_from_section.is_some()) {
            // A fresh job must not inherit metadata left by a canceled job,
            // even if it fails before its first budget checkpoint is saved.
            self.persistence.clear_download_resume(&write, &object_keys).await?;
        }
        let _ = access.release_read().await;
        let existing_names = self.persistence.list_novel_object_names(&object_keys).await?;
        let existing_sections = PersistenceService::existing_section_indices(&existing_names);
        let mut checkpointed_subtitles = if resume_from > 0 {
            self.persistence.load_download_resume(access.ensure_read().await?, &object_keys).await?
        } else {
            None
        }.map(|toc| (toc.toc_url, toc.subtitles));
        if let Some((saved_url, saved)) = &mut checkpointed_subtitles {
            saved.truncate(resume_from);
            if saved_url != &toc_url || saved.iter().zip(&subtitles).any(|(saved, latest)| {
                saved.index != latest.index || saved.href != latest.href
                    || saved.file_subtitle != latest.file_subtitle
                    || saved.subdate != latest.subdate || saved.subupdate != latest.subupdate
            }) {
                return Err(NarouError::DownloadResumeCorrupt("TOC changed since the download checkpoint".into()));
            }
            for (section_index, subtitle) in saved.iter().enumerate() {
                let key = object_keys.section(&subtitle.index, &subtitle.file_subtitle);
                let name = &key.as_ref()[object_keys.prefix().as_ref().len() + 1..];
                if !existing_names.contains(name) {
                    return Err(NarouError::DownloadResumeCorrupt(format!(
                        "persisted section {section_index} ({}) is missing", subtitle.index,
                    )));
                }
            }
        }
        let checkpointed_subtitles = checkpointed_subtitles.map(|(_, subtitles)| subtitles).unwrap_or_default();
        let restored_count = checkpointed_subtitles.len();
        let mut checkpointed_subtitles = checkpointed_subtitles.into_iter();

        struct SectionPlan {
            is_new_arrival: bool,
            needs_download: bool,
            /// The section file existed before this run (canonical or legacy
            /// name). Drives the cache move without a second read.
            existed: bool,
            predownloaded: Option<(SectionElement, String)>,
            /// Restore this body lazily when its execution turn is reached.
            resumed: bool,
            checkpointed: Option<SubtitleInfo>,
            preserved: bool,
        }

        let mut updated_count = 0usize;
        let mut new_arrivals = existing_id.is_none();
        let mut new_arrival_subtitles = Vec::new();
        let mut final_subtitles = Vec::with_capacity(subtitles.len());
        let strong_update = self.settings.local_setting_bool("update.strong");
        // Budgeted comparisons and legacy-prefix recovery fetch only in
        // execution order. A corrupt resumed prefix must fail before fetching
        // any tail section, including an unbudgeted strong-update comparison.
        let defer_strong_update = strong_update && (budget.is_some() || restored_count < resume_from);
        let site_timezone = setting.site_timezone();
        let save_diff_snapshots = self.settings.save_diff_snapshots();
        let mut cache_dir: Option<String> = None;
        let mut cache_version_recorded = false;
        #[cfg(not(feature = "native-runtime"))]
        let mut superseded_section_keys = Vec::new();
        let mut pending_section_hashes: HashMap<String, String> = HashMap::new();
        let display_id = provisional_id;
        let mut section_plans = Vec::with_capacity(subtitles.len());
        let mut download_count = 0usize;
        let guard_spoiler = self.settings.local_setting_bool("guard-spoiler");
        for (section_index, subtitle) in subtitles.iter().enumerate() {
            let preserved = preserved_indices.contains(subtitle.index.as_str());
            let checkpointed = checkpointed_subtitles.next();
            let resumed = (restored_count..resume_from).contains(&section_index)
                || (preserved && checkpointed.is_none());
            let is_new_arrival = !old_subtitles.contains_key(&subtitle.index);
            let existed = existing_sections.contains(&subtitle.index);
            let (needs_download, predownloaded) = if checkpointed.is_some() || resumed {
                (false, None)
            } else if force {
                (true, None)
            } else {
                self.section_needs_download(&mut access, SectionDecision {
                    setting: &mut setting,
                    auth: &mut auth,
                    latest: subtitle,
                    old: old_subtitles.get(&subtitle.index).copied(),
                    existing_id,
                    keys: &object_keys,
                    toc_url: &toc_url,
                    strong_update: strong_update && !defer_strong_update,
                    timezone: site_timezone,
                    existed,
                })
                .await?
            };
            if needs_download {
                download_count += 1;
            }
            section_plans.push(SectionPlan {
                is_new_arrival,
                needs_download,
                existed,
                predownloaded,
                resumed,
                checkpointed,
                preserved,
            });
        }

        if let Some(ref p) = self.progress {
            p.set_position_async(0).await;
            p.set_length_async(download_count as u64).await;
            p.set_message_async(&format!("DL {}", title)).await;
        }

        let mut last_chapter = String::new();
        let mut last_subchapter = String::new();
        let mut started_download = false;
        let mut downloaded_index = 0usize;

        // Resumed plans retain no bodies. Consume the ordered bounded reader
        // directly, without collecting the prefix before section execution.
        use futures::StreamExt as _;
        let resume_persistence = self.persistence.clone();
        // Only build the ordered reader when a restored prefix actually
        // exists; an empty slice needs neither the clone nor an extra poll.
        let resumed_sections = if restored_count < resume_from {
            futures::future::Either::Left(resume_persistence.stream_sections_ordered(
                access.ensure_read().await?.clone(), &object_keys, &subtitles[restored_count..resume_from],
            ))
        } else {
            futures::future::Either::Right(futures::stream::empty())
        };
        let mut resumed_sections = Box::pin(resumed_sections);

        let section_result: Result<()> = async {
            for (section_index, (subtitle, mut plan)) in subtitles.iter().zip(section_plans).enumerate() {
                if section_index >= resume_from && let Some(budget) = budget.as_deref_mut() {
                    budget.check_active().await?;
                    if budget.should_yield(section_index) {
                        return Err(NarouError::DownloadBudgetExpired {
                            next_section_index: section_index,
                        });
                    }
                }
                let resumed = if (restored_count..resume_from).contains(&section_index) {
                    access.ensure_read().await?;
                    match resumed_sections.next().await {
                        Some(Ok(section)) => Some(section),
                        Some(Err(SectionLoadFailure::Missing { section_index })) => {
                            let section_index = restored_count + section_index;
                            return Err(NarouError::DownloadResumeCorrupt(format!(
                                "persisted section {section_index} ({}) is missing",
                                subtitles[section_index].index,
                            )));
                        }
                        Some(Err(SectionLoadFailure::Read(error))) => return Err(error),
                        None => return Err(NarouError::DownloadResumeCorrupt(format!(
                            "persisted section {section_index} ({}) is missing", subtitle.index,
                        ))),
                    }
                } else if plan.resumed {
                    Some(self.persistence.load_section(access.ensure_read().await?, &object_keys, subtitle)
                        .await?.ok_or_else(|| NarouError::Platform(format!(
                            "preserved section {} is missing from storage", subtitle.index,
                        )))?)
                } else {
                    None
                };
                if defer_strong_update && !force && !plan.resumed && plan.needs_download {
                    let (needs_download, predownloaded) = self
                        .section_needs_download(&mut access, SectionDecision {
                            setting: &mut setting,
                            auth: &mut auth,
                            latest: subtitle,
                            old: old_subtitles.get(&subtitle.index).copied(),
                            existing_id,
                            keys: &object_keys,
                            toc_url: &toc_url,
                            strong_update: true,
                            timezone: site_timezone,
                            existed: plan.existed,
                        })
                        .await?;
                    plan.needs_download = needs_download;
                    plan.predownloaded = predownloaded;
                    if !needs_download {
                        download_count -= 1;
                        if let Some(ref p) = self.progress {
                            p.set_length_async(download_count as u64).await;
                        }
                    }
                }
                let is_new_arrival = plan.is_new_arrival;
                let needs_download = plan.needs_download;
                let resumed_was_tracked = plan.resumed || plan.checkpointed.is_some();
                // 中断された実行を再開したセクションは、前回の索引保存前に実行が
                // 終わっている可能性がある (挿絵の実体だけが残り、対応表が無い
                // 状態)。再開したセクションは取得を省く分だけ挿絵の登録も
                // 通り過ぎてしまうため、保存済みの本文から索引を作り直す。
                // 強更新の予定段階で取得しただけの本文も同じ理由でここを通す。
                // 索引が残っている通常の更新では `download_illustration` が
                // 取得済みと判断して何もしない。
                if !plan.preserved && (resumed_was_tracked || (!needs_download && plan.predownloaded.is_some())) {
                    let section = resumed
                        .as_ref()
                        .map(|resumed| &resumed.element)
                        .or_else(|| plan.predownloaded.as_ref().map(|(section, _)| section));
                    if let Some(section) = section {
                        // `resumed_sections` still shares `access.read`: releasing
                        // it here would orphan the stream's lease token, so keep
                        // the reader admitted until the last restored section.
                        if section_index + 1 >= resume_from {
                            let _ = access.release_read().await;
                        }
                        self.download_illustration(
                            &write,
                            &setting,
                            section,
                            &object_keys,
                            &mut illustration_store,
                            stored_illustrations.as_mut(),
                            &toc_url,
                            None,
                            &mut budget,
                            section_index,
                        )
                        .await?;
                    }
                }

                let previous = old_subtitles.get(&subtitle.index).copied();
                let previous_revision = previous.map_or(1, |old| old.download_revision.max(1));
                let (download_time, download_revision) = if let Some(checkpointed) = plan.checkpointed {
                    (checkpointed.download_time, checkpointed.download_revision)
                } else if let Some(resumed) = resumed {
                    (resumed.download_time, resumed.download_revision)
                } else if needs_download {
                    if !started_download {
                        self.report_line(&colorize(
                            &messages::download::download_started(display_id, &title),
                            "green",
                        )).await;
                        started_download = true;
                    }
                    if let Some(ref p) = self.progress {
                        p.set_message_async(&format!(
                            "DL {} [{}/{}]",
                            title,
                            downloaded_index + 1,
                            download_count
                        )).await;
                    }

                    if !subtitle.chapter.is_empty() && subtitle.chapter != last_chapter {
                        self.report_line(&subtitle.chapter).await;
                        last_chapter = subtitle.chapter.clone();
                    }
                    if !subtitle.subchapter.is_empty() && subtitle.subchapter != last_subchapter {
                        self.report_line(&subtitle.subchapter).await;
                        last_subchapter = subtitle.subchapter.clone();
                    }

                    let (section, raw_html) = match plan.predownloaded {
                        Some(downloaded) => downloaded,
                        None => {
                            let _ = access.release_read().await;
                            self
                            .download_section_with_logins(
                                &mut setting,
                                &mut auth,
                                subtitle,
                                &toc_url,
                            )
                            .await?
                        },
                    };
                    let had_existing = plan.existed;
                    if had_existing {
                        // The stored section still has the previous TOC's filename.
                        let previous = previous.unwrap_or(subtitle);
                        if cache_dir.is_none() {
                            cache_dir = cache_timestamp(self.clock.as_ref(), save_diff_snapshots);
                        }
                        if let Some(id) = existing_id {
                            self.clear_section_digest(id, &section_relative_path(previous));
                        }
                        if let Some(timestamp) = cache_dir.as_deref() {
                            let cached = self.persistence
                                .cache_section(&write, access.ensure_read().await?, &object_keys, timestamp, previous)
                                .await?;
                            if cached && !cache_version_recorded {
                                self.persistence.record_cache_version(&write, &object_keys, timestamp).await?;
                                cache_version_recorded = true;
                            }
                        }
                    }
                    // A revision identifies a successful body write, not wall
                    // time: a frozen/adjusted clock must not hide a resumed edit.
                    let revision = previous_revision.wrapping_add(1).max(2);
                    let download_time = self
                        .persistence
                        .save_section(&write, &object_keys, subtitle, &section, revision)
                        .await?;
                    let digest = compute_section_hash(&section);
                    let relative_path = section_relative_path(subtitle);
                    if let Some(id) = existing_id {
                        self.store_section_digest(id, &relative_path, &digest);
                    } else {
                        pending_section_hashes.insert(relative_path, digest);
                    }
                    // 生データの保存は環境で切れる (Worker は常に切る)。
                    // 挿絵の走査はこの直後、メモリ上の raw_html で行う。
                    if self.settings.save_raw_html() {
                        self.persistence
                            .save_raw(&write, &object_keys, subtitle, &raw_html)
                            .await?;
                    }
                    // Same guard as above: `resumed_sections` shares `access.read`
                    // while restored sections remain, so only release once the
                    // last restored index passed — idle reader leases must not
                    // be held across the (possibly long) illustration fetch.
                    if section_index + 1 >= resume_from {
                        let _ = access.release_read().await;
                    }
                    self.download_illustration(
                            &write,
                        &setting,
                        &section,
                        &object_keys,
                        &mut illustration_store,
                        stored_illustrations.as_mut(),
                        &toc_url,
                        Some(raw_html.as_str()),
                        &mut budget,
                        section_index,
                    )
                    .await?;
                    downloaded_index += 1;
                    (Some(download_time), revision)
                } else {
                    // 内容が変わっていない (`needs_download=false`) セクションは
                    // 保存も取得もしないので、保存時刻は旧 TOC の値を引き継ぐ。
                    (previous.and_then(|old| old.download_time.clone()), previous_revision)
                };
                #[cfg(not(feature = "native-runtime"))]
                if (needs_download || resumed_was_tracked)
                    && let Some(previous) = old_subtitles.get(&subtitle.index)
                    && previous.file_subtitle != subtitle.file_subtitle
                {
                    let previous_key = object_keys.section(&previous.index, &previous.file_subtitle);
                    let current_key = object_keys.section(&subtitle.index, &subtitle.file_subtitle);
                    if previous_key != current_key {
                        superseded_section_keys.push(previous_key);
                    }
                }

                let mut sub = subtitle.clone();
                sub.download_time = download_time;
                sub.download_revision = download_revision.max(1);
                // A completed prefix still belongs to this update, not to the
                // last committed TOC. Unchanged/preserved prefix entries do not.
                let changed = needs_download || (resumed_was_tracked && !plan.preserved
                    && previous.is_none_or(|old| {
                        // An old checkpoint without a generation cannot prove
                        // it was unchanged. Count it once, then retain baseline
                        // 1 in the TOC; unchanged legacy bodies need no rewrite.
                        (download_revision == 0 && old.download_revision == 0)
                            || old.download_revision.max(1) != sub.download_revision
                            || old.download_time != sub.download_time
                    }));
                if changed {
                    updated_count += 1;
                }
                if changed && is_new_arrival {
                    new_arrivals = true;
                    new_arrival_subtitles.push(sub.clone());
                }
                final_subtitles.push(sub);

                if needs_download {
                    let mut line = String::new();
                    if novel_type == 1 {
                        // Series: "第{index}部分　" (only if index ≤ 4 digits)
                        if subtitle.index.len() <= 4 {
                            line.push_str(&format!("第{}部分　", subtitle.index));
                        }
                    } else {
                        line.push_str("短編　");
                    }
                    let printable_subtitle = if guard_spoiler {
                        mask_spoiler_text(&subtitle.subtitle)
                    } else {
                        subtitle.subtitle.clone()
                    };
                    line.push_str(&format!(
                        "{} ({}/{})",
                        printable_subtitle, downloaded_index, download_count
                    ));
                    if needs_download {
                        if is_new_arrival && (existing_id.is_some() || force) {
                            line.push_str(&colorize(" (新着)", "magenta"));
                        } else if !is_new_arrival && force {
                            line.push_str(" (更新あり)");
                        }
                    }
                    self.report_line(&line).await;
                    if let Some(ref p) = self.progress {
                        p.inc_async(1).await;
                    }
                }
            }
            Ok(())
        }.await;

        // Exact prefix consumers need not poll one extra EOF. Drop the
        // bounded window before releasing the operation's final lease owner.
        drop(resumed_sections);
        // 挿絵を 1 枚も扱わなかった小説には空の索引を作らない (変化が無ければ
        // 書き戻さない)。
        let cache_result = if illustration_store.is_dirty() {
            self.persistence.save_cache(&write, &object_keys, &illustration_store).await
        } else {
            Ok(())
        };
        let cache_saved = cache_result.is_ok();
        if let Err(error) = cache_result {
            if section_result.is_err() {
                self.report_warn(&messages::indented_error(&error)).await;
            } else {
                return Err(error);
            }
        }
        if let Err(error) = section_result {
            if cache_saved && matches!(error, NarouError::DownloadBudgetExpired { .. }) {
                // The image index is durable before metadata allows a future
                // invocation to skip completed bodies. The unfinished episode
                // is deliberately absent, including yields inside its images.
                self.persistence.save_download_resume(&write, &object_keys, &TocFile {
                    title, author, toc_url, story: None,
                    subtitles: final_subtitles, novel_type: Some(novel_type),
                }).await?;
            }
            return Err(error);
        }

        if let Some(ref p) = self.progress {
            p.finish_with_message_async(&format!(
                "DL {} done ({}/{})",
                title,
                updated_count,
                subtitles.len()
            )).await;
        }

        let db_title = existing_record
            .as_ref()
            .map(|r| r.title.clone())
            .filter(|t| !t.is_empty());
        let db_author = existing_record
            .as_ref()
            .map(|r| r.author.clone())
            .filter(|a| !a.is_empty());
        let old_title_for_compare = old_title.clone().or_else(|| db_title.clone());
        let old_author_for_compare = old_author.clone().or_else(|| db_author.clone());
        let title_changed = !title.is_empty() && old_title_for_compare.as_deref() != Some(&title);
        let author_changed =
            !author.is_empty() && old_author_for_compare.as_deref() != Some(&author);
        let story_changed = story_changed(&old_story, &fetched_story);
        let new_story = if story_changed {
            fetched_story
        } else {
            old_story.clone().or(fetched_story)
        };
        let sections_deleted = old_section_count > subtitles.len();

        let toc_title = if title.is_empty() {
            old_title
                .filter(|t| !t.is_empty())
                .or(db_title)
                .unwrap_or_default()
        } else {
            title.clone()
        };
        let toc_author = if author.is_empty() {
            old_author
                .filter(|t| !t.is_empty())
                .or(db_author)
                .unwrap_or_default()
        } else {
            author.clone()
        };

        // Site rules are independent of auto-add-tags and can use the
        // preprocessed first TOC page as well as persisted metadata.
        let work_type = if setting.work_type_rules.is_empty() {
            crate::downloader::site_setting::WorkType::Novel
        } else {
            let episode_count = if setting.work_type_rules.iter().any(|rule| rule.field == self::site_setting::WorkTypeField::EpisodeCount) {
                subtitles.len().to_string()
            } else { String::new() };
            let novel_type_code = if setting.work_type_rules.iter().any(|rule| rule.field == self::site_setting::WorkTypeField::NovelType) {
                novel_type.to_string()
            } else { String::new() };
            setting.work_type_for(&WorkTypeSubjects {
                ncode: ncode.as_deref().unwrap_or_default(),
                title: &toc_title,
                author: &toc_author,
                url: &toc_url,
                novel_type: &novel_type_code,
                episode_count: &episode_count,
                source: &toc_source,
            })
        };

        let toc_file = TocFile {
            title: toc_title.clone(),
            author: toc_author.clone(),
            toc_url: toc_url.clone(),
            story: new_story.clone(),
            subtitles: final_subtitles,
            novel_type: Some(novel_type),
        };
        self.persistence.save_toc(&write, &object_keys, &toc_file).await?;
        // Portable history uses copy: retire renamed current keys only after
        // both the replacement body and its new TOC are durable.
        #[cfg(not(feature = "native-runtime"))]
        for key in superseded_section_keys {
            write.delete(&key).await?;
        }
        self.persistence
            .ensure_default_files(&write, &object_keys,
                &toc_title,
                &toc_author,
                &toc_url,
                Some(&existing_names),
            )
            .await?;

        let now = self.clock.now_utc();
        let mut record = NovelRecord {
            id: provisional_id,
            author: toc_author.clone(),
            title: toc_title.clone(),
            file_title: file_title.clone(),
            toc_url,
            sitename,
            novel_type,
            end: is_end,
            last_update: now,
            new_arrivals_date: Some(now),
            use_subdirectory,
            general_firstup: info.general_firstup,
            novelupdated_at: info.novelupdated_at.or_else(|| {
                sections_latest_update_time_with_timezone(
                    &subtitles,
                    "subupdate",
                    Some("subdate"),
                    site_timezone,
                )
            }),
            general_lastup: info.general_lastup.or_else(|| {
                sections_latest_update_time_with_timezone(
                    &subtitles,
                    "subdate",
                    None,
                    site_timezone,
                )
            }),
            last_mail_date: None,
            tags: Vec::new(),
            ncode,
            domain: Some(setting.domain.clone()),
            general_all_no: Some(subtitles.len() as i64),
            length: info.length,
            suspend: false,
            is_narou: setting.is_narou,
            last_check_date: None,
            convert_failure: false,
            requires_login: auth.requires_login(),
            login_session: auth.active().map(|active| active.id.clone()),
            extra_fields: Default::default(),
        };
        // 取得に使う URL (toc_url) と利用者が指定した URL が違う場合、後者を
        // 控えておく。Pixiv は toc_url が API なので、Web UI のリンクが
        // API を開いてしまうのを避ける (通常サイトは両者同じなので記録しない)。
        if target != record.toc_url {
            record.set_original_url(target);
        }
        if track_raw_title {
            record.set_raw_title(raw_title);
        }

        let auto_add_tags = self.settings.local_setting_bool("auto-add-tags");
        let auto_tags = if auto_add_tags {
            collect_site_tags(info.tags.as_deref(), &page_tags)
        } else {
            Vec::new()
        };

        let status = resolve_download_status(
            force,
            updated_count,
            existing_id,
            title_changed,
            author_changed,
            story_changed,
            sections_deleted,
        );

        let id = if let Some(eid) = existing_id {
            match self.novels.get_current(eid.into()).await? {
                Some(existing) => {
                    let mut updated = existing.clone();
                    if !record.author.is_empty() {
                        updated.author = record.author.clone();
                    }
                    if !record.title.is_empty() {
                        updated.title = record.title.clone();
                    }
                    if record.has_raw_title() {
                        updated.set_raw_title(record.raw_title().to_string());
                    }
                    updated.file_title = record.file_title.clone();
                    updated.toc_url = record.toc_url.clone();
                    updated.sitename = record.sitename.clone();
                    updated.end = record.end;
                    merge_update_timestamps(&mut updated, &record, status);
                    if updated_count > 0 {
                        updated.new_arrivals_date = record.new_arrivals_date;
                    }
                    updated.use_subdirectory = record.use_subdirectory;
                    updated.general_firstup = record.general_firstup.or(updated.general_firstup);
                    updated.general_all_no = record.general_all_no;
                    updated.length = record.length.or(updated.length);
                    updated.domain = record.domain.clone();
                    updated.suspend = false;
                    updated.is_narou = record.is_narou;
                    let requires_login = auth.requires_login();
                    let login_session = auth.active().map(|active| active.id.clone());
                    if login_site_changed {
                        updated.requires_login = requires_login;
                    } else {
                        updated.requires_login |= requires_login;
                    }
                    if login_site_changed || login_session.is_some() {
                        updated.login_session = login_session;
                    }
                    // 既存の小説をページ URL 指定で取り直したときも控え直す。
                    if target != updated.toc_url && updated.original_url().is_none() {
                        updated.set_original_url(target);
                    }
                    for tag in &auto_tags {
                        if !updated.tags.contains(tag) {
                            updated.tags.push(tag.clone());
                        }
                    }
                    updated.set_attribute_tag(work_type.tag());
                    let merged_id = updated.id;
                    if updated != existing {
                        self.novels
                            .apply_batch_guarded(write.fence(), vec![NovelMutation::Upsert(updated)])
                            .await?;
                    }
                    merged_id
                }
                None => {
                    return Err(NarouError::NotFound(format!("novel removed during download: {eid}")));
                }
            }
        } else {
            let by_url = self.novels.find_by_toc_url_current(&record.toc_url).await?;
            let by_prefix = self.novels.find_by_storage_prefix_current(object_keys.prefix()).await?;
            for claimed in [by_url, by_prefix].into_iter().flatten() {
                if claimed.id != provisional_id {
                    return Err(NarouError::Platform(format!("novel_busy: novel identity was registered before record publication: {}", claimed.id)));
                }
            }
            // 新規: 先頭で確保した provisional_id をそのまま使う。ここで採番し
            // 直すと DL開始行の ID と実際のレコード ID が食い違い、誰も使わない
            // ID が 1 つ消費される (Ruby 版も Downloader 初期化時の
            // `create_new_id` をそのままレコードの ID に使う)。
            let new_id = provisional_id;
            let mut rec = record.clone();
            rec.id = new_id;
            rec.tags = auto_tags;
            rec.set_attribute_tag(work_type.tag());
            let creation = creation.as_deref_mut().ok_or_else(|| NarouError::Platform("new download lost its create claim".into()))?;
            self.novels.apply_create_guarded(write.fence(), &creation.reservation, rec).await?;
            creation.committed = true;
            new_id
        };

        if let Some(old_id) = existing_id {
            self.move_section_hash_bucket(old_id, id);
        } else {
            for (relative_path, digest) in pending_section_hashes {
                self.store_section_digest(id, &relative_path, &digest);
            }
        }
        self.flush_section_hash_cache()?;

        // Ruby's latest-only cache version has no prior section snapshots.
        // Persist it only after all content, TOC and record writes succeeded.
        if existing_id.is_some()
            && old_section_count > 0
            && updated_count > 0
            && !new_arrival_subtitles.is_empty()
            && !cache_version_recorded
            && !partial_listing
            && let Some(timestamp) = cache_timestamp(self.clock.as_ref(), save_diff_snapshots)
        {
            self.persistence.record_cache_version(&write, &object_keys, &timestamp).await?;
        }
        if budget.is_some() || resume_from_section.is_some() {
            self.persistence.clear_download_resume(&write, &object_keys).await?;
        }

        Ok(DownloadResult {
            id,
            title: toc_title.clone(),
            author: toc_author.clone(),
            novel_dir,
            new_novel: existing_id.is_none(),
            new_arrivals,
            new_arrival_subtitles,
            updated_count,
            total_count: subtitles.len(),
            status,
            title_changed,
            author_changed,
            story_changed,
            sections_deleted,
        })
        }.await;
        let _ = access.release().await;
        operation
    }

    /// The result for a novel the site no longer serves.
    ///
    /// narou.rb freezes the record and reports the novel as unavailable. A
    /// site that declares `login_wall_error: login` never reaches here: its
    /// wall stays an authentication error, so the record is not frozen.
    ///
    /// The freeze is a canonical metadata write, so it runs under the writer
    /// fence of the run that found the novel gone. A run without that fence
    /// cannot claim the record is missing, and fails instead of writing.
    async fn unavailable_result(
        &self,
        write: Option<&NovelWriteGuard>,
        id: i64,
    ) -> Result<DownloadResult> {
        self.report_line(messages::download::novel_unavailable()).await;
        #[cfg(feature = "native-runtime")]
        {
            let write = write.ok_or_else(|| {
                NarouError::DownloadResumeCorrupt(
                    "not-found update has no original writer fence".into(),
                )
            })?;
            crate::compat::mark_not_found_and_freeze(write, id)?;
        }
        #[cfg(not(feature = "native-runtime"))]
        let _ = write;
        let record = self.novels.get_current(id.into()).await?;
        let title = record
            .as_ref()
            .map(|record| record.title.clone())
            .unwrap_or_default();
        let author = record
            .as_ref()
            .map(|record| record.author.clone())
            .unwrap_or_default();
        let novel_dir = {
            // archive_root は Inventory 依存 (Phase 4 対象) のため
            // 従来どおり Database 経由で取得する。Worker は論理パスを
            // そのまま使う (freeze は native Inventory の責務)。
            #[cfg(feature = "native-runtime")]
            let archive_root =
                crate::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
                    .unwrap_or_else(|_| PathBuf::from(types::ARCHIVE_ROOT_DIR));
            #[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
            let archive_root = PathBuf::from(types::ARCHIVE_ROOT_DIR);
            record
                .as_ref()
                .map(|record| crate::db::novel_dir_for_record(&archive_root, record))
                .unwrap_or_default()
        };
        Ok(DownloadResult {
            id,
            title,
            author,
            novel_dir,
            new_novel: false,
            new_arrivals: false,
            new_arrival_subtitles: Vec::new(),
            updated_count: 0,
            total_count: 0,
            status: UpdateStatus::Failed,
            title_changed: false,
            author_changed: false,
            story_changed: false,
            sections_deleted: false,
        })
    }

    /// Try the next stored login after an authentication wall.
    ///
    /// The stored logins are read once per run, and only when a credential is
    /// selected or recovered. Every candidate starts from the site's own
    /// cookie, so one account's cookies never reach another account's
    /// request.
    async fn try_next_login(
        &mut self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
    ) -> bool {
        if auth.groups().is_none() {
            let groups = self.stored_groups_for(&*setting).await;
            auth.set_groups(groups);
        }
        if !auth.apply_next(setting) {
            return false;
        }
        self.report_line(messages::download::login_retry()).await;
        true
    }

    /// Renew the browser-issued token and send the refreshed credential.
    ///
    /// Returns `false` when no fresh token was obtained; the caller then
    /// keeps its legacy behavior of walking the stored candidates.
    async fn renew_token(
        &mut self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        target_url: &str,
    ) -> bool {
        // The result is bound before the credential is written: the request
        // for the new token must be over before the login changes.
        let refreshed = self.refresh_browser_token(&*setting, auth, target_url).await;
        match refreshed {
            Some(group) => auth.apply_refreshed(&mut *setting, group),
            None => false,
        }
    }

    /// Ask the store for a fresh browser-issued token of the site.
    ///
    /// The store answers `None` when the capability is unavailable (the
    /// storage only adapters) or when the site defines no browser token, so
    /// the caller keeps its legacy behavior of walking the stored
    /// candidates. A failure is reported and treated as "no token": the
    /// stored candidates may still work.
    ///
    /// Only the site that defines a browser token is asked, and only after a
    /// known authentication or browser-verification failure.
    async fn refresh_browser_token(
        &self,
        setting: &SiteSetting,
        auth: &mut AuthState,
        target_url: &str,
    ) -> Option<LoginGroup> {
        let cookies = self.cookies.as_ref()?;
        let config = setting.browser_token.as_ref()?;
        // One renewal per logical operation: a renewed token that is still
        // rejected must not start another one.
        if !auth.claim_token_refresh() {
            return None;
        }
        if auth.groups().is_none() {
            let groups = self.stored_groups_for(setting).await;
            auth.set_groups(groups);
        }
        // The login in effect is the one whose token is renewed; without one
        // the store picks the login that carries the token.
        let session_id = auth
            .active()
            .and_then(|login| (!login.id.is_empty()).then_some(login.id.as_str()));
        let renewed = cookies
            .refresh_browser_token(&setting.domain, target_url, &config.cookie_name, session_id)
            .await;
        match renewed {
            Ok(Some(group)) => {
                self.report_line(messages::download::browser_token_retry()).await;
                Some(group)
            }
            Ok(None) => None,
            Err(error) => {
                self.report_warn(&messages::download::token_refresh_failed(&error))
                    .await;
                None
            }
        }
    }

    /// Fetch the table of contents, retrying with the stored logins while the
    /// site answers with a login wall.
    ///
    /// A wall is the site's own login page (the `login_pattern` matched the
    /// body), a missing page or a browser-verification answer. The page is
    /// fetched again for every credential, so a wall never leaves a half
    /// fetched table of contents behind. When no stored login works, the
    /// site's wall error is returned.
    ///
    /// A browser-issued token is renewed once before the stored candidates
    /// are walked, because an expired token is not fixed by another stored
    /// login.
    async fn recover_toc_page(
        &mut self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        toc_url: &str,
        mut toc_source: Result<String>,
    ) -> Result<String> {
        loop {
            let (wall, renewable) = match &toc_source {
                Ok(source) => {
                    let wall = setting.is_login_wall(source);
                    (wall, wall)
                }
                // A missing page is not a token problem; only a
                // browser-verification answer is.
                Err(error) => (is_auth_failure(error), is_login_required(error)),
            };
            if !wall {
                return toc_source;
            }
            let renewed = renewable && self.renew_token(setting, auth, toc_url).await;
            if !renewed && !self.try_next_login(setting, auth).await {
                return match toc_source {
                    Ok(_) => Err(setting.login_wall_failure()),
                    Err(error) => Err(error),
                };
            }
            toc_source = fetch_toc(
                self.http.as_ref(),
                self.rate_limiter.as_ref(),
                setting,
                toc_url,
                &mut self.preprocess_jobs,
            )
            .await;
        }
    }

    /// Read the metadata and the episode list of an already fetched table of
    /// contents.
    async fn build_toc_attempt(
        &mut self,
        setting: &SiteSetting,
        toc_url: &str,
        url_captures: &HashMap<String, String>,
        toc_source: String,
    ) -> Result<TocAttempt> {
        let info = self
            .load_novel_info(setting, &toc_source, url_captures, toc_url)
            .await?;
        let (novel_type, is_end) =
            resolve_novel_type(setting, &toc_source, url_captures, &info).await;
        let listing = if novel_type == 2 {
            toc::TocListing {
                subtitles: create_short_story_subtitles(setting, &toc_source, &info).await?,
                partial: false,
                tag_sources: setting
                    .resolve_info_pattern("tags", &toc_source)
                    .await
                    .into_iter()
                    .collect(),
                wall: false,
            }
        } else {
            parse_subtitles_multipage(toc::MultipageTocRequest {
                http: self.http.as_ref(),
                rate_limiter: self.rate_limiter.as_ref(),
                setting,
                toc_source: &toc_source,
                url_captures,
                title: info.title.as_deref().unwrap_or_default(),
                progress: self.progress.as_deref(),
                jobs: &mut self.preprocess_jobs,
            })
            .await?
        };
        Ok(TocAttempt {
            source: toc_source,
            info,
            novel_type,
            is_end,
            listing,
        })
    }

    /// Fetch the table of contents and its episode list with the credential in
    /// effect, switching to the next stored login while the site asks for
    /// authentication.
    ///
    /// `source` is the first page that was already fetched; later rounds
    /// fetch it again, because the credential changed.
    async fn fetch_attempt_with_logins(
        &mut self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        toc_url: &str,
        url_captures: &HashMap<String, String>,
        source: Option<String>,
    ) -> Result<TocAttempt> {
        let mut source = source;
        loop {
            let page = match source.take() {
                Some(page) => Ok(page),
                None => fetch_toc(
                    self.http.as_ref(),
                    self.rate_limiter.as_ref(),
                    &*setting,
                    toc_url,
                    &mut self.preprocess_jobs,
                )
                .await,
            };
            let page = match page {
                Ok(page) => page,
                Err(error) if is_auth_failure(&error) => {
                    // ページが無い / ブラウザ確認の答えなら、次の保存済み
                    // ログインで読み直す。失効したトークンは先に更新する。
                    let renewed = is_login_required(&error)
                        && self.renew_token(setting, auth, toc_url).await;
                    if !renewed && !self.try_next_login(setting, auth).await {
                        return Err(error);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            if setting.is_login_wall(&page) {
                let renewed = self.renew_token(setting, auth, toc_url).await;
                if !renewed && !self.try_next_login(setting, auth).await {
                    return Err(setting.login_wall_failure());
                }
                continue;
            }
            match self
                .build_toc_attempt(&*setting, toc_url, url_captures, page)
                .await
            {
                Ok(attempt) => return Ok(attempt),
                Err(error) if is_auth_failure(&error) => {
                    // 続きのページが壁だった場合も、次のログインで最初の一
                    // ページから読み直す。失効したトークンは先に更新する。
                    let renewed = is_login_required(&error)
                        && self.renew_token(setting, auth, toc_url).await;
                    if !renewed && !self.try_next_login(setting, auth).await {
                        return Err(error);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Read the episode list, retrying with the stored logins while the site
    /// answers with a login wall or an incomplete list.
    ///
    /// The walk starts again from the first page for every credential, so a
    /// truncated listing is never stored. The listing of the credential that
    /// answered completely wins; an incomplete listing is kept only when no
    /// stored login does better.
    async fn recover_toc_listing(
        &mut self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        toc_url: &str,
        url_captures: &HashMap<String, String>,
        first_source: String,
    ) -> Result<TocAttempt> {
        // 採用した一覧の資格情報。候補が採用されなかったときは戻す。
        let mut original = auth.active().cloned();
        let mut best: Option<TocAttempt> = None;
        let mut source = Some(first_source);
        loop {
            let attempt = match self
                .fetch_attempt_with_logins(setting, auth, toc_url, url_captures, source.take())
                .await
            {
                Ok(attempt) => attempt,
                Err(error) => {
                    // An optional login cannot invalidate an earlier usable listing.
                    auth.restore(setting, original.as_ref());
                    return best.ok_or(error);
                }
            };
            if attempt.listing.wall {
                // 壁が残った。失効したトークンを先に更新し、それもだめなら
                // 保存済みログインを順に試す。
                let renewed = self.renew_token(setting, auth, toc_url).await;
                if !renewed && !self.try_next_login(setting, auth).await {
                    auth.restore(setting, original.as_ref());
                    return best.ok_or_else(|| setting.login_wall_failure());
                }
                continue;
            }
            let better = match &best {
                None => true,
                Some(best) => {
                    !attempt.listing.subtitles.is_empty()
                        && ((!attempt.listing.partial && best.listing.partial)
                            || attempt.listing.subtitles.len() > best.listing.subtitles.len())
                }
            };
            if better {
                best = Some(attempt);
                original = auth.active().cloned();
            } else {
                // 採用されなかった候補は元の資格情報へ戻す。
                auth.restore(setting, original.as_ref());
            }
            let complete = best
                .as_ref()
                .is_some_and(|best| !best.listing.partial && !best.listing.subtitles.is_empty());
            if complete {
                return Ok(best.expect("a listing was built"));
            }
            // 一覧が欠けている (Pixiv の小説シリーズは未ログインだと 0 件に
            // なる)。保存済みログインを順に試す。
            if !self.try_next_login(setting, auth).await {
                auth.restore(setting, original.as_ref());
                return Ok(best.expect("a listing was built"));
            }
        }
    }

    /// Fetch a section body, retrying with the stored logins while the site
    /// answers with a login wall.
    ///
    /// The login that works stays in effect for the remaining bodies and is
    /// recorded with the novel, so later runs start with it.
    ///
    /// Every body may renew the browser-issued token once, so a long work can
    /// renew it again after the site invalidated it.
    async fn download_section_with_logins(
        &mut self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        subtitle: &SubtitleInfo,
        toc_url: &str,
    ) -> Result<(SectionElement, String)> {
        // The renewal budget belongs to this body: the next body starts with
        // a fresh one.
        auth.reset_token_refresh();
        loop {
            let attempt = download_section(
                self.http.as_ref(),
                self.rate_limiter.as_ref(),
                &mut self.section_cache,
                setting,
                subtitle,
                toc_url,
                &mut self.preprocess_jobs,
            )
            .await;
            match attempt {
                Ok(section) => return Ok(section),
                Err(error) if is_login_required(&error) => {
                    let body_url = build_section_url(&*setting, toc_url, &subtitle.href);
                    let renewed = self.renew_token(setting, auth, &body_url).await;
                    if !renewed && !self.try_next_login(setting, auth).await {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// 戻り値: `(要 DL, 強更新の比較で取得済みなら (本文, raw))`。
    ///
    /// 取得はセクションループと同じ `download_section` を通るので、
    /// 比較済みの本文を保存・挿絵走査へ渡せば二重取得は起きない。
    async fn section_needs_download(
        &mut self,
        access: &mut NovelAccess,
        probe: SectionDecision<'_>,
    ) -> Result<(bool, Option<(SectionElement, String)>)> {
        let SectionDecision {
            setting,
            auth,
            latest,
            old,
            existing_id,
            keys,
            toc_url,
            strong_update,
            timezone,
            existed,
        } = probe;
        let Some(old) = old else {
            return Ok((true, None));
        };

        // file_subtitle が変わると公開する目次が別の本文キーを参照するよう
        // なるため、日付が変わっていなくても本文を書き直す。旧サニタイザの
        // `_` から Ruby 互換の全角写像への移行がこれに該当する。
        if old.subtitle != latest.subtitle
            || old.chapter != latest.chapter
            || old.file_subtitle != latest.file_subtitle
        {
            return Ok((true, None));
        }

        // A missing section file forces a re-download even when the dates say
        // otherwise. Existence comes from the pre-computed listing set, so the
        // common path performs no storage I/O at all.
        if !existed {
            return Ok((true, None));
        }

        let latest_subupdate = latest.subupdate.as_deref();
        let mut old_subupdate = old.subupdate.as_deref();
        if latest_subupdate.is_some() && old_subupdate.is_none() {
            old_subupdate = Some(old.subdate.as_str());
        }

        let (date_says_update, strong_basis_date) = if let (
            Some(old_subupdate),
            Some(latest_subupdate),
        ) = (old_subupdate, latest_subupdate)
        {
            if old_subupdate.is_empty() {
                return Ok((!latest_subupdate.is_empty(), None));
            }
            (
                date_string_is_newer_with_timezone(latest_subupdate, old_subupdate, timezone),
                Some(old_subupdate),
            )
        } else {
            if old.subdate.is_empty() {
                return Ok((true, None));
            }
            (
                date_string_is_newer_with_timezone(&latest.subdate, &old.subdate, timezone),
                Some(old.subdate.as_str()),
            )
        };

        if !date_says_update {
            return Ok((false, None));
        }

        if strong_update && let Some(basis_date) = strong_basis_date {
            let Some(old_section) = self.persistence.load_section(access.ensure_read().await?, keys, old).await? else {
                return Ok((true, None));
            };
            if date_string_to_ymd_with_timezone(basis_date, timezone)
                == section_timestamp_ymd(old_section.download_time.as_deref(), timezone)
            {
                // The comparison fetch can outlive the 30s lease TTL; release
                // the idle lease first so it does not stall a writer drain.
                let _ = access.release_read().await;
                let downloaded = self
                    .download_section_with_logins(setting, auth, latest, toc_url)
                    .await?;
                let new_hash = compute_section_hash(&downloaded.0);
                let relative_path = section_relative_path(old);
                let old_hash = existing_id
                    .and_then(|id| {
                        self.cached_section_digest(id, &relative_path)
                            .map(str::to_string)
                    })
                    .or_else(|| {
                        let digest = compute_section_hash(&old_section.element);
                        if let Some(id) = existing_id {
                            self.store_section_digest(id, &relative_path, &digest);
                        }
                        Some(digest)
                    });
                if old_hash.as_deref() == Some(new_hash.as_str()) {
                    if let Some(id) = existing_id {
                        self.store_section_digest(id, &relative_path, &new_hash);
                    }
                    // 内容が同じなので保存は不要。予定段階で取得した本文で
                    // 挿絵の走査だけ行えるよう、取得結果を返す。
                    return Ok((false, Some(downloaded)));
                }
                return Ok((true, Some(downloaded)));
            }
        }

        Ok((true, None))
    }

    fn ageauth_redirect_target(url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        if parsed.host_str() != Some("nl.syosetu.com") || parsed.path() != "/redirect/ageauth/" {
            return None;
        }
        let target = parsed
            .query_pairs()
            .find_map(|(key, value)| (key == "url").then(|| value.into_owned()))?;
        is_safe_public_url_syntax(&target).then_some(target)
    }

    async fn resolve_target_for_download(
        &self,
        target: &str,
    ) -> Result<(Option<i64>, &SiteSetting)> {
        let target_type = Self::get_target_type(target);

        match target_type {
            TargetType::Url => {
                let setting = self.site_setting_for_url(target).ok_or_else(|| {
                    NarouError::InvalidTarget(format!("No site setting for URL: {}", target))
                })?;
                let toc_url = setting
                    .toc_url_with_url_captures(target)
                    .unwrap_or_else(|| setting.toc_url());
                let existing_id = self
                    .novels
                    .find_by_toc_url_current(&toc_url)
                    .await?
                    .map(|record| record.id);
                Ok((existing_id, setting))
            }
            TargetType::Ncode => {
                let ncode = target.to_lowercase();
                // 既存レコードはその toc_url から、見つからない場合は合成した
                // なろう URL から、取得先定義と TOC URL を決める。URL → レコード
                // の照合はどちらの分岐も同じ 1 経路に集約する。
                let (known_id, setting, toc_url) =
                    match self.novels.find_by_ncode(&ncode).await? {
                        Some(record) => {
                            let record = self.novels.get_current(record.id.into()).await?.ok_or_else(|| {
                                NarouError::NotFound("novel disappeared during ncode discovery".into())
                            })?;
                            let setting = self.site_setting_for_stored_target(&record, TargetType::Ncode)?;
                            (Some(record.id), setting, record.toc_url)
                        }
                        None => {
                            let narou_url = narou_ncode_url(&ncode);
                            let setting = Self::site_setting_for_new_ncode_in(&self.site_settings, &narou_url, &ncode)?;
                            let toc_url = setting
                                .toc_url_with_url_captures(&narou_url)
                                .unwrap_or_else(|| setting.toc_url());
                            (None, setting, toc_url)
                        }
                    };
                let existing_id = match known_id {
                    Some(id) => Some(id),
                    None => self
                        .novels
                        .find_by_toc_url_current(&toc_url)
                        .await?
                        .map(|record| record.id),
                };
                Ok((existing_id, setting))
            }
            TargetType::Id => {
                let id: i64 = target
                    .parse()
                    .map_err(|_| NarouError::InvalidTarget(target.to_string()))?;
                let record = self.novels.get_current(id.into()).await?.ok_or_else(|| {
                    NarouError::NotFound(format!("Novel not found for ID: {}", id))
                })?;
                let setting = self.site_setting_for_stored_target(&record, TargetType::Id)?;
                Ok((Some(id), setting))
            }
            TargetType::Other => {
                let existing_id = self
                    .novels
                    .find_by_title(target)
                    .await?
                    .or(self.novels.find_by_ncode(target).await?)
                    .map(|record| record.id);
                if let Some(id) = existing_id {
                    let record = self.novels.get_current(id.into()).await?.ok_or_else(|| {
                        NarouError::NotFound(format!("Novel record {} has no toc_url", id))
                    })?;
                    let setting = self.site_setting_for_url(&record.toc_url).ok_or_else(|| {
                        NarouError::SiteSetting("No matching site setting".into())
                    })?;
                    Ok((Some(id), setting))
                } else {
                    Err(NarouError::NotFound(format!(
                        "Novel not found: {} (use URL for new downloads)",
                        target
                    )))
                }
            }
        }
    }

    async fn extract_ncode(&self, setting: &SiteSetting, toc_source: &str) -> Option<String> {
        let url_pattern = {
            let re = regex::Regex::new(r"(?i)[/?](n\d+[a-z]+)").ok()?;
            re.captures(&setting.toc_url())
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_lowercase())
        };
        match url_pattern {
            Some(value) => Some(value),
            None => setting.resolve_info_pattern("ncode", toc_source).await,
        }
    }

    fn compute_file_title(
        &self,
        ncode: &Option<String>,
        title: &str,
        append_title: bool,
        existing_record: Option<&NovelRecord>,
    ) -> String {
        if let Some(record) = existing_record
            && !record.file_title.is_empty()
        {
            return record.file_title.clone();
        }

        if let Some(ncode) = ncode {
            if !append_title {
                return ncode.clone();
            }
            let limit = load_length_limit("folder-length-limit", Some(50));
            let combined_title = ruby_filename_special_chars(&format!("{} {}", ncode, title));
            let sanitized = sanitize_filename_with_limit(&combined_title, limit);
            if sanitized.is_empty() {
                ncode.clone()
            } else {
                sanitized
            }
        } else {
            sanitize_filename_with_limit(
                &ruby_filename_special_chars(title),
                load_length_limit("folder-length-limit", Some(50)),
            )
        }
    }

    fn compute_novel_dir(
        &self,
        sitename: &str,
        file_title: &str,
        use_subdirectory: bool,
    ) -> PathBuf {
        let root = PathBuf::from(types::ARCHIVE_ROOT_DIR);
        crate::db::paths::novel_dir_from_components(&root, sitename, file_title, use_subdirectory)
    }

    fn download_use_subdirectory(&self, existing_record: Option<&NovelRecord>) -> bool {
        if let Some(record) = existing_record {
            return record.use_subdirectory;
        }

        self.settings.download_use_subdirectory()
    }

    fn cached_section_digest(&self, id: i64, relative_path: &str) -> Option<&str> {
        self.section_hash_cache
            .get(&id.to_string())
            .and_then(|bucket| bucket.get(relative_path))
            .map(String::as_str)
    }

    fn store_section_digest(&mut self, id: i64, relative_path: &str, digest: &str) {
        let bucket = self.section_hash_cache.entry(id.to_string()).or_default();
        if bucket.get(relative_path).map(String::as_str) != Some(digest) {
            bucket.insert(relative_path.to_string(), digest.to_string());
            self.section_hash_cache_dirty = true;
        }
    }

    fn clear_section_digest(&mut self, id: i64, relative_path: &str) {
        let key = id.to_string();
        let should_remove_bucket = if let Some(bucket) = self.section_hash_cache.get_mut(&key) {
            if bucket.remove(relative_path).is_some() {
                self.section_hash_cache_dirty = true;
                bucket.is_empty()
            } else {
                false
            }
        } else {
            false
        };
        if should_remove_bucket {
            self.section_hash_cache.remove(&key);
        }
    }

    fn move_section_hash_bucket(&mut self, from_id: i64, to_id: i64) {
        if from_id == to_id {
            return;
        }
        if let Some(bucket) = self.section_hash_cache.remove(&from_id.to_string())
            && !bucket.is_empty()
        {
            self.section_hash_cache.insert(to_id.to_string(), bucket);
            self.section_hash_cache_dirty = true;
        }
    }

    fn flush_section_hash_cache(&mut self) -> Result<()> {
        if !self.section_hash_cache_dirty {
            return Ok(());
        }
        self.settings
            .save_section_hash_cache(&self.section_hash_cache)?;
        self.section_hash_cache_dirty = false;
        Ok(())
    }

    pub fn set_progress(&mut self, progress: Box<dyn ProgressReporter>) {
        self.progress = Some(progress);
    }

    /// Close the current operation before a job's console sender is drained.
    pub async fn clear_progress(&mut self) {
        if let Some(progress) = self.progress.take() {
            progress.finish_with_message_async("").await;
        }
    }

    /// ユーザーに見える行の送り先を差し替える。native のコマンドは
    /// `progress::console_sink()` (コンソールとログの両方へ書く)、Worker の
    /// ジョブ実行は PushHub へ `echo` する sink を渡す。未設定のときは cfg 別の
    /// フォールバックで従来動作を維持する。
    pub fn set_message_sink(&mut self, sink: Arc<dyn crate::application::messages::MessageSink>) {
        self.message_sink = Some(sink);
    }

    /// Release invocation-owned routing before reusing a downloader.
    pub fn clear_message_sink(&mut self) {
        self.message_sink = None;
    }

    /// `println!` (stdout 行) 相当の送出。
    async fn report_line(&self, line: &str) {
        use crate::application::messages::Stream;
        match &self.message_sink {
            Some(sink) => sink.emit_async(Stream::Stdout, line).await,
            None => default_report_line(line),
        }
    }

    /// `eprintln!` (stderr 行) 相当の送出。
    async fn report_warn(&self, line: &str) {
        use crate::application::messages::Stream;
        match &self.message_sink {
            Some(sink) => sink.emit_async(Stream::Stderr, line).await,
            None => default_report_warn(line),
        }
    }

    pub fn site_setting_matches_url(&self, url: &str) -> bool {
        self.find_site_setting(url).is_some()
    }
}

/// Merge the stored login cookie into the site's static cookie value.
fn apply_login_cookie(
    setting: &mut SiteSetting,
    login_cookie: &str,
    cookies: &[crate::platform::HostCookie],
) {
    setting.cookie = crate::platform::merge_cookie_headers(setting.cookie(), Some(login_cookie));
    setting.login_cookie_hosts = cookies.iter().map(|entry| entry.host.clone()).collect();
}

/// Whether the error means the site wants a credential rather than a broken
/// fetch.
///
/// A missing page (404) and a browser-verification answer both mean the fetch
/// may need authentication; every other error is a real failure.
fn is_auth_failure(error: &NarouError) -> bool {
    matches!(error, NarouError::NotFound(_) | NarouError::Login(_))
}

/// Whether the error means a body needs a credential.
///
/// Only a login wall or a browser-verification answer does. A missing body
/// stays the plain failure it always was.
fn is_login_required(error: &NarouError) -> bool {
    matches!(error, NarouError::Login(_))
}

#[cfg(test)]
mod tests {
    use super::novel_info::NovelInfo;
    use super::persistence::PersistenceService;
    use super::site_setting::{SiteSetting, SiteSettingValue};
    use super::types::{SectionElement, TocFile};
    use super::util::compile_html_pattern;
    use super::{
        DownloadExecutionOptions, Downloader, DownloaderPlatform, Over18AccessDecision,
        SectionBudget, illustration_sources, over18_access_decision, requires_over18_confirmation,
        resolve_novel_type, resolve_user_agent,
    };
    use crate::db::{self, Database};
    use crate::downloader::SectionFile;
    use crate::error::{NarouError, Result};
    use crate::platform::{NovelId, NovelIdReservation, NovelRepository, ObjectKey};
    use crate::platform::mocks::{
        FakeRateLimiter, MemoryCookieStore, MemoryNovelRepository, MemoryObjectStore,
        MockHttpClient,
    };
    use crate::platform::cookie_store::LoginGroup;
    use crate::platform::HostCookie;
    use crate::downloader::settings::DownloaderSettings;
    use crate::platform::{
        AssetStore, AssetStream, CookieStore, HttpResponse, NovelMutation, NovelObjectKeys, ObjectListRequest,
        ObjectMetadata, ObjectPrefix, ObjectStore, PlatformFuture, SystemClock,
    };
    use base64::Engine;
    use chrono::TimeZone;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::application::novel_access::{AccessOrigin, NovelReadLease, NovelWriteGuard, NovelReadScope};

    async fn test_read(service: &PersistenceService, keys: &NovelObjectKeys) -> NovelReadLease {
        NovelReadLease::admit_for(service.objects(), keys, "downloader-test").await.unwrap()
    }
    async fn test_write(service: &PersistenceService, keys: &NovelObjectKeys) -> NovelWriteGuard {
        NovelWriteGuard::capture_for(service.objects(), None, keys).await.unwrap()
    }
    async fn test_origin(downloader: &Downloader, keys: &NovelObjectKeys) -> NovelWriteGuard {
        NovelWriteGuard::capture_for(downloader.persistence.objects(), Some(&downloader.assets), keys).await.unwrap()
    }

    #[tokio::test]
    async fn default_creation_restore_requires_existing_exact_allocator_proof() {
        struct NoYield;
        impl SectionBudget for NoYield { fn should_yield(&mut self, _: usize) -> bool { false } }
        let mut baseline = sample_record(chrono::Utc::now());
        baseline.id = 0;
        let novels = MemoryNovelRepository::from_records(vec![baseline]);
        let mut budget = NoYield;
        assert!(budget.restore_creation_id(&novels, NovelId(1), 1).await.is_err());
        let allocated = novels.allocate_id().await.unwrap();
        assert_eq!(allocated.id(), NovelId(1), "missing proof cannot allocate a replacement");
        let (id, generation) = (allocated.id(), allocated.generation());
        allocated.adopt();
        assert!(budget.restore_creation_id(&novels, id, generation + 1).await.is_err());
        let restored = budget.restore_creation_id(&novels, id, generation).await.unwrap();
        assert_eq!((restored.id(), restored.generation()), (id, generation));
        novels.release_id(restored).await.unwrap();
        assert!(budget.restore_creation_id(&novels, id, generation).await.is_err());
    }

    #[tokio::test]
    async fn creation_restore_without_budget_rejects_missing_proof_before_http() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, _objects, novels, http, _keys) = creation_download_fixture();
        let error = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: None, access: None,
            creation_reservation: Some((NovelId(1), 1)),
        }).await.unwrap_err();
        assert!(http.requested_urls().is_empty(), "proof restoration must fail before HTTP: {error}");
        let next = novels.allocate_id().await.unwrap();
        assert_eq!(next.id(), NovelId(1), "failed restore cannot consume a fresh identity");
        novels.release_id(next).await.unwrap();
    }

    struct CheckpointBudget<B> {
        inner: B,
        checkpoint: Arc<MemoryObjectStore>,
        creation_reservation: Option<(crate::platform::NovelId, i64)>,
        allocations: usize,
        restorations: usize,
    }

    impl<B> CheckpointBudget<B> {
        fn new(inner: B, checkpoint: Arc<MemoryObjectStore>) -> Self {
            Self { inner, checkpoint, creation_reservation: None, allocations: 0, restorations: 0 }
        }
    }

    impl<B: SectionBudget> SectionBudget for CheckpointBudget<B> {
        fn job_owner(&self) -> Option<Arc<crate::platform::JobExecutionOwner>> { self.inner.job_owner() }
        fn allocate_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository) -> PlatformFuture<'a, Result<(NovelIdReservation, bool)>> {
            self.allocations += 1;
            self.inner.allocate_creation_id(novels)
        }
        fn restore_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository, id: NovelId, generation: i64) -> PlatformFuture<'a, Result<NovelIdReservation>> {
            self.restorations += 1;
            self.inner.restore_creation_id(novels, id, generation)
        }
        fn set_access_origin<'a>(&'a mut self, origin: &'a AccessOrigin, creation: Option<(crate::platform::NovelId, i64)>) -> PlatformFuture<'a, Result<bool>> {
            Box::pin(async move {
                let key = ObjectKey::try_new(".narou-transfer/test-create-checkpoint")?;
                if let Some(bytes) = self.checkpoint.read_small(&key).await? {
                    let (saved_origin, saved_creation): (AccessOrigin, Option<(crate::platform::NovelId, i64)>) = serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(error.to_string()))?;
                    if saved_origin != *origin || saved_creation != creation {
                        return Err(NarouError::Platform("checkpoint ownership changed during continuation".into()));
                    }
                }
                let bytes = serde_json::to_vec(&(origin, creation)).map_err(|error| NarouError::Platform(error.to_string()))?;
                self.checkpoint.write_small(&key, bytes).await?;
                self.creation_reservation = creation;
                Ok(true)
            })
        }
        fn should_yield(&mut self, next_section: usize) -> bool { self.inner.should_yield(next_section) }
        fn check_active(&mut self) -> PlatformFuture<'_, Result<()>> { self.inner.check_active() }
    }

    fn creation_download_fixture() -> (Downloader, Arc<MemoryObjectStore>, Arc<MemoryNovelRepository>, Arc<MockHttpClient>, NovelObjectKeys) {
        let mut setting: SiteSetting = serde_yaml::from_str(r#"
name: Creation Identity
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Creation Identity
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)" data-date="(?<subdate>[^"]+)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#).unwrap();
        setting.compile();
        let http = Arc::new(MockHttpClient::new());
        http.add_text("https://example.com/n1234ab/", 200, r#"<h1>Creation Identity</h1><span>Author</span><a href="/n1234ab/1/" data-date="2000-01-01">第1話</a>"#);
        http.add_text("https://example.com/n1234ab/1/", 200, r#"<div class="body">持続本文</div>"#);
        let objects = Arc::new(MemoryObjectStore::new());
        let mut baseline = sample_record(chrono::Utc::now());
        baseline.id = 0;
        baseline.toc_url = "https://example.com/n0000aa/".into();
        let novels = Arc::new(MemoryNovelRepository::from_records(vec![baseline]).with_objects(objects.clone()));
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: novels.clone(), objects: objects.clone(), assets: objects.clone(),
                clock: Arc::new(SystemClock),
            }, vec![setting], HashMap::new(),
        ).unwrap();
        (downloader, objects, novels, http, NovelObjectKeys::new("Creation Identity", "n1234ab", false).unwrap())
    }

    #[tokio::test]
    async fn pending_creation_claim_rejects_another_identity_before_canonical_writes() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let target = "https://example.com/n1234ab/";
        for (same_prefix, same_url) in [(true, true), (true, false), (false, true)] {
            let (mut downloader, objects, novels, http, keys) = creation_download_fixture();
            let owner_keys = if same_prefix { keys.clone() } else { NovelObjectKeys::new("Other namespace", "n1234ab", false).unwrap() };
            let owner = novels.allocate_id().await.unwrap();
            let owner_id = owner.id();
            let owner_generation = owner.generation();
            let write = test_origin(&downloader, &owner_keys).await;
            let owner_url = if same_url { target } else { "https://other.example.com/n1234ab/" };
            novels.reserve_create(write.fence(), &owner, owner_url).await.unwrap();
            let owner_body = owner_keys.section("1", "第1話");
            write.write_small(&owner_body, b"reserved owner's section".to_vec()).await.unwrap();
            let candidate_body = keys.section("1", "第1話");
            let before = ObjectStore::read_small(objects.as_ref(), &candidate_body).await.unwrap();
            let error = downloader.download_novel(target).await.unwrap_err();
            assert!(!matches!(error, NarouError::DownloadBudgetExpired { .. } | NarouError::DownloadResumeCorrupt(_)), "claim collision must fail before section progress: {error}");
            assert_eq!(ObjectStore::read_small(objects.as_ref(), &candidate_body).await.unwrap(), before);
            assert_eq!(ObjectStore::read_small(objects.as_ref(), &owner_body).await.unwrap(), Some(b"reserved owner's section".to_vec()));
            assert!(ObjectStore::read_small(objects.as_ref(), &keys.toc()).await.unwrap().is_none());
            assert!(ObjectStore::read_small(objects.as_ref(), &keys.converted_text()).await.unwrap().is_none());
            assert!(!http.requested_urls().iter().any(|url| url.ends_with("/n1234ab/1/")));
            write.write_small(&owner_body, b"owner remains live".to_vec()).await.unwrap();
            let released_loser = novels.allocate_id().await.unwrap();
            assert_eq!(released_loser.id().0, owner_id.0 + 1, "the losing non-durable reservation must be released, not the winner");
            assert!(novels.get_current(owner_id).await.unwrap().is_none());
            novels.release_id(released_loser).await.unwrap();
            if let Err(error) = novels.release_id(owner).await {
                let pending = objects.access_fence().unwrap().pending_removals(None, 100).await.unwrap().0;
                let expected_prefix = crate::platform::ObjectPrefix::from(owner_keys.prefix().clone());
                let (_, removal_owner, closed_epoch) = pending.iter().find(|(prefix, _, _)| prefix == &expected_prefix)
                    .unwrap_or_else(|| panic!("release failed without durable prefix cleanup intent: {error}"));
                let (removed_id, delete_files) = crate::platform::access_fence::parse_removal_owner(removal_owner).unwrap();
                assert_eq!(removed_id, owner_id);
                assert!(delete_files);
                assert_eq!(*closed_epoch, write.origin().epoch + 1);
                let freeze = Arc::new(crate::application::MemoryFreezeMutationStore::new());
                let actions = crate::application::NovelActionService::new(novels.clone(), freeze.clone(), freeze, Some(objects.clone()));
                assert!(actions.recover_removals(None, 100, 100).await.unwrap().is_none());
                novels.release_id(NovelIdReservation::new(owner_id, owner_generation)).await.unwrap();
            }
            assert!(ObjectStore::read_small(objects.as_ref(), &owner_body).await.unwrap().is_none(), "unpublished bytes must be purged before releasing their namespace");
            assert!(write.write_small(&owner_body, b"late abandoned writer".to_vec()).await.is_err());
        }
    }

    #[tokio::test]
    async fn lost_creation_checkpoint_ack_keeps_the_same_identity_for_retry() {
        struct NoYield;
        impl SectionBudget for NoYield { fn should_yield(&mut self, _: usize) -> bool { false } }
        struct LostAcknowledgement(CheckpointBudget<NoYield>);
        impl SectionBudget for LostAcknowledgement {
            fn job_owner(&self) -> Option<Arc<crate::platform::JobExecutionOwner>> { self.0.job_owner() }
            fn allocate_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository) -> PlatformFuture<'a, Result<(NovelIdReservation, bool)>> { self.0.allocate_creation_id(novels) }
            fn restore_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository, id: NovelId, generation: i64) -> PlatformFuture<'a, Result<NovelIdReservation>> { self.0.restore_creation_id(novels, id, generation) }
            fn should_yield(&mut self, _: usize) -> bool { false }
            fn set_access_origin<'a>(&'a mut self, origin: &'a AccessOrigin, creation: Option<(crate::platform::NovelId, i64)>) -> PlatformFuture<'a, Result<bool>> {
                Box::pin(async move {
                    self.0.set_access_origin(origin, creation).await?;
                    Err(NarouError::Platform("checkpoint acknowledgement lost".into()))
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, objects, novels, http, keys) = creation_download_fixture();
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = LostAcknowledgement(CheckpointBudget::new(NoYield, checkpoint.clone()));
        let error = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: None, creation_reservation: None,
        }).await.unwrap_err();
        assert!(error.to_string().contains("checkpoint acknowledgement lost"));
        let bytes = ObjectStore::read_small(checkpoint.as_ref(), &ObjectKey::try_new(".narou-transfer/test-create-checkpoint").unwrap()).await.unwrap().unwrap();
        let (origin, creation): (AccessOrigin, Option<(crate::platform::NovelId, i64)>) = serde_json::from_slice(&bytes).unwrap();
        let (id, generation) = creation.unwrap();
        assert_eq!(budget.0.creation_reservation, creation);
        assert!(generation > 0);
        assert!(novels.get_current(id).await.unwrap().is_none());
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.toc()).await.unwrap().is_none());
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap().is_none());
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.converted_text()).await.unwrap().is_none());
        assert!(!http.requested_urls().iter().any(|url| url.ends_with("/n1234ab/1/")));
        let other = novels.allocate_id().await.unwrap();
        assert_ne!(other.id(), id, "an ambiguous persisted checkpoint must keep its reservation");
        novels.release_id(other).await.unwrap();
        let objects_port: Arc<dyn ObjectStore> = objects.clone();
        let assets_port: Arc<dyn AssetStore> = objects.clone();
        let resumed = NovelWriteGuard::resume(objects_port, Some(assets_port), &origin).await.unwrap();
        let result = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: None, access: Some(resumed), creation_reservation: creation,
        }).await.unwrap();
        assert_eq!(result.id, id.0);
        assert_eq!(novels.get_current(id).await.unwrap().unwrap().toc_url, "https://example.com/n1234ab/");
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.toc()).await.unwrap().is_some());
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn published_creation_replays_its_checkpoint_without_reallocating_or_revoking() {
        struct NoYield;
        impl SectionBudget for NoYield { fn should_yield(&mut self, _: usize) -> bool { false } }
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, objects, novels, http, keys) = creation_download_fixture();
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = CheckpointBudget::new(NoYield, checkpoint.clone());
        let first = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: None, creation_reservation: None,
        }).await.unwrap();
        let bytes = ObjectStore::read_small(checkpoint.as_ref(), &ObjectKey::try_new(".narou-transfer/test-create-checkpoint").unwrap()).await.unwrap().unwrap();
        let (origin, creation): (AccessOrigin, Option<(crate::platform::NovelId, i64)>) = serde_json::from_slice(&bytes).unwrap();
        let (id, generation) = creation.unwrap();
        assert_eq!(first.id, id.0);
        let body = ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap().unwrap();
        let missing_origin = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: None, access: None, creation_reservation: creation,
        }).await.unwrap_err();
        assert!(matches!(missing_origin, NarouError::DownloadResumeCorrupt(_)), "a consumed reservation cannot recapture a newer writer");
        let objects_port: Arc<dyn ObjectStore> = objects.clone();
        let assets_port: Arc<dyn AssetStore> = objects.clone();
        let resumed = NovelWriteGuard::resume(objects_port.clone(), Some(assets_port.clone()), &origin).await.unwrap();
        let replay = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: Some(resumed), creation_reservation: creation,
        }).await.unwrap();
        assert_eq!(replay.id, id.0);
        assert_eq!(budget.creation_reservation, creation);
        assert_eq!(budget.restorations, 1, "published replay rehydrates the saved reservation capability");
        assert_eq!(ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap(), Some(body));
        assert_eq!(http.requested_urls().iter().filter(|url| url.ends_with("/n1234ab/1/")).count(), 1);
        novels.release_id(crate::platform::NovelIdReservation::new(id, generation)).await.unwrap();
        let still_current = NovelWriteGuard::resume(objects_port, Some(assets_port), &origin).await.unwrap();
        still_current.write_small(&keys.converted_text(), b"same published generation".to_vec()).await.unwrap();
        assert_eq!(ObjectStore::read_small(objects.as_ref(), &keys.converted_text()).await.unwrap(), Some(b"same published generation".to_vec()));
        let next = novels.allocate_id().await.unwrap();
        assert_ne!(next.id(), id, "successful replay must not free the published identity");
        novels.release_id(next).await.unwrap();
    }

    #[tokio::test]
    async fn numeric_only_creation_checkpoint_binds_its_first_origin_without_reallocation() {
        struct NoYield;
        impl SectionBudget for NoYield { fn should_yield(&mut self, _: usize) -> bool { false } }
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, objects, novels, http, keys) = creation_download_fixture();
        let reservation = novels.allocate_id().await.unwrap();
        let creation = Some((reservation.id(), reservation.generation()));
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let numeric_key = ObjectKey::try_new(".narou-transfer/test-create-numeric-checkpoint").unwrap();
        ObjectStore::write_small(checkpoint.as_ref(), &numeric_key, serde_json::to_vec(&creation).unwrap()).await.unwrap();
        let mut budget = CheckpointBudget::new(NoYield, checkpoint.clone());
        let invalid = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: Some(0), access: None, creation_reservation: creation,
        }).await.unwrap_err();
        assert!(matches!(invalid, NarouError::DownloadResumeCorrupt(_)));
        assert!(http.requested_urls().is_empty(), "a body cursor cannot exist before first prefix capture");
        assert_eq!(budget.allocations, 0);
        assert_eq!(budget.restorations, 0);
        let result = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: None, creation_reservation: creation,
        }).await.unwrap();
        assert_eq!(result.id, reservation.id().0);
        assert_eq!(budget.allocations, 0, "a planned durable tuple must never allocate a replacement");
        assert_eq!(budget.creation_reservation, creation);
        assert_eq!(budget.restorations, 1);
        let bytes = ObjectStore::read_small(checkpoint.as_ref(), &ObjectKey::try_new(".narou-transfer/test-create-checkpoint").unwrap()).await.unwrap().unwrap();
        let (origin, saved): (AccessOrigin, Option<(crate::platform::NovelId, i64)>) = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved, creation);
        assert_eq!(origin.prefix, keys.prefix().as_ref());
        let objects_port: Arc<dyn ObjectStore> = objects.clone();
        let assets_port: Arc<dyn AssetStore> = objects;
        NovelWriteGuard::resume(objects_port, Some(assets_port), &origin).await.unwrap();
        reservation.adopt();
    }

    #[tokio::test]
    async fn durable_allocation_owns_failure_before_any_prefix_is_captured() {
        struct EarlyOwner {
            checkpoint: Arc<MemoryObjectStore>,
            creation: Option<(crate::platform::NovelId, i64)>,
        }
        impl SectionBudget for EarlyOwner {
            fn should_yield(&mut self, _: usize) -> bool { false }
            fn allocate_creation_id<'a>(&'a mut self, novels: &'a dyn NovelRepository) -> PlatformFuture<'a, Result<(NovelIdReservation, bool)>> {
                Box::pin(async move {
                    let reservation = novels.allocate_id().await?;
                    let creation = (reservation.id(), reservation.generation());
                    let key = ObjectKey::try_new(".narou-transfer/test-create-early-owned")?;
                    let bytes = serde_json::to_vec(&creation).map_err(|error| NarouError::Platform(error.to_string()))?;
                    ObjectStore::write_small(self.checkpoint.as_ref(), &key, bytes).await?;
                    self.creation = Some(creation);
                    Ok((reservation, true))
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, objects, novels, http, keys) = creation_download_fixture();
        http.add_text("https://example.com/n1234ab/", 404, "missing");
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = EarlyOwner { checkpoint: checkpoint.clone(), creation: None };
        downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: None, creation_reservation: None,
        }).await.unwrap_err();
        let bytes = ObjectStore::read_small(checkpoint.as_ref(), &ObjectKey::try_new(".narou-transfer/test-create-early-owned").unwrap()).await.unwrap().unwrap();
        let (id, generation): (crate::platform::NovelId, i64) = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(budget.creation, Some((id, generation)));
        assert!(novels.get_current(id).await.unwrap().is_none());
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.toc()).await.unwrap().is_none());
        assert!(ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap().is_none());
        let next = novels.allocate_id().await.unwrap();
        assert_ne!(next.id(), id, "the durable owner must retain even a pre-TOC reservation");
        novels.release_id(next).await.unwrap();
        novels.release_id(crate::platform::NovelIdReservation::new(id, generation)).await.unwrap();
    }

    #[tokio::test]
    async fn existing_download_cannot_upgrade_a_bound_writer_to_another_execution() {
        struct OwnerBudget(Arc<crate::platform::JobExecutionOwner>);
        impl SectionBudget for OwnerBudget {
            fn job_owner(&self) -> Option<Arc<crate::platform::JobExecutionOwner>> { Some(self.0.clone()) }
            fn should_yield(&mut self, _: usize) -> bool { false }
        }
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, objects, novels, http, keys) = creation_download_fixture();
        let first = downloader.download_novel("https://example.com/n1234ab/").await.unwrap();
        let body = ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap().unwrap();
        let requests = http.requested_urls();
        let mut write = test_origin(&downloader, &keys).await;
        let origin = write.origin();
        write.bind_job_owner(Arc::new(crate::platform::JobExecutionOwner::new("job", "old-execution"))).unwrap();
        let owner = Arc::new(crate::platform::JobExecutionOwner::new("job", "new-execution"));
        let mut budget = CheckpointBudget::new(OwnerBudget(owner.clone()), Arc::new(MemoryObjectStore::new()));
        assert!(Arc::ptr_eq(&budget.job_owner().unwrap(), &owner));
        downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: Some(write), creation_reservation: None,
        }).await.unwrap_err();
        assert_eq!(http.requested_urls(), requests, "owner replacement is rejected before another source request");
        assert_eq!(ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap(), Some(body));
        assert_eq!(novels.get_current(first.id.into()).await.unwrap().unwrap().id, first.id);
        assert_eq!(test_origin(&downloader, &keys).await.origin(), origin, "rejecting a stale owner never upgrades the epoch");
        assert_eq!(budget.allocations, 0);
        assert_eq!(budget.restorations, 0);
    }

    #[cfg(feature = "native-runtime")]
    #[tokio::test]
    async fn existing_title_policy_propagates_invalid_original_scoped_ini() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let database = Database::new().unwrap();
        let previous_db = db::DATABASE.lock().replace(database);
        let _db_guard = DatabaseGuard(previous_db);
        let (mut downloader, objects, novels, _http, keys) = creation_download_fixture();
        let first = downloader.download_novel("https://example.com/n1234ab/").await.unwrap();
        let body = ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap().unwrap();
        let write = test_origin(&downloader, &keys).await;
        write.write_small(&keys.setting(), vec![0xff]).await.unwrap();
        let error = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: None, access: Some(write), creation_reservation: None,
        }).await.unwrap_err();
        assert!(error.to_string().contains("setting.ini is not UTF-8"), "canonical settings failures cannot become defaults: {error}");
        assert_eq!(ObjectStore::read_small(objects.as_ref(), &keys.setting()).await.unwrap(), Some(vec![0xff]));
        assert_eq!(ObjectStore::read_small(objects.as_ref(), &keys.section("1", "第1話")).await.unwrap(), Some(body));
        assert_eq!(novels.get_current(first.id.into()).await.unwrap().unwrap().id, first.id);
    }

    struct DatabaseGuard(Option<Database>);

    impl Drop for DatabaseGuard {
        fn drop(&mut self) {
            *db::DATABASE.lock() = self.0.take();
        }
    }

    #[test]
    fn syosetu_org_illustration_pattern_uses_new_raw_html_without_crossing_links() {
        let setting: SiteSetting =
            serde_yaml::from_str(include_str!("../../webnovel/syosetu.org.yaml")).unwrap();
        let re = compile_html_pattern(setting.illust_grep_pattern.as_deref().unwrap()).unwrap();
        let section = SectionElement {
            data_type: "html".to_string(),
            body: "本文".to_string(),
            introduction: String::new(),
            postscript: String::new(),
        };
        let raw_html = r#"<a href="https://syosetu.org/?mode=url_jump&amp;url=https%3A%2F%2Fexample.com">通販サイト</a>
<span>後書き</span>
<a href="http://syosetu.org/img/user/10193/110088.jpg" alt="挿絵" name='img'>【挿絵表示】</a>"#;

        assert!(!illustration_sources(&section, None).any(|source| re.is_match(source)));
        let mut urls = Vec::new();
        for source in illustration_sources(&section, Some(raw_html)) {
            for caps in re.captures_iter(source) {
                if let Some(url_match) = caps.get(1) {
                    urls.push(url_match.as_str());
                }
            }
        }
        assert_eq!(urls, vec!["http://syosetu.org/img/user/10193/110088.jpg"]);
    }

    /// なろうのサイト定義は `illust_grep_pattern: null` で、narou.rb は
    /// `HTML#initialize` の既定 `<img>` パターンを使う。なろうの挿絵 URL は
    /// protocol-relative (`//host/...`) なので、本文 URL 基準で解決してから
    /// mitemin の取得用 URL (`viewimagebig` → `viewimage`) へ寄せる。
    /// 本文は実際の n9364bq (12話「※【囁くラタトスク】挿絵」) の形。
    #[test]
    fn narou_definition_uses_the_default_img_pattern_for_mitemin_illustrations() {
        let setting: SiteSetting =
            serde_yaml::from_str(include_str!("../../webnovel/ncode.syosetu.com.yaml")).unwrap();
        assert!(setting.illust_grep_pattern.is_none());

        let re = compile_html_pattern(super::DEFAULT_ILLUST_GREP_PATTERN).unwrap();
        let body = r#"<p>前</p><p><a href="//9496.mitemin.net/i88657/" target="_blank"><img src="//9496.mitemin.net/userpageimage/viewimagebig/icode/i88657/" alt="挿絵(By みてみん)" border="0" /></a></p>"#;
        let captured = re.captures(body).expect("default img pattern matches");
        let source = captured.get(1).unwrap().as_str();
        assert_eq!(
            source,
            "//9496.mitemin.net/userpageimage/viewimagebig/icode/i88657/"
        );

        let resolved = crate::illustration_store::normalize_illustration_url(
            &crate::downloader::util::build_section_url(
                &setting,
                "https://ncode.syosetu.com/n9364bq/12/",
                source,
            ),
        );
        assert_eq!(
            resolved,
            "https://9496.mitemin.net/userpageimage/viewimage/icode/i88657/"
        );
        assert_eq!(
            crate::illustration_store::mitemin_illustration_id(&resolved).as_deref(),
            Some("i88657")
        );
    }

    /// 実体確認の並行度と結果を観測する資産ストア。`stat` は 1 回だけ
    /// Pending を返すので、窓の分だけ並行に走る。
    struct YieldingAssets {
        inner: MemoryObjectStore,
        stats: std::sync::atomic::AtomicUsize,
        inflight: std::sync::atomic::AtomicUsize,
        max_inflight: std::sync::atomic::AtomicUsize,
        fail_on: Option<String>,
    }

    impl AssetStore for YieldingAssets {
        fn stat<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            let failed = self.fail_on.as_deref() == Some(key.as_ref());
            Box::pin(async move {
                let inflight = self
                    .inflight
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                self.max_inflight
                    .fetch_max(inflight, std::sync::atomic::Ordering::SeqCst);
                self.stats.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut first_poll = true;
                futures::future::poll_fn(|cx| {
                    if first_poll {
                        first_poll = false;
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    } else {
                        std::task::Poll::Ready(())
                    }
                })
                .await;
                let result = if failed {
                    Err(crate::error::NarouError::Platform("stat failed".into()))
                } else {
                    ObjectStore::stat(&self.inner, key).await
                };
                self.inflight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                result
            })
        }

        fn read_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
            AssetStore::read_stream(&self.inner, key)
        }

        fn read_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
            AssetStore::read_range(&self.inner, key, range)
        }

        fn write_stream<'a>(
            &'a self,
            key: &'a ObjectKey,
            stream: AssetStream,
        ) -> PlatformFuture<'a, Result<()>> {
            AssetStore::write_stream(&self.inner, key, stream)
        }

        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            AssetStore::delete(&self.inner, key)
        }

        fn copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            AssetStore::copy(&self.inner, source, destination)
        }

        fn move_or_copy<'a>(
            &'a self,
            source: &'a ObjectKey,
            destination: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<()>> {
            AssetStore::move_or_copy(&self.inner, source, destination)
        }
    }

    /// 挿絵の実体確認は (a) 保存側と同じキーで保存物だけを返し、(b) 同時に
    /// 走らせる `stat` を窓の件数に抑え、(c) `stat` の失敗では `None` に
    /// 倒す (呼び出し側は索引だけで判断する従来動作へ戻る)。
    #[test]
    fn illustration_file_verification_is_bounded_and_reports_stored_files() {
        // `stat` 失敗経路はデバッグ行を流すので、プロセス共通状態を触る
        // テストと直列化する。
        let _global = crate::test_support::global_state_guard();
        let keys = NovelObjectKeys::new("example.test", "n1234ab", true).unwrap();
        let filenames: Vec<String> = (0..20).map(|index| format!("i{index}.png")).collect();
        let assets = Arc::new(YieldingAssets {
            inner: MemoryObjectStore::new(),
            stats: std::sync::atomic::AtomicUsize::new(0),
            inflight: std::sync::atomic::AtomicUsize::new(0),
            max_inflight: std::sync::atomic::AtomicUsize::new(0),
            fail_on: None,
        });

        // 先頭 5 枚だけ保存する。
        futures::executor::block_on(async {
            for filename in filenames.iter().take(5) {
                ObjectStore::write_small(
                    &assets.inner,
                    &keys.illustration(filename).unwrap(),
                    b"png".to_vec(),
                )
                .await
                .unwrap();
            }
        });

        let stored = futures::executor::block_on(super::verify_illustration_files(
            assets.as_ref(),
            &keys,
            &filenames,
        ))
        .unwrap();
        assert_eq!(stored.len(), 5, "見つかった実体だけを返す: {stored:?}");
        for filename in filenames.iter().take(5) {
            assert!(stored.contains(filename), "{filename} が見つからない");
        }
        assert_eq!(
            assets.stats.load(std::sync::atomic::Ordering::SeqCst),
            filenames.len(),
            "索引の全ファイルを確認する"
        );
        assert_eq!(
            assets
                .max_inflight
                .load(std::sync::atomic::Ordering::SeqCst),
            super::ILLUSTRATION_VERIFY_WINDOW,
            "同時に走る stat は窓の件数まで"
        );

        // `stat` の失敗は `None` (索引だけで判断する従来動作へ戻る)。
        let failing = Arc::new(YieldingAssets {
            inner: MemoryObjectStore::new(),
            stats: std::sync::atomic::AtomicUsize::new(0),
            inflight: std::sync::atomic::AtomicUsize::new(0),
            max_inflight: std::sync::atomic::AtomicUsize::new(0),
            fail_on: Some(
                keys.illustration(&filenames[2])
                    .unwrap()
                    .as_ref()
                    .to_string(),
            ),
        });
        assert!(
            futures::executor::block_on(super::verify_illustration_files(
                failing.as_ref(),
                &keys,
                &filenames,
            ))
            .is_none()
        );
    }

    /// サイト定義に挿絵パターンが無いサイト (なろう等) でも、ダウンロード時に
    /// narou.rb 既定の `<img>` パターンで挿絵を取得し、挿絵側のストア
    /// (Worker では S3) へ保存すること。`SplitStore` で Worker と同じ配線にする。
    #[tokio::test]
    async fn default_img_pattern_stores_illustrations_in_the_illustration_store() {
        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Illustration Default Test
domain: illust-default.test
top_url: https://illust-default.test
url: https?://illust-default\.test/(?<ncode>n\d+[a-z]+)/?
sitename: Illustration Default Test
toc_url: 'https://illust-default.test/\k<ncode>/'
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();
        assert!(setting.illust_grep_pattern.is_none());


        let target = "https://illust-default.test/n1234ab/";
        let http = Arc::new(MockHttpClient::new());
        http.add_text(
            target,
            200,
            r#"<h1>挿絵テスト</h1><span>作者</span><a href="/n1234ab/1/">第1話</a><a href="/n1234ab/2/">第2話</a>"#,
        );
        http.add_text(
            "https://illust-default.test/n1234ab/1/",
            200,
            r#"<div class="body"><a href="//9496.mitemin.net/i88657/"><img src="//9496.mitemin.net/userpageimage/viewimagebig/icode/i88657/" alt="挿絵(By みてみん)" border="0" /></a>本文</div>"#,
        );
        http.add_text(
            "https://illust-default.test/n1234ab/2/",
            200,
            r#"<div class="body">挿絵の無い話</div>"#,
        );
        http.add_response(
            "https://9496.mitemin.net/userpageimage/viewimage/icode/i88657/",
            crate::platform::HttpResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "image/jpeg".to_string())],
                body: b"jpeg-bytes".to_vec(),
            },
        );

        let primary = Arc::new(MemoryObjectStore::new());
        let illustrations = Arc::new(MemoryObjectStore::new_with_shared_authority(primary.as_ref()));
        let split = Arc::new(crate::platform::SplitStore::new(
            (primary.clone(), primary.clone()),
            (illustrations.clone(), illustrations.clone()),
        ));
        let objects: Arc<dyn ObjectStore> = split.clone();
        let assets: Arc<dyn AssetStore> = split;

        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(primary.clone())),
                objects,
                assets,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
            Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
        )
        .unwrap();

        downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: None,
                    resume_from_section: None, access: None,
                    creation_reservation: None,
                },
            )
            .await
            .unwrap();


        async fn keys_of(store: Arc<MemoryObjectStore>) -> Vec<String> {
            store
                .list_page(&ObjectListRequest::new(
                    ObjectPrefix::new("").unwrap(),
                    std::num::NonZeroUsize::new(100).unwrap(),
                ))
                .await
                .unwrap()
                .objects
                .iter()
                .map(|metadata| metadata.key.as_ref().to_string())
                .collect()
        }

        let illustration_keys = keys_of(illustrations.clone()).await;
        assert!(
            illustration_keys
                .iter()
                .any(|key| key.ends_with("/挿絵/i88657.jpg")),
            "the illustration must be stored on the illustration side: {illustration_keys:?}"
        );
        let primary_keys = keys_of(primary).await;
        assert!(
            primary_keys.iter().all(|key| !key.contains("挿絵/")),
            "illustrations must not be stored on the primary side: {primary_keys:?}"
        );
        assert!(
            http.requested_urls()
                .iter()
                .any(|url| url.contains("viewimage/icode/i88657")),
            "the fetch must use the normalized mitemin URL: {:?}",
            http.requested_urls()
        );

        // 索引には残っているが実体が無い状態 (ストアの切替・手動削除) を作り、
        // 次の実行が取り直すこと (索引だけを信じると永久に取り逃す)。
        let picture = crate::platform::ObjectKey::try_new(
            illustration_keys
                .iter()
                .find(|key| key.ends_with("/挿絵/i88657.jpg"))
                .expect("stored illustration key")
                .clone(),
        )
        .unwrap();
        ObjectStore::delete(illustrations.as_ref(), &picture)
            .await
            .unwrap();
        assert!(keys_of(illustrations.clone()).await.is_empty());

        downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: None,
                    resume_from_section: None, access: None,
                    creation_reservation: None,
                },
            )
            .await
            .unwrap();

        assert!(
            keys_of(illustrations)
                .await
                .iter()
                .any(|key| key.ends_with("/挿絵/i88657.jpg")),
            "取り直した挿絵が挿絵側のストアへ戻る"
        );
    }

    #[test]
    fn sanitize_filename_removes_windows_trailing_dots_and_spaces() {
        assert_eq!(super::util::sanitize_filename("title. "), "title");
        assert_eq!(super::util::sanitize_filename("bad/name?"), "bad_name_");
    }

    #[test]
    fn resolve_user_agent_prefers_cli_value_over_saved_value() {
        assert_eq!(
            resolve_user_agent(Some("cli-agent"), Some("saved-agent".to_string())),
            "cli-agent"
        );
    }

    #[test]
    fn ageauth_redirect_target_extracts_original_novel18_url() {
        assert_eq!(
            Downloader::ageauth_redirect_target(
                "https://nl.syosetu.com/redirect/ageauth/?url=https%3A%2F%2Fnovel18.syosetu.com%2Fn7274mc%2F&hash=abc"
            )
            .as_deref(),
            Some("https://novel18.syosetu.com/n7274mc/")
        );
    }

    #[test]
    fn repository_injection_resolves_existing_ncode_without_database_global() {
        // Keep DATABASE absent: every data source is explicitly injected.
        let _state = crate::test_support::global_state_guard();
        let previous_db = db::DATABASE.lock().take();
        let _db_guard = DatabaseGuard(previous_db);
        let mut record = sample_record(chrono::Utc::now());
        record.id = 42;
        record.toc_url = "https://ncode.syosetu.com/n1234ab/".to_string();
        record.ncode = Some("n1234ab".to_string());
        record.is_narou = true;

        let objects = Arc::new(MemoryObjectStore::new());
        let repository = Arc::new(MemoryNovelRepository::from_records(vec![record]).with_objects(objects.clone()));
        let mut setting: SiteSetting = serde_yaml::from_str(include_str!("../../webnovel/ncode.syosetu.com.yaml")).unwrap();
        setting.compile();
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: repository,
                objects: objects.clone(),
                assets: objects,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
        )
        .unwrap();

        let (id, setting) =
            futures::executor::block_on(downloader.resolve_target_for_download("N1234AB")).unwrap();
        assert_eq!(id, Some(42));
        assert_eq!(setting.domain, "ncode.syosetu.com");
        assert!(db::DATABASE.lock().is_none());
    }

    #[tokio::test]
    async fn admission_site_key_groups_yaml_aliases_ids_and_expansion_targets_without_fetching() {
        let mut sites: Vec<SiteSetting> = [
            include_str!("../../webnovel/syosetu.org.yaml"),
            include_str!("../../webnovel/ncode.syosetu.com.yaml"),
        ].into_iter().map(|yaml| serde_yaml::from_str(yaml).unwrap()).collect();
        for setting in &mut sites { setting.compile(); }
        let mut hameln = sample_record(chrono::DateTime::from_timestamp(0, 0).unwrap());
        hameln.id = 42;
        hameln.toc_url = "https://h.syosetu.org/novel/123456/".into();
        let mut narou = sample_record(chrono::DateTime::from_timestamp(0, 0).unwrap());
        narou.id = 43;
        narou.toc_url = "https://ncode.syosetu.com/n1234ab/".into();
        narou.ncode = Some("n1234ab".into());
        let http = Arc::new(MockHttpClient::new());
        let store = Arc::new(MemoryObjectStore::new());
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::from_records(vec![hameln, narou]).with_objects(store.clone())),
                objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
            },
            sites, HashMap::new(),
        ).unwrap();
        for target in [
            "https://syosetu.org/novel/123456/", "https://h.syosetu.org/novel/123456/",
            "https://novel.syosetu.org/123456/", "https://syosetu.org/user/267675/", "42",
        ] {
            assert_eq!(downloader.site_key_for_target(target, None).await.unwrap(), "syosetu.org", "{target}");
        }
        for target in [
            "N1234AB", "43", "https://ncode.syosetu.com/n1234ab/",
            "https://mypage.syosetu.com/2842627/", "https://ncode.syosetu.com/s1234ab/",
        ] {
            assert_eq!(downloader.site_key_for_target(target, None).await.unwrap(), "syosetu.com", "{target}");
        }
        assert!(downloader.site_key_for_target("https://unknown.example/book/", None).await.is_err());
        assert!(http.requested_urls().is_empty(), "admission must never fetch a site or send cookies");
    }

    #[tokio::test]
    async fn admission_reuses_metadata_without_changing_ageauth_target_precedence() {
        let sites = SiteSetting::load_bundled(&[include_str!("../../webnovel/ncode.syosetu.com.yaml")]).unwrap();
        let mut record = sample_record(chrono::DateTime::from_timestamp(0, 0).unwrap());
        record.id = 42;
        record.ncode = Some("n1234ab".into());
        record.toc_url = "https://nl.syosetu.com/redirect/ageauth/?url=https%3A%2F%2Fncode.syosetu.com%2Fn1234ab%2F".into();
        let store = Arc::new(MemoryObjectStore::new());
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()), rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::from_records(vec![record.clone()]).with_objects(store.clone())),
                objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
            },
            sites, HashMap::new(),
        ).unwrap();
        for known in [None, Some(&record)] {
            assert_eq!(downloader.site_key_for_target("42", known).await.unwrap(), "syosetu.com");
            assert!(matches!(downloader.site_key_for_target("N1234AB", known).await, Err(crate::error::NarouError::SiteSetting(_))));
        }
    }

    /// 暁の R-18 ゲート: TOC がサイト定義に一致しない `/novels/adult_check`
    /// へ 302 しても、採用する toc_url は元の作品 URL のままにする。
    /// 採用してしまうと認証リトライがゲート URL へ送られて必ず失敗する。
    #[tokio::test]
    async fn redirect_to_ungated_page_does_not_replace_toc_url() {
        let sites = SiteSetting::load_bundled(&[include_str!("../../webnovel/www.akatsuki-novels.com.yaml")]).unwrap();
        let toc_url = "https://www.akatsuki-novels.com/stories/index/novel_id~10044";
        let gate_url = "https://www.akatsuki-novels.com/novels/adult_check?back=https%3A%2F%2Fwww.akatsuki-novels.com%2Fstories%2Findex%2Fnovel_id%7E10044";
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            toc_url,
            HttpResponse {
                status: 302,
                headers: vec![
                    ("Location".into(), "/novels/adult_check?back=https%3A%2F%2Fwww.akatsuki-novels.com%2Fstories%2Findex%2Fnovel_id%7E10044".into()),
                    ("Content-Type".into(), "text/html; charset=utf-8".into()),
                ],
                body: Vec::new(),
            },
        );
        // ゲートの本文はサイト定義の login_pattern (「あなたは18歳以上ですか」) に一致する。
        http.add_text(gate_url, 200, "<html>あなたは18歳以上ですか</html>");
        // 年齢承認を済ませたログイン (akatsuki_adult_allowed) が保存されている。
        let cookies = Arc::new(MemoryCookieStore::new());
        cookies.save_groups(
            "www.akatsuki-novels.com",
            &[LoginGroup::new(
                "www.akatsuki-novels.com",
                vec![HostCookie {
                    host: "www.akatsuki-novels.com".into(),
                    cookie: "akatsuki_adult_allowed=on".into(),
                }],
            ).with_id("adult")],
        ).await.unwrap();

        let store = Arc::new(MemoryObjectStore::new());
        let novels = Arc::new(MemoryNovelRepository::new().with_objects(store.clone()));
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels,
                objects: store.clone(),
                assets: store,
                clock: Arc::new(SystemClock),
            },
            sites,
            HashMap::new(),
        )
        .unwrap()
        .with_cookie_store(cookies);

        // モックは保存 Cookie でもゲートしか返せないので、壁として失敗するのは
        // 従来どおり。ポイントは認証リトライがどの URL へ届くか。
        let result = downloader.download_novel(toc_url).await;
        assert!(result.is_err());
        // 旧実装はゲート URL を toc_url に採用し、以後の要求がすべて
        // `/novels/adult_check` になった。修正後は作品 URL へ認証リトライが届く。
        let sent = http.sent_requests();
        let urls: Vec<&str> = sent.iter().map(|(url, _)| url.as_str()).collect();
        assert_eq!(
            urls,
            vec![toc_url, gate_url, toc_url, gate_url, toc_url, gate_url],
            "cookie リトライは作品の toc_url へ届く必要がある"
        );
        // 保存した年齢承認 Cookie が作品 URL のリクエストに乗っていること。
        // 3 番目の要求 (index 2) は壁検出前の匿名フェッチ、5 番目 (index 4)
        // が保存済みログインを送るリトライ。
        let (_, retry_headers) = &sent[4];
        assert!(
            retry_headers.iter().any(|(name, value)|
                name.eq_ignore_ascii_case("cookie")
                    && value.contains("akatsuki_adult_allowed=on")),
            "retry to the work URL must carry the stored adult cookie: {retry_headers:?}"
        );
    }

    #[test]
    fn explicit_storage_constructor_avoids_native_loaders() {
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store.clone();
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store)),
                objects,
                assets,
                clock: Arc::new(SystemClock),
            },
            Vec::new(),
            HashMap::new(),
        )
        .unwrap();
        assert!(downloader.site_settings.is_empty());
        assert!(downloader.section_hash_cache.is_empty());
    }

    #[tokio::test]
    async fn memory_storage_download_pipeline_persists_toc_section_raw_and_record() {
        let mut setting: SiteSetting =
            serde_yaml::from_str(include_str!("../../webnovel/ncode.syosetu.com.yaml")).unwrap();
        setting.subtitles = Some(SiteSettingValue::Single(
            r#"<a href="(?<href>[^"]+)" class="p-eplist__subtitle">(?<subtitle>[^<]+)</a>"#
                .to_string(),
        ));
        setting.body_pattern =
            Some(r#"<div class="js-novel-text p-novel__text">(?<body>.+?)</div>"#.to_string());
        setting.compile();
        let toc_url = "https://ncode.syosetu.com/n1234ab/";
        let section_url = "https://ncode.syosetu.com/n1234ab/1/";
        let http = Arc::new(MockHttpClient::new());
        http.add_text(
            toc_url,
            200,
            r#"<div class="p-eplist__sublist">
<a href="/n1234ab/1/" class="p-eplist__subtitle">第1話</a>
<div class="p-eplist__update">2026年01月01日</div>
</div>"#,
        );
        http.add_text(
            section_url,
            200,
            r#"<div class="js-novel-text p-novel__text">本文テスト</div>"#,
        );

        let toc_source = super::toc::fetch_toc(
            http.as_ref(),
            &FakeRateLimiter::new(),
            &setting,
            toc_url,
            &mut super::preprocess::PreprocessJobs::new(),
        )
        .await
        .unwrap();
        let captures = HashMap::from([("ncode".to_string(), "n1234ab".to_string())]);
        let subtitles = super::toc::parse_subtitles(&setting, &toc_source, &captures).unwrap();
        assert_eq!(subtitles.len(), 1);

        let mut section_cache = super::section::SectionCache::new();
        let (element, raw_html) = super::section::download_section(
            http.as_ref(),
            &FakeRateLimiter::new(),
            &mut section_cache,
            &setting,
            &subtitles[0],
            toc_url,
            &mut super::preprocess::PreprocessJobs::new(),
        )
        .await
        .unwrap();
        assert_eq!(element.body, "本文テスト");

        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let persistence =
            PersistenceService::with_clock(objects.clone(), Arc::new(SystemClock));
        let keys = NovelObjectKeys::new("小説家になろう", "テスト作品", false).unwrap();
        let toc = TocFile {
            title: "テスト作品".to_string(),
            author: "作者".to_string(),
            toc_url: toc_url.to_string(),
            story: Some("あらすじ".to_string()),
            subtitles: subtitles.clone(),
            novel_type: Some(1),
        };
        persistence.save_toc(&test_write(&persistence, &keys).await, &keys, &toc).await.unwrap();
        persistence
            .save_section(&test_write(&persistence, &keys).await, &keys, &subtitles[0], &element, 2)
            .await
            .unwrap();
        persistence
            .save_raw(&test_write(&persistence, &keys).await, &keys, &subtitles[0], &raw_html)
            .await
            .unwrap();
        assert_eq!(
            persistence.load_toc(&mut test_read(&persistence, &keys).await, &keys).await.unwrap().unwrap().title,
            "テスト作品"
        );
        assert_eq!(
            persistence
                .load_section(&mut test_read(&persistence, &keys).await, &keys, &subtitles[0])
                .await
                .unwrap()
                .unwrap()
                .element
                .body,
            "本文テスト"
        );
        assert!(
            objects
                .read_small(&keys.raw_section(&subtitles[0].index, &subtitles[0].file_subtitle))
                .await
                .unwrap()
                .is_some()
        );

        let mut record = sample_record(chrono::Utc::now());
        record.id = 7;
        record.toc_url = toc_url.to_string();
        record.title = "テスト作品".to_string();
        let repository = MemoryNovelRepository::new();
        repository
            .apply_batch(vec![NovelMutation::Upsert(record.clone())])
            .await
            .unwrap();
        assert_eq!(
            repository.get(7.into()).await.unwrap().unwrap().title,
            "テスト作品"
        );
        assert_eq!(http.request_count(), 2);
    }

    /// 設定ポートが生データを拒否したら `raw/*.html` を保存しないこと。
    /// (Worker は常に拒否し、native は `economy: nosave_raw` で拒否する)
    #[tokio::test]
    async fn raw_html_is_skipped_when_the_settings_disable_it() {
        struct NoRawSettings;

        impl DownloaderSettings for NoRawSettings {
            fn save_raw_html(&self) -> bool {
                false
            }
        }

        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: No Raw Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: No Raw Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();

        let target = "https://example.com/n1234ab/";
        let http = Arc::new(MockHttpClient::new());
        http.add_text(
            target,
            200,
            r#"
<h1>No Raw Test</h1><span>Author</span>
<a href="/n1234ab/1/">第1話</a>
"#,
        );
        http.add_text(
            "https://example.com/n1234ab/1/",
            200,
            r#"<div class="body">本文1</div>"#,
        );

        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store.clone();
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects: objects.clone(),
                assets,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
            Arc::new(NoRawSettings),
        )
        .unwrap();

        downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: None,
                    resume_from_section: None, access: None,
                    creation_reservation: None,
                },
            )
            .await
            .unwrap();

        let page = objects
            .list_page(&ObjectListRequest::new(
                ObjectPrefix::new("").unwrap(),
                std::num::NonZeroUsize::new(500).unwrap(),
            ))
            .await
            .unwrap();
        let keys: Vec<String> = page
            .objects
            .iter()
            .map(|metadata| metadata.key.as_ref().to_string())
            .collect();
        assert!(
            keys.iter().all(|key| !key.contains("/raw/")),
            "raw HTML must not be stored: {keys:?}"
        );
        assert!(
            keys.iter().any(|key| key.contains("/本文/")),
            "sections must still be stored: {keys:?}"
        );
    }

    #[cfg(feature = "native-runtime")]
    #[tokio::test]
    async fn diff_versions_require_successful_moves_or_completed_existing_updates() {
        use crate::application::webui::diff::{plan_diff_cache_clean, read_diff_cache_catalog};
        use crate::downloader::settings::SnapshotDownloaderSettings;

        struct FixedDiffClock(chrono::DateTime<chrono::Utc>);
        impl crate::platform::Clock for FixedDiffClock {
            fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
                self.0
            }
        }
        struct RejectTocWrites(Arc<MemoryObjectStore>);
        impl ObjectStore for RejectTocWrites {
            fn access_fence(&self) -> Option<&dyn crate::platform::NovelAccessFence> { self.0.access_fence() }
            fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a crate::platform::GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> { self.0.read_stream_leased(prefix, lease, key, range) }
            fn write_small_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
                if key.as_ref().ends_with("/toc.yaml") { Box::pin(async { Err(crate::error::NarouError::Platform("TOC write failed".into())) }) }
                else { self.0.write_small_guarded(fence, key, data) }
            }
            fn delete_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> { self.0.delete_guarded(fence, key) }
            fn update_small_guarded<'a, 'data: 'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, mutate: &'a mut crate::platform::SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<crate::platform::ObjectUpdateResult>> { self.0.update_small_guarded(fence, key, mutate) }

            fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
                ObjectStore::stat(self.0.as_ref(), key)
            }
            fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
                self.0.read_small(key)
            }
            fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
                if key.as_ref().ends_with("/toc.yaml") {
                    Box::pin(async { Err(crate::error::NarouError::Platform("TOC write failed".into())) })
                } else {
                    self.0.write_small(key, data)
                }
            }
            fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
                ObjectStore::delete(self.0.as_ref(), key)
            }
            fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<crate::platform::ObjectListPage>> {
                self.0.list_page(request)
            }
        }
        struct YieldBefore(usize);
        impl SectionBudget for YieldBefore {
            fn should_yield(&mut self, next_section_index: usize) -> bool {
                next_section_index >= self.0
            }
        }

        const TARGET: &str = "https://example.com/n1234ab/";
        fn http_with_sections(count: usize, failed_section: Option<usize>) -> Arc<MockHttpClient> {
            let http = Arc::new(MockHttpClient::new());
            let mut toc = "<h1>Diff Test</h1><span>Author</span>".to_string();
            for index in 1..=count {
                toc.push_str(&format!(
                    "<a href=\"/n1234ab/{index}/\" data-date=\"2000-01-01\">第{index}話</a>"
                ));
                http.add_text(
                    format!("{TARGET}{index}/"),
                    if failed_section == Some(index) { 500 } else { 200 },
                    format!("<div class=\"body\">本文{index}</div>"),
                );
            }
            http.add_text(TARGET, 200, toc);
            http
        }
        fn clock_time(tick: u32) -> chrono::DateTime<chrono::Utc> {
            chrono::Utc.with_ymd_and_hms(2026, 10, 13, 1, 2, tick).single().unwrap()
        }
        fn version(tick: u32) -> String {
            clock_time(tick).with_timezone(&chrono::Local).format("%Y.%m.%d@%H.%M.%S").to_string()
        }

        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Diff Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Diff Test
toc_url: 'https://example.com/\k<ncode>/'
timezone: UTC
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)" data-date="(?<subdate>[^<]+)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        ).unwrap();
        setting.compile();
        let store = Arc::new(MemoryObjectStore::new());
        let novels = Arc::new(MemoryNovelRepository::new().with_objects(store.clone()));
        let persistence = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("Diff Test", "n1234ab", false).unwrap();
        let settings: Arc<dyn DownloaderSettings> = Arc::new(SnapshotDownloaderSettings::default());
        let downloader = |count, failed_section, tick, objects: Arc<dyn ObjectStore>,
                          assets: Arc<dyn AssetStore>, settings: Arc<dyn DownloaderSettings>| {
            Downloader::with_platform_and_storage_and_settings_and_support(
                DownloaderPlatform {
                    http: http_with_sections(count, failed_section),
                    rate_limiter: Arc::new(FakeRateLimiter::new()),
                    novels: novels.clone(),
                    objects,
                    assets,
                    clock: Arc::new(FixedDiffClock(clock_time(tick))),
                },
                vec![setting.clone()],
                HashMap::new(),
                settings,
            ).unwrap()
        };

        let initial = downloader(1, None, 0, store.clone(), store.clone(), settings.clone())
            .download_novel(TARGET).await.unwrap();
        assert!(initial.new_novel);
        assert!(read_diff_cache_catalog(&mut NovelReadScope::new(persistence.objects(), "diff-test"), store.as_ref(), keys.prefix(), &[]).await.unwrap().is_empty());
        let unchanged = downloader(1, None, 1, store.clone(), store.clone(), settings.clone())
            .download_novel(TARGET).await.unwrap();
        assert_eq!(unchanged.updated_count, 0);
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());
        assert!(downloader(2, Some(2), 2, store.clone(), store.clone(), settings.clone())
            .download_novel(TARGET).await.is_err());
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());

        let test_objects: Arc<dyn ObjectStore> = store.clone();
        let test_assets: Arc<dyn AssetStore> = store.clone();
        let origin = NovelWriteGuard::capture_for(&test_objects, Some(&test_assets), &keys).await.unwrap();
        let mut budget = YieldBefore(2);
        let partial = downloader(3, None, 3, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: Some(&mut budget), resume_from_section: None, access: Some(origin.clone()),
                creation_reservation: None,
            }).await.unwrap_err();
        assert!(matches!(partial, crate::error::NarouError::DownloadBudgetExpired { next_section_index: 2 }));
        assert!(store.read_small(&keys.section("2", "第2話")).await.unwrap().is_some(),
            "the partial run really saved its new section");
        assert_eq!(persistence.load_toc(&mut test_read(&persistence, &keys).await, &keys).await.unwrap().unwrap().subtitles.len(), 1);
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());
        assert!(downloader(3, Some(3), 4, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: Some(2), access: Some(origin.clone()),
                creation_reservation: None,
            }).await.is_err());
        assert!(downloader(3, None, 5, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: Some(4), access: Some(origin.clone()),
                creation_reservation: None,
            }).await.is_err());
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());

        let completed = downloader(3, None, 6, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: Some(2), access: Some(origin.clone()),
                creation_reservation: None,
            }).await.unwrap();
        assert_eq!(completed.id, initial.id);
        assert!(!completed.new_novel);
        assert_eq!(completed.updated_count, 2);
        assert_eq!(persistence.load_toc(&mut test_read(&persistence, &keys).await, &keys).await.unwrap().unwrap().subtitles.len(), 3);
        let versions = read_diff_cache_catalog(&mut NovelReadScope::new(persistence.objects(), "diff-test"), store.as_ref(), keys.prefix(), &[]).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, version(6));
        assert!(versions[0].only_latest_update);
        assert_eq!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap(),
            vec![keys.cached_version_marker(&version(6))]);

        let unchanged = downloader(3, None, 7, store.clone(), store.clone(), settings.clone())
            .download_novel(TARGET).await.unwrap();
        assert_eq!(unchanged.updated_count, 0);
        assert!(store.read_small(&keys.cached_version_marker(&version(7))).await.unwrap().is_none());
        let economy = serde_yaml::from_str("['nosave_diff']").unwrap();
        let no_diff: Arc<dyn DownloaderSettings> =
            Arc::new(SnapshotDownloaderSettings::default().with_economy(Some(&economy)));
        let appended = downloader(4, None, 8, store.clone(), store.clone(), no_diff)
            .download_novel(TARGET).await.unwrap();
        assert_eq!(appended.updated_count, 1);
        assert!(store.read_small(&keys.cached_version_marker(&version(8))).await.unwrap().is_none());
        assert!(store.read_small(&keys.raw_section("4", "第4話")).await.unwrap().is_none(),
            "the snapshot keeps Worker's intentional no-raw policy");

        let failed_toc = downloader(5, None, 9, Arc::new(RejectTocWrites(store.clone())), store.clone(), settings.clone())
            .download_novel(TARGET).await.unwrap_err();
        assert!(failed_toc.to_string().contains("TOC write failed"));
        assert!(store.read_small(&keys.section("5", "第5話")).await.unwrap().is_some());
        assert_eq!(persistence.load_toc(&mut test_read(&persistence, &keys).await, &keys).await.unwrap().unwrap().subtitles.len(), 4);
        assert!(store.read_small(&keys.cached_version_marker(&version(9))).await.unwrap().is_none(),
            "successful section content alone does not complete an empty version");

        let updated = downloader(4, None, 10, store.clone(), store.clone(), settings.clone())
            .download_novel_with_force(TARGET, true).await.unwrap();
        assert_eq!(updated.updated_count, 4);
        let archived = read_diff_cache_catalog(&mut NovelReadScope::new(persistence.objects(), "diff-test"), store.as_ref(), keys.prefix(), &[]).await.unwrap();
        assert_eq!(archived.len(), 2);
        assert_eq!(archived[0].version, version(10));
        assert!(!archived[0].only_latest_update);
        assert_eq!(archived[0].entries.len(), 4);
        assert!(store.read_small(&keys.cached_version_marker(&version(10))).await.unwrap().is_some());
        assert_eq!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().len(), 6,
            "one marker per version, plus four moved sections");

        let missing_conn = crate::native::sqlite::open_in_memory().unwrap();
        let missing_mirror = crate::native::object_store::NativeObjectStore::from_root(
            temp.path().join("missing-assets"),
        ).unwrap();
        let missing_assets: Arc<dyn AssetStore> = Arc::new(
            crate::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
                missing_conn, missing_mirror, false,
            ),
        );
        assert!(downloader(4, None, 11, store.clone(), missing_assets, settings)
            .download_novel_with_force(TARGET, true).await.is_err());
        assert!(store.read_small(&keys.cached_version_marker(&version(11))).await.unwrap().is_none(),
            "a failed first cache move cannot create a marker");
        assert!(store.read_small(&keys.section("1", "第1話")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn renamed_subtitle_and_chapter_preserve_history_and_allow_later_updates() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Rename Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Rename Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <h2>(?<chapter>[^<]+)</h2><a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a><time>(?<subdate>[^<]+)</time>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();
        let target = "https://example.com/n1234ab/";
        let section_url = "https://example.com/n1234ab/1/";
        let store = Arc::new(MemoryObjectStore::new());
        let novels = Arc::new(MemoryNovelRepository::new().with_objects(store.clone()));
        let http = Arc::new(MockHttpClient::new());
        let persistence = PersistenceService::new(store.clone());
        let mut record_id = None;
        let mut keys = None;
        for (chapter, subtitle, body, expected_updates) in [
            ("第三章", "03_164 屍者の来訪", "旧本文", 1),
            ("第三章", "03_164 冬の屍者", "新本文", 1),
            ("第三章", "03_164 冬の屍者", "新本文", 0),
            ("第三章 改題", "03_164 冬の屍者", "新本文", 1),
        ] {
            http.add_text(target, 200, format!(
                r#"<h1>Rename Test</h1><span>Author</span><h2>{chapter}</h2><a href="/n1234ab/1/">{subtitle}</a><time>2026-09-29</time>"#
            ));
            http.add_text(section_url, 200, format!(r#"<div class="body">{body}</div>"#));
            let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
                DownloaderPlatform {
                    http: http.clone(),
                    rate_limiter: Arc::new(FakeRateLimiter::new()),
                    novels: novels.clone(),
                    objects: store.clone(),
                    assets: store.clone(),
                    clock: Arc::new(SystemClock),
                },
                vec![setting.clone()],
                HashMap::new(),
                Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
            )
            .unwrap();
            let result = downloader.download_novel_with_force(target, false).await.unwrap();
            assert_eq!(result.updated_count, expected_updates);
            if let Some(id) = record_id {
                assert_eq!(result.id, id);
                assert!(!result.new_novel);
                assert!(!result.new_arrivals);
            } else {
                record_id = Some(result.id);
                let record = novels.get(result.id.into()).await.unwrap().unwrap();
                keys = Some(NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).unwrap());
            }
            let toc = persistence.load_toc(&mut test_read(&persistence, keys.as_ref().unwrap()).await, keys.as_ref().unwrap()).await.unwrap().unwrap();
            assert_eq!(toc.subtitles[0].subtitle, subtitle);
            let saved = persistence.load_section(&mut test_read(&persistence, keys.as_ref().unwrap()).await, keys.as_ref().unwrap(), &toc.subtitles[0]).await.unwrap().unwrap();
            assert_eq!(saved.chapter, chapter);
            assert_eq!(saved.element.body, body);
        }
        let keys = keys.unwrap();
        assert!(store.read_small(&keys.section("1", "03_164 屍者の来訪")).await.unwrap().is_none());
        let names = persistence.list_novel_object_names(&keys).await.unwrap();
        let archived = names.iter().find(|name| name.starts_with("本文/cache/") && name.ends_with("/1 03_164 屍者の来訪.yaml")).unwrap();
        let archived_key = crate::platform::ObjectKey::try_new(format!("{}/{archived}", keys.prefix().as_ref())).unwrap();
        let bytes = store.read_small(&archived_key).await.unwrap().unwrap();
        let old: super::types::SectionFile = serde_yaml::from_slice(&bytes).unwrap();
        assert_eq!(old.element.body, "旧本文");
        assert_eq!(old.subtitle, "03_164 屍者の来訪");
        assert_eq!(http.requested_urls().iter().filter(|url| url.ends_with(section_url)).count(), 3);
    }

    /// Downloader のレポート行 (DL 開始・削除済み小説の通知) はコンソールと
    /// ログの両方に出る。narou.rb は `$stdout` への write をすべてログへ
    /// 追記する (`lib/narou_logger.rb`) ため、片方だけに出す sink は使えない。
    /// v0.3.0 の並列化以降これらがログから消えていた (issue #36)。
    #[tokio::test]
    async fn downloader_report_lines_reach_the_log_file() {
        use crate::application::messages;

        let temp = tempfile::tempdir().unwrap();
        let log_text = {
            let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
            std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
            std::fs::write(
                temp.path().join(".narou").join("local_setting.yaml"),
                "logging: true\n",
            )
            .unwrap();
            crate::logger::init();

            let mut setting: SiteSetting = serde_yaml::from_str(
                r#"
name: Log Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Log Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <h2>(?<chapter>[^<]+)</h2><a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a><time>(?<subdate>[^<]+)</time>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
            )
            .unwrap();
            setting.compile();
            let target = "https://example.com/n1234ab/";
            let store = Arc::new(MemoryObjectStore::new());
            let novels = Arc::new(MemoryNovelRepository::new().with_objects(store.clone()));
            let http = Arc::new(MockHttpClient::new());
            http.add_text(
                target,
                200,
                r#"<h1>Log Test</h1><span>Author</span><h2>第一章</h2><a href="/n1234ab/1/">1 話</a><time>2026-10-09</time>"#,
            );
            http.add_text(
                "https://example.com/n1234ab/1/",
                200,
                r#"<div class="body">本文</div>"#,
            );

            let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
                DownloaderPlatform {
                    http: http.clone(),
                    rate_limiter: Arc::new(FakeRateLimiter::new()),
                    novels: novels.clone(),
                    objects: store.clone(),
                    assets: store.clone(),
                    clock: Arc::new(SystemClock),
                },
                vec![setting],
                HashMap::new(),
                Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
            )
            .unwrap();
            downloader.set_message_sink(crate::progress::console_sink());

            let first = downloader.download_novel_with_force(target, false).await.unwrap();

            // 2 回目は 404 (サイトから消えた小説)。エラーになった小説の行も
            // ログに残らないと、どの小説が失敗したのか追えない。
            http.add_text(target, 404, "");
            let _ = downloader.download_novel_with_force(target, false).await;

            let log_path = std::fs::read_dir(temp.path().join("log"))
                .unwrap()
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .next()
                .expect("logging: true must create a log file");
            let text = std::fs::read_to_string(&log_path).unwrap();
            assert!(
                text.contains(&messages::download::download_started(first.id, "Log Test")),
                "DL 開始行がログに無い: {text}"
            );
            text
        };
        // このテストの CWD 依存のログ設定を後続テストへ持ち越さない。
        crate::logger::init();

        assert!(
            log_text.contains(messages::download::novel_unavailable()),
            "削除済み小説の行がログに無い: {log_text}"
        );
    }

    #[tokio::test]
    async fn long_download_resumes_beyond_the_storage_request_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountedStore {
            inner: Arc<MemoryObjectStore>,
            requests: Arc<AtomicUsize>,
        }
        impl ObjectStore for CountedStore {
            fn access_fence(&self) -> Option<&dyn crate::platform::NovelAccessFence> { self.inner.access_fence() }
            fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a crate::platform::GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
                if key.as_ref().contains("/本文/") { self.requests.fetch_add(1, Ordering::SeqCst); }
                self.inner.read_stream_leased(prefix, lease, key, range)
            }
            fn write_small_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
                if key.as_ref().contains("/本文/") { self.requests.fetch_add(1, Ordering::SeqCst); }
                self.inner.write_small_guarded(fence, key, data)
            }
            fn delete_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> { self.inner.delete_guarded(fence, key) }
            fn update_small_guarded<'a, 'data: 'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, mutate: &'a mut crate::platform::SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<crate::platform::ObjectUpdateResult>> { self.inner.update_small_guarded(fence, key, mutate) }

            fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
                ObjectStore::stat(self.inner.as_ref(), key)
            }
            fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
                if key.as_ref().contains("/本文/") {
                    self.requests.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.read_small(key)
            }
            fn write_small<'a>(&'a self, key: &'a ObjectKey, bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
                if key.as_ref().contains("/本文/") {
                    self.requests.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.write_small(key, bytes)
            }
            fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
                ObjectStore::delete(self.inner.as_ref(), key)
            }
            fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<crate::platform::ObjectListPage>> {
                self.inner.list_page(request)
            }
            fn update_small<'a, 'data: 'a>(&'a self, key: &'a ObjectKey, mutate: &'a mut crate::platform::SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<crate::platform::ObjectUpdateResult>> {
                self.inner.update_small(key, mutate)
            }
        }
        struct RequestBudget(Arc<AtomicUsize>);
        impl SectionBudget for RequestBudget {
            fn should_yield(&mut self, _: usize) -> bool {
                self.0.load(Ordering::SeqCst) >= 8
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let previous_db = db::DATABASE.lock().replace(Database::new().unwrap());
        let _db_guard = DatabaseGuard(previous_db);
        let mut setting: SiteSetting = serde_yaml::from_str(r#"
name: Long Resume
domain: example.com
top_url: https://example.com
url: 'https://example\.com/(?<ncode>n1234ab)/?'
sitename: Long Resume
toc_url: 'https://example.com/\k<ncode>/'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
subtitles: '<a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
"#).unwrap();
        setting.compile();
        let target = "https://example.com/n1234ab/";
        let http = Arc::new(MockHttpClient::new());
        let mut toc = "<h1>Long Resume</h1><span>Author</span>".to_string();
        for index in 1..=32 {
            toc.push_str(&format!("<a href=\"/n1234ab/{index}/\">第{index}話</a>"));
            http.add_text(format!("{target}{index}/"), 200, format!("<div class=\"body\">本文{index}</div>"));
        }
        http.add_text(target, 200, toc);
        let inner = Arc::new(MemoryObjectStore::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let objects = Arc::new(CountedStore { inner: inner.clone(), requests: requests.clone() });
        let novels = Arc::new(MemoryNovelRepository::new().with_objects(inner.clone()));
        let keys = NovelObjectKeys::new("Long Resume", "n1234ab", false).unwrap();
        let test_objects: Arc<dyn ObjectStore> = objects.clone();
        let test_assets: Arc<dyn AssetStore> = inner.clone();
        let origin = NovelWriteGuard::capture_for(&test_objects, Some(&test_assets), &keys).await.unwrap();
        let mut next = 0;
        let mut completed = false;
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut creation_reservation = None;
        for invocation in 0..5 {
            requests.store(0, Ordering::SeqCst);
            let mut budget = CheckpointBudget::new(RequestBudget(requests.clone()), checkpoint.clone());
            let mut downloader = Downloader::with_platform_and_storage_and_settings(
                DownloaderPlatform {
                    http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()),
                    novels: novels.clone(), objects: objects.clone(), assets: inner.clone(),
                    clock: Arc::new(SystemClock),
                },
                vec![setting.clone()], HashMap::new(),
            ).unwrap();
            let outcome = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
                force: false, budget: Some(&mut budget),
                resume_from_section: (invocation > 0).then_some(next), access: Some(origin.clone()),
                creation_reservation,
            }).await;
            if creation_reservation.is_some() { assert_eq!(budget.creation_reservation, creation_reservation); }
            creation_reservation = budget.creation_reservation;
            assert_eq!(budget.allocations, usize::from(invocation == 0), "only the first invocation may allocate a create identity");
            assert_eq!(budget.restorations, usize::from(invocation > 0), "each continuation rehydrates the same reservation");
            assert!(creation_reservation.is_some(), "new-work ownership must survive each invocation");
            match outcome {
                Ok(result) => {
                    assert_eq!(result.total_count, 32);
                    completed = true;
                    break;
                }
                Err(crate::error::NarouError::DownloadBudgetExpired { next_section_index }) => {
                    if invocation == 1 {
                        assert_eq!(next_section_index, next, "legacy prefix recovery must retain its cursor");
                    } else {
                        assert!(next_section_index > next, "resume made no progress at section {next}");
                    }
                    next = next_section_index;
                    if invocation == 0 {
                        // A checkpoint created by the previous version has
                        // bodies but no compact resume metadata.
                        ObjectStore::delete(inner.as_ref(), &keys.prefix().join(".download_resume").unwrap()).await.unwrap();
                    }
                }
                Err(error) => panic!("{error}"),
            }
        }
        assert!(completed, "the novel must complete across bounded invocations");
        let persistence = PersistenceService::new(inner.clone());
        let toc = persistence.load_toc(&mut test_read(&persistence, &keys).await, &keys).await.unwrap().unwrap();
        assert_eq!(toc.subtitles.len(), 32);
        for subtitle in &toc.subtitles {
            let section = persistence.load_section(&mut test_read(&persistence, &keys).await, &keys, subtitle).await.unwrap().unwrap();
            assert_eq!(section.download_time, subtitle.download_time);
            assert_eq!(section.element.body, format!("本文{}", subtitle.index));
            let section_url = format!("{target}{}/", subtitle.index);
            assert_eq!(http.requested_urls().iter().filter(|url| url.ends_with(&section_url)).count(), 1);
        }

        // A legacy checkpoint has no compact metadata. Recovery must begin
        // processing its first body before reading the entire stored prefix.
        // Stopping at its first image also drops the bounded prefetch window.
        struct StopAtFirstImage {
            reads: Arc<AtomicUsize>,
            checked: bool,
        }
        impl SectionBudget for StopAtFirstImage {
            fn should_yield(&mut self, section_index: usize) -> bool {
                assert_eq!(section_index, 0);
                assert!(self.reads.load(Ordering::SeqCst) <= 4,
                    "resume must not collect all eight prefix bodies before processing one");
                self.checked = true;
                true
            }
        }
        let first = &toc.subtitles[0];
        let mut section = persistence.load_section(&mut test_read(&persistence, &keys).await, &keys, first).await.unwrap().unwrap();
        section.element.body.push_str(r#"<img src="https://example.com/resume-image.png">"#);
        inner.write_small(
            &keys.section(&first.index, &first.file_subtitle),
            serde_yaml::to_string(&section).unwrap().into_bytes(),
        ).await.unwrap();
        requests.store(0, Ordering::SeqCst);
        let mut stop = StopAtFirstImage { reads: requests.clone(), checked: false };
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: novels.clone(), objects: objects.clone(), assets: inner.clone(),
                clock: Arc::new(SystemClock),
            },
            vec![setting], HashMap::new(),
        ).unwrap();
        let stopped = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: Some(&mut stop), resume_from_section: Some(8), access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap_err();
        assert!(stop.checked, "the first restored body must reach illustration recovery");
        assert!(matches!(stopped, crate::error::NarouError::DownloadBudgetExpired { next_section_index: 0 }));
        assert!(requests.load(Ordering::SeqCst) <= 4, "canceling recovery stops further source reads");
    }

    #[tokio::test]
    async fn resumable_download_reuses_persisted_prefix_without_refetching() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::new().unwrap();
        let previous_db = {
            let mut db_slot = db::DATABASE.lock();
            let previous_db = db_slot.take();
            *db_slot = Some(db);
            previous_db
        };
        let _db_guard = DatabaseGuard(previous_db);

        struct YieldBeforeSection(usize);

        impl SectionBudget for YieldBeforeSection {
            fn should_yield(&mut self, next_section_index: usize) -> bool {
                next_section_index >= self.0
            }
        }

        struct FixedClock;
        impl crate::platform::Clock for FixedClock {
            fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
                chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
            }
        }

        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Resume Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Resume Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)" data-date="(?<subdate>[^"]+)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();

        let target = "https://example.com/n1234ab/";
        let toc_url = target;
        let section_1 = "https://example.com/n1234ab/1/";
        let section_2 = "https://example.com/n1234ab/2/";
        let toc = r#"
<h1>Resume Test</h1><span>Author</span>
<a href="/n1234ab/1/" data-date="2000-01-01">第1話</a>
<a href="/n1234ab/2/" data-date="2000-01-01">第2話</a>
"#;
        let http = Arc::new(MockHttpClient::new());
        http.add_text(toc_url, 200, toc);
        http.add_text(section_1, 200, r#"<div class="body">本文1</div>"#);
        http.add_text(section_2, 200, r#"<div class="body">本文2</div>"#);
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store.clone();
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects,
                assets,
                clock: Arc::new(FixedClock),
            },
            vec![setting],
            HashMap::new(),
        )
        .unwrap();

        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = CheckpointBudget::new(YieldBeforeSection(1), checkpoint.clone());
        let origin = test_origin(&downloader, &NovelObjectKeys::new("Resume Test", "n1234ab", false).unwrap()).await;
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: Some(&mut budget),
                    resume_from_section: None, access: Some(origin.clone()),
                    creation_reservation: None,
                },
            )
            .await
            .unwrap_err();
        let creation_reservation = budget.creation_reservation;
        assert!(creation_reservation.is_some());
        assert!(
            matches!(
                partial,
                crate::error::NarouError::DownloadBudgetExpired {
                    next_section_index: 1
                }
            ),
            "unexpected partial result: {partial:?}; requests={:?}",
            http.requested_urls()
        );
        let first_requests = http.requested_urls();
        assert_eq!(
            first_requests
                .iter()
                .filter(|url| url.ends_with(section_1))
                .count(),
            1
        );
        assert_eq!(
            first_requests
                .iter()
                .filter(|url| url.ends_with(section_2))
                .count(),
            0
        );

        http.add_text(target, 200, toc.replace("/n1234ab/1/", "/n1234ab/99/"));
        let changed = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: Some(1), access: Some(origin.clone()),
            creation_reservation,
        }).await.unwrap_err();
        assert!(matches!(changed, crate::error::NarouError::DownloadResumeCorrupt(_)),
            "changed TOC order cannot reuse a completed-prefix checkpoint");
        http.add_text(target, 200, toc);

        let completed = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: None,
                    resume_from_section: Some(1), access: Some(origin.clone()),
                    creation_reservation,
                },
            )
            .await
            .unwrap();
        assert_eq!(completed.updated_count, 2);
        assert_eq!(Some(completed.id), creation_reservation.map(|(id, _)| id.0));
        assert_eq!(completed.new_arrival_subtitles.iter().map(|section| section.index.as_str()).collect::<Vec<_>>(), ["1", "2"]);
        let requests = http.requested_urls();
        assert_eq!(
            requests
                .iter()
                .filter(|url| url.ends_with(section_1))
                .count(),
            1,
            "the persisted prefix must not be fetched again"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|url| url.ends_with(section_2))
                .count(),
            1
        );

        // Unchanged legacy data acquires a baseline in TOC/checkpoint metadata,
        // without rewriting the stored bodies or becoming a spurious update.
        let keys = NovelObjectKeys::new("Resume Test", "n1234ab", false).unwrap();
        let objects = downloader.persistence.objects().clone();
        let mut legacy_toc = downloader.persistence.load_toc(&mut test_read(&downloader.persistence, &keys).await, &keys).await.unwrap().unwrap();
        for subtitle in &mut legacy_toc.subtitles {
            subtitle.download_revision = 0;
            let mut section = downloader.persistence.load_section(&mut test_read(&downloader.persistence, &keys).await, &keys, subtitle).await.unwrap().unwrap();
            section.download_revision = 0;
            objects.write_small(&keys.section(&subtitle.index, &subtitle.file_subtitle),
                serde_yaml::to_string(&section).unwrap().into_bytes()).await.unwrap();
        }
        downloader.persistence.save_toc(&test_write(&downloader.persistence, &keys).await, &keys, &legacy_toc).await.unwrap();

        let mut unchanged_budget = YieldBeforeSection(1);
        let interrupted = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: Some(&mut unchanged_budget), resume_from_section: None, access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap_err();
        assert!(matches!(interrupted, crate::error::NarouError::DownloadBudgetExpired { .. }));
        let unchanged = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: Some(1), access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap();
        assert_eq!(unchanged.status, crate::downloader::UpdateStatus::None);
        assert_eq!(unchanged.updated_count, 0);
        assert!(!unchanged.new_arrivals);
        assert!(unchanged.new_arrival_subtitles.is_empty());

        // A checkpoint written by an old binary has no change identity. An
        // ambiguous prefix is counted once, even when its clock did not move.
        downloader.persistence.save_toc(&test_write(&downloader.persistence, &keys).await, &keys, &legacy_toc).await.unwrap();
        let mut legacy_checkpoint = legacy_toc.clone();
        legacy_checkpoint.subtitles.truncate(1);
        downloader.persistence.save_download_resume(&test_write(&downloader.persistence, &keys).await, &keys, &legacy_checkpoint).await.unwrap();
        let mut legacy_section = downloader.persistence.load_section(&mut test_read(&downloader.persistence, &keys).await, &keys, &legacy_toc.subtitles[0]).await.unwrap().unwrap();
        legacy_section.element.body = "旧版で保存した改稿".into();
        objects.write_small(&keys.section("1", "第1話"),
            serde_yaml::to_string(&legacy_section).unwrap().into_bytes()).await.unwrap();
        let recovered = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: Some(1), access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap();
        assert_eq!(recovered.status, crate::downloader::UpdateStatus::Ok);
        assert_eq!(recovered.updated_count, 1);

        // A later metadata-less recovery can still identify the unchanged old
        // body from the committed baseline; it must not convert on every run.
        let mut stop = YieldBeforeSection(1);
        downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: Some(&mut stop), resume_from_section: None, access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap_err();
        downloader.persistence.clear_download_resume(&test_write(&downloader.persistence, &keys).await, &keys).await.unwrap();
        let unchanged = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: Some(1), access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap();
        assert_eq!(unchanged.status, crate::downloader::UpdateStatus::None);
        assert_eq!(unchanged.updated_count, 0);
        assert_eq!(http.requested_urls().iter().filter(|url| url.ends_with(section_1) || url.ends_with(section_2)).count(), 2);

        // A new forced job can fail after replacing its first body, before a
        // budget yield. Its retry must not reuse a canceled job's old times.
        downloader.section_cache = super::section::SectionCache::new();
        let mut stop = YieldBeforeSection(1);
        downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: true, budget: Some(&mut stop), resume_from_section: None, access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap_err();
        let mut old = downloader.persistence.load_download_resume(&mut test_read(&downloader.persistence, &keys).await, &keys).await.unwrap().unwrap();
        old.subtitles[0].download_time = Some("2000-01-01 00:00:00.000000 +0000".into());
        let first_key = keys.section("1", "第1話");
        let mut first: SectionFile = serde_yaml::from_slice(&objects.read_small(&first_key).await.unwrap().unwrap()).unwrap();
        first.download_time = old.subtitles[0].download_time.clone();
        objects.write_small(&first_key, serde_yaml::to_string(&first).unwrap().into_bytes()).await.unwrap();
        downloader.persistence.save_download_resume(&test_write(&downloader.persistence, &keys).await, &keys, &old).await.unwrap();
        http.add_text(section_1, 200, r#"<div class="body">改稿1</div>"#);
        http.add_text(section_2, 500, "temporary failure");
        downloader.section_cache = super::section::SectionCache::new();
        let mut unlimited = YieldBeforeSection(usize::MAX);
        let failed = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: true, budget: Some(&mut unlimited), resume_from_section: None, access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap_err();
        assert!(matches!(failed, crate::error::NarouError::Http(_)));
        http.add_text(section_2, 200, r#"<div class="body">本文2</div>"#);
        let retried = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: Some(1), access: Some(origin.clone()),
            creation_reservation: None,
        }).await.unwrap();
        assert_eq!(retried.status, crate::downloader::UpdateStatus::Ok);
        assert_eq!(retried.updated_count, 1, "a changed prefix with an unchanged tail still requires conversion");
        let toc = downloader.persistence.load_toc(&mut test_read(&downloader.persistence, &keys).await, &keys).await.unwrap().unwrap();
        let first = downloader.persistence.load_section(&mut test_read(&downloader.persistence, &keys).await, &keys, &toc.subtitles[0]).await.unwrap().unwrap();
        assert_eq!(first.element.body, "改稿1");
        assert_ne!(toc.subtitles[0].download_time, old.subtitles[0].download_time);
        assert_eq!(toc.subtitles[0].download_time, first.download_time);
    }

    /// 再開時のプレフィックスに欠落した話がある場合、その位置を報告して
    /// 中断する。欠落したプレフィックスは取り直さず、後続の本文取得も
    /// 始めない (途中まで成功として進めない)。
    #[tokio::test]
    async fn resume_with_a_missing_middle_section_reports_the_corrupt_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::new().unwrap();
        let previous_db = {
            let mut db_slot = db::DATABASE.lock();
            let previous_db = db_slot.take();
            *db_slot = Some(db);
            previous_db
        };
        let _db_guard = DatabaseGuard(previous_db);

        struct YieldBeforeSection(usize);

        impl SectionBudget for YieldBeforeSection {
            fn should_yield(&mut self, next_section_index: usize) -> bool {
                next_section_index >= self.0
            }
        }

        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Resume Corrupt Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Resume Corrupt Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();

        let target = "https://example.com/n1234ab/";
        let section_1 = "https://example.com/n1234ab/1/";
        let section_2 = "https://example.com/n1234ab/2/";
        let section_3 = "https://example.com/n1234ab/3/";
        let toc = r#"
<h1>Resume Corrupt Test</h1><span>Author</span>
<a href="/n1234ab/1/">第1話</a>
<a href="/n1234ab/2/">第2話</a>
<a href="/n1234ab/3/">第3話</a>
"#;
        let http = Arc::new(MockHttpClient::new());
        http.add_text(target, 200, toc);
        http.add_text(section_1, 200, r#"<div class="body">本文1</div>"#);
        http.add_text(section_2, 200, r#"<div class="body">本文2</div>"#);
        http.add_text(section_3, 200, r#"<div class="body">本文3</div>"#);
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store.clone();
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects,
                assets,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
        )
        .unwrap();

        // 1 回目: 第 1 話だけ保存して第 2 話の直前で予算により中断する。
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = CheckpointBudget::new(YieldBeforeSection(1), checkpoint.clone());
        let origin = test_origin(&downloader, &NovelObjectKeys::new("Resume Corrupt Test", "n1234ab", false).unwrap()).await;
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: Some(&mut budget),
                    resume_from_section: None, access: Some(origin.clone()),
                    creation_reservation: None,
                },
            )
            .await
            .unwrap_err();
        let creation_reservation = budget.creation_reservation;
        assert!(creation_reservation.is_some());
        assert!(
            matches!(
                partial,
                crate::error::NarouError::DownloadBudgetExpired {
                    next_section_index: 1
                }
            ),
            "unexpected partial result: {partial:?}; requests={:?}",
            http.requested_urls()
        );

        let keys = NovelObjectKeys::new("Resume Corrupt Test", "n1234ab", false).unwrap();
        let first_key = keys.section("1", "第1話");
        let objects = downloader.persistence.objects().clone();
        let first_body = objects.read_small(&first_key).await.unwrap().unwrap();
        objects.delete(&first_key).await.unwrap();
        let obsolete_key = keys.section("1", "旧題の第1話");
        objects.write_small(&obsolete_key, first_body.clone()).await.unwrap();
        let missing = downloader.download_novel_with_execution_options(target, DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: Some(1), access: Some(origin.clone()),
            creation_reservation,
        }).await.unwrap_err();
        assert!(matches!(missing, crate::error::NarouError::DownloadResumeCorrupt(_)));
        assert!(missing.to_string().contains("persisted section 0 (1) is missing"));
        objects.write_small(&first_key, first_body).await.unwrap();
        objects.delete(&obsolete_key).await.unwrap();

        // 2 回目: 第 2 話が欠落したまま 2 話分の復元を要求する。
        let error = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: None,
                    resume_from_section: Some(2), access: Some(origin.clone()),
                    creation_reservation,
                },
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            matches!(error, crate::error::NarouError::DownloadResumeCorrupt(_)),
            "unexpected error: {message}; requests={:?}",
            http.requested_urls()
        );
        assert!(
            message.contains("persisted section 1 (2) is missing"),
            "the missing middle section must be named: {message}"
        );

        let requests = http.requested_urls();
        assert_eq!(
            requests
                .iter()
                .filter(|url| url.ends_with(section_1))
                .count(),
            1,
            "the persisted prefix must not be fetched again: {requests:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|url| url.ends_with(section_2))
                .count(),
            0,
            "a corrupt checkpoint must fail before fetching after it: {requests:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|url| url.ends_with(section_3))
                .count(),
            0,
            "a corrupt checkpoint must fail before fetching after it: {requests:?}"
        );
    }

    /// 強更新 (`update.strong`) の予定段階の取得は予算の対象で、中断しても
    /// 取得済みの本文は保存される。以前は予定段階が予算チェックの外にあり、
    /// 予定段階だけで予算を使い切ると `next_section_index: 0` のまま同じ
    /// セクションを毎回取り直して前進しなかった。
    #[tokio::test]
    async fn strong_update_plan_phase_stops_at_the_budget_and_keeps_its_progress() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::new().unwrap();
        let previous_db = {
            let mut db_slot = db::DATABASE.lock();
            let previous_db = db_slot.take();
            *db_slot = Some(db);
            previous_db
        };
        let _db_guard = DatabaseGuard(previous_db);

        struct YieldBeforeSection(usize);

        impl SectionBudget for YieldBeforeSection {
            fn should_yield(&mut self, next_section_index: usize) -> bool {
                next_section_index >= self.0
            }
        }

        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Strong Budget Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Strong Budget Test
toc_url: 'https://example.com/\k<ncode>/'
timezone: UTC
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)" data-date="(?<subdate>[^<]+)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();

        let target = "https://example.com/n1234ab/";
        let section_1 = "https://example.com/n1234ab/1/";
        let section_2 = "https://example.com/n1234ab/2/";
        let http = Arc::new(MockHttpClient::new());
        http.add_text(
            target,
            200,
            r#"<h1>Strong Budget Test</h1><span>Author</span>
<a href="/n1234ab/1/" data-date="2000-01-01">第一話</a>
<a href="/n1234ab/2/" data-date="2000-01-01">第二話</a>"#,
        );
        http.add_text(section_1, 200, r#"<div class="body">本文1</div>"#);
        http.add_text(section_2, 200, r#"<div class="body">本文2</div>"#);

        struct StrongSettings;
        impl crate::downloader::settings::DownloaderSettings for StrongSettings {
            fn local_setting_bool(&self, key: &str) -> bool {
                key == "update.strong"
            }
        }

        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store.clone();
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects: objects.clone(),
                assets,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
            Arc::new(StrongSettings),
        )
        .unwrap();
        let keys = NovelObjectKeys::new("Strong Budget Test", "n1234ab", false).unwrap();

        let initial = downloader.download_novel(target).await.unwrap();
        // 強更新の基準日 (`subupdate`) と保存済み本文の取得日を揃え、
        // 本文が変わったセクションだけが強更新の予定段階を通るようにする。
        let toc_bytes = objects.read_small(&keys.toc()).await.unwrap().unwrap();
        let toc: TocFile = serde_yaml::from_slice(&toc_bytes).unwrap();
        for subtitle in &toc.subtitles {
            let key = keys.section(&subtitle.index, &subtitle.file_subtitle);
            let mut section: SectionFile =
                serde_yaml::from_slice(&objects.read_small(&key).await.unwrap().unwrap()).unwrap();
            section.download_time = Some("2000-01-01T00:00:00Z".into());
            objects
                .write_small(&key, serde_yaml::to_string(&section).unwrap().into_bytes())
                .await
                .unwrap();
        }
        // A new Worker invocation has a fresh HTTP/section cache.
        let http = Arc::new(MockHttpClient::new());
        downloader.http = http.clone();
        downloader.section_cache = super::section::SectionCache::new();
        http.add_text(
            target,
            200,
            r#"<h1>Strong Budget Test</h1><span>Author</span>
<a href="/n1234ab/1/" data-date="2000-01-02">第一話</a>
<a href="/n1234ab/2/" data-date="2000-01-02">第二話</a>"#,
        );
        http.add_text(section_1, 200, r#"<div class="body">改稿1</div>"#);
        http.add_text(section_2, 200, r#"<div class="body">改稿2</div>"#);

        // 1 回目: 予算内でセクション 0 を取得・保存し、保存済み prefix の
        // 次 (1) で止まる。予定段階の取得で止まると同じ prefix を毎回
        // 取り直して前進しなくなる (修正前の livelock) ので、ここは必ず
        // 「保存済みの次」を指す。
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let origin = test_origin(&downloader, &keys).await;
        let mut budget = CheckpointBudget::new(YieldBeforeSection(1), checkpoint.clone());
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: None, access: Some(origin.clone()),
                    creation_reservation: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(budget.allocations, 0, "an existing-record update never allocates a replacement identity");
        assert_eq!(budget.restorations, 0, "an ordinary update has no creation receipt to restore");
        let creation_reservation = budget.creation_reservation;
        assert!(creation_reservation.is_none());
        let checkpoint_key = ObjectKey::try_new(".narou-transfer/test-create-checkpoint").unwrap();
        let checkpoint_bytes = checkpoint.read_small(&checkpoint_key).await.unwrap().unwrap();
        let (saved_origin, saved_creation): (AccessOrigin, Option<(NovelId, i64)>) = serde_json::from_slice(&checkpoint_bytes).unwrap();
        assert_eq!(saved_origin, origin.origin());
        assert_eq!(saved_creation, creation_reservation);
        assert!(
            matches!(
                partial,
                crate::error::NarouError::DownloadBudgetExpired {
                    next_section_index: 1
                }
            ),
            "unexpected partial result: {partial:?}; requests={:?}",
            http.requested_urls()
        );
        let first_section: SectionFile = serde_yaml::from_slice(
            &objects
                .read_small(&keys.section("1", "第一話"))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first_section.element.body, "改稿1");
        // 予算 1 は「本文を 1 件保存したら止まる」なので、未保存の後続を
        // 取得しない。
        assert_eq!(
            http.requested_urls()
                .iter()
                .filter(|url| url.ends_with(section_2))
                .count(),
            0,
            "予算外のセクションは取得しない"
        );

        // 2 回目: 同じ予算で再開。境界で直ちに止まるので後続を取得せず、
        // 保存済み本文も取り直さない。
        let mut budget = CheckpointBudget::new(YieldBeforeSection(1), checkpoint.clone());
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: Some(1), access: Some(origin.clone()),
                    creation_reservation,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(budget.creation_reservation, creation_reservation);
        assert_eq!(budget.allocations, 0);
        assert_eq!(budget.restorations, 0);
        assert!(
            matches!(
                partial,
                crate::error::NarouError::DownloadBudgetExpired {
                    next_section_index: 1
                }
            ),
            "unexpected partial result: {partial:?}"
        );
        let first_section: SectionFile = serde_yaml::from_slice(
            &objects
                .read_small(&keys.section("1", "第一話"))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            first_section.element.body, "改稿1",
            "再開時に保存済みの本文を上書きしない"
        );

        // 3 回目: 1 件先まで許可し、保存済み prefix の次まで進む。
        let mut budget = CheckpointBudget::new(YieldBeforeSection(2), checkpoint.clone());
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: Some(1), access: Some(origin.clone()),
                    creation_reservation,
                },
            )
            .await
            .unwrap();
        assert_eq!(budget.creation_reservation, creation_reservation);
        assert_eq!(budget.allocations, 0);
        assert_eq!(budget.restorations, 0);
        assert_eq!(partial.status, crate::downloader::types::UpdateStatus::Ok);
        assert_eq!(partial.updated_count, 2);
        assert_eq!(partial.id, initial.id, "the update retains the existing novel identity");
        assert_eq!(checkpoint.read_small(&checkpoint_key).await.unwrap().unwrap(), checkpoint_bytes,
            "all update continuations retain the original epoch and no creation reservation");
        assert_eq!(
            http.requested_urls().iter().filter(|url| url.ends_with(section_1)).count(),
            1,
            "保存済み prefix の本文を取り直さない"
        );
        assert_eq!(
            http.requested_urls().iter().filter(|url| url.ends_with(section_2)).count(),
            1,
            "予算に収まった後続を一度だけ取得する"
        );

    }

    #[tokio::test]
    async fn budget_resume_advances_within_one_illustrated_section() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let previous_db = db::DATABASE.lock().replace(Database::new().unwrap());
        let _db_guard = DatabaseGuard(previous_db);
        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Image Budget Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Image Budget Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: '<a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        ).unwrap();
        setting.compile();
        struct ImageBudget(Arc<MockHttpClient>);
        impl SectionBudget for ImageBudget {
            fn should_yield(&mut self, _next_section_index: usize) -> bool {
                self.0.requested_urls().iter().filter(|url| url.contains("/images/")).count() >= 2
            }
        }
        let target = "https://example.com/n1234ab/";
        let image_urls: Vec<_> = (0..5).map(|i| format!("https://example.com/images/{i}.png")).collect();
        let body = image_urls.iter().map(|url| format!("<img src=\"{url}\" />")).collect::<String>();
        let keys = NovelObjectKeys::new("Image Budget Test", "n1234ab", false).unwrap();
        for force in [false, true] {
            let store = Arc::new(MemoryObjectStore::new());
            let novels = Arc::new(MemoryNovelRepository::new().with_objects(store.clone()));
            let test_objects: Arc<dyn ObjectStore> = store.clone();
            let test_assets: Arc<dyn AssetStore> = store.clone();
            let origin = NovelWriteGuard::capture_for(&test_objects, Some(&test_assets), &keys).await.unwrap();
            let mut fetched = Vec::new();
            let checkpoint = Arc::new(MemoryObjectStore::new());
            let mut creation_reservation = None;
            for invocation in 0..3 {
                let http = Arc::new(MockHttpClient::new());
                http.add_text(target, 200, r#"<h1>Image Budget Test</h1><span>Author</span><a href="/n1234ab/1/">第一話</a>"#);
                http.add_text("https://example.com/n1234ab/1/", 200, format!("<div class=\"body\">{body}</div>"));
                for (i, url) in image_urls.iter().enumerate() {
                    http.add_response(url, HttpResponse {
                        status: 200,
                        headers: vec![("Content-Type".into(), "image/png".into())],
                        body: vec![i as u8; 8],
                    });
                }
                let mut downloader = Downloader::with_platform_and_storage_and_settings(
                    DownloaderPlatform {
                        http: http.clone(),
                        rate_limiter: Arc::new(FakeRateLimiter::new()),
                        novels: novels.clone(),
                        objects: store.clone(),
                        assets: store.clone(),
                        clock: Arc::new(SystemClock),
                    },
                    vec![setting.clone()],
                    HashMap::new(),
                ).unwrap();
                let mut budget = CheckpointBudget::new(ImageBudget(http.clone()), checkpoint.clone());
                let result = downloader.download_novel_with_execution_options(
                    target,
                    DownloadExecutionOptions {
                        force,
                        budget: Some(&mut budget),
                        resume_from_section: (invocation > 0).then_some(0), access: Some(origin.clone()),
                        creation_reservation,
                    },
                ).await;
                if creation_reservation.is_some() { assert_eq!(budget.creation_reservation, creation_reservation); }
                creation_reservation = budget.creation_reservation;
                assert_eq!(budget.allocations, usize::from(invocation == 0), "an unfinished illustrated section retains its allocated identity");
                assert_eq!(budget.restorations, usize::from(invocation > 0), "illustrated continuations restore, never allocate");
                assert!(creation_reservation.is_some());
                if invocation < 2 {
                    assert!(matches!(result, Err(crate::error::NarouError::DownloadBudgetExpired { next_section_index: 0 })),
                        "the unfinished section must yield before its third new image: {result:?}");
                } else {
                    assert_eq!(result.unwrap().total_count, 1);
                }
                fetched.extend(http.requested_urls().into_iter().filter(|url| url.contains("/images/")));
                let cache = ObjectStore::read_small(store.as_ref(), &keys.illustration_cache()).await.unwrap().unwrap();
                let index: crate::illustration_store::IllustrationIndex = serde_yaml::from_slice(&cache).unwrap();
                let saved = ((invocation + 1) * 2).min(image_urls.len());
                assert_eq!(index.source_count(), saved, "every yield must durably retain image progress");
                for (i, url) in image_urls.iter().enumerate().take(saved) {
                    let filename = index.filename_for_source(url).unwrap();
                    let bytes = ObjectStore::read_small(store.as_ref(), &keys.illustration(filename).unwrap()).await.unwrap().unwrap();
                    assert_eq!(bytes, vec![i as u8; 8]);
                }
            }
            let expected: Vec<_> = image_urls.iter().map(|url| format!("GET {url}")).collect();
            assert_eq!(fetched, expected, "fresh invocations must fetch each image exactly once");
        }
    }

    /// 中断後の新しい実行枠を、取得済み挿絵の再取得で使い切らずに続行できる。
    #[tokio::test]
    async fn budget_resume_keeps_illustrations_without_refetching() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::new().unwrap();
        let previous_db = {
            let mut db_slot = db::DATABASE.lock();
            let previous_db = db_slot.take();
            *db_slot = Some(db);
            previous_db
        };
        let _db_guard = DatabaseGuard(previous_db);

        struct YieldBeforeSection(usize);

        impl SectionBudget for YieldBeforeSection {
            fn should_yield(&mut self, next_section_index: usize) -> bool {
                next_section_index >= self.0
            }
        }

        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Resume Image Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Resume Image Test
toc_url: 'https://example.com/\k<ncode>/'
subtitles: |-
  <a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>
body_pattern: '<div class="body">(?<body>.+?)</div>'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
is_narou: false
"#,
        )
        .unwrap();
        setting.compile();

        let target = "https://example.com/n1234ab/";
        let section_1 = "https://example.com/n1234ab/1/";
        let section_2 = "https://example.com/n1234ab/2/";
        let image_url = "https://9496.mitemin.net/userpageimage/viewimage/icode/i83860/";
        let next_image_url = "https://9496.mitemin.net/userpageimage/viewimage/icode/i83861/";
        let second_body = r#"本文2<img src="//9496.mitemin.net/userpageimage/viewimagebig/icode/i83861/" />"#;
        let second_page = format!(r#"<div class="body">{second_body}</div>"#);
        let http = Arc::new(MockHttpClient::new());
        let toc = r#"<h1>Resume Image Test</h1><span>Author</span>
<a href="/n1234ab/1/">第一話</a>
<a href="/n1234ab/2/">第二話</a>"#;
        http.add_text(target, 200, toc);
        http.add_text(
            section_1,
            200,
            r#"<div class="body">本文1<img src="//9496.mitemin.net/userpageimage/viewimagebig/icode/i83860/" /></div>"#,
        );
        http.add_text(section_2, 200, second_page.as_str());
        let png = base64::engine::general_purpose::STANDARD
            .decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO3Z5A0AAAAASUVORK5CYII=",
            )
            .unwrap();
        http.add_response(
            image_url,
            HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "image/png".into())],
                body: png.clone(),
            },
        );

        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store.clone();
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects: objects.clone(),
                assets: assets.clone(),
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
        )
        .unwrap();
        let keys = NovelObjectKeys::new("Resume Image Test", "n1234ab", false).unwrap();

        // 第 1 話と挿絵を保存した後、第 2 話の直前で実行枠を使い切る。
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = CheckpointBudget::new(YieldBeforeSection(1), checkpoint.clone());
        let origin = test_origin(&downloader, &NovelObjectKeys::new("Resume Image Test", "n1234ab", false).unwrap()).await;
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: Some(&mut budget),
                    resume_from_section: None, access: Some(origin.clone()),
                    creation_reservation: None,
                },
            )
            .await
            .unwrap_err();
        let creation_reservation = budget.creation_reservation;
        assert!(creation_reservation.is_some());
        assert!(matches!(
            partial,
            crate::error::NarouError::DownloadBudgetExpired { .. }
        ));
        assert!(
            AssetStore::stat(assets.as_ref(), &keys.illustration("i83860.png").unwrap())
                .await
                .unwrap()
                .is_some(),
            "第 1 話の挿絵は保存されている"
        );

        // 再開した実行枠でも挿絵を取り直すと、再び同じ位置で中断してしまう。
        struct ImageFetchBudget(Arc<MockHttpClient>, &'static str);

        impl SectionBudget for ImageFetchBudget {
            fn should_yield(&mut self, _next_section_index: usize) -> bool {
                self.0.requested_urls().iter().any(|url| url.ends_with(self.1))
            }
        }

        let http = Arc::new(MockHttpClient::new());
        http.add_text(target, 200, toc);
        http.add_text(section_2, 200, second_page);
        http.add_response(
            image_url,
            HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "image/png".into())],
                body: png.clone(),
            },
        );
        let mut next_png = png;
        next_png.push(0);
        http.add_response(
            next_image_url,
            HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "image/png".into())],
                body: next_png,
            },
        );
        downloader.http = http.clone();
        downloader.section_cache = super::section::SectionCache::new();
        let mut budget = ImageFetchBudget(http.clone(), image_url);
        downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: Some(1), access: Some(origin.clone()),
                    creation_reservation,
                },
            )
            .await
            .unwrap();
        assert!(
            !http.requested_urls().iter().any(|url| url.ends_with(image_url)),
            "保存済み挿絵の再取得で再開後の実行枠を消費してはならない"
        );
        assert_eq!(
            http.requested_urls().iter().filter(|url| url.ends_with(next_image_url)).count(),
            1,
            "再開後の新しい挿絵も本文と raw HTML の重複で取り直してはならない"
        );
        let saved = objects
            .read_small(&keys.section("2", "第二話"))
            .await
            .unwrap()
            .expect("再開後は次の話まで保存する");
        let saved: super::types::SectionFile = serde_yaml::from_slice(&saved).unwrap();
        assert_eq!(saved.element.body, second_body);
        let cache = objects
            .read_small(&keys.illustration_cache())
            .await
            .unwrap()
            .expect("再開後は索引が保存される");
        let index: crate::illustration_store::IllustrationIndex =
            serde_yaml::from_slice(&cache).unwrap();
        assert_eq!(
            index.filename_for_source(
                "//9496.mitemin.net/userpageimage/viewimagebig/icode/i83860/"
            ),
            Some("i83860.png")
        );
        {
            let (_, unresolved) = crate::illustration_refs::rewrite(
                "［＃挿絵（//9496.mitemin.net/userpageimage/viewimagebig/icode/i83860/）入る］".into(),
                |url| {
                    index
                        .filename_for_source(url)
                        .map(|name| format!("挿絵/{name}"))
                },
            );
            assert!(
                unresolved.is_empty(),
                "再開後は EPUB の挿絵参照が解決できる: {unresolved:?}"
            );
        }
    }

    #[test]
    fn existing_record_keeps_file_title_when_site_naming_changes() {
        let store = Arc::new(MemoryObjectStore::new());
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
            },
            Vec::new(), HashMap::new(),
        ).unwrap();
        let mut record = sample_record(chrono::DateTime::from_timestamp(0, 0).unwrap());
        for saved in ["n1234ab", "N1234AB", "保存済みフォルダ"] {
            record.file_title = saved.to_string();
            for ncode in [Some("n1234ab".to_string()), Some("n9999zz".to_string()), None] {
                for append in [false, true] {
                    for title in ["", "新しい題名"] {
                        assert_eq!(downloader.compute_file_title(&ncode, title, append, Some(&record)), saved);
                    }
                }
            }
        }
    }

    #[test]
    fn new_file_title_uses_ruby_fullwidth_special_chars() {
        let store = Arc::new(MemoryObjectStore::new());
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
            },
            Vec::new(), HashMap::new(),
        ).unwrap();
        assert_eq!(
            downloader.compute_file_title(
                &Some("n1234ab".to_string()),
                "作品<C201>タイトル",
                true,
                None,
            ),
            "n1234ab 作品〈C201〉タイトル"
        );
    }

    #[test]
    fn resolve_user_agent_uses_saved_value_when_cli_value_missing() {
        assert_eq!(
            resolve_user_agent(None, Some("saved-agent".to_string())),
            "saved-agent"
        );
    }

    #[test]
    fn resolve_user_agent_treats_auto_as_randomized_value() {
        let resolved = resolve_user_agent(Some("auto"), Some("saved-agent".to_string()));
        assert!(!resolved.is_empty());
        assert_ne!(resolved, "saved-agent");
        assert_ne!(resolved, "auto");
    }

    #[test]
    fn resolve_user_agent_treats_saved_auto_as_randomized_value() {
        let resolved = resolve_user_agent(None, Some("auto".to_string()));
        assert!(!resolved.is_empty());
        assert_ne!(resolved, "auto");
    }

    #[tokio::test]
    async fn filename_mapping_change_forces_body_rewrite_with_unchanged_dates() {
        let store = Arc::new(MemoryObjectStore::new());
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new().with_objects(store.clone())),
                objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
            },
            Vec::new(), HashMap::new(),
        ).unwrap();
        let keys = NovelObjectKeys::new("site", "n1234ab", false).unwrap();
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "ncode.syosetu.com").unwrap().clone();
        let mut setting = setting;

        // 連載・短編のどちらも、日付と表示名が同じでも file_subtitle が
        // 変われば本文を書き直す。旧 `_` サニタイザからの移行がこれに当たる。
        // 3 行目は制御ケース (何も変わっていなければ再取得しない)。
        for (title, old_name, new_name, expected) in [
            ("第1話 Part:1", "第1話_Part_1", "第1話_Part：1", true),
            ("Part:1", "Part_1", "Part：1", true),
            ("Part:1", "Part_1", "Part_1", false),
        ] {
            let old = super::types::SubtitleInfo {
                index: "1".to_string(), href: String::new(), chapter: String::new(),
                subchapter: String::new(), subtitle: title.to_string(),
                file_subtitle: old_name.to_string(), subdate: "2026-01-01".to_string(),
                subupdate: Some("2026-01-01".to_string()), download_time: None,
                download_revision: 1,
            };
            let mut latest = old.clone();
            latest.file_subtitle = new_name.to_string();
            let mut auth = super::auth::AuthState::new(None);
            let mut access = super::NovelAccess::read_only(test_read(&downloader.persistence, &keys).await);
            let (needs, _) = downloader.section_needs_download(
                &mut access,
                super::SectionDecision {
                    setting: &mut setting,
                    auth: &mut auth,
                    latest: &latest,
                    old: Some(&old),
                    existing_id: None,
                    keys: &keys,
                    toc_url: "https://example.com/n1234ab/",
                    strong_update: false,
                    timezone: super::SiteTimezone::Named(chrono_tz::Asia::Tokyo),
                    existed: true,
                },
            ).await.unwrap();
            assert_eq!(needs, expected, "{title}: {old_name} -> {new_name}");
        }
    }

    #[test]
    fn update_date_comparison_uses_newer_dates_not_inequality() {
        assert!(super::date_string_is_newer(
            "2026-04-12 10:00",
            "2026-04-12 09:59"
        ));
        assert!(!super::date_string_is_newer(
            "2026-04-12 09:59",
            "2026-04-12 10:00"
        ));
        assert!(!super::date_string_is_newer(
            "2026年04月12日 10時00分",
            "2026-04-12 10:00"
        ));
    }

    #[test]
    fn update_strong_date_basis_matches_ruby_ymd_conversion() {
        assert_eq!(
            super::date_string_to_ymd("2026年04月12日 10時00分"),
            Some("20260412".to_string())
        );
        assert_eq!(
            super::date_string_to_ymd("2025年 10月 28日 07時 56分"),
            Some("20251028".to_string())
        );
        assert_eq!(
            super::date_string_to_ymd("2026-04-12 10:00:00.123456 +0900"),
            Some("20260412".to_string())
        );
    }

    #[test]
    fn timezone_less_site_datetime_is_interpreted_in_site_timezone() {
        let parsed =
            super::parse_datetime_with_timezone("2026年04月19日 12時00分", Some("Asia/Tokyo"))
                .expect("datetime");

        assert_eq!(
            parsed.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-04-19 03:00:00"
        );
        assert_eq!(
            parsed
                .with_timezone(&chrono_tz::Asia::Tokyo)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            "2026-04-19 12:00:00"
        );
    }

    #[test]
    fn timezone_less_site_datetime_accepts_fixed_offset_setting() {
        let parsed = super::parse_datetime_with_timezone("2026-04-19 12:00:00", Some("+09:00"))
            .expect("datetime");

        assert_eq!(
            parsed.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-04-19 03:00:00"
        );
    }

    #[test]
    fn forced_redownload_never_reports_no_updates() {
        assert!(matches!(
            super::resolve_download_status(false, 0, Some(1), false, false, false, false),
            super::types::UpdateStatus::None
        ));
        assert!(matches!(
            super::resolve_download_status(true, 0, Some(1), false, false, false, false),
            super::types::UpdateStatus::Ok
        ));
    }

    #[test]
    fn no_update_preserves_update_timestamps() {
        assert!(!super::should_replace_last_update(
            super::types::UpdateStatus::None
        ));
        assert!(super::should_replace_last_update(
            super::types::UpdateStatus::Ok
        ));
    }

    fn sample_record(
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> crate::db::novel_record::NovelRecord {
        crate::db::novel_record::NovelRecord {
            id: 1,
            author: "author".to_string(),
            title: "title".to_string(),
            file_title: "file-title".to_string(),
            toc_url: "https://example.com".to_string(),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: timestamp,
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: Some(timestamp),
            general_lastup: Some(timestamp),
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: Some("example.com".to_string()),
            general_all_no: Some(1),
            length: Some(100),
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn none_status_keeps_last_update_and_general_lastup_in_sync() {
        let original = chrono::Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap();
        let fetched = original + chrono::Duration::hours(2);
        let mut updated = sample_record(original);
        let record = sample_record(fetched);

        super::merge_update_timestamps(&mut updated, &record, super::types::UpdateStatus::None);

        assert_eq!(updated.last_update, original);
        assert_eq!(updated.novelupdated_at, Some(original));
        assert_eq!(updated.general_lastup, Some(original));
    }

    #[test]
    fn ok_status_refreshes_last_update_and_general_lastup_together() {
        let original = chrono::Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap();
        let fetched = original + chrono::Duration::hours(2);
        let mut updated = sample_record(original);
        let record = sample_record(fetched);

        super::merge_update_timestamps(&mut updated, &record, super::types::UpdateStatus::Ok);

        assert_eq!(updated.last_update, fetched);
        assert_eq!(updated.novelupdated_at, Some(fetched));
        assert_eq!(updated.general_lastup, Some(fetched));
    }

    #[test]
    fn short_story_subtitles_use_novel_info_dates() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "ncode.syosetu.com")
            .unwrap();
        let mut raw_captures = std::collections::HashMap::new();
        raw_captures.insert("gf".to_string(), "2024-01-02 03:04".to_string());
        raw_captures.insert("gl".to_string(), "2024-01-05 06:07".to_string());

        let info = NovelInfo {
            title: Some("短編タイトル".to_string()),
            author: None,
            story: None,
            novel_type: Some(2),
            end: Some(true),
            general_firstup: None,
            general_lastup: None,
            novelupdated_at: None,
            length: None,
            tags: None,
            sitename: None,
            raw_captures: raw_captures.clone(),
        };
        let subtitles = futures::executor::block_on(super::toc::create_short_story_subtitles(setting, "", &info)).unwrap();
        assert_eq!(subtitles[0].subdate, "2024-01-02 03:04");
        assert_eq!(subtitles[0].subupdate.as_deref(), Some("2024-01-05 06:07"));

        raw_captures.remove("gl");
        let info = NovelInfo {
            raw_captures,
            ..info
        };
        let subtitles = futures::executor::block_on(super::toc::create_short_story_subtitles(setting, "", &info)).unwrap();
        assert_eq!(subtitles[0].subupdate.as_deref(), Some("2024-01-02 03:04"));
    }

    #[test]
    fn kakuyomu_yaml_preprocess_supports_table_of_contents_v2_and_tags() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "カクヨム").unwrap();
        assert!(setting.preprocess_pipeline().is_some());

        let json = r#"{
            "props": {
                "pageProps": {
                    "__APOLLO_STATE__": {
                        "Work:1177354055617350769": {
                            "title": "先輩の妹じゃありません！",
                            "author": {"__ref": "UserAccount:1"},
                            "alternateAuthorName": null,
                            "introduction": "intro\nbody",
                            "serialStatus": "COMPLETED",
                            "publicEpisodeCount": 1,
                            "publishedAt": "2021-01-10T16:13:02Z",
                            "editedAt": "2021-01-11T16:13:02Z",
                            "lastEpisodePublishedAt": "2021-01-12T16:13:02Z",
                            "totalCharacterCount": 1234,
                            "tagLabels": ["tag-a", "tag-b"],
                            "tableOfContents": [],
                            "tableOfContentsV2": [{"__ref": "TableOfContentsChapter:10"}]
                        },
                        "UserAccount:1": {
                            "activityName": "author-name"
                        },
                        "TableOfContentsChapter:10": {
                            "chapter": {"__ref": "Chapter:10"},
                            "episodeUnions": [{"__ref": "Episode:20"}, {"__ref": "Episode:21"}]
                        },
                        "Chapter:10": {
                            "__typename": "Chapter",
                            "id": "10",
                            "level": 1,
                            "title": "第一章"
                        },
                        "Episode:20": {
                            "__typename": "Episode",
                            "id": "20",
                            "publishedAt": "2021-01-12T16:13:02Z",
                            "editedAt": "2021-01-13T16:13:02Z",
                            "title": "第1話"
                        },
                        "Episode:21": {
                            "__typename": "Episode",
                            "id": "21",
                            "publishedAt": "2021-01-14T16:13:02Z",
                            "title": "第2話"
                        }
                    }
                }
            },
            "query": {
                "workId": "1177354055617350769"
            }
        }"#;
        let mut html = format!(
            r#"<html><script id="__NEXT_DATA__" type="application/json">{}</script></html>"#,
            json
        );

        super::util::pretreatment_source(&mut html, "UTF-8", Some(setting));

        assert!(html.contains("KakuyomuPreprocessEvalMagicWord"));
        assert!(html.contains("title::先輩の妹じゃありません！"));
        assert!(html.contains("author::author-name"));
        assert!(html.contains("introduction::intro<br>body"));
        assert!(html.contains("tag::tag-a"));
        assert!(html.contains("tag::tag-b"));
        let tags = futures::executor::block_on(setting.resolve_info_pattern("tags", &html)).unwrap();
        assert_eq!(super::sanitize_site_tags(&tags), vec!["tag-a", "tag-b"]);
        assert!(html.contains("Chapter;1;10;第一章"));
        assert!(!html.contains("Chapter;1;10;;第一章"));
        assert!(html.contains("Episode;20;2021-01-12T16:13:02Z;2021-01-13T16:13:02Z;第1話"));
        assert!(html.contains("Episode;21;2021-01-14T16:13:02Z;2021-01-14T16:13:02Z;第2話"));
        let mut url_captures = HashMap::new();
        url_captures.insert("ncode".to_string(), "1177354055617350769".to_string());
        let subtitles = super::toc::parse_subtitles(setting, &html, &url_captures).unwrap();
        assert_eq!(subtitles[0].subdate, "2021-01-12T16:13:02Z");
        assert_eq!(
            subtitles[0].subupdate.as_deref(),
            Some("2021-01-13T16:13:02Z")
        );
        assert_eq!(subtitles[1].subdate, "2021-01-14T16:13:02Z");
        assert_eq!(
            subtitles[1].subupdate.as_deref(),
            Some("2021-01-14T16:13:02Z")
        );
    }

    #[test]
    fn akatsuki_yaml_preprocess_keeps_body_paragraph_breaks() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "www.akatsuki-novels.com")
            .expect("bundled www.akatsuki-novels.com.yaml should load");
        assert!(setting.preprocess_pipeline().is_some());

        // リニューアル後の本文は <br /> ではなく CSS whitespace-pre-line の
        // 生改行で段落を表す。旧デザインと同じ <br> へ変換されなければならない。
        let mut html = "<html><body><div class=\"whitespace-pre-line reading-body text-sm\">　はたから見ればほんの一瞬の事だったんだろう。\n\n　スマートフォンに目を落とし、</div></body></html>".to_string();
        super::util::pretreatment_source(&mut html, "UTF-8", Some(setting));

        assert!(html.contains("AkatsukiPreprocessMagicWord2026"));
        assert!(html.contains(
            "はたから見ればほんの一瞬の事だったんだろう。<br><br>　スマートフォンに目を落とし、"
        ), "got: {html}");

        let (element, _) = super::section::parse_section_html(setting, html).unwrap();
        let aozora = super::html::to_aozora(&element.body);
        assert_eq!(
            aozora,
            "　はたから見ればほんの一瞬の事だったんだろう。\n\n　スマートフォンに目を落とし、"
        );
    }

    fn pixiv_setting() -> &'static SiteSetting {
        static SETTING: std::sync::OnceLock<SiteSetting> = std::sync::OnceLock::new();
        SETTING.get_or_init(|| {
            let settings = SiteSetting::load_all().unwrap();
            settings
                .iter()
                .find(|s| s.domain == "www.pixiv.net")
                .cloned()
                .expect("bundled www.pixiv.net.yaml should load")
        })
    }

    fn run_pixiv_preprocess(json: &str) -> String {
        let setting = pixiv_setting();
        let mut source = json.to_string();
        super::util::pretreatment_source(&mut source, "UTF-8", Some(setting));
        source
    }

    #[test]
    fn a_target_that_differs_from_the_toc_url_is_remembered() {
        // Pixiv は取得に API を使うため toc_url が API になる。利用者が指定した
        // ページ URL を控えておかないと、Web UI のリンクが API を開いてしまう。
        let record = crate::db::novel_record::NovelRecord {
            toc_url: "https://www.pixiv.net/ajax/novel/series/768265".to_string(),
            ..sample_record(chrono::Utc::now())
        };
        assert_eq!(record.display_url(), record.toc_url, "未記録なら toc_url");

        let mut record = record;
        record.set_original_url("https://www.pixiv.net/novel/series/768265");
        assert_eq!(
            record.display_url(),
            "https://www.pixiv.net/novel/series/768265"
        );
        // 追加フィールドとして保存され、往復しても保たれる。
        let yaml = serde_yaml::to_string(&record).unwrap();
        assert!(yaml.contains("original_url:"), "got {yaml}");
        let back: crate::db::novel_record::NovelRecord = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.display_url(), record.display_url());
    }

    #[test]
    fn pixiv_author_listing_yields_novels_and_series() {
        // 作者ページの作品一覧 (/ajax/user/{id}/profile/all) を preprocess に通すと、
        // 単体小説と小説シリーズが作品として並ぶ。
        let json = r#"{"error":false,"message":"","body":{"novels":{"2594847":null,"2610142":null},
            "novelSeries":[{"id":"272850","userId":"1"},{"id":"551006","userId":"1"}],
            "mangaSeries":[{"id":"329992","userId":"1"}],
            "illusts":{"149803200":null,"149768354":null},
            "manga":[]}}"#;
        let html = run_pixiv_preprocess(json);

        assert!(html.contains("author_novel::https://www.pixiv.net/novel/show.php?id=2594847"));
        assert!(html.contains("author_novel::https://www.pixiv.net/novel/show.php?id=2610142"));
        assert!(html.contains("author_novel::https://www.pixiv.net/novel/series/272850"));
        assert!(html.contains("author_novel::https://www.pixiv.net/novel/series/551006"));
        // 漫画シリーズも作品として並ぶ。
        assert!(html.contains("author_novel::https://www.pixiv.net/ajax/series/329992"));
        assert!(html.contains("author_comic_series::329992"));
        // 単体イラストも作品。古い順に並べる (一覧は新しい順で返る)。
        assert!(html.contains("author_novel::https://www.pixiv.net/artworks/149803200"));
        assert!(html.contains("author_novel::https://www.pixiv.net/artworks/149768354"));
        // 並びは一覧の JSON 順 (id の文字列昇順) で、narou_bridge の sorted() と同じ。
        assert!(
            html.find("artworks/149768354") < html.find("artworks/149803200"),
            "一覧の順に並べる: {html}"
        );
        // 空のときは配列で返るので、その場合は何も出さない。
        assert!(!html.contains("artworks/manga"));

        let setting = pixiv_setting();
        let urls = setting.author_novel_urls(&html).expect("Pixiv lists works");
        assert_eq!(urls.len(), 7, "got {urls:?}");
        assert!(urls.iter().any(|url| url.ends_with("/ajax/series/329992")));
        assert_eq!(setting.author_comic_series_ids(&html), vec!["329992"]);
        let mut captures = HashMap::new();
        captures.insert("user_id".to_string(), "6519870".to_string());
        assert_eq!(
            setting.author_comic_series_pages_fetch_url(&captures, "329992", 2),
            Some("https://www.pixiv.net/ajax/series/329992?p=2&lang=ja".to_string())
        );
        assert_eq!(setting.author_series_ids(&html), vec!["272850", "551006"]);

    }

    #[test]
    fn pixiv_login_markers_distinguish_novel_pages_from_author_listings() {
        // 本文が無い小説ページ (R-18 / ログイン限定) は再試行の目印を出す。
        let novel = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{"id":"29204764","title":"作品","userName":"作者","description":"","content":null,"tags":{"tags":[]}}}"#,
        );
        assert!(novel.contains("login_required::1"), "got: {novel}");
        // 作者一覧の応答はログイン壁ではない: 作品を持たない作者 (空の一覧) が
        // 「ログインが必要」に化けると、空を成功として返せなくなる。
        let author = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{"novels":{},"novelSeries":[],"mangaSeries":[],"illusts":[],"manga":[]}}"#,
        );
        assert!(!author.contains("login_required::1"), "got: {author}");
        // 取得できない応答 (削除/非公開/ログイン要求) は目印を残す。
        let missing = run_pixiv_preprocess(r#"{"error":true,"message":"見つかりません"}"#);
        assert!(missing.contains("login_required::1"), "got: {missing}");
    }

    #[test]
    fn pixiv_targets_resolve_to_their_own_api_endpoint() {
        let setting = pixiv_setting();

        for (target, expected) in [
            (
                "https://www.pixiv.net/novel/show.php?id=29204764",
                "https://www.pixiv.net/ajax/novel/29204764",
            ),
            (
                "https://www.pixiv.net/novel/series/16299140",
                "https://www.pixiv.net/ajax/novel/series/16299140",
            ),
            (
                "https://www.pixiv.net/artworks/143868144",
                "https://www.pixiv.net/ajax/illust/143868144",
            ),
            (
                "https://www.pixiv.net/user/855548/series/205917?p=1#seriesContents",
                "https://www.pixiv.net/ajax/series/205917?p=1&lang=ja",
            ),
            // 保存済みレコードの toc_url (API 側) も同じ定義で解決できる。
            (
                "https://www.pixiv.net/ajax/illust/143868144",
                "https://www.pixiv.net/ajax/illust/143868144",
            ),
            (
                "https://www.pixiv.net/ajax/series/205917",
                "https://www.pixiv.net/ajax/series/205917?p=1&lang=ja",
            ),
        ] {
            assert_eq!(
                setting.toc_url_with_url_captures(target).as_deref(),
                Some(expected),
                "target: {target}"
            );
        }
    }

    #[test]
    fn pixiv_artwork_emits_short_story_metadata_and_page_images() {
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "id":"143868144","illustType":1,"title":"アイドル一騎打ち","illustTitle":"アイドル一騎打ち",
                "userName":"mini_La（ミニラ）","description":"漫画です","pageCount":2,
                "createDate":"2026-04-22T13:02:00+00:00","uploadDate":"2026-04-22T13:02:00+00:00",
                "seriesNavData":null,
                "urls":{"original":"https://i.pximg.net/img/143868144_p0.png"},
                "tags":{"tags":[{"tag":"漫画"}]}}}"#,
        );

        assert!(html.contains("ncode::a143868144"), "got: {html}");
        assert!(html.contains("title::アイドル一騎打ち"));
        assert!(html.contains("author::mini_La（ミニラ）"));
        assert!(html.contains("story::漫画です"));
        assert!(html.contains("novel_type::短編"));
        assert!(html.contains("tag::漫画"));
        // ページ画像は追加 API の結果を待つので、未解決のうちは代表画像に落ちる
        assert!(html.contains("<img src=\"https://i.pximg.net/img/143868144_p0.png\">"));
    }

    #[test]
    fn pixiv_ugoira_emits_the_frame_archive_with_its_delays() {
        // うごイラは静止画ではなくフレーム集約 zip と表示時間を渡す。
        let setting = pixiv_setting();
        let http = MockHttpClient::new();
        http.add_text(
            "https://www.pixiv.net/ajax/illust/69642452/ugoira_meta?lang=ja",
            200,
            r#"{"error":false,"body":{"originalSrc":"https://i.pximg.net/img-zip-ugoira/img/x/69642452_ugoira1920x1080.zip","frames":[{"file":"000000.jpg","delay":120},{"file":"000001.jpg","delay":80}]}}"#,
        );
        http.add_text(
            "https://www.pixiv.net/ajax/illust/69642452/pages",
            200,
            r#"{"error":false,"body":[{"urls":{"original":"https://i.pximg.net/img-original/img/x/69642452_ugoira0.png"}}]}"#,
        );
        let mut jobs = super::preprocess::PreprocessJobs::new();
        let mut source = r#"{"error":false,"message":"","body":{
            "id":"69642452","illustType":2,"title":"うごイラ作品","illustTitle":"うごイラ作品",
            "userName":"hamati","description":"","pageCount":1,
            "createDate":"2018-07-11T10:47:00+00:00","uploadDate":"2018-07-11T10:47:00+00:00",
            "seriesNavData":null,
            "urls":{"original":null},
            "tags":{"tags":[{"tag":"うごイラ"}]}}}"#
            .to_string();

        futures::executor::block_on(super::util::pretreatment_source_with_jobs(
            super::util::PretreatmentRequest {
                http: &http,
                rate_limiter: &FakeRateLimiter::new(),
                policy: &crate::downloader::http_policy::FetchPolicy::for_site(setting),
                src: &mut source,
                encoding: "UTF-8",
                setting: Some(setting),
                jobs: &mut jobs,
                url: "https://www.pixiv.net/ajax/illust/69642452",
            },
        ))
        .unwrap();

        let body = setting_body(&source);
        assert_eq!(
            body,
            "<img src=\"https://i.pximg.net/img-zip-ugoira/img/x/69642452_ugoira1920x1080.zip?ugoira=120,80\">",
            "ugoira should pass the archive and its frame delays"
        );
        assert_eq!(
            crate::illustration_animation::frame_delays("https://i.pximg.net/x.zip?ugoira=120,80"),
            Some(vec![120, 80])
        );
    }

    #[test]
    fn pixiv_plain_illustration_is_treated_as_an_artwork() {
        // illustType 0 (イラスト) でも作品として扱う。数値 0 は DSL では偽なので
        // 判定は illustTitle の有無で行う。
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "id":"141939696","illustType":0,"title":"花火vs火花（仮）","illustTitle":"花火vs火花（仮）",
                "userName":"猫目ミト","description":"","pageCount":1,
                "createDate":"2026-03-01T00:00:00+00:00","uploadDate":"2026-03-01T00:00:00+00:00",
                "seriesNavData":null,
                "urls":{"original":"https://i.pximg.net/img/141939696_p0.jpg"},
                "tags":{"tags":[{"tag":"イラスト"}]}}}"#,
        );

        assert!(html.contains("ncode::a141939696"), "got: {html}");
        assert!(html.contains("novel_type::短編"));
        assert!(html.contains("<img src=\"https://i.pximg.net/img/141939696_p0.jpg\">"));
        assert!(
            !html.contains("login_required::1"),
            "an illustration must not be read as a login wall: {html}"
        );
    }

    #[test]
    fn pixiv_artwork_pages_resolve_into_image_tags() {
        let setting = pixiv_setting();
        let http = MockHttpClient::new();
        http.add_text(
            "https://www.pixiv.net/ajax/illust/143868144/pages",
            200,
            r#"{"error":false,"body":[{"urls":{"original":"https://i.pximg.net/img/143868144_p0.png"}},{"urls":{"original":"https://i.pximg.net/img/143868144_p1.png"}}]}"#,
        );
        let mut jobs = super::preprocess::PreprocessJobs::new();
        let mut source = r#"{"error":false,"message":"","body":{
            "id":"143868144","illustType":1,"title":"アイドル一騎打ち","illustTitle":"アイドル一騎打ち",
            "userName":"mini_La（ミニラ）","description":"漫画です","pageCount":2,
            "createDate":"2026-04-22T13:02:00+00:00","uploadDate":"2026-04-22T13:02:00+00:00",
            "seriesNavData":null,
            "urls":{"original":"https://i.pximg.net/img/143868144_p0.png"},
            "tags":{"tags":[{"tag":"漫画"}]}}}"#
            .to_string();

        futures::executor::block_on(super::util::pretreatment_source_with_jobs(
            super::util::PretreatmentRequest {
                http: &http,
                rate_limiter: &FakeRateLimiter::new(),
                policy: &crate::downloader::http_policy::FetchPolicy::for_site(setting),
                src: &mut source,
                encoding: "UTF-8",
                setting: Some(setting),
                jobs: &mut jobs,
                url: "https://www.pixiv.net/ajax/illust/143868144",
            },
        ))
        .unwrap();

        let body = setting_body(&source);
        assert_eq!(
            body,
            "<img src=\"https://i.pximg.net/img/143868144_p0.png\"><br><img src=\"https://i.pximg.net/img/143868144_p1.png\">"
        );
    }

    #[test]
    fn pixiv_manga_series_emits_ascending_episodes_and_pages_on() {
        let setting = pixiv_setting();
        let http = MockHttpClient::new();
        for (id, title) in [("139115096", "第19話"), ("139115097", "第20話")] {
            http.add_text(
                format!("https://www.pixiv.net/ajax/illust/{id}"),
                200,
                format!(
                    r#"{{"error":false,"body":{{"id":"{id}","title":"{title}","userName":"作者名","uploadDate":"2026-01-02T03:04:05+09:00","urls":{{"original":"https://i.pximg.net/{id}.png"}},"tags":{{"tags":[{{"tag":"共通タグ"}},{{"tag":"タグ{id}"}}]}}}}}}"#
                ),
            );
        }
        let mut jobs = super::preprocess::PreprocessJobs::new();
        // API は order の降順で返す。
        let mut source = r#"{"error":false,"message":"","body":{
            "illustSeries":[{"id":"205917","title":"靈夢と訓練する","description":"シリーズの説明",
                             "total":21,"createDate":"2025-01-01T00:00:00+09:00","updateDate":"2026-01-02T00:00:00+09:00"}],
            "page":{"seriesId":"205917","total":3,
                    "series":[{"workId":"139115097","order":20},{"workId":"139115096","order":19}]}}}"#
            .to_string();

        futures::executor::block_on(super::util::pretreatment_source_with_jobs(
            super::util::PretreatmentRequest {
                http: &http,
                rate_limiter: &FakeRateLimiter::new(),
                policy: &crate::downloader::http_policy::FetchPolicy::for_site(setting),
                src: &mut source,
                encoding: "UTF-8",
                setting: Some(setting),
                jobs: &mut jobs,
                url: "https://www.pixiv.net/ajax/series/205917?p=1&lang=ja",
            },
        ))
        .unwrap();

        assert!(source.contains("ncode::c205917"), "got: {source}");
        assert!(source.contains("title::靈夢と訓練する"));
        assert!(source.contains("story::シリーズの説明"));
        assert!(source.contains("novel_type::連載中"));
        // 公開話数はシリーズ情報の total
        assert!(source.contains("general_all_no::21"));
        // 作者は各話ページから拾う
        assert!(source.contains("author::作者名"), "got: {source}");
        // 漫画シリーズでも各話の作品 API のタグを統合する (重複除去はタグ収集側)。
        assert!(source.contains("tag::共通タグ"), "got: {source}");
        assert!(source.contains("tag::タグ139115096"), "got: {source}");
        assert!(source.contains("tag::タグ139115097"), "got: {source}");
        // 目次は order の昇順で出る
        let first = source
            .find("Episode;19;https://www.pixiv.net/ajax/illust/139115096")
            .expect("order 19 entry");
        let second = source
            .find("Episode;20;https://www.pixiv.net/ajax/illust/139115097")
            .expect("order 20 entry");
        assert!(first < second, "episodes must be ascending: {source}");
        assert!(
            source.contains(";第19話"),
            "titles come from each work: {source}"
        );
        // order 1 に到達していないので次ページを要求する
        assert!(
            source.contains("next::/ajax/series/205917?p=2&lang=ja"),
            "got: {source}"
        );

        let mut url_captures = HashMap::new();
        url_captures.insert("series_id".to_string(), "205917".to_string());
        let subtitles = super::toc::parse_subtitles(setting, &source, &url_captures).unwrap();
        assert_eq!(subtitles.len(), 2);
        assert_eq!(subtitles[0].index, "19");
        assert_eq!(subtitles[0].subtitle, "第19話");
        assert_eq!(subtitles[1].index, "20");
    }

    #[test]
    fn pixiv_manga_series_stops_paging_at_order_one() {
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "illustSeries":[{"id":"311834","title":"もここのグルメ","description":"説明","total":5,
                                 "createDate":"2025-11-18T11:48:54+09:00","updateDate":"2025-12-11T07:54:14+09:00"}],
                "page":{"seriesId":"311834","total":1,"series":[{"workId":"138467437","order":1}]}}}"#,
        );
        assert!(html.contains("ncode::c311834"));
        assert!(!html.contains("next::"), "got: {html}");
    }

    #[test]
    fn pixiv_series_detail_emits_info_and_first_content_page() {
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "id":"16299140","title":"トゥルースヒーローズ設定集","userId":"92017217","userName":"ゆっきー593",
                "caption":"設定をまとめます","total":6,"isConcluded":false,
                "publishedTotalCharacterCount":80418,
                "tags":["トゥルースヒーローズ","設定"],
                "createDate":"2026-08-03T22:26:01+09:00","updateDate":"2026-09-23T17:04:49+09:00"}}"#,
        );

        assert!(html.contains("PixivPreprocessEvalMagicWord"));
        assert!(html.contains("title::トゥルースヒーローズ設定集"));
        assert!(html.contains("author::ゆっきー593"));
        assert!(html.contains("story::設定をまとめます"));
        assert!(html.contains("novel_type::連載中"));
        assert!(html.contains("general_all_no::6"));
        assert!(html.contains("general_firstup::2026-08-03T22:26:01+09:00"));
        assert!(html.contains("general_lastup::2026-09-23T17:04:49+09:00"));
        assert!(html.contains("length::80418"));
        assert!(html.contains("tag::トゥルースヒーローズ"));
        assert!(html.contains(
            "next::/ajax/novel/series_content/16299140?limit=30&last_order=0&order_by=asc"
        ));

        let setting = pixiv_setting();
        let next = setting
            .next_toc_pattern()
            .unwrap()
            .captures(&html)
            .and_then(|caps| caps.name("next_page").map(|m| m.as_str().to_string()))
            .unwrap();
        assert_eq!(
            next,
            "/ajax/novel/series_content/16299140?limit=30&last_order=0&order_by=asc"
        );
        assert_eq!(
            setting.get_next_url_with_captures(setting.next_url.as_deref().unwrap(), &{
                let mut captures = HashMap::new();
                captures.insert("next_page".to_string(), next);
                captures
            }),
            "https://www.pixiv.net/ajax/novel/series_content/16299140?limit=30&last_order=0&order_by=asc"
        );
    }


    #[test]
    fn pixiv_partial_series_content_page_stops_pagination() {
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "thumbnails":{"novel":[{"id":"28776498","title":"設定集","seriesContentOrder":1,"seriesId":"16299140","createDate":"2026-08-04T22:49:29+09:00"}]},
                "page":{"seriesContents":[{"id":"28776498","series":{"id":16299140,"contentOrder":1}}]}}}"#,
        );

        assert!(html.contains("Episode;1;https://www.pixiv.net/ajax/novel/28776498"));
        assert!(!html.contains("next::"));
    }

    #[test]
    fn pixiv_single_novel_emits_metadata_and_converts_body_markup() {
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "id":"29204764","title":"短編集","userName":"沼月時雨","description":"あらすじです",
                "content":"1行目\n2行目\n[[rb:漢字 > かんじ]]\n[chapter:第二章]\n[newpage]\n[[jumpuri:作者サイト > https://example.com/]]\n[pixivimage:83791147-1]\n[uploadedimage:22638629]\n[jump:2]",
                "createDate":"2026-09-23T07:48:07+00:00","uploadDate":"2026-09-23T07:48:07+00:00",
                "characterCount":1274,"seriesNavData":null,
                "tags":{"tags":[{"tag":"タグA"},{"tag":"タグB"}]}}}"#,
        );

        assert!(html.contains("ncode::n29204764"));
        assert!(html.contains("title::短編集"));
        assert!(html.contains("author::沼月時雨"));
        assert!(html.contains("story::あらすじです"));
        assert!(html.contains("novel_type::短編"));
        assert!(html.contains("general_all_no::1"));
        assert!(html.contains("general_lastup::2026-09-23T07:48:07+00:00"));
        assert!(html.contains("length::1274"));
        assert!(html.contains("tag::タグA"));
        assert!(html.contains("tag::タグB"));
        // 短編は目次を出さない (href 付きの目次があると連載中と判定される)
        assert!(!html.contains("Episode;"), "got: {html}");
        assert!(!html.contains("next::"));

        // 短編として扱われ、本文ページはこの URL 自身になる
        let setting = pixiv_setting();
        let mut url_captures = HashMap::new();
        url_captures.insert("work_id".to_string(), "29204764".to_string());
        let (novel_type, _) =
            futures::executor::block_on(super::resolve_novel_type(setting, &html, &url_captures, &pixiv_info(&html)));
        assert_eq!(novel_type, 2);

        // 本文は Aozora へ渡せる HTML へ変換される (素の改行は to_aozora が落とす)
        let body = setting_body(&html);
        assert_eq!(
            body,
            "1行目<br>2行目<br><ruby>漢字<rp>(</rp><rt>かんじ</rt><rp>)</rp></ruby><br>第二章<br>［＃改ページ］<br><a href=\"https://example.com/\">作者サイト</a><br><!--pixivimage:83791147-1--><br><!--uploadedimage:22638629--><br>（2ページ目へ）"
        );
        assert!(!body.contains("[pixivimage"), "raw tag leaked: {body}");
        // 単体作品の description はあらすじなので前書きには回さない
        assert!(!html.contains("introduction::"));

        let aozora = super::html::to_aozora(&body);
        assert!(aozora.contains("｜漢字《かんじ》"));
        assert!(aozora.contains("［＃改ページ］"));
        // narou.rb と同じく <a> は落としてリンク文字列だけ残す
        assert!(aozora.contains("作者サイト"));
        assert!(!aozora.contains("<a href"));
        // 挿絵・ジャンプは本文を汚さない形にする
        assert!(aozora.contains("（2ページ目へ）"), "got: {aozora}");
        assert!(!aozora.contains("pixivimage"));
        assert!(!aozora.contains("uploadedimage"));
    }

    #[test]
    fn pixiv_series_episode_emits_introduction_and_poll_postscript() {
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "id":"29205030","title":"純血とマグル","userName":"ラギー","description":"前書き<br />二行目",
                "content":"本文です","createDate":"2026-09-23T08:20:09+00:00","uploadDate":"2026-09-23T08:20:09+00:00",
                "characterCount":4444,
                "seriesNavData":{"seriesId":14889031,"title":"魔法学園ヘタリア","order":2},
                "pollData":{"question":"どれ？","total":10,"choices":[{"text":"A","count":6},{"text":"B","count":4}]},
                "tags":{"tags":[{"tag":"夢小説"}]}}}"#,
        );

        // シリーズの各話でもレコードは話そのもの
        assert!(html.contains("ncode::n29205030"), "got: {html}");
        assert!(html.contains("introduction::"));
        assert!(html.contains("前書き<br />二行目"));
        assert!(html.contains("::introduction_end"));
        // 同じ文を あらすじ と 前書き の両方には出さない
        assert!(!html.contains("story::前書き"), "got: {html}");
        assert!(html.contains("postscript::"));
        assert!(
            html.contains("アンケート　\"どれ？\"　　票数10票<br>　　　A　　6票<br>　　　B　　4票")
        );
        assert!(html.contains("::postscript_end"));
        assert_eq!(setting_body(&html), "本文です");
    }

    #[test]
    fn pixiv_illustration_references_resolve_through_the_job_queue() {
        // 挿絵 URL は同じ応答に無いので、DSL が追加 API を要求し、実行側が
        // 取得して再実行した結果として <img> に変わる。
        let setting = pixiv_setting();
        let http = MockHttpClient::new();
        http.add_text(
            "https://www.pixiv.net/ajax/illust/83791147/pages",
            200,
            r#"{"error":false,"body":[{"urls":{"original":"https://i.pximg.net/img/83791147_p0.jpg"}}]}"#,
        );
        let mut jobs = super::preprocess::PreprocessJobs::new();
        let mut source = r#"{"error":false,"message":"","body":{
            "id":"29204764","title":"短編集","userName":"沼月時雨","description":"あらすじ",
            "content":"前\n[pixivimage:83791147-1]\n[uploadedimage:22638629]\n後",
            "createDate":"2026-09-23T07:48:07+00:00","uploadDate":"2026-09-23T07:48:07+00:00",
            "characterCount":10,"seriesNavData":null,
            "textEmbeddedImages":{"22638629":{"urls":{"original":"https://i.pximg.net/img/uploaded.png"}}},
            "tags":{"tags":[{"tag":"タグA"}]}}}"#
            .to_string();

        futures::executor::block_on(super::util::pretreatment_source_with_jobs(
            super::util::PretreatmentRequest {
                http: &http,
                rate_limiter: &FakeRateLimiter::new(),
                policy: &crate::downloader::http_policy::FetchPolicy::for_site(setting),
                src: &mut source,
                encoding: "UTF-8",
                setting: Some(setting),
                jobs: &mut jobs,
                url: "https://www.pixiv.net/ajax/novel/29204764",
            },
        ))
        .unwrap();

        assert_eq!(
            http.requested_urls(),
            vec!["GET https://www.pixiv.net/ajax/illust/83791147/pages"],
            "the definition should ask for the illustration pages once"
        );
        assert_eq!(jobs.len(), 1);
        let body = setting_body(&source);
        assert!(
            body.contains(r#"<img src="https://i.pximg.net/img/83791147_p0.jpg">"#),
            "got: {body}"
        );
        // 同じ応答にある挿絵は追加取得なしで解決する
        assert!(
            body.contains(r#"<img src="https://i.pximg.net/img/uploaded.png">"#),
            "got: {body}"
        );
        assert!(!body.contains("pixivimage"), "got: {body}");
    }

    #[test]
    fn pixiv_unresolved_illustration_keeps_only_a_marker() {
        // 取得に失敗しても本文は落とさず、目印だけ残す。
        let setting = pixiv_setting();
        let http = MockHttpClient::new();
        let mut jobs = super::preprocess::PreprocessJobs::new();
        let mut source = r#"{"error":false,"message":"","body":{
            "id":"29204764","title":"短編集","userName":"沼月時雨","description":"あらすじ",
            "content":"前\n[pixivimage:83791147-1]\n後",
            "createDate":"2026-09-23T07:48:07+00:00","uploadDate":"2026-09-23T07:48:07+00:00",
            "characterCount":10,"seriesNavData":null,"tags":{"tags":[{"tag":"タグA"}]}}}"#
            .to_string();

        futures::executor::block_on(super::util::pretreatment_source_with_jobs(
            super::util::PretreatmentRequest {
                http: &http,
                rate_limiter: &FakeRateLimiter::new(),
                policy: &crate::downloader::http_policy::FetchPolicy::for_site(setting),
                src: &mut source,
                encoding: "UTF-8",
                setting: Some(setting),
                jobs: &mut jobs,
                url: "https://www.pixiv.net/ajax/novel/29204764",
            },
        ))
        .unwrap();

        let body = setting_body(&source);
        assert_eq!(body, "前<br><!--pixivimage:83791147-1--><br>後");
        // 失敗したジョブは null として残り、無駄に再取得しない
        assert_eq!(jobs.len(), 1);
    }

    #[test]
    fn pixiv_error_envelope_is_reported_as_missing_novel() {
        let html = run_pixiv_preprocess(r#"{"error":true,"message":"作品がありません","body":[]}"#);

        let setting = pixiv_setting();
        let pattern = setting.compiled_error_message_pattern().unwrap();
        assert!(
            pattern.is_match(&html),
            "error envelope should match error_message: {html}"
        );
    }

    #[test]
    fn pixiv_login_only_novel_asks_for_a_stored_cookie() {
        // ログイン限定作品は HTTP 200 のまま content だけが欠ける。
        let html = run_pixiv_preprocess(
            r#"{"error":false,"message":"","body":{
                "id":"26352975","title":"技術テスト","userName":"るみゃ","description":"技術テストです",
                "content":null,"isLoginOnly":true,
                "createDate":"2025-11-02T03:18:05+00:00","uploadDate":"2025-11-02T03:18:05+00:00",
                "characterCount":36,"seriesNavData":null,
                "tags":{"tags":[{"tag":"技術畑"}]}}}"#,
        );

        assert!(html.contains("login_required::1"), "got: {html}");
        assert!(!html.contains("body::"));
        assert!(!html.contains("title::技術テスト"));

        let setting = pixiv_setting();
        assert!(
            setting
                .compiled_error_message_pattern()
                .unwrap()
                .is_match(&html),
            "login wall should also read as missing so the download fails without a cookie"
        );
        assert!(
            setting.compiled_login_pattern().unwrap().is_match(&html),
            "login wall should match login_pattern so the stored cookie is retried"
        );
        // 本文が無いので本文パターンは一致しない
        let (element, _) = super::section::parse_section_html(setting, html.clone()).unwrap();
        assert_eq!(element.body, "");
    }

    fn pixiv_info(html: &str) -> super::novel_info::NovelInfo {
        futures::executor::block_on(super::novel_info::NovelInfo::from_novel_info_source(pixiv_setting(), html))
    }

    fn setting_body(html: &str) -> String {
        let setting = pixiv_setting();
        let (element, _) = super::section::parse_section_html(setting, html.to_string()).unwrap();
        element.body
    }

    #[test]
    fn r18_narou_sitename_pattern_is_moved_to_sitename_pattern_field() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "novel18.syosetu.com")
            .unwrap();

        assert!(
            !setting.sitename.contains("(?<"),
            "sitename should be a plain display name after compile, got: {}",
            setting.sitename
        );
        assert!(
            setting.sitename_pattern.is_some(),
            "sitename_pattern should be populated for R18 narou"
        );
        assert_eq!(setting.sitename, "小説家になろうR18");
    }

    #[test]
    fn bundled_japanese_site_definitions_set_jst_timezone() {
        let settings = SiteSetting::load_all().unwrap();
        for domain in [
            "ncode.syosetu.com",
            "novel18.syosetu.com",
            "syosetu.org",
            "www.akatsuki-novels.com",
            "www.mai-net.net",
        ] {
            let setting = settings.iter().find(|s| s.domain == domain).unwrap();
            assert_eq!(setting.timezone.as_deref(), Some("Asia/Tokyo"));
        }
    }

    #[test]
    fn r18_narou_extracts_sitename_from_info_html() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "novel18.syosetu.com")
            .unwrap();

        assert!(setting.sitename_pattern.is_some());

        let html = "<h1 class=\"p-infotop-title\">\n<a href=\"/n7534il/\">テスト小説タイトル</a>\n</h1>\n<dt class=\"p-infotop-data__title\">掲載サイト</dt>\n<dd class=\"p-infotop-data__value\">ノクターンノベルズ(夜の恋愛)</dd>\n<dt class=\"p-infotop-data__title\">作者名</dt>\n<dd class=\"p-infotop-data__value\"><a href=\"/mypage/top/view/id/12345/\">テスト作者</a></dd>";

        let info = futures::executor::block_on(NovelInfo::from_novel_info_source(setting, html));

        assert_eq!(info.title.as_deref(), Some("テスト小説タイトル"));
        assert_eq!(info.author.as_deref(), Some("テスト作者"));
        assert_eq!(info.sitename.as_deref(), Some("ノクターンノベルズ"));
    }

    #[test]
    fn syosetu_org_info_patterns_extract_title_and_author() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let html = r#"
<tr><td class="label" width="13%">タイトル</td><td ><a href=https://syosetu.org/novel/232822/>和風ファンタジーな鬱エロゲーの名無し戦闘員に転生したんだが周囲の女がヤベー奴ばかりで嫌な予感しかしない件</a></td><td class="label" width="10%">小説ID</td><td width="20%">232822</td></tr>
<tr><td class="label">原作</td><td>ファンタジー</td><td class="label">作者</td><td ><a href=https://syosetu.org/user/214537/>鉄鋼怪人</a></td></tr>
<tr><td class="label">話数</td><td >連載(連載中) 251話</td></tr>
<tr><td class="label">掲載開始</td><td width="26%">2020年08月01日(土) 00:33</td><td class="label">話数</td><td width="20%">連載(連載中) 251話</td></tr>
<tr><td class="label">最新投稿</td><td>2026年04月17日(金) 07:00</td><td class="label">総文字数</td><td>3,666,651文字</td></tr>
"#;

        let info = futures::executor::block_on(NovelInfo::from_novel_info_source(setting, html));

        assert_eq!(
            info.title.as_deref(),
            Some(
                "和風ファンタジーな鬱エロゲーの名無し戦闘員に転生したんだが周囲の女がヤベー奴ばかりで嫌な予感しかしない件"
            )
        );
        assert_eq!(info.author.as_deref(), Some("鉄鋼怪人"));
        assert_eq!(info.novel_type, Some(1));
        assert_eq!(
            info.general_firstup.map(|dt| dt
                .with_timezone(&chrono_tz::Asia::Tokyo)
                .format("%Y-%m-%d %H:%M")
                .to_string()),
            Some("2020-08-01 00:33".to_string())
        );
        assert_eq!(
            info.general_lastup.map(|dt| dt
                .with_timezone(&chrono_tz::Asia::Tokyo)
                .format("%Y-%m-%d %H:%M")
                .to_string()),
            Some("2026-04-17 07:00".to_string())
        );
    }

    #[test]
    fn syosetu_org_tag_pattern_extracts_all_tags() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let html = r#"
<tr><td class="label">タグ</td><td colspan=3 ><a href="https://syosetu.org/search/?mode=search&word=和風ファンタジー">和風ファンタジー</a> <a href="https://syosetu.org/search/?mode=search&word=妖">妖</a> <a href="https://syosetu.org/search/?mode=search&word=ヤンデレ">ヤンデレ</a> <a href="https://syosetu.org/search/?mode=search&word=闇夜の蛍">闇夜の蛍</a> </td></tr>
<tr><td class="label">必須タグ</td><td colspan=3 ><a href="https://syosetu.org/search/?mode=search&word=残酷な描写" class="alert_color">残酷な描写</a> </td></tr>
<tr><td class="label">掲載開始</td><td>2020年08月01日(土) 00:33</td></tr>
"#;

        let tags = futures::executor::block_on(setting.resolve_info_pattern("tags", html)).unwrap();

        assert_eq!(
            super::sanitize_site_tags(&tags),
            vec![
                "和風ファンタジー",
                "妖",
                "ヤンデレ",
                "闇夜の蛍",
                "残酷な描写"
            ]
        );
    }

    #[test]
    fn syosetu_org_toc_tag_pattern_supports_series_and_stops_before_short_story_body() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let short_story_html = r#"
<br>タグ：<span itemprop="keywords"><a href="/search/?word=tag1">タグ1</a></span> <span itemprop="keywords"><a href="/search/?word=tag2">タグ2</a></span>
<hr style="margin:20px 0px;"></div>
<div class="ss">本文に含めてはいけないあらすじ<hr style="margin:20px 0px;">
<div id="honbun">本文に含めてはいけない本文</div>
<br>
"#;
        let series_html = r##"
<BR>タグ：<a href="/search/?word=R-15" class="alert_color">R-15</a> <span itemprop="keywords"><a href="/search/?word=tag3">タグ3</a></span>
<br /><a href="#fmenu">▼下部メニューに飛ぶ</a>
<hr style="margin:20px 0px;"></div>
<div class="ss">本文に含めてはいけないあらすじ</div>
"##;

        let short_story_tags = futures::executor::block_on(setting.resolve_info_pattern("tags", short_story_html)).unwrap();
        let series_tags = futures::executor::block_on(setting.resolve_info_pattern("tags", series_html)).unwrap();

        assert_eq!(
            super::sanitize_site_tags(&short_story_tags),
            vec!["タグ1", "タグ2"]
        );
        assert_eq!(
            super::sanitize_site_tags(&series_tags),
            vec!["R-15", "タグ3"]
        );
    }

    #[test]
    fn syosetu_org_accepts_h_domain_and_keeps_domain_in_toc_url() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();

        // R18 は h.syosetu.org に分離されたが同一サイト扱い(単一定義):
        // どちらのドメインの URL も受け付け、toc_url は実際に取得可能な
        // ドメインを保持する(フェッチ層はリダイレクトを追わないため)。
        assert!(setting.matches_url("https://h.syosetu.org/novel/412369/"));
        assert!(setting.matches_url("https://syosetu.org/novel/405366/"));
        assert!(!setting.matches_url("https://hsyosetu.org/novel/1/"));

        assert_eq!(
            setting
                .toc_url_with_url_captures("https://h.syosetu.org/novel/412369/")
                .as_deref(),
            Some("https://h.syosetu.org/novel/412369/")
        );
        assert_eq!(
            setting
                .toc_url_with_url_captures("https://syosetu.org/novel/405366/")
                .as_deref(),
            Some("https://syosetu.org/novel/405366/")
        );
        // ドメインをキャプチャしない旧 novel.syosetu.org 形式はフィールド既定値に落ちる
        assert_eq!(
            setting
                .toc_url_with_url_captures("https://novel.syosetu.org/12345")
                .as_deref(),
            Some("https://syosetu.org/novel/12345/")
        );
    }

    #[test]
    fn syosetu_org_r18_marker_uses_global_over18_setting() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let html = r#"
<div class="ss">
<strong><span class="alert_color">※この作品はR-18です。</span></strong><BR>
</div>
"#;

        assert!(requires_over18_confirmation(setting, html));
        assert_eq!(
            over18_access_decision(setting, html, None),
            Over18AccessDecision::Prompt
        );
        assert_eq!(
            over18_access_decision(setting, html, Some(true)),
            Over18AccessDecision::Allow
        );
        assert_eq!(
            over18_access_decision(setting, html, Some(false)),
            Over18AccessDecision::Deny
        );
    }

    #[test]
    fn syosetu_org_r18_toc_extracts_title_and_author() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let html = r#"
<div class="ss">
<strong><span class="alert_color">※この作品はR-18です。</span></strong><BR>
<span style="font-size:150%" itemprop="name">一次創作キャットファイトとかレズバトルもの</span>
<div align="right">作者：<span itemprop="author"><a href="//syosetu.org/user/178289/">就活をするゴミ箱の魂</a></span></div>
</div>
"#;

        let info = futures::executor::block_on(NovelInfo::from_toc_source(setting, html));

        assert_eq!(
            info.title.as_deref(),
            Some("一次創作キャットファイトとかレズバトルもの")
        );
        assert_eq!(info.author.as_deref(), Some("就活をするゴミ箱の魂"));
    }

    #[test]
    fn syosetu_org_multi_episode_toc_overrides_short_story_type() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let toc_source = r##"
<div class="ss">
<span style="font-size:150%" itemprop="name">一次創作キャットファイトとかレズバトルもの</span>
<section class="episode-list" aria-label="話一覧">
<ul class="episode-list__items">
<li class="episode-list__item">
<a href="./1.html" class="episode-list__link">
<span class="episode-list__mark"></span>
<span class="episode-list__title" >ソープランドがレズバトルで抗争するようです</span>
<time class="episode-list__date">2020/05/31 20:38</time>
<span class="episode-list__revision"></span>
</a>
</li>
<li class="episode-list__item">
<a href="./2.html" class="episode-list__link">
<span class="episode-list__mark"></span>
<span class="episode-list__title" >北の王女と西の王女</span>
<time class="episode-list__date">2021/02/21 09:31</time>
<span class="episode-list__revision"></span>
</a>
</li>
</ul>
</section>
</div>
"##;
        let info = NovelInfo {
            title: Some("一次創作キャットファイトとかレズバトルもの".to_string()),
            author: None,
            story: None,
            novel_type: Some(2),
            end: Some(false),
            general_firstup: None,
            general_lastup: None,
            novelupdated_at: None,
            length: None,
            tags: None,
            sitename: None,
            raw_captures: HashMap::new(),
        };

        let (novel_type, is_end) = futures::executor::block_on(resolve_novel_type(setting, toc_source, &HashMap::new(), &info));

        assert_eq!(novel_type, 1);
        assert!(!is_end);
    }

    #[test]
    fn short_story_type_is_preserved_when_toc_has_no_episode_list() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let toc_source = r#"
<div class="ss">
<span style="font-size:150%" itemprop="name">短編タイトル</span>
</div>
"#;
        let info = NovelInfo {
            title: Some("短編タイトル".to_string()),
            author: None,
            story: None,
            novel_type: Some(2),
            end: Some(false),
            general_firstup: None,
            general_lastup: None,
            novelupdated_at: None,
            length: None,
            tags: None,
            sitename: None,
            raw_captures: HashMap::new(),
        };

        let (novel_type, is_end) = futures::executor::block_on(resolve_novel_type(setting, toc_source, &HashMap::new(), &info));

        assert_eq!(novel_type, 2);
        assert!(!is_end);
    }

    #[test]
    fn non_r18_toc_does_not_require_over18_setting() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "ハーメルン").unwrap();
        let html = r#"
<div class="ss">
<span style="font-size:150%" itemprop="name">全年齢作品</span>
</div>
"#;

        assert!(!requires_over18_confirmation(setting, html));
        assert_eq!(
            over18_access_decision(setting, html, Some(false)),
            Over18AccessDecision::Allow
        );
    }

    #[test]
    fn arcadia_toc_patterns_extract_title_and_author_from_legacy_yaml_keys() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "Arcadia").unwrap();
        let html = r#"
<html>
  <body>
    <font size=4 color=4444aa>異世界に来たけど至って普通に喫茶店とかやってますが何か問題でも？</font>
    <tt>Name: 風見鶏</tt>
  </body>
</html>
"#;

        let info = futures::executor::block_on(NovelInfo::from_toc_source(setting, html));

        assert_eq!(
            info.title.as_deref(),
            Some("異世界に来たけど至って普通に喫茶店とかやってますが何か問題でも？")
        );
        assert_eq!(info.author.as_deref(), Some("風見鶏"));
    }

    #[test]
    fn section_hash_cache_store_and_clear_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::new().unwrap();
        let mut db_slot = db::DATABASE.lock();
        let previous_db = db_slot.take();
        *db_slot = Some(db);
        drop(db_slot);
        let _db_guard = DatabaseGuard(previous_db);

        let mut downloader = Downloader::with_user_agent(None).unwrap();
        downloader.store_section_digest(42, "本文\\1 test.yaml", "digest-1");

        assert_eq!(
            downloader.cached_section_digest(42, "本文\\1 test.yaml"),
            Some("digest-1")
        );

        downloader.clear_section_digest(42, "本文\\1 test.yaml");

        assert_eq!(
            downloader.cached_section_digest(42, "本文\\1 test.yaml"),
            None
        );
    }
    #[tokio::test]
    async fn replacement_during_toc_fetch_rejects_old_downloader_and_preserves_bytes() {
        use crate::platform::{HttpClient, HttpRequest, HttpStreamResponse, NovelAccessFence};
        use std::sync::atomic::{AtomicBool, Ordering};
        struct OriginBudget(Arc<parking_lot::Mutex<Vec<AccessOrigin>>>);
        impl SectionBudget for OriginBudget {
            fn set_access_origin<'a>(&'a mut self, origin: &'a AccessOrigin, _creation_reservation: Option<(crate::platform::NovelId, i64)>) -> PlatformFuture<'a, Result<bool>> {
                self.0.lock().push(origin.clone());
                Box::pin(async { Ok(false) })
            }
            fn should_yield(&mut self, _: usize) -> bool { false }
        }
        struct ReplacingHttp {
            http: MockHttpClient,
            store: Arc<MemoryObjectStore>,
            repository: Arc<MemoryNovelRepository>,
            record: crate::db::NovelRecord,
            keys: NovelObjectKeys,
            swapped: AtomicBool,
            origins: Arc<parking_lot::Mutex<Vec<AccessOrigin>>>,
        }
        impl ReplacingHttp {
            async fn replace_once(&self) -> Result<()> {
                if self.swapped.swap(true, Ordering::SeqCst) { return Ok(()); }
                if self.repository.get_current(self.record.id.into()).await?.is_some() && self.origins.lock().is_empty() {
                    return Err(NarouError::Platform("download origin was not checkpointed before HTTP".into()));
                }
                let prefix = ObjectPrefix::from(self.keys.prefix().clone());
                let epoch = self.store.close_gate(&prefix, "replacement").await?;
                self.store.open_gate(&prefix, "replacement", epoch).await?;
                let fresh = self.store.capture_writer(&prefix).await?;
                let mut record = self.record.clone();
                let reservation = if self.repository.get_current(record.id.into()).await?.is_none() {
                    Some(self.repository.allocate_id().await?)
                } else { None };
                if let Some(reservation) = &reservation {
                    record.id = reservation.id().0;
                    self.repository.reserve_create(&fresh, reservation, &record.toc_url).await?;
                }
                record.title = "Replacement".into();
                self.store.write_small_guarded(&fresh, &self.keys.toc(), b"title: Replacement\nauthor: Author\ntoc_url: https://example.com/n1234ab/\nstory: null\nsubtitles: []\nnovel_type: 1\n".to_vec()).await?;
                self.store.write_small_guarded(&fresh, &self.keys.section("1", "episode"), b"replacement section bytes".to_vec()).await?;
                self.store.write_small_guarded(&fresh, &self.keys.converted_text(), b"replacement output".to_vec()).await?;
                if let Some(reservation) = reservation {
                    self.repository.apply_create_guarded(&fresh, &reservation, record).await?;
                    reservation.adopt();
                    Ok(())
                } else {
                    self.repository.apply_batch_guarded(&fresh, vec![NovelMutation::Upsert(record)]).await
                }
            }
        }
        impl HttpClient for ReplacingHttp {
            fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
                Box::pin(async move { self.replace_once().await?; self.http.send(request).await })
            }
            fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpStreamResponse>> {
                Box::pin(async move { self.replace_once().await?; self.http.send_stream(request).await })
            }
            fn validate_url<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<()>> { self.http.validate_url(url) }
        }
        let mut setting: SiteSetting = serde_yaml::from_str(r#"
name: Fence Test
domain: example.com
top_url: https://example.com
url: 'https://example\.com/(?<ncode>n1234ab)/?'
sitename: Fence Test
toc_url: 'https://example.com/\k<ncode>/'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
subtitles: '<a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
"#).unwrap();
        setting.compile();
        let record: crate::db::NovelRecord = serde_yaml::from_str("id: 1\nauthor: Author\ntitle: Original\nfile_title: n1234ab\ntoc_url: https://example.com/n1234ab/\nsitename: Fence Test\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        for supplied_origin in [false, true] {
            let store = Arc::new(MemoryObjectStore::new());
            let repository = Arc::new(MemoryNovelRepository::from_records(vec![record.clone()]).with_objects(store.clone()));
            let keys = NovelObjectKeys::new("Fence Test", "n1234ab", false).unwrap();
            let http = MockHttpClient::new();
            http.add_text("https://example.com/n1234ab/", 200, "<h1>Original</h1><span>Author</span><a href=\"/n1234ab/1/\">episode</a>");
            http.add_text("https://example.com/n1234ab/1/", 200, "<div class=\"body\">old fetched body</div>");
            let origins = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let http = Arc::new(ReplacingHttp { http, store: store.clone(), repository: repository.clone(), record: record.clone(), keys: keys.clone(), swapped: AtomicBool::new(false), origins: origins.clone() });
            let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
                DownloaderPlatform {
                    http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()), novels: repository.clone(),
                    objects: store.clone(), assets: store.clone(), clock: Arc::new(SystemClock),
                }, vec![setting.clone()], HashMap::new(), Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
            ).unwrap();
            let access = if supplied_origin { Some(downloader.capture_write_for_record(&record).await.unwrap()) } else { None };
            let mut budget = OriginBudget(origins.clone());
            let error = downloader.download_novel_with_execution_options("1", DownloadExecutionOptions {
                force: false, budget: Some(&mut budget), resume_from_section: None, access,
                creation_reservation: None,
            }).await.unwrap_err();
            assert!(error.to_string().contains("novel_busy"), "{error}");
            assert!(http.swapped.load(Ordering::SeqCst));
            assert_eq!(store.read_small(&keys.section("1", "episode")).await.unwrap().unwrap(), b"replacement section bytes");
            assert_eq!(store.read_small(&keys.converted_text()).await.unwrap().unwrap(), b"replacement output");
            let toc = store.read_small(&keys.toc()).await.unwrap().unwrap();
            assert_eq!(crate::downloader::persistence::deserialize_toc(&toc).unwrap().title, "Replacement");
            assert_eq!(repository.get(1.into()).await.unwrap().unwrap().title, "Replacement");
            let replacement_epoch = store.gate_info(&ObjectPrefix::from(keys.prefix().clone())).await.unwrap().epoch;
            {
                let origins = origins.lock();
                assert_eq!(origins.len(), 2, "capture is checkpointed before HTTP and reused after it");
                assert!(origins.iter().all(|origin| origin == &origins[0]), "continuation must retain the first epoch");
                assert_ne!(origins[0].epoch, replacement_epoch);
            }
        }

        // The prefix cannot be known until a new work's first TOC fetch.
        // A transfer may claim either its URL or the same physical prefix
        // while that fetch awaits; a fresh late capture must not bless it.
        for (claimed_url, owns_creation) in [
            ("https://example.com/n1234ab/", false), ("https://example.com/different-url/", false),
            ("https://example.com/n1234ab/", true), ("https://example.com/different-url/", true),
        ] {
            let store = Arc::new(MemoryObjectStore::new());
            let mut baseline = sample_record(chrono::Utc::now());
            baseline.id = 0;
            baseline.toc_url = "https://example.com/n0000aa/".into();
            let repository = Arc::new(MemoryNovelRepository::from_records(vec![baseline]).with_objects(store.clone()));
            let creation_reservation = if owns_creation {
                let reservation = repository.allocate_id().await.unwrap();
                let pair = (reservation.id(), reservation.generation());
                reservation.adopt();
                Some(pair)
            } else { None };
            let keys = NovelObjectKeys::new("Fence Test", "n1234ab", false).unwrap();
            let imported: crate::db::NovelRecord = serde_yaml::from_str(&format!(
                "id: 1\nauthor: Author\ntitle: Original\nfile_title: n1234ab\ntoc_url: {claimed_url}\nsitename: Fence Test\nlast_update: 2026-01-01T00:00:00Z\n",
            )).unwrap();
            let raw = MockHttpClient::new();
            raw.add_text("https://example.com/n1234ab/", 200, "<h1>Original</h1><span>Author</span><a href=\"/n1234ab/1/\">episode</a>");
            raw.add_text("https://example.com/n1234ab/1/", 200, "<div class=\"body\">old fetched body</div>");
            let http = Arc::new(ReplacingHttp {
                http: raw, store: store.clone(), repository: repository.clone(), record: imported,
                keys: keys.clone(), swapped: AtomicBool::new(false), origins: Arc::new(parking_lot::Mutex::new(Vec::new())),
            });
            let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
                DownloaderPlatform {
                    http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()), novels: repository.clone(),
                    objects: store.clone(), assets: store.clone(), clock: Arc::new(SystemClock),
                }, vec![setting.clone()], HashMap::new(), Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
            ).unwrap();
            let error = downloader.download_novel_with_execution_options("https://example.com/n1234ab/", DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: None, access: None, creation_reservation,
            }).await.unwrap_err();
            assert!(error.to_string().contains("novel_busy"), "{error}");
            assert!(http.swapped.load(Ordering::SeqCst));
            assert_eq!(store.read_small(&keys.section("1", "episode")).await.unwrap().unwrap(), b"replacement section bytes");
            assert_eq!(store.read_small(&keys.converted_text()).await.unwrap().unwrap(), b"replacement output");
            let toc = store.read_small(&keys.toc()).await.unwrap().unwrap();
            assert_eq!(crate::downloader::persistence::deserialize_toc(&toc).unwrap().title, "Replacement");
            let claimed = repository.find_by_toc_url_current(claimed_url).await.unwrap().unwrap();
            if let Some((id, generation)) = creation_reservation {
                assert_ne!(claimed.id, id.0, "transfer must not reuse the downloader's live ID reservation");
                assert!(repository.get_current(id).await.unwrap().is_none());
                let reservation = repository.resume_id(id, generation).await.unwrap();
                repository.release_id(reservation).await.unwrap();
            }
            assert_eq!(claimed.title, "Replacement");
        }
    }

    #[tokio::test]
    async fn canonical_redirect_admits_once_without_replacing_an_owned_identity() {
        struct NoYield;
        impl SectionBudget for NoYield { fn should_yield(&mut self, _: usize) -> bool { false } }
        let definition = r#"
name: Redirect Test
domain: alias.test
top_url: https://alias.test
url: 'https://alias\.test/(?<ncode>n\d+[a-z]+)/?'
sitename: Redirect Test
toc_url: 'https://alias.test/\k<ncode>/'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
subtitles: '<a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
is_narou: false
"#;
        let mut alias_setting: SiteSetting = serde_yaml::from_str(definition).unwrap();
        let mut canonical_setting: SiteSetting = serde_yaml::from_str(&definition.replace("alias", "canonical")).unwrap();
        alias_setting.compile();
        canonical_setting.compile();
        let alias_url = "https://alias.test/n1234ab/";
        let canonical_url = "https://canonical.test/n1234ab/";
        let mut record = sample_record(chrono::Utc::now());
        record.id = 0;
        record.title = "Redirected".into();
        record.author = "Author".into();
        record.sitename = "Redirect Test".into();
        record.file_title = "kept-directory".into();
        record.toc_url = canonical_url.into();
        record.domain = Some("canonical.test".into());
        let store = Arc::new(MemoryObjectStore::new());
        let novels = Arc::new(MemoryNovelRepository::from_records(vec![record.clone()]).with_objects(store.clone()));
        let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, false).unwrap();
        let retained = keys.prefix().join("retained.dat").unwrap();
        store.write_small(&retained, b"canonical retained bytes".to_vec()).await.unwrap();
        let http = Arc::new(MockHttpClient::new());
        http.add_response(alias_url, HttpResponse {
            status: 302, headers: vec![("Location".into(), canonical_url.into())], body: Vec::new(),
        });
        http.add_text(canonical_url, 200, "<h1>Redirected</h1><span>Author</span><a href=\"/n1234ab/1/\">episode</a>");
        http.add_text("https://canonical.test/n1234ab/1/", 200, "<div class=\"body\">canonical body</div>");
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()), novels: novels.clone(),
                objects: store.clone(), assets: store.clone(), clock: Arc::new(SystemClock),
            }, vec![alias_setting.clone(), canonical_setting.clone()], HashMap::new(),
            Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
        ).unwrap();
        let checkpoint = Arc::new(MemoryObjectStore::new());
        let mut budget = CheckpointBudget::new(NoYield, checkpoint.clone());
        let result = downloader.download_novel_with_execution_options(alias_url, DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: None, creation_reservation: None,
        }).await.unwrap();
        assert_eq!(result.id, record.id);
        assert!(!result.new_novel);
        assert_eq!(budget.allocations, 0, "canonical discovery admits the existing identity before allocating a creation");
        assert_eq!(budget.restorations, 0);
        assert!(budget.creation_reservation.is_none());
        let saved = checkpoint.read_small(&ObjectKey::try_new(".narou-transfer/test-create-checkpoint").unwrap()).await.unwrap().unwrap();
        let (origin, creation): (AccessOrigin, Option<(NovelId, i64)>) = serde_json::from_slice(&saved).unwrap();
        assert_eq!(origin.prefix.as_str(), keys.prefix().as_ref());
        assert!(creation.is_none());
        assert_eq!(store.read_small(&retained).await.unwrap().unwrap(), b"canonical retained bytes");
        let body = store.read_small(&keys.section("1", "episode")).await.unwrap().unwrap();
        let section: SectionFile = serde_yaml::from_slice(&body).unwrap();
        assert_eq!(section.element.body, "canonical body");
        let toc = store.read_small(&keys.toc()).await.unwrap().unwrap();
        let requested = http.requested_urls();
        assert_eq!(requested.iter().filter(|url| url.ends_with(alias_url)).count(), 1);
        assert_eq!(requested.iter().filter(|url| url.ends_with(canonical_url)).count(), 2,
            "redirect discovery is reused; only the cookie-scoped canonical fetch follows it");

        // Once a creation is owned, the same redirect is a competing identity,
        // not permission to adopt the canonical record or its newer writer.
        let reservation = novels.allocate_id().await.unwrap();
        let creation = (reservation.id(), reservation.generation());
        reservation.adopt();
        let mut budget = CheckpointBudget::new(NoYield, Arc::new(MemoryObjectStore::new()));
        let error = downloader.download_novel_with_execution_options(alias_url, DownloadExecutionOptions {
            force: false, budget: Some(&mut budget), resume_from_section: None, access: None,
            creation_reservation: Some(creation),
        }).await.unwrap_err();
        assert!(error.to_string().contains("novel_busy"), "{error}");
        assert_eq!(budget.allocations, 0);
        assert_eq!(budget.restorations, 1);
        assert_eq!(store.read_small(&keys.section("1", "episode")).await.unwrap().unwrap(), body);
        assert_eq!(store.read_small(&keys.toc()).await.unwrap().unwrap(), toc);
        assert!(novels.get_current(creation.0).await.unwrap().is_none());
        let reservation = novels.resume_id(creation.0, creation.1).await.unwrap();
        novels.release_id(reservation).await.unwrap();

        // An already-known alias likewise keeps its original ID and prefix.
        let mut source = record.clone();
        source.id = 1;
        source.sitename = "Alias Source".into();
        source.file_title = "kept-source".into();
        source.toc_url = alias_url.into();
        let source_keys = NovelObjectKeys::new(&source.sitename, &source.file_title, false).unwrap();
        let source_body = source_keys.section("1", "episode");
        store.write_small(&source_body, b"original source bytes".to_vec()).await.unwrap();
        let current = novels.get_current(record.id.into()).await.unwrap().unwrap();
        let novels = Arc::new(MemoryNovelRepository::from_records(vec![current, source.clone()]).with_objects(store.clone()));
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http, rate_limiter: Arc::new(FakeRateLimiter::new()), novels: novels.clone(),
                objects: store.clone(), assets: store.clone(), clock: Arc::new(SystemClock),
            }, vec![alias_setting, canonical_setting], HashMap::new(),
            Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
        ).unwrap();
        let error = downloader.download_novel(alias_url).await.unwrap_err();
        assert!(error.to_string().contains("novel_busy"), "{error}");
        assert_eq!(store.read_small(&source_body).await.unwrap().unwrap(), b"original source bytes");
        assert_eq!(store.read_small(&keys.section("1", "episode")).await.unwrap().unwrap(), body);
        assert_eq!(store.read_small(&keys.toc()).await.unwrap().unwrap(), toc);
        assert_eq!(novels.get_current(source.id.into()).await.unwrap().unwrap().toc_url, alias_url);
    }


    #[tokio::test]
    async fn a_removed_existing_record_is_not_recreated_after_section_fetch() {
        use crate::platform::{HttpClient, HttpRequest, HttpStreamResponse};
        struct RemovingHttp {
            http: MockHttpClient,
            repository: Arc<MemoryNovelRepository>,
        }
        impl RemovingHttp {
            async fn remove_on_body(&self, request: &HttpRequest) -> Result<()> {
                if request.url.ends_with("/n1234ab/1/") {
                    self.repository.apply_batch(vec![NovelMutation::Remove(1.into())]).await?;
                }
                Ok(())
            }
        }
        impl HttpClient for RemovingHttp {
            fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
                Box::pin(async move { self.remove_on_body(&request).await?; self.http.send(request).await })
            }
            fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpStreamResponse>> {
                Box::pin(async move { self.remove_on_body(&request).await?; self.http.send_stream(request).await })
            }
            fn validate_url<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<()>> { self.http.validate_url(url) }
        }
        let mut setting: SiteSetting = serde_yaml::from_str(r#"
name: Removed Test
domain: example.com
top_url: https://example.com
url: 'https://example\.com/(?<ncode>n1234ab)/?'
sitename: Removed Test
toc_url: 'https://example.com/\k<ncode>/'
title: '<h1>(?<title>[^<]+)</h1>'
author: '<span>(?<author>[^<]+)</span>'
subtitles: '<a href="(?<href>/n1234ab/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
"#).unwrap();
        setting.compile();
        let record: crate::db::NovelRecord = serde_yaml::from_str("id: 1\nauthor: Author\ntitle: Original\nfile_title: n1234ab\ntoc_url: https://example.com/n1234ab/\nsitename: Removed Test\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        let store = Arc::new(MemoryObjectStore::new());
        let repository = Arc::new(MemoryNovelRepository::from_records(vec![record.clone()]).with_objects(store.clone()));
        let http = MockHttpClient::new();
        http.add_text("https://example.com/n1234ab/", 200, "<h1>Original</h1><span>Author</span><a href=\"/n1234ab/1/\">episode</a>");
        http.add_text("https://example.com/n1234ab/1/", 200, "<div class=\"body\">fetched body</div>");
        let http = Arc::new(RemovingHttp { http, repository: repository.clone() });
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http, rate_limiter: Arc::new(FakeRateLimiter::new()), novels: repository.clone(),
                objects: store.clone(), assets: store.clone(), clock: Arc::new(SystemClock),
            }, vec![setting], HashMap::new(), Arc::new(crate::downloader::settings::SnapshotDownloaderSettings::default()),
        ).unwrap();
        // This token must come from the injected store pair, not a newly
        // constructed native/default backend with an unrelated gate.
        let origin = downloader.capture_write_for_record(&record).await.unwrap();
        let keys = NovelObjectKeys::new("Removed Test", "n1234ab", false).unwrap();
        assert_eq!(origin.prefix(), &ObjectPrefix::from(keys.prefix().clone()));
        let error = downloader.download_novel_with_execution_options("1", DownloadExecutionOptions {
            force: false, budget: None, resume_from_section: None, access: Some(origin),
            creation_reservation: None,
        }).await.unwrap_err();
        assert!(matches!(error, NarouError::NotFound(_)), "{error}");
        assert!(error.to_string().contains("novel removed during download"));
        assert!(repository.get(1.into()).await.unwrap().is_none());
    }
}
