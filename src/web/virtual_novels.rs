//! バーチャル小説の Web API。
//!
//! 仮想小説は `novels` の通常行なので一覧 (`/api/list`) にはそのまま出る。
//! ここでは登録・メンバー順の変更・削除を扱う。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;

use crate::native::novel_repository::NativeNovelRepository;
use crate::native::virtual_novel::{self, NewVirtualNovel};
use crate::platform::NovelId;
use crate::virtual_novel as core;

use super::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateVirtualRequest {
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub sitename: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub members: Vec<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderVirtualRequest {
    pub members: Vec<i64>,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({"success": false, "message": message.into()})),
    )
}

pub async fn api_virtual_list() -> Json<serde_json::Value> {
    let repository = NativeNovelRepository::new();
    match virtual_novel::load() {
        Ok(members) => {
            let novels = members
                .groups
                .iter()
                .filter_map(|(id, members)| {
                    let record = repository.get_sync(NovelId(*id)).ok().flatten()?;
                    Some(serde_json::json!({
                        "id": record.id,
                        "ncode": core::ncode(*id),
                        "title": record.title,
                        "author": record.author,
                        "sitename": record.sitename,
                        "domain": record.domain,
                        "tags": record.tags,
                        "members": members,
                    }))
                })
                .collect::<Vec<_>>();
            Json(serde_json::json!({"success": true, "novels": novels}))
        }
        Err(error) => Json(serde_json::json!({"success": false, "message": error.to_string()})),
    }
}

pub async fn api_virtual_create(
    State(_state): State<AppState>,
    Json(body): Json<CreateVirtualRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match virtual_novel::create(NewVirtualNovel {
        title: body.title,
        author: body.author,
        sitename: body.sitename,
        domain: body.domain,
        tags: body.tags,
        members: body.members,
    }) {
        Ok(record) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "id": record.id,
                "ncode": core::ncode(record.id),
                "title": record.title,
            })),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_order(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ReorderVirtualRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match virtual_novel::reorder_by_ids(id, body.members) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true})),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_remove(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    match virtual_novel::remove(id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true})),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

