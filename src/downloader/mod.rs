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

use self::novel_info::NovelInfo;
use self::persistence::section_filename;
use self::persistence::{PersistenceService, SectionLoadFailure, compute_section_hash};

use crate::platform::{CookieStore, LoginGroup};
use self::section::{SectionCache, download_section};
use self::security::is_safe_public_url_syntax;
use self::settings::DownloaderSettings;
use self::site_setting::SiteSetting;
use self::toc::{
    create_short_story_subtitles, fetch_toc, parse_subtitles, parse_subtitles_multipage,
};
use self::util::{
    build_section_url, compile_html_pattern, load_length_limit, mask_spoiler_text,
    sanitize_filename_with_limit,
};
use crate::application::messages;
use crate::db::novel_record::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, Clock, HttpClient, NovelMutation, NovelObjectKeys, NovelRepository, ObjectStore,
    ProgressReporter, RateLimiter,
};

pub use self::types::{
    ARCHIVE_ROOT_DIR, DownloadResult, NarouApiEntry, RAW_DATA_DIR, SECTION_SAVE_DIR,
    SectionElement, SectionFile, SubtitleInfo, TargetType, TocFile, TocObject, UpdateStatus,
};
pub use self::util::pretreatment_source;

#[cfg(feature = "native-runtime")]
pub(crate) const SECTION_HASH_CACHE_NAME: &str = "section_hash_cache";
const DEFAULT_SITE_TIMEZONE: &str = "Asia/Tokyo";

/// A platform-neutral budget checked only at safe section boundaries.
///
/// Implementations must be cheap and side-effect free: the downloader calls
/// this before starting the next section, never while a section is in flight.
pub trait SectionBudget: Send {
    fn should_yield(&mut self, next_section_index: usize) -> bool;
}

/// Execution options for a resumable download.
///
/// `resume_from_section` identifies the first section that still needs
/// network work. The downloader restores every preceding persisted section
/// from the object store before fetching anything after that boundary.
pub struct DownloadExecutionOptions<'a> {
    pub force: bool,
    pub budget: Option<&'a mut dyn SectionBudget>,
    pub resume_from_section: Option<usize>,
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
    existing_id: Option<i64>,
    toc_url: &'a str,
    novel_dir: &'a Path,
    title: &'a str,
    latest_story: &'a str,
    old_count: usize,
    latest_count: usize,
}

/// One [`Downloader::section_needs_download`] probe: the latest and stored
/// subtitle rows plus the cache/state handles the decision consults.
struct SectionDecision<'a> {
    setting: &'a SiteSetting,
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

fn toc_title_for_type_inference(
    setting: &SiteSetting,
    toc_source: &str,
    info: &NovelInfo,
) -> String {
    info.title
        .clone()
        .or_else(|| setting.resolve_info_pattern("t", toc_source))
        .unwrap_or_default()
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

fn resolve_novel_type(
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
    info: &NovelInfo,
) -> (u8, bool) {
    let title = toc_title_for_type_inference(setting, toc_source, info);
    let toc_implies_series = toc_subtitles_imply_series(setting, toc_source, url_captures, &title);

    if let Some(nt) = info.novel_type {
        if nt == 2 && toc_implies_series {
            return (1u8, false);
        }
        return (nt, info.end.unwrap_or(false));
    }

    if let Some(text) = setting.resolve_info_pattern("nt", toc_source) {
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
                crate::application::debug::emit_error(format!(
                    "挿絵: 実体の確認に失敗 ({error})"
                ));
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
            PersistenceService::with_clock(objects.clone(), assets.clone(), clock.clone());

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

    /// Admission identity follows the effective YAML definition, not an alias host.
    /// Resolving this key performs no HTTP requests and does not clone the DSL.
    pub async fn site_key_for_target(&self, target: &str) -> Result<String> {
        if matches!(Self::get_target_type(target), TargetType::Url) {
            let setting = self.site_settings.iter()
                .find(|setting| setting.matches_series_url(target))
                .or_else(|| self.site_setting_for_url(target))
                .or_else(|| self.site_settings.iter().find(|setting| setting.matches_author_url(target)))
                .ok_or_else(|| NarouError::InvalidTarget(format!("No site setting for URL: {target}")))?;
            return Ok(crate::platform::site_family(&setting.domain));
        }
        let (_, setting) = self.resolve_target_for_download(target).await?;
        Ok(crate::platform::site_family(&setting.domain))
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
            .get(id.into())
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
            resolve_novel_type(&setting, &toc_source, &url_captures, &info);
        let is_end = Some(inferred_is_end);

        // Ruby: get_general_lastup / get_novelupdated_at fall back to subtitle dates
        // when novel_info_url is unavailable (e.g. Arcadia)
        let mut novelupdated_at = info.novelupdated_at;
        let mut general_lastup = info.general_lastup;
        if novelupdated_at.is_none() || general_lastup.is_none() {
            let site_timezone = setting.site_timezone();
            let subtitles = if novel_type == 2 {
                create_short_story_subtitles(&setting, &toc_source, &info).ok()
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
                            let _ = crate::compat::set_frozen_state(id, true);
                        }
                        return Ok(true);
                    }
                    crate::compat::DigestChoice::Backup => {
                        let backup_name = crate::compat::create_backup(novel_dir, title)?;
                        self.report_line(&messages::download::backup_created(backup_name));
                    }
                    crate::compat::DigestChoice::ShowStory => {
                        self.report_line(messages::download::story_label());
                        self.report_line(latest_story);
                    }
                    crate::compat::DigestChoice::OpenBrowser => {
                        crate::compat::open_browser(toc_url);
                    }
                    crate::compat::DigestChoice::OpenFolder => {
                        crate::compat::open_directory(novel_dir, None);
                    }
                    crate::compat::DigestChoice::Convert => {
                        if let Some(id) = existing_id {
                            let author = self
                                .novels
                                .get(id.into())
                                .await
                                .ok()
                                .flatten()
                                .map(|record| record.author)
                                .unwrap_or_default();
                            let _ = crate::compat::convert_existing_novel(
                                id, title, &author, novel_dir, false,
                            );
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
        setting: &SiteSetting,
        section: &SectionElement,
        object_keys: &NovelObjectKeys,
        illustration_store: &mut crate::illustration_store::IllustrationIndex,
        mut stored_illustrations: Option<&mut std::collections::HashSet<String>>,
        toc_url: &str,
        raw_html: Option<&str>,
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
                    self.report_warn(&messages::download::warn_unsafe_illustration_url(url));
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
                        crate::application::debug::emit(format!(
                            "挿絵: 取得済みのため再取得しません: {url}"
                        ));
                        continue;
                    }
                    crate::application::debug::emit(format!(
                        "挿絵: 索引にはありますが実体が無いので取り直します: 挿絵/{filename} ({url})"
                    ));
                }

                match crate::downloader::http_policy::fetch_bytes(
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
                        // Animated works arrive as a frame archive; the site
                        // definition declares the frame timings on the URL.
                        let (body, content_type) =
                            match crate::illustration_animation::assemble_animation(
                                url,
                                &response.body,
                            ) {
                                Some(Ok(apng)) => (apng, "image/png".to_string()),
                                Some(Err(err)) => {
                                    self.report_warn(&messages::download::warn_animation_assemble(
                                        url, err,
                                    ));
                                    (response.body.clone(), content_type)
                                }
                                None => (response.body.clone(), content_type),
                            };
                        let ext =
                            crate::illustration_store::illustration_extension_from_content_type(
                                &content_type,
                            )
                            .unwrap_or_else(|| {
                                crate::illustration_store::guessed_extension_from_url(url)
                            });
                        match storage
                            .store_bytes(object_keys, illustration_store, url, &body, ext)
                            .await
                        {
                            Err(err) => {
                                crate::application::debug::emit_error(format!(
                                    "挿絵の保存に失敗 ({url}): {err}"
                                ));
                                self.report_warn(&messages::download::warn_illustration_save(url, err));
                            }
                            Ok(stored) => {
                                let filename = stored.filename;
                                crate::application::debug::emit(format!(
                                    "挿絵を保存しました: {url} → 挿絵/{filename} ({} bytes)",
                                    body.len()
                                ));
                                if let Some(names) = stored_illustrations.as_deref_mut() {
                                    names.insert(filename);
                                }
                            }
                        }
                    }
                    Err(err) => {
                        crate::application::debug::emit_error(format!(
                            "挿絵の取得に失敗 ({url}): {err}"
                        ));
                        self.report_warn(&messages::download::warn_illustration_download(url, err));
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
            },
        )
        .await
    }

    /// Download with an optional durable section resume cursor.
    pub async fn download_novel_with_execution_options(
        &mut self,
        target: &str,
        options: DownloadExecutionOptions<'_>,
    ) -> Result<DownloadResult> {
        // Preprocess job results belong to one novel: a new download starts
        // with an empty set so nothing leaks between works.
        self.preprocess_jobs.reset();
        self.section_cache = section::SectionCache::new();
        let (existing_id, setting) = self.resolve_target_for_download(target).await?;
        let setting = setting.clone();
        let (provisional_id, reservation) = match existing_id {
            Some(id) => (id, None),
            None => {
                let reservation = self.novels.allocate_id().await?;
                (reservation.id().0, Some(reservation))
            }
        };
        let result = self.download_novel_reserved(
            target, options, existing_id, setting, provisional_id,
        ).await;
        if let Some(reservation) = reservation
            && !matches!(&result, Ok(download) if download.status == UpdateStatus::Ok && download.id == reservation.id().0)
            && let Err(release_error) = self.novels.release_id(reservation).await
        {
            return Err(match result {
                Err(error) => NarouError::Platform(format!("{error}; 小説IDの予約を解放できませんでした: {release_error}")),
                Ok(_) => release_error,
            });
        }
        result
    }

    async fn download_novel_reserved(
        &mut self,
        target: &str,
        mut options: DownloadExecutionOptions<'_>,
        existing_id: Option<i64>,
        mut setting: SiteSetting,
        provisional_id: i64,
    ) -> Result<DownloadResult> {
        let force = options.force;
        let resume_from_section = options.resume_from_section;
        let mut budget = options.budget.take();

        let db_toc_url = if let Some(id) = existing_id {
            self.novels
                .get(id.into())
                .await
                .ok()
                .flatten()
                .map(|record| record.toc_url)
        } else {
            None
        };

        let url_captures = db_toc_url
            .as_deref()
            .and_then(|url| setting.extract_url_captures(url))
            .or_else(|| setting.extract_url_captures(target))
            .or_else(|| ncode_target_url(target).and_then(|url| setting.extract_url_captures(&url)))
            .unwrap_or_default();
        let toc_url = if let Some(ref url) = db_toc_url {
            url.clone()
        } else if url_captures.is_empty() {
            setting.toc_url()
        } else {
            let mut selects = url_captures.clone();
            selects.insert("__target_url".to_string(), target.to_string());
            setting.get_toc_url_with_captures(&selects)
        };
        let toc_url = Self::ageauth_redirect_target(&toc_url).unwrap_or(toc_url);
        // Login fallback: a novel already flagged as needing authentication
        // sends the stored cookie from the first request; every other novel
        // stays anonymous until a fetch actually fails.
        let mut configured_cookie = setting.cookie.clone();
        let mut login_session: Option<String> = None;
        let mut login_cookie = match existing_id {
            Some(id) => {
                let record = self.novels.get(id.into()).await.ok().flatten();
                let requires = record.as_ref().is_some_and(|record| record.requires_login);
                if requires {
                    // その小説で前に成功したログインがあればそれを、無ければ
                    // 一覧の先頭を最初のリクエストから送る。
                    let groups = self.stored_groups_for(&setting).await;
                    let chosen = record
                        .as_ref()
                        .and_then(|record| record.login_session.as_deref())
                        .and_then(|id| groups.iter().find(|group| group.id == id))
                        .or_else(|| groups.first());
                    chosen.and_then(|group| {
                        let cookie = group.merged_cookie();
                        if cookie.is_empty() {
                            return None;
                        }
                        apply_login_cookie(&mut setting, &cookie, &group.cookies);
                        login_session = Some(group.id.clone());
                        Some(cookie)
                    })
                } else {
                    None
                }
            }
            None => None,
        };
        let mut requires_login = login_cookie.is_some();
        let mut login_site_changed = false;
        let (resolved_toc_url, prefetched_toc) =
            match crate::downloader::http_policy::resolve_final_url_with_body(
                self.http.as_ref(),
                self.rate_limiter.as_ref(),
                &toc_url,
                &crate::downloader::http_policy::FetchPolicy::for_site(&setting),
            )
            .await
            {
                Ok((final_url, _body)) if final_url != toc_url => {
                    let final_url = Self::ageauth_redirect_target(&final_url).unwrap_or(final_url);
                    if let Some(mut final_setting) = self.find_site_setting(&final_url) {
                        configured_cookie = final_setting.cookie.clone();
                        if final_setting.domain == setting.domain {
                            final_setting.cookie = setting.cookie.clone();
                            final_setting.login_cookie_hosts = setting.login_cookie_hosts.clone();
                        } else {
                            login_site_changed = true;
                            login_cookie = None;
                            login_session = None;
                            requires_login = false;
                        }
                        setting = final_setting;
                        (
                            setting
                                .toc_url_with_url_captures(&final_url)
                                .unwrap_or(final_url),
                            None,
                        )
                    } else {
                        (final_url, None)
                    }
                }
                Ok((final_url, body)) => (final_url, body),
                Err(_) => (toc_url, None),
            };
        let toc_url = resolved_toc_url;
        let url_captures = setting
            .extract_url_captures(&toc_url)
            .unwrap_or(url_captures);
        // リダイレクト解決で toc_url が入力時と変わった場合、解決後の URL で
        // 既存レコードを再照合して ID を引き継ぐ。別ドメインへ転送されるサイト
        // (例: ハーメルンの h.syosetu.org 分離)で同じ小説が重複登録されるのを防ぐ。
        let existing_id = match existing_id {
            Some(id) => Some(id),
            None => self
                .novels
                .find_by_toc_url(&toc_url)
                .await
                .ok()
                .flatten()
                .map(|record| record.id),
        };
        let provisional_id = existing_id.unwrap_or(provisional_id);
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
                        let policy =
                            crate::downloader::http_policy::FetchPolicy::for_site(&setting);
                        crate::downloader::util::pretreatment_source_with_jobs(
                            crate::downloader::util::PretreatmentRequest {
                                http: self.http.as_ref(),
                                rate_limiter: self.rate_limiter.as_ref(),
                                policy: &policy,
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
        // Login fallback: retry once with the stored cookie before treating the
        // novel as deleted. Nothing is sent when no cookie is stored, so sites
        // the user never signed in to behave exactly as before.
        let mut toc_source = toc_source;
        let login_wall = should_retry_with_login(&setting, &toc_source);
        if login_cookie.is_none() && login_wall {
            // ログイン壁だけ先に解消する。部分一覧の比較は全ページを読んでから行う。
            let groups = self.stored_groups_for(&setting).await;
            if !groups.is_empty() {
                self.report_line(messages::download::login_retry());
                let base_cookie = setting.cookie.clone();
                let mut winner: Option<LoginGroup> = None;
                for group in &groups {
                    setting.cookie = base_cookie.clone();
                    apply_login_cookie(&mut setting, &group.merged_cookie(), &group.cookies);
                    let mut candidate_jobs = preprocess::PreprocessJobs::new();
                    let retried = fetch_toc(
                        self.http.as_ref(),
                        self.rate_limiter.as_ref(),
                        &setting,
                        &toc_url,
                        &mut candidate_jobs,
                    )
                    .await;
                    if retried.is_ok() && !should_retry_with_login(&setting, &retried) {
                        requires_login = true;
                        login_session = Some(group.id.clone());
                        toc_source = retried;
                        winner = Some(group.clone());
                        self.preprocess_jobs = candidate_jobs;
                        break;
                    }
                }
                // 採用した資格情報を最終状態に残す（本文も同じ Cookie で取る）。
                setting.cookie = base_cookie;
                setting.login_cookie_hosts.clear();
                if let Some(winner) = &winner {
                    apply_login_cookie(&mut setting, &winner.merged_cookie(), &winner.cookies);
                }
            }
        }
        if toc_source.is_ok() && should_retry_with_login(&setting, &toc_source) {
            toc_source = Err(NarouError::NotFound("Novel deleted or private".into()));
        }
        let mut toc_source = match toc_source {
            Ok(source) => source,
            Err(NarouError::NotFound(_)) if existing_id.is_some() => {
                let id = existing_id.unwrap();
                self.report_line(messages::download::novel_unavailable());
                #[cfg(feature = "native-runtime")]
                {
                    let _ = crate::compat::mark_not_found_and_freeze(id);
                }
                let record = self.novels.get(id.into()).await.ok().flatten();
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
                return Ok(DownloadResult {
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
                });
            }
            Err(err) => return Err(err),
        };
        let toc_preview_info = NovelInfo::from_toc_source(&setting, &toc_source);
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

        let mut info = self
            .load_novel_info(&setting, &toc_source, &url_captures, &toc_url)
            .await?;

        let existing_record = match existing_id {
            Some(eid) => self.novels.get(eid.into()).await.ok().flatten(),
            None => None,
        };

        let (novel_type, is_end) = resolve_novel_type(&setting, &toc_source, &url_captures, &info);

        let toc::TocListing { mut subtitles, partial: mut partial_listing, tag_sources: mut page_tags } = if novel_type == 2 {
            toc::TocListing {
                subtitles: create_short_story_subtitles(&setting, &toc_source, &info)?,
                partial: false,
                tag_sources: setting.resolve_info_pattern("tags", &toc_source).into_iter().collect(),
            }
        } else {
            parse_subtitles_multipage(toc::MultipageTocRequest {
                http: self.http.as_ref(),
                rate_limiter: self.rate_limiter.as_ref(),
                setting: &setting,
                toc_source: &toc_source,
                url_captures: &url_captures,
                title: info.title.as_deref().unwrap_or_default(),
                progress: self.progress.as_deref(),
                jobs: &mut self.preprocess_jobs,
            })
            .await?
        };

        // 目次ページの途中で「一覧が欠けている」と分かった場合 (Pixiv の小説
        // シリーズは未ログインだと本文一覧が 0 件になる) は、保存済み Cookie で
        // 取り直し、話数が増えたときだけ採用する。
        if partial_listing || subtitles.is_empty() {
            let groups = self.stored_groups_for(&setting).await;
            if !groups.is_empty() {
                self.report_line(messages::download::login_retry());
                let active_cookie = setting.cookie.clone();
                let active_cookie_hosts = setting.login_cookie_hosts.clone();
                let base_cookie = configured_cookie.clone();
                let mut winner: Option<LoginGroup> = None;
                for group in &groups {
                    if login_session.as_deref() == Some(group.id.as_str()) {
                        continue;
                    }
                    setting.cookie = base_cookie.clone();
                    apply_login_cookie(&mut setting, &group.merged_cookie(), &group.cookies);
                    let mut candidate_jobs = preprocess::PreprocessJobs::new();
                    let Ok(retried_source) = fetch_toc(
                        self.http.as_ref(), self.rate_limiter.as_ref(), &setting,
                        &toc_url, &mut candidate_jobs,
                    ).await else {
                        continue;
                    };
                    let Ok(toc::TocListing { subtitles: retried, partial: retried_partial, tag_sources: retried_tags }) =
                        parse_subtitles_multipage(toc::MultipageTocRequest {
                            http: self.http.as_ref(),
                            rate_limiter: self.rate_limiter.as_ref(),
                            setting: &setting,
                            toc_source: &retried_source,
                            url_captures: &url_captures,
                            title: info.title.as_deref().unwrap_or_default(),
                            progress: self.progress.as_deref(),
                            jobs: &mut candidate_jobs,
                        })
                        .await
                    else {
                        continue;
                    };
                    if !retried.is_empty()
                        && ((!retried_partial && partial_listing) || retried.len() > subtitles.len())
                    {
                        let resolved = !retried_partial;
                        subtitles = retried;
                        partial_listing = retried_partial;
                        page_tags = retried_tags;
                        requires_login = true;
                        login_session = Some(group.id.clone());
                        winner = Some(group.clone());
                        self.preprocess_jobs = candidate_jobs;
                        toc_source = retried_source;
                        info = self.load_novel_info(&setting, &toc_source, &url_captures, &toc_url).await?;
                        if resolved {
                            break;
                        }
                    }
                }
                setting.cookie = active_cookie;
                setting.login_cookie_hosts = active_cookie_hosts;
                if let Some(winner) = &winner {
                    setting.cookie = base_cookie;
                    setting.login_cookie_hosts.clear();
                    apply_login_cookie(&mut setting, &winner.merged_cookie(), &winner.cookies);
                }
            }
        }
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
        let strip_title_prefix =
            self.settings
                .strip_title_prefix_for(provisional_id, &raw_title, &author);
        let track_raw_title = strip_title_prefix
            || existing_record
                .as_ref()
                .is_some_and(NovelRecord::has_raw_title);
        let title = crate::title::project_title(&raw_title, strip_title_prefix);
        let use_subdirectory = self.download_use_subdirectory(existing_record.as_ref());
        let ncode = self
            .extract_ncode(&setting, &toc_source)
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

        let old_toc = self
            .persistence
            .load_toc(&object_keys)
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
                });
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
            self.report_warn(&messages::download::partial_listing(accessible_section_count));
        }

        #[cfg(feature = "native-runtime")]
        if let Some(existing) = existing_record.as_ref() {
            crate::title::rename_projected_outputs(&novel_dir, &author, &existing.title, &title)?;
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
            .load_cache(&object_keys)
            .await?
            .unwrap_or_default();
        if setting.illust_grep_pattern.is_none() {
            // 既定パターンは本文の `<img>` 全部に当たるため、セクションごとでは
            // なく小説 1 件につき 1 行だけ出す (24 話で 24 行出していた)。
            crate::application::debug::emit(format!(
                "挿絵: サイト定義に illust_grep_pattern が無いので既定の <img> パターンで取得します ({})",
                setting.domain
            ));
        }
        // 索引が空なら突き合わせる相手が無いので一覧は取らない (小説 1 件に
        // つき 1〜2 サブリクエスト増えるため、必要なときだけ)。
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
                crate::application::debug::emit(format!(
                    "挿絵: 保存済み {} 件 / 索引 {} 件 ({})",
                    names.as_ref().map(|names| names.len()).unwrap_or(0),
                    illustration_store.source_count(),
                    object_keys.prefix().as_ref(),
                ));
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
        // 再開時に復元する既読セクション。直列 1 件ずつの読みをやめ、
        // PersistenceService 側の窓 (SECTION_READ_WINDOW) で先読みする。
        // 走査順で最初の失敗の扱いは直列時と同じ。
        let resumed = match self
            .persistence
            .load_sections_ordered(&object_keys, &subtitles[..resume_from])
            .await
        {
            Ok(sections) => sections,
            Err(SectionLoadFailure::Missing { section_index }) => {
                return Err(NarouError::DownloadResumeCorrupt(format!(
                    "persisted section {section_index} ({}) is missing",
                    subtitles[section_index].index
                )));
            }
            Err(SectionLoadFailure::Read(error)) => return Err(error),
        };
        let mut resumed_sections: HashMap<usize, SectionFile> =
            resumed.into_iter().enumerate().collect();

        // One listing pass replaces a per-section existence read. On the
        // Worker this is the difference between ⌈N/1000⌉ LIST subrequests
        // and N GETs; on native it avoids N read+parse round trips.
        let existing_names = self
            .persistence
            .list_novel_object_names(&object_keys)
            .await?;
        let existing_sections = PersistenceService::existing_section_indices(&existing_names);

        struct SectionPlan {
            is_new_arrival: bool,
            needs_download: bool,
            /// The section file existed before this run (canonical or legacy
            /// name). Drives the cache move without a second read.
            existed: bool,
            predownloaded: Option<(SectionElement, String)>,
            resumed: Option<SectionFile>,
            preserved: bool,
        }

        let mut updated_count = 0usize;
        let mut new_arrivals = existing_id.is_none();
        let mut new_arrival_subtitles = Vec::new();
        let mut final_subtitles = Vec::with_capacity(subtitles.len());
        let strong_update = self.settings.local_setting_bool("update.strong");
        // Budgeted strong-update comparisons must fetch only after the
        // per-section yield check. Unbudgeted CLI runs retain exact pre-counts.
        let defer_strong_update = strong_update && budget.is_some();
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
            let resumed = match resumed_sections.remove(&section_index) {
                Some(section) => Some(section),
                None if preserved => Some(self.persistence.load_section(&object_keys, subtitle)
                    .await?.ok_or_else(|| NarouError::Platform(format!(
                        "preserved section {} is missing from storage", subtitle.index,
                    )))?),
                None => None,
            };
            let is_new_arrival = resumed.is_none() && !old_subtitles.contains_key(&subtitle.index);
            let existed = existing_sections.contains(&subtitle.index);
            let (needs_download, predownloaded) = if resumed.is_some() {
                (false, None)
            } else if force {
                (true, None)
            } else {
                self.section_needs_download(SectionDecision {
                    setting: &setting,
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
                preserved,
            });
        }

        if let Some(ref p) = self.progress {
            p.set_position(0);
            p.set_length(download_count as u64);
            p.set_message(&format!("DL {}", title));
        }

        let mut last_chapter = String::new();
        let mut last_subchapter = String::new();
        let mut started_download = false;
        let mut downloaded_index = 0usize;

        let section_result: Result<()> = async {
            for (section_index, (subtitle, mut plan)) in subtitles.iter().zip(section_plans).enumerate() {
                if section_index >= resume_from
                    && budget
                        .as_deref_mut()
                        .is_some_and(|budget| budget.should_yield(section_index))
                {
                    return Err(NarouError::DownloadBudgetExpired {
                        next_section_index: section_index,
                    });
                }
                if defer_strong_update && !force && plan.resumed.is_none() && plan.needs_download {
                    let (needs_download, predownloaded) = self
                        .section_needs_download(SectionDecision {
                            setting: &setting,
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
                            p.set_length(download_count as u64);
                        }
                    }
                }
                let is_new_arrival = plan.is_new_arrival;
                let needs_download = plan.needs_download;
                let resumed_was_tracked = plan.resumed.is_some();
                // 中断された実行を再開したセクションは、前回の索引保存前に実行が
                // 終わっている可能性がある (挿絵の実体だけが残り、対応表が無い
                // 状態)。再開したセクションは取得を省く分だけ挿絵の登録も
                // 通り過ぎてしまうため、保存済みの本文から索引を作り直す。
                // 強更新の予定段階で取得しただけの本文も同じ理由でここを通す。
                // 索引が残っている通常の更新では `download_illustration` が
                // 取得済みと判断して何もしない。
                if !plan.preserved && (resumed_was_tracked || (!needs_download && plan.predownloaded.is_some())) {
                    let section = plan
                        .resumed
                        .as_ref()
                        .map(|resumed| &resumed.element)
                        .or_else(|| plan.predownloaded.as_ref().map(|(section, _)| section));
                    if let Some(section) = section {
                        self.download_illustration(
                            &setting,
                            section,
                            &object_keys,
                            &mut illustration_store,
                            stored_illustrations.as_mut(),
                            &toc_url,
                            None,
                        )
                        .await?;
                    }
                }

                let download_time = if let Some(resumed) = plan.resumed {
                    resumed.download_time
                } else if needs_download {
                    if !started_download {
                        self.report_line(&colorize(
                            &messages::download::download_started(display_id, &title),
                            "green",
                        ));
                        started_download = true;
                    }
                    if let Some(ref p) = self.progress {
                        p.set_message(&format!(
                            "DL {} [{}/{}]",
                            title,
                            downloaded_index + 1,
                            download_count
                        ));
                    }

                    if !subtitle.chapter.is_empty() && subtitle.chapter != last_chapter {
                        self.report_line(&subtitle.chapter);
                        last_chapter = subtitle.chapter.clone();
                    }
                    if !subtitle.subchapter.is_empty() && subtitle.subchapter != last_subchapter {
                        self.report_line(&subtitle.subchapter);
                        last_subchapter = subtitle.subchapter.clone();
                    }

                    let (section, raw_html) = match plan.predownloaded {
                        Some(downloaded) => downloaded,
                        None => download_section(
                            self.http.as_ref(),
                            self.rate_limiter.as_ref(),
                            &mut self.section_cache,
                            &setting,
                            subtitle,
                            &toc_url,
                            &mut self.preprocess_jobs,
                        )
                        .await?,
                    };
                    let had_existing = plan.existed;
                    if had_existing {
                        // The stored section still has the previous TOC's filename.
                        let previous = old_subtitles.get(&subtitle.index).copied().unwrap_or(subtitle);
                        if cache_dir.is_none() {
                            cache_dir = cache_timestamp(self.clock.as_ref(), save_diff_snapshots);
                        }
                        if let Some(id) = existing_id {
                            self.clear_section_digest(id, &section_relative_path(previous));
                        }
                        if let Some(timestamp) = cache_dir.as_deref() {
                            let cached = self.persistence
                                .cache_section(&object_keys, timestamp, previous)
                                .await?;
                            if cached && !cache_version_recorded {
                                self.persistence.record_cache_version(&object_keys, timestamp).await?;
                                cache_version_recorded = true;
                            }
                        }
                    }
                    let download_time = self
                        .persistence
                        .save_section(&object_keys, subtitle, &section)
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
                            .save_raw(&object_keys, subtitle, &raw_html)
                            .await?;
                    }
                    self.download_illustration(
                        &setting,
                        &section,
                        &object_keys,
                        &mut illustration_store,
                        stored_illustrations.as_mut(),
                        &toc_url,
                        Some(raw_html.as_str()),
                    )
                    .await?;
                    updated_count += 1;
                    downloaded_index += 1;
                    Some(download_time)
                } else {
                    // 内容が変わっていない (`needs_download=false`) セクションは
                    // 保存も取得もしないので、保存時刻は旧 TOC の値を引き継ぐ。
                    old_subtitles
                        .get(&subtitle.index)
                        .and_then(|old| old.download_time.clone())
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
                if needs_download && is_new_arrival {
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
                    self.report_line(&line);
                    if let Some(ref p) = self.progress {
                        p.inc(1);
                    }
                }
            }
            Ok(())
        }.await;

        // 挿絵を 1 枚も扱わなかった小説には空の索引を作らない (変化が無ければ
        // 書き戻さない)。
        let cache_result = if illustration_store.is_dirty() {
            self.persistence.save_cache(&object_keys, &illustration_store).await
        } else {
            Ok(())
        };
        if let Err(error) = cache_result {
            if section_result.is_err() {
                self.report_warn(&messages::indented_error(&error));
            } else {
                return Err(error);
            }
        }
        section_result?;

        if let Some(ref p) = self.progress {
            p.finish_with_message(&format!(
                "DL {} done ({}/{})",
                title,
                updated_count,
                subtitles.len()
            ));
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

        let toc_file = TocFile {
            title: toc_title.clone(),
            author: toc_author.clone(),
            toc_url: toc_url.clone(),
            story: new_story.clone(),
            subtitles: final_subtitles,
            novel_type: Some(novel_type),
        };
        self.persistence.save_toc(&object_keys, &toc_file).await?;
        // Portable history uses copy: retire renamed current keys only after
        // both the replacement body and its new TOC are durable.
        #[cfg(not(feature = "native-runtime"))]
        for key in superseded_section_keys {
            self.persistence.objects().delete(&key).await?;
        }
        self.persistence
            .ensure_default_files(
                &object_keys,
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
            requires_login,
            login_session: login_session.clone(),
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
            match self.novels.get(eid.into()).await? {
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
                    if login_site_changed {
                        updated.requires_login = requires_login;
                    } else {
                        updated.requires_login |= requires_login;
                    }
                    if login_site_changed || login_session.is_some() {
                        updated.login_session = login_session.clone();
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
                    let merged_id = updated.id;
                    self.novels
                        .apply_batch(vec![NovelMutation::Upsert(updated)])
                        .await?;
                    merged_id
                }
                None => {
                    // レコードが消えていた場合: 先頭で確保した provisional_id を使う
                    // (従来の update_records 分岐と同挙動)。
                    let mut rec = record.clone();
                    rec.id = provisional_id;
                    rec.tags = auto_tags.clone();
                    self.novels
                        .apply_batch(vec![NovelMutation::Upsert(rec)])
                        .await?;
                    provisional_id
                }
            }
        } else {
            // 新規: 先頭で確保した provisional_id をそのまま使う。ここで採番し
            // 直すと DL開始行の ID と実際のレコード ID が食い違い、誰も使わない
            // ID が 1 つ消費される (Ruby 版も Downloader 初期化時の
            // `create_new_id` をそのままレコードの ID に使う)。
            let new_id = provisional_id;
            let mut rec = record.clone();
            rec.id = new_id;
            rec.tags = auto_tags.clone();
            self.novels
                .apply_batch(vec![NovelMutation::Upsert(rec)])
                .await?;
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
            self.persistence.record_cache_version(&object_keys, &timestamp).await?;
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
    }

    /// 戻り値: `(要 DL, 強更新の比較で取得済みなら (本文, raw))`。
    ///
    /// 取得はセクションループと同じ `download_section` を通るので、
    /// 比較済みの本文を保存・挿絵走査へ渡せば二重取得は起きない。
    async fn section_needs_download(
        &mut self,
        probe: SectionDecision<'_>,
    ) -> Result<(bool, Option<(SectionElement, String)>)> {
        let SectionDecision {
            setting,
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

        if old.subtitle != latest.subtitle || old.chapter != latest.chapter {
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
            let Some(old_section) = self.persistence.load_section(keys, old).await? else {
                return Ok((true, None));
            };
            if date_string_to_ymd_with_timezone(basis_date, timezone)
                == section_timestamp_ymd(old_section.download_time.as_deref(), timezone)
            {
                let downloaded = download_section(
                    self.http.as_ref(),
                    self.rate_limiter.as_ref(),
                    &mut self.section_cache,
                    setting,
                    latest,
                    toc_url,
                    &mut self.preprocess_jobs,
                )
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
                    .find_by_toc_url(&toc_url)
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
                            let setting =
                                self.site_setting_for_url(&record.toc_url).ok_or_else(|| {
                                    NarouError::SiteSetting("No matching site setting".into())
                                })?;
                            (Some(record.id), setting, record.toc_url.clone())
                        }
                        None => {
                            let narou_url = narou_ncode_url(&ncode);
                            let setting =
                                self.site_setting_for_url(&narou_url).ok_or_else(|| {
                                    NarouError::InvalidTarget(format!(
                                        "対応外のncodeです({})",
                                        ncode
                                    ))
                                })?;
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
                        .find_by_toc_url(&toc_url)
                        .await?
                        .map(|record| record.id),
                };
                Ok((existing_id, setting))
            }
            TargetType::Id => {
                let id: i64 = target
                    .parse()
                    .map_err(|_| NarouError::InvalidTarget(target.to_string()))?;
                let setting = self.novels.get(id.into()).await?.and_then(|record| {
                    self.site_setting_for_url(&record.toc_url).or_else(|| {
                        Self::ageauth_redirect_target(&record.toc_url)
                            .and_then(|url| self.site_setting_for_url(&url))
                    })
                });
                let setting = setting.ok_or_else(|| {
                    NarouError::NotFound(format!("Novel not found for ID: {}", id))
                })?;
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
                    let record = self.novels.get(id.into()).await?.ok_or_else(|| {
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

    fn extract_ncode(&self, setting: &SiteSetting, toc_source: &str) -> Option<String> {
        let url_pattern = {
            let re = regex::Regex::new(r"(?i)[/?](n\d+[a-z]+)").ok()?;
            re.captures(&setting.toc_url())
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_lowercase())
        };
        url_pattern.or_else(|| setting.resolve_info_pattern("ncode", toc_source))
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
            let combined_title = format!("{} {}", ncode, title);
            let sanitized = sanitize_filename_with_limit(&combined_title, limit);
            if sanitized.is_empty() {
                ncode.clone()
            } else {
                sanitized
            }
        } else {
            sanitize_filename_with_limit(title, load_length_limit("folder-length-limit", Some(50)))
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
    pub fn clear_progress(&mut self) {
        self.progress = None;
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
    fn report_line(&self, line: &str) {
        use crate::application::messages::Stream;
        match &self.message_sink {
            Some(sink) => sink.emit(Stream::Stdout, line),
            None => default_report_line(line),
        }
    }

    /// `eprintln!` (stderr 行) 相当の送出。
    fn report_warn(&self, line: &str) {
        use crate::application::messages::Stream;
        match &self.message_sink {
            Some(sink) => sink.emit(Stream::Stderr, line),
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

/// A missing page (404) or a login wall served with HTTP 200 both mean the
/// fetch may need authentication; every other error is a real failure.
fn should_retry_with_login(setting: &SiteSetting, toc_source: &Result<String>) -> bool {
    match toc_source {
        Err(NarouError::NotFound(_)) => true,
        Ok(source) => setting
            .compiled_login_pattern()
            .is_some_and(|pattern| pattern.is_match(source)),
        Err(_) => false,
    }
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
    use crate::error::Result;
    use crate::platform::{NovelRepository, ObjectKey};
    use crate::platform::mocks::{
        FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore, MockHttpClient,
    };
    use crate::downloader::settings::DownloaderSettings;
    use crate::platform::{
        AssetStore, AssetStream, HttpResponse, NovelMutation, NovelObjectKeys, ObjectListRequest,
        ObjectMetadata, ObjectPrefix, ObjectStore, PlatformFuture, SystemClock,
    };
    use base64::Engine;
    use chrono::TimeZone;
    use std::collections::HashMap;
    use std::sync::Arc;

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
        let illustrations = Arc::new(MemoryObjectStore::new());
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
                novels: Arc::new(MemoryNovelRepository::new()),
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
                    resume_from_section: None,
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
                    resume_from_section: None,
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
        // `with_platform` loads サイト定義 from the working directory, so this
        // test must not overlap another one that changes it.
        let _state = crate::test_support::global_state_guard();
        let mut record = sample_record(chrono::Utc::now());
        record.id = 42;
        record.toc_url = "https://ncode.syosetu.com/n1234ab/".to_string();
        record.ncode = Some("n1234ab".to_string());
        record.is_narou = true;

        let repository = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let downloader = Downloader::with_platform(
            Arc::new(MockHttpClient::new()),
            Arc::new(FakeRateLimiter::new()),
            repository,
        )
        .unwrap();

        let (id, setting) =
            futures::executor::block_on(downloader.resolve_target_for_download("N1234AB")).unwrap();
        assert_eq!(id, Some(42));
        assert_eq!(setting.domain, "ncode.syosetu.com");
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
                novels: Arc::new(MemoryNovelRepository::from_records(vec![hameln, narou])),
                objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
            },
            sites, HashMap::new(),
        ).unwrap();
        for target in [
            "https://syosetu.org/novel/123456/", "https://h.syosetu.org/novel/123456/",
            "https://novel.syosetu.org/123456/", "https://syosetu.org/user/267675/", "42",
        ] {
            assert_eq!(downloader.site_key_for_target(target).await.unwrap(), "syosetu.org", "{target}");
        }
        for target in [
            "N1234AB", "43", "https://ncode.syosetu.com/n1234ab/",
            "https://mypage.syosetu.com/2842627/", "https://ncode.syosetu.com/s1234ab/",
        ] {
            assert_eq!(downloader.site_key_for_target(target).await.unwrap(), "ncode.syosetu.com", "{target}");
        }
        assert!(downloader.site_key_for_target("https://unknown.example/book/").await.is_err());
        assert!(http.requested_urls().is_empty(), "admission must never fetch a site or send cookies");
    }

    #[test]
    fn explicit_storage_constructor_avoids_native_loaders() {
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: Arc::new(MockHttpClient::new()),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new()),
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
        let assets: Arc<dyn AssetStore> = store.clone();
        let persistence =
            PersistenceService::with_clock(objects.clone(), assets, Arc::new(SystemClock));
        let keys = NovelObjectKeys::new("小説家になろう", "テスト作品", false).unwrap();
        let toc = TocFile {
            title: "テスト作品".to_string(),
            author: "作者".to_string(),
            toc_url: toc_url.to_string(),
            story: Some("あらすじ".to_string()),
            subtitles: subtitles.clone(),
            novel_type: Some(1),
        };
        persistence.save_toc(&keys, &toc).await.unwrap();
        persistence
            .save_section(&keys, &subtitles[0], &element)
            .await
            .unwrap();
        persistence
            .save_raw(&keys, &subtitles[0], &raw_html)
            .await
            .unwrap();
        assert_eq!(
            persistence.load_toc(&keys).await.unwrap().unwrap().title,
            "テスト作品"
        );
        assert_eq!(
            persistence
                .load_section(&keys, &subtitles[0])
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
        let assets: Arc<dyn AssetStore> = store;
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new()),
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
                    resume_from_section: None,
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
        let novels = Arc::new(MemoryNovelRepository::new());
        let persistence = PersistenceService::new(store.clone(), store.clone());
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
        assert!(read_diff_cache_catalog(store.as_ref(), keys.prefix(), &[]).await.unwrap().is_empty());
        let unchanged = downloader(1, None, 1, store.clone(), store.clone(), settings.clone())
            .download_novel(TARGET).await.unwrap();
        assert_eq!(unchanged.updated_count, 0);
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());
        assert!(downloader(2, Some(2), 2, store.clone(), store.clone(), settings.clone())
            .download_novel(TARGET).await.is_err());
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());

        let mut budget = YieldBefore(2);
        let partial = downloader(3, None, 3, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: Some(&mut budget), resume_from_section: None,
            }).await.unwrap_err();
        assert!(matches!(partial, crate::error::NarouError::DownloadBudgetExpired { next_section_index: 2 }));
        assert!(store.read_small(&keys.section("2", "第2話")).await.unwrap().is_some(),
            "the partial run really saved its new section");
        assert_eq!(persistence.load_toc(&keys).await.unwrap().unwrap().subtitles.len(), 1);
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());
        assert!(downloader(3, Some(3), 4, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: Some(2),
            }).await.is_err());
        assert!(downloader(3, None, 5, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: Some(4),
            }).await.is_err());
        assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());

        let completed = downloader(3, None, 6, store.clone(), store.clone(), settings.clone())
            .download_novel_with_execution_options(TARGET, DownloadExecutionOptions {
                force: false, budget: None, resume_from_section: Some(2),
            }).await.unwrap();
        assert_eq!(completed.id, initial.id);
        assert!(!completed.new_novel);
        assert_eq!(completed.updated_count, 1);
        assert_eq!(persistence.load_toc(&keys).await.unwrap().unwrap().subtitles.len(), 3);
        let versions = read_diff_cache_catalog(store.as_ref(), keys.prefix(), &[]).await.unwrap();
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
        assert_eq!(persistence.load_toc(&keys).await.unwrap().unwrap().subtitles.len(), 4);
        assert!(store.read_small(&keys.cached_version_marker(&version(9))).await.unwrap().is_none(),
            "successful section content alone does not complete an empty version");

        let updated = downloader(4, None, 10, store.clone(), store.clone(), settings.clone())
            .download_novel_with_force(TARGET, true).await.unwrap();
        assert_eq!(updated.updated_count, 4);
        let archived = read_diff_cache_catalog(store.as_ref(), keys.prefix(), &[]).await.unwrap();
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
        let novels = Arc::new(MemoryNovelRepository::new());
        let http = Arc::new(MockHttpClient::new());
        let persistence = PersistenceService::new(store.clone(), store.clone());
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
            let toc = persistence.load_toc(keys.as_ref().unwrap()).await.unwrap().unwrap();
            assert_eq!(toc.subtitles[0].subtitle, subtitle);
            let saved = persistence.load_section(keys.as_ref().unwrap(), &toc.subtitles[0]).await.unwrap().unwrap();
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
            let novels = Arc::new(MemoryNovelRepository::new());
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

        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Resume Test
domain: example.com
top_url: https://example.com
url: https?://example\.com/(?<ncode>n\d+[a-z]+)/?
sitename: Resume Test
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
        let toc_url = target;
        let section_1 = "https://example.com/n1234ab/1/";
        let section_2 = "https://example.com/n1234ab/2/";
        let toc = r#"
<h1>Resume Test</h1><span>Author</span>
<a href="/n1234ab/1/">第1話</a>
<a href="/n1234ab/2/">第2話</a>
"#;
        let http = Arc::new(MockHttpClient::new());
        http.add_text(toc_url, 200, toc);
        http.add_text(section_1, 200, r#"<div class="body">本文1</div>"#);
        http.add_text(section_2, 200, r#"<div class="body">本文2</div>"#);
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new()),
                objects,
                assets,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
        )
        .unwrap();

        let mut budget = YieldBeforeSection(1);
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: Some(&mut budget),
                    resume_from_section: None,
                },
            )
            .await
            .unwrap_err();
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

        downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: None,
                    resume_from_section: Some(1),
                },
            )
            .await
            .unwrap();
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
        let assets: Arc<dyn AssetStore> = store;
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new()),
                objects,
                assets,
                clock: Arc::new(SystemClock),
            },
            vec![setting],
            HashMap::new(),
        )
        .unwrap();

        // 1 回目: 第 1 話だけ保存して第 2 話の直前で予算により中断する。
        let mut budget = YieldBeforeSection(1);
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: Some(&mut budget),
                    resume_from_section: None,
                },
            )
            .await
            .unwrap_err();
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

        // 2 回目: 第 2 話が欠落したまま 2 話分の復元を要求する。
        let error = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: None,
                    resume_from_section: Some(2),
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
        let assets: Arc<dyn AssetStore> = store;
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new()),
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

        downloader.download_novel(target).await.unwrap();
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
        let mut budget = YieldBeforeSection(1);
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: None,
                },
            )
            .await
            .unwrap_err();
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
        let mut budget = YieldBeforeSection(1);
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: Some(1),
                },
            )
            .await
            .unwrap_err();
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
        let mut budget = YieldBeforeSection(2);
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: false,
                    budget: Some(&mut budget),
                    resume_from_section: Some(1),
                },
            )
            .await
            .unwrap();
        assert_eq!(partial.status, crate::downloader::types::UpdateStatus::Ok);
        assert_eq!(partial.updated_count, 1);
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
        let assets: Arc<dyn AssetStore> = store;
        let mut downloader = Downloader::with_platform_and_storage_and_settings(
            DownloaderPlatform {
                http: http.clone(),
                rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: Arc::new(MemoryNovelRepository::new()),
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
        let mut budget = YieldBeforeSection(1);
        let partial = downloader
            .download_novel_with_execution_options(
                target,
                DownloadExecutionOptions {
                    force: true,
                    budget: Some(&mut budget),
                    resume_from_section: None,
                },
            )
            .await
            .unwrap_err();
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
                    resume_from_section: Some(1),
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
        #[cfg(feature = "lite")]
        {
            let (_, unresolved) = crate::epub_lite::rewrite_illustration_references(
                "［＃挿絵（//9496.mitemin.net/userpageimage/viewimagebig/icode/i83860/）入る］",
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
                novels: Arc::new(MemoryNovelRepository::new()),
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
        let subtitles = super::toc::create_short_story_subtitles(setting, "", &info).unwrap();
        assert_eq!(subtitles[0].subdate, "2024-01-02 03:04");
        assert_eq!(subtitles[0].subupdate.as_deref(), Some("2024-01-05 06:07"));

        raw_captures.remove("gl");
        let info = NovelInfo {
            raw_captures,
            ..info
        };
        let subtitles = super::toc::create_short_story_subtitles(setting, "", &info).unwrap();
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
        let tags = setting.resolve_info_pattern("tags", &html).unwrap();
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
            super::resolve_novel_type(setting, &html, &url_captures, &pixiv_info(&html));
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
        super::novel_info::NovelInfo::from_novel_info_source(pixiv_setting(), html)
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

        let info = NovelInfo::from_novel_info_source(setting, html);

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

        let info = NovelInfo::from_novel_info_source(setting, html);

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

        let tags = setting.resolve_info_pattern("tags", html).unwrap();

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

        let short_story_tags = setting
            .resolve_info_pattern("tags", short_story_html)
            .unwrap();
        let series_tags = setting.resolve_info_pattern("tags", series_html).unwrap();

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

        let info = NovelInfo::from_toc_source(setting, html);

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

        let (novel_type, is_end) = resolve_novel_type(setting, toc_source, &HashMap::new(), &info);

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

        let (novel_type, is_end) = resolve_novel_type(setting, toc_source, &HashMap::new(), &info);

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

        let info = NovelInfo::from_toc_source(setting, html);

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
}
