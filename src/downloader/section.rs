use std::collections::HashMap;

use crate::error::Result;

use super::fetch::HttpFetcher;
use super::site_setting::SiteSetting;
use super::types::{MAX_SECTION_CACHE, SectionElement, SubtitleInfo};
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

    pub fn get(&self, key: &str) -> Option<&SectionElement> {
        self.cache.get(key)
    }

    pub fn insert(&mut self, key: String, element: SectionElement) {
        if self.cache.len() >= MAX_SECTION_CACHE {
            if let Some(oldest_key) = self.cache.keys().next().cloned() {
                self.cache.remove(&oldest_key);
            }
        }
        self.cache.insert(key, element);
    }
}

fn section_cache_key(setting: &SiteSetting, toc_url: &str, subtitle: &SubtitleInfo) -> String {
    build_section_url(setting, toc_url, &subtitle.href)
}

pub fn download_section(
    fetcher: &mut HttpFetcher,
    cache: &mut SectionCache,
    setting: &SiteSetting,
    subtitle: &SubtitleInfo,
    toc_url: &str,
) -> Result<(SectionElement, String)> {
    let url = section_cache_key(setting, toc_url, subtitle);
    if let Some(cached) = cache.get(&url) {
        return Ok((cached.clone(), String::new()));
    }

    fetcher.rate_limiter.wait_for_url(&url);

    let html_source = fetcher.fetch_text(&url, setting.cookie(), Some(setting.encoding()))?;
    let mut html_source = html_source;
    pretreatment_source(&mut html_source, setting.encoding(), Some(setting));
    let (element, raw_html) = parse_section_html(setting, html_source)?;
    cache.insert(url, element.clone());
    Ok((element, raw_html))
}

pub fn parse_section_html(
    setting: &SiteSetting,
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

    Ok((element, html_source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtitle(index: &str, href: &str) -> SubtitleInfo {
        SubtitleInfo {
            index: index.to_string(),
            href: href.to_string(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "subtitle".to_string(),
            file_subtitle: "subtitle".to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
        }
    }

    #[test]
    fn section_cache_key_uses_resolved_section_url_not_index() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "kakuyomu.jp").unwrap();
        let toc_url = "https://kakuyomu.jp/works/111";

        let first = subtitle("10", "/works/111/episodes/10");
        let second = subtitle("10", "/works/222/episodes/10");

        assert_ne!(
            section_cache_key(setting, toc_url, &first),
            section_cache_key(setting, toc_url, &second)
        );
    }
}
