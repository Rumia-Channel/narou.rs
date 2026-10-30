//! バーチャル小説の Worker API。
//!
//! 仮想小説も `novels` の通常行で、`ncode=vs_<ID>`。メンバー順は native と
//! 同じ `app_state(scope='inv', key='virtual_novel_members')` に保存する。
//! `POST /api/virtual_novels` の body は `action: create|order|remove`。

use std::sync::Arc;

use chrono::Utc;
use narou_rs::db::NovelRecord;
use narou_rs::downloader::types::TocFile;
use narou_rs::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelId, NovelMutation, NovelObjectKeys, NovelRepository};
use narou_rs::virtual_novel::{self as core, AttributeComposition, VirtualNovelMembers};
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

async fn composition(db: &DbHandle, members: &[i64]) -> Result<AttributeComposition> {
    core::validate_members(members).map_err(NarouError::InvalidTarget)?;
    let repository = crate::d1_repository::D1NovelRepository::new(db.primary_view());
    let mut result = AttributeComposition::default();
    for id in members {
        let record = repository.get(NovelId(*id)).await?
            .ok_or_else(|| NarouError::NotFound(format!("作品が見つかりません: {id}")))?;
        if record.ncode.as_deref().is_some_and(core::is_virtual_ncode) {
            return Err(NarouError::InvalidTarget("仮想小説をメンバーにはできません".into()));
        }
        result.add(record.attribute_tag());
    }
    Ok(result)
}

/// Reconcile only virtual rows, keeping a mixed work's stored selection.
/// Correlated member lookups use the primary-key index; no full library scan.
pub(crate) async fn reconcile_attributes(db: &DbHandle) -> Result<()> {
    db.prepare(r#"
WITH groups AS (
  SELECT CAST(g.key AS INTEGER) AS id, g.value AS members
  FROM app_state a, json_each(a.value_json) g
  WHERE a.scope='inv' AND a.key='virtual_novel_members'
), classified AS (
  SELECT g.id, CASE
    WHEN NOT EXISTS(SELECT 1 FROM json_each(g.members) m JOIN novels n ON n.id=m.value WHERE COALESCE(n.attribute_tag,'小説')<>'漫画') THEN '漫画'
    WHEN NOT EXISTS(SELECT 1 FROM json_each(g.members) m JOIN novels n ON n.id=m.value WHERE n.attribute_tag='漫画') THEN '小説'
    ELSE COALESCE(v.attribute_tag,'小説') END AS attribute
  FROM groups g JOIN novels v ON v.id=g.id
  WHERE json_array_length(g.members)>0
    AND NOT EXISTS(SELECT 1 FROM json_each(g.members) m LEFT JOIN novels n ON n.id=m.value WHERE n.id IS NULL)
)
UPDATE novels SET attribute_tag=(SELECT attribute FROM classified WHERE classified.id=novels.id)
WHERE id IN (SELECT id FROM classified)
  AND attribute_tag IS NOT (SELECT attribute FROM classified WHERE classified.id=novels.id)
"#).run().await.map_err(db_error)?;
    Ok(())
}

#[derive(Deserialize)]
struct AttributeBody { attribute_tag: String }

pub async fn handle_attribute(mut req: Request, env: Env, id: i64) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await { return response; }
    if req.method() != Method::Post { return Response::error("Method Not Allowed", 405); }
    let body: AttributeBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return Ok(Response::from_json(&json!({"success":false,"message":"invalid JSON body"}))?.with_status(400)),
    };
    let db = DbHandle::primary(Arc::new(env.d1("DB")?));
    let result = async {
        let snapshot = db.prepare(SELECT_SQL).first::<Row>(None).await.map_err(db_error)?
            .ok_or_else(|| db_error("バーチャル小説は登録されていません"))?.value_json;
        let groups: VirtualNovelMembers = serde_json::from_str(&snapshot).map_err(db_error)?;
        let ids = groups.get(id).ok_or_else(|| NarouError::NotFound(format!("仮想小説 vs_{id} は登録されていません")))?;
        let types = composition(&db, ids).await?;
        if !types.is_mixed() {
            return Err(NarouError::InvalidTarget("混在していないバーチャル小説の属性タグは変更できません".into()));
        }
        let attribute = types.resolve(Some(&body.attribute_tag), None).map_err(NarouError::InvalidTarget)?;
        let member_json = serde_json::to_string(ids).map_err(db_error)?;
        let updated = db.prepare(r#"
UPDATE novels SET attribute_tag=?1 WHERE id=?2
AND EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='virtual_novel_members' AND value_json=?3)
AND EXISTS(SELECT 1 FROM json_each(?4) m JOIN novels n ON n.id=m.value WHERE n.attribute_tag='漫画')
AND EXISTS(SELECT 1 FROM json_each(?4) m JOIN novels n ON n.id=m.value WHERE COALESCE(n.attribute_tag,'小説')='小説')
AND NOT EXISTS(SELECT 1 FROM json_each(?4) m LEFT JOIN novels n ON n.id=m.value WHERE n.id IS NULL)
RETURNING id"#)
            .bind(&[attribute.into(), (id as f64).into(), snapshot.into(), member_json.into()]).map_err(db_error)?
            .all().await.map_err(db_error)?.results::<IdRow>().map_err(db_error)?;
        if updated.is_empty() { return Err(db_error("メンバー構成が変更されました。再試行してください")); }
        Ok(attribute)
    }.await;
    match result {
        Ok(attribute) => Response::from_json(&json!({"success":true,"id":id,"mixed":true,"attribute_tag":attribute})),
        Err(error) => Ok(Response::from_json(&json!({"success":false,"message":error.to_string()}))?.with_status(400)),
    }
}

#[derive(Deserialize)]
struct IdRow { #[serde(rename="id")] _id: i64 }

pub async fn handle_classify(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await { return response; }
    if req.method() != Method::Post { return Response::error("Method Not Allowed", 405); }
    let body: OrderBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return Ok(Response::from_json(&json!({"success":false,"message":"invalid JSON body"}))?.with_status(400)),
    };
    let db = DbHandle::primary(Arc::new(env.d1("DB")?));
    let result = async {
        let members = core::parse_member_ids(&body.members).map_err(NarouError::InvalidTarget)?;
        let types = composition(&db, &members).await?;
        let tag = types.resolve(None,None).map_err(NarouError::InvalidTarget)?;
        Ok::<_,NarouError>((types.is_mixed(),tag))
    }.await;
    match result {
        Ok((mixed,tag)) => Response::from_json(&json!({"success":true,"mixed":mixed,"attribute_tag":tag})),
        Err(error) => Ok(Response::from_json(&json!({"success":false,"message":error.to_string()}))?.with_status(400)),
    }
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
    members: Vec<serde_json::Value>,
    attribute_tag: Option<String>,
}

/// メンバーの目次から話数を数える (`general_all_no` が未設定の古いレコード用)。
async fn count_sections(runtime: &WorkerRuntime, record: &NovelRecord) -> Result<i64> {
    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
        .map_err(|error| NarouError::Platform(error.to_string()))?;
    let Some(bytes) = runtime.objects().read_small(&keys.toc()).await? else {
        return Ok(0);
    };
    let toc: TocFile = serde_yaml::from_slice(&bytes)
        .map_err(|error| NarouError::Platform(format!("toc.yaml: {error}")))?;
    Ok(toc.subtitles.len() as i64)
}

async fn create(runtime: &WorkerRuntime, body: VirtualRequest) -> Result<NovelRecord> {    let title = body
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| NarouError::InvalidTarget("タイトルを指定してください".into()))?;
    let members = core::parse_member_ids(&body.members).map_err(NarouError::InvalidTarget)?;
    if members.is_empty() {
        return Err(NarouError::InvalidTarget(
            "メンバーを指定してください".into(),
        ));
    }
    let all = read(runtime.write_db()).await?;
    let summary = summarize_members(runtime, &members).await?;
    if all
        .groups
        .values()
        .any(|existing| existing.iter().any(|member| members.contains(member)))
    {
        return Err(NarouError::InvalidTarget(
            "既に別の仮想小説に属している作品があります".into(),
        ));
    }
    if body.tags.iter().any(|tag| narou_rs::application::attribute_tags::is_attribute_tag(tag.trim())) {
        return Err(NarouError::InvalidTarget("属性タグは編集できません".into()));
    }

    let types = composition(runtime.write_db(), &members).await?;
    let attribute = types.resolve(body.attribute_tag.as_deref(),None).map_err(NarouError::InvalidTarget)?;
    let reservation = runtime.novels.allocate_id().await?;
    let id = reservation.id().0;
    let ncode = core::ncode(id);
    let mut tags: Vec<String> = body
        .tags
        .iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .chain(summary.tags.into_iter().filter(|tag| !narou_rs::application::attribute_tags::is_attribute_tag(tag)))
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
    let mut record = NovelRecord {
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
        general_lastup: summary.latest_update,
        last_mail_date: None,
        tags,
        ncode: Some(ncode),
        domain: body.domain.clone(),
        general_all_no: (summary.general_all_no > 0).then_some(summary.general_all_no),
        length: None,
        suspend: false,
        is_narou: false,
        last_check_date: None,
        convert_failure: false,
        requires_login: false,
        login_session: None,
        extra_fields: Default::default(),
    };
    record.set_attribute_tag(attribute);
    runtime
        .novels
        .apply_batch(vec![NovelMutation::Upsert(record.clone())])
        .await?;
    if let Err(error) = write(
        runtime.write_db(),
        &{
            let mut updated = all;
            updated.insert(id, members.clone()).map_err(|message| {
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

/// メンバー構成から仮想小説の派生値 (タグ・話数・最新話掲載日) を求める。
struct MemberSummary {
    tags: Vec<String>,
    latest_update: Option<chrono::DateTime<Utc>>,
    general_all_no: i64,
}

async fn summarize_members(runtime: &WorkerRuntime, members: &[i64]) -> Result<MemberSummary> {
    let mut summary = MemberSummary {
        tags: Vec::new(),
        latest_update: None,
        general_all_no: 0,
    };
    for member in members {
        let record = runtime
            .novels
            .get(NovelId(*member))
            .await?
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {member}")))?;
        if record.ncode.as_deref().is_some_and(core::is_virtual_ncode) {
            return Err(NarouError::InvalidTarget(format!(
                "仮想小説をメンバーにはできません: {member}"
            )));
        }
        summary.tags.extend(
            record
                .tags
                .iter()
                .filter(|tag| !core::is_internal_tag(tag))
                .cloned(),
        );
        if let Some(updated) = record.general_lastup
            && summary
                .latest_update
                .is_none_or(|current| updated > current)
        {
            summary.latest_update = Some(updated);
        }
        let count = match record.general_all_no {
            Some(count) if count > 0 => count,
            _ => count_sections(runtime, &record).await.unwrap_or(0),
        };
        summary.general_all_no += count;
    }
    Ok(summary)
}

async fn refresh_record(runtime: &WorkerRuntime, id: i64, summary: &MemberSummary) -> Result<()> {
    let record = runtime
        .novels
        .get(NovelId(id))
        .await?
        .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} が見つかりません")))?;
    let mut tags: Vec<String> = record
        .tags
        .iter()
        .filter(|tag| !core::is_internal_tag(tag))
        .cloned()
        .chain(summary.tags.iter().filter(|tag| !narou_rs::application::attribute_tags::is_attribute_tag(tag)).cloned())
        .collect();
    tags.push(core::tag(id));
    tags.sort();
    tags.dedup();
    let db = runtime.write_db();
    let snapshot = crate::tag_sequence::read_snapshot(db).await?;
    let before = if snapshot.sequence.is_initialized() { None } else { Some(crate::tag_sequence::library_tags(db).await?) };
    let written: Vec<&str> = tags.iter().map(String::as_str).filter(|tag| !core::is_internal_tag(tag)).collect();
    let next = crate::tag_sequence::advance(&snapshot, before.as_ref(), &written);
    let fold = narou_rs::db::novel_codec::fold;
    let folded: Vec<String> = tags.iter().map(|tag| fold(tag)).collect();
    let date = summary.latest_update.map(|date| date.to_rfc3339()).map(JsValue::from).unwrap_or(JsValue::NULL);
    let mut statements = vec![
        db.prepare("UPDATE novels SET tags_json=?1,tags_fold=?2,tags_sort=?3,general_all_no=?4,general_lastup=?5 WHERE id=?6 AND tags_json=?7")
            .bind(&[serde_json::to_string(&tags).map_err(db_error)?.into(),folded.join("\n").into(),folded.join("\u{1f}").into(),
                if summary.general_all_no>0 {(summary.general_all_no as f64).into()}else{JsValue::NULL},date,(id as f64).into(),
                serde_json::to_string(&record.tags).map_err(db_error)?.into()]).map_err(db_error)?,
        db.prepare("INSERT INTO novel_id_sequence(id,next_id) SELECT 1,0 WHERE changes()=0"),
        db.prepare("DELETE FROM novel_tags WHERE novel_id=?1").bind(&[(id as f64).into()]).map_err(db_error)?,
    ];
    for (position,tag) in tags.iter().enumerate() {
        statements.push(db.prepare("INSERT INTO novel_tags(novel_id,position,tag,tag_fold) VALUES (?1,?2,?3,?4)")
            .bind(&[(id as f64).into(),(position as f64).into(),tag.clone().into(),fold(tag).into()]).map_err(db_error)?);
    }
    statements.push(crate::tag_sequence::upsert_statement(db,snapshot.raw_json.as_deref(),&next)?);
    statements.push(crate::tag_sequence::guard_statement(db,&next)?);
    db.batch(statements).await.map_err(db_error)?;
    Ok(())
}

/// 既存の仮想小説へメンバーを追加する (native の `add_members` と同じ規則)。
async fn add_members(runtime: &WorkerRuntime, id: i64, new_members: Vec<i64>) -> Result<()> {
    let current = read(runtime.write_db()).await?;
    let mut updated = current.clone();
    updated
        .append(id, new_members.clone())
        .map_err(NarouError::InvalidTarget)?;
    let merged = updated.get(id).unwrap_or_default().to_vec();
    let summary = summarize_members(runtime, &merged).await?;
    let previous = current.get(id).map(<[i64]>::to_vec).unwrap_or_default();
    write(runtime.write_db(), &updated).await?;
    if let Err(error) = refresh_record(runtime, id, &summary).await {
        // レコードだけ古いまま残らないよう、メンバー構成も元へ戻す。
        let mut restored = read(runtime.write_db()).await.unwrap_or_default();
        restored.remove(id);
        if restored.insert(id, previous).is_ok() {
            let _ = write(runtime.write_db(), &restored).await;
        }
        return Err(error);
    }
    reconcile_attributes(runtime.write_db()).await?;
    Ok(())
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

/// `/api/virtual_novels/{id}/download.epub` — メンバーを登録順に結合した EPUB。
pub async fn download_epub(req: Request, env: Env, id: i64) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = WorkerRuntime::build_ui(&env).await?;
    let record = match runtime
        .novels
        .get(NovelId(id))
        .await
        .map_err(|error| worker::Error::RustError(error.to_string()))?
    {
        Some(record) => record,
        None => return Response::error("Not Found", 404),
    };
    let members = match read(runtime.write_db()).await {
        Ok(members) => members,
        Err(error) => return Response::error(format!("State error: {error}"), 503),
    };
    let Some(member_ids) = members.get(id) else {
        return Response::error("Not Found", 404);
    };
    let mut text = format!("{}\n{}\n", record.title, record.author);
    for member_id in member_ids {
        let Some(member) = runtime
            .novels
            .get(NovelId(*member_id))
            .await
            .map_err(|error| worker::Error::RustError(error.to_string()))?
        else {
            return Response::error(format!("Member {member_id} not found"), 404);
        };
        let converted = match crate::convert::convert_text_only(&runtime, &member).await {
            Ok(text) => text,
            Err(error) => return Response::error(format!("Convert failed: {error}"), 409),
        };
        text.push_str("［＃改ページ］\n［＃３字下げ］［＃大見出し］");
        text.push_str(&member.title);
        text.push_str("［＃大見出し終わり］\n［＃改ページ］\n");
        text.push_str(&converted);
    }

    let options = narou_rs::epub_lite::EpubBuildOptions {
        title: record.title.clone(),
        author: record.author.clone(),
        vertical: true,
        cover_from_first_image: false,
        assets_dir: None,
        kindle: false,
        extra_assets: narou_rs::epub_lite::narou_font_assets(false, false, 1.8),
        rotate_image: None,
    };
    let source = std::sync::Arc::new(narou_rs::epub_lite::EpubTextSource::new(
        text,
        Vec::new(),
    ));
    let preparation = match narou_rs::epub_lite::prepare_book_from_source(source, &options) {
        Ok(preparation) => preparation,
        Err(error) => return Response::error(format!("EPUB preparation failed: {error}"), 500),
    };
    let book = match preparation.finish(Vec::new(), None, &options) {
        Ok(book) => book,
        Err(error) => return Response::error(format!("EPUB build failed: {error}"), 500),
    };
    let sink = narou_rs::epub_lite::ChunkSink::new();
    if let Err(error) = narou_rs::epub_lite::stream_epub(&book.book, sink.clone(), |_| None) {
        return Response::error(format!("EPUB write failed: {error}"), 500);
    }
    let bytes = sink.take();
    let local_map = crate::load_local_settings_map(&env).await;
    let naming_settings = narou_rs::converter::settings::NovelSettings::default();
    let filename = crate::epub_download_filename(&record, &naming_settings, &local_map);
    let headers = worker::Headers::new();
    headers.set("Content-Type", "application/epub+zip")?;
    headers.set(
        "Content-Disposition",
        &format!("attachment; filename=\"{}\"", filename.replace('"', "")),
    )?;
    headers.set("Cache-Control", "no-store")?;
    Response::from_bytes(bytes).map(|response| response.with_headers(headers))
}

#[derive(Deserialize)]
struct OrderBody {
    members: Vec<serde_json::Value>,
}

pub async fn handle_order(mut req: Request, env: Env, id: i64) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = WorkerRuntime::build_ui(&env).await?;
    let body: OrderBody = match req.json().await {
        Ok(body) => body,
        Err(_) => {
            return Ok(Response::from_json(
                &json!({"success":false,"message":"invalid JSON body"}),
            )?
            .with_status(400));
        }
    };
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => return Ok(Response::from_json(&json!({"success":false,"message":message}))?
            .with_status(400)),
    };
    match reorder(&runtime, id, members).await {
        Ok(()) => Ok(Response::from_json(&json!({"success":true}))?),
        Err(error) => Ok(Response::from_json(
            &json!({"success":false,"message":error.to_string()}),
        )?
        .with_status(400)),
    }
}

pub async fn handle_remove(req: Request, env: Env, id: i64) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = WorkerRuntime::build_ui(&env).await?;
    match remove(&runtime, id).await {
        Ok(()) => Ok(Response::from_json(&json!({"success":true}))?),
        Err(error) => Ok(Response::from_json(
            &json!({"success":false,"message":error.to_string()}),
        )?
        .with_status(400)),
    }
}

pub async fn handle_add_members(mut req: Request, env: Env, id: i64) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = WorkerRuntime::build_ui(&env).await?;
    let body: OrderBody = match req.json().await {
        Ok(body) => body,
        Err(_) => {
            return Ok(Response::from_json(
                &json!({"success":false,"message":"invalid JSON body"}),
            )?
            .with_status(400));
        }
    };
    let members = match core::parse_member_ids(&body.members) {
        Ok(members) => members,
        Err(message) => {
            return Ok(
                Response::from_json(&json!({"success":false,"message":message}))?.with_status(400),
            );
        }
    };
    match add_members(&runtime, id, members).await {
        Ok(()) => Ok(Response::from_json(
            &json!({"success":true,"id":id,"ncode":core::ncode(id)}),
        )?),
        Err(error) => Ok(Response::from_json(
            &json!({"success":false,"message":error.to_string()}),
        )?
        .with_status(400)),
    }
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
            reconcile_attributes(runtime.write_db()).await.map_err(|error| worker::Error::RustError(error.to_string()))?;
            for (id, member_ids) in &members.groups {
                if let Some(record) = runtime
                    .novels
                    .get(NovelId(*id))
                    .await
                    .map_err(|error| worker::Error::RustError(error.to_string()))?
                {
                    let mut member_titles = Vec::with_capacity(member_ids.len());
                    let classification = composition(runtime.write_db(), member_ids).await.ok();
                    let mixed = classification.as_ref().is_some_and(AttributeComposition::is_mixed);
                    for member_id in member_ids {
                        member_titles.push(
                            runtime
                                .novels
                                .get(NovelId(*member_id))
                                .await
                                .map_err(|error| worker::Error::RustError(error.to_string()))?
                                .map(|member| member.title)
                                .unwrap_or_default(),
                        );
                    }
                    novels.push(json!({
                        "id": record.id,
                        "ncode": core::ncode(*id),
                        "title": record.title,
                        "author": record.author,
                        "sitename": record.sitename,
                        "domain": record.domain,
                        "tags": record.tags,
                        "members": member_ids,
                        "member_titles": member_titles,
                        "mixed": mixed,
                        "attribute_tag": record.attribute_tag(),
                        "attribute_editable": classification.is_some(),
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
                    let members = match core::parse_member_ids(&body.members) {
                        Ok(members) => members,
                        Err(message) => {
                            return Ok(Response::from_json(
                                &json!({"success":false,"message":message}),
                            )?
                            .with_status(400));
                        }
                    };
                    reorder(&runtime, id, members)
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
