pub mod html;
pub mod info_cache;
pub mod novel_info;
pub mod preprocess;
pub mod rate_limit;
pub mod site_setting;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use reqwest::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONNECTION, HeaderMap, HeaderValue,
};
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
    user_agent: String,
    prefer_curl: bool,
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
        let client = reqwest::blocking::Client::builder()
            .user_agent(&ua)
            .default_headers(default_request_headers())
            .cookie_store(true)
            .http1_only()
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let site_settings = SiteSetting::load_all()?;

        Ok(Self {
            client,
            user_agent: ua,
            prefer_curl: false,
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

        if self.prefer_curl {
            if let Some(body) = self.fetch_text_with_curl(&url, setting.cookie()) {
                let mut body = body;
                pretreatment_source(&mut body, setting.encoding(), Some(setting));
                return Ok(body);
            }
        }

        let mut request = self.client.get(&url);

        if let Some(cookie) = setting.cookie() {
            request = request.header("Cookie", cookie);
        }

        let response = request.send()?;
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 403 {
                if let Some(body) = self.fetch_text_with_curl(&url, setting.cookie()) {
                    self.prefer_curl = true;
                    let mut body = body;
                    pretreatment_source(&mut body, setting.encoding(), Some(setting));

                    if let Some(error_pattern) = setting.error_message() {
                        if let Ok(re) = regex::Regex::new(error_pattern) {
                            if re.is_match(&body) {
                                return Err(NarouError::NotFound(
                                    "Novel deleted or private".into(),
                                ));
                            }
                        }
                    }

                    return Ok(body);
                }
            }
            if status.as_u16() == 503 {
                return Err(NarouError::SuspendDownload("Rate limited (503)".into()));
            }
            if status.as_u16() == 404 {
                return Err(NarouError::NotFound(url));
            }
            return Err(response.error_for_status().unwrap_err().into());
        }

        let mut body = response.text()?;
        pretreatment_source(&mut body, setting.encoding(), Some(setting));

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
        toc_url: &str,
    ) -> Result<(SectionElement, String)> {
        if let Some(cached) = self.section_cache.get(&subtitle.index) {
            return Ok((cached.clone(), String::new()));
        }

        self.rate_limiter.wait();
        let url = build_section_url(setting, toc_url, &subtitle.href);

        if self.prefer_curl {
            if let Some(mut html_source) = self.fetch_text_with_curl(&url, setting.cookie()) {
                pretreatment_source(&mut html_source, setting.encoding(), Some(setting));
                return self.parse_section_html(setting, subtitle, html_source);
            }
        }

        let mut request = self.client.get(&url);
        if let Some(cookie) = setting.cookie() {
            request = request.header("Cookie", cookie);
        }

        let response = request.send()?;
        let status = response.status();
        let mut html_source = if !status.is_success() {
            if status.as_u16() == 403 {
                if let Some(body) = self.fetch_text_with_curl(&url, setting.cookie()) {
                    self.prefer_curl = true;
                    body
                } else {
                    return Err(response.error_for_status().unwrap_err().into());
                }
            } else {
                if status.as_u16() == 503 {
                    return Err(NarouError::SuspendDownload("Rate limited (503)".into()));
                }
                return Err(response.error_for_status().unwrap_err().into());
            }
        } else {
            response.text()?
        };

        pretreatment_source(&mut html_source, setting.encoding(), Some(setting));
        self.parse_section_html(setting, subtitle, html_source)
    }

    fn parse_section_html(
        &mut self,
        setting: &SiteSetting,
        subtitle: &SubtitleInfo,
        html_source: String,
    ) -> Result<(SectionElement, String)> {
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

    fn fetch_text_with_curl(&self, url: &str, cookie: Option<&str>) -> Option<String> {
        let mut command = std::process::Command::new(curl_command());
        command
            .arg("--fail")
            .arg("--silent")
            .arg("--show-error")
            .arg("--location")
            .arg("--http1.1")
            .arg("--compressed")
            .arg("--user-agent")
            .arg(&self.user_agent)
            .arg("--header")
            .arg("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
            .arg("--header")
            .arg("Accept-Language: ja,en-US;q=0.9,en;q=0.8")
            .arg("--header")
            .arg(curl_accept_encoding_header())
            .arg("--header")
            .arg("Accept-Charset: utf-8")
            .arg("--header")
            .arg("Connection: keep-alive");

        if let Some(cookie) = cookie {
            command.arg("--header").arg(format!("Cookie: {cookie}"));
        }

        let output = command.arg(url).output().ok()?;
        if !output.status.success() {
            return None;
        }

        Some(String::from_utf8_lossy(&output.stdout).into_owned())
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

        if self.prefer_curl {
            if let Some(mut body) = self.fetch_text_with_curl(&resolved_url, setting.cookie()) {
                pretreatment_source(&mut body, setting.encoding(), Some(setting));
                return Ok(NovelInfo::from_novel_info_source(setting, &body));
            }
        }

        let mut request = self.client.get(&resolved_url);
        if let Some(cookie) = setting.cookie() {
            request = request.header("Cookie", cookie);
        }

        let response = request.send()?;
        if response.status().is_success() {
            let mut body = response.text()?;
            pretreatment_source(&mut body, setting.encoding(), Some(setting));
            return Ok(NovelInfo::from_novel_info_source(setting, &body));
        }

        if response.status().as_u16() == 403 {
            if let Some(mut body) = self.fetch_text_with_curl(&resolved_url, setting.cookie()) {
                self.prefer_curl = true;
                pretreatment_source(&mut body, setting.encoding(), Some(setting));
                return Ok(NovelInfo::from_novel_info_source(setting, &body));
            }
        }

        Ok(NovelInfo::from_toc_source(setting, toc_source))
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
        crate::db::novel_dir_for_record(&PathBuf::from(ARCHIVE_ROOT_DIR), record)
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
            self.create_short_story_subtitles(&setting, &toc_source)?
        } else {
            self.parse_subtitles_multipage(&setting, &toc_source, &url_captures)?
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
                let (section, raw_html) = self.download_section(&setting, subtitle, &toc_url)?;
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
            story: info.story.as_ref().map(|s| s.replace("<br>", "\n")),

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
            pretreatment_source(&mut body, setting.encoding(), Some(setting));
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
        let mut dir = PathBuf::from(ARCHIVE_ROOT_DIR);
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

fn default_request_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("ja,en-US;q=0.9,en;q=0.8"),
    );
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    );
    headers.insert(ACCEPT_CHARSET, HeaderValue::from_static("utf-8"));
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    headers
}

fn curl_command() -> &'static str {
    if cfg!(windows) { "curl.exe" } else { "curl" }
}

fn curl_accept_encoding_header() -> &'static str {
    if curl_supports_brotli() {
        "Accept-Encoding: gzip, deflate, br"
    } else {
        "Accept-Encoding: gzip, deflate"
    }
}

fn curl_supports_brotli() -> bool {
    static SUPPORTS_BROTLI: OnceLock<bool> = OnceLock::new();
    *SUPPORTS_BROTLI.get_or_init(|| {
        let output = std::process::Command::new(curl_command())
            .arg("-V")
            .output()
            .ok();
        output
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|text| {
                let lower = text.to_ascii_lowercase();
                lower.contains("brotli") || lower.contains("libbrotli")
            })
            .unwrap_or(false)
    })
}

fn build_section_url(setting: &SiteSetting, toc_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", setting.top_url(), href)
    } else if href.is_empty() {
        toc_url.to_string()
    } else {
        format!("{}/{}", toc_url.trim_end_matches('/'), href)
    }
}

fn compile_html_pattern(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .size_limit(10_000_000)
        .build()
}

pub fn pretreatment_source(src: &mut String, _encoding: &str, setting: Option<&SiteSetting>) {
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
    if let Some(setting) = setting {
        if let Some(pipeline) = setting.preprocess_pipeline() {
            preprocess::run_preprocess(pipeline, src);
        }
    }
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
        .collect::<String>()
        .trim_end_matches([' ', '.'])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::novel_info::NovelInfo;
    use super::site_setting::SiteSetting;

    #[test]
    fn sanitize_filename_removes_windows_trailing_dots_and_spaces() {
        assert_eq!(super::sanitize_filename("title. "), "title");
        assert_eq!(super::sanitize_filename("bad/name?"), "bad_name_");
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

        super::pretreatment_source(&mut html, "UTF-8", Some(setting));

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
