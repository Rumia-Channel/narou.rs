pub mod html;
pub mod rate_limit;
pub mod site_setting;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::db::DATABASE;
use crate::error::{NarouError, Result};

use self::rate_limit::RateLimiter;
use self::site_setting::SiteSetting;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postscript: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, Copy)]
pub enum TargetType {
    Url,
    Ncode,
    Id,
    Other,
}

const SECTION_SAVE_DIR: &str = "本文";
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

    pub fn fetch_toc(&mut self, setting: &SiteSetting) -> Result<String> {
        self.rate_limiter.wait();
        let url = setting.toc_url();
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
                .unwrap_or_default();
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
    ) -> Result<SectionElement> {
        if let Some(cached) = self.section_cache.get(&subtitle.index) {
            return Ok(cached.clone());
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

        let mut body = response.text()?;
        pretreatment_source(&mut body, setting.encoding());

        let mut element = SectionElement {
            data_type: "html".to_string(),
            introduction: None,
            postscript: None,
            body: String::new(),
        };

        if let Some(pattern) = setting.introduction_pattern() {
            if let Ok(re) = regex::Regex::new(pattern) {
                if let Some(caps) = re.captures(&body) {
                    element.introduction =
                        caps.name("introduction").map(|m| m.as_str().to_string());
                }
            }
        }

        if let Some(pattern) = setting.postscript_pattern() {
            if let Ok(re) = regex::Regex::new(pattern) {
                if let Some(caps) = re.captures(&body) {
                    element.postscript = caps.name("postscript").map(|m| m.as_str().to_string());
                }
            }
        }

        if let Some(pattern) = setting.body_pattern() {
            if let Ok(re) = regex::Regex::new(pattern) {
                if let Some(caps) = re.captures(&body) {
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

        Ok(element)
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

fn pretreatment_source(src: &mut String, _encoding: &str) {
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
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
