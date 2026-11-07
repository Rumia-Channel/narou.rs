//! お気に入りタグの D1 実装と Web API。
//!
//! 保存形式は native と同じ `app_state(scope='inv', key='favorite_tags')`。
//! `value_yaml` は YAML 管理への書き戻し互換のため、`value_json` と同じ
//! 内容を YAML でも保存する。
//!
//! native の `src/web/favorite_tags.rs` と同じ JSON 形・検証規則・
//! ステータスコードを返す。

use std::sync::Arc;

use narou_rs::application::favorite_tags::{FavoriteTags, FavoriteTagsMutation, FavoriteTagsRequest};
use narou_rs::error::{NarouError, Result};
use serde::Deserialize;
use serde_json::json;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response};

use crate::db_handle::DbHandle;

const SELECT_SQL: &str =
    "SELECT value_json FROM app_state WHERE scope='inv' AND key='favorite_tags'";
const SAVE_SQL: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','favorite_tags',?1,?2
WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='favorite_tags' AND value_json=?3)
ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml
WHERE ?3 IS NOT NULL AND app_state.value_json=?3
RETURNING key
"#;

#[derive(Deserialize)]
struct Row {
    value_json: String,
}

#[derive(Deserialize)]
struct KeyRow {
    key: String,
}

fn db_error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(value.to_string())
}

async fn read(db: &DbHandle) -> Result<FavoriteTags> {
    let row = db
        .prepare(SELECT_SQL)
        .first::<Row>(None)
        .await
        .map_err(db_error)?;
    match row {
        Some(row) => serde_json::from_str(&row.value_json).map_err(db_error),
        None => Ok(FavoriteTags::default()),
    }
}

/// 変更を最新スナップショットへ適用し、競合時は読み直してから再適用する。
async fn update(db: &DbHandle, mutation: &FavoriteTagsMutation) -> Result<(FavoriteTags, bool)> {
    for _ in 0..8 {
        let row = db.prepare(SELECT_SQL).first::<Row>(None).await.map_err(db_error)?;
        let mut state: FavoriteTags = match &row {
            Some(row) => serde_json::from_str(&row.value_json).map_err(db_error)?,
            None => FavoriteTags::default(),
        };
        if !mutation.apply(&mut state) {
            return Ok((state, false));
        }
        let encoded = serde_json::to_string(&state).map_err(db_error)?;
        let yaml = serde_yaml::to_string(&state)?;
        let expected = row.map_or(JsValue::NULL, |row| row.value_json.into());
        let rows = db.prepare(SAVE_SQL)
            .bind(&[encoded.into(), yaml.into(), expected]).map_err(db_error)?
            .all().await.map_err(db_error)?
            .results::<KeyRow>().map_err(db_error)?;
        if rows.iter().any(|row| row.key == "favorite_tags") {
            return Ok((state, true));
        }
    }
    Err(db_error("お気に入りタグが同時に更新されています。再試行してください"))
}


pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if !matches!(
        (req.path().as_str(), req.method()),
        ("/api/favorite_tags", Method::Get | Method::Post)
    ) {
        return Ok(Response::from_json(
            &json!({"success":false,"message":"Method Not Allowed"}),
        )?
        .with_status(405));
    }
    let db = DbHandle::primary(Arc::new(env.d1("DB")?));
    if req.method() == Method::Get {
        return Ok(match read(&db).await {
            Ok(state) => Response::from_json(&state.payload())?,
            Err(error) => Response::from_json(
                &json!({"success":false,"message":error.to_string()}),
            )?
            .with_status(503),
        });
    }
    let body: FavoriteTagsRequest = match req.json().await {
        Ok(body) => body,
        Err(_) => {
            return Ok(Response::from_json(
                &json!({"success":false,"message":"invalid JSON body"}),
            )?
            .with_status(400));
        }
    };
    let mutation = match body.validate() {
        Ok(mutation) => mutation,
        Err(message) => return Ok(Response::from_json(
            &json!({"success":false,"message":message}),
        )?.with_status(400)),
    };
    Ok(match update(&db, &mutation).await {
        Ok((state, changed)) => {
            if changed {
                let push = crate::push_hub::PushHubClient::new(&env, crate::budget::SubrequestBudget::new());
                push.broadcast_best_effort(&[
                    narou_rs::application::push_events::event("favorite_tags.reload", ""),
                ]).await;
            }
            Response::from_json(&state.payload())?
        }
        Err(error) => Response::from_json(
            &json!({"success":false,"message":error.to_string()}),
        )?
        .with_status(503),
    })
}


/// D1 ハンドルからお気に入りタグを読む。
///
/// お気に入りの変更直後の再描画で古い replica の順序へ戻さないため、
/// この小さな状態行は primary から読む。
pub(crate) async fn load_with_db(db: &DbHandle) -> Result<FavoriteTags> {
    read(&db.primary_view()).await
}
