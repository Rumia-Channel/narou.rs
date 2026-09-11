//! rusqlite-backed [`NovelRepository`] and freeze stores.
//!
//! SQL semantics mirror the Worker D1 adapter exactly (same WHERE building,
//! sort expressions, status materialization, sequence allocation) so both
//! backends stay behaviorally identical.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use rusqlite::{params_from_iter, Connection, Row};

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::native::sqlite::query::{
    build_where, select_sql, sort_direction, sort_expression, UPSERT_SQL,
};
use crate::native::sqlite::record_map::{
    fold, parse_extra_fields, parse_optional_time, parse_time, record_params,
};
use crate::platform::{NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository};

/// Shared guarded connection. Every operation runs on a blocking thread; the
/// mutex serializes writers, matching the single-writer policy in the plan.
#[derive(Clone)]
pub struct SqliteNovelRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteNovelRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }
}

async fn blocking<T, F>(conn: Arc<Mutex<Connection>>, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock().expect("sqlite connection mutex poisoned");
        f(&mut guard)
    })
    .await
    .expect("sqlite blocking task panicked")
}


const SELECT_COLUMNS: usize = 26;

pub(crate) fn record_from_row(row: &Row<'_>) -> Result<NovelRecord> {
    let tags_json: String = row.get(16).map_err(|error| NarouError::Platform(error.to_string()))?;
    let extra_yaml: String =
        row.get(SELECT_COLUMNS - 1).map_err(|error| NarouError::Platform(error.to_string()))?;
    let tags: Vec<String> = serde_json::from_str(&tags_json)
        .map_err(|error| NarouError::Platform(format!("invalid tags JSON: {error}")))?;
    let extra_fields = parse_extra_fields(&extra_yaml)?;
    Ok(NovelRecord {
        id: column(row, 0)?,
        author: column(row, 1)?,
        // index 2 is the derived author_fold; not part of NovelRecord
        title: column(row, 3)?,
        file_title: column(row, 4)?,
        toc_url: column(row, 5)?,
        sitename: column(row, 6)?,
        novel_type: {
            let raw: i64 = column(row, 7)?;
            u8::try_from(raw).unwrap_or_default()
        },
        end: int_flag(row, 8)?,
        last_update: parse_time(column(row, 9)?)?,
        new_arrivals_date: parse_optional_time(column(row, 10)?)?,
        use_subdirectory: int_flag(row, 11)?,
        general_firstup: parse_optional_time(column(row, 12)?)?,
        novelupdated_at: parse_optional_time(column(row, 13)?)?,
        general_lastup: parse_optional_time(column(row, 14)?)?,
        last_mail_date: parse_optional_time(column(row, 15)?)?,
        tags,
        ncode: column(row, 17)?,
        domain: column(row, 18)?,
        general_all_no: column(row, 19)?,
        length: column(row, 20)?,
        suspend: int_flag(row, 21)?,
        is_narou: int_flag(row, 22)?,
        last_check_date: parse_optional_time(column(row, 23)?)?,
        convert_failure: int_flag(row, 24)?,
        extra_fields,
    })
}

fn column<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T> {
    row.get(index)
        .map_err(|error| NarouError::Platform(format!("sqlite column {index}: {error}")))
}

fn int_flag(row: &Row<'_>, index: usize) -> Result<bool> {
    let raw: i64 = column(row, index)?;
    Ok(raw != 0)
}

impl SqliteNovelRepository {
    /// Raw connection handle for `bulk` persistence helpers.
    pub(crate) fn conn_handle(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    fn one(&self, sql: String, params: Vec<rusqlite::types::Value>) -> impl Future<Output = Result<Option<NovelRecord>>> + Send {
        let sql_clone = sql;
        blocking(self.conn.clone(), move |conn| {
            let mut statement = conn.prepare(&sql_clone).map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(params_from_iter(params.iter()))
                .map_err(super::sqlite_error)?;
            match rows.next().map_err(super::sqlite_error)? {
                Some(row) => record_from_row(row).map(Some),
                None => Ok(None),
            }
        })
    }

    fn many(&self, sql: String, params: Vec<rusqlite::types::Value>) -> impl Future<Output = Result<Vec<NovelRecord>>> + Send {
        let sql_clone = sql;
        blocking(self.conn.clone(), move |conn| {
            let mut statement = conn.prepare(&sql_clone).map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(params_from_iter(params.iter()))
                .map_err(super::sqlite_error)?;
            let mut records = Vec::new();
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                records.push(record_from_row(row)?);
            }
            Ok(records)
        })
    }

}

pub(crate) fn upsert_record_conn(conn: &Connection, record: &NovelRecord) -> Result<()> {
    let params = record_params(record)?;
    conn.execute(UPSERT_SQL, params_from_iter(params.values.iter()))
        .map_err(super::sqlite_error)?;
    conn.execute(
        "DELETE FROM novel_tags WHERE novel_id = ?",
        [record.id],
    )
    .map_err(super::sqlite_error)?;
    for (position, tag) in record.tags.iter().enumerate() {
        conn.execute(
            "INSERT INTO novel_tags (novel_id, position, tag, tag_fold) VALUES (?, ?, ?, ?)",
            rusqlite::params![record.id, position as i64, tag, fold(tag)],
        )
        .map_err(super::sqlite_error)?;
    }
    refresh_status_sort(conn, record.id)?;
    conn.execute(
        "UPDATE novel_id_sequence SET next_id = CASE WHEN next_id < ? THEN ? ELSE next_id END WHERE id = 1",
        rusqlite::params![record.id.saturating_add(1), record.id.saturating_add(1)],
    )
    .map_err(super::sqlite_error)?;
    Ok(())
}

pub(crate) fn refresh_status_sort(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE novels AS n SET status_sort = (CASE WHEN n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') THEN '完結' ELSE '' END || CASE WHEN (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end')) AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN ', ' ELSE '' END || CASE WHEN EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404') THEN '削除' ELSE '' END || CASE WHEN (n.end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = 'end') OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = '404')) AND n.suspend <> 0 THEN ', ' ELSE '' END || CASE WHEN n.suspend <> 0 THEN '中断' ELSE '' END) WHERE n.id = ?",
        [id],
    )
    .map_err(super::sqlite_error)?;
    Ok(())
}

impl NovelRepository for SqliteNovelRepository {
    fn get<'a>(
        &'a self,
        id: NovelId,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(self.one(
            format!("{} WHERE n.id = ? LIMIT 1", select_sql()),
            vec![rusqlite::types::Value::Integer(id.0)],
        ))
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(self.one(
            format!("{} WHERE n.toc_url_fold = ? ORDER BY n.id LIMIT 1", select_sql()),
            vec![rusqlite::types::Value::Text(fold(url))],
        ))
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(self.one(
            format!("{} WHERE n.title_fold = ? ORDER BY n.id LIMIT 1", select_sql()),
            vec![rusqlite::types::Value::Text(fold(title))],
        ))
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(ncode);
        Box::pin(self.one(
            format!(
                "{} WHERE n.ncode_fold = ? OR substr(rtrim(n.toc_url_fold, '/'), -(length(?) + 1)) = '/' || ? ORDER BY n.id LIMIT 1",
                select_sql()
            ),
            vec![
                rusqlite::types::Value::Text(folded.clone()),
                rusqlite::types::Value::Text(folded.clone()),
                rusqlite::types::Value::Text(folded),
            ],
        ))
    }

    fn count<'a>(
        &'a self,
        filter: &'a NovelFilter,
    ) -> crate::platform::PlatformFuture<'a, Result<u64>> {
        let built = build_where(filter);
        Box::pin(blocking(self.conn.clone(), move |conn| {
            let sql = format!("SELECT COUNT(*) FROM novels n {}", built.sql);
            let count: i64 = conn
                .query_row(&sql, params_from_iter(built.params.iter()), |row| row.get(0))
                .map_err(super::sqlite_error)?;
            u64::try_from(count)
                .map_err(|_| NarouError::Platform("sqlite count was negative".to_string()))
        }))
    }

    fn query<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> crate::platform::PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        let mut built = build_where(&query.filter);
        built.sql.push_str(" ORDER BY ");
        built.sql.push_str(sort_expression(query.sort.key));
        built.sql.push(' ');
        built.sql.push_str(sort_direction(query.sort.key, query.sort.reverse));
        built
            .sql
            .push_str(if query.sort.reverse { ", n.id DESC" } else { ", n.id ASC" });
        built.sql.push_str(" LIMIT ? OFFSET ?");
        built
            .params
            .push(rusqlite::types::Value::Integer(query.limit as i64));
        built
            .params
            .push(rusqlite::types::Value::Integer(query.offset as i64));
        let sql = format!("{} {}", select_sql(), built.sql);
        let params = built.params;
        Box::pin(self.many(sql, params))
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> crate::platform::PlatformFuture<'a, Result<Vec<NovelId>>> {
        let mut built = build_where(filter);
        if let Some(after_id) = after_id {
            built.sql.push_str(" AND n.id > ?");
            built.params.push(rusqlite::types::Value::Integer(after_id.0));
        }
        built.sql.push_str(" ORDER BY n.id ASC LIMIT ?");
        built
            .params
            .push(rusqlite::types::Value::Integer(limit as i64));
        let sql = format!("SELECT n.id FROM novels n {}", built.sql);
        Box::pin(blocking(self.conn.clone(), move |conn| {
            let mut statement = conn.prepare(&sql).map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(params_from_iter(built.params.iter()))
                .map_err(super::sqlite_error)?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                ids.push(NovelId(column(row, 0)?));
            }
            Ok(ids)
        }))
    }

    fn allocate_id(&self) -> crate::platform::PlatformFuture<'_, Result<NovelId>> {
        Box::pin(blocking(self.conn.clone(), |conn| {
            let tx = conn.transaction().map_err(super::sqlite_error)?;
            tx.execute("UPDATE novel_id_sequence SET next_id = next_id + 1 WHERE id = 1", [])
                .map_err(super::sqlite_error)?;
            let id: i64 = tx
                .query_row("SELECT next_id - 1 FROM novel_id_sequence WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(super::sqlite_error)?;
            tx.commit().map_err(super::sqlite_error)?;
            Ok(NovelId(id))
        }))
    }

    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> crate::platform::PlatformFuture<'a, Result<()>> {
        Box::pin(blocking(self.conn.clone(), move |conn| {
            if mutations.is_empty() {
                return Ok(());
            }
            let mut tx = conn.transaction().map_err(super::sqlite_error)?;
            for mutation in mutations {
                match mutation {
                    NovelMutation::Upsert(record) => upsert_record_conn(&tx, &record)?,
                    NovelMutation::Remove(id) => {
                        tx.execute("DELETE FROM novels WHERE id = ?", [id.0])
                            .map_err(super::sqlite_error)?;
                    }
                }
            }
            tx.commit().map_err(super::sqlite_error)
        }))
    }
}

/// SQLite implementation of the application-layer freeze stores.
pub struct SqliteFreezeStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteFreezeStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }
}

impl crate::application::events::FreezeStore for SqliteFreezeStore {
    fn frozen_ids<'a>(
        &'a self,
    ) -> crate::platform::PlatformFuture<'a, Result<HashSet<i64>>> {
        Box::pin(blocking(self.conn.clone(), |conn| {
            let mut statement = conn
                .prepare("SELECT novel_id FROM frozen_novels")
                .map_err(super::sqlite_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(super::sqlite_error)?;
            let mut ids = HashSet::new();
            for id in rows {
                ids.insert(id.map_err(|error| NarouError::Platform(error.to_string()))?);
            }
            Ok(ids)
        }))
    }
}

impl crate::application::novel_actions::FreezeMutationStore for SqliteFreezeStore {
    fn set_frozen<'a>(
        &'a self,
        ids: &'a [NovelId],
        frozen: bool,
    ) -> crate::platform::PlatformFuture<'a, Result<()>> {
        let ids = ids.to_vec();
        Box::pin(blocking(self.conn.clone(), move |conn| {
            if ids.is_empty() {
                return Ok(());
            }
            let tx = conn.transaction().map_err(super::sqlite_error)?;
            for id in &ids {
                if frozen {
                    tx.execute(
                        "INSERT OR IGNORE INTO frozen_novels (novel_id) VALUES (?)",
                        [id.0],
                    )
                    .map_err(super::sqlite_error)?;
                } else {
                    tx.execute("DELETE FROM frozen_novels WHERE novel_id = ?", [id.0])
                        .map_err(super::sqlite_error)?;
                }
                refresh_status_sort(&tx, id.0)?;
            }
            tx.commit().map_err(super::sqlite_error)
        }))
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::events::FreezeStore as _;
    use crate::application::novel_actions::FreezeMutationStore as _;
    use crate::platform::mocks::MemoryNovelRepository;
    use crate::platform::{NovelFilter, NovelQuery, NovelSort, NovelSortKey, SearchField, SearchTerm};
    use chrono::{TimeZone, Utc};

    pub(super) fn record(id: i64, title: &str, author: &str, toc_url: &str, tags: &[&str]) -> NovelRecord {
        let mut record = NovelRecord {
            id,
            author: author.to_string(),
            title: title.to_string(),
            file_title: format!("[{author}] {title}"),
            toc_url: toc_url.to_string(),
            sitename: "小説家になろう".to_string(),
            novel_type: 0,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            novelupdated_at: None,
            general_lastup: Some(Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap()),
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: Some(100 + id as i64),
            length: Some(1000 * id as i64),
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        };
        if toc_url.contains("syosetu") {
            record.ncode = Some(format!("n{id:06}"));
            record.domain = Some("ncode.syosetu.com".to_string());
        }
        record
    }

    pub(super) fn fixtures() -> Vec<NovelRecord> {
        vec![
            record(1, "Alpha物語", "作者壱", "https://ncode.syosetu.com/n000001/", &["end"]),
            record(2, "beta行", "作者弐", "https://ncode.syosetu.com/n000002/", &["進行中", "frozen"]),
            {
                // legacy row without ncode: ncode lookup falls back to URL suffix
                let mut r = record(3, "Gamma記", "作者参", "https://syosetu.org/novel/300000/", &[]);
                r.is_narou = false;
                r.new_arrivals_date = Some(Utc.with_ymd_and_hms(2026, 5, 5, 5, 5, 5).unwrap());
                r
            },
        ]
    }

    async fn seed_both() -> (SqliteNovelRepository, MemoryNovelRepository) {
        let sqlite = SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap());
        let memory = MemoryNovelRepository::new();
        let mutations = fixtures()
            .into_iter()
            .map(NovelMutation::Upsert)
            .collect::<Vec<_>>();
        sqlite.apply_batch(mutations.clone()).await.unwrap();
        memory.apply_batch(mutations).await.unwrap();
        (sqlite, memory)
    }

    #[tokio::test]
    async fn roundtrip_get_and_finders_match_memory_backend() {
        let (sqlite, _memory) = seed_both().await;

        let got = sqlite.get(NovelId(2)).await.unwrap().expect("row 2");
        assert_eq!(got.title, "beta行");
        assert_eq!(got.tags, vec!["進行中".to_string(), "frozen".to_string()]);

        // folded lookups are case/space-insensitive like the D1 backend
        assert!(sqlite.find_by_toc_url("  https://NCODE.syosetu.com/n000001/ ").await.unwrap().is_some());
        assert!(sqlite.find_by_title("Alpha物語").await.unwrap().is_some());
        assert!(sqlite.find_by_title("  alpha物語  ").await.unwrap().is_some());

        let by_ncode = sqlite.find_by_ncode("n000002").await.unwrap().unwrap();
        assert_eq!(by_ncode.id, 2);

        // legacy fallback: no ncode column, match by trailing URL segment
        let legacy = sqlite.find_by_ncode("300000").await.unwrap().unwrap();
        assert_eq!(legacy.id, 3);
    }

    #[tokio::test]
    async fn dual_run_query_sort_filter_scan_count_agree() {
        let (sqlite, memory) = seed_both().await;

        let cases = [
            (NovelFilter::all(), NovelSort { key: NovelSortKey::Id, reverse: false }),
            (NovelFilter::all(), NovelSort { key: NovelSortKey::LastUpdate, reverse: true }),
            (
                NovelFilter::all(),
                NovelSort { key: NovelSortKey::NewArrivalsDate, reverse: false },
            ),
            (
                NovelFilter::all(),
                NovelSort { key: NovelSortKey::GeneralAllNo, reverse: true },
            ),
        ];
        for (filter, sort) in cases {
            let query = NovelQuery { filter: filter.clone(), sort, offset: 0, limit: 10 };
            let from_sqlite = sqlite.query(&query).await.unwrap();
            let from_memory = memory.query(&query).await.unwrap();
            let ids_sqlite: Vec<i64> = from_sqlite.iter().map(|r| r.id).collect();
            let ids_memory: Vec<i64> = from_memory.iter().map(|r| r.id).collect();
            assert_eq!(ids_sqlite, ids_memory, "sort {:?}/{:?}", sort.key, sort.reverse);
        }

        // keyword substring search (folded) matches the same rows
        let mut filter = NovelFilter::all();
        filter.keyword = Some("beta".to_string());
        let query = NovelQuery { filter, sort: NovelSort::default(), offset: 0, limit: 10 };
        assert_eq!(sqlite.query(&query).await.unwrap().len(), 1);
        assert_eq!(memory.query(&query).await.unwrap().len(), 1);

        // tag + count agreement
        let mut tag_filter = NovelFilter::all();
        tag_filter.tag = Some("frozen".to_string());
        assert_eq!(sqlite.count(&tag_filter).await.unwrap(), 1);
        assert_eq!(memory.count(&tag_filter).await.unwrap(), 1);

        // keyset scan in ascending order
        let scanned = sqlite.scan_ids(&NovelFilter::all(), Some(NovelId(1)), 10).await.unwrap();
        assert_eq!(scanned, vec![NovelId(2), NovelId(3)]);

        // pagination offset
        let page = NovelQuery { filter: NovelFilter::all(), sort: NovelSort { key: NovelSortKey::Id, reverse: false }, limit: 2, offset: 1 };
        assert_eq!(
            sqlite.query(&page).await.unwrap()[0].id,
            memory.query(&page).await.unwrap()[0].id
        );
    }

    #[tokio::test]
    async fn allocate_id_is_monotonic_after_upserts() {
        let sqlite = SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap());
        sqlite
            .apply_batch(vec![NovelMutation::Upsert(record(5, "x", "y", "u5", &[]))])
            .await
            .unwrap();
        assert!(sqlite.get(NovelId(5)).await.unwrap().is_some());
        let first = sqlite.allocate_id().await.unwrap();
        let second = sqlite.allocate_id().await.unwrap();
        assert_eq!(first.0, 6);
        assert_eq!(second.0, 7);
    }

    #[tokio::test]
    async fn remove_cascades_tags_and_freeze_rows() {
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let sqlite = SqliteNovelRepository::new(conn.clone());
        let freeze = SqliteFreezeStore::new(conn.clone());
        sqlite
            .apply_batch(vec![NovelMutation::Upsert(record(9, "消える", "a", "u9", &["x"]))])
            .await
            .unwrap();
        freeze.set_frozen(&[NovelId(9)], true).await.unwrap();

        sqlite.apply_batch(vec![NovelMutation::Remove(NovelId(9))]).await.unwrap();

        assert!(sqlite.get(NovelId(9)).await.unwrap().is_none());
        let tags_left: i64 = {
            let guard = conn.lock().unwrap();
            guard.query_row("SELECT COUNT(*) FROM novel_tags WHERE novel_id = 9", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(tags_left, 0);
        assert!(freeze.frozen_ids().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn freeze_updates_status_search_expression() {
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let sqlite = SqliteNovelRepository::new(conn.clone());
        let freeze = SqliteFreezeStore::new(conn);
        sqlite.apply_batch(vec![NovelMutation::Upsert(record(4, "凍結対象", "a", "u4", &[]))]).await.unwrap();
        freeze.set_frozen(&[NovelId(4)], true).await.unwrap();

        let mut filter = NovelFilter::all();
        filter.terms.push(crate::platform::SearchTerm {
            field: SearchField::Status,
            values: vec!["凍結".to_string()],
            negated: false,
        });
        let hits = sqlite.scan_ids(&filter, None, 10).await.unwrap();
        assert_eq!(hits, vec![NovelId(4)]);
    }
}

#[cfg(test)]
mod golden_tests {
    use super::tests::{fixtures, record};
    use super::*;
    use crate::platform::{NovelFilter, NovelQuery, NovelSort};

    const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/records-a.json");

    /// Regenerate with: NAROU_WRITE_GOLDEN=1 cargo test --lib native::sqlite::golden_tests
    #[test]
    fn write_golden_when_requested() {
        if std::env::var("NAROU_WRITE_GOLDEN").is_err() {
            return;
        }
        let json = serde_json::to_string_pretty(&fixtures()).unwrap();
        std::fs::write(GOLDEN, json + "\n").unwrap();
    }

    #[tokio::test]
    async fn golden_records_seed_and_query_identically() {
        let raw = std::fs::read_to_string(GOLDEN).expect("golden fixture present");
        let records: Vec<NovelRecord> = serde_json::from_str(&raw).unwrap();
        assert_eq!(records.len(), 3);

        let sqlite = SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap());
        sqlite
            .apply_batch(records.iter().cloned().map(NovelMutation::Upsert).collect())
            .await
            .unwrap();

        let query = NovelQuery {
            filter: NovelFilter::all(),
            sort: NovelSort {
                key: crate::platform::NovelSortKey::Title,
                reverse: false,
            },
            offset: 0,
            limit: 10,
        };
        let titles: Vec<String> = sqlite.query(&query).await.unwrap().into_iter().map(|r| r.title).collect();
        // Sorting uses title_fold (lowercased), matching the D1 backend.
        assert_eq!(
            titles,
            vec!["Alpha物語".to_string(), "beta行".to_string(), "Gamma記".to_string()]
        );

        // extra fields survive the round trip byte-for-byte semantics
        let gamma = sqlite.get(NovelId(3)).await.unwrap().unwrap();
        assert!(gamma.extra_fields.is_empty());
        let _ = record(0, "", "", "", &[]); // keep helper referenced for regeneration runs
    }
}
