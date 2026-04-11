use serde::{Deserialize, Serialize};

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
    pub author: String,
    pub novel_dir: std::path::PathBuf,
    pub new_novel: bool,
    pub updated_count: usize,
    pub total_count: usize,
}

pub const SECTION_SAVE_DIR: &str = "本文";
pub const RAW_DATA_DIR: &str = "raw";
pub const CACHE_SAVE_DIR: &str = "cache";
pub const MAX_SECTION_CACHE: usize = 20;
pub const ARCHIVE_ROOT_DIR: &str = "小説データ";
