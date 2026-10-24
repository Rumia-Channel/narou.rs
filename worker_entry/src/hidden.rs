//! 非表示設定の D1 実装と Web API。
//!
//! 保存形式は native と同じ `app_state(scope='inv', key='hidden')`。
//! `value_yaml` は YAML 管理への書き戻し互換のため、`value_json` と同じ
//! 内容を YAML でも保存する。

use narou_rs::error::{NarouError, Result};
use narou_rs::hidden::HiddenState;
use serde::Deserialize;
use serde_json::json;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response};

use crate::composition::WorkerRuntime;
use crate::db_handle::DbHandle;

const SELECT_SQL: &str =
    "SELECT value_json FROM app_state WHERE scope='inv' AND key='hidden'";
const SAVE_SQL: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','hidden',?1,?2
WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='hidden' AND value_json=?3)
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

async fn read(db: &DbHandle) -> Result<HiddenState> {
    let row = db
        .prepare(SELECT_SQL)
        .first::<Row>(None)
        .await
        .map_err(db_error)?;
    match row {
        Some(row) => Ok(serde_json::from_str(&row.value_json)?),
        None => Ok(HiddenState::default()),
    }
}

/// 全置換。同時更新はスナップショット CAS で検出して再試行する。
async fn write(db: &DbHandle, state: &HiddenState) -> Result<()> {
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
            if rows.iter().any(|row| row.key == "hidden") {
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
        if rows.iter().any(|row| row.key == "hidden") {
            return Ok(());
        }
    }
    Err(db_error("非表示設定が同時に更新されています。再試行してください"))
}

pub async fn load(runtime: &WorkerRuntime) -> Result<HiddenState> {
    read(runtime.write_db()).await
}

/// リクエストハンドラが `WorkerRuntime` を組み立てずに読むための入口。
pub async fn load_from_env(env: &Env) -> Result<HiddenState> {
    let db = DbHandle::primary(std::sync::Arc::new(env.d1("DB").map_err(db_error)?));
    read(&db).await
}

fn invalid(message: &str) -> NarouError {
    NarouError::InvalidTarget(message.to_string())
}

#[derive(Deserialize, Default)]
struct HiddenRequest {
    action: Option<String>,
    #[serde(default)]
    novels: Vec<i64>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    sites: Vec<String>,
}

fn payload(state: &HiddenState) -> serde_json::Value {
    json!({
        "success": true,
        "novels": state.novels,
        "tags": state.tags,
        "authors": state.authors,
        "sites": state.sites,
    })
}

pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if !matches!((req.path().as_str(), req.method()), ("/api/hidden", Method::Get | Method::Post)) {
        return Response::from_json(&json!({"success":false,"message":"Method Not Allowed"}))?
            .with_status(405);
    }
    let runtime = WorkerRuntime::build_ui(&env).await?;
    if req.method() == Method::Get {
        return match load(&runtime).await {
            Ok(state) => Response::from_json(&payload(&state)),
            Err(error) => Response::from_json(&json!({"success":false,"message":error.to_string()}))?
                .with_status(503),
        };
    }
    let body: HiddenRequest = match req.json().await {
        Ok(body) => body,
        Err(_) => {
            return Response::from_json(&json!({"success":false,"message":"invalid JSON body"}))?
                .with_status(400);
        }
    };
    let action = body.action.as_deref().unwrap_or("add");
    let db = runtime.write_db().clone();
    let mut state = match read(&db).await {
        Ok(state) => state,
        Err(error) => {
            return Response::from_json(&json!({"success":false,"message":error.to_string()}))?
                .with_status(503);
        }
    };
    match action {
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
        "clear" => state = HiddenState::default(),
        _ => {
            let error = invalid("unknown hidden action");
            return Response::from_json(&json!({"success":false,"message":error.to_string()}))?
                .with_status(400);
        }
    }
    match write(&db, &state).await {
        Ok(()) => Response::from_json(&payload(&state)),
        Err(error) => Response::from_json(&json!({"success":false,"message":error.to_string()}))?
            .with_status(503),
    }
}
