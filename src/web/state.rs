use serde::{Deserialize, Serialize};

/// 保存・削除などの応答、一覧の入出力は Worker と共有する。
pub use crate::application::web_payloads::{
    ApiResponse, ListParams, NovelListItem, NovelListResponse,
};

#[derive(Debug, Deserialize)]
pub struct IdPath {
    pub id: i64,
}




#[derive(Debug, Deserialize)]
pub struct BatchIdsBody {
    pub ids: Vec<i64>,
    #[serde(default)]
    pub with_file: Option<bool>,
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
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
    #[serde(default)]
    pub targets: Vec<serde_json::Value>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub update_all: bool,
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ConvertBody {
    pub targets: Vec<String>,
    pub device: Option<String>,
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct LogsParams {
    pub count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct TargetsBody {
    pub targets: Vec<serde_json::Value>,
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct IdsBody {
    pub ids: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct EditTagBody {
    pub ids: Vec<serde_json::Value>,
    pub states: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
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
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct TagInfoBody {
    pub ids: Vec<serde_json::Value>,
    #[serde(default)]
    pub with_exclusion: Option<bool>,
    #[serde(default)]
    pub sort_state: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmRunningTasksBody {
    #[serde(default)]
    pub rerun: Option<String>,
}
