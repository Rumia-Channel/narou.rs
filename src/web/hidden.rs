//! 非表示設定の Web API。
//!
//! `GET /api/hidden` は現在の設定、`POST /api/hidden` は add/remove/clear を
//! 受け付ける。表示の絞り込みは `/api/list` が `show_hidden` を見て行う。

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;

use crate::hidden::HiddenState;
use crate::native::hidden as store;

use super::AppState;

#[derive(Debug, Deserialize, Default)]
pub struct HiddenRequest {
    /// `add` / `remove` / `clear`。
    pub action: Option<String>,
    #[serde(default)]
    pub novels: Vec<i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub sites: Vec<String>,
}

fn payload(state: &HiddenState) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "novels": state.novels,
        "tags": state.tags,
        "authors": state.authors,
        "sites": state.sites,
    })
}

pub async fn api_hidden_get() -> Json<serde_json::Value> {
    match store::load() {
        Ok(state) => Json(payload(&state)),
        Err(error) => Json(serde_json::json!({"success":false,"message":error.to_string()})),
    }
}

pub async fn api_hidden(
    State(_state): State<AppState>,
    Json(body): Json<HiddenRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let action = body.action.as_deref().unwrap_or("add");
    let result = store::update(|state| match action {
        "add" => {
            for id in &body.novels {
                state.add_novel(*id);
            }
            for tag in &body.tags {
                state.add_tag(tag);
            }
            for author in &body.authors {
                state.add_author(author);
            }
            for site in &body.sites {
                state.add_site(site);
            }
        }
        "remove" => {
            for id in &body.novels {
                state.remove_novel(*id);
            }
            for tag in &body.tags {
                state.remove_tag(tag);
            }
            for author in &body.authors {
                state.remove_author(author);
            }
            for site in &body.sites {
                state.remove_site(site);
            }
        }
        "clear" => {
            *state = HiddenState::default();
        }
        other => {
            let _ = other;
        }
    });
    match result {
        Ok(state) => (StatusCode::OK, Json(payload(&state))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"success":false,"message":error.to_string()})),
        ),
    }
}
