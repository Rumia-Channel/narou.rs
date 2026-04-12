use std::collections::HashMap;

use crate::error::Result;

use super::fetch::HttpFetcher;
use super::site_setting::SiteSetting;
use super::types::{SectionElement, SubtitleInfo, MAX_SECTION_CACHE};
use super::util::{build_section_url, compile_html_pattern, pretreatment_source};

pub struct SectionCache {
    cache: HashMap<String, SectionElement>,
}

impl SectionCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    pub fn get(&self, index: &str) -> Option<&SectionElement> {
        self.cache.get(index)
    }

    pub fn insert(&mut self, index: String, element: SectionElement) {
        if self.cache.len() >= MAX_SECTION_CACHE {
            if let Some(oldest_key) = self.cache.keys().next().cloned() {
                self.cache.remove(&oldest_key);
            }
        }
        self.cache.insert(index, element);
    }
}

pub fn download_section(
    fetcher: &mut HttpFetcher,
    cache: &mut SectionCache,
    setting: &SiteSetting,
    subtitle: &SubtitleInfo,
    toc_url: &str,
) -> Result<(SectionElement, String)> {
    if let Some(cached) = cache.get(&subtitle.index) {
        return Ok((cached.clone(), String::new()));
    }

    fetcher.rate_limiter.wait();
    let url = build_section_url(setting, toc_url, &subtitle.href);

    let html_source = fetcher.fetch_text(&url, setting.cookie())?;
    let mut html_source = html_source;
    pretreatment_source(&mut html_source, setting.encoding(), Some(setting));
    parse_section_html(cache, setting, subtitle, html_source)
}

pub fn parse_section_html(
    cache: &mut SectionCache,
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

    cache.insert(subtitle.index.clone(), element.clone());

    Ok((element, html_source))
}
