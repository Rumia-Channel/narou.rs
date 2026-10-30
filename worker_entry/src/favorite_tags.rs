//! お気に入りタグの D1 実装と Web API。
//!
//! 保存形式は native と同じ `app_state(scope='inv', key='favorite_tags')`。
//! `value_yaml` は YAML 管理への書き戻し互換のため、`value_json` と同じ
//! 内容を YAML でも保存する。
//!
//! native の `src/web/favorite_tags.rs` と同じ JSON 形・検証規則・
//! ステータスコードを返す。

use narou_rs::application::favorite_tags::FavoriteTags;
use narou_rs::application::webui::validate_web_tag_name;
use narou_rs::error::{NarouError, Result};
use serde::Deserialize;
use serde_json::json;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response};

use crate::composition::WorkerRuntime;
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

/// 全置換。同時更新はスナップショット CAS で検出して再試行する。
async fn write(db: &DbHandle, state: &FavoriteTags) -> Result<()> {
    let encoded = serde_json::to_string(state).map_err(db_error)?;
    let yaml = serde_yaml::to_string(state)?;
    for _ in 0..8 {
        let Some(row) = db
            .prepare(SELECT_SQL)
            .first::<Row>(None)
            .await
            .map_err(db_error)?
        else {
            // 行が無い = まだ誰も保存していない。INSERT の CAS は ?3 IS NULL。
            let rows = db
                .prepare(SAVE_SQL)
                .bind(&[encoded.clone().into(), yaml.clone().into(), JsValue::NULL])
                .map_err(db_error)?
                .all()
                .await
                .map_err(db_error)?
                .results::<KeyRow>()
                .map_err(db_error)?;
            if rows.iter().any(|row| row.key == "favorite_tags") {
                return Ok(());
            }
            continue;
        };
        let rows = db
            .prepare(SAVE_SQL)
            .bind(&[
                encoded.clone().into(),
                yaml.clone().into(),
                row.value_json.clone().into(),
            ])
            .map_err(db_error)?
            .all()
            .await
            .map_err(db_error)?
            .results::<KeyRow>()
            .map_err(db_error)?;
        if rows.iter().any(|row| row.key == "favorite_tags") {
            return Ok(());
        }
    }
    Err(db_error("お気に入りタグが同時に更新されています。再試行してください"))
}

#[derive(Deserialize, Default)]
struct FavoriteTagsRequest {
    /// `add` / `remove` / `clear` / `reorder`。
    action: Option<String>,
    /// 追加・削除するタグ、または `reorder` のときの並び順。
    #[serde(default)]
    tags: Vec<String>,
}

fn payload(state: &FavoriteTags) -> serde_json::Value {
    json!({
        "success": true,
        "tags": state.tags,
    })
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
    let runtime = WorkerRuntime::build_ui(&env).await?;
    if req.method() == Method::Get {
        return Ok(match load(&runtime).await {
            Ok(state) => Response::from_json(&payload(&state))?,
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
    let action = body.action.as_deref().unwrap_or("add");
    let mut tags = Vec::new();
    for tag in &body.tags {
        match validate_web_tag_name(tag) {
            Ok(tag) => tags.push(tag),
            Err(message) => {
                return Ok(Response::from_json(
                    &json!({"success":false,"message":message}),
                )?
                .with_status(400));
            }
        }
    }
    let db = runtime.write_db().clone();
    let mut state = match read(&db).await {
        Ok(state) => state,
        Err(error) => {
            return Ok(Response::from_json(
                &json!({"success":false,"message":error.to_string()}),
            )?
            .with_status(503));
        }
    };
    match action {
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
        "reorder" => {
            state.reorder(&tags);
        }
        _ => {
            return Ok(Response::from_json(
                &json!({"success":false,"message":"unknown favorite tags action"}),
            )?
            .with_status(400));
        }
    }
    Ok(match write(&db, &state).await {
        Ok(()) => Response::from_json(&payload(&state))?,
        Err(error) => Response::from_json(
            &json!({"success":false,"message":error.to_string()}),
        )?
        .with_status(503),
    })
}

async fn load(runtime: &WorkerRuntime) -> Result<FavoriteTags> {
    read(runtime.write_db()).await
}

/// D1 ハンドルからお気に入りタグを読む。
///
/// タグ一覧の表示順 (お気に入り優先) で使う。読み取り専用の経路なので
/// UI セッションのハンドルでよい。
pub(crate) async fn load_with_db(db: &DbHandle) -> Result<FavoriteTags> {
    read(db).await
}
