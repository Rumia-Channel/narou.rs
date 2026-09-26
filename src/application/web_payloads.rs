//! Web API の入出力ペイロード（native の Web サーバーと Worker で共有する）。
//!
//! `src/web/*` は native 専用ビルドに閉じているため、Worker 側ハンドラが同じ契約を
//! 満たすには型を共有できる場所が必要になる。ここに置いた型は serde だけに依存し、
//! どちらのビルドからも使える。

use serde::{Deserialize, Serialize};

/// 保存・削除など「成否とメッセージ」だけを返す応答。
#[derive(Debug, Serialize)]
pub struct ApiResponse {
    pub success: bool,
    pub message: String,
}

/// `GET/POST /api/list` のパラメータ。
///
/// axum はクエリ文字列と `application/x-www-form-urlencoded` 本文を同じ形に
/// 展開するため、キー名はブラケット込みのリテラル (`search[value]` 等) になる。
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub draw: Option<u64>,
    pub start: Option<u64>,
    pub length: Option<u64>,
    pub all: Option<bool>,
    pub filter: Option<String>,
    #[serde(rename = "search[value]")]
    pub search_value: Option<String>,
    #[serde(rename = "order[0][column]")]
    pub order_column: Option<u64>,
    #[serde(rename = "order[0][dir]")]
    pub order_dir: Option<String>,
}

/// DataTables 形式の一覧応答。
#[derive(Debug, Serialize)]
pub struct NovelListResponse {
    pub draw: u64,
    pub records_total: u64,
    pub records_filtered: u64,
    pub data: Vec<NovelListItem>,
}

/// 一覧 1 行分。日時は Unix epoch 秒で、`Option` は `null` になる。
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
    pub ncode: Option<String>,
    pub general_all_no: Option<i64>,
}
