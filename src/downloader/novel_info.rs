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
    fn empty() -> Self {
        Self {
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
        }
    }

    pub fn load(
        setting: &SiteSetting,
        client: &reqwest::blocking::Client,
        toc_source: &str,
        url_captures: &HashMap<String, String>,
    ) -> Result<Self> {
        if let Some(novel_info_url) = &setting.novel_info_url {
            let resolved_url = setting
                .novel_info_url_with_captures(url_captures)
                .unwrap_or_else(|| setting.interpolate(novel_info_url));
            let response = client.get(&resolved_url).send()?;
            if !response.status().is_success() {
                return Ok(Self::empty());
            }
            let mut body = response.text()?;
            crate::downloader::pretreatment_source(&mut body, setting.encoding(), Some(setting));

            Ok(Self::from_novel_info_source(setting, &body))
        } else {
            Ok(Self::from_toc_source(setting, toc_source))
        }
    }

    pub fn from_novel_info_source(setting: &SiteSetting, source: &str) -> Self {
        let mut info = Self::empty();
        let keys = [
            "t", "w", "s", "nt", "ga", "gf", "nu", "gl", "l", "tags", "sitename",
        ];
        info.raw_captures = setting.multi_match(source, &keys);
        if info.raw_captures.is_empty() {
            return info;
        }

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
            .get("gf")
            .and_then(|s| parse_narou_date(s));
        info.general_lastup = info
            .raw_captures
            .get("gl")
            .and_then(|s| parse_narou_date(s));
        info.novelupdated_at = info
            .raw_captures
            .get("nu")
            .and_then(|s| parse_narou_date(s));
        info.length = info.raw_captures.get("l").and_then(|s| {
            s.replace(',', "").trim().parse().ok()
        });

        info
    }

    pub fn from_toc_source(setting: &SiteSetting, toc_source: &str) -> Self {
        let mut info = Self::empty();
        let keys = ["title", "author", "story", "tags"];
        info.raw_captures = setting.multi_match(toc_source, &keys);
        info.title = info.raw_captures.get("title").cloned();
        info.author = info.raw_captures.get("author").cloned();
        info.story = info.raw_captures.get("story").cloned();
        info.tags = info.raw_captures.get("tags").cloned();
        info
    }
}

fn parse_narou_date(s: &str) -> Option<DateTime<Utc>> {
    let s = normalize_narou_date(s);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    if let Ok(ts) = s.parse::<i64>() {
        return DateTime::from_timestamp(ts, 0);
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
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
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc());
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
            return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        }
    }

    None
}

fn normalize_narou_date(s: &str) -> String {
    let mut normalized = String::with_capacity(s.len());
    let mut skipping_paren = false;

    for ch in s.trim().chars() {
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
            _ => normalized.push(ch),
        }
    }

    normalized.trim().trim_end_matches(':').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_narou_date;
    use chrono::{Datelike, Timelike};

    #[test]
    fn parse_narou_date_accepts_kakuyomu_rfc3339() {
        let date = parse_narou_date("2021-01-10T16:13:02Z").expect("date");

        assert_eq!(date.year(), 2021);
        assert_eq!(date.month(), 1);
        assert_eq!(date.day(), 10);
        assert_eq!(date.hour(), 16);
        assert_eq!(date.minute(), 13);
        assert_eq!(date.second(), 2);
    }

    #[test]
    fn parse_narou_date_accepts_japanese_datetime_with_weekday() {
        let date = parse_narou_date("2026年04月17日(金) 07:00").expect("date");

        assert_eq!(date.year(), 2026);
        assert_eq!(date.month(), 4);
        assert_eq!(date.day(), 17);
        assert_eq!(date.hour(), 7);
        assert_eq!(date.minute(), 0);
        assert_eq!(date.second(), 0);
    }
}
