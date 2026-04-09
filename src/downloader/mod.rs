pub mod html;
pub mod info_cache;
pub mod novel_info;
pub mod rate_limit;
pub mod site_setting;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::db::DATABASE;
use crate::error::{NarouError, Result};

use self::rate_limit::RateLimiter;
use self::site_setting::SiteSetting;

use chrono::Utc;

use self::novel_info::NovelInfo;
use crate::db::novel_record::NovelRecord;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarouApiResult {
    #[serde(default)]
    pub allcount: i64,
    #[serde(default)]
    pub data: Vec<NarouApiEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarouApiEntry {
    #[serde(default)]
    pub ncode: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub writer: String,
    #[serde(default)]
    pub story: String,
    #[serde(default)]
    pub novel_type: i64,
    #[serde(default)]
    pub end: i64,
    #[serde(default)]
    pub general_all_no: i64,
    #[serde(default)]
    pub general_firstup: String,
    #[serde(default)]
    pub general_lastup: String,
    #[serde(default)]
    pub novelupdated_at: String,
    #[serde(default)]
    pub length: i64,
}

pub struct Downloader {
    client: reqwest::blocking::Client,
    rate_limiter: RateLimiter,
    site_settings: Vec<SiteSetting>,
    section_cache: HashMap<String, SectionElement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleInfo {
    pub index: String,
    pub href: String,
    #[serde(default)]
    pub chapter: String,
    #[serde(default)]
    pub subchapter: String,
    pub subtitle: String,
    pub file_subtitle: String,
    pub subdate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subupdate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TocObject {
    pub title: String,
    pub author: String,
    pub toc_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub story: Option<String>,
    pub subtitles: Vec<SubtitleInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub novel_type: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionElement {
    pub data_type: String,
    #[serde(default)]
    pub introduction: String,
    #[serde(default)]
    pub postscript: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionFile {
    pub index: String,
    pub href: String,
    #[serde(default)]
    pub chapter: String,
    #[serde(default)]
    pub subchapter: String,
    pub subtitle: String,
    pub file_subtitle: String,
    pub subdate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subupdate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_time: Option<String>,
    pub element: SectionElement,
}

#[derive(Debug, Clone, Copy)]
pub enum TargetType {
    Url,
    Ncode,
    Id,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TocFile {
    pub title: String,
    pub author: String,
    pub toc_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub story: Option<String>,
    pub subtitles: Vec<SubtitleInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub novel_type: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub id: i64,
    pub title: String,
    pub new_novel: bool,
    pub updated_count: usize,
    pub total_count: usize,
}

pub const SECTION_SAVE_DIR: &str = "本文";
const RAW_DATA_DIR: &str = "raw";
const CACHE_SAVE_DIR: &str = "cache";
const MAX_SECTION_CACHE: usize = 20;

impl Downloader {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let site_settings = SiteSetting::load_all()?;

        Ok(Self {
            client,
            rate_limiter: RateLimiter::new(),
            site_settings,
            section_cache: HashMap::new(),
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

    pub fn fetch_toc(&mut self, setting: &SiteSetting, toc_url: &str) -> Result<String> {
        self.rate_limiter.wait();
        let url = toc_url.to_string();
        let mut request = self.client.get(&url);

        if let Some(cookie) = setting.cookie() {
            request = request.header("Cookie", cookie);
        }

        let response = request.send()?;
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 503 {
                return Err(NarouError::SuspendDownload("Rate limited (503)".into()));
            }
            if status.as_u16() == 404 {
                return Err(NarouError::NotFound(url));
            }
            return Err(response.error_for_status().unwrap_err().into());
        }

        let mut body = response.text()?;
        pretreatment_source(&mut body, setting.encoding());

        if let Some(error_pattern) = setting.error_message() {
            if let Ok(re) = regex::Regex::new(error_pattern) {
                if re.is_match(&body) {
                    return Err(NarouError::NotFound("Novel deleted or private".into()));
                }
            }
        }

        Ok(body)
    }

    pub fn parse_subtitles(
        &self,
        setting: &SiteSetting,
        toc_source: &str,
        url_captures: &HashMap<String, String>,
    ) -> Result<Vec<SubtitleInfo>> {
        let subtitles_pattern = setting
            .subtitles_pattern()
            .ok_or_else(|| NarouError::SiteSetting("No subtitles pattern defined".into()))?;

        let mut subtitles = Vec::new();
        let mut remaining = toc_source;

        while let Some(caps) = subtitles_pattern.captures(remaining) {
            let full_match = caps.get(0).unwrap();
            remaining = &remaining[full_match.end()..];

            let index = caps
                .name("index")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let href = caps
                .name("href")
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| {
                    if let Some(href_tpl) = &setting.href {
                        setting.interpolate_subtitles_href(href_tpl, &index, url_captures)
                    } else {
                        String::new()
                    }
                });
            let chapter = caps
                .name("chapter")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let subchapter = caps
                .name("subchapter")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let subtitle_raw = caps
                .name("subtitle")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let subdate = caps
                .name("subdate")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let subupdate = caps.name("subupdate").map(|m| m.as_str().to_string());
            let subdate = if subdate.is_empty() {
                subupdate.clone().unwrap_or_default()
            } else {
                subdate
            };

            let file_subtitle = sanitize_filename(&subtitle_raw);

            subtitles.push(SubtitleInfo {
                index,
                href,
                chapter,
                subchapter,
                subtitle: subtitle_raw,
                file_subtitle,
                subdate,
                subupdate,
                download_time: None,
            });

            if full_match.end() == 0 {
                break;
            }
        }

        Ok(subtitles)
    }

    pub fn download_section(
        &mut self,
        setting: &SiteSetting,
        subtitle: &SubtitleInfo,
    ) -> Result<(SectionElement, String)> {
        if let Some(cached) = self.section_cache.get(&subtitle.index) {
            return Ok((cached.clone(), String::new()));
        }

        self.rate_limiter.wait();
        let url = build_section_url(setting, &subtitle.href);

        let mut request = self.client.get(&url);
        if let Some(cookie) = setting.cookie() {
            request = request.header("Cookie", cookie);
        }

        let response = request.send()?;
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 503 {
                return Err(NarouError::SuspendDownload("Rate limited (503)".into()));
            }
            return Err(response.error_for_status().unwrap_err().into());
        }

        let mut html_source = response.text()?;
        pretreatment_source(&mut html_source, setting.encoding());

        let mut element = SectionElement {
            data_type: "html".to_string(),
            introduction: String::new(),
            postscript: String::new(),
            body: String::new(),
        };

        if let Some(pattern) = setting.introduction_pattern() {
            if let Ok(re) = compile_html_pattern(pattern) {
                if let Some(caps) = re.captures(&html_source) {
                    element.introduction = caps
                        .name("introduction")
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                }
            }
        }

        if let Some(pattern) = setting.postscript_pattern() {
            if let Ok(re) = compile_html_pattern(pattern) {
                if let Some(caps) = re.captures(&html_source) {
                    element.postscript = caps
                        .name("postscript")
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                }
            }
        }

        if let Some(pattern) = setting.body_pattern() {
            if let Ok(re) = compile_html_pattern(pattern) {
                if let Some(caps) = re.captures(&html_source) {
                    element.body = caps
                        .name("body")
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                }
            }
        }

        if self.section_cache.len() >= MAX_SECTION_CACHE {
            if let Some(oldest_key) = self.section_cache.keys().next().cloned() {
                self.section_cache.remove(&oldest_key);
            }
        }
        self.section_cache
            .insert(subtitle.index.clone(), element.clone());

        Ok((element, html_source))
    }

    pub fn narou_api_batch_update(&mut self) -> Result<(usize, usize)> {
        let narou_ids: Vec<(i64, String)> = crate::db::with_database(|db| {
            Ok(db
                .all_records()
                .values()
                .filter(|r| r.is_narou && r.ncode.is_some())
                .filter_map(|r| r.ncode.as_ref().map(|nc| (r.id, nc.clone())))
                .collect())
        })
        .unwrap_or_default();

        if narou_ids.is_empty() {
            return Ok((0, 0));
        }

        let mut all_ncodes = Vec::new();
        for chunk in narou_ids.chunks(50) {
            let ncodes: Vec<&str> = chunk.iter().map(|(_, nc)| nc.as_str()).collect();
            all_ncodes.push(ncodes.join("-"));
        }

        let api_url = "https://api.syosetu.com/novelapi/api/";
        let mut total_updated = 0usize;
        let mut total_failed = 0usize;

        for ncode_chunk in &all_ncodes {
            self.rate_limiter.wait();
            let url = format!(
                "{}?of=t-nt-ga-gf-nu-gl-l-w-s-e-ncode-allno-novelpage&out=json&ncode={}",
                api_url, ncode_chunk
            );

            let response = match self.client.get(&url).send() {
                Ok(r) => r,
                Err(_e) => {
                    total_failed += 50;
                    continue;
                }
            };

            if !response.status().is_success() {
                total_failed += 50;
                continue;
            }

            let body = match response.text() {
                Ok(b) => b,
                Err(_) => {
                    total_failed += 50;
                    continue;
                }
            };

            let api_result: NarouApiResult = match serde_json::from_str(&body) {
                Ok(r) => r,
                Err(_) => {
                    total_failed += 50;
                    continue;
                }
            };

            for entry in &api_result.data {
                if let Some(id) = narou_ids
                    .iter()
                    .find(|(_, nc)| nc == &entry.ncode)
                    .map(|(id, _)| *id)
                {
                    let updated = crate::db::with_database_mut(|db| {
                        if let Some(record) = db.get(id).cloned() {
                            let mut r = record;
                            r.title = entry.title.clone();
                            r.author = entry.writer.clone();
                            r.end = entry.end == 1;
                            r.general_all_no = Some(entry.general_all_no);
                            r.length = Some(entry.length);

                            if let Ok(dt) =
                                chrono::DateTime::parse_from_rfc3339(&entry.general_firstup)
                            {
                                r.general_firstup = Some(dt.with_timezone(&Utc));
                            }
                            if let Ok(dt) =
                                chrono::DateTime::parse_from_rfc3339(&entry.general_lastup)
                            {
                                r.general_lastup = Some(dt.with_timezone(&Utc));
                            }
                            if let Ok(dt) =
                                chrono::DateTime::parse_from_rfc3339(&entry.novelupdated_at)
                            {
                                r.novelupdated_at = Some(dt.with_timezone(&Utc));
                            }

                            if entry.novel_type == 2 {
                                r.novel_type = 2;
                            } else {
                                r.novel_type = 1;
                            }

                            db.insert(r);
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    })
                    .unwrap_or(false);

                    if updated {
                        total_updated += 1;
                    }
                }
            }
        }

        let _ = crate::db::with_database_mut(|db| db.save());
        Ok((total_updated, total_failed))
    }

    fn compute_section_hash(section: &SectionElement) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(section.body.as_bytes());
        hasher.update(section.introduction.as_bytes());
        hasher.update(section.postscript.as_bytes());
        hex::encode(hasher.finalize())
    }

    fn section_needs_update(
        section_dir: &PathBuf,
        subtitle: &SubtitleInfo,
        new_section: &SectionElement,
    ) -> bool {
        let filename = format!("{} {}.yaml", subtitle.index, subtitle.file_subtitle);
        let path = section_dir.join(&filename);
        if !path.exists() {
            return true;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(existing) = serde_yaml::from_str::<SectionElement>(&content) {
                let old_hash = Self::compute_section_hash(&existing);
                let new_hash = Self::compute_section_hash(new_section);
                return old_hash != new_hash;
            }
        }
        true
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

                    self.rate_limiter.wait();
                    match self.client.get(url).send() {
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

    pub fn get_novel_data_dir(&self, record: &crate::db::novel_record::NovelRecord) -> PathBuf {
        let mut dir = PathBuf::from(ARCHIVE_ROOT_DIR);
        dir.push(&record.sitename);
        if record.use_subdirectory {
            if let Some(ref ncode) = record.ncode {
                if ncode.len() >= 2 {
                    dir.push(&ncode[..2]);
                }
            }
        }
        dir.push(&record.file_title);
        dir
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
        let toc_source = self.fetch_toc(&setting, &toc_url)?;

        let info = NovelInfo::load(&setting, &self.client, &toc_source, &url_captures)?;

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
            self.create_short_story_subtitles(&setting, &toc_source)?
        } else {
            self.parse_subtitles_multipage(&setting, &toc_source, &url_captures)?
        };

        let use_subdirectory = setting.domain.contains("syosetu.com");
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

        let novel_dir = self.compute_novel_dir(&sitename, &file_title, use_subdirectory, &ncode);
        std::fs::create_dir_all(&novel_dir)?;

        let section_dir = novel_dir.join(SECTION_SAVE_DIR);
        let raw_dir = novel_dir.join(RAW_DATA_DIR);
        std::fs::create_dir_all(&section_dir)?;
        std::fs::create_dir_all(&raw_dir)?;

        let old_toc = self.load_toc_file(&novel_dir);
        let old_subtitles: HashMap<String, &SubtitleInfo> = old_toc
            .as_ref()
            .map(|t| t.subtitles.iter().map(|s| (s.index.clone(), s)).collect())
            .unwrap_or_default();

        let mut updated_count = 0usize;
        let mut final_subtitles = Vec::with_capacity(subtitles.len());
        for subtitle in &subtitles {
            let needs_download = match old_subtitles.get(&subtitle.index) {
                Some(old) => {
                    subtitle.subtitle != old.subtitle
                        || subtitle.subdate != old.subdate
                        || subtitle.subupdate != old.subupdate
                }
                None => true,
            };

            let download_time = if needs_download {
                let (section, raw_html) = self.download_section(&setting, subtitle)?;
                self.save_section_file(&section_dir, subtitle, &section)?;
                self.save_raw_file(&raw_dir, subtitle, &raw_html)?;
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
        }

        let toc_file = TocFile {
            title: title.clone(),
            author: author.clone(),
            toc_url: toc_url.clone(),
            story: info
                .story
                .as_ref()
                .map(|s| s.replace("<br>", "\n")),

            subtitles: final_subtitles,
            novel_type: Some(novel_type),
        };
        self.save_toc_file(&novel_dir, &toc_file)?;
        self.ensure_default_files(&novel_dir, &title, &author, &toc_url);

        let record = NovelRecord {
            id: existing_id.unwrap_or(0),
            author,
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
            title,
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

    fn parse_subtitles_multipage(
        &mut self,
        setting: &SiteSetting,
        mut toc_source: &str,
        url_captures: &HashMap<String, String>,
    ) -> Result<Vec<SubtitleInfo>> {
        let mut all_subtitles = Vec::new();
        let mut page = 0;
        let max_pages = if let Some(pattern) = setting.toc_page_max_pattern() {
            pattern
                .captures(toc_source)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1)
                .max(1)
        } else {
            50
        };

        loop {
            let page_subs = self.parse_subtitles(setting, toc_source, url_captures)?;
            all_subtitles.extend(page_subs);

            page += 1;
            if page >= max_pages {
                break;
            }

            let next_toc_pattern = match setting.next_toc_pattern() {
                Some(p) => p,
                None => break,
            };

            let caps = match next_toc_pattern.captures(toc_source) {
                Some(c) => c,
                None => break,
            };

            let mut next_captures: HashMap<String, String> = HashMap::new();
            for name in next_toc_pattern.capture_names().flatten() {
                if let Some(m) = caps.name(name) {
                    next_captures.insert(name.to_string(), m.as_str().to_string());
                }
            }

            let next_url_val = match &setting.next_url {
                Some(u) => u.clone(),
                None => break,
            };
            let next_url = setting.get_next_url_with_captures(&next_url_val, &next_captures);

            self.rate_limiter.wait();
            let response = self.client.get(&next_url).send()?;
            if !response.status().is_success() {
                break;
            }
            let mut body = response.text()?;
            pretreatment_source(&mut body, setting.encoding());
            toc_source = Box::leak(body.into_boxed_str());
        }

        Ok(all_subtitles)
    }

    fn create_short_story_subtitles(
        &self,
        setting: &SiteSetting,
        toc_source: &str,
    ) -> Result<Vec<SubtitleInfo>> {
        let title = setting
            .resolve_info_pattern("t", toc_source)
            .unwrap_or_else(|| "短編".to_string());

        Ok(vec![SubtitleInfo {
            index: "1".to_string(),
            href: String::new(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: title,
            file_subtitle: sanitize_filename("短編"),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
        }])
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
            format!("{} {}", ncode, sanitized)
        } else {
            sanitize_filename(title)
        }
    }

    fn compute_novel_dir(
        &self,
        sitename: &str,
        file_title: &str,
        use_subdirectory: bool,
        ncode: &Option<String>,
    ) -> PathBuf {
        let mut dir = PathBuf::from(ARCHIVE_ROOT_DIR);
        dir.push(sitename);

        if use_subdirectory {
            if let Some(ncode) = ncode {
                if ncode.len() >= 2 {
                    dir.push(&ncode[..2]);
                }
            }
        }

        dir.push(file_title);
        dir
    }

    fn save_section_file(
        &self,
        section_dir: &PathBuf,
        subtitle: &SubtitleInfo,
        section: &SectionElement,
    ) -> Result<()> {
        let filename = format!("{} {}.yaml", subtitle.index, subtitle.file_subtitle);
        let path = section_dir.join(filename);
        let file_data = SectionFile {
            index: subtitle.index.clone(),
            href: subtitle.href.clone(),
            chapter: subtitle.chapter.clone(),
            subchapter: subtitle.subchapter.clone(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: subtitle.subupdate.clone(),
            download_time: Some(Utc::now().format("%Y-%m-%d %H:%M:%S%.6f %z").to_string()),
            element: section.clone(),
        };
        let yaml_body = serde_yaml::to_string(&file_data)?;
        let content = format!("---\n{}\n", yaml_body);
        std::fs::write(&path, content)?;
        Ok(())
    }

    fn save_raw_file(
        &self,
        raw_dir: &PathBuf,
        subtitle: &SubtitleInfo,
        raw_html: &str,
    ) -> Result<()> {
        let filename = format!("{} {}.html", subtitle.index, subtitle.file_subtitle);
        let path = raw_dir.join(filename);
        std::fs::write(&path, raw_html)?;
        Ok(())
    }

    fn load_toc_file(&self, novel_dir: &PathBuf) -> Option<TocFile> {
        let path = novel_dir.join("toc.yaml");
        let content = std::fs::read_to_string(&path).ok()?;
        serde_yaml::from_str(&content).ok()
    }

    fn fix_yaml_block_scalar(yaml: &str) -> String {
        let re = regex::Regex::new(r"(?m)^story:\s*\|[-+]?\s*$").unwrap();
        let result = re.replace_all(yaml, "story: |-").to_string();
        result
    }

    fn save_toc_file(&self, novel_dir: &PathBuf, toc: &TocFile) -> Result<()> {
        let path = novel_dir.join("toc.yaml");
        let yaml_body = serde_yaml::to_string(toc)?;
        let content = format!("---\n{}\n", Self::fix_yaml_block_scalar(&yaml_body));
        std::fs::write(&path, content)?;
        Ok(())
    }

    fn ensure_default_files(&self, novel_dir: &PathBuf, title: &str, author: &str, toc_url: &str) {
        let setting_path = novel_dir.join("setting.ini");
        if !setting_path.exists() {
            let default_ini = crate::converter::settings::IniData::new();
            let content = default_ini.to_ini_string();
            let _ = std::fs::write(&setting_path, content);
        }

        let replace_path = novel_dir.join("replace.txt");
        if !replace_path.exists() {
            let content = format!(
                "; 単純置換用ファイル\n;\n; 対象小説情報\n; タイトル: {}\n; 作者: {}\n; URL: {}\n;\n; 書式\n; 置換対象<tab>置換文字\n;\n; サンプル\n; 一〇歳\t十歳\n; 第一章\t［＃ゴシック体］第一章［＃ゴシック体終わり］\n;\n; 正規表現での置換などは converter.yaml で対応して下さい\n",
                title, author, toc_url
            );
            let _ = std::fs::write(&replace_path, content);
        }
    }
}

const ARCHIVE_ROOT_DIR: &str = "小説データ";

fn build_section_url(setting: &SiteSetting, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", setting.top_url(), href)
    } else {
        format!("{}/{}", setting.toc_url().trim_end_matches('/'), href)
    }
}

fn compile_html_pattern(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .size_limit(10_000_000)
        .build()
}

fn kakuyomu_preprocess(src: &mut String) {
    let magic = "KakuyomuPreprocessEvalMagicWord";
    if src.contains(magic) {
        return;
    }
    let re = match regex::Regex::new(
        r#"(?s)<script id="__NEXT_DATA__" type="application/json">(.+?)</script>"#,
    ) {
        Ok(r) => r,
        Err(_) => return,
    };
    let caps = match re.captures(src) {
        Some(c) => c,
        None => return,
    };
    let json_str = match caps.get(1) {
        Some(m) => m.as_str(),
        None => return,
    };
    let root: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return,
    };

    let state = match root
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .and_then(|p| p.get("__APOLLO_STATE__"))
    {
        Some(v) => v,
        None => return,
    };

    let work_id = match root.get("query").and_then(|q| q.get("workId")) {
        Some(v) => match v.as_str() {
            Some(s) => s.to_string(),
            None => return,
        },
        None => return,
    };

    let work_key = format!("Work:{}", work_id);
    let work = match state.get(&work_key) {
        Some(v) => v,
        None => return,
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("<!---"));
    lines.push(magic.to_string());

    let title = work.get("title").and_then(|v| v.as_str()).unwrap_or("");
    lines.push(format!("title::{}", title));

    let author = work
        .get("author")
        .and_then(|a| a.get("__ref"))
        .and_then(|r| r.as_str())
        .and_then(|r| state.get(r))
        .and_then(|a| a.get("activityName"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let alt_author = work
        .get("alternateAuthorName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let author_line = if !alt_author.is_empty() {
        format!("author::{}／{}", alt_author, author)
    } else {
        format!("author::{}", author)
    };
    lines.push(author_line);

    let intro = work
        .get("introduction")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .replace('\n', "<br>");
    lines.push(format!("introduction::{}", intro));

    let serial_status = work
        .get("serialStatus")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    lines.push(format!("serialStatus::{}", serial_status));

    let pub_count = work
        .get("publicEpisodeCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    lines.push(format!("publicEpisodeCount::{}", pub_count));

    let published_at = work
        .get("publishedAt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    lines.push(format!("publishedAt::{}", published_at));

    let edited_at = work.get("editedAt").and_then(|v| v.as_str()).unwrap_or("");
    lines.push(format!("editedAt::{}", edited_at));

    let last_ep_pub = work
        .get("lastEpisodePublishedAt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    lines.push(format!("lastEpisodePublishedAt::{}", last_ep_pub));

    let total_chars = work
        .get("totalCharacterCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    lines.push(format!("totalCharacterCount::{}", total_chars));

    if let Some(tags) = work.get("tagLabels").and_then(|v| v.as_array()) {
        for tag in tags {
            if let Some(t) = tag.as_str() {
                lines.push(format!("tag::{}", t));
            }
        }
    }

    let toc = match work.get("tableOfContents").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => return,
    };

    let mut toc_entries: Vec<String> = Vec::new();
    for toc_item in &toc {
        if let Some(toc_ref) = toc_item.get("__ref").and_then(|r| r.as_str()) {
            let resolved = match state.get(toc_ref) {
                Some(v) => v,
                None => continue,
            };
            let chapter = resolved.get("chapter");
            let episodes = resolved.get("episodeUnions");

            if let Some(ch) = chapter {
                if let Some(ch_ref) = ch.get("__ref").and_then(|r| r.as_str()) {
                    let ch_resolved = match state.get(ch_ref) {
                        Some(v) => v,
                        None => continue,
                    };
                    let level = ch_resolved
                        .get("level")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(1);
                    let ch_id = ch_resolved
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let ch_title = ch_resolved
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    toc_entries.push(format!("Chapter;{};{};{}", level, ch_id, ch_title));
                }
            }

            if let Some(ep_arr) = episodes.and_then(|v| v.as_array()) {
                for ep in ep_arr {
                    if let Some(ep_ref) = ep.get("__ref").and_then(|r| r.as_str()) {
                        let ep_resolved = match state.get(ep_ref) {
                            Some(v) => v,
                            None => continue,
                        };
                        let ep_id = ep_resolved.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let ep_pub = ep_resolved
                            .get("publishedAt")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let ep_title = ep_resolved
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        toc_entries.push(format!("Episode;{};{};{}", ep_id, ep_pub, ep_title));
                    }
                }
            }
        }
    }

    for entry in &toc_entries {
        lines.push(entry.clone());
    }

    lines.push(format!("--->"));

    if let Some(pos) = caps.get(0) {
        let insert_pos = pos.start();
        let block = lines.join("\n");
        src.insert_str(insert_pos, &block);
    }
}

pub fn pretreatment_source(src: &mut String, encoding: &str) {
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
    kakuyomu_preprocess(src);
}

fn decode_numeric_entities(src: &mut String) {
    let hex_re = regex::Regex::new(r"&#x([0-9a-fA-F]+);").unwrap();
    let dec_re = regex::Regex::new(r"&#(\d+);").unwrap();

    *src = hex_re
        .replace_all(src, |caps: &regex::Captures| {
            let code = u32::from_str_radix(&caps[1], 16).unwrap_or(0xFFFD);
            char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
        })
        .to_string();

    *src = dec_re
        .replace_all(src, |caps: &regex::Captures| {
            let code: u32 = caps[1].parse().unwrap_or(0xFFFD);
            char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
        })
        .to_string();
}

fn sanitize_filename(name: &str) -> String {
    let invalid = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    name.chars()
        .map(|c| if invalid.contains(&c) { '_' } else { c })
        .collect::<String>()
        .chars()
        .take(80)
        .collect()
}
