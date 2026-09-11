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

#[cfg(feature = "native-runtime")]
use self::narou_api::narou_api_batch_update;
use self::novel_info::NovelInfo;
use self::persistence::section_filename;
use self::persistence::{PersistenceService, compute_section_hash};
use self::section::{SectionCache, download_section};
use self::security::is_safe_public_url;
use self::settings::DownloaderSettings;
use self::site_setting::SiteSetting;
use self::toc::{
    create_short_story_subtitles, fetch_toc, parse_subtitles, parse_subtitles_multipage,
};
use self::util::{
    build_section_url, compile_html_pattern, load_length_limit, mask_spoiler_text,
    sanitize_filename_with_limit,
};
use crate::db::novel_record::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, Clock, HttpClient, NovelMutation, NovelObjectKeys, NovelRepository, ObjectStore,
    ProgressReporter, RateLimiter,
};

pub use self::types::{
    ARCHIVE_ROOT_DIR, DownloadResult, NarouApiEntry, NarouApiResult, RAW_DATA_DIR,
    SECTION_SAVE_DIR, SectionElement, SectionFile, SubtitleInfo, TargetType, TocFile, TocObject,
    UpdateStatus,
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

#[derive(Clone, Copy)]
pub(crate) enum SiteTimezone {
    Named(Tz),
    Fixed(FixedOffset),
}

impl SiteTimezone {
    fn from_local_datetime(self, dt: NaiveDateTime) -> Option<DateTime<Utc>> {
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
    site_settings: Vec<SiteSetting>,
    section_cache: SectionCache,
    section_hash_cache: HashMap<String, HashMap<String, String>>,
    section_hash_cache_dirty: bool,
    settings: Arc<dyn DownloaderSettings>,
    progress: Option<Box<dyn ProgressReporter>>,
}

/// Report a status line. Native prints to stdout atomically; Worker routes to
/// the trace log (no terminal on the platform).
#[cfg(feature = "native-runtime")]
fn report_line(line: &str) {
    crate::progress::safe_println(line);
}

#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
fn report_line(line: &str) {
    tracing::debug!("{line}");
}

/// Report a warning line. Native prints to stderr; Worker routes to the trace
/// log.
#[cfg(feature = "native-runtime")]
fn report_warn(line: &str) {
    crate::progress::safe_stderr_println(line);
}

#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
fn report_warn(line: &str) {
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

fn ncode_target_url(target: &str) -> Option<String> {
    if matches!(Downloader::get_target_type(target), TargetType::Ncode) {
        Some(format!(
            "https://ncode.syosetu.com/{}/",
            target.to_lowercase()
        ))
    } else {
        None
    }
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

fn cache_timestamp(clock: &dyn Clock) -> Option<String> {
    #[cfg(feature = "native-runtime")]
    {
        if crate::compat::load_local_setting_list("economy")
            .iter()
            .any(|v| v == "nosave_diff")
        {
            return None;
        }
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
            return timezone.from_local_datetime(dt);
        }
        if let Ok(date) = NaiveDate::parse_from_str(&value, fmt) {
            return date
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| timezone.from_local_datetime(dt));
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
/// `random`. Native-only: requires the `ua_generator` randomizer; Worker
/// builds inject their own user agent through the platform `HttpClient`.
#[cfg(feature = "native-runtime")]
pub(crate) fn resolve_user_agent(
    user_agent: Option<&str>,
    saved_user_agent: Option<String>,
) -> String {
    let is_auto = |ua: &str| {
        let trimmed = ua.trim();
        trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("auto")
            || trimmed.eq_ignore_ascii_case("random")
    };
    match user_agent {
        Some(ua) if is_auto(ua) => ua_generator::ua::spoof_firefox_ua().to_string(),
        Some(ua) if !ua.trim().is_empty() => ua.to_string(),
        _ => match saved_user_agent {
            Some(ua) if is_auto(&ua) => ua_generator::ua::spoof_firefox_ua().to_string(),
            Some(ua) if !ua.trim().is_empty() => ua,
            _ => ua_generator::ua::spoof_firefox_ua().to_string(),
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

fn section_timestamp_ymd(download_time: Option<&str>, timezone: SiteTimezone) -> Option<String> {
    download_time.and_then(|value| date_string_to_ymd_with_timezone(value, timezone))
}

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

impl Downloader {
    /// Native constructor: loads site definitions from the filesystem and the
    /// section hash cache from the Inventory. Worker builds use
    /// [`Self::with_platform_and_storage_and_settings`] with bundled values.
    #[cfg(feature = "native-runtime")]
    pub fn with_platform_and_storage(
        http: Arc<dyn HttpClient>,
        rate_limiter: Arc<dyn RateLimiter>,
        novels: Arc<dyn NovelRepository>,
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let site_settings = SiteSetting::load_all()?;
        let settings = crate::downloader::settings::default_settings();
        let section_hash_cache = settings.load_section_hash_cache();
        Self::with_platform_and_storage_and_settings(
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            clock,
            site_settings,
            section_hash_cache,
        )
    }

    /// Build the downloader without loading site definitions or Inventory.
    ///
    /// Worker callers and filesystem-free tests supply both values explicitly.
    pub fn with_platform_and_storage_and_settings(
        http: Arc<dyn HttpClient>,
        rate_limiter: Arc<dyn RateLimiter>,
        novels: Arc<dyn NovelRepository>,
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        clock: Arc<dyn Clock>,
        site_settings: Vec<SiteSetting>,
        section_hash_cache: HashMap<String, HashMap<String, String>>,
    ) -> Result<Self> {
        Self::with_platform_and_storage_and_settings_and_support(
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            clock,
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
        http: Arc<dyn HttpClient>,
        rate_limiter: Arc<dyn RateLimiter>,
        novels: Arc<dyn NovelRepository>,
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        clock: Arc<dyn Clock>,
        site_settings: Vec<SiteSetting>,
        section_hash_cache: HashMap<String, HashMap<String, String>>,
        settings: Arc<dyn DownloaderSettings>,
    ) -> Result<Self> {
        let persistence =
            PersistenceService::with_clock(objects.clone(), assets.clone(), clock.clone());

        Ok(Self {
            http,
            rate_limiter,
            novels,
            assets,
            clock,
            persistence,
            site_settings,
            section_cache: SectionCache::new(),
            section_hash_cache,
            section_hash_cache_dirty: false,
            settings,
            progress: None,
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
        for setting in &self.site_settings {
            if setting.matches_url(url) {
                return Some(setting.clone());
            }
        }
        None
    }

    async fn load_novel_info(
        &self,
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
                parse_subtitles_multipage(
                    self.http.as_ref(),
                    self.rate_limiter.as_ref(),
                    &setting,
                    &toc_source,
                    &url_captures,
                    title,
                    self.progress.as_deref(),
                )
                .await
                .ok()
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

    async fn process_digest(
        &self,
        existing_id: Option<i64>,
        toc_url: &str,
        novel_dir: &Path,
        title: &str,
        latest_story: &str,
        old_count: usize,
        latest_count: usize,
    ) -> Result<bool> {
        #[cfg(feature = "native-runtime")]
        {
            if latest_count >= old_count {
                return Ok(false);
            }

            let mut message = format!(
                "更新後の話数が保存されている話数より減少していることを検知しました。\nダイジェスト化されている可能性があるので、更新に関しての処理を選択して下さい。\n\n保存済み話数: {}\n更新後の話数: {}\n\n",
                old_count, latest_count
            );

            let mut auto_choices = crate::compat::load_digest_auto_choices();

            loop {
                match crate::compat::choose_digest_action_with_auto_choices(
                    title,
                    &message,
                    &mut auto_choices,
                ) {
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
                        println!("{} を作成しました", backup_name);
                    }
                    crate::compat::DigestChoice::ShowStory => {
                        println!("あらすじ");
                        println!("{}", latest_story);
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

    async fn download_illustration(
        &mut self,
        setting: &SiteSetting,
        section: &SectionElement,
        object_keys: &NovelObjectKeys,
        illustration_store: &mut crate::illustration_store::IllustrationIndex,
        toc_url: &str,
        raw_html: Option<&str>,
    ) -> Result<()> {
        let illust_url_pattern = match &setting.illust_grep_pattern {
            Some(p) => p,
            None => return Ok(()),
        };

        let re = compile_html_pattern(illust_url_pattern).map_err(NarouError::Regex)?;
        let storage =
            crate::illustration_store::IllustrationStorageService::new(self.assets.clone());

        // Parsed sections may omit site-specific image markup. Search the raw
        // HTML from this fetch as well, while keeping all persistence logical.
        for source in illustration_sources(section, raw_html) {
            for caps in re.captures_iter(source) {
                let Some(url_match) = caps.get(1) else {
                    continue;
                };
                let raw_url = url_match.as_str();
                if raw_url.is_empty() {
                    continue;
                }
                let resolved = build_section_url(setting, toc_url, raw_url);
                let url = resolved.as_str();
                if !is_safe_public_url(url) {
                    report_warn(&format!("WARN: skipping unsafe illustration URL: {url}"));
                    continue;
                }

                if illustration_store.filename_for_source(url).is_some()
                    || crate::illustration_store::mitemin_illustration_id(url)
                        .and_then(|id| illustration_store.filename_for_mitemin_id(&id))
                        .is_some()
                {
                    continue;
                }

                match crate::downloader::http_policy::fetch_bytes(
                    self.http.as_ref(),
                    self.rate_limiter.as_ref(),
                    url,
                    None,
                    setting.is_narou,
                )
                .await
                {
                    Ok(response) => {
                        let content_type =
                            response.header("Content-Type").unwrap_or("").to_string();
                        let ext =
                            crate::illustration_store::illustration_extension_from_content_type(
                                &content_type,
                            )
                            .unwrap_or_else(|| {
                                crate::illustration_store::guessed_extension_from_url(url)
                            });
                        if let Err(err) = storage
                            .store_bytes(object_keys, illustration_store, url, &response.body, ext)
                            .await
                        {
                            report_warn(&format!(
                                "WARN: failed to save illustration {url}: {err}"
                            ));
                        }
                    }
                    Err(err) => {
                        report_warn(&format!(
                            "WARN: failed to download illustration {url}: {err}"
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn expand_series_target(&mut self, target: &str) -> Result<Option<Vec<String>>> {
        if !matches!(Self::get_target_type(target), TargetType::Url) {
            return Ok(None);
        }
        let setting = self
            .site_settings
            .iter()
            .find(|setting| setting.matches_series_url(target))
            .cloned();
        let Some(setting) = setting else {
            return Ok(None);
        };
        let pattern = setting.compile_series_item_pattern().ok_or_else(|| {
            NarouError::SiteSetting(format!(
                "No series_item_url pattern defined: {}",
                setting.name
            ))
        })?;

        let mut body = crate::downloader::http_policy::fetch_text(
            self.http.as_ref(),
            self.rate_limiter.as_ref(),
            target,
            setting.cookie(),
            Some(setting.encoding()),
            setting.is_narou,
        )
        .await?;
        crate::downloader::util::pretreatment_source(&mut body, setting.encoding(), None);

        let mut targets = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for caps in pattern.captures_iter(&body) {
            if let Some(url) = setting.series_item_url_from_captures(target, &pattern, &caps)
                && seen.insert(url.clone())
            {
                targets.push(url);
            }
        }

        if targets.is_empty() {
            return Err(NarouError::NotFound(format!(
                "No novels found in series URL: {}",
                target
            )));
        }
        Ok(Some(targets))
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
        self.download_novel_with_force_budget(target, force, None).await
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
        mut options: DownloadExecutionOptions<'_>,
    ) -> Result<DownloadResult> {
        let force = options.force;
        let resume_from_section = options.resume_from_section;
        let mut budget = options.budget.take();
        let (existing_id, mut setting) = self.resolve_target_for_download(target).await?;
        let provisional_id = match existing_id {
            Some(id) => id,
            None => self.novels.allocate_id().await?.0,
        };

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
            setting.interpolate(&setting.toc_url)
        } else {
            setting.interpolate_with_captures(&setting.toc_url, &url_captures)
        };
        let toc_url = Self::ageauth_redirect_target(&toc_url).unwrap_or(toc_url);
        let (resolved_toc_url, prefetched_toc) =
            match crate::downloader::http_policy::resolve_final_url_with_body(
                self.http.as_ref(),
                self.rate_limiter.as_ref(),
                &toc_url,
                setting.cookie(),
                setting.is_narou,
            )
            .await
            {
                Ok((final_url, _body)) if final_url != toc_url => {
                    let final_url =
                        Self::ageauth_redirect_target(&final_url).unwrap_or(final_url);
                    if let Some(final_setting) = self.find_site_setting(&final_url) {
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
                let response = crate::downloader::http_policy::ensure_success_response(
                    &toc_url, response,
                );
                match response {
                    Ok(response) => {
                        let mut body = crate::downloader::http_policy::decode_with_encoding(
                            &response.body,
                            Some(setting.encoding()),
                        );
                        pretreatment_source(&mut body, setting.encoding(), Some(&setting));
                        if let Some(re) = setting.compiled_error_message_pattern()
                            && re.is_match(&body)
                        {
                            return Err(NarouError::NotFound(
                                "Novel deleted or private".into(),
                            ));
                        }
                        Ok(body)
                    }
                    Err(error) => Err(error),
                }
            }
            None => fetch_toc(
                self.http.as_ref(),
                self.rate_limiter.as_ref(),
                &setting,
                &toc_url,
            )
            .await,
        };
        let toc_source = match toc_source {
            Ok(source) => source,
            Err(NarouError::NotFound(_)) if existing_id.is_some() => {
                let id = existing_id.unwrap();
                report_line("小説が削除されているか非公開な可能性があります");
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

        let info = self
            .load_novel_info(&setting, &toc_source, &url_captures, &toc_url)
            .await?;

        let author = info.author.clone().unwrap_or_default();
        let existing_record = match existing_id {
            Some(eid) => self.novels.get(eid.into()).await.ok().flatten(),
            None => None,
        };
        let fetched_raw_title = info.title.clone().unwrap_or_default();
        let raw_title = if fetched_raw_title.is_empty() {
            existing_record
                .as_ref()
                .map(|record| record.raw_title().to_string())
                .unwrap_or_default()
        } else {
            fetched_raw_title
        };
        #[cfg(feature = "native-runtime")]
        let previous_novel_dir = existing_record.as_ref().map(|record| {
            crate::db::existing_novel_dir_for_record(Path::new(types::ARCHIVE_ROOT_DIR), record)
        });
        let strip_title_prefix = self
            .settings
            .strip_title_prefix_for(provisional_id, &raw_title, &author);
        let track_raw_title = strip_title_prefix
            || existing_record
                .as_ref()
                .is_some_and(NovelRecord::has_raw_title);
        let title = crate::title::project_title(&raw_title, strip_title_prefix);

        let (novel_type, is_end) = resolve_novel_type(&setting, &toc_source, &url_captures, &info);

        let subtitles = if novel_type == 2 {
            create_short_story_subtitles(&setting, &toc_source, &info)?
        } else {
            parse_subtitles_multipage(
                self.http.as_ref(),
                self.rate_limiter.as_ref(),
                &setting,
                &toc_source,
                &url_captures,
                &title,
                self.progress.as_deref(),
            )
            .await?
        };

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
        let sitename = info
            .sitename
            .clone()
            .or_else(|| {
                existing_record
                    .as_ref()
                    .filter(|r| r.domain.as_deref() == Some(setting.domain.as_str()))
                    .and_then(|r| {
                        if r.sitename.is_empty() {
                            None
                        } else {
                            Some(r.sitename.clone())
                        }
                    })
            })
            .unwrap_or_else(|| setting.sitename.clone());

        let object_keys = NovelObjectKeys::new(&sitename, &file_title, use_subdirectory)
            .map_err(|error| NarouError::Platform(error.to_string()))?;
        let novel_dir = self.compute_novel_dir(&sitename, &file_title, use_subdirectory);
        #[cfg(feature = "native-runtime")]
        if let Some(existing) = existing_record.as_ref() {
            crate::title::rename_projected_outputs(&novel_dir, &author, &existing.title, &title)?;
        }

        let old_toc = self
            .persistence
            .load_toc(&object_keys)
            .await
            .unwrap_or(None);
        let old_subtitles: HashMap<String, &SubtitleInfo> = old_toc
            .as_ref()
            .map(|t| t.subtitles.iter().map(|s| (s.index.clone(), s)).collect())
            .unwrap_or_default();

        let old_title = old_toc.as_ref().map(|t| t.title.clone());
        let old_author = old_toc.as_ref().map(|t| t.author.clone());
        let old_story = old_toc.as_ref().and_then(|t| t.story.clone());
        let old_section_count = old_toc.as_ref().map(|t| t.subtitles.len()).unwrap_or(0);

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
                .process_digest(
                    existing_id,
                    &toc_url,
                    &novel_dir,
                    &title_for_digest,
                    &digest_story,
                    old_section_count,
                    subtitles.len(),
                )
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

        let mut illustration_store = if setting.illust_grep_pattern.is_some() {
            Some(
                self.persistence
                    .load_cache(&object_keys)
                    .await?
                    .unwrap_or_default(),
            )
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
        let mut resumed_sections = HashMap::new();
        for (section_index, subtitle) in subtitles.iter().enumerate().take(resume_from) {
            let section = self
                .persistence
                .load_section(&object_keys, subtitle)
                .await?
                .ok_or_else(|| {
                    NarouError::DownloadResumeCorrupt(format!(
                        "persisted section {section_index} ({}) is missing",
                        subtitle.index
                    ))
                })?;
            resumed_sections.insert(section_index, section);
        }

        // One listing pass replaces a per-section existence read. On the
        // Worker this is the difference between ⌈N/1000⌉ LIST subrequests
        // and N GETs; on native it avoids N read+parse round trips.
        let existing_names = self
            .persistence
            .list_novel_object_names(&object_keys)
            .await?;
        let existing_sections =
            PersistenceService::existing_section_indices(&existing_names);

        struct SectionPlan {
            is_new_arrival: bool,
            needs_download: bool,
            /// The section file existed before this run (canonical or legacy
            /// name). Drives the cache move without a second read.
            existed: bool,
            predownloaded: Option<(SectionElement, String)>,
            resumed: Option<SectionFile>,
        }

        let mut updated_count = 0usize;
        let mut new_arrivals = existing_id.is_none();
        let mut new_arrival_subtitles = Vec::new();
        let mut final_subtitles = Vec::with_capacity(subtitles.len());
        let strong_update = self.settings.local_setting_bool("update.strong");
        let site_timezone = setting.site_timezone();
        let mut cache_dir: Option<String> = None;
        let mut pending_section_hashes: HashMap<String, String> = HashMap::new();
        let display_id = provisional_id;
        let mut section_plans = Vec::with_capacity(subtitles.len());
        let mut download_count = 0usize;
        let guard_spoiler = self.settings.local_setting_bool("guard-spoiler");
        for (section_index, subtitle) in subtitles.iter().enumerate() {
            let resumed = resumed_sections.remove(&section_index);
            let is_new_arrival =
                resumed.is_none() && old_subtitles.get(&subtitle.index).is_none();
            let existed = existing_sections.contains(&subtitle.index);
            let (needs_download, predownloaded) = if resumed.is_some() {
                (false, None)
            } else if force {
                (true, None)
            } else {
                self.section_needs_download(
                    &setting,
                    subtitle,
                    old_subtitles.get(&subtitle.index).copied(),
                    existing_id,
                    &object_keys,
                    &toc_url,
                    strong_update,
                    site_timezone,
                    existed,
                )
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

        for (section_index, (subtitle, plan)) in subtitles
            .iter()
            .zip(section_plans.into_iter())
            .enumerate()
        {
            if section_index >= resume_from
                && budget
                    .as_deref_mut()
                    .is_some_and(|budget| budget.should_yield(section_index))
            {
                return Err(NarouError::DownloadBudgetExpired {
                    next_section_index: section_index,
                });
            }
            let is_new_arrival = plan.is_new_arrival;
            let needs_download = plan.needs_download;

            let download_time = if let Some(resumed) = plan.resumed {
                resumed.download_time
            } else if needs_download {
                if !started_download {
                    report_line(&colorize(
                        &format!("ID:{display_id}　{title} のDL開始"),
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
                    report_line(&subtitle.chapter);
                    last_chapter = subtitle.chapter.clone();
                }
                if !subtitle.subchapter.is_empty() && subtitle.subchapter != last_subchapter {
                    report_line(&subtitle.subchapter);
                    last_subchapter = subtitle.subchapter.clone();
                }

                let (section, raw_html) = if let Some(downloaded) = plan.predownloaded {
                    downloaded
                } else {
                    download_section(
                        self.http.as_ref(),
                        self.rate_limiter.as_ref(),
                        &mut self.section_cache,
                        &setting,
                        subtitle,
                        &toc_url,
                    )
                    .await?
                };
                let had_existing = plan.existed;
                if had_existing {
                    if cache_dir.is_none() {
                        cache_dir = cache_timestamp(self.clock.as_ref());
                    }
                    if let Some(id) = existing_id {
                        self.clear_section_digest(id, &section_relative_path(subtitle));
                    }
                    if let Some(timestamp) = cache_dir.as_deref() {
                        self.persistence
                            .move_section_to_cache(&object_keys, timestamp, subtitle)
                            .await?;
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
                self.persistence
                    .save_raw(&object_keys, subtitle, &raw_html)
                    .await?;
                if let Some(store) = illustration_store.as_mut() {
                    self.download_illustration(
                        &setting,
                        &section,
                        &object_keys,
                        store,
                        &toc_url,
                        Some(raw_html.as_str()),
                    )
                    .await?;
                }
                updated_count += 1;
                downloaded_index += 1;
                Some(download_time)
            } else {
                if let Some(store) = illustration_store.as_mut()
                    && let Some(section_file) = self
                        .persistence
                        .load_section(&object_keys, subtitle)
                        .await?
                {
                    self.download_illustration(
                        &setting,
                        &section_file.element,
                        &object_keys,
                        store,
                        &toc_url,
                        None,
                    )
                    .await?;
                }
                old_subtitles
                    .get(&subtitle.index)
                    .and_then(|old| old.download_time.clone())
            };

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
                report_line(&line);
                if let Some(ref p) = self.progress {
                    p.inc(1);
                }
            }
        }

        if let Some(store) = illustration_store.as_ref() {
            self.persistence.save_cache(&object_keys, store).await?;
        }

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
            extra_fields: Default::default(),
        };
        if track_raw_title {
            record.set_raw_title(raw_title);
        }

        let auto_add_tags = self.settings.local_setting_bool("auto-add-tags");
        let auto_tags = if auto_add_tags {
            info.tags
                .clone()
                .or_else(|| setting.resolve_info_pattern("tags", &toc_source))
                .map(|raw_tags| sanitize_site_tags(&raw_tags))
                .unwrap_or_default()
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
            // 新規: コミット時点で一意 ID を予約する (従来の max+1 と同セマンティクス)。
            let new_id = self.novels.allocate_id().await?.0;
            let mut rec = record.clone();
            rec.id = new_id;
            rec.tags = auto_tags.clone();
            self.novels
                .apply_batch(vec![NovelMutation::Upsert(rec)])
                .await?;
            new_id
        };

        #[cfg(feature = "native-runtime")]
        self.remove_migrated_novel_dir(previous_novel_dir.as_deref(), &novel_dir)
            .await;

        if let Some(old_id) = existing_id {
            self.move_section_hash_bucket(old_id, id);
        } else {
            for (relative_path, digest) in pending_section_hashes {
                self.store_section_digest(id, &relative_path, &digest);
            }
        }
        self.flush_section_hash_cache()?;

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

    async fn section_needs_download(
        &mut self,
        setting: &SiteSetting,
        latest: &SubtitleInfo,
        old: Option<&SubtitleInfo>,
        existing_id: Option<i64>,
        keys: &NovelObjectKeys,
        toc_url: &str,
        strong_update: bool,
        timezone: SiteTimezone,
        existed: bool,
    ) -> Result<(bool, Option<(SectionElement, String)>)> {
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

        if strong_update
            && let Some(basis_date) = strong_basis_date
        {
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
                    return Ok((false, None));
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
        is_safe_public_url(&target).then_some(target)
    }

    async fn resolve_target_for_download(
        &self,
        target: &str,
    ) -> Result<(Option<i64>, SiteSetting)> {
        let target_type = Self::get_target_type(target);

        match target_type {
            TargetType::Url => {
                let setting = self.find_site_setting(target).ok_or_else(|| {
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
                let existing_id = self
                    .novels
                    .find_by_ncode(&ncode)
                    .await?
                    .map(|record| record.id);
                if let Some(id) = existing_id {
                    let record = self.novels.get(id.into()).await?.ok_or_else(|| {
                        NarouError::NotFound(format!("Novel record {} has no toc_url", id))
                    })?;
                    let setting = self.find_site_setting(&record.toc_url).ok_or_else(|| {
                        NarouError::SiteSetting("No matching site setting".into())
                    })?;
                    Ok((Some(id), setting))
                } else {
                    let narou_url = format!("https://ncode.syosetu.com/{}/", ncode);
                    let setting = self.find_site_setting(&narou_url).ok_or_else(|| {
                        NarouError::InvalidTarget(format!("対応外のncodeです({})", ncode))
                    })?;
                    let toc_url = setting
                        .toc_url_with_url_captures(&narou_url)
                        .unwrap_or_else(|| setting.toc_url());
                    let existing_id = self
                        .novels
                        .find_by_toc_url(&toc_url)
                        .await?
                        .map(|record| record.id);
                    Ok((existing_id, setting))
                }
            }
            TargetType::Id => {
                let id: i64 = target
                    .parse()
                    .map_err(|_| NarouError::InvalidTarget(target.to_string()))?;
                let setting = self.novels.get(id.into()).await?.and_then(|record| {
                    self.find_site_setting(&record.toc_url).or_else(|| {
                        Self::ageauth_redirect_target(&record.toc_url)
                            .and_then(|url| self.find_site_setting(&url))
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
                    .map(|record| record.id);
                if let Some(id) = existing_id {
                    let record = self.novels.get(id.into()).await?.ok_or_else(|| {
                        NarouError::NotFound(format!("Novel record {} has no toc_url", id))
                    })?;
                    let setting = self.find_site_setting(&record.toc_url).ok_or_else(|| {
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
        if let Some(record) = existing_record {
            if !record.file_title.is_empty() {
                if append_title
                    && ncode
                        .as_deref()
                        .is_some_and(|ncode| record.file_title.eq_ignore_ascii_case(ncode))
                    && !title.is_empty()
                {
                    // Recover records created before the correct site/title was known.
                } else {
                    return record.file_title.clone();
                }
            }
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
        crate::db::paths::novel_dir_from_components(
            &root,
            sitename,
            file_title,
            use_subdirectory,
        )
    }

    /// Remove the previous novel directory after a title change, but only when
    /// no other record still references it. Native filesystem compatibility;
    /// Worker builds have no filesystem to migrate.
    #[cfg(feature = "native-runtime")]
    async fn remove_migrated_novel_dir(&self, previous: Option<&Path>, current: &Path) {
        let Some(previous) = previous else {
            return;
        };
        if previous == current || !previous.exists() {
            return;
        }

        let filter = crate::platform::NovelFilter::all();
        let mut after_id = None;
        let mut still_referenced = false;
        loop {
            let ids = match self.novels.scan_ids(&filter, after_id, 256).await {
                Ok(ids) => ids,
                Err(_) => {
                    still_referenced = true;
                    break;
                }
            };
            if ids.is_empty() {
                break;
            }
            after_id = ids.last().copied();
            for id in ids {
                match self.novels.get(id).await {
                    Ok(Some(record)) => {
                        if crate::db::novel_dir_for_record(
                            Path::new(types::ARCHIVE_ROOT_DIR),
                            &record,
                        ) == previous
                        {
                            still_referenced = true;
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        still_referenced = true;
                        break;
                    }
                }
            }
            if still_referenced {
                break;
            }
        }
        if !still_referenced {
            let _ = std::fs::remove_dir_all(previous);
        }
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
        if let Some(bucket) = self.section_hash_cache.remove(&from_id.to_string()) {
            if !bucket.is_empty() {
                self.section_hash_cache.insert(to_id.to_string(), bucket);
                self.section_hash_cache_dirty = true;
            }
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

    pub fn site_setting_matches_url(&self, url: &str) -> bool {
        self.find_site_setting(url).is_some()
    }

    /// Batch-refresh なろうAPI metadata for all なろう records. Uses the
    /// native dedicated API rate limiter; Worker builds schedule their own
    /// batch jobs.
    #[cfg(feature = "native-runtime")]
    pub async fn narou_api_batch_update(&mut self) -> Result<(usize, usize)> {
        narou_api_batch_update(self.http.as_ref(), self.novels.as_ref()).await
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
        DownloadExecutionOptions, Downloader, Over18AccessDecision, SectionBudget,
        illustration_sources, over18_access_decision, requires_over18_confirmation,
        resolve_novel_type, resolve_user_agent,
    };
    use crate::db::{self, Database};
    use crate::platform::NovelRepository;
    use crate::platform::mocks::{
        FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore, MockHttpClient,
    };
    use crate::platform::{AssetStore, NovelMutation, NovelObjectKeys, ObjectStore, SystemClock};
    use chrono::TimeZone;
    use std::collections::HashMap;
    use std::path::PathBuf;
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

    #[test]
    fn explicit_storage_constructor_avoids_native_loaders() {
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let downloader = Downloader::with_platform_and_storage_and_settings(
            Arc::new(MockHttpClient::new()),
            Arc::new(FakeRateLimiter::new()),
            Arc::new(MemoryNovelRepository::new()),
            objects,
            assets,
            Arc::new(SystemClock),
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

        let toc_source =
            super::toc::fetch_toc(http.as_ref(), &FakeRateLimiter::new(), &setting, toc_url)
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

    #[tokio::test]
    async fn resumable_download_reuses_persisted_prefix_without_refetching() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::new().unwrap();
        let mut db_slot = db::DATABASE.lock();
        let previous_db = db_slot.take();
        *db_slot = Some(db);
        drop(db_slot);
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
            http.clone(),
            Arc::new(FakeRateLimiter::new()),
            Arc::new(MemoryNovelRepository::new()),
            objects,
            assets,
            Arc::new(SystemClock),
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

    #[test]
    fn remove_migrated_novel_dir_keeps_previous_dir_when_still_referenced() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());

        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();

        let db = Database::new().unwrap();
        let mut db_slot = db::DATABASE.lock();
        let previous_db = db_slot.take();
        *db_slot = Some(db);
        drop(db_slot);
        let _db_guard = DatabaseGuard(previous_db);

        let downloader = Downloader::with_user_agent(None).unwrap();
        let timestamp = chrono::Utc::now();
        let mut previous_record = sample_record(timestamp);
        previous_record.id = 1;
        previous_record.sitename = "site".to_string();
        previous_record.file_title = "old".to_string();
        let mut referenced_record = sample_record(timestamp);
        referenced_record.id = 2;
        referenced_record.sitename = "site".to_string();
        referenced_record.file_title = "old".to_string();
        let mut current_record = sample_record(timestamp);
        current_record.id = 3;
        current_record.sitename = "site".to_string();
        current_record.file_title = "new".to_string();

        let previous = db::paths::novel_dir_for_record(
            &PathBuf::from(super::types::ARCHIVE_ROOT_DIR),
            &previous_record,
        );
        let current = db::paths::novel_dir_for_record(
            &PathBuf::from(super::types::ARCHIVE_ROOT_DIR),
            &current_record,
        );
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        {
            let mut db_slot = db::DATABASE.lock();
            let db = db_slot.as_mut().unwrap();
            db.insert(previous_record);
            db.insert(referenced_record);
            db.insert(current_record);
        }

        futures::executor::block_on(
            downloader.remove_migrated_novel_dir(Some(&previous), &current),
        );
        assert!(previous.exists());
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
