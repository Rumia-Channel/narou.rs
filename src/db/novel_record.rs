use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NovelRecord {
    pub id: i64,
    pub author: String,
    pub title: String,
    pub file_title: String,
    pub toc_url: String,
    pub sitename: String,
    #[serde(default)]
    pub novel_type: u8,
    #[serde(default)]
    pub end: bool,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub last_update: DateTime<Utc>,
    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub new_arrivals_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub use_subdirectory: bool,
    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub general_firstup: Option<DateTime<Utc>>,
    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub novelupdated_at: Option<DateTime<Utc>>,
    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub general_lastup: Option<DateTime<Utc>>,
    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub last_mail_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub ncode: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub general_all_no: Option<i64>,
    #[serde(default)]
    pub length: Option<i64>,
    #[serde(default)]
    pub suspend: bool,
    #[serde(default)]
    pub is_narou: bool,
}
