use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use narou_rs::application::tag_colors::{TagColorStore, TagColors};
use narou_rs::application::events::FreezeStore;
use narou_rs::application::novel_actions::FreezeMutationStore;
use narou_rs::application::settings::SettingsStore;
use narou_rs::db::NovelRecord;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository, NovelSortKey,
    PlatformFuture, SearchField,
};
use narou_rs::setting_core::SettingScope;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, D1Result};

const MAX_EXTRA_FIELDS_BYTES: usize = 1_900_000;

#[derive(Debug, Clone)]
pub struct D1NovelRepository {
    db: Arc<D1Database>,
}

impl D1NovelRepository {
    pub fn new(db: Arc<D1Database>) -> Self {
        Self { db }
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
}

impl NovelRepository for D1NovelRepository {
    fn get<'a>(&'a self, id: NovelId) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(async move {
            self.one(
                &format!("{} WHERE n.id = ? LIMIT 1", select_sql()),
                vec![BindValue::Int(id.0)],
            )
            .await
        })
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(url);
        Box::pin(async move {
            self.one(
                &format!("{} WHERE n.toc_url_fold = ? LIMIT 1", select_sql()),
                vec![BindValue::Text(folded)],
            )
            .await
        })
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(title);
        Box::pin(async move {
            self.one(
                &format!("{} WHERE n.title_fold = ? LIMIT 1", select_sql()),
                vec![BindValue::Text(folded)],
            )
            .await
        })
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(ncode);
        Box::pin(async move {
            self.one(
                &format!(
                    "{} WHERE n.ncode_fold = ? OR substr(rtrim(n.toc_url_fold, '/'), -(length(?) + 1)) = '/' || ? ORDER BY n.id LIMIT 1",
                    select_sql()
                ),
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
        let built = build_where(filter);
        Box::pin(async move {
            let statement = self.prepare(
                &format!("SELECT COUNT(*) AS count FROM novels n {}", built.sql),
                built.binds,
            )?;
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
        let mut built = build_where(&query.filter);
        built.sql.push_str(" ORDER BY ");
        built.sql.push_str(sort_expression(query.sort.key));
        built.sql.push_str(if query.sort.reverse { " DESC NULLS FIRST" } else { " ASC NULLS LAST" });
        built.sql.push_str(if query.sort.reverse { ", n.id DESC" } else { ", n.id ASC" });
        built.sql.push_str(" LIMIT ? OFFSET ?");
        built.binds.push(BindValue::Int(query.limit as i64));
        built.binds.push(BindValue::Int(query.offset as i64));

        Box::pin(async move { self.rows(&format!("{} {}", select_sql(), built.sql), built.binds).await })
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<NovelId>>> {
        let mut built = build_where(filter);
        if let Some(after_id) = after_id {
            built.sql.push_str(" AND n.id > ?");
            built.binds.push(BindValue::Int(after_id.0));
        }
        built.sql.push_str(" ORDER BY n.id ASC LIMIT ?");
        built.binds.push(BindValue::Int(limit as i64));
        Box::pin(async move {
            let statement = self.prepare(
                &format!("SELECT n.id FROM novels n {}", built.sql),
                built.binds,
            )?;
            let result = statement.all().await.map_err(worker_error)?;
            let rows: Vec<IdRow> = result.results().map_err(worker_error)?;
            Ok(rows.into_iter().map(|row| NovelId(row.id)).collect())
        })
    }

    fn allocate_id(&self) -> PlatformFuture<'_, Result<NovelId>> {
        Box::pin(async move {
            let update = self.prepare(
                "UPDATE novel_id_sequence SET next_id = next_id + 1 WHERE id = 1",
                Vec::new(),
            )?;
            let select = self.prepare(
                "SELECT next_id - 1 AS id FROM novel_id_sequence WHERE id = 1",
                Vec::new(),
            )?;
            let results = self.db.batch(vec![update, select]).await.map_err(worker_error)?;
            ensure_batch_success(&results)?;
            let row: SequenceRow = results
                .get(1)
                .ok_or_else(|| NarouError::Platform("D1 sequence returned no result".to_string()))?
                .results()
                .map_err(worker_error)?
                .into_iter()
                .next()
                .ok_or_else(|| NarouError::Platform("D1 sequence returned no id".to_string()))?;
            Ok(NovelId(row.id))
        })
    }

    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if mutations.is_empty() {
                return Ok(());
            }
            let mut statements = Vec::new();
            for mutation in mutations {
                match mutation {
                    NovelMutation::Upsert(record) => {
                        let max_id = record.id.saturating_add(1);
                        statements.push(self.prepare(UPSERT_SQL, record_binds(&record))?);
                        statements.push(self.prepare(
                            "DELETE FROM novel_tags WHERE novel_id = ?",
                            vec![BindValue::Int(record.id)],
                        )?);
                        for (position, tag) in record.tags.iter().enumerate() {
                            statements.push(self.prepare(
                                "INSERT INTO novel_tags (novel_id, position, tag, tag_fold) VALUES (?, ?, ?, ?)",
                                vec![
                                    BindValue::Int(record.id),
                                    BindValue::Int(position as i64),
                                    BindValue::Text(tag.clone()),
                                    BindValue::Text(fold(tag)),
                                ],
                            )?);
                        }
                        statements.push(self.prepare(
                            "UPDATE novel_id_sequence SET next_id = CASE WHEN next_id < ? THEN ? ELSE next_id END WHERE id = 1",
                            vec![BindValue::Int(max_id), BindValue::Int(max_id)],
                        )?);
                    }
                    NovelMutation::Remove(id) => {
                        statements.push(self.prepare(
                            "DELETE FROM novels WHERE id = ?",
                            vec![BindValue::Int(id.0)],
                        )?);
                    }
                }
            }
            let results = self.db.batch(statements).await.map_err(worker_error)?;
            ensure_batch_success(&results)
        })
    }
}

#[derive(Debug, Clone)]
pub struct D1FreezeStore {
    db: Arc<D1Database>,
}

impl D1FreezeStore {
    pub fn new(db: Arc<D1Database>) -> Self {
        Self { db }
    }
}

impl FreezeStore for D1FreezeStore {
    fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, Result<HashSet<i64>>> {
        Box::pin(async move {
            let rows: Vec<IdRow> = self
                .db
                .prepare("SELECT novel_id AS id FROM frozen_novels ORDER BY novel_id")
                .all()
                .await
                .map_err(worker_error)?
                .results()
                .map_err(worker_error)?;
            Ok(rows.into_iter().map(|row| row.id).collect())
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
            }
            let results = self.db.batch(statements).await.map_err(worker_error)?;
            ensure_batch_success(&results)
        })
    }
}

#[derive(Debug, Clone)]
pub struct D1SettingsStore {
    db: Arc<D1Database>,
}

impl D1SettingsStore {
    pub fn new(db: Arc<D1Database>) -> Self {
        Self { db }
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
        Box::pin(async move {
            let rows: Vec<StateRow> = self
                .db
                .prepare("SELECT key, value_json FROM app_state WHERE scope = ? ORDER BY key")
                .bind(&[JsValue::from_str(Self::scope_name(scope))])
                .map_err(worker_error)?
                .all()
                .await
                .map_err(worker_error)?
                .results()
                .map_err(worker_error)?;
            let mut values = HashMap::new();
            for row in rows {
                let value: JsonValue = serde_json::from_str(&row.value_json)
                    .map_err(|error| NarouError::Platform(format!("invalid D1 setting JSON: {error}")))?;
                values.insert(
                    row.key,
                    serde_yaml::to_value(value).map_err(|error| {
                        NarouError::Platform(format!("invalid D1 setting value: {error}"))
                    })?,
                );
            }
            Ok(values)
        })
    }

    fn save<'a>(
        &'a self,
        scope: SettingScope,
        settings: &'a HashMap<String, YamlValue>,
    ) -> PlatformFuture<'a, Result<()>> {
        let entries: Vec<(String, String)> = match settings
            .iter()
            .map(|(key, value)| {
                let json = serde_json::to_string(value).map_err(|error| {
                    NarouError::Platform(format!("cannot serialize D1 setting {key}: {error}"))
                })?;
                Ok((key.clone(), json))
            })
            .collect::<Result<_>>()
        {
            Ok(entries) => entries,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        Box::pin(async move {
            let scope_name = Self::scope_name(scope);
            let mut statements = vec![bind_statement(
                self.db.prepare("DELETE FROM app_state WHERE scope = ?"),
                vec![BindValue::Text(scope_name.to_string())],
            )?];
            for (key, value) in entries {
                statements.push(bind_statement(
                    self.db
                        .prepare("INSERT INTO app_state (scope, key, value_json) VALUES (?, ?, ?)"),
                    vec![
                        BindValue::Text(scope_name.to_string()),
                        BindValue::Text(key),
                        BindValue::Text(value),
                    ],
                )?);
            }
            let results = self.db.batch(statements).await.map_err(worker_error)?;
            ensure_batch_success(&results)
        })
    }
}

#[derive(Debug, Clone)]
pub struct D1TagColorStore {
    db: Arc<D1Database>,
}

impl D1TagColorStore {
    pub fn new(db: Arc<D1Database>) -> Self {
        Self { db }
    }
}

impl TagColorStore for D1TagColorStore {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<TagColors>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare("SELECT value_json FROM app_state WHERE scope = 'tag_colors' AND key = 'colors'")
                .first::<StateValueRow>(None)
                .await
                .map_err(worker_error)?;
            let mut colors = TagColors::default();
            if let Some(row) = row {
                let map: HashMap<String, String> = serde_json::from_str(&row.value_json)
                    .map_err(|error| NarouError::Platform(format!("invalid D1 tag colors: {error}")))?;
                for (tag, color) in map {
                    colors.set(&tag, &color);
                }
            }
            Ok(colors)
        })
    }

    fn save<'a>(&'a self, colors: &'a TagColors) -> PlatformFuture<'a, Result<()>> {
        let value = match serde_json::to_string(&colors.clone().into_map()) {
            Ok(value) => value,
            Err(error) => {
                return Box::pin(async move {
                    Err(NarouError::Platform(format!("cannot serialize D1 tag colors: {error}")))
                });
            }
        };
        Box::pin(async move {
            let statement = bind_statement(
                self.db.prepare(
                    "INSERT INTO app_state (scope, key, value_json) VALUES ('tag_colors', 'colors', ?) ON CONFLICT(scope, key) DO UPDATE SET value_json = excluded.value_json",
                ),
                vec![BindValue::Text(value)],
            )?;
            ensure_batch_success(&self.db.batch(vec![statement]).await.map_err(worker_error)?)
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
    extra_fields_json: String,
}

impl NovelRow {
    fn into_record(self) -> Result<NovelRecord> {
        let tags = serde_json::from_str(&self.tags_json)
            .map_err(|error| NarouError::Platform(format!("invalid D1 tags JSON: {error}")))?;
        let extra_fields = parse_extra_fields(&self.extra_fields_json)?;
        Ok(NovelRecord {
            id: self.id,
            author: self.author,
            title: self.title,
            file_title: self.file_title,
            toc_url: self.toc_url,
            sitename: self.sitename,
            novel_type: u8::try_from(self.novel_type).unwrap_or_default(),
            end: self.end != 0,
            last_update: parse_time(self.last_update)?,
            new_arrivals_date: parse_optional_time(self.new_arrivals_date)?,
            use_subdirectory: self.use_subdirectory != 0,
            general_firstup: parse_optional_time(self.general_firstup)?,
            novelupdated_at: parse_optional_time(self.novelupdated_at)?,
            general_lastup: parse_optional_time(self.general_lastup)?,
            last_mail_date: parse_optional_time(self.last_mail_date)?,
            tags,
            ncode: self.ncode,
            domain: self.domain,
            general_all_no: self.general_all_no,
            length: self.length,
            suspend: self.suspend != 0,
            is_narou: self.is_narou != 0,
            last_check_date: parse_optional_time(self.last_check_date)?,
            convert_failure: self.convert_failure != 0,
            extra_fields,
        })
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
}
#[derive(Debug, Deserialize)]
struct StateRow {
    key: String,
    value_json: String,
}

#[derive(Debug, Clone)]
enum BindValue {
    Text(String),
    Int(i64),
}

fn bind_statement(statement: D1PreparedStatement, values: Vec<BindValue>) -> Result<D1PreparedStatement> {
    let values: Vec<JsValue> = values
        .into_iter()
        .map(|value| match value {
            BindValue::Text(value) => JsValue::from_str(&value),
            BindValue::Int(value) => JsValue::from_f64(value as f64),
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

fn ensure_batch_success(results: &[D1Result]) -> Result<()> {
    for result in results {
        if !result.success() {
            return Err(NarouError::Platform(
                result.error().unwrap_or_else(|| "D1 batch statement failed".to_string()),
            ));
        }
    }
    Ok(())
}

fn fold(value: &str) -> String {
    value.to_lowercase()
}

fn format_time(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|value| value.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

#[derive(Debug, Deserialize)]
struct StateValueRow {
    value_json: String,
}
fn parse_time(value: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| NarouError::Platform(format!("invalid D1 timestamp: {error}")))
}

fn parse_optional_time(value: Option<String>) -> Result<Option<DateTime<Utc>>> {
    value.map(parse_time).transpose()
}

fn parse_extra_fields(value: &str) -> Result<BTreeMap<String, YamlValue>> {
    if value.len() > MAX_EXTRA_FIELDS_BYTES {
        return Err(NarouError::Platform("D1 extra fields payload exceeds limit".to_string()));
    }
    let json: JsonValue = serde_json::from_str(value)
        .map_err(|error| NarouError::Platform(format!("invalid D1 extra fields JSON: {error}")))?;
    let yaml = serde_yaml::to_value(json)
        .map_err(|error| NarouError::Platform(format!("invalid D1 extra fields: {error}")))?;
    let YamlValue::Mapping(mapping) = yaml else {
        return Ok(BTreeMap::new());
    };
    Ok(mapping
        .into_iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value)))
        .collect())
}

fn record_binds(record: &NovelRecord) -> Vec<BindValue> {
    let tags_json = serde_json::to_string(&record.tags).unwrap_or_else(|_| "[]".to_string());
    let tags_fold = record.tags.iter().map(|tag| fold(tag)).collect::<Vec<_>>().join("\n");
    let tags_sort = record.tags.iter().map(|tag| fold(tag)).collect::<Vec<_>>().join("\u{1f}");
    let extra_fields_json = serde_json::to_string(&record.extra_fields).unwrap_or_else(|_| "{}".to_string());
    vec![
        BindValue::Int(record.id),
        BindValue::Text(record.author.clone()),
        BindValue::Text(fold(&record.author)),
        BindValue::Text(record.title.clone()),
        BindValue::Text(fold(&record.title)),
        BindValue::Text(record.file_title.clone()),
        BindValue::Text(record.toc_url.clone()),
        BindValue::Text(fold(&record.toc_url)),
        BindValue::Text(record.sitename.clone()),
        BindValue::Text(fold(&record.sitename)),
        BindValue::Int(i64::from(record.novel_type)),
        BindValue::Int(record.end as i64),
        BindValue::Text(format_time(Some(record.last_update)).unwrap_or_default()),
        optional_text(format_time(record.new_arrivals_date)),
        BindValue::Int(record.use_subdirectory as i64),
        optional_text(format_time(record.general_firstup)),
        optional_text(format_time(record.novelupdated_at)),
        optional_text(format_time(record.general_lastup)),
        optional_text(format_time(record.last_mail_date)),
        BindValue::Text(tags_json),
        BindValue::Text(tags_fold),
        BindValue::Text(tags_sort),
        optional_text(record.ncode.clone()),
        optional_text(record.ncode.as_deref().map(fold)),
        optional_text(record.domain.clone()),
        optional_text(record.domain.as_deref().map(fold)),
        optional_int(record.general_all_no),
        optional_int(record.length),
        BindValue::Int(record.suspend as i64),
        BindValue::Int(record.is_narou as i64),
        optional_text(format_time(record.last_check_date)),
        BindValue::Int(record.convert_failure as i64),
        BindValue::Text(extra_fields_json.clone()),
        BindValue::Int(extra_fields_json.len() as i64),
    ]
}

fn optional_text(value: Option<String>) -> BindValue {
    value.map(BindValue::Text).unwrap_or_else(|| BindValue::Text(String::new()))
}
fn optional_int(value: Option<i64>) -> BindValue {
    BindValue::Int(value.unwrap_or(-1))
}

const UPSERT_SQL: &str = "INSERT INTO novels (id, author, author_fold, title, title_fold, file_title, toc_url, toc_url_fold, sitename, sitename_fold, novel_type, end, last_update, new_arrivals_date, use_subdirectory, general_firstup, novelupdated_at, general_lastup, last_mail_date, tags_json, tags_fold, tags_sort, ncode, ncode_fold, domain, domain_fold, general_all_no, length, suspend, is_narou, last_check_date, convert_failure, extra_fields_json, extra_fields_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULLIF(?, ''), NULLIF(?, ''), NULLIF(?, ''), NULLIF(?, ''), NULLIF(?, -1), NULLIF(?, -1), ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET author=excluded.author, author_fold=excluded.author_fold, title=excluded.title, title_fold=excluded.title_fold, file_title=excluded.file_title, toc_url=excluded.toc_url, toc_url_fold=excluded.toc_url_fold, sitename=excluded.sitename, sitename_fold=excluded.sitename_fold, novel_type=excluded.novel_type, end=excluded.end, last_update=excluded.last_update, new_arrivals_date=excluded.new_arrivals_date, use_subdirectory=excluded.use_subdirectory, general_firstup=excluded.general_firstup, novelupdated_at=excluded.novelupdated_at, general_lastup=excluded.general_lastup, last_mail_date=excluded.last_mail_date, tags_json=excluded.tags_json, tags_fold=excluded.tags_fold, tags_sort=excluded.tags_sort, ncode=excluded.ncode, ncode_fold=excluded.ncode_fold, domain=excluded.domain, domain_fold=excluded.domain_fold, general_all_no=excluded.general_all_no, length=excluded.length, suspend=excluded.suspend, is_narou=excluded.is_narou, last_check_date=excluded.last_check_date, convert_failure=excluded.convert_failure, extra_fields_json=excluded.extra_fields_json, extra_fields_bytes=excluded.extra_fields_bytes";

fn select_sql() -> &'static str {
    "SELECT n.id, n.author, n.author_fold, n.title, n.file_title, n.toc_url, n.sitename, n.novel_type, n.end, n.last_update, n.new_arrivals_date, n.use_subdirectory, n.general_firstup, n.novelupdated_at, n.general_lastup, n.last_mail_date, n.tags_json, n.ncode, n.domain, n.general_all_no, n.length, n.suspend, n.is_narou, n.last_check_date, n.convert_failure, n.extra_fields_json FROM novels n"
}

struct WhereBuilder {
    sql: String,
    binds: Vec<BindValue>,
}

fn build_where(filter: &NovelFilter) -> WhereBuilder {
    let mut builder = WhereBuilder {
        sql: "WHERE 1=1".to_string(),
        binds: Vec::new(),
    };
    if let Some(ids) = &filter.ids
        && !ids.is_empty()
    {
        let groups = ids
            .chunks(80)
            .map(|chunk| {
                let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(",");
                format!("n.id IN ({placeholders})")
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        builder.sql.push_str(" AND (");
        builder.sql.push_str(&groups);
        builder.sql.push(')');
        builder.binds.extend(ids.iter().map(|id| BindValue::Int(id.0)));
    }
    if let Some(keyword) = &filter.keyword {
        let keyword = fold(keyword);
        builder.sql.push_str(" AND (instr(n.title_fold, ?) > 0 OR instr(n.author_fold, ?) > 0)");
        builder.binds.push(BindValue::Text(keyword.clone()));
        builder.binds.push(BindValue::Text(keyword));
    }
    if let Some(site) = &filter.site {
        builder.sql.push_str(" AND n.sitename_fold = ?");
        builder.binds.push(BindValue::Text(fold(site)));
    }
    if let Some(domain) = &filter.domain {
        builder.sql.push_str(" AND n.domain_fold = ?");
        builder.binds.push(BindValue::Text(fold(domain)));
    }
    if let Some(tag) = &filter.tag {
        builder.sql.push_str(" AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = ?)");
        builder.binds.push(BindValue::Text(tag.clone()));
    }
    if let Some(ncode) = &filter.ncode {
        builder.sql.push_str(" AND n.ncode_fold = ?");
        builder.binds.push(BindValue::Text(fold(ncode)));
    }
    if let Some(is_narou) = filter.is_narou {
        builder.sql.push_str(" AND n.is_narou = ?");
        builder.binds.push(BindValue::Int(is_narou as i64));
    }
    if let Some(suspend) = filter.suspend {
        builder.sql.push_str(" AND n.suspend = ?");
        builder.binds.push(BindValue::Int(suspend as i64));
    }
    if let Some(novel_type) = filter.novel_type {
        builder.sql.push_str(" AND n.novel_type = ?");
        builder.binds.push(BindValue::Int(i64::from(novel_type)));
    }
    if let Some(end) = filter.end {
        builder.sql.push_str(" AND n.end = ?");
        builder.binds.push(BindValue::Int(end as i64));
    }
    for term in &filter.terms {
        let expression = term_expression(term.field);
        if term.values.is_empty() {
            builder.sql.push_str(if term.negated { " AND 1=1" } else { " AND 0=1" });
            continue;
        }
        builder.sql.push_str(if term.negated { " AND NOT (" } else { " AND (" });
        for (index, value) in term.values.iter().enumerate() {
            if index > 0 {
                builder.sql.push_str(" OR ");
            }
            builder.sql.push_str(&expression);
            let bind_count = if matches!(term.field, SearchField::Any) { 5 } else { 1 };
            for _ in 0..bind_count {
                builder.binds.push(BindValue::Text(fold(value)));
            }
        }
        builder.sql.push(')');
    }
    builder
}

fn term_expression(field: SearchField) -> String {
    match field {
        SearchField::Title => "instr(n.title_fold, ?) > 0".to_string(),
        SearchField::Author => "instr(n.author_fold, ?) > 0".to_string(),
        SearchField::Site => "instr(n.sitename_fold, ?) > 0".to_string(),
        SearchField::Tag => "instr(n.tags_fold, ?) > 0".to_string(),
        SearchField::Status => format!("instr({STATUS_SEARCH_EXPRESSION}, ?) > 0"),
        SearchField::Any => format!(
            "(instr(n.title_fold, ?) > 0 OR instr(n.author_fold, ?) > 0 OR instr(n.sitename_fold, ?) > 0 OR instr(n.tags_fold, ?) > 0 OR instr({STATUS_SEARCH_EXPRESSION}, ?) > 0)"
        ),
    }
}
const STATUS_SEARCH_EXPRESSION: &str = "(CASE WHEN EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen') THEN '凍結' ELSE '' END || CASE WHEN (EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen')) AND (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) THEN ', ' ELSE '' END || CASE WHEN n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') THEN '完結' ELSE '' END || CASE WHEN (EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen') OR n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN ', ' ELSE '' END || CASE WHEN EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN '削除' ELSE '' END || CASE WHEN (EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen') OR n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404')) AND n.suspend <> 0 THEN ', ' ELSE '' END || CASE WHEN n.suspend <> 0 THEN '中断' ELSE '' END)";

const STATUS_SORT_EXPRESSION: &str = "(CASE WHEN n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') THEN '完結' ELSE '' END || CASE WHEN (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN ', ' ELSE '' END || CASE WHEN EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN '削除' ELSE '' END || CASE WHEN (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404')) AND n.suspend <> 0 THEN ', ' ELSE '' END || CASE WHEN n.suspend <> 0 THEN '中断' ELSE '' END)";

fn sort_expression(key: NovelSortKey) -> &'static str {
    match key {
        NovelSortKey::Id => "n.id",
        NovelSortKey::LastUpdate => "n.last_update",
        NovelSortKey::GeneralLastup => "n.general_lastup",
        NovelSortKey::LastCheckDate => "n.last_check_date",
        NovelSortKey::Title => "n.title_fold",
        NovelSortKey::Author => "n.author_fold",
        NovelSortKey::SiteName => "n.sitename_fold",
        NovelSortKey::NovelType => "n.novel_type",
        NovelSortKey::Tags => "n.tags_sort",
        NovelSortKey::GeneralAllNo => "n.general_all_no",
        NovelSortKey::Length => "n.length",
        NovelSortKey::Status => STATUS_SORT_EXPRESSION,
        NovelSortKey::TocUrl => "n.toc_url_fold",
        NovelSortKey::NewArrivalsDate => "n.new_arrivals_date",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use narou_rs::platform::{SearchTerm, SearchField};

    #[test]
    fn any_search_binds_every_sql_placeholder() {
        let filter = NovelFilter {
            terms: vec![SearchTerm::new(SearchField::Any, false, vec!["needle".to_string()])],
            ..NovelFilter::default()
        };
        let built = build_where(&filter);
        assert_eq!(built.sql.matches('?').count(), built.binds.len());
        assert_eq!(built.binds.len(), 5);
    }

    #[test]
    fn extra_fields_payload_is_bounded() {
        let error = parse_extra_fields(&"x".repeat(MAX_EXTRA_FIELDS_BYTES + 1)).unwrap_err();
        assert!(error.to_string().contains("exceeds limit"));
    }
}
