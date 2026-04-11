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
use std::path::PathBuf;

use chrono::Utc;

use crate::db::DATABASE;
use crate::db::novel_record::NovelRecord;
use crate::error::{NarouError, Result};
use crate::progress::ProgressReporter;

use self::fetch::HttpFetcher;
use self::narou_api::narou_api_batch_update;
use self::novel_info::NovelInfo;
use self::persistence::{
    ensure_default_files, load_toc_file, save_raw_file, save_section_file, save_toc_file,
};
use self::section::{download_section, SectionCache};
use self::site_setting::SiteSetting;
use self::toc::{
    create_short_story_subtitles, fetch_toc, parse_subtitles_multipage,
};
use self::util::sanitize_filename;

pub use self::types::{
    DownloadResult, NarouApiEntry, NarouApiResult, SectionElement, SectionFile,
    SubtitleInfo, TocFile, TocObject, TargetType,
    SECTION_SAVE_DIR, RAW_DATA_DIR, ARCHIVE_ROOT_DIR,
};
pub use self::util::pretreatment_source;

pub struct Downloader {
    fetcher: HttpFetcher,
    site_settings: Vec<SiteSetting>,
    section_cache: SectionCache,
    progress: Option<Box<dyn ProgressReporter>>,
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

        Ok(Self {
            fetcher,
            site_settings,
            section_cache: SectionCache::new(),
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
                let toc_url = setting.toc_url();
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

        match self.fetcher.fetch_text(&resolved_url, setting.cookie()) {
            Ok(mut body) => {
                pretreatment_source(&mut body, setting.encoding(), Some(setting));
                Ok(NovelInfo::from_novel_info_source(setting, &body))
            }
            Err(_) => Ok(NovelInfo::from_toc_source(setting, toc_source)),
        }
    }

    fn handle_over18(&self, setting: &SiteSetting, body: &str) -> Option<String> {
        if !setting.confirm_over18 {
            return None;
        }
        let patterns = [
            r"(?i)over.?18|age.?verification|年齢確認",
            r"(?i)<form[^>]*>.*?</form>",
        ];
        for pattern in &patterns {
            if let Ok(re) = regex::Regex::new(pattern) {
                if re.is_match(body) {
                    return Some("over18=yes".to_string());
                }
            }
        }
        None
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
        let (existing_id, setting) = self.resolve_target_for_download(target)?;

        let db_toc_url = if let Some(id) = existing_id {
            crate::db::with_database(|db| Ok(db.get(id).map(|r| r.toc_url.clone())))
                .ok()
                .flatten()
        } else {
            None
        };

        let url_captures = setting.extract_url_captures(target).unwrap_or_default();
        let toc_url = if let Some(ref url) = db_toc_url {
            url.clone()
        } else if url_captures.is_empty() {
            setting.interpolate(&setting.toc_url)
        } else {
            setting.interpolate_with_captures(&setting.toc_url, &url_captures)
        };
        let toc_source = fetch_toc(&mut self.fetcher, &setting, &toc_url)?;

        let info = self.load_novel_info(&setting, &toc_source, &url_captures)?;

        let title = info.title.clone().unwrap_or_default();
        let author = info.author.clone().unwrap_or_default();

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
        let sitename = info.sitename.unwrap_or_else(|| setting.sitename.clone());

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

        let mut updated_count = 0usize;
        let total = subtitles.len() as u64;
        let mut final_subtitles = Vec::with_capacity(subtitles.len());

        if let Some(ref p) = self.progress {
            p.set_length(total);
            p.set_message(&format!("DL {}", title));
        }

        for subtitle in &subtitles {
            if let Some(ref p) = self.progress {
                p.set_message(&format!("DL {} [{}/{}]",
                    title, final_subtitles.len() + 1, subtitles.len()));
            }

            let needs_download = match old_subtitles.get(&subtitle.index) {
                Some(old) => {
                    subtitle.subtitle != old.subtitle
                        || subtitle.subdate != old.subdate
                        || subtitle.subupdate != old.subupdate
                }
                None => true,
            };

            let download_time = if needs_download {
                let (section, raw_html) =
                    download_section(&mut self.fetcher, &mut self.section_cache, &setting, subtitle, &toc_url)?;
                save_section_file(&section_dir, subtitle, &section)?;
                save_raw_file(&raw_dir, subtitle, &raw_html)?;
                updated_count += 1;
                Some(Utc::now().format("%Y-%m-%d %H:%M:%S%.6f %z").to_string())
            } else {
                old_subtitles
                    .get(&subtitle.index)
                    .and_then(|old| old.download_time.clone())
            };

            let mut sub = subtitle.clone();
            sub.download_time = download_time;
            final_subtitles.push(sub);

            if let Some(ref p) = self.progress {
                p.inc(1);
            }
        }

        if let Some(ref p) = self.progress {
            p.finish_with_message(&format!(
                "DL {} done ({}/{})",
                title, updated_count, subtitles.len()
            ));
        }

        let toc_file = TocFile {
            title: title.clone(),
            author: author.clone(),
            toc_url: toc_url.clone(),
            story: info.story.as_ref().map(|s| s.replace("<br>", "\n")),

            subtitles: final_subtitles,
            novel_type: Some(novel_type),
        };
        save_toc_file(&novel_dir, &toc_file)?;
        ensure_default_files(&novel_dir, &title, &author, &toc_url);

        let record = NovelRecord {
            id: existing_id.unwrap_or(0),
            author: author.clone(),
            title: title.clone(),
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
        };

        let id = crate::db::with_database_mut(|db| {
            let id = if let Some(eid) = existing_id {
                if let Some(existing) = db.get(eid) {
                    let mut updated = existing.clone();
                    updated.author = record.author.clone();
                    updated.title = record.title.clone();
                    updated.file_title = record.file_title.clone();
                    updated.end = record.end;
                    updated.last_update = record.last_update;
                    updated.general_firstup = record.general_firstup;
                    updated.novelupdated_at = record.novelupdated_at;
                    updated.general_lastup = record.general_lastup;
                    updated.general_all_no = record.general_all_no;
                    updated.length = record.length;
                    updated.suspend = false;
                    db.insert(updated);
                    eid
                } else {
                    let new_id = db.create_new_id();
                    let mut rec = record;
                    rec.id = new_id;
                    db.insert(rec);
                    new_id
                }
            } else {
                let new_id = db.create_new_id();
                let mut rec = record;
                rec.id = new_id;
                db.insert(rec);
                new_id
            };
            db.save()?;
            Ok::<i64, NarouError>(id)
        })?;

        Ok(DownloadResult {
            id,
            title: title.clone(),
            author: author.clone(),
            novel_dir,
            new_novel: existing_id.is_none(),
            updated_count,
            total_count: subtitles.len(),
        })
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
                    Err(NarouError::NotFound(format!(
                        "Novel not found for ncode: {} (use URL for new downloads)",
                        ncode
                    )))
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

    pub fn set_progress(&mut self, progress: Box<dyn ProgressReporter>) {
        self.progress = Some(progress);
    }

    pub fn narou_api_batch_update(&mut self) -> Result<(usize, usize)> {
        narou_api_batch_update(&mut self.fetcher)
    }
}

#[cfg(test)]
mod tests {
    use super::novel_info::NovelInfo;
    use super::site_setting::SiteSetting;

    #[test]
    fn sanitize_filename_removes_windows_trailing_dots_and_spaces() {
        assert_eq!(super::util::sanitize_filename("title. "), "title");
        assert_eq!(super::util::sanitize_filename("bad/name?"), "bad_name_");
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
}
