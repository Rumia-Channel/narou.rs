use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::error::Result;

use super::site_setting::SiteSetting;

pub struct NovelInfo {
    pub title: Option<String>,
    pub author: Option<String>,
    pub story: Option<String>,
    pub novel_type: Option<u8>,
    pub end: Option<bool>,
    pub general_firstup: Option<DateTime<Utc>>,
    pub general_lastup: Option<DateTime<Utc>>,
    pub novelupdated_at: Option<DateTime<Utc>>,
    pub length: Option<i64>,
    pub tags: Option<String>,
    pub sitename: Option<String>,
    pub raw_captures: HashMap<String, String>,
}

impl NovelInfo {
    pub fn load(
        setting: &SiteSetting,
        client: &reqwest::blocking::Client,
        toc_source: &str,
        url_captures: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut info = Self {
            title: None,
            author: None,
            story: None,
            novel_type: None,
            end: None,
            general_firstup: None,
            general_lastup: None,
            novelupdated_at: None,
            length: None,
            tags: None,
            sitename: None,
            raw_captures: HashMap::new(),
        };

        if let Some(novel_info_url) = &setting.novel_info_url {
            let resolved_url = setting
                .novel_info_url_with_captures(url_captures)
                .unwrap_or_else(|| setting.interpolate(novel_info_url));
            let response = client.get(&resolved_url).send()?;
            if !response.status().is_success() {
                return Ok(info);
            }
            let mut body = response.text()?;
            body.retain(|c| c != '\r');

            let keys = [
                "t", "w", "s", "nt", "ga", "gf", "nu", "gl", "l", "tags", "sitename",
            ];
            info.raw_captures = setting.multi_match(&body, &keys);

            info.title = info.raw_captures.get("t").cloned();
            info.author = info.raw_captures.get("w").cloned();
            info.story = info.raw_captures.get("s").cloned();
            info.tags = info.raw_captures.get("tags").cloned();
            info.sitename = info.raw_captures.get("sitename").cloned();

            if let Some(nt_text) = info.raw_captures.get("nt") {
                let (novel_type, is_end) = setting.get_novel_type_from_string(nt_text);
                info.novel_type = Some(novel_type);
                info.end = Some(is_end);
            }

            info.general_firstup = info
                .raw_captures
                .get("ga")
                .and_then(|s| parse_narou_date(s));
            info.general_lastup = info
                .raw_captures
                .get("gl")
                .and_then(|s| parse_narou_date(s));
            info.novelupdated_at = info
                .raw_captures
                .get("nu")
                .and_then(|s| parse_narou_date(s));
            info.length = info.raw_captures.get("l").and_then(|s| s.parse().ok());
        } else {
            let keys = ["title", "author", "story"];
            info.raw_captures = setting.multi_match(toc_source, &keys);
            info.title = info.raw_captures.get("title").cloned();
            info.author = info.raw_captures.get("author").cloned();
            info.story = info.raw_captures.get("story").cloned();
        }

        Ok(info)
    }
}

fn parse_narou_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    if let Ok(ts) = s.parse::<i64>() {
        return DateTime::from_timestamp(ts, 0);
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
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc());
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
            return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        }
    }

    None
}
