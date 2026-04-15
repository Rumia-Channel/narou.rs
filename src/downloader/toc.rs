use std::collections::HashMap;

use crate::error::{NarouError, Result};

use super::fetch::HttpFetcher;
use super::site_setting::SiteSetting;
use super::types::SubtitleInfo;
use super::util::{pretreatment_source, sanitize_filename};

pub fn fetch_toc(
    fetcher: &mut HttpFetcher,
    setting: &SiteSetting,
    toc_url: &str,
) -> Result<String> {
    fetcher.rate_limiter.wait();

    let body = fetcher.fetch_text(toc_url, setting.cookie(), Some(setting.encoding()))?;
    let mut body = body;
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

pub fn parse_subtitles_multipage(
    fetcher: &mut HttpFetcher,
    setting: &SiteSetting,
    mut toc_source: &str,
    url_captures: &HashMap<String, String>,
    title: &str,
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

    if max_pages >= 5 && !title.is_empty() {
        println!("{} の目次ページを取得中...", title);
    }

    loop {
        let page_subs = parse_subtitles(setting, toc_source, url_captures)?;
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

        fetcher.rate_limiter.wait();
        let response = fetcher.client.get(&next_url).send()?;
        if !response.status().is_success() {
            break;
        }
        let mut body = response.text()?;
        pretreatment_source(&mut body, setting.encoding(), Some(setting));
        toc_source = Box::leak(body.into_boxed_str());
    }

    Ok(all_subtitles)
}

pub fn create_short_story_subtitles(
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
