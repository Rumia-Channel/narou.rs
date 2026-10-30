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

/// D1 tolerates much larger statement payloads than the native store's
/// `novel_codec::MAX_EXTRA_FIELDS_BYTES`; keep the historical D1 cap.
const MAX_EXTRA_FIELDS_BYTES: usize = 1_900_000;
const ATTRIBUTE_MARKER: &str = "work_type_attributes_v2";

pub(crate) const KNOWN_TOC_URLS: &str = r#"
SELECT CAST(j.key AS INTEGER) AS position,
       EXISTS(SELECT 1 FROM novels WHERE toc_url_fold=j.value) AS known
FROM json_each(?1) AS j ORDER BY CAST(j.key AS INTEGER)
"#;

#[derive(Debug, Clone)]
pub struct D1NovelRepository {
    db: DbHandle,
    sites: Arc<Vec<narou_rs::downloader::site_setting::SiteSetting>>,
}

impl D1NovelRepository {
    pub fn new(db: DbHandle) -> Self {
        Self { db, sites: Arc::new(Vec::new()) }
    }

    pub fn with_sites(mut self, sites: Arc<Vec<narou_rs::downloader::site_setting::SiteSetting>>) -> Self {
        self.sites = sites;
        self
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

impl NovelRepository for D1NovelRepository {
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
            self.one(&select_by_title_sql(), vec![BindValue::Text(folded)]).await
        })
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(ncode);
        Box::pin(async move {
            self.one(
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
        Box::pin(async move {
            let statement = self.prepare(novel_query::RELEASE_ID_SQL,
                vec![BindValue::Int(reservation.id().0), BindValue::Int(reservation.generation())])?;
            let result = statement.run().await.map_err(worker_error)?;
            ensure_batch_success(&[result])
        })
    }

    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut mutations = mutations;
            for mutation in &mut mutations {
                if let NovelMutation::Upsert(record) = mutation {
                    narou_rs::application::attribute_tags::initialize_record(record, &self.sites);
                }
            }
            if mutations.is_empty() {
                return Ok(());
            }
            // 書き込む小説のタグ (小説 ID 順)。
            let mut written: Vec<(i64, Vec<String>)> = Vec::new();
            for mutation in &mutations {
                if let NovelMutation::Upsert(record) = mutation {
                    written.push((record.id, record.tags.clone()));
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
                let mut statements = Vec::new();
                for mutation in &mutations {
                    match mutation {
                        NovelMutation::Upsert(record) => {
                            // Same contract as the native driver's
                            // `upsert_record_conn`: registration paths can merge
                            // default tags with retained ones, so normalize before
                            // encoding — `novel_tags` is UNIQUE (novel_id, tag)
                            // and tags_json must agree with the indexed rows.
                            let mut normalized = record.clone();
                            novel_codec::dedup_tags(&mut normalized.tags);
                            let record = &normalized;
                            let max_id = record.id.saturating_add(1);
                            statements.push(self.prepare(NOVEL_UPSERT_SQL, record_binds(record)?)?);
                            statements.push(self.prepare(
                                novel_query::COMMIT_ID_RESERVATION_SQL,
                                vec![BindValue::Int(record.id)],
                            )?);
                            statements.push(self.prepare(
                                novel_query::DELETE_NOVEL_TAGS_SQL,
                                vec![BindValue::Int(record.id)],
                            )?);
                            for (position, tag) in record.tags.iter().enumerate() {
                                statements.push(self.prepare(
                                    novel_query::INSERT_NOVEL_TAG_SQL,
                                    vec![
                                        BindValue::Int(record.id),
                                        BindValue::Int(position as i64),
                                        BindValue::Text(tag.clone()),
                                        BindValue::Text(fold(tag)),
                                    ],
                                )?);
                            }
                            statements.push(self.prepare(
                                &novel_query::refresh_status_sort_sql(),
                                vec![BindValue::Int(record.id)],
                            )?);
                            statements.push(self.prepare(
                                novel_query::BUMP_SEQUENCE_SQL,
                                vec![BindValue::Int(max_id), BindValue::Int(max_id)],
                            )?);
                        }
                        NovelMutation::Remove(id) => {
                            statements.push(self.prepare(
                                novel_query::DELETE_NOVEL_SQL,
                                vec![BindValue::Int(id.0)],
                            )?);
                        }
                    }
                }
                // 連番表の更新を作品と同じ batch に入れる。batch は 1 トランザクション
                // なので、作品と順序が同時に確定する。CAS が外れたときは guard 文が
                // 一意制約違反を起こして batch 全体を中断し、作品もロールバックする。
                statements.push(crate::tag_sequence::upsert_statement(
                    &self.db,
                    snapshot.raw_json.as_deref(),
                    &next,
                )?);
                statements.push(crate::tag_sequence::guard_statement(&self.db, &next)?);
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
        })
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

/// isolate ごとの凍結 ID キャッシュ。一覧の描画や検索のたびに読んでいた
/// ため (1 回 = D1 往復 1 回)、書き込み側 (`set_frozen`) で即時無効化し、
/// 他の isolate は TTL で追従する。
static FROZEN_IDS_CACHE: std::sync::LazyLock<crate::isolate_cache::TtlMap<HashSet<i64>>> =
    std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(30_000.0));

impl FreezeStore for D1FreezeStore {
    fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, Result<HashSet<i64>>> {
        let fresh_reads = self.fresh_reads;
        Box::pin(async move {
            if !fresh_reads && let Some(cached) = FROZEN_IDS_CACHE.get("frozen") {
                return Ok(cached);
            }
            let rows: Vec<IdRow> = self
                .db
                .prepare("SELECT novel_id AS id FROM frozen_novels ORDER BY novel_id")
                .all()
                .await
                .map_err(worker_error)?
                .results()
                .map_err(worker_error)?;
            let ids: HashSet<i64> = rows.into_iter().map(|row| row.id).collect();
            FROZEN_IDS_CACHE.put("frozen", ids.clone());
            Ok(ids)
        })
    }
}

impl FreezeMutationStore for D1FreezeStore {
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
> = std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(30_000.0));
const GLOBAL_REPLACE_SCOPE: &str = "global_replace";
const GLOBAL_REPLACE_KEY: &str = "replace.txt";
const TAG_COLORS_CACHE_KEY: &str = "inv/tag_colors";

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
static TAG_COLORS_CACHE: std::sync::LazyLock<crate::isolate_cache::TtlMap<TagColors>> =
    std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(30_000.0));

impl TagColorStore for D1TagColorStore {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<TagColors>> {
        Box::pin(async move {
            if let Some(cached) = TAG_COLORS_CACHE.get(TAG_COLORS_CACHE_KEY) {
                return Ok(cached);
            }
            let colors = Self::current_rows(&self.db).await?.colors()?;
            // A primary update may have populated the cache while this
            // read-only replica request was in flight.
            if let Some(cached) = TAG_COLORS_CACHE.get(TAG_COLORS_CACHE_KEY) {
                return Ok(cached);
            }
            TAG_COLORS_CACHE.put(TAG_COLORS_CACHE_KEY, colors.clone());
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
                narou_rs::application::tag_colors::apply_tag_color_update(&mut colors, &update);
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
                    TAG_COLORS_CACHE.put(TAG_COLORS_CACHE_KEY, colors.clone());
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
fn parse_setting_value(value_yaml: &str, legacy_value_json: &str) -> Result<YamlValue> {
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
        assert_eq!(binds.len(), 36);
        assert_eq!(NOVEL_UPSERT_SQL.matches('?').count(), 36);
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

    #[test]
    fn sql_targets_yaml_columns() {
        assert!(select_sql().contains("n.extra_fields_yaml"));
        assert!(!select_sql().contains("extra_fields_json"));
        assert!(NOVEL_UPSERT_SQL.contains("extra_fields_yaml"));
        assert!(!NOVEL_UPSERT_SQL.contains("extra_fields_json"));
        assert!(NOVEL_UPSERT_SQL.contains("extra_fields_bytes"));
    }
}
