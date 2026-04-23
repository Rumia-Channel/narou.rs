use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct ApiResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct IdPath {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub draw: Option<u64>,
    pub start: Option<u64>,
    pub length: Option<u64>,
    pub all: Option<bool>,
    #[serde(rename = "search[value]")]
    pub search_value: Option<String>,
    #[serde(rename = "order[0][column]")]
    pub order_column: Option<u64>,
    #[serde(rename = "order[0][dir]")]
    pub order_dir: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NovelListResponse {
    pub draw: u64,
    pub records_total: u64,
    pub records_filtered: u64,
    pub data: Vec<NovelListItem>,
}

#[derive(Debug, Serialize)]
pub struct NovelListItem {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub novel_type: u8,
    pub end: bool,
    pub last_update: i64,
    pub general_lastup: Option<i64>,
    pub last_check_date: Option<i64>,
    pub new_arrivals_date: Option<i64>,
    pub tags: Vec<String>,
    pub new_arrivals: bool,
    pub frozen: bool,
    pub suspend: bool,
    pub length: Option<i64>,
    pub toc_url: String,
    pub general_all_no: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct BatchIdsBody {
    pub ids: Vec<i64>,
    #[serde(default)]
    pub with_file: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TagBody {
    pub tag: String,
}

#[derive(Debug, Deserialize)]
pub struct TagsBody {
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct DownloadBody {
    pub targets: Vec<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub mail: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    pub targets: Vec<serde_json::Value>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct ConvertBody {
    pub targets: Vec<String>,
    pub device: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogsParams {
    pub count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct TargetsBody {
    pub targets: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct IdsBody {
    pub ids: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct EditTagBody {
    pub ids: Vec<serde_json::Value>,
    pub states: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct CsvImportBody {
    pub csv: String,
}

#[derive(Debug, Deserialize)]
pub struct DiffBody {
    pub ids: Vec<serde_json::Value>,
    #[serde(default = "default_diff_number")]
    pub number: String,
}

fn default_diff_number() -> String {
    "1".to_string()
}

#[derive(Debug, Deserialize)]
pub struct DiffCleanBody {
    pub target: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct TaskIdBody {
    pub task_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    pub task_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateByTagBody {
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub exclusion_tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TagInfoBody {
    pub ids: Vec<serde_json::Value>,
    #[serde(default)]
    pub with_exclusion: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmRunningTasksBody {
    #[serde(default)]
    pub rerun: Option<String>,
}
