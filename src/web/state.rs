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
    pub last_update: String,
    pub general_lastup: Option<String>,
    pub last_check_date: Option<String>,
    pub new_arrivals_date: Option<String>,
    pub tags: Vec<String>,
    pub new_arrivals: bool,
    pub frozen: bool,
    pub length: Option<i64>,
    pub toc_url: String,
    pub general_all_no: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct BatchIdsBody {
    pub ids: Vec<i64>,
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
pub struct CsvImportBody {
    pub csv: String,
}
