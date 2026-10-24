//! バーチャル小説の Worker API。
//!
//! 仮想小説も `novels` の通常行で、`ncode=vs_<ID>`。メンバー順は native と
//! 同じ `app_state(scope='inv', key='virtual_novel_members')` に保存する。
//! `POST /api/virtual_novels` の body は `action: create|order|remove`。

use chrono::Utc;
use narou_rs::db::NovelRecord;
use narou_rs::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelId, NovelMutation};
use narou_rs::virtual_novel::{self as core, VirtualNovelMembers};
use serde::Deserialize;
use serde_json::json;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response};

use crate::composition::WorkerRuntime;
use crate::db_handle::DbHandle;

const SELECT_SQL: &str =
    "SELECT value_json FROM app_state WHERE scope='inv' AND key='virtual_novel_members'";
const SAVE_SQL: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','virtual_novel_members',?1,?2
WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='virtual_novel_members' AND value_json=?3)
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

async fn read(db: &DbHandle) -> Result<VirtualNovelMembers> {
    let row = db
        .prepare(SELECT_SQL)
        .first::<Row>(None)
        .await
        .map_err(db_error)?;
    match row {
        Some(row) => serde_json::from_str(&row.value_json).map_err(db_error),
        None => Ok(VirtualNovelMembers::default()),
    }
}

async fn write(db: &DbHandle, members: &VirtualNovelMembers) -> Result<()> {    let encoded = serde_json::to_string(members).map_err(db_error)?;
    let yaml = serde_yaml::to_string(members)?;
    for _ in 0..8 {
        let Some(row) = db
            .prepare(SELECT_SQL)
            .first::<Row>(None)
            .await
            .map_err(db_error)?
        else {
            let rows = db
                .prepare(SAVE_SQL)
                .bind(&[encoded.clone().into(), yaml.clone().into(), JsValue::NULL])
                .map_err(db_error)?
                .all()
                .await
                .map_err(db_error)?
                .results::<KeyRow>()
                .map_err(db_error)?;
            if rows.iter().any(|row| row.key == "virtual_novel_members") {
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
        if rows.iter().any(|row| row.key == "virtual_novel_members") {
            return Ok(());
        }
    }
    Err(db_error(
        "バーチャル小説のメンバー構成が同時に更新されています。再試行してください",
    ))
}

/// リクエストハンドラが `WorkerRuntime` を組み立てずに読むための入口。
pub async fn load_from_env(env: &Env) -> Result<VirtualNovelMembers> {
    let db = DbHandle::primary(std::sync::Arc::new(env.d1("DB").map_err(db_error)?));
    read(&db).await
}

#[derive(Deserialize)]
struct VirtualRequest {
    action: Option<String>,
    id: Option<i64>,
    title: Option<String>,
    #[serde(default)]
    author: String,
    #[serde(default)]
    sitename: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    members: Vec<i64>,
}

async fn create(runtime: &WorkerRuntime, body: VirtualRequest) -> Result<NovelRecord> {
    let title = body
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| NarouError::InvalidTarget("タイトルを指定してください".into()))?;
    if body.members.is_empty() {
        return Err(NarouError::InvalidTarget(
            "メンバーを指定してください".into(),
        ));
    }
    let all = read(runtime.write_db()).await?;
    for member in &body.members {
        let record = runtime
            .novels
            .get(NovelId(*member))
            .await?
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {member}")))?;
        if record
            .ncode
            .as_deref()
            .is_some_and(core::is_virtual_ncode)
        {
            return Err(NarouError::InvalidTarget(
                "仮想小説をメンバーにはできません".into(),
            ));
        }
    }
    if all
        .groups
        .values()
        .any(|members| members.iter().any(|member| body.members.contains(member)))
    {
        return Err(NarouError::InvalidTarget(
            "既に別の仮想小説に属している作品があります".into(),
        ));
    }

    let reservation = runtime.novels.allocate_id().await?;
    let id = reservation.id().0;
    let ncode = core::ncode(id);
    let mut tags: Vec<String> = body
        .tags
        .iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.push(core::tag(id));
    tags.sort();
    tags.dedup();
    let limit = load_length_limit("folder-length-limit", Some(50));
    let file_title = sanitize_filename_with_limit(&title, limit);
    let file_title = if file_title.is_empty() {
        ncode.clone()
    } else {
        file_title
    };
    let record = NovelRecord {
        id,
        author: body.author.clone(),
        title: title.clone(),
        file_title,
        toc_url: format!("https://narou.rs/virtual/{ncode}"),
        sitename: body.sitename.clone(),
        novel_type: 0,
        end: true,
        last_update: Utc::now(),
        new_arrivals_date: None,
        use_subdirectory: false,
        general_firstup: None,
        novelupdated_at: None,
        general_lastup: None,
        last_mail_date: None,
        tags,
        ncode: Some(ncode),
        domain: body.domain.clone(),
        general_all_no: None,
        length: None,
        suspend: false,
        is_narou: false,
        last_check_date: None,
        convert_failure: false,
        requires_login: false,
        login_session: None,
        extra_fields: Default::default(),
    };
    runtime
        .novels
        .apply_batch(vec![NovelMutation::Upsert(record.clone())])
        .await?;
    if let Err(error) = write(
        runtime.write_db(),
        &{
            let mut updated = all;
            updated.insert(id, body.members.clone()).map_err(|message| {
                NarouError::InvalidTarget(message)
            })?;
            updated
        },
    )
    .await
    {
        let _ = runtime.novels.release_id(reservation).await;
        let _ = runtime
            .novels
            .apply_batch(vec![NovelMutation::Remove(NovelId(id))])
            .await;
        return Err(error);
    }
    Ok(record)
}

async fn reorder(runtime: &WorkerRuntime, id: i64, ordered: Vec<i64>) -> Result<()> {
    let mut members = read(runtime.write_db()).await?;
    members
        .reorder(id, ordered)
        .map_err(|message| NarouError::InvalidTarget(message))?;
    write(runtime.write_db(), &members).await
}

async fn remove(runtime: &WorkerRuntime, id: i64) -> Result<()> {
    let mut members = read(runtime.write_db()).await?;
    if members.remove(id).is_none() {
        return Err(NarouError::Database(format!(
            "仮想小説 vs_{id} は登録されていません"
        )));
    }
    write(runtime.write_db(), &members).await?;
    runtime
        .novels
        .apply_batch(vec![NovelMutation::Remove(NovelId(id))])
        .await?;
    Ok(())
}

pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    let runtime = WorkerRuntime::build_ui(&env).await?;
    match (req.path().as_str(), req.method()) {
        ("/api/virtual_novels", Method::Get) => {
            let members = match read(runtime.write_db()).await {
                Ok(members) => members,
                Err(error) => {
                    return Ok(Response::from_json(
                        &json!({"success":false,"message":error.to_string()}),
                    )?
                    .with_status(503));
                }
            };
            let mut novels = Vec::new();
            for (id, member_ids) in &members.groups {
                if let Some(record) = runtime
                    .novels
                    .get(NovelId(*id))
                    .await
                    .map_err(|error| worker::Error::RustError(error.to_string()))?
                {
                    novels.push(json!({
                        "id": record.id,
                        "ncode": core::ncode(*id),
                        "title": record.title,
                        "author": record.author,
                        "sitename": record.sitename,
                        "domain": record.domain,
                        "tags": record.tags,
                        "members": member_ids,
                    }));
                }
            }
            Ok(Response::from_json(
                &json!({"success":true,"novels":novels}),
            )?)
        }
        ("/api/virtual_novels", Method::Post) => {
            let body: VirtualRequest = match req.json().await {
                Ok(body) => body,
                Err(_) => {
                    return Ok(Response::from_json(
                        &json!({"success":false,"message":"invalid JSON body"}),
                    )?
                    .with_status(400));
                }
            };
            let result = match body.action.as_deref().unwrap_or("create") {
                "create" => create(&runtime, body).await.map(|record| {
                    json!({"success":true,"id":record.id,"ncode":core::ncode(record.id),"title":record.title})
                }),
                "order" => {
                    let Some(id) = body.id else {
                        return Ok(Response::from_json(
                            &json!({"success":false,"message":"id が必要です"}),
                        )?
                        .with_status(400));
                    };
                    reorder(&runtime, id, body.members)
                        .await
                        .map(|()| json!({"success":true}))
                }
                "remove" => {
                    let Some(id) = body.id else {
                        return Ok(Response::from_json(
                            &json!({"success":false,"message":"id が必要です"}),
                        )?
                        .with_status(400));
                    };
                    remove(&runtime, id)
                        .await
                        .map(|()| json!({"success":true}))
                }
                _ => Err(NarouError::InvalidTarget(
                    "未知の action です".to_string(),
                )),
            };
            match result {
                Ok(payload) => Ok(Response::from_json(&payload)?),
                Err(error) => Ok(Response::from_json(
                    &json!({"success":false,"message":error.to_string()}),
                )?
                .with_status(400)),
            }
        }
        _ => Ok(Response::from_json(
            &json!({"success":false,"message":"Method Not Allowed"}),
        )?
        .with_status(405)),
    }
}
