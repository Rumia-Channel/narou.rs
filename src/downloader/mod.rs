pub mod fetch;
pub mod html;
pub mod info_cache;
pub mod narou_api;
pub mod novel_info;
pub mod persistence;
pub mod preprocess;
pub mod rate_limit;
pub mod section;
pub mod site_setting;
pub mod toc;
pub mod types;
pub mod util;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};
use regex::Regex;

use crate::db::DATABASE;
use crate::db::novel_record::NovelRecord;
use crate::error::{NarouError, Result};
use crate::progress::ProgressReporter;

use self::fetch::HttpFetcher;
use self::narou_api::narou_api_batch_update;
use self::novel_info::NovelInfo;
use self::persistence::{
    compute_section_hash, ensure_default_files, load_section_file, load_toc_file, move_file_to_dir,
    remove_dir_if_empty, save_raw_file, save_section_file, save_toc_file,
};
use self::section::{SectionCache, download_section};
use self::site_setting::SiteSetting;
use self::toc::{create_short_story_subtitles, fetch_toc, parse_subtitles_multipage};
use self::util::sanitize_filename;

pub use self::types::{
    ARCHIVE_ROOT_DIR, DownloadResult, NarouApiEntry, NarouApiResult, RAW_DATA_DIR,
    SECTION_SAVE_DIR, SectionElement, SectionFile, SubtitleInfo, TargetType, TocFile, TocObject,
    UpdateStatus,
};
pub use self::util::pretreatment_source;

const SECTION_HASH_CACHE_NAME: &str = "section_hash_cache";

pub struct Downloader {
    fetcher: HttpFetcher,
    site_settings: Vec<SiteSetting>,
    section_cache: SectionCache,
    section_hash_cache: HashMap<String, HashMap<String, String>>,
    section_hash_cache_dirty: bool,
    progress: Option<Box<dyn ProgressReporter>>,
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

fn load_local_setting_bool(key: &str) -> bool {
    crate::compat::load_local_setting_bool(key)
}

fn load_local_setting_string(key: &str) -> Option<String> {
    crate::compat::load_local_setting_string(key)
}

fn load_global_setting_bool(key: &str) -> bool {
    crate::db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db.inventory().load(
            "global_setting",
            crate::db::inventory::InventoryScope::Global,
        )?;
        Ok(settings.get(key).and_then(|value| match value {
            serde_yaml::Value::Bool(v) => Some(*v),
            serde_yaml::Value::String(v) => Some(matches!(v.as_str(), "true" | "yes" | "on" | "1")),
            serde_yaml::Value::Number(v) => Some(v.as_i64().unwrap_or(0) != 0),
            _ => None,
        }))
    })
    .ok()
    .flatten()
    .unwrap_or(false)
}

fn save_global_setting_bool(key: &str, value: bool) -> Result<()> {
    crate::db::with_database_mut(|db| {
        let mut settings: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load(
                "global_setting",
                crate::db::inventory::InventoryScope::Global,
            )
            .unwrap_or_default();
        settings.insert(key.to_string(), serde_yaml::Value::Bool(value));
        db.inventory().save(
            "global_setting",
            crate::db::inventory::InventoryScope::Global,
            &settings,
        )?;
        Ok(())
    })
}

fn section_filename(subtitle: &SubtitleInfo) -> String {
    format!("{} {}.yaml", subtitle.index, subtitle.file_subtitle)
}

fn section_relative_path(subtitle: &SubtitleInfo) -> String {
    PathBuf::from(types::SECTION_SAVE_DIR)
        .join(section_filename(subtitle))
        .to_string_lossy()
        .to_string()
}

fn create_cache_dir(section_dir: &Path) -> Result<Option<PathBuf>> {
    if crate::compat::load_local_setting_list("economy")
        .iter()
        .any(|v| v == "nosave_diff")
    {
        return Ok(None);
    }
    let cache_dir = section_dir
        .join(types::CACHE_SAVE_DIR)
        .join(chrono::Local::now().format("%Y.%m.%d@%H.%M.%S").to_string());
    std::fs::create_dir_all(&cache_dir)?;
    Ok(Some(cache_dir))
}

fn move_to_cache_dir(
    section_dir: &Path,
    cache_dir: Option<&Path>,
    subtitle: &SubtitleInfo,
) -> Result<()> {
    let Some(cache_dir) = cache_dir else {
        return Ok(());
    };
    let path = section_dir.join(section_filename(subtitle));
    move_file_to_dir(&path, cache_dir)
}

fn remove_cache_dir_if_empty(cache_dir: Option<&Path>) -> Result<()> {
    if let Some(cache_dir) = cache_dir {
        remove_dir_if_empty(cache_dir)?;
    }
    Ok(())
}

fn sections_latest_update_time(
    subtitles: &[SubtitleInfo],
    key: &str,
    subkey: Option<&str>,
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
        let Some(parsed) = parse_loose_datetime(value) else {
            continue;
        };
        if latest.is_none_or(|current| parsed > current) {
            latest = Some(parsed);
        }
    }
    latest
}

fn parse_loose_datetime(value: &str) -> Option<DateTime<Utc>> {
    let mut value = value.trim().to_string();
    if value.is_empty() {
        return None;
    }

    value = value
        .replace('年', "/")
        .replace('月', "/")
        .replace('日', "")
        .replace('時', ":")
        .replace('分', ":")
        .replace('秒', "");
    value = value.trim().trim_end_matches(':').to_string();

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
            return Some(dt.and_utc());
        }
        if let Ok(date) = NaiveDate::parse_from_str(&value, fmt) {
            return date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        }
    }

    None
}

fn date_string_is_newer(latest: &str, old: &str) -> bool {
    match (parse_loose_datetime(latest), parse_loose_datetime(old)) {
        (Some(latest_dt), Some(old_dt)) => latest_dt > old_dt,
        _ => latest > old,
    }
}

fn date_string_to_ymd(value: &str) -> Option<String> {
    let dt = parse_loose_datetime(value)?;
    Some(format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()))
}

fn sanitize_site_tags(raw: &str) -> Vec<String> {
    let cleaned = crate::downloader::html::sanitize_text(raw)
        .replace("キーワードが設定されていません", "")
        .replace("キーワード", "");
    let regex_meta = Regex::new(r#"\"?\(\?\.\+\?\)\"?|\(\?<?[^)]*\)"#).expect("valid regex");
    let cleaned = regex_meta.replace_all(&cleaned, "").to_string();
    cleaned
        .split([' ', '　'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

fn section_timestamp_ymd(path: &PathBuf, download_time: Option<&str>) -> Option<String> {
    if let Some(download_time) = download_time
        && let Some(ymd) = date_string_to_ymd(download_time)
    {
        return Some(ymd);
    }

    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt = DateTime::<Utc>::from(modified);
    Some(format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()))
}

impl Downloader {
    pub fn new() -> Result<Self> {
        Self::with_user_agent(None)
    }

    pub fn with_user_agent(user_agent: Option<&str>) -> Result<Self> {
        let ua = match user_agent {
            Some(ua) if ua.eq_ignore_ascii_case("random") => {
                ua_generator::ua::spoof_firefox_ua().to_string()
            }
            Some(ua) if !ua.trim().is_empty() => ua.to_string(),
            _ => ua_generator::ua::spoof_firefox_ua().to_string(),
        };

        let fetcher = HttpFetcher::new(&ua)?;
        let site_settings = SiteSetting::load_all()?;
        let section_hash_cache = crate::db::with_database(|db| {
            db.inventory().load(
                SECTION_HASH_CACHE_NAME,
                crate::db::inventory::InventoryScope::Local,
            )
        })
        .unwrap_or_default();

        Ok(Self {
            fetcher,
            site_settings,
            section_cache: SectionCache::new(),
            section_hash_cache,
            section_hash_cache_dirty: false,
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

    pub fn resolve_target(&self, target: &str) -> Result<(i64, SiteSetting)> {
        let target_type = Self::get_target_type(target);

        match target_type {
            TargetType::Url => {
                let setting = self.find_site_setting(target).ok_or_else(|| {
                    NarouError::InvalidTarget(format!("No site setting found for URL: {}", target))
                })?;
                let toc_url = setting
                    .toc_url_with_url_captures(target)
                    .unwrap_or_else(|| setting.toc_url());
                let db = DATABASE.lock();
                if let Some(db) = db.as_ref() {
                    if let Some(record) = db.get_by_toc_url(&toc_url) {
                        return Ok((record.id, setting));
                    }
                }
                Err(NarouError::NotFound(format!(
                    "Novel not found for URL: {}",
                    target
                )))
            }
            TargetType::Ncode => {
                let ncode = target.to_lowercase();
                let db = DATABASE.lock();
                if let Some(db) = db.as_ref() {
                    for record in db.all_records().values() {
                        if record.ncode.as_deref() == Some(&ncode) {
                            let setting =
                                self.find_site_setting(&record.toc_url).ok_or_else(|| {
                                    NarouError::SiteSetting("No matching site setting".into())
                                })?;
                            return Ok((record.id, setting));
                        }
                    }
                }
                Err(NarouError::NotFound(format!(
                    "Novel not found for ncode: {}",
                    ncode
                )))
            }
            TargetType::Id => {
                let id: i64 = target
                    .parse()
                    .map_err(|_| NarouError::InvalidTarget(target.to_string()))?;
                let db = DATABASE.lock();
                if let Some(db) = db.as_ref() {
                    if let Some(record) = db.get(id) {
                        let setting = self.find_site_setting(&record.toc_url).ok_or_else(|| {
                            NarouError::SiteSetting("No matching site setting".into())
                        })?;
                        return Ok((record.id, setting));
                    }
                }
                Err(NarouError::NotFound(format!(
                    "Novel not found for ID: {}",
                    id
                )))
            }
            TargetType::Other => {
                let db = DATABASE.lock();
                if let Some(db) = db.as_ref() {
                    if let Some(record) = db.find_by_title(target) {
                        let setting = self.find_site_setting(&record.toc_url).ok_or_else(|| {
                            NarouError::SiteSetting("No matching site setting".into())
                        })?;
                        return Ok((record.id, setting));
                    }
                }
                Err(NarouError::NotFound(format!("Novel not found: {}", target)))
            }
        }
    }

    fn find_site_setting(&self, url: &str) -> Option<SiteSetting> {
        for setting in &self.site_settings {
            if setting.matches_url(url) {
                return Some(setting.clone());
            }
        }
        None
    }

    fn load_novel_info(
        &mut self,
        setting: &SiteSetting,
        toc_source: &str,
        url_captures: &HashMap<String, String>,
    ) -> Result<NovelInfo> {
        let Some(novel_info_url) = &setting.novel_info_url else {
            return Ok(NovelInfo::from_toc_source(setting, toc_source));
        };

        let resolved_url = setting
            .novel_info_url_with_captures(url_captures)
            .unwrap_or_else(|| setting.interpolate(novel_info_url));

        match self
            .fetcher
            .fetch_text(&resolved_url, setting.cookie(), Some(setting.encoding()))
        {
            Ok(mut body) => {
                pretreatment_source(&mut body, setting.encoding(), Some(setting));
                Ok(NovelInfo::from_novel_info_source(setting, &body))
            }
            Err(_) => Ok(NovelInfo::from_toc_source(setting, toc_source)),
        }
    }

    pub fn fetch_latest_status_by_id(
        &mut self,
        id: i64,
    ) -> Result<(
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<i64>,
        Option<bool>,
    )> {
        let (toc_url, ncode) = crate::db::with_database(|db| {
            Ok(db.get(id).map(|r| (r.toc_url.clone(), r.ncode.clone())))
        })?
        .ok_or_else(|| NarouError::NotFound(format!("Novel not found for ID: {}", id)))?;

        let setting = self
            .find_site_setting(&toc_url)
            .ok_or_else(|| NarouError::SiteSetting("No matching site setting".into()))?;

        let mut url_captures = setting.extract_url_captures(&toc_url).unwrap_or_default();
        if let Some(ncode) = ncode {
            url_captures.entry("ncode".to_string()).or_insert(ncode);
        }

        let toc_source = fetch_toc(&mut self.fetcher, &setting, &toc_url)?;
        let info = self.load_novel_info(&setting, &toc_source, &url_captures)?;

        let is_end = if info.novel_type.is_some() {
            info.end
        } else {
            setting
                .resolve_info_pattern("nt", &toc_source)
                .map(|text| setting.get_novel_type_from_string(&text).1)
        };

        Ok((
            info.novelupdated_at,
            info.general_lastup,
            info.length,
            is_end,
        ))
    }

    fn process_digest(
        &self,
        existing_id: Option<i64>,
        toc_url: &str,
        novel_dir: &Path,
        title: &str,
        latest_story: &str,
        old_count: usize,
        latest_count: usize,
    ) -> Result<bool> {
        if latest_count >= old_count {
            return Ok(false);
        }

        let mut message = format!(
            "更新後の話数が保存されている話数より減少していることを検知しました。\nダイジェスト化されている可能性があるので、更新に関しての処理を選択して下さい。\n\n保存済み話数: {}\n更新後の話数: {}\n\n",
            old_count, latest_count
        );

        loop {
            match crate::compat::choose_digest_action(title, &message) {
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
                        let author = crate::db::with_database(|db| {
                            Ok(db
                                .get(id)
                                .map(|record| record.author.clone())
                                .unwrap_or_default())
                        })
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

    fn download_illustration(
        &mut self,
        setting: &SiteSetting,
        section: &SectionElement,
        section_dir: &PathBuf,
        subtitle: &SubtitleInfo,
    ) -> Result<()> {
        let illust_url_pattern = match &setting.illust_grep_pattern {
            Some(p) => p,
            None => return Ok(()),
        };

        let re = regex::Regex::new(illust_url_pattern).map_err(|e| NarouError::Regex(e))?;

        let intro_text = section.introduction.as_str();
        let post_text = section.postscript.as_str();
        let sources = [&section.body, intro_text, post_text];

        let mut illust_dir = section_dir.clone();
        illust_dir.pop();
        illust_dir.push("挿絵");
        std::fs::create_dir_all(&illust_dir)?;

        let mut illust_count = 0usize;
        for source in &sources {
            for caps in re.captures_iter(source) {
                if let Some(url_match) = caps.get(1) {
                    let url = url_match.as_str();
                    if url.is_empty() {
                        continue;
                    }

                    let ext = if url.contains(".png") {
                        "png"
                    } else if url.contains(".gif") {
                        "gif"
                    } else if url.contains(".webp") {
                        "webp"
                    } else {
                        "jpg"
                    };

                    let filename = format!("{}-{}.{}", subtitle.index, illust_count, ext);
                    let save_path = illust_dir.join(&filename);

                    if save_path.exists() {
                        illust_count += 1;
                        continue;
                    }

                    self.fetcher.rate_limiter.wait();
                    match self.fetcher.client.get(url).send() {
                        Ok(resp) => {
                            if resp.status().is_success() {
                                if let Ok(bytes) = resp.bytes() {
                                    let _ = std::fs::write(&save_path, &bytes);
                                }
                            }
                        }
                        Err(_) => {}
                    }

                    illust_count += 1;
                }
            }
        }

        Ok(())
    }

    pub fn get_novel_data_dir(&self, record: &NovelRecord) -> PathBuf {
        crate::db::novel_dir_for_record(&PathBuf::from(types::ARCHIVE_ROOT_DIR), record)
    }

    pub fn download_novel(&mut self, target: &str) -> Result<DownloadResult> {
        self.download_novel_with_force(target, false)
    }

    pub fn download_novel_with_force(
        &mut self,
        target: &str,
        force: bool,
    ) -> Result<DownloadResult> {
        let (existing_id, setting) = self.resolve_target_for_download(target)?;

        let db_toc_url = if let Some(id) = existing_id {
            crate::db::with_database(|db| Ok(db.get(id).map(|r| r.toc_url.clone())))
                .ok()
                .flatten()
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
        let toc_source = fetch_toc(&mut self.fetcher, &setting, &toc_url)?;
        if setting.confirm_over18 && !load_global_setting_bool("over18") {
            if !crate::compat::confirm("年齢認証：あなたは18歳以上ですか", false, false)
            {
                return Ok(DownloadResult {
                    id: existing_id.unwrap_or(0),
                    title: String::new(),
                    author: String::new(),
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
            save_global_setting_bool("over18", true)?;
        }

        let info = self.load_novel_info(&setting, &toc_source, &url_captures)?;

        let title = info.title.clone().unwrap_or_default();
        let author = info.author.clone().unwrap_or_default();
        let existing_record = existing_id.and_then(|eid| {
            crate::db::with_database(|db| Ok(db.get(eid).cloned()))
                .ok()
                .flatten()
        });

        let (novel_type, is_end) = match info.novel_type {
            Some(nt) => (nt, info.end.unwrap_or(false)),
            None => {
                let nt_text = setting.resolve_info_pattern("nt", &toc_source);
                match nt_text {
                    Some(text) => setting.get_novel_type_from_string(&text),
                    None => (1u8, false),
                }
            }
        };

        let subtitles = if novel_type == 2 {
            create_short_story_subtitles(&setting, &toc_source)?
        } else {
            parse_subtitles_multipage(&mut self.fetcher, &setting, &toc_source, &url_captures)?
        };

        let use_subdirectory = self.download_use_subdirectory(existing_id);
        let ncode = self
            .extract_ncode(&setting, &toc_source)
            .or_else(|| url_captures.get("ncode").cloned());
        let file_title = self.compute_file_title(
            &ncode,
            &title,
            setting.append_title_to_folder_name,
            existing_id,
        );
        let sitename = existing_record
            .as_ref()
            .and_then(|r| {
                if r.sitename.is_empty() {
                    None
                } else {
                    Some(r.sitename.clone())
                }
            })
            .or_else(|| info.sitename.clone())
            .unwrap_or_else(|| setting.sitename.clone());

        let novel_dir = self.compute_novel_dir(&sitename, &file_title, use_subdirectory);
        std::fs::create_dir_all(&novel_dir)?;

        let section_dir = novel_dir.join(types::SECTION_SAVE_DIR);
        let raw_dir = novel_dir.join(types::RAW_DATA_DIR);
        std::fs::create_dir_all(&section_dir)?;
        std::fs::create_dir_all(&raw_dir)?;

        let old_toc = load_toc_file(&novel_dir);
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
            if self.process_digest(
                existing_id,
                &toc_url,
                &novel_dir,
                &title_for_digest,
                &digest_story,
                old_section_count,
                subtitles.len(),
            )? {
                return Ok(DownloadResult {
                    id: existing_id.unwrap_or(0),
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

        let mut updated_count = 0usize;
        let mut new_arrivals = existing_id.is_none();
        let mut new_arrival_subtitles = Vec::new();
        let total = subtitles.len() as u64;
        let mut final_subtitles = Vec::with_capacity(subtitles.len());
        let strong_update = load_local_setting_bool("update.strong");
        let mut cache_dir: Option<PathBuf> = None;
        let mut pending_section_hashes: HashMap<String, String> = HashMap::new();
        let display_id = existing_id.unwrap_or(0);

        if let Some(ref p) = self.progress {
            p.set_length(total);
            p.set_message(&format!("DL {}", title));
        }
        println!("ID:{}　{} のDL開始", display_id, title);

        let mut last_chapter = String::new();
        let mut last_subchapter = String::new();

        for (si, subtitle) in subtitles.iter().enumerate() {
            let latest_section_path = section_dir.join(section_filename(subtitle));
            let is_new_arrival = !latest_section_path.exists();

            if let Some(ref p) = self.progress {
                p.set_message(&format!(
                    "DL {} [{}/{}]",
                    title,
                    final_subtitles.len() + 1,
                    subtitles.len()
                ));
            }

            // Print chapter/subchapter headers (Ruby: only when changed)
            if !subtitle.chapter.is_empty() && subtitle.chapter != last_chapter {
                println!("{}", subtitle.chapter);
                last_chapter = subtitle.chapter.clone();
            }
            if !subtitle.subchapter.is_empty() && subtitle.subchapter != last_subchapter {
                println!("{}", subtitle.subchapter);
                last_subchapter = subtitle.subchapter.clone();
            }

            let (needs_download, predownloaded) = if force {
                (true, None)
            } else {
                self.section_needs_download(
                    &setting,
                    subtitle,
                    old_subtitles.get(&subtitle.index).copied(),
                    existing_id,
                    &section_dir,
                    &toc_url,
                    strong_update,
                )?
            };

            let download_time = if needs_download {
                let (section, raw_html) = if let Some(downloaded) = predownloaded {
                    downloaded
                } else {
                    download_section(
                        &mut self.fetcher,
                        &mut self.section_cache,
                        &setting,
                        subtitle,
                        &toc_url,
                    )?
                };
                if latest_section_path.exists() {
                    if cache_dir.is_none() {
                        cache_dir = create_cache_dir(&section_dir)?;
                    }
                    if let Some(id) = existing_id {
                        self.clear_section_digest(id, &section_relative_path(subtitle));
                    }
                    move_to_cache_dir(&section_dir, cache_dir.as_deref(), subtitle)?;
                }
                save_section_file(&section_dir, subtitle, &section)?;
                let digest = compute_section_hash(&section);
                let relative_path = section_relative_path(subtitle);
                if let Some(id) = existing_id {
                    self.store_section_digest(id, &relative_path, &digest);
                } else {
                    pending_section_hashes.insert(relative_path, digest);
                }
                save_raw_file(&raw_dir, subtitle, &raw_html)?;
                self.download_illustration(&setting, &section, &section_dir, subtitle)?;
                updated_count += 1;
                Some(Utc::now().format("%Y-%m-%d %H:%M:%S%.6f %z").to_string())
            } else {
                if setting.illust_grep_pattern.is_some() {
                    if let Ok(content) = std::fs::read_to_string(&latest_section_path) {
                        if let Ok(section_file) = serde_yaml::from_str::<SectionFile>(&content) {
                            self.download_illustration(
                                &setting,
                                &section_file.element,
                                &section_dir,
                                subtitle,
                            )?;
                        }
                    }
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

            // Ruby-compatible section progress line
            {
                let mut line = String::new();
                if novel_type == 1 {
                    // Series: "第{index}部分　" (only if index ≤ 4 digits)
                    if subtitle.index.len() <= 4 {
                        line.push_str(&format!("第{}部分　", subtitle.index));
                    }
                } else {
                    line.push_str("短編　");
                }
                line.push_str(&format!(
                    "{} ({}/{})",
                    subtitle.subtitle,
                    si + 1,
                    subtitles.len()
                ));
                if needs_download {
                    if is_new_arrival && (existing_id.is_some() || force) {
                        line.push_str(" (新着)");
                    } else if !is_new_arrival && force {
                        line.push_str(" (更新あり)");
                    }
                }
                println!("{}", line);
            }
            if let Some(ref p) = self.progress {
                p.inc(1);
            }
        }

        remove_cache_dir_if_empty(cache_dir.as_deref())?;

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
        save_toc_file(&novel_dir, &toc_file)?;
        ensure_default_files(&novel_dir, &toc_title, &toc_author, &toc_url);

        let record = NovelRecord {
            id: existing_id.unwrap_or(0),
            author: toc_author.clone(),
            title: toc_title.clone(),
            file_title: file_title.clone(),
            toc_url,
            sitename,
            novel_type,
            end: is_end,
            last_update: Utc::now(),
            new_arrivals_date: Some(Utc::now()),
            use_subdirectory,
            general_firstup: info.general_firstup,
            novelupdated_at: info.novelupdated_at,
            general_lastup: info.general_lastup,
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
        };

        let auto_add_tags = load_local_setting_bool("auto-add-tags");
        let mut merged_tags = existing_record
            .as_ref()
            .map(|record| record.tags.clone())
            .unwrap_or_default();
        if auto_add_tags {
            if let Some(raw_tags) = setting.resolve_info_pattern("tags", &toc_source) {
                for tag in sanitize_site_tags(&raw_tags) {
                    if !merged_tags.contains(&tag) {
                        merged_tags.push(tag);
                    }
                }
            }
        }

        let id = crate::db::with_database_mut(|db| {
            let id = if let Some(eid) = existing_id {
                if let Some(existing) = db.get(eid) {
                    let mut updated = existing.clone();
                    if !record.author.is_empty() {
                        updated.author = record.author.clone();
                    }
                    if !record.title.is_empty() {
                        updated.title = record.title.clone();
                    }
                    updated.file_title = record.file_title.clone();
                    updated.toc_url = record.toc_url.clone();
                    updated.sitename = record.sitename.clone();
                    updated.end = record.end;
                    updated.last_update = record.last_update;
                    if updated_count > 0 {
                        updated.new_arrivals_date = record.new_arrivals_date;
                    }
                    updated.use_subdirectory = record.use_subdirectory;
                    updated.general_firstup = record.general_firstup;
                    updated.novelupdated_at = record.novelupdated_at;
                    updated.general_lastup = record.general_lastup;
                    updated.general_all_no = record.general_all_no;
                    updated.length = record.length;
                    updated.domain = record.domain.clone();
                    updated.suspend = false;
                    updated.is_narou = record.is_narou;
                    if !merged_tags.is_empty() {
                        updated.tags = merged_tags.clone();
                    }
                    db.insert(updated);
                    eid
                } else {
                    let new_id = db.create_new_id();
                    let mut rec = record;
                    rec.id = new_id;
                    rec.tags = merged_tags.clone();
                    db.insert(rec);
                    new_id
                }
            } else {
                let new_id = db.create_new_id();
                let mut rec = record;
                rec.id = new_id;
                rec.tags = merged_tags.clone();
                db.insert(rec);
                new_id
            };
            db.save()?;
            Ok::<i64, NarouError>(id)
        })?;

        if let Some(old_id) = existing_id {
            self.move_section_hash_bucket(old_id, id);
        } else {
            for (relative_path, digest) in pending_section_hashes {
                self.store_section_digest(id, &relative_path, &digest);
            }
        }
        self.flush_section_hash_cache()?;

        let has_changes = updated_count > 0
            || existing_id.is_none()
            || title_changed
            || author_changed
            || story_changed
            || sections_deleted;

        let status = if has_changes {
            types::UpdateStatus::Ok
        } else {
            types::UpdateStatus::None
        };

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

    fn section_needs_download(
        &mut self,
        setting: &SiteSetting,
        latest: &SubtitleInfo,
        old: Option<&SubtitleInfo>,
        existing_id: Option<i64>,
        section_dir: &PathBuf,
        toc_url: &str,
        strong_update: bool,
    ) -> Result<(bool, Option<(SectionElement, String)>)> {
        let Some(old) = old else {
            return Ok((true, None));
        };

        if old.subtitle != latest.subtitle || old.chapter != latest.chapter {
            return Ok((true, None));
        }

        let old_section_path = section_dir.join(section_filename(old));
        if !old_section_path.exists() {
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
                date_string_is_newer(latest_subupdate, old_subupdate),
                Some(old_subupdate),
            )
        } else {
            if old.subdate.is_empty() {
                return Ok((true, None));
            }
            (
                date_string_is_newer(&latest.subdate, &old.subdate),
                Some(old.subdate.as_str()),
            )
        };

        if !date_says_update {
            return Ok((false, None));
        }

        if strong_update
            && let Some(basis_date) = strong_basis_date
            && date_string_to_ymd(basis_date)
                == section_timestamp_ymd(&old_section_path, old.download_time.as_deref())
        {
            let downloaded = download_section(
                &mut self.fetcher,
                &mut self.section_cache,
                setting,
                latest,
                toc_url,
            )?;
            let new_hash = compute_section_hash(&downloaded.0);
            let relative_path = section_relative_path(old);
            let old_hash = existing_id
                .and_then(|id| {
                    self.ensure_cached_section_digest(id, &relative_path, &old_section_path)
                })
                .or_else(|| {
                    load_section_file(&old_section_path).map(|section| {
                        let digest = compute_section_hash(&section.element);
                        if let Some(id) = existing_id {
                            self.store_section_digest(id, &relative_path, &digest);
                        }
                        digest
                    })
                });
            if old_hash.as_deref() == Some(new_hash.as_str()) {
                if let Some(id) = existing_id {
                    self.store_section_digest(id, &relative_path, &new_hash);
                }
                return Ok((false, None));
            }
            return Ok((true, Some(downloaded)));
        }

        Ok((true, None))
    }

    fn resolve_target_for_download(&self, target: &str) -> Result<(Option<i64>, SiteSetting)> {
        let target_type = Self::get_target_type(target);

        match target_type {
            TargetType::Url => {
                let setting = self.find_site_setting(target).ok_or_else(|| {
                    NarouError::InvalidTarget(format!("No site setting for URL: {}", target))
                })?;
                let toc_url = setting
                    .toc_url_with_url_captures(target)
                    .unwrap_or_else(|| setting.toc_url());
                let existing_id =
                    crate::db::with_database(|db| Ok(db.get_by_toc_url(&toc_url).map(|r| r.id)))
                        .ok()
                        .flatten();
                Ok((existing_id, setting))
            }
            TargetType::Ncode => {
                let ncode = target.to_lowercase();
                let existing_id = crate::db::with_database(|db| {
                    Ok(db
                        .all_records()
                        .values()
                        .find(|r| r.ncode.as_deref() == Some(ncode.as_str()))
                        .map(|r| r.id))
                })
                .ok()
                .flatten();
                if let Some(id) = existing_id {
                    let toc_url =
                        crate::db::with_database(|db| Ok(db.get(id).map(|r| r.toc_url.clone())))
                            .ok()
                            .flatten();
                    let setting = match toc_url {
                        Some(ref url) => self.find_site_setting(url).ok_or_else(|| {
                            NarouError::SiteSetting("No matching site setting".into())
                        })?,
                        None => {
                            return Err(NarouError::NotFound(format!(
                                "Novel record {} has no toc_url",
                                id
                            )));
                        }
                    };
                    Ok((Some(id), setting))
                } else {
                    let narou_url = format!("https://ncode.syosetu.com/{}/", ncode);
                    let setting = self.find_site_setting(&narou_url).ok_or_else(|| {
                        NarouError::InvalidTarget(format!("対応外のncodeです({})", ncode))
                    })?;
                    let existing_id = crate::db::with_database(|db| {
                        let toc_url = setting
                            .toc_url_with_url_captures(&narou_url)
                            .unwrap_or_else(|| setting.toc_url());
                        Ok(db.get_by_toc_url(&toc_url).map(|r| r.id))
                    })
                    .ok()
                    .flatten();
                    Ok((existing_id, setting))
                }
            }
            TargetType::Id => {
                let id: i64 = target
                    .parse()
                    .map_err(|_| NarouError::InvalidTarget(target.to_string()))?;
                let setting = crate::db::with_database(|db| {
                    Ok(db.get(id).and_then(|r| self.find_site_setting(&r.toc_url)))
                })
                .ok()
                .flatten();
                let setting = setting.ok_or_else(|| {
                    NarouError::NotFound(format!("Novel not found for ID: {}", id))
                })?;
                Ok((Some(id), setting))
            }
            TargetType::Other => {
                let existing_id =
                    crate::db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
                        .ok()
                        .flatten();
                if let Some(id) = existing_id {
                    let toc_url =
                        crate::db::with_database(|db| Ok(db.get(id).map(|r| r.toc_url.clone())))
                            .ok()
                            .flatten();
                    let setting = match toc_url {
                        Some(ref url) => self.find_site_setting(url).ok_or_else(|| {
                            NarouError::SiteSetting("No matching site setting".into())
                        })?,
                        None => {
                            return Err(NarouError::NotFound(format!(
                                "Novel record {} has no toc_url",
                                id
                            )));
                        }
                    };
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
        existing_id: Option<i64>,
    ) -> String {
        if let Some(id) = existing_id {
            if let Ok(Some(record)) = crate::db::with_database(|db| Ok(db.get(id).cloned())) {
                if !record.file_title.is_empty() {
                    return record.file_title;
                }
            }
        }

        if let Some(ncode) = ncode {
            if !append_title {
                return ncode.clone();
            }
            let sanitized = sanitize_filename(title);
            if sanitized.is_empty() {
                ncode.clone()
            } else {
                format!("{} {}", ncode, sanitized)
            }
        } else {
            sanitize_filename(title)
        }
    }

    fn compute_novel_dir(
        &self,
        sitename: &str,
        file_title: &str,
        use_subdirectory: bool,
    ) -> PathBuf {
        let mut dir = PathBuf::from(types::ARCHIVE_ROOT_DIR);
        dir.push(sitename);

        if use_subdirectory {
            let subdirectory = crate::db::create_subdirectory_name(file_title);
            if !subdirectory.is_empty() {
                dir.push(subdirectory);
            }
        }

        dir.push(file_title);
        dir
    }

    fn download_use_subdirectory(&self, existing_id: Option<i64>) -> bool {
        if let Some(id) = existing_id {
            if let Ok(Some(record)) = crate::db::with_database(|db| Ok(db.get(id).cloned())) {
                return record.use_subdirectory;
            }
        }

        crate::db::with_database(|db| {
            let settings: HashMap<String, serde_yaml::Value> = db
                .inventory()
                .load("local_setting", crate::db::inventory::InventoryScope::Local)?;
            Ok(settings
                .get("download.use-subdirectory")
                .and_then(|value| value.as_bool())
                .unwrap_or(false))
        })
        .unwrap_or(false)
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

    fn ensure_cached_section_digest(
        &mut self,
        id: i64,
        relative_path: &str,
        full_path: &PathBuf,
    ) -> Option<String> {
        if let Some(digest) = self.cached_section_digest(id, relative_path) {
            return Some(digest.to_string());
        }

        let section = load_section_file(full_path)?;
        let digest = compute_section_hash(&section.element);
        self.store_section_digest(id, relative_path, &digest);
        Some(digest)
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
        crate::db::with_database(|db| {
            db.inventory().save(
                SECTION_HASH_CACHE_NAME,
                crate::db::inventory::InventoryScope::Local,
                &self.section_hash_cache,
            )?;
            Ok(())
        })?;
        self.section_hash_cache_dirty = false;
        Ok(())
    }

    pub fn set_progress(&mut self, progress: Box<dyn ProgressReporter>) {
        self.progress = Some(progress);
    }

    pub fn site_setting_matches_url(&self, url: &str) -> bool {
        self.find_site_setting(url).is_some()
    }

    pub fn narou_api_batch_update(&mut self) -> Result<(usize, usize)> {
        narou_api_batch_update(&mut self.fetcher)
    }
}

#[cfg(test)]
mod tests {
    use super::Downloader;
    use super::novel_info::NovelInfo;
    use super::site_setting::SiteSetting;

    #[test]
    fn sanitize_filename_removes_windows_trailing_dots_and_spaces() {
        assert_eq!(super::util::sanitize_filename("title. "), "title");
        assert_eq!(super::util::sanitize_filename("bad/name?"), "bad_name_");
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
            super::date_string_to_ymd("2026-04-12 10:00:00.123456 +0900"),
            Some("20260412".to_string())
        );
    }

    #[test]
    fn kakuyomu_preprocess_yaml_supports_table_of_contents_v2_and_tags() {
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
                            "tagLabels": ["tag-a"],
                            "tableOfContentsV2": [{"__ref": "TableOfContentsChapter:10"}]
                        },
                        "UserAccount:1": {
                            "activityName": "author-name"
                        },
                        "TableOfContentsChapter:10": {
                            "chapter": {"__ref": "Chapter:10"},
                            "episodeUnions": [{"__ref": "Episode:20"}]
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
                            "title": "第1話"
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
        assert!(html.contains("Chapter;1;10;第一章"));
        assert!(!html.contains("Chapter;1;10;;第一章"));
        assert!(html.contains("Episode;20;2021-01-12T16:13:02Z;第1話"));
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
    }

    #[test]
    fn arcadia_toc_patterns_extract_title_and_author_from_legacy_yaml_keys() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "Arcadia").unwrap();
        let html = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("sample")
                .join("novel")
                .join("test_arcadia_page.html"),
        )
        .unwrap();

        let info = NovelInfo::from_toc_source(setting, &html);

        assert_eq!(
            info.title.as_deref(),
            Some("異世界に来たけど至って普通に喫茶店とかやってますが何か問題でも？")
        );
        assert_eq!(info.author.as_deref(), Some("風見鶏"));
    }

    #[test]
    fn section_hash_cache_store_and_clear_roundtrip() {
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
