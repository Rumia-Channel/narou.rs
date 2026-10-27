//! お気に入りタグの Web API。
//!
//! `GET /api/favorite_tags` は現在の登録済みタグ、`POST /api/favorite_tags` は
//! add / remove / clear を受け付ける。保存先は在庫の `favorite_tags` キー
//! (Native は `.narou/favorite_tags.yaml` / SQLite `app_state`、Worker は D1)。
//!
//! タグ名は Web UI のタグ名検証と同じ規則を通す。

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;

use crate::application::favorite_tags::FavoriteTags;
use crate::application::webui::validate_web_tag_name;
use crate::native::favorite_tags as store;

use super::AppState;

#[derive(Debug, Deserialize, Default)]
pub struct FavoriteTagsRequest {
    /// `add` / `remove` / `clear`。
    pub action: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn payload(state: &FavoriteTags) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "tags": state.tags,
    })
}

pub async fn api_favorite_tags_get() -> Json<serde_json::Value> {
    match store::load() {
        Ok(state) => Json(payload(&state)),
        Err(error) => Json(serde_json::json!({"success":false,"message":error.to_string()})),
    }
}

pub async fn api_favorite_tags(
    State(_state): State<AppState>,
    Json(body): Json<FavoriteTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let action = body.action.as_deref().unwrap_or("add");
    let mut tags = Vec::new();
    for tag in &body.tags {
        match validate_web_tag_name(tag) {
            Ok(tag) => tags.push(tag),
            Err(message) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"success":false,"message":message})),
                );
            }
        }
    }

    let result = store::update(|state| match action {
        "add" => {
            for tag in &tags {
                state.add(tag);
            }
        }
        "remove" => {
            for tag in &tags {
                state.remove(tag);
            }
        }
        "clear" => {
            state.clear();
        }
        _ => {}
    });

    match result {
        Ok(state) => (StatusCode::OK, Json(payload(&state))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"success":false,"message":error.to_string()})),
        ),
    }
}
