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
use narou_rs::platform::{NovelObjectKeys, NovelRepository};
use narou_rs::transfer::ArchiveNovel;
use narou_rs::virtual_novel::{self as core, AttributeComposition, VirtualNovelMembers};
use serde::Deserialize;
use serde_json::json;
use worker::wasm_bindgen::JsValue;
use worker::{D1PreparedStatement, Env, Method, Request, Response};

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
"#;

#[derive(Deserialize)]
struct Row {
    value_json: String,
}

struct Snapshot {
    raw_json: Option<String>,
    members: VirtualNovelMembers,
}

#[derive(Deserialize, PartialEq)]
struct ParentSnapshot {
    tags_json: String,
    attribute_tag: Option<String>,
}

#[derive(Clone, Copy)]
enum MembershipMutation<'a> {
    Create { record: &'a NovelRecord, members: &'a [i64] },
    Append { id: i64, members: &'a [i64] },
    Reorder { id: i64, ordered: &'a [i64] },
    /// Replace the whole membership of an existing group (import overwrite).
    Replace { id: i64, metadata: &'a ArchiveNovel, members: &'a [i64] },
    Remove(i64),
}

/// Guard for a new parent: the id and the portable identity must be free and
/// every member must still exist as an ordinary work.
///
/// The insert collides with the single sequence row, so the whole batch is
/// aborted: a publication never replaces an existing record.
fn guard_new_parent(
    db: &DbHandle,
    record: &NovelRecord,
    member_ids: &[i64],
) -> Result<D1PreparedStatement> {
    let members = serde_json::to_string(member_ids).map_err(db_error)?;
    db.prepare(
        "INSERT INTO novel_id_sequence(id,next_id) SELECT 1,0 WHERE \
EXISTS(SELECT 1 FROM novels WHERE id=?1) \
OR EXISTS(SELECT 1 FROM novels WHERE toc_url_fold=?2) \
OR EXISTS(SELECT 1 FROM json_each(?3) m LEFT JOIN novels n ON n.id=m.value WHERE n.id IS NULL) \
OR EXISTS(SELECT 1 FROM json_each(?3) m JOIN novels n ON n.id=m.value \
WHERE n.ncode GLOB 'vs_[0-9]*' AND substr(n.ncode,4) NOT GLOB '*[^0-9]*')",
    ).bind(&[
        (record.id as f64).into(),
        narou_rs::db::novel_codec::fold(&record.toc_url).into(),
        members.into(),
    ]).map_err(db_error)
}

fn db_error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(value.to_string())
}

async fn read_snapshot(db: &DbHandle) -> Result<Snapshot> {
    let row = db.prepare(SELECT_SQL).first::<Row>(None).await.map_err(db_error)?;
    Ok(match row {
        Some(row) => Snapshot {
            members: serde_json::from_str(&row.value_json).map_err(db_error)?,
            raw_json: Some(row.value_json),
        },
        None => Snapshot { raw_json: None, members: VirtualNovelMembers::default() },
    })
}

async fn read(db: &DbHandle) -> Result<VirtualNovelMembers> {
    Ok(read_snapshot(db).await?.members)
}

async fn parent_snapshot(db: &DbHandle, id: i64) -> Result<Option<ParentSnapshot>> {
    db.prepare("SELECT tags_json,attribute_tag FROM novels WHERE id=?1")
        .bind(&[(id as f64).into()]).map_err(db_error)?
        .first::<ParentSnapshot>(None).await.map_err(db_error)
}

async fn mutate_membership(runtime: &WorkerRuntime, mutation: MembershipMutation<'_>) -> Result<()> {
    let db = runtime.write_db();
    for _ in 0..8 {
        let Snapshot { raw_json, mut members } = read_snapshot(db).await?;
        let mut statements = Vec::new();
        let mut guarded_parent = None;
        let mut guarded_composition = None;
        let written_tags: Option<std::borrow::Cow<'_, [String]>> = match mutation {
            MembershipMutation::Create { record, members: added } => {
                members.insert(record.id, added.to_vec()).map_err(NarouError::InvalidTarget)?;
                // The id and the portable identity must be free and every
                // member must still be an ordinary work: the guard aborts the
                // batch, so the publication never replaces a record.
                statements.push(guard_new_parent(db, record, added)?);
                crate::d1_repository::push_upsert_statements(db, record, &mut statements)?;
                Some(std::borrow::Cow::Borrowed(&record.tags))
            }
            MembershipMutation::Append { id, members: added } => {
                members.append(id, added.to_vec()).map_err(NarouError::InvalidTarget)?;
                let member_ids = members.get(id).expect("validated virtual novel");
                let summary = summarize_members(runtime, member_ids).await?;
                let parent = parent_snapshot(db, id).await?
                    .ok_or_else(|| NarouError::NotFound(format!("仮想小説 vs_{id} が見つかりません")))?;
                let existing: Vec<String> = serde_json::from_str(&parent.tags_json).map_err(db_error)?;
                let tags = summary.merged_tags(id, &existing);
                let attribute = summary.composition.resolve(None, parent.attribute_tag.as_deref())
                    .map_err(NarouError::InvalidTarget)?;
                let fold = narou_rs::db::novel_codec::fold;
                let folded: Vec<String> = tags.iter().map(|tag| fold(tag)).collect();
                let date = summary.latest_update.map(|date| date.to_rfc3339()).map(JsValue::from).unwrap_or(JsValue::NULL);
                statements.push(db.prepare(
                    r#"UPDATE novels SET tags_json=?1,tags_fold=?2,tags_sort=?3,general_all_no=?4,general_lastup=?5,attribute_tag=?6
WHERE id=?7 AND tags_json=?8 AND attribute_tag IS ?9
AND NOT EXISTS(SELECT 1 FROM json_each(?10) m LEFT JOIN novels n ON n.id=m.value WHERE n.id IS NULL)
AND EXISTS(SELECT 1 FROM json_each(?10) m CROSS JOIN novels n ON n.id=m.value WHERE COALESCE(n.attribute_tag,'小説')='小説')=?11
AND EXISTS(SELECT 1 FROM json_each(?10) m CROSS JOIN novels n ON n.id=m.value WHERE n.attribute_tag='漫画')=?12"#
                ).bind(&[
                    serde_json::to_string(&tags).map_err(db_error)?.into(), folded.join("\n").into(), folded.join("\u{1f}").into(),
                    if summary.general_all_no > 0 { (summary.general_all_no as f64).into() } else { JsValue::NULL },
                    date, attribute.into(), (id as f64).into(), parent.tags_json.as_str().into(),
                    parent.attribute_tag.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),
                    serde_json::to_string(member_ids).map_err(db_error)?.into(),
                    (u8::from(summary.composition.has_novel) as f64).into(),
                    (u8::from(summary.composition.has_manga) as f64).into(),
                ]).map_err(db_error)?);
                statements.push(db.prepare("INSERT INTO novel_id_sequence(id,next_id) SELECT 1,0 WHERE changes()=0"));
                crate::d1_repository::push_tag_statements(db, id, &tags, &mut statements)?;
                guarded_parent = Some((id, parent));
                guarded_composition = Some((member_ids, summary.composition));
                Some(std::borrow::Cow::Owned(tags))
            }
            MembershipMutation::Reorder { id, ordered } => {
                members.reorder(id, ordered.to_vec()).map_err(NarouError::InvalidTarget)?;
                None
            }
            MembershipMutation::Replace { id, metadata, members: replacement } => {
                core::validate_members(replacement).map_err(NarouError::InvalidTarget)?;
                if !members.groups.contains_key(&id) {
                    return Err(NarouError::NotFound(format!("バーチャル小説 vs_{id} は登録されていません")));
                }
                // A member belongs to at most one group.
                for member in replacement {
                    if let Some(owner) = members.owner_of(*member) && owner != id {
                        return Err(NarouError::InvalidTarget(format!(
                            "小説 {member} は既にバーチャル小説 vs_{owner} に属しています"
                        )));
                    }
                }
                members.groups.insert(id, replacement.to_vec());
                let member_ids = members.get(id).expect("validated virtual novel");
                let summary = summarize_members(runtime, member_ids).await?;
                // The live parent on the primary: the import planned against an
                // older snapshot, and a concurrent writer must not be erased.
                let parent = parent_snapshot(db, id).await?
                    .ok_or_else(|| NarouError::NotFound(format!("バーチャル小説 vs_{id} が見つかりません")))?;
                let mut existing: Vec<String> = serde_json::from_str(&parent.tags_json).map_err(db_error)?;
                existing.extend(metadata.tags.iter().filter(|tag| !core::is_internal_tag(tag)).cloned());
                let tags = summary.merged_tags(id, &existing);
                // The archived classification wins for a mixed group; a single
                // kind is always derived from the members.
                let requested = metadata
                    .attribute_tag
                    .as_deref()
                    .or(parent.attribute_tag.as_deref());
                let attribute = summary
                    .composition
                    .resolve(None, requested)
                    .map_err(NarouError::InvalidTarget)?;
                let folded: Vec<String> = tags
                    .iter()
                    .map(|tag| narou_rs::db::novel_codec::fold(tag))
                    .collect();
                let date = summary
                    .latest_update
                    .or_else(|| metadata.general_lastup.as_deref()
                        .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
                        .map(|date| date.with_timezone(&Utc)))
                    .map(|date| date.to_rfc3339())
                    .map(JsValue::from)
                    .unwrap_or(JsValue::NULL);
                let member_json = serde_json::to_string(member_ids).map_err(db_error)?;
                // One guarded statement writes the derived metadata and the
                // fields an overwrite may fill, and verifies the member set
                // inside the same transaction.
                statements.push(db.prepare(
                    r#"UPDATE novels SET tags_json=?1,tags_fold=?2,tags_sort=?3,general_all_no=?4,general_lastup=?5,attribute_tag=?6,
title=CASE WHEN trim(title)='' THEN ?7 ELSE title END,
title_fold=CASE WHEN trim(title)='' THEN ?16 ELSE title_fold END,
author=CASE WHEN trim(author)='' THEN ?8 ELSE author END,
author_fold=CASE WHEN trim(author)='' THEN ?17 ELSE author_fold END,
domain=COALESCE(domain,?9),domain_fold=CASE WHEN domain IS NULL THEN ?18 ELSE domain_fold END
WHERE id=?10 AND tags_json=?11 AND attribute_tag IS ?12
AND ncode GLOB 'vs_[0-9]*' AND substr(ncode,4) NOT GLOB '*[^0-9]*'
AND NOT EXISTS(SELECT 1 FROM json_each(?13) m LEFT JOIN novels n ON n.id=m.value WHERE n.id IS NULL)
AND NOT EXISTS(SELECT 1 FROM json_each(?13) m JOIN novels n ON n.id=m.value WHERE n.ncode GLOB 'vs_[0-9]*' AND substr(n.ncode,4) NOT GLOB '*[^0-9]*')
AND EXISTS(SELECT 1 FROM json_each(?13) m CROSS JOIN novels n ON n.id=m.value WHERE COALESCE(n.attribute_tag,'小説')='小説')=?14
AND EXISTS(SELECT 1 FROM json_each(?13) m CROSS JOIN novels n ON n.id=m.value WHERE n.attribute_tag='漫画')=?15"#
                ).bind(&[
                    serde_json::to_string(&tags).map_err(db_error)?.into(), folded.join("\n").into(), folded.join("\u{1f}").into(),
                    if summary.general_all_no > 0 { (summary.general_all_no as f64).into() } else { JsValue::NULL },
                    date, attribute.into(),
                    metadata.title.as_str().into(), metadata.author.as_str().into(),
                    metadata.domain.clone().map(JsValue::from).unwrap_or(JsValue::NULL),
                    (id as f64).into(), parent.tags_json.as_str().into(),
                    parent.attribute_tag.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),
                    member_json.into(),
                    (u8::from(summary.composition.has_novel) as f64).into(),
                    (u8::from(summary.composition.has_manga) as f64).into(),
                    narou_rs::db::novel_codec::fold(&metadata.title).into(),
                    narou_rs::db::novel_codec::fold(&metadata.author).into(),
                    metadata.domain.as_deref().map(narou_rs::db::novel_codec::fold).map(JsValue::from).unwrap_or(JsValue::NULL),
                ]).map_err(db_error)?);
                // The guarded update must have matched: the guard statement
                // aborts the batch when it did not.
                statements.push(db.prepare("INSERT INTO novel_id_sequence(id,next_id) SELECT 1,0 WHERE changes()=0"));
                crate::d1_repository::push_tag_statements(db, id, &tags, &mut statements)?;
                guarded_parent = Some((id, parent));
                guarded_composition = Some((member_ids, summary.composition));
                Some(std::borrow::Cow::Owned(tags))
            }
            MembershipMutation::Remove(id) => {
                if members.remove(id).is_none() {
                    return Err(NarouError::NotFound(format!("仮想小説 vs_{id} は登録されていません")));
                }
                statements.push(db.prepare(narou_rs::db::novel_query::DELETE_NOVEL_SQL)
                    .bind(&[(id as f64).into()]).map_err(db_error)?);
                None
            }
        };
        let mut tag_snapshot = None;
        if let Some(tags) = written_tags {
            let snapshot = crate::tag_sequence::read_snapshot(db).await?;
            let before = if snapshot.sequence.is_initialized() { None } else { Some(crate::tag_sequence::library_tags(db).await?) };
            let written: Vec<&str> = tags.iter().map(String::as_str).filter(|tag| !core::is_internal_tag(tag)).collect();
            let next = crate::tag_sequence::advance(&snapshot, before.as_ref(), &written);
            if next != snapshot.sequence {
                statements.push(crate::tag_sequence::upsert_statement(db, snapshot.raw_json.as_deref(), &next)?);
                statements.push(crate::tag_sequence::guard_statement(db, &next)?);
                tag_snapshot = Some(snapshot);
            }
        }
        statements.push(db.prepare(SAVE_SQL).bind(&[
            serde_json::to_string(&members).map_err(db_error)?.into(),
            serde_yaml::to_string(&members)?.into(),
            raw_json.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),
        ]).map_err(db_error)?);
        // Check whether this CAS wrote, not whether another writer produced the
        // same value. A duplicate append must fail after re-reading membership.
        statements.push(db.prepare("INSERT INTO novel_id_sequence(id,next_id) SELECT 1,0 WHERE changes()=0"));
        let outcome = match db.batch(statements).await {
            Ok(results) => crate::d1_repository::ensure_batch_success(&results),
            Err(error) => Err(db_error(error)),
        };
        match outcome {
            Ok(()) => return Ok(()),
            Err(error) => {
                if read_snapshot(db).await?.raw_json != raw_json {
                    continue;
                }
                if let Some(snapshot) = tag_snapshot
                    && crate::tag_sequence::read_snapshot(db).await?.raw_json != snapshot.raw_json
                {
                    continue;
                }
                if let Some((id, parent)) = guarded_parent
                    && parent_snapshot(db, id).await?.as_ref() != Some(&parent)
                {
                    continue;
                }
                if let Some((ids, previous)) = guarded_composition
                    && composition(db, ids).await? != previous
                {
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err(db_error("バーチャル小説のメンバー構成が同時に更新されています。再試行してください"))
}

/// リクエストハンドラが `WorkerRuntime` を組み立てずに読むための入口。
pub async fn load_from_env(env: &Env) -> Result<VirtualNovelMembers> {
    let db = DbHandle::primary(std::sync::Arc::new(env.d1("DB").map_err(db_error)?));
    read(&db).await
}

/// 転送経路がメンバー構成を読むための入口。
pub(crate) async fn load_membership(db: &DbHandle) -> Result<VirtualNovelMembers> {
    read(db).await
}

/// 新規のバーチャル小説を公開する (レコードと所属は 1 つのトランザクション)。
pub(crate) async fn publish_virtual_parent(
    runtime: &WorkerRuntime,
    record: &NovelRecord,
    members: &[i64],
) -> Result<()> {
    mutate_membership(runtime, MembershipMutation::Create { record, members }).await
}

/// 既存のバーチャル小説の所属と派生メタデータを置き換える。
/// live 行を読み直して、アーカイブが担う項目だけを反映する。
pub(crate) async fn replace_virtual_members(
    runtime: &WorkerRuntime,
    id: i64,
    metadata: &ArchiveNovel,
    members: &[i64],
) -> Result<()> {
    mutate_membership(runtime, MembershipMutation::Replace { id, metadata, members }).await
}

/// D1 の所属を [`VirtualNovelDirectory`] として使うアダプタ。
///
/// 外部公開 API と MCP はバーチャル小説の所属 (どの作品をまとめているか) を
/// 読む必要がある。Worker 側は D1 の `app_state` がその実体。
pub struct D1VirtualNovelDirectory {
    env: Env,
}

impl D1VirtualNovelDirectory {
    pub fn new(env: &Env) -> Self {
        Self { env: env.clone() }
    }
}

impl narou_rs::application::external_api::VirtualNovelDirectory for D1VirtualNovelDirectory {
    fn members(
        &self,
    ) -> narou_rs::platform::PlatformFuture<'_, Result<VirtualNovelMembers>> {
        Box::pin(async move { load_from_env(&self.env).await })
    }
}

async fn composition(db: &DbHandle, members: &[i64]) -> Result<AttributeComposition> {
    core::validate_members(members).map_err(NarouError::InvalidTarget)?;
    let repository = crate::d1_repository::D1NovelRepository::new(db.primary_view());
    let records = narou_rs::application::virtual_book::load_member_records(&repository, members).await?;
    let mut result = AttributeComposition::default();
    for record in records.values() {
        result.add(record.attribute_tag());
    }
    Ok(result)
}

/// Reconcile only virtual rows, keeping a mixed work's stored selection.
/// CROSS JOIN keeps members on the outside of the primary-key probes; otherwise
/// the attribute index can make SQLite scan every manga for each group.
pub(crate) async fn reconcile_attributes(db: &DbHandle) -> Result<()> {
    db.prepare(r#"
WITH groups AS (
  SELECT CAST(g.key AS INTEGER) AS id, g.value AS members
  FROM app_state a, json_each(a.value_json) g
  WHERE a.scope='inv' AND a.key='virtual_novel_members'
), classified AS MATERIALIZED (
  SELECT g.id, CASE
    WHEN NOT EXISTS(SELECT 1 FROM json_each(g.members) m CROSS JOIN novels n ON n.id=m.value WHERE COALESCE(n.attribute_tag,'小説')<>'漫画') THEN '漫画'
    WHEN NOT EXISTS(SELECT 1 FROM json_each(g.members) m CROSS JOIN novels n ON n.id=m.value WHERE n.attribute_tag='漫画') THEN '小説'
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

async fn create(runtime: &WorkerRuntime, body: VirtualRequest) -> Result<NovelRecord> {
    let title = body.title
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| NarouError::InvalidTarget("タイトルを指定してください".into()))?;
    let members = core::parse_member_ids(&body.members).map_err(NarouError::InvalidTarget)?;
    if members.is_empty() {
        return Err(NarouError::InvalidTarget(
            "メンバーを指定してください".into(),
        ));
    }
    let summary = summarize_members(runtime, &members).await?;
    if body.tags.iter().any(|tag| narou_rs::application::attribute_tags::is_attribute_tag(tag.trim())) {
        return Err(NarouError::InvalidTarget("属性タグは編集できません".into()));
    }

    let attribute = summary.composition.resolve(body.attribute_tag.as_deref(), None).map_err(NarouError::InvalidTarget)?;
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
        author: body.author,
        title,
        file_title,
        // The transfer identity is portable: it addresses the same group in
        // another library, unlike the library-local `vs_<ID>` ncode.
        toc_url: core::new_toc_url()?,
        sitename: body.sitename,
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
        domain: body.domain,
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
    if let Err(error) = mutate_membership(runtime, MembershipMutation::Create { record: &record, members: &members }).await {
        let _ = runtime.novels.release_id(reservation).await;
        return Err(error);
    }
    Ok(record)
}

pub(crate) async fn summarize_members(runtime: &WorkerRuntime, members: &[i64]) -> Result<core::MemberSummary> {
    core::validate_members(members).map_err(NarouError::InvalidTarget)?;
    // Mutation retries must not reuse a possibly stale read-replica snapshot.
    let repository = crate::d1_repository::D1NovelRepository::new(runtime.write_db().clone());
    let records = narou_rs::application::virtual_book::load_member_records(&repository, members).await?;
    let mut summary = core::MemberSummary::default();
    for member in members {
        let record = &records[member];
        let count = match record.general_all_no {
            Some(count) if count > 0 => count,
            _ => count_sections(runtime, record).await.unwrap_or(0),
        };
        summary.add(record, count, record.attribute_tag());
    }
    Ok(summary)
}

/// 既存の仮想小説へメンバーを追加する (native の `add_members` と同じ規則)。
async fn add_members(runtime: &WorkerRuntime, id: i64, new_members: Vec<i64>) -> Result<()> {
    mutate_membership(runtime, MembershipMutation::Append { id, members: &new_members }).await
}

async fn reorder(runtime: &WorkerRuntime, id: i64, ordered: Vec<i64>) -> Result<()> {
    mutate_membership(runtime, MembershipMutation::Reorder { id, ordered: &ordered }).await
}

async fn remove(runtime: &WorkerRuntime, id: i64) -> Result<()> {
    mutate_membership(runtime, MembershipMutation::Remove(id)).await
}

/// `/api/virtual_novels/{id}/download.epub` — メンバーを登録順に結合した EPUB。
pub async fn download_epub(req: Request, env: Env, id: i64) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    crate::build_epub_response(&env, id, true).await
}

pub(crate) async fn load_book(
    db: &DbHandle,
    record: &NovelRecord,
    repository: &dyn NovelRepository,
    persistence: &narou_rs::downloader::persistence::PersistenceService,
) -> Result<narou_rs::application::virtual_book::VirtualBook> {
    let members = read(db).await?;
    let ids = members.get(record.id).ok_or_else(|| {
        NarouError::NotFound(format!("バーチャル小説 {} がありません", record.id))
    })?;
    narou_rs::application::virtual_book::load(record, ids, repository, persistence).await
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
            // build_ui already reconciled attributes. Read the records from
            // primary too, so the response cannot undo that with replica lag.
            let repository = crate::d1_repository::D1NovelRepository::new(runtime.write_db().clone());
            let novels = narou_rs::application::virtual_book::load_listing(
                &repository, &members, runtime.site_settings(),
            ).await.map_err(|error| worker::Error::RustError(error.to_string()))?;
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
