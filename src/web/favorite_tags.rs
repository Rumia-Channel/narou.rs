//! お気に入りタグの Web API。
//!
//! `GET /api/favorite_tags` は現在の登録済みタグ、`POST /api/favorite_tags` は
//! add / remove / clear / reorder を受け付ける。保存先は在庫の `favorite_tags` キー
//! (Native は `.narou/favorite_tags.yaml` / SQLite `app_state`、Worker は D1)。
//!
//! タグ名は Web UI のタグ名検証と同じ規則を通す。

use axum::{Json, extract::State, http::StatusCode};

use crate::application::favorite_tags::FavoriteTagsRequest;
use crate::native::favorite_tags as store;

use super::AppState;

pub async fn api_favorite_tags_get() -> (StatusCode, Json<serde_json::Value>) {
    match store::load() {
        Ok(state) => (StatusCode::OK, Json(state.payload())),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"success":false,"message":error.to_string()}))),
    }
}

pub async fn api_favorite_tags(
    State(app): State<AppState>,
    Json(body): Json<FavoriteTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mutation = match body.validate() {
        Ok(mutation) => mutation,
        Err(message) => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success":false,"message":message})),
        ),
    };
    let mut changed = false;
    let result = store::update(|state| { changed = mutation.apply(state); });

    match result {
        Ok(state) => {
            if changed {
                app.push_server.broadcast_event("favorite_tags.reload", "");
            }
            (StatusCode::OK, Json(state.payload()))
        }
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"success":false,"message":error.to_string()})),
        ),
    }
}
