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
    pub members: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderVirtualRequest {
    pub members: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AddMembersRequest {
    pub members: Vec<serde_json::Value>,
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
                    let member_titles = members
                        .iter()
                        .map(|member| {
                            repository
                                .get_sync(NovelId(*member))
                                .ok()
                                .flatten()
                                .map(|record| record.title)
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>();
                    Some(serde_json::json!({
                        "id": record.id,
                        "ncode": core::ncode(*id),
                        "title": record.title,
                        "author": record.author,
                        "sitename": record.sitename,
                        "domain": record.domain,
                        "tags": record.tags,
                        "members": members,
                        "member_titles": member_titles,
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
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match virtual_novel::create(NewVirtualNovel {
        title: body.title,
        author: body.author,
        sitename: body.sitename,
        domain: body.domain,
        tags: body.tags,
        members,
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
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match virtual_novel::reorder_by_ids(id, members) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true})),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_add_members(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddMembersRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match virtual_novel::add_members(id, members) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "id": id,
                "ncode": core::ncode(id),
            })),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub async fn api_virtual_convert(
    State(_state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
        let record = NativeNovelRepository::new()
            .get_sync(NovelId(id))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("仮想小説 vs_{id} が見つかりません"))?;
        let members = virtual_novel::load()
            .map_err(|error| error.to_string())?
            .get(id)
            .map(|members| members.to_vec())
            .ok_or_else(|| format!("仮想小説 vs_{id} のメンバーがありません"))?;
        let path = virtual_novel::convert_virtual(id, &record, &members, None, false, false)
            .map_err(|error| error.to_string())?;
        Ok((
            core::ncode(id),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
        ))
    })
    .await;
    match result {
        Ok(Ok((ncode, filename))) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true, "ncode": ncode, "filename": filename})),
        ),
        Ok(Err(message)) => error_response(StatusCode::BAD_REQUEST, message),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "変換処理が中断されました",
        ),
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

