use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use narou_rs::application::tag_colors::{TagColorStore, TagColors};
use narou_rs::application::events::FreezeStore;
use narou_rs::application::novel_actions::FreezeMutationStore;
use narou_rs::application::settings::SettingsStore;
use narou_rs::db::novel_codec::{self, fold, NovelBind, NOVEL_UPSERT_SQL};
use narou_rs::db::novel_query::{
    self, select_by_id_sql, select_by_ncode_sql, select_by_title_sql, select_by_toc_url_sql,
    SqlQuery,
};
use narou_rs::db::NovelRecord;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository, PlatformFuture,
};
use narou_rs::setting_core::SettingScope;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use wasm_bindgen::JsValue;
use worker::{D1PreparedStatement, D1Result};

use crate::db_handle::DbHandle;

#[path="d1_creation.rs"]
mod creation;
pub(crate) use creation::ancestors;

/// D1 tolerates much larger statement payloads than the native store's
/// `novel_codec::MAX_EXTRA_FIELDS_BYTES`; keep the historical D1 cap.
const MAX_EXTRA_FIELDS_BYTES: usize = 1_900_000;
pub(crate) const ATTRIBUTE_MARKER: &str = "work_type_attributes_v2";

pub(crate) const KNOWN_TOC_URLS: &str = r#"
SELECT CAST(j.key AS INTEGER) AS position,
       EXISTS(SELECT 1 FROM novels WHERE toc_url_fold=j.value) AS known
FROM json_each(?1) AS j ORDER BY CAST(j.key AS INTEGER)
"#;

pub(crate) struct WebListInputs {
    pub(crate) hidden: Option<narou_rs::hidden::HiddenState>,
    pub(crate) members: narou_rs::virtual_novel::VirtualNovelMembers,
    pub(crate) raw_members: Option<String>,
    pub(crate) attribute_tags_enabled: bool,
}

#[derive(Clone)]
pub struct D1NovelRepository {
    db: DbHandle,
    sites: Arc<Vec<narou_rs::downloader::site_setting::SiteSetting>>,
    objects:Option<Arc<dyn narou_rs::platform::ObjectStore>>,
}
impl std::fmt::Debug for D1NovelRepository{fn fmt(&self,formatter:&mut std::fmt::Formatter<'_>)->std::fmt::Result{formatter.debug_struct("D1NovelRepository").field("objects_configured",&self.objects.is_some()).finish_non_exhaustive()}}

impl D1NovelRepository {
    pub fn new(db: DbHandle) -> Self {
        Self { db, sites: Arc::new(Vec::new()),objects:None }
    }

    pub fn with_sites(mut self, sites: Arc<Vec<narou_rs::downloader::site_setting::SiteSetting>>) -> Self {
        self.sites = sites;
        self
    }
    pub fn with_objects(mut self,objects:Arc<dyn narou_rs::platform::ObjectStore>)->Self{self.objects=Some(objects);self}
    fn primary(&self)->Self{Self{db:self.db.primary_view(),sites:self.sites.clone(),objects:self.objects.clone()}}

    /// id/prefix bind candidates for one durable origin row.
    ///
    /// The closed-gate rejection covers creation of a NEW binding and moving
    /// an existing binding into a namespace that is closed for recovery. A
    /// durable binding that already equals the record's derived prefix is a
    /// no-op refresh: under a snapshot/publication owner's closed gate the
    /// record itself cannot change, so refusing it deadlocked export
    /// admission against its own held gate.
    fn bind_origin_statement(db:&DbHandle,id:i64,prefix:&str,sitename:&str,file_title:&str,use_subdirectory:i64)->Result<D1PreparedStatement>{
        bind_statement(db.prepare("INSERT INTO novel_gate_bindings(novel_id,prefix) SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM novels WHERE id=?1 AND sitename=?3 AND file_title=?4 AND use_subdirectory=?5) AND (EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=?1 AND prefix=?2) OR NOT EXISTS(SELECT 1 FROM novel_access_gates WHERE closed=1 AND (prefix=?2 OR prefix=(SELECT prefix FROM novel_gate_bindings WHERE novel_id=?1)))) ON CONFLICT(novel_id) DO UPDATE SET prefix=excluded.prefix RETURNING novel_id"),vec![BindValue::Int(id),BindValue::Text(prefix.into()),BindValue::Text(sitename.into()),BindValue::Text(file_title.into()),BindValue::Int(use_subdirectory)])
    }

    pub async fn bind_record_origin(&self,record:&NovelRecord)->Result<()>{
        let primary=self.primary();let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;
        #[derive(Deserialize)]struct Bound{novel_id:i64}
        let results=primary.db.batch(vec![Self::bind_origin_statement(&primary.db,record.id,keys.prefix().as_ref(),&record.sitename,&record.file_title,i64::from(record.use_subdirectory))?,
            primary.prepare("INSERT OR IGNORE INTO novel_revisions(novel_id,record_revision) SELECT id,1 FROM novels WHERE id=?",vec![BindValue::Int(record.id)])?]).await.map_err(worker_error)?;ensure_batch_success(&results)?;
        if !results[0].results::<Bound>().map_err(worker_error)?.iter().any(|row|row.novel_id==record.id){return Err(NarouError::Platform("record_changed_or_novel_recovering".into()));}Ok(())
    }
    pub async fn bind_record_origins_page(&self,after:Option<NovelId>,limit:usize)->Result<Option<NovelId>>{
        #[derive(Deserialize)]struct Row{id:i64,sitename:String,file_title:String,use_subdirectory:i64}
        #[derive(Deserialize)]struct Bound{novel_id:i64}
        let db=self.db.primary_view();let cap=limit.clamp(1,32);
        let rows=bind_statement(db.prepare("SELECT id,sitename,file_title,use_subdirectory FROM novels WHERE id>? ORDER BY id LIMIT ?"),vec![BindValue::Int(after.map_or(-1,|id|id.0)),BindValue::Int((cap+1) as i64)])?.all().await.map_err(worker_error)?.results::<Row>().map_err(worker_error)?;
        let next=if rows.len()>cap{Some(NovelId(rows[cap-1].id))}else{None};let mut statements=Vec::with_capacity(cap*2);let mut expected=Vec::with_capacity(cap);
        for row in rows.into_iter().take(cap){let keys=narou_rs::platform::NovelObjectKeys::new(&row.sitename,&row.file_title,row.use_subdirectory!=0)?;expected.push(row.id);
            statements.push(Self::bind_origin_statement(&db,row.id,keys.prefix().as_ref(),&row.sitename,&row.file_title,row.use_subdirectory)?);
            statements.push(bind_statement(db.prepare("INSERT OR IGNORE INTO novel_revisions(novel_id,record_revision) SELECT id,1 FROM novels WHERE id=?"),vec![BindValue::Int(row.id)])?);
        }
        if !statements.is_empty(){let results=db.batch(statements).await.map_err(worker_error)?;ensure_batch_success(&results)?;
            for(index,id)in expected.into_iter().enumerate(){if !results[index*2].results::<Bound>().map_err(worker_error)?.iter().any(|row|row.novel_id==id){return Err(NarouError::Platform("record_changed_or_novel_recovering: origin bootstrap cursor retained".into()));}}
        }Ok(next)
    }
    /// Advance one durable primary-only page. Response loss only repeats an
    /// idempotent page; a skipped/closed record can never advance the cursor.
    pub async fn bootstrap_record_origins(&self)->Result<bool>{
        #[derive(Default,Serialize,Deserialize)]struct Cursor{after:Option<i64>,complete:bool}
        const KEY:&str="transfer_gate_binding_bootstrap_v1";
        let db=self.db.primary_view();let raw=bind_statement(db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key=?"),vec![BindValue::Text(KEY.into())])?.first::<String>(Some("value_json")).await.map_err(worker_error)?;
        let current:Cursor=raw.as_deref().map(serde_json::from_str).transpose().map_err(|error|NarouError::Platform(format!("invalid origin bootstrap cursor: {error}")))?.unwrap_or_default();
        if current.complete{return Ok(true);}let next=self.bind_record_origins_page(current.after.map(NovelId),32).await?;
        let value=serde_json::to_string(&Cursor{after:next.map(|id|id.0),complete:next.is_none()}).map_err(|error|NarouError::Platform(error.to_string()))?;
        let result=bind_statement(db.prepare("INSERT INTO app_state(scope,key,value_json,value_yaml) SELECT 'inv',?1,?2,?2 WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key=?1 AND value_json=?3) ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml WHERE app_state.value_json IS ?3 RETURNING value_json"),vec![BindValue::Text(KEY.into()),BindValue::Text(value.clone()),raw.map_or(BindValue::Null,BindValue::Text)])?.run().await.map_err(worker_error)?;
        ensure_batch_success(std::slice::from_ref(&result))?;
        Ok(next.is_none()&&result.results::<JsonValue>().map_err(worker_error)?.iter().any(|row|row.get("value_json").and_then(JsonValue::as_str)==Some(value.as_str())))
    }

    /// Whether the durable origin-bootstrap cursor has been marked complete.
    async fn origin_bootstrap_ready(&self)->Result<bool>{
        let primary=self.db.primary_view();
        let marker=bind_statement(primary.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key='transfer_gate_binding_bootstrap_v1'"),vec![])?.first::<String>(Some("value_json")).await.map_err(worker_error)?;
        Ok(marker.as_deref().map(serde_json::from_str::<JsonValue>).transpose().map_err(worker_error)?.is_some_and(|value|value.get("complete").and_then(JsonValue::as_bool)==Some(true)))
    }

    fn prepare(&self, sql: &str, values: Vec<BindValue>) -> Result<D1PreparedStatement> {
        bind_statement(self.db.prepare(sql), values)
    }

    async fn rows(&self, sql: &str, values: Vec<BindValue>) -> Result<Vec<NovelRecord>> {
        let statement = self.prepare(sql, values)?;
        let result = statement
            .all()
            .await
            .map_err(worker_error)?;
        rows_from_result(result)
    }

    /// Resolve only the list's filter inputs, together on the primary.
    pub(crate) async fn web_list_inputs(&self, include_hidden: bool) -> Result<WebListInputs> {
        use narou_rs::application::attribute_tags::ENABLED_SETTING;
        use narou_rs::setting_core::setting_bool_value;

        let primary = self.primary();
        let state = primary.prepare(
            "SELECT key, value_yaml, value_json FROM app_state
             WHERE (scope='inv' AND (key='virtual_novel_members' OR (?1=0 AND key='hidden')))
                OR (scope='local' AND key=?2)",
            vec![BindValue::Int(i64::from(include_hidden)), BindValue::Text(ENABLED_SETTING.into())],
        )?;
        let state = state.all().await.map_err(worker_error)?;
        ensure_batch_success(std::slice::from_ref(&state))?;
        let mut inputs = WebListInputs {
            hidden: None,
            members: Default::default(),
            raw_members: None,
            attribute_tags_enabled: false,
        };
        for row in state.results::<StateRow>().map_err(worker_error)? {
            match row.key.as_str() {
                "hidden" => inputs.hidden = serde_json::from_str(&row.value_json).ok(),
                "virtual_novel_members" => {
                    inputs.members = serde_json::from_str(&row.value_json).map_err(worker_error)?;
                    inputs.raw_members = Some(row.value_json);
                }
                ENABLED_SETTING => {
                    inputs.attribute_tags_enabled = parse_setting_value(&row.value_yaml, &row.value_json)
                        .ok().and_then(|value| setting_bool_value(&value)).unwrap_or(false);
                }
                _ => unreachable!("unexpected list setting"),
            }
        }
        Ok(inputs)
    }

    /// A buffered metadata listing has one commit point: rows, counts and
    /// closed gates are read in the same primary D1 transaction. Holding
    /// durable reader leases until HTTP EOF is only needed for deferred reads.
    pub(crate) async fn web_list_snapshot(
        &self, query: NovelQuery, raw_members: Option<String>,
    ) -> Result<Option<narou_rs::platform::repository::NovelMetadataPage>> {
        let primary = self.primary();
        let all_matches = query.offset == 0 && query.limit == usize::MAX;
        let unrestricted = query.filter.is_unrestricted();
        let SqlQuery { sql, binds } = novel_query::query_sql(&query, |bind| bind);
        let mut statements = vec![
            primary.prepare(&sql, binds)?,
            primary.db.prepare("SELECT prefix FROM novel_access_gates WHERE closed=1"),
            primary.db.prepare(FROZEN_IDS_SQL),
            primary.prepare(
                "SELECT (SELECT value_json FROM app_state
                         WHERE scope='inv' AND key='virtual_novel_members') IS ?1 AS unchanged",
                vec![raw_members.map_or(BindValue::Null, BindValue::Text)],
            )?,
        ];
        let count_statement = |filter: &NovelFilter| {
            let SqlQuery { sql, binds } = novel_query::count_sql(filter, |bind| bind);
            primary.prepare(&sql, binds)
        };
        if !all_matches {
            statements.push(count_statement(&query.filter)?);
        }
        if !unrestricted {
            statements.push(count_statement(&NovelFilter::all())?);
        }
        let results = primary.db.batch(statements).await.map_err(worker_error)?;
        ensure_batch_success(&results)?;
        let mut results = results.into_iter();
        let mut next = || results.next().ok_or_else(|| worker_error("incomplete list snapshot"));
        let rows = next()?;
        #[derive(Deserialize)]
        struct ClosedGate { prefix: String }
        let closed = next()?.results::<ClosedGate>().map_err(worker_error)?;
        let frozen_ids = next()?.results::<IdRow>().map_err(worker_error)?
            .into_iter().map(|row| row.id).collect();
        #[derive(Deserialize)]
        struct MembershipCheck { unchanged: i64 }
        let check = next()?.results::<MembershipCheck>().map_err(worker_error)?
            .into_iter().next().ok_or_else(|| worker_error("missing list membership check"))?;
        if check.unchanged == 0 {
            return Ok(None);
        }
        let records = rows_from_result(rows)?;
        if !closed.is_empty() {
            let closed: HashSet<_> = closed.into_iter().map(|gate| gate.prefix).collect();
            for record in &records {
                let keys = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
                if closed.contains(keys.prefix().as_ref()) {
                    return Err(narou_rs::platform::access_fence::gate_closed_error(
                        &narou_rs::platform::ObjectPrefix::from(keys.prefix().clone()),
                    ));
                }
            }
        }
        let mut count = || {
            let row = next()?.results::<CountRow>().map_err(worker_error)?
                .into_iter().next().ok_or_else(|| worker_error("missing list snapshot count"))?;
            u64::try_from(row.count).map_err(worker_error)
        };
        let filtered = if all_matches { records.len() as u64 } else { count()? };
        let total = if unrestricted { filtered } else { count()? };
        Ok(Some(narou_rs::platform::repository::NovelMetadataPage {
            records_total: total,
            records_filtered: filtered,
            records,
            frozen_ids,
        }))
    }

    async fn one(&self, sql: &str, values: Vec<BindValue>) -> Result<Option<NovelRecord>> {
        let statement = self.prepare(sql, values)?;
        let row = statement
            .first::<NovelRow>(None)
            .await
            .map_err(worker_error)?;
        row.map(NovelRow::into_record).transpose()
    }

    pub async fn attributes_initialized(&self) -> Result<bool> {
        Ok(self.prepare(
            "SELECT value_json FROM app_state WHERE scope='inv' AND key=?1",
            vec![BindValue::Text(ATTRIBUTE_MARKER.into())],
        )?.first::<JsonValue>(None).await.map_err(worker_error)?.is_some())
    }

    /// Backfill old rows without replacing concurrently edited novel metadata.
    /// The compare-and-swap guard, tag index and sequence share one D1 batch.
    pub async fn initialize_attributes(
        &self,
        sites: &[narou_rs::downloader::site_setting::SiteSetting],
    ) -> Result<()> {
        if self.attributes_initialized().await? {
            return Ok(());
        }
        let sql = format!(
            "{} WHERE n.id > ?1 ORDER BY n.id LIMIT 20",
            novel_codec::NOVEL_SELECT_SQL
        );
        let mut conflicts = 0;
        let mut after = i64::MIN;
        loop {
            let result = self.prepare(&sql, vec![BindValue::Int(after)])?.all().await.map_err(worker_error)?;
            let rows: Vec<NovelRow> = result.results().map_err(worker_error)?;
            if rows.is_empty() {
                let mark = self.prepare(
                    "INSERT OR IGNORE INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv',?1,'true','true')",
                    vec![BindValue::Text(ATTRIBUTE_MARKER.into())],
                )?;
                ensure_batch_success(&[mark.run().await.map_err(worker_error)?])?;
                return Ok(());
            }
            let last_id = rows.last().expect("nonempty page").id;
            let mut migrated = Vec::with_capacity(rows.len());
            for row in rows {
                let old_tags = row.tags_json.clone();
                let old_attribute = row.attribute_tag.clone();
                let mut record = row.into_record()?;
                if narou_rs::application::attribute_tags::initialize_record(&mut record, sites) {
                    novel_codec::dedup_tags(&mut record.tags);
                    migrated.push((record, old_tags, old_attribute));
                }
            }
            if migrated.is_empty() {
                after = last_id;
                continue;
            }
            let snapshot = crate::tag_sequence::read_snapshot(&self.db).await?;
            let before = if snapshot.sequence.is_initialized() {
                None
            } else {
                Some(crate::tag_sequence::library_tags(&self.db).await?)
            };
            let written: Vec<&str> = migrated.iter().flat_map(|(record, _, _)| record.tags.iter().map(String::as_str))
                .filter(|tag| !narou_rs::virtual_novel::is_internal_tag(tag)).collect();
            let next = crate::tag_sequence::advance(&snapshot, before.as_ref(), &written);
            let mut statements = Vec::new();
            for (record, old_tags, old_attribute) in &migrated {
                let json = serde_json::to_string(&record.tags).map_err(worker_error)?;
                let folded: Vec<String> = record.tags.iter().map(|tag| fold(tag)).collect();
                statements.push(self.prepare(
                    "UPDATE novels SET tags_json=?2,tags_fold=?3,tags_sort=?4,attribute_tag=?5 WHERE id=?1 AND tags_json=?6 AND attribute_tag IS ?7",
                    vec![BindValue::Int(record.id), BindValue::Text(json), BindValue::Text(folded.join("\n")), BindValue::Text(folded.join("\u{1f}")), BindValue::Text(record.attribute_tag().expect("initialized").to_string()), BindValue::Text(old_tags.clone()), old_attribute.as_ref().map(|tag| BindValue::Text(tag.clone())).unwrap_or(BindValue::Null)],
                )?);
                // A stale row aborts the whole batch; retry from the primary.
                statements.push(self.prepare(
                    "INSERT INTO novel_id_sequence(id,next_id) SELECT 1,0 WHERE changes()=0",
                    vec![],
                )?);
                statements.push(self.prepare(novel_query::DELETE_NOVEL_TAGS_SQL, vec![BindValue::Int(record.id)])?);
                for (position, tag) in record.tags.iter().enumerate() {
                    statements.push(self.prepare(novel_query::INSERT_NOVEL_TAG_SQL, vec![
                        BindValue::Int(record.id), BindValue::Int(position as i64),
                        BindValue::Text(tag.clone()), BindValue::Text(fold(tag)),
                    ])?);
                }
            }
            statements.push(crate::tag_sequence::upsert_statement(&self.db, snapshot.raw_json.as_deref(), &next)?);
            statements.push(crate::tag_sequence::guard_statement(&self.db, &next)?);
            match self.db.batch(statements).await {
                Ok(results) => {
                    ensure_batch_success(&results)?;
                    after = last_id;
                    conflicts = 0;
                }
                Err(error) => {
                    conflicts += 1;
                    if conflicts >= 8 { return Err(worker_error(error)); }
                }
            }
        }
    }
}

/// Prepare the row, tag index and ID bookkeeping for the caller's transaction.
/// Tags must already be normalized; the caller also commits the tag sequence.
pub(crate) fn push_upsert_statements(db: &DbHandle, record: &NovelRecord, statements: &mut Vec<D1PreparedStatement>) -> Result<()> {
    let prepare = |sql: &str, values| bind_statement(db.prepare(sql), values);
    statements.reserve(record.tags.len() + 5);
    let use_subdirectory=record.use_subdirectory;
    let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,use_subdirectory)?;
    statements.push(prepare("INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES (?,?) ON CONFLICT(novel_id) DO UPDATE SET prefix=excluded.prefix",vec![BindValue::Int(record.id),BindValue::Text(keys.prefix().as_ref().to_owned())])?);
    statements.push(prepare(NOVEL_UPSERT_SQL, record_binds(record)?)?);
    statements.push(prepare(novel_query::COMMIT_ID_RESERVATION_SQL, vec![BindValue::Int(record.id)])?);
    push_tag_statements(db, record.id, &record.tags, statements)?;
    statements.push(prepare(&novel_query::refresh_status_sort_sql(), vec![BindValue::Int(record.id)])?);
    let max_id = record.id.saturating_add(1);
    statements.push(prepare(novel_query::BUMP_SEQUENCE_SQL, vec![BindValue::Int(max_id), BindValue::Int(max_id)])?);
    Ok(())
}

pub(crate) fn push_tag_statements(db: &DbHandle, id: i64, tags: &[String], statements: &mut Vec<D1PreparedStatement>) -> Result<()> {
    statements.reserve(tags.len() + 1);
    statements.push(bind_statement(db.prepare(novel_query::DELETE_NOVEL_TAGS_SQL), vec![BindValue::Int(id)])?);
    for (position, tag) in tags.iter().enumerate() {
        statements.push(bind_statement(db.prepare(novel_query::INSERT_NOVEL_TAG_SQL), vec![
            BindValue::Int(id), BindValue::Int(position as i64),
            BindValue::Text(tag.clone()), BindValue::Text(fold(tag)),
        ])?);
    }
    Ok(())
}

impl NovelRepository for D1NovelRepository {
    fn allocate_id_for_job<'a>(&'a self,owner:&'a Arc<narou_rs::platform::JobExecutionOwner>)->PlatformFuture<'a,Result<narou_rs::platform::NovelIdReservation>>{Box::pin(self.allocate_job_creation(owner))}
    fn resume_id_for_job<'a>(&'a self,owner:&'a Arc<narou_rs::platform::JobExecutionOwner>,id:NovelId,generation:i64)->PlatformFuture<'a,Result<narou_rs::platform::NovelIdReservation>>{Box::pin(self.resume_job_creation(owner,id,generation))}
    fn reserve_create<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,reservation:&'a narou_rs::platform::NovelIdReservation,toc_url:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(self.reserve_creation(fence,reservation,toc_url))}
    fn apply_create_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,reservation:&'a narou_rs::platform::NovelIdReservation,record:NovelRecord)->PlatformFuture<'a,Result<()>>{Box::pin(self.commit_creation(fence,reservation,record))}
    fn remove_closed<'a>(&'a self,prefix:&'a narou_rs::platform::ObjectPrefix,owner:&'a str,epoch:u64,id:NovelId)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if narou_rs::platform::access_fence::parse_removal_owner(owner)?.0!=id{return Err(NarouError::Platform("removal owner ID mismatch".into()));}let db=self.db.primary_view();let storage=crate::d1_object_store::D1ObjectStore::new(db.clone());let primary=Self::new(db.clone());let record=primary.get_current(id).await?;let token=crate::content_store::nonce()?;let mut statements=storage.publication_guard(prefix,owner,epoch,&token)?;
        if let Some(record)=record{let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if keys.prefix().as_ref()!=prefix.as_ref().trim_end_matches('/'){return Err(NarouError::Platform("removal record identity changed".into()));}
            statements.push(bind_statement(db.prepare("SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM novels WHERE id=?1) OR EXISTS(SELECT 1 FROM novels WHERE id=?1 AND sitename=?2 AND file_title=?3 AND use_subdirectory=?4) THEN 1 ELSE abs(-9223372036854775808) END"),vec![BindValue::Int(id.0),BindValue::Text(record.sitename),BindValue::Text(record.file_title),BindValue::Int(i64::from(record.use_subdirectory))])?);
        }else{statements.push(bind_statement(db.prepare("SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM novels WHERE id=?) THEN 1 ELSE abs(-9223372036854775808) END"),vec![BindValue::Int(id.0)])?);}
        statements.push(bind_statement(db.prepare(novel_query::DELETE_NOVEL_SQL),vec![BindValue::Int(id.0)])?);statements.push(storage.finish_publication_guard(&token)?);let results=db.batch(statements).await.map_err(worker_error)?;ensure_batch_success(&results)
    })}
    fn get_current<'a>(&'a self,id:NovelId)->PlatformFuture<'a,Result<Option<NovelRecord>>>{Box::pin(async move{self.primary().one(&select_by_id_sql(),vec![BindValue::Int(id.0)]).await})}
    fn get_many_current<'a>(&'a self,ids:&'a [NovelId])->PlatformFuture<'a,Result<Vec<Option<NovelRecord>>>>{Box::pin(async move{if ids.is_empty(){return Ok(Vec::new());}let primary=self.primary();let sql=format!("{} WHERE n.id IN (SELECT CAST(value AS INTEGER) FROM json_each(?1))",novel_codec::NOVEL_SELECT_SQL);let encoded=serde_json::to_string(ids).map_err(worker_error)?;Ok(narou_rs::platform::repository::align_current_records(ids,primary.rows(&sql,vec![BindValue::Text(encoded)]).await?))})}
    fn query_current<'a>(&'a self,query:&'a NovelQuery)->PlatformFuture<'a,Result<Vec<NovelRecord>>>{let SqlQuery{sql,binds}=novel_query::query_sql(query,|bind|bind);Box::pin(async move{self.primary().rows(&sql,binds).await})}
    fn find_by_toc_url_current<'a>(&'a self,url:&'a str)->PlatformFuture<'a,Result<Option<NovelRecord>>>{Box::pin(async move{self.primary().one(&select_by_toc_url_sql(),vec![BindValue::Text(fold(url))]).await})}
    fn find_by_storage_prefix_current<'a>(&'a self,prefix:&'a narou_rs::platform::ObjectKey)->PlatformFuture<'a,Result<Option<NovelRecord>>>{Box::pin(async move{
        if !self.origin_bootstrap_ready().await?{
            // The downloader/acquisition path never goes through the transfer
            // routes that advance this cursor, so drive bounded pages here:
            // each invocation migrates up to 4*32 rows on the primary, then
            // reports pending only if completion is still in flight.
            for _ in 0..4{if self.bootstrap_record_origins().await?{break;}}
            if !self.origin_bootstrap_ready().await?{return Err(NarouError::Platform("novel origin bindings initialization pending".into()));}
        }
        self.primary().one(&format!("{} WHERE n.id IN (SELECT novel_id FROM novel_gate_bindings WHERE prefix=?1) ORDER BY n.id LIMIT 1",novel_codec::NOVEL_SELECT_SQL),vec![BindValue::Text(prefix.to_string())]).await
    })}
    fn get<'a>(&'a self, id: NovelId) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(async move {
            self.one(&select_by_id_sql(), vec![BindValue::Int(id.0)]).await
        })
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(url);
        Box::pin(async move {
            self.one(&select_by_toc_url_sql(), vec![BindValue::Text(folded)]).await
        })
    }

    fn contains_toc_urls<'a>(&'a self, urls: &'a [String]) -> PlatformFuture<'a, Result<Vec<bool>>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Membership { position: usize, known: i64 }
            let db = self.db.primary_view();
            let mut known = Vec::with_capacity(urls.len());
            for chunk in urls.chunks(100) {
                let folded: Vec<String> = chunk.iter().map(|url| fold(url)).collect();
                let encoded = serde_json::to_string(&folded).map_err(|error| NarouError::Platform(error.to_string()))?;
                let rows = db.prepare(KNOWN_TOC_URLS).bind(&[encoded.into()]).map_err(worker_error)?
                    .all().await.map_err(worker_error)?.results::<Membership>().map_err(worker_error)?;
                if rows.len() != chunk.len() || rows.iter().enumerate().any(|(index, row)| row.position != index) {
                    return Err(NarouError::Platform("invalid URL membership result".into()));
                }
                known.extend(rows.into_iter().map(|row| row.known != 0));
            }
            Ok(known)
        })
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(title);
        Box::pin(async move {
            self.primary().one(&select_by_title_sql(), vec![BindValue::Text(folded)]).await
        })
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(ncode);
        Box::pin(async move {
            self.primary().one(
                &select_by_ncode_sql(),
                vec![
                    BindValue::Text(folded.clone()),
                    BindValue::Text(folded.clone()),
                    BindValue::Text(folded),
                ],
            )
            .await
        })
    }

    fn count<'a>(&'a self, filter: &'a NovelFilter) -> PlatformFuture<'a, Result<u64>> {
        let SqlQuery { sql, binds } = novel_query::count_sql(filter, |bind| bind);
        Box::pin(async move {
            let statement = self.prepare(&sql, binds)?;
            let row = statement
                .first::<CountRow>(None)
                .await
                .map_err(worker_error)?
                .ok_or_else(|| NarouError::Platform("D1 count returned no row".to_string()))?;
            u64::try_from(row.count)
                .map_err(|_| NarouError::Platform("D1 count was negative".to_string()))
        })
    }

    fn query<'a>(&'a self, query: &'a NovelQuery) -> PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        let SqlQuery { sql, binds } = novel_query::query_sql(query, |bind| bind);
        Box::pin(async move { self.rows(&sql, binds).await })
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<NovelId>>> {
        let SqlQuery { sql, binds } = novel_query::scan_ids_sql(filter, after_id, limit, |bind| bind);
        Box::pin(async move {
            let statement = self.prepare(&sql, binds)?;
            let result = statement.all().await.map_err(worker_error)?;
            let rows: Vec<IdRow> = result.results().map_err(worker_error)?;
            Ok(rows.into_iter().map(|row| NovelId(row.id)).collect())
        })
    }

    fn allocate_id(&self) -> PlatformFuture<'_, Result<narou_rs::platform::NovelIdReservation>> {
        Box::pin(async move {
            let reserve = self.prepare(novel_query::ALLOCATE_STEP_SQL, Vec::new())?;
            let bump = self.prepare(novel_query::ALLOCATE_BUMP_SQL, Vec::new())?;
            let select = self.prepare(novel_query::ALLOCATE_READ_SQL, Vec::new())?;
            let results = self.db.batch(vec![reserve, bump, select]).await.map_err(worker_error)?;
            ensure_batch_success(&results)?;
            let row: SequenceRow = results
                .get(2)
                .ok_or_else(|| NarouError::Platform("D1 reservation returned no result".to_string()))?
                .results()
                .map_err(worker_error)?
                .into_iter()
                .next()
                .ok_or_else(|| NarouError::Platform("D1 reservation returned no id".to_string()))?;
            Ok(narou_rs::platform::NovelIdReservation::new(NovelId(row.id), row.generation))
        })
    }

    fn release_id(&self, reservation: narou_rs::platform::NovelIdReservation) -> PlatformFuture<'_, Result<()>> {
        Box::pin(self.release_creation(reservation))
    }

    fn apply_batch<'a>(&'a self,mutations:Vec<NovelMutation>)->PlatformFuture<'a,Result<()>>{Box::pin(self.apply_mutations(mutations,None))}
    fn apply_batch_guarded_many<'a>(&'a self,fences:&'a [narou_rs::platform::WriteFence],mutations:Vec<NovelMutation>)->PlatformFuture<'a,Result<()>>{Box::pin(self.apply_mutations(mutations,Some(fences)))}
}
impl D1NovelRepository{
    async fn apply_mutations(&self,mutations:Vec<NovelMutation>,fences:Option<&[narou_rs::platform::WriteFence]>)->Result<()>{
        self.primary().apply_mutations_primary(mutations,fences,None).await
    }
    async fn apply_mutations_primary(&self,mutations:Vec<NovelMutation>,fences:Option<&[narou_rs::platform::WriteFence]>,creation:Option<(&narou_rs::platform::WriteFence,&narou_rs::platform::NovelIdReservation)>)->Result<()>{
        if fences.is_some_and(|fences|fences.is_empty())&&!mutations.is_empty(){return Err(NarouError::Platform("record mutation has no writer fence".into()));}

            let mut mutations = mutations;
            for mutation in &mut mutations {
                if let NovelMutation::Upsert(record) = mutation {
                    narou_rs::application::attribute_tags::initialize_record(record, &self.sites);
                    // Keep the tag index, encoded row and registration sequence
                    // on the same normalized tags without cloning the record.
                    novel_codec::dedup_tags(&mut record.tags);
                }
            }
            if mutations.is_empty() {
                return Ok(());
            }
            // 書き込む小説のタグ (小説 ID 順)。
            let mut written: Vec<(i64, &[String])> = Vec::new();
            for mutation in &mutations {
                if let NovelMutation::Upsert(record) = mutation {
                    written.push((record.id, &record.tags));
                }
            }
            written.sort_by_key(|(id, _)| *id);
            let written: Vec<&str> = written
                .iter()
                .flat_map(|(_, tags)| tags.iter().map(String::as_str))
                .filter(|tag| !narou_rs::virtual_novel::is_internal_tag(tag))
                .collect();
            // タグの登録順 (どのタグが先にライブラリへ入ったか) を追跡する。
            // 連番表の更新は作品と同じ batch (1 トランザクション) に入れる。
            // CAS が外れたときは読み直して作り直す (8 回まで ✓)。
            for _ in 0..8 {
                let snapshot = crate::tag_sequence::read_snapshot(&self.db).await?;
                // 登録順が未初期化なら、既存ライブラリをバックフィルするために
                // 書き込み前のタグを先に読む。
                let before = if snapshot.sequence.is_initialized() {
                    None
                } else {
                    Some(crate::tag_sequence::library_tags(&self.db).await?)
                };
                let next = crate::tag_sequence::advance(&snapshot, before.as_ref(), &written);
                let mut statements=Vec::new();let storage=crate::d1_object_store::D1ObjectStore::new(self.db.clone());let mut gate_tokens=Vec::new();
                if let Some(fences)=fences{
                    for fence in fences{let token=crate::content_store::nonce()?;statements.push(storage.open_guard(fence,&token)?);gate_tokens.push(token);}
                    for mutation in &mutations{let id=match mutation{NovelMutation::Upsert(record)=>{let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("record outside captured fence".into()));}record.id},NovelMutation::Remove(id)=>id.0};
                        let mut sql=String::from("SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=?1) OR EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=?1 AND prefix IN (");
                        let mut binds=vec![BindValue::Int(id)];for(index,fence)in fences.iter().enumerate(){if index!=0{sql.push(',');}sql.push('?');sql.push_str(&(index+2).to_string());binds.push(BindValue::Text(fence.prefix.as_ref().trim_end_matches('/').into()));}sql.push_str(")) THEN 1 ELSE abs(-9223372036854775808) END");statements.push(self.prepare(&sql,binds)?);
                    }
                }
                if let Some((fence,reservation))=creation{statements.extend(self.creation_commit_guards(fence,reservation)?);}
                for mutation in &mutations {
                    match mutation {
                        NovelMutation::Upsert(record) => {
                            if creation.is_none(){statements.push(self.namespace_mutation_guard(record)?);}
                            push_upsert_statements(&self.db, record, &mut statements)?;
                        }
                        NovelMutation::Remove(id) => {
                            statements.push(self.prepare(
                                novel_query::DELETE_NOVEL_SQL,
                                vec![BindValue::Int(id.0)],
                            )?);
                        }
                    }
                }
                if let Some((fence,reservation))=creation{statements.extend(self.creation_commit_finish(fence,reservation)?);}
                // 連番表の更新を作品と同じ batch に入れる。batch は 1 トランザクション
                // なので、作品と順序が同時に確定する。CAS が外れたときは guard 文が
                // 一意制約違反を起こして batch 全体を中断し、作品もロールバックする。
                statements.push(crate::tag_sequence::upsert_statement(
                    &self.db,
                    snapshot.raw_json.as_deref(),
                    &next,
                )?);
                statements.push(crate::tag_sequence::guard_statement(&self.db, &next)?);
                if mutations.iter().any(|mutation|matches!(mutation,NovelMutation::Remove(_)))&&let Some(fences)=fences{for fence in fences{statements.push(self.prepare("UPDATE novel_access_gates SET epoch=CASE WHEN epoch<9223372036854775807 THEN epoch+1 ELSE abs(-9223372036854775808) END,updated_at=unixepoch() WHERE prefix=? AND epoch=? AND closed=0",vec![BindValue::Text(fence.prefix.as_ref().trim_end_matches('/').into()),BindValue::Int(fence.epoch as i64)])?);}}
                for token in &gate_tokens{statements.push(storage.finish_publication_guard(token)?);}
                let outcome = match self.db.batch(statements).await {
                    Ok(results) => {
                        ensure_batch_success(&results).map_err(|error| error.to_string())
                    }
                    Err(error) => Err(error.to_string()),
                };
                match outcome {
                    Ok(()) => return Ok(()),
                    Err(message) => {
                        // 連番表が他の書き込みで変わっていれば CAS 競合
                        // (読み直して作り直す ✓)。変わっていなければ本来の失敗 ✓。
                        let current = crate::tag_sequence::read_snapshot(&self.db).await?;
                        if current.raw_json == snapshot.raw_json {
                            return Err(NarouError::Platform(message));
                        }
                    }
                }
            }
            Err(NarouError::Platform(
                "タグの登録順が同時に更新されています。再試行してください".to_string(),
            ))
    }
}

#[derive(Debug, Clone)]
pub struct D1FreezeStore {
    db: DbHandle,
    /// `Self::for_writes` は `frozen_ids` がキャッシュを介さず読む
    /// (`D1SettingsStore::for_writes` と同じく、読み取りが書き込み判断の
    /// 材料になる経路で古い凍結集合を使わないため)。
    fresh_reads: bool,
}

impl D1FreezeStore {
    pub fn new(db: DbHandle) -> Self {
        Self {
            db,
            fresh_reads: false,
        }
    }

    /// 更新判断の読み取り用。`frozen_ids` が isolate キャッシュを介さず
    /// ハンドル先を読む。
    pub fn for_writes(db: DbHandle) -> Self {
        Self {
            db,
            fresh_reads: true,
        }
    }
}

const FROZEN_IDS_SQL: &str = "SELECT novel_id AS id FROM frozen_novels ORDER BY novel_id";

/// isolate ごとの凍結 ID キャッシュ。一覧の描画や検索のたびに読んでいた
/// ため (1 回 = D1 往復 1 回)、書き込み側 (`set_frozen`) で即時無効化し、
/// 他の isolate は TTL で追従する。
static FROZEN_IDS_CACHE: std::sync::LazyLock<crate::isolate_cache::TtlMap<Arc<HashSet<i64>>>> =
    std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(30_000.0, 256 * 1024, |ids| ids.capacity() * 24));

impl FreezeStore for D1FreezeStore {
    fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, Result<HashSet<i64>>> {
        let fresh_reads = self.fresh_reads;
        Box::pin(async move {
            if !fresh_reads && let Some(cached) = FROZEN_IDS_CACHE.get("frozen") {
                return Ok((*cached).clone());
            }
            let rows: Vec<IdRow> = self
                .db
                .prepare(FROZEN_IDS_SQL)
                .all()
                .await
                .map_err(worker_error)?
                .results()
                .map_err(worker_error)?;
            let ids: HashSet<i64> = rows.into_iter().map(|row| row.id).collect();
            FROZEN_IDS_CACHE.put("frozen", Arc::new(ids.clone()));
            Ok(ids)
        })
    }
}

impl FreezeMutationStore for D1FreezeStore {
    fn set_frozen_guarded_many<'a>(&'a self,fences:&'a [narou_rs::platform::WriteFence],ids:&'a [NovelId],frozen:bool)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if ids.is_empty(){return Ok(());}if fences.is_empty(){return Err(NarouError::Platform("freeze has no captured fence".into()));}
        let db=self.db.primary_view();let repository=D1NovelRepository::new(db.clone());let records=repository.get_many_current(ids).await?;
        let storage=crate::d1_object_store::D1ObjectStore::new(db.clone());let mut statements=Vec::new();let mut tokens=Vec::with_capacity(fences.len());
        for fence in fences{let token=crate::content_store::nonce()?;statements.push(storage.open_guard(fence,&token)?);tokens.push(token);}
        for(id,record)in ids.iter().zip(records){let record=record.ok_or_else(||NarouError::NotFound(format!("Novel {}",id.0)))?;let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;
            if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("freeze outside captured fences".into()));}
            statements.push(bind_statement(db.prepare("INSERT INTO novel_gate_bindings(novel_id,prefix) SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM novels WHERE id=?1 AND sitename=?3 AND file_title=?4 AND use_subdirectory=?5) AND NOT EXISTS(SELECT 1 FROM novel_access_gates WHERE closed=1 AND prefix=(SELECT prefix FROM novel_gate_bindings WHERE novel_id=?1)) ON CONFLICT(novel_id) DO UPDATE SET prefix=excluded.prefix"),vec![BindValue::Int(id.0),BindValue::Text(keys.prefix().to_string()),BindValue::Text(record.sitename),BindValue::Text(record.file_title),BindValue::Int(i64::from(record.use_subdirectory))])?);
            statements.push(bind_statement(db.prepare("SELECT CASE WHEN EXISTS(SELECT 1 FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id WHERE n.id=? AND b.prefix=?) THEN 1 ELSE abs(-9223372036854775808) END"),vec![BindValue::Int(id.0),BindValue::Text(keys.prefix().to_string())])?);
            statements.push(bind_statement(db.prepare(if frozen{"INSERT OR IGNORE INTO frozen_novels(novel_id) VALUES (?)"}else{"DELETE FROM frozen_novels WHERE novel_id=?"}),vec![BindValue::Int(id.0)])?);statements.push(bind_statement(db.prepare(novel_query::refresh_status_sort_sql()),vec![BindValue::Int(id.0)])?);
        }
        for token in tokens{statements.push(storage.finish_publication_guard(&token)?);}let results=db.batch(statements).await.map_err(worker_error)?;ensure_batch_success(&results)?;FROZEN_IDS_CACHE.invalidate("frozen");Ok(())
    })}

    fn set_frozen<'a>(
        &'a self,
        ids: &'a [NovelId],
        frozen: bool,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if ids.is_empty() {
                return Ok(());
            }
            let mut statements = Vec::with_capacity(ids.len());
            for id in ids {
                let sql = if frozen {
                    "INSERT OR IGNORE INTO frozen_novels (novel_id) VALUES (?)"
                } else {
                    "DELETE FROM frozen_novels WHERE novel_id = ?"
                };
                statements.push(bind_statement(
                    self.db.prepare(sql),
                    vec![BindValue::Int(id.0)],
                )?);
                statements.push(bind_statement(
                    self.db.prepare(novel_query::refresh_status_sort_sql()),
                    vec![BindValue::Int(id.0)],
                )?);
            }
            let results = self.db.batch(statements).await.map_err(worker_error)?;
            ensure_batch_success(&results)?;
            FROZEN_IDS_CACHE.invalidate("frozen");
            Ok(())
        })
    }
}

/// isolate ごとの設定キャッシュ (scope 名 → 値)。
///
/// Web UI のハンドラは `settings.get("webui.theme")` のようにキー単位で読む
/// ため、キャッシュが無いと 1 リクエストで同じ scope を何度も D1 へ読みに
/// 行っていた (1 回 = 往復 1 回)。書き込みは `save` で即時無効化し、他の
/// isolate は TTL で追従する。
static SETTINGS_CACHE: std::sync::LazyLock<
    crate::isolate_cache::TtlMap<std::sync::Arc<HashMap<String, YamlValue>>>,
> = std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(30_000.0, 512 * 1024, |values| {
    values.capacity() * (std::mem::size_of::<(String, YamlValue)>() + 16)
        + values.iter().map(|(key, value)| key.capacity() + yaml_heap_bytes(value)).sum::<usize>()
}));
const GLOBAL_REPLACE_SCOPE: &str = "global_replace";
const GLOBAL_REPLACE_KEY: &str = "replace.txt";
const TAG_COLORS_CACHE_KEY: &str = "inv/tag_colors";

fn yaml_heap_bytes(value: &YamlValue) -> usize {
    match value {
        YamlValue::String(value) => value.capacity(),
        YamlValue::Sequence(values) => values.capacity() * std::mem::size_of::<YamlValue>()
            + values.iter().map(yaml_heap_bytes).sum::<usize>(),
        YamlValue::Mapping(values) => values.len() * (std::mem::size_of::<(YamlValue, YamlValue)>() + 64)
            + values.iter().map(|(key, value)| yaml_heap_bytes(key) + yaml_heap_bytes(value)).sum::<usize>(),
        YamlValue::Tagged(value) => std::mem::size_of_val(value.as_ref())
            + value.tag.to_string().len() + yaml_heap_bytes(&value.value),
        _ => 0,
    }
}

#[derive(Debug, Clone)]
pub struct D1SettingsStore {
    db: DbHandle,
    /// 更新系ストア (`Self::for_writes`) は `load` で isolate キャッシュを
    /// 読まない。`save` の全文 DELETE+INSERT は読んだ map をそのまま書き戻す
    /// ため、読み取りが古いと同時期の他 isolate の変更を上書き消しする。
    /// 変更要件の読み取りは常にハンドル先 (mutation 組立では primary) から
    /// 取る。`save` 側のキャッシュ無効化は変わらない。
    fresh_reads: bool,
}

impl D1SettingsStore {
    /// UI 読み取り系のストア。`load` は isolate キャッシュ (30 秒 TTL) を使う。
    pub fn new(db: DbHandle) -> Self {
        Self {
            db,
            fresh_reads: false,
        }
    }

    /// 更新系のストア。`load` がキャッシュを介さずハンドル先を読むので、
    /// read-modify-write が読み取り時点の最新値を基にする。
    pub fn for_writes(db: DbHandle) -> Self {
        Self {
            db,
            fresh_reads: true,
        }
    }

    fn scope_name(scope: SettingScope) -> &'static str {
        match scope {
            SettingScope::Local => "local",
            SettingScope::Global => "global",
        }
    }
}

impl SettingsStore for D1SettingsStore {
    fn load<'a>(&'a self, scope: SettingScope) -> PlatformFuture<'a, Result<HashMap<String, YamlValue>>> {
        let fresh_reads = self.fresh_reads;
        Box::pin(async move {
            let scope_name = Self::scope_name(scope);
            if !fresh_reads && let Some(cached) = SETTINGS_CACHE.get(scope_name) {
                return Ok((*cached).clone());
            }
            let rows: Vec<StateRow> = self
                .db
                .prepare("SELECT key, value_yaml, value_json FROM app_state WHERE scope = ? ORDER BY key")
                .bind(&[JsValue::from_str(scope_name)])
                .map_err(worker_error)?
                .all()
                .await
                .map_err(worker_error)?
                .results()
                .map_err(worker_error)?;
            let mut values = HashMap::new();
            for row in rows {
                values.insert(row.key, parse_setting_value(&row.value_yaml, &row.value_json)?);
            }
            SETTINGS_CACHE.put(scope_name, std::sync::Arc::new(values.clone()));
            Ok(values)
        })
    }

    fn compare_save<'a>(
        &'a self,
        scope: SettingScope,
        expected: &'a HashMap<String, YamlValue>,
        replacement: &'a HashMap<String, YamlValue>,
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            let scope_name = Self::scope_name(scope);
            let rows: Vec<StateRow> = self.db
                .prepare("SELECT key, value_yaml, value_json FROM app_state WHERE scope = ? ORDER BY key")
                .bind(&[JsValue::from_str(scope_name)])
                .map_err(worker_error)?
                .all().await.map_err(worker_error)?
                .results().map_err(worker_error)?;
            if rows.len() != expected.len() {
                SETTINGS_CACHE.invalidate(scope_name);
                return Ok(false);
            }
            for row in &rows {
                if expected.get(&row.key) != Some(&parse_setting_value(&row.value_yaml, &row.value_json)?) {
                    SETTINGS_CACHE.invalidate(scope_name);
                    return Ok(false);
                }
            }
            let snapshot = serde_json::to_string(&rows)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let guard_key = format!("settings_write_guard/{scope_name}");
            let token = format!("{}:{}", js_sys::Date::now(), js_sys::Math::random());
            // D1 batch is one transaction. Its first statement captures a
            // scope-wide CAS; only that token can authorize the delta writes.
            let mut statements = vec![bind_statement(self.db.prepare(
                "INSERT INTO app_state(scope, key, value_yaml, value_json)
                 SELECT 'inv', ?, ?, '{}'
                 WHERE (SELECT json_group_array(json_object(
                   'key', key, 'value_yaml', value_yaml, 'value_json', value_json))
                   FROM (SELECT key, value_yaml, value_json FROM app_state WHERE scope = ? ORDER BY key)) = ?
                 ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml, value_json = '{}'
                 RETURNING key"
            ), vec![
                BindValue::Text(guard_key.clone()), BindValue::Text(token.clone()),
                BindValue::Text(scope_name.to_string()), BindValue::Text(snapshot),
            ])?];
            for key in expected.keys().filter(|key| !replacement.contains_key(*key)) {
                statements.push(bind_statement(self.db.prepare(
                    "DELETE FROM app_state WHERE scope = ? AND key = ?
                     AND EXISTS(SELECT 1 FROM app_state WHERE scope = 'inv' AND key = ? AND value_yaml = ?)"
                ), vec![
                    BindValue::Text(scope_name.to_string()), BindValue::Text(key.clone()),
                    BindValue::Text(guard_key.clone()), BindValue::Text(token.clone()),
                ])?);
            }
            for (key, value) in replacement {
                if expected.get(key) == Some(value) {
                    continue;
                }
                let yaml = serde_yaml::to_string(value).map_err(|error| NarouError::Platform(error.to_string()))?;
                let json = serde_json::to_string(value).map_err(|error| NarouError::Platform(error.to_string()))?;
                statements.push(bind_statement(self.db.prepare(
                    "INSERT INTO app_state(scope, key, value_yaml, value_json)
                     SELECT ?, ?, ?, ? WHERE EXISTS(
                       SELECT 1 FROM app_state WHERE scope = 'inv' AND key = ? AND value_yaml = ?)
                     ON CONFLICT(scope, key) DO UPDATE SET
                       value_yaml = excluded.value_yaml, value_json = excluded.value_json"
                ), vec![
                    BindValue::Text(scope_name.to_string()), BindValue::Text(key.clone()),
                    BindValue::Text(yaml), BindValue::Text(json),
                    BindValue::Text(guard_key.clone()), BindValue::Text(token.clone()),
                ])?);
            }
            statements.push(bind_statement(self.db.prepare(
                "DELETE FROM app_state WHERE scope = 'inv' AND key = ? AND value_yaml = ?"
            ), vec![BindValue::Text(guard_key), BindValue::Text(token)])?);
            let results = self.db.batch(statements).await.map_err(worker_error)?;
            ensure_batch_success(&results)?;
            SETTINGS_CACHE.invalidate(scope_name);
            Ok(!results[0].results::<JsonValue>().map_err(worker_error)?.is_empty())
        })
    }
    fn load_replace_content<'a>(&'a self) -> PlatformFuture<'a, Result<String>> {
        Box::pin(async move {
            let row: Option<StateYamlRow> = self
                .db
                .prepare("SELECT value_yaml FROM app_state WHERE scope = ? AND key = ?")
                .bind(&[
                    JsValue::from_str(GLOBAL_REPLACE_SCOPE),
                    JsValue::from_str(GLOBAL_REPLACE_KEY),
                ])
                .map_err(worker_error)?
                .first(None)
                .await
                .map_err(worker_error)?;
            Ok(row.map(|row| row.value_yaml).unwrap_or_default())
        })
    }

    fn compare_save_replace_content<'a>(
        &'a self,
        expected: &'a str,
        replacement: &'a str,
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            let result = bind_statement(self.db.prepare(
                "INSERT INTO app_state(scope, key, value_yaml, value_json)
                 SELECT ?, ?, ?, '{}' WHERE COALESCE(
                   (SELECT value_yaml FROM app_state WHERE scope = ? AND key = ?), '') = ?
                 ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml, value_json = '{}'
                 RETURNING key"
            ), vec![
                BindValue::Text(GLOBAL_REPLACE_SCOPE.to_string()),
                BindValue::Text(GLOBAL_REPLACE_KEY.to_string()),
                BindValue::Text(replacement.to_string()),
                BindValue::Text(GLOBAL_REPLACE_SCOPE.to_string()),
                BindValue::Text(GLOBAL_REPLACE_KEY.to_string()),
                BindValue::Text(expected.to_string()),
            ])?.all().await.map_err(worker_error)?;
            Ok(!result.results::<JsonValue>().map_err(worker_error)?.is_empty())
        })
    }
}

mod tag_colors;

use tag_colors::{TagColorRow, TagColorSnapshot};

#[derive(Debug, Clone)]
pub struct D1TagColorStore {
    db: DbHandle,
}

impl D1TagColorStore {
    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }

    async fn current_rows(db: &DbHandle) -> Result<TagColorSnapshot> {
        let rows: Vec<TagColorRow> = db
            .prepare(tag_colors::READ_SQL)
            .all()
            .await
            .map_err(worker_error)?
            .results()
            .map_err(worker_error)?;
        Ok(TagColorSnapshot::from_rows(rows))
    }
}

/// isolate ごとのタグ色キャッシュ (`TagColorStore::load` 参照)。
static TAG_COLORS_CACHE: std::sync::LazyLock<crate::isolate_cache::TtlMap<Arc<TagColors>>> =
    std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(30_000.0, 256 * 1024, |colors| colors.heap_bytes()));

impl TagColorStore for D1TagColorStore {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<TagColors>> {
        Box::pin(async move {
            if let Some(cached) = TAG_COLORS_CACHE.get(TAG_COLORS_CACHE_KEY) {
                return Ok((*cached).clone());
            }
            let colors = Self::current_rows(&self.db).await?.colors()?;
            // A primary update may have populated the cache while this
            // read-only replica request was in flight.
            if let Some(cached) = TAG_COLORS_CACHE.get(TAG_COLORS_CACHE_KEY) {
                return Ok((*cached).clone());
            }
            TAG_COLORS_CACHE.put(TAG_COLORS_CACHE_KEY, Arc::new(colors.clone()));
            Ok(colors)
        })
    }

    fn update<'a>(
        &'a self,
        update: narou_rs::application::tag_colors::TagColorUpdate,
    ) -> PlatformFuture<'a, Result<TagColors>> {
        Box::pin(async move {
            let primary = self.db.primary_view();
            for _ in 0..4 {
                let snapshot = Self::current_rows(&primary).await?;
                let mut colors = snapshot.colors()?;
                let changed = narou_rs::application::tag_colors::apply_tag_color_update(&mut colors, &update);
                if !snapshot.needs_write(changed) {
                    // This read may overlap a newer commit; do not replace or
                    // invalidate the cache populated by that writer.
                    return Ok(colors);
                }
                let value_yaml = colors.to_yaml_string().map_err(|error| {
                    NarouError::Platform(format!("cannot serialize D1 tag colors: {error}"))
                })?;
                let upsert = bind_statement(
                    primary.prepare(tag_colors::UPDATE_SQL),
                    snapshot.into_update_binds(value_yaml),
                )?;
                let remove_legacy = primary.prepare(tag_colors::REMOVE_LEGACY_SQL);
                let results = primary.batch(vec![upsert, remove_legacy]).await.map_err(worker_error)?;
                ensure_batch_success(&results)?;
                let cas_result = results.first().ok_or_else(|| {
                    NarouError::Platform("D1 tag color update batch returned no results".to_string())
                })?;
                if !cas_result.results::<JsonValue>().map_err(worker_error)?.is_empty() {
                    TAG_COLORS_CACHE.invalidate(TAG_COLORS_CACHE_KEY);
                    TAG_COLORS_CACHE.put(TAG_COLORS_CACHE_KEY, Arc::new(colors.clone()));
                    return Ok(colors);
                }
            }
            Err(NarouError::Platform(
                "D1 tag color update conflicted after four attempts".to_string(),
            ))
        })
    }
}

#[derive(Debug, Deserialize)]
struct NovelRow {
    id: i64,
    author: String,
    title: String,
    file_title: String,
    toc_url: String,
    sitename: String,
    novel_type: i64,
    end: i64,
    last_update: String,
    new_arrivals_date: Option<String>,
    use_subdirectory: i64,
    general_firstup: Option<String>,
    novelupdated_at: Option<String>,
    general_lastup: Option<String>,
    last_mail_date: Option<String>,
    tags_json: String,
    ncode: Option<String>,
    domain: Option<String>,
    general_all_no: Option<i64>,
    length: Option<i64>,
    suspend: i64,
    is_narou: i64,
    last_check_date: Option<String>,
    convert_failure: i64,
    extra_fields_yaml: String,
    #[serde(default)]
    requires_login: i64,
    #[serde(default)]
    login_session: Option<String>,
    #[serde(default)]
    attribute_tag: Option<String>,
}

impl NovelRow {
    /// Column values as the codec's named binds. Serde only types the cells;
    /// the column→field mapping itself is shared (`record_from_columns`).
    fn into_binds(self) -> Vec<(&'static str, NovelBind)> {
        fn nullable_text(value: Option<String>) -> NovelBind {
            value.map(NovelBind::Text).unwrap_or(NovelBind::Null)
        }
        fn nullable_int(value: Option<i64>) -> NovelBind {
            value.map(NovelBind::Int).unwrap_or(NovelBind::Null)
        }
        vec![
            ("id", NovelBind::Int(self.id)),
            ("author", NovelBind::Text(self.author)),
            ("title", NovelBind::Text(self.title)),
            ("file_title", NovelBind::Text(self.file_title)),
            ("toc_url", NovelBind::Text(self.toc_url)),
            ("sitename", NovelBind::Text(self.sitename)),
            ("novel_type", NovelBind::Int(self.novel_type)),
            ("end", NovelBind::Int(self.end)),
            ("last_update", NovelBind::Text(self.last_update)),
            ("new_arrivals_date", nullable_text(self.new_arrivals_date)),
            ("use_subdirectory", NovelBind::Int(self.use_subdirectory)),
            ("general_firstup", nullable_text(self.general_firstup)),
            ("novelupdated_at", nullable_text(self.novelupdated_at)),
            ("general_lastup", nullable_text(self.general_lastup)),
            ("last_mail_date", nullable_text(self.last_mail_date)),
            ("tags_json", NovelBind::Text(self.tags_json)),
            ("ncode", nullable_text(self.ncode)),
            ("domain", nullable_text(self.domain)),
            ("general_all_no", nullable_int(self.general_all_no)),
            ("length", nullable_int(self.length)),
            ("suspend", NovelBind::Int(self.suspend)),
            ("is_narou", NovelBind::Int(self.is_narou)),
            ("last_check_date", nullable_text(self.last_check_date)),
            ("convert_failure", NovelBind::Int(self.convert_failure)),
            ("extra_fields_yaml", NovelBind::Text(self.extra_fields_yaml)),
            ("requires_login", NovelBind::Int(self.requires_login)),
            ("login_session", nullable_text(self.login_session)),
            ("attribute_tag", nullable_text(self.attribute_tag)),
        ]
    }

    fn into_record(self) -> Result<NovelRecord> {
        novel_codec::record_from_columns(self.into_binds(), parse_extra_fields)
    }
}

#[derive(Debug, Deserialize)]
struct CountRow {
    count: i64,
}
#[derive(Debug, Deserialize)]
struct IdRow {
    id: i64,
}
#[derive(Debug, Deserialize)]
struct SequenceRow {
    id: i64,
    generation: i64,
}
#[derive(Debug, Deserialize, Serialize)]
struct StateRow {
    key: String,
    value_yaml: String,
    value_json: String,
}
/// D1 bind values come from the shared codec; `BindValue` stays as the local
/// alias so `build_where` and call sites read naturally.
type BindValue = NovelBind;

fn bind_statement(statement: D1PreparedStatement, values: Vec<BindValue>) -> Result<D1PreparedStatement> {
    let values: Vec<JsValue> = values
        .into_iter()
        .map(|value| match value {
            BindValue::Text(value) => JsValue::from_str(&value),
            BindValue::Int(value) => JsValue::from_f64(value as f64),
            BindValue::Null => JsValue::NULL,
        })
        .collect();
    statement.bind(&values).map_err(worker_error)
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker storage error: {error}"))
}

fn rows_from_result(result: D1Result) -> Result<Vec<NovelRecord>> {
    let rows: Vec<NovelRow> = result.results().map_err(worker_error)?;
    rows.into_iter().map(NovelRow::into_record).collect()
}

pub(crate) fn ensure_batch_success(results: &[D1Result]) -> Result<()> {
    for result in results {
        if !result.success() {
            return Err(NarouError::Platform(
                result.error().unwrap_or_else(|| "D1 batch statement failed".to_string()),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StateYamlRow {
    value_yaml: String,
}


fn parse_extra_fields(value: &str) -> Result<BTreeMap<String, YamlValue>> {
    if value.len() > MAX_EXTRA_FIELDS_BYTES {
        return Err(NarouError::Platform("D1 extra fields payload exceeds limit".to_string()));
    }
    let yaml = match serde_yaml::from_str::<YamlValue>(value) {
        Ok(yaml) => yaml,
        Err(yaml_error) => {
            // Rows written before Phase 8 stored JSON text; JSON is a subset
            // of YAML, but keep the legacy read for YAML 1.1 edge cases.
            let json: JsonValue = serde_json::from_str(value).map_err(|json_error| {
                NarouError::Platform(format!(
                    "invalid D1 extra fields YAML: {yaml_error}; legacy JSON: {json_error}"
                ))
            })?;
            serde_yaml::to_value(json)
                .map_err(|error| NarouError::Platform(format!("invalid D1 extra fields: {error}")))?
        }
    };
    novel_codec::extra_fields_mapping(yaml)
}

/// Parse one `app_state` setting value: YAML text first, with the legacy
/// `value_json` payload as fallback. Fails (no silent empty/default) when
/// neither format parses.
pub(crate) fn parse_setting_value(value_yaml: &str, legacy_value_json: &str) -> Result<YamlValue> {
    match serde_yaml::from_str::<YamlValue>(value_yaml) {
        Ok(value) => Ok(value),
        Err(yaml_error) => {
            let json: JsonValue = serde_json::from_str(legacy_value_json).map_err(|json_error| {
                NarouError::Platform(format!(
                    "invalid D1 setting YAML: {yaml_error}; legacy JSON: {json_error}"
                ))
            })?;
            serde_yaml::to_value(json)
                .map_err(|error| NarouError::Platform(format!("invalid D1 setting value: {error}")))
        }
    }
}


fn record_binds(record: &NovelRecord) -> Result<Vec<BindValue>> {
    let binds = novel_codec::novel_binds(record)?;
    novel_codec::check_extra_fields_limit(&binds, MAX_EXTRA_FIELDS_BYTES)?;
    // Positional order comes from the SQL itself, never from list order here —
    // a name that does not match the UPSERT columns fails instead of skewing.
    Ok(novel_codec::ordered_binds(NOVEL_UPSERT_SQL, &binds)?
        .into_iter()
        .map(|(_, value)| value.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use narou_rs::db::novel_query::{self, select_sql, sort_direction, WhereSql};
    use narou_rs::platform::{NovelSortKey, SearchField, SearchTerm};

    /// D1's bind type *is* the shared [`NovelBind`], so the tests below run the
    /// same builder the native adapter runs, with the identity conversion.
    fn build_where(filter: &NovelFilter) -> WhereSql<BindValue> {
        novel_query::build_where(filter, |bind| bind)
    }

    fn ids_filter(ids: Option<Vec<i64>>) -> NovelFilter {
        NovelFilter {
            ids: ids.map(|ids| ids.into_iter().map(NovelId).collect()),
            ..NovelFilter::default()
        }
    }

    fn ids_bind(built: &WhereSql<BindValue>) -> Vec<i64> {
        match &built.binds[0] {
            BindValue::Text(json) => {
                serde_json::from_str(json).expect("ids bind must be a JSON array")
            }
            _ => panic!("ids bind must be text"),
        }
    }

    fn sample_record(extra_fields: BTreeMap<String, YamlValue>) -> NovelRecord {
        NovelRecord {
            id: 7,
            author: "Author".to_string(),
            title: "Title".to_string(),
            file_title: "file_title".to_string(),
            toc_url: "https://example.com/ncode".to_string(),
            sitename: "小説家になろう".to_string(),
            novel_type: 2,
            end: true,
            last_update: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["tag1".to_string(), "tag2".to_string()],
            ncode: Some("n0000aa".to_string()),
            domain: None,
            general_all_no: Some(3),
            length: Some(1000),
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields,
        }
    }

    #[test]
    fn fold_trims_then_lowercases() {
        assert_eq!(fold("  Hello World  "), "hello world");
        assert_eq!(fold("\tABC\n"), "abc");
        assert_eq!(fold(""), "");
    }

    #[test]
    fn sort_direction_matches_native_null_ordering() {
        let expectations = [
            (NovelSortKey::Id, false, "ASC NULLS LAST"),
            (NovelSortKey::Id, true, "DESC NULLS FIRST"),
            (NovelSortKey::LastUpdate, false, "ASC NULLS LAST"),
            (NovelSortKey::LastUpdate, true, "DESC NULLS FIRST"),
            (NovelSortKey::GeneralLastup, false, "ASC NULLS LAST"),
            (NovelSortKey::GeneralLastup, true, "DESC NULLS FIRST"),
            (NovelSortKey::LastCheckDate, false, "ASC NULLS LAST"),
            (NovelSortKey::LastCheckDate, true, "DESC NULLS FIRST"),
            (NovelSortKey::Title, false, "ASC NULLS LAST"),
            (NovelSortKey::Title, true, "DESC NULLS FIRST"),
            (NovelSortKey::Author, false, "ASC NULLS LAST"),
            (NovelSortKey::Author, true, "DESC NULLS FIRST"),
            (NovelSortKey::SiteName, false, "ASC NULLS LAST"),
            (NovelSortKey::SiteName, true, "DESC NULLS FIRST"),
            (NovelSortKey::NovelType, false, "ASC NULLS LAST"),
            (NovelSortKey::NovelType, true, "DESC NULLS FIRST"),
            (NovelSortKey::Tags, false, "ASC NULLS LAST"),
            (NovelSortKey::Tags, true, "DESC NULLS FIRST"),
            (NovelSortKey::GeneralAllNo, false, "ASC NULLS LAST"),
            (NovelSortKey::GeneralAllNo, true, "DESC NULLS FIRST"),
            (NovelSortKey::Length, false, "ASC NULLS LAST"),
            (NovelSortKey::Length, true, "DESC NULLS FIRST"),
            (NovelSortKey::Status, false, "ASC NULLS LAST"),
            (NovelSortKey::Status, true, "DESC NULLS FIRST"),
            (NovelSortKey::TocUrl, false, "ASC NULLS LAST"),
            (NovelSortKey::TocUrl, true, "DESC NULLS FIRST"),
            (NovelSortKey::NewArrivalsDate, false, "ASC NULLS FIRST"),
            (NovelSortKey::NewArrivalsDate, true, "DESC NULLS LAST"),
        ];
        assert_eq!(expectations.len(), NovelSortKey::ALL.len() * 2);
        for (key, reverse, expected) in expectations {
            assert_eq!(sort_direction(key, reverse), expected, "{key:?} reverse={reverse}");
        }
        // Every key in both directions must produce a direction-consistent clause.
        for key in NovelSortKey::ALL {
            for reverse in [false, true] {
                let direction = sort_direction(*key, reverse);
                assert!(direction.starts_with(if reverse { "DESC" } else { "ASC" }));
                assert!(direction.contains("NULLS"));
            }
        }
    }

    #[test]
    fn any_search_binds_every_sql_placeholder() {
        let filter = NovelFilter {
            terms: vec![SearchTerm::new(SearchField::Any, false, vec!["needle".to_string()])],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert_eq!(built.sql.matches('?').count(), built.binds.len());
        assert_eq!(built.binds.len(), 1);
        assert!(built.sql.contains("SELECT 1 FROM json_each(?)"));
    }

    #[test]
    fn ids_none_adds_no_ids_clause() {
        let built = build_where(&ids_filter(None));
        assert!(!built.sql.contains("n.id IN"));
        assert!(!built.sql.contains("0=1"));
        assert!(built.binds.is_empty());
    }

    #[test]
    fn ids_some_empty_matches_nothing() {
        let built = build_where(&ids_filter(Some(vec![])));
        assert!(built.sql.contains("AND 0=1"));
        assert!(built.binds.is_empty());
        assert_eq!(built.sql.matches('?').count(), built.binds.len());
    }

    #[test]
    fn ids_use_a_single_json_each_bind() {
        for count in [1, 100, 101, 10000] {
            let ids: Vec<i64> = (1..=count).collect();
            let built = build_where(&ids_filter(Some(ids.clone())));
            assert_eq!(built.binds.len(), 1, "{count} ids must use one bind");
            assert!(built
                .sql
                .contains("n.id IN (SELECT CAST(value AS INTEGER) FROM json_each(?))"));
            assert_eq!(built.sql.matches('?').count(), built.binds.len());
            assert_eq!(ids_bind(&built), ids, "{count} ids must round-trip through JSON");
        }
    }

    #[test]
    fn keyword_uses_one_json_each_bind() {
        let filter = NovelFilter {
            keyword: Some("  Hello World  ".to_string()),
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert_eq!(built.binds.len(), 1);
        assert_eq!(built.sql.matches('?').count(), built.binds.len());
        assert!(built
            .sql
            .contains("EXISTS (SELECT 1 FROM json_each(?) WHERE instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0)"));
        let BindValue::Text(json) = &built.binds[0] else {
            panic!("keyword bind must be text");
        };
        let parsed: Vec<String> = serde_json::from_str(json).expect("keyword bind must be JSON");
        assert_eq!(parsed, vec!["hello world"]);
    }

    #[test]
    fn term_values_use_a_single_json_each_bind() {
        for count in [1, 100, 10000] {
            let values: Vec<String> = (0..count).map(|i| format!("Needle {i}")).collect();
            let filter = NovelFilter {
                terms: vec![SearchTerm::new(SearchField::Title, false, values)],
                ..NovelFilter::default()
            };
            let built = build_where(&filter);
            assert_eq!(built.binds.len(), 1, "{count} values must use one bind");
            assert_eq!(built.sql.matches('?').count(), built.binds.len());
            assert!(built.sql.contains("EXISTS (SELECT 1 FROM json_each(?) WHERE"));
            let BindValue::Text(json) = &built.binds[0] else {
                panic!("term bind must be text");
            };
            let parsed: Vec<String> = serde_json::from_str(json).expect("term bind must be JSON");
            assert_eq!(parsed.len(), count);
            assert_eq!(parsed[0], "needle 0");
            assert!(parsed.iter().all(|value| value.chars().all(|c| !c.is_uppercase())));
        }
    }

    #[test]
    fn any_field_uses_one_bind_regardless_of_value_count() {
        let filter = NovelFilter {
            terms: vec![SearchTerm::new(
                SearchField::Any,
                false,
                (0..10000).map(|i| format!("Value {i}")).collect(),
            )],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert_eq!(built.binds.len(), 1);
        assert_eq!(built.sql.matches('?').count(), built.binds.len());
        assert!(built.sql.contains("SELECT 1 FROM json_each(?) WHERE instr(n.title_fold, value)"));
        assert!(built.sql.contains("instr(n.tags_fold, value)"));
    }

    #[test]
    fn empty_term_preserves_negation_semantics() {
        let filter = NovelFilter {
            terms: vec![SearchTerm::new(SearchField::Title, false, vec![])],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert!(built.sql.contains("AND 0=1"));
        assert!(built.binds.is_empty());

        let filter = NovelFilter {
            terms: vec![SearchTerm::new(SearchField::Any, true, vec![])],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert!(built.sql.contains("AND 1=1"));
        assert!(built.binds.is_empty());
    }

    #[test]
    fn negated_terms_wrap_exists_in_not() {
        let filter = NovelFilter {
            terms: vec![SearchTerm::new(
                SearchField::Author,
                true,
                vec!["a".to_string(), "b".to_string()],
            )],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert!(built.sql.contains("AND NOT EXISTS (SELECT 1 FROM json_each(?) WHERE"));
        assert!(built.sql.contains("instr(n.author_fold, value) > 0"));
        assert_eq!(built.binds.len(), 1);
    }

    #[test]
    fn large_filter_stays_under_d1_bind_limit() {
        let filter = NovelFilter {
            ids: Some((0..10000).map(NovelId).collect()),
            keyword: Some("keyword".to_string()),
            site: Some("site".to_string()),
            domain: Some("domain".to_string()),
            tag: Some("tag".to_string()),
            ncode: Some("ncode".to_string()),
            is_narou: Some(true),
            suspend: Some(false),
            novel_type: Some(1),
            end: Some(true),
            terms: vec![
                SearchTerm::new(
                    SearchField::Any,
                    false,
                    (0..5000).map(|i| format!("a{i}")).collect(),
                ),
                SearchTerm::new(
                    SearchField::Title,
                    true,
                    (0..5000).map(|i| format!("b{i}")).collect(),
                ),
                SearchTerm::new(SearchField::Status, false, vec!["完結".to_string()]),
            ],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        // 8 scalar clauses + ids + keyword + 3 terms.
        assert_eq!(built.binds.len(), 13);
        assert_eq!(built.sql.matches('?').count(), built.binds.len());
        assert!(built.binds.len() <= 100);
    }

    #[test]
    fn extra_fields_payload_is_bounded() {
        let error = parse_extra_fields(&"x".repeat(MAX_EXTRA_FIELDS_BYTES + 1)).unwrap_err();
        assert!(error.to_string().contains("exceeds limit"));
    }

    #[test]
    fn extra_fields_reads_yaml_first() {
        let yaml = "---\nraw_title: 作品名\nanswer: 42\n";
        let fields = parse_extra_fields(yaml).unwrap();
        assert_eq!(fields.get("raw_title").and_then(YamlValue::as_str), Some("作品名"));
        assert_eq!(fields.get("answer").and_then(YamlValue::as_i64), Some(42));
    }

    #[test]
    fn extra_fields_falls_back_to_legacy_json() {
        let json = r#"{"raw_title":"作品名","nested":{"answer":42}}"#;
        let fields = parse_extra_fields(json).unwrap();
        assert_eq!(fields.get("raw_title").and_then(YamlValue::as_str), Some("作品名"));
        assert_eq!(fields["nested"]["answer"].as_i64(), Some(42));
    }

    #[test]
    fn extra_fields_rejects_non_mapping() {
        let error = parse_extra_fields("---\n- a\n- b\n").unwrap_err();
        assert!(error.to_string().contains("must be a mapping"));
        let error = parse_extra_fields("hello").unwrap_err();
        assert!(error.to_string().contains("must be a mapping"));
    }

    #[test]
    fn extra_fields_rejects_unparseable_payload() {
        let error = parse_extra_fields("{").unwrap_err();
        assert!(error.to_string().contains("legacy JSON"));
    }

    #[test]
    fn setting_value_reads_yaml_first() {
        let value = parse_setting_value("---\nupdate.interval: 2.0\n", "2.0").unwrap();
        assert_eq!(value["update.interval"].as_f64(), Some(2.0));
    }

    #[test]
    fn setting_value_falls_back_to_legacy_json() {
        let value = parse_setting_value("{", r#"{"a":1}"#).unwrap();
        assert_eq!(value["a"].as_i64(), Some(1));
    }

    #[test]
    fn setting_value_rejects_both_formats() {
        let error = parse_setting_value("{", "{").unwrap_err();
        assert!(error.to_string().contains("invalid D1 setting YAML"));
        assert!(error.to_string().contains("legacy JSON"));
    }

    #[test]
    fn record_binds_serialize_extra_fields_as_yaml() {
        let record = sample_record(BTreeMap::from([
            ("raw_title".to_string(), YamlValue::String("作品名".to_string())),
            ("answer".to_string(), YamlValue::Number(serde_yaml::Number::from(42_i64))),
        ]));
        let binds = record_binds(&record).unwrap();
        let BindValue::Text(yaml) = &binds[32] else {
            panic!("extra fields bind must be text");
        };
        let fields: BTreeMap<String, YamlValue> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fields.get("raw_title").and_then(YamlValue::as_str), Some("作品名"));
        assert_eq!(fields.get("answer").and_then(YamlValue::as_i64), Some(42));
        let BindValue::Int(bytes) = binds[33] else {
            panic!("extra fields size bind must be int");
        };
        assert_eq!(bytes as usize, yaml.len());
    }

    #[test]
    fn record_binds_reject_oversized_extra_fields() {
        let record = sample_record(BTreeMap::from([(
            "huge".to_string(),
            YamlValue::String("x".repeat(MAX_EXTRA_FIELDS_BYTES + 1)),
        )]));
        let error = record_binds(&record).unwrap_err();
        assert!(error.to_string().contains("exceeds limit"));
    }

    #[test]
    fn tag_colors_merge_legacy_rows_before_canonical_without_hash_order() {
        let canonical = tag_colors::parse_tag_colors_payload("shared: blue\ncurrent: green\n").unwrap();
        let legacy = tag_colors::parse_tag_colors_row(
            Some("{}"),
            Some(r#"{"shared":"red","legacy":"yellow"}"#),
        )
        .unwrap();
        let mut merged = legacy;
        merged.merge(&canonical);
        assert_eq!(merged.color_for("shared"), Some("blue"));
        assert_eq!(merged.color_for("current"), Some("green"));
        assert_eq!(merged.color_for("legacy"), Some("yellow"));

        let persisted = merged.to_yaml_string().unwrap();
        let reloaded = tag_colors::parse_tag_colors_payload(&persisted).unwrap();
        assert_eq!(reloaded.color_for("legacy"), Some("yellow"));
        assert_eq!(reloaded.color_for("shared"), Some("blue"));
    }

}
