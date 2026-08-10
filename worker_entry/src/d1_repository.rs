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
                &format!("{} WHERE n.toc_url_fold = ? ORDER BY n.id LIMIT 1", select_sql()),
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
                &format!("{} WHERE n.title_fold = ? ORDER BY n.id LIMIT 1", select_sql()),
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
        built.sql.push(' ');
        built.sql.push_str(sort_direction(query.sort.key, query.sort.reverse));
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
                        statements.push(self.prepare(UPSERT_SQL, record_binds(&record)?)?);
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
                        let refresh_status = format!(
                            "UPDATE novels AS n SET status_sort = {STATUS_SORT_EXPRESSION} WHERE n.id = ?"
                        );
                        statements.push(self.prepare(
                            &refresh_status,
                            vec![BindValue::Int(record.id)],
                        )?);
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
                let refresh_status = format!(
                    "UPDATE novels AS n SET status_sort = {STATUS_SORT_EXPRESSION} WHERE n.id = ?"
                );
                statements.push(bind_statement(
                    self.db.prepare(&refresh_status),
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
                .prepare("SELECT key, value_yaml, value_json FROM app_state WHERE scope = ? ORDER BY key")
                .bind(&[JsValue::from_str(Self::scope_name(scope))])
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
                let yaml = serde_yaml::to_string(value).map_err(|error| {
                    NarouError::Platform(format!("cannot serialize D1 setting {key}: {error}"))
                })?;
                Ok((key.clone(), yaml))
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
                        .prepare("INSERT INTO app_state (scope, key, value_yaml) VALUES (?, ?, ?)"),
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
    extra_fields_yaml: String,
}

impl NovelRow {
    fn into_record(self) -> Result<NovelRecord> {
        let tags = serde_json::from_str(&self.tags_json)
            .map_err(|error| NarouError::Platform(format!("invalid D1 tags JSON: {error}")))?;
        let extra_fields = parse_extra_fields(&self.extra_fields_yaml)?;
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
    value_yaml: String,
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
    value.trim().to_lowercase()
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
    let YamlValue::Mapping(mapping) = yaml else {
        return Err(NarouError::Platform(
            "D1 extra fields must be a mapping".to_string(),
        ));
    };
    Ok(mapping
        .into_iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value)))
        .collect())
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
    let tags_json = serde_json::to_string(&record.tags)
        .map_err(|error| NarouError::Platform(format!("cannot serialize D1 tags: {error}")))?;
    let tags_fold = record.tags.iter().map(|tag| fold(tag)).collect::<Vec<_>>().join("\n");
    let tags_sort = record.tags.iter().map(|tag| fold(tag)).collect::<Vec<_>>().join("\u{1f}");
    let extra_fields_yaml = serde_yaml::to_string(&record.extra_fields)
        .map_err(|error| NarouError::Platform(format!("cannot serialize D1 extra fields: {error}")))?;
    if extra_fields_yaml.len() > MAX_EXTRA_FIELDS_BYTES {
        return Err(NarouError::Platform("D1 extra fields payload exceeds limit".to_string()));
    }
    Ok(vec![
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
        BindValue::Text(extra_fields_yaml.clone()),
        BindValue::Int(extra_fields_yaml.len() as i64),
    ])
}

fn optional_text(value: Option<String>) -> BindValue {
    value.map(BindValue::Text).unwrap_or_else(|| BindValue::Text(String::new()))
}
fn optional_int(value: Option<i64>) -> BindValue {
    BindValue::Int(value.unwrap_or(-1))
}

const UPSERT_SQL: &str = "INSERT INTO novels (id, author, author_fold, title, title_fold, file_title, toc_url, toc_url_fold, sitename, sitename_fold, novel_type, end, last_update, new_arrivals_date, use_subdirectory, general_firstup, novelupdated_at, general_lastup, last_mail_date, tags_json, tags_fold, tags_sort, ncode, ncode_fold, domain, domain_fold, general_all_no, length, suspend, is_narou, last_check_date, convert_failure, extra_fields_yaml, extra_fields_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULLIF(?, ''), NULLIF(?, ''), NULLIF(?, ''), NULLIF(?, ''), NULLIF(?, -1), NULLIF(?, -1), ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET author=excluded.author, author_fold=excluded.author_fold, title=excluded.title, title_fold=excluded.title_fold, file_title=excluded.file_title, toc_url=excluded.toc_url, toc_url_fold=excluded.toc_url_fold, sitename=excluded.sitename, sitename_fold=excluded.sitename_fold, novel_type=excluded.novel_type, end=excluded.end, last_update=excluded.last_update, new_arrivals_date=excluded.new_arrivals_date, use_subdirectory=excluded.use_subdirectory, general_firstup=excluded.general_firstup, novelupdated_at=excluded.novelupdated_at, general_lastup=excluded.general_lastup, last_mail_date=excluded.last_mail_date, tags_json=excluded.tags_json, tags_fold=excluded.tags_fold, tags_sort=excluded.tags_sort, ncode=excluded.ncode, ncode_fold=excluded.ncode_fold, domain=excluded.domain, domain_fold=excluded.domain_fold, general_all_no=excluded.general_all_no, length=excluded.length, suspend=excluded.suspend, is_narou=excluded.is_narou, last_check_date=excluded.last_check_date, convert_failure=excluded.convert_failure, extra_fields_yaml=excluded.extra_fields_yaml, extra_fields_bytes=excluded.extra_fields_bytes";

fn select_sql() -> &'static str {
    "SELECT n.id, n.author, n.author_fold, n.title, n.file_title, n.toc_url, n.sitename, n.novel_type, n.end, n.last_update, n.new_arrivals_date, n.use_subdirectory, n.general_firstup, n.novelupdated_at, n.general_lastup, n.last_mail_date, n.tags_json, n.ncode, n.domain, n.general_all_no, n.length, n.suspend, n.is_narou, n.last_check_date, n.convert_failure, n.extra_fields_yaml FROM novels n"
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
    if let Some(ids) = &filter.ids {
        if ids.is_empty() {
            // An explicitly empty id set is "match nothing", not "no filter".
            builder.sql.push_str(" AND 0=1");
        } else {
            let json = serde_json::to_string(&ids.iter().map(|id| id.0).collect::<Vec<_>>())
                .expect("serializing integer ids cannot fail");
            builder
                .sql
                .push_str(" AND n.id IN (SELECT CAST(value AS INTEGER) FROM json_each(?))");
            builder.binds.push(BindValue::Text(json));
        }
    }
    if let Some(keyword) = &filter.keyword {
        let keyword = fold(keyword);
        let json = serde_json::to_string(&[keyword]).expect("serializing a folded keyword cannot fail");
        builder.sql.push_str(
            " AND EXISTS (SELECT 1 FROM json_each(?) WHERE instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0)",
        );
        builder.binds.push(BindValue::Text(json));
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
        if term.values.is_empty() {
            builder.sql.push_str(if term.negated { " AND 1=1" } else { " AND 0=1" });
            continue;
        }
        let values = term.values.iter().map(|value| fold(value)).collect::<Vec<_>>();
        let json = serde_json::to_string(&values).expect("serializing folded term values cannot fail");
        builder.sql.push_str(if term.negated {
            " AND NOT EXISTS (SELECT 1 FROM json_each(?) WHERE "
        } else {
            " AND EXISTS (SELECT 1 FROM json_each(?) WHERE "
        });
        builder.sql.push_str(&term_expression(term.field));
        builder.sql.push(')');
        builder.binds.push(BindValue::Text(json));
    }
    builder
}

fn term_expression(field: SearchField) -> String {
    match field {
        SearchField::Title => "instr(n.title_fold, value) > 0".to_string(),
        SearchField::Author => "instr(n.author_fold, value) > 0".to_string(),
        SearchField::Site => "instr(n.sitename_fold, value) > 0".to_string(),
        SearchField::Tag => "instr(n.tags_fold, value) > 0".to_string(),
        SearchField::Status => format!("instr({STATUS_SEARCH_EXPRESSION}, value) > 0"),
        SearchField::Any => format!(
            "instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0 OR instr(n.sitename_fold, value) > 0 OR instr(n.tags_fold, value) > 0 OR instr({STATUS_SEARCH_EXPRESSION}, value) > 0"
        ),
    }
}
const STATUS_SEARCH_EXPRESSION: &str = "(CASE WHEN EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen') THEN '凍結' ELSE '' END || CASE WHEN (EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen')) AND (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) THEN ', ' ELSE '' END || CASE WHEN n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') THEN '完結' ELSE '' END || CASE WHEN (EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen') OR n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN ', ' ELSE '' END || CASE WHEN EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN '削除' ELSE '' END || CASE WHEN (EXISTS (SELECT 1 FROM frozen_novels f WHERE f.novel_id = n.id) OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag_fold = 'frozen') OR n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404')) AND n.suspend <> 0 THEN ', ' ELSE '' END || CASE WHEN n.suspend <> 0 THEN '中断' ELSE '' END)";

const STATUS_SORT_EXPRESSION: &str = "(CASE WHEN n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') THEN '完結' ELSE '' END || CASE WHEN (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN ', ' ELSE '' END || CASE WHEN EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN '削除' ELSE '' END || CASE WHEN (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404')) AND n.suspend <> 0 THEN ', ' ELSE '' END || CASE WHEN n.suspend <> 0 THEN '中断' ELSE '' END)";

/// SQL NULL ordering for a sort key, mirroring the native comparators:
/// `new_arrivals_date` uses `Option::cmp` (None < Some), every other optional
/// key uses `compare_optional` (Some < None).
fn sort_direction(key: NovelSortKey, reverse: bool) -> &'static str {
    match (key, reverse) {
        (NovelSortKey::NewArrivalsDate, false) => "ASC NULLS FIRST",
        (NovelSortKey::NewArrivalsDate, true) => "DESC NULLS LAST",
        (_, false) => "ASC NULLS LAST",
        (_, true) => "DESC NULLS FIRST",
    }
}

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
        NovelSortKey::Status => "n.status_sort",
        NovelSortKey::TocUrl => "n.toc_url_fold",
        NovelSortKey::NewArrivalsDate => "n.new_arrivals_date",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use narou_rs::platform::{SearchTerm, SearchField};

    fn ids_filter(ids: Option<Vec<i64>>) -> NovelFilter {
        NovelFilter {
            ids: ids.map(|ids| ids.into_iter().map(NovelId).collect()),
            ..NovelFilter::default()
        }
    }

    fn ids_bind(built: &WhereBuilder) -> Vec<i64> {
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
        assert_eq!(binds.len(), 34);
        assert_eq!(UPSERT_SQL.matches('?').count(), 34);
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
    fn sql_targets_yaml_columns() {
        assert!(select_sql().contains("n.extra_fields_yaml"));
        assert!(!select_sql().contains("extra_fields_json"));
        assert!(UPSERT_SQL.contains("extra_fields_yaml"));
        assert!(!UPSERT_SQL.contains("extra_fields_json"));
        assert!(UPSERT_SQL.contains("extra_fields_bytes"));
    }
}
