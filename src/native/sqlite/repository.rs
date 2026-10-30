//! rusqlite-backed [`NovelRepository`] and freeze stores.
//!
//! Every statement comes from `crate::db::novel_query`, the same builder the
//! Worker D1 adapter uses, so both backends stay behaviorally identical; this
//! file only supplies the connection handling and the `NovelBind` → driver
//! conversion.

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{params_from_iter, Connection, Row};

use crate::db::NovelRecord;
use crate::db::novel_codec::{dedup_tags, fold, NOVEL_UPSERT_SQL};
use crate::db::novel_query::{
    self, select_by_id_sql, select_by_ncode_sql, select_by_title_sql, select_by_toc_url_sql,
    SqlQuery,
};
use crate::error::{NarouError, Result};
use crate::native::sqlite::record_map::{query_bind, record_from_row, record_params};
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
        let mut guard = conn.lock();
        f(&mut guard)
    })
    .await
    .expect("sqlite blocking task panicked")
}

fn column<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T> {
    row.get(index)
        .map_err(|error| NarouError::Platform(format!("sqlite column {index}: {error}")))
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
    // A re-registered novel can receive the same tag from multiple sources.
    // Normalize before encoding tags_json/tags_sort as well as novel_tags, so
    // the denormalized and indexed representations never disagree.
    let mut normalized = record.clone();
    dedup_tags(&mut normalized.tags);
    let record = &normalized;
    let params = record_params(record)?;
    conn.execute(NOVEL_UPSERT_SQL, params_from_iter(params.iter()))
        .map_err(super::sqlite_error)?;
    conn.execute(novel_query::DELETE_NOVEL_TAGS_SQL, [record.id])
        .map_err(super::sqlite_error)?;
    for (position, tag) in record.tags.iter().enumerate() {
        conn.execute(
            novel_query::INSERT_NOVEL_TAG_SQL,
            rusqlite::params![record.id, position as i64, tag, fold(tag)],
        )
        .map_err(super::sqlite_error)?;
    }
    refresh_status_sort(conn, record.id)?;
    conn.execute(
        novel_query::BUMP_SEQUENCE_SQL,
        rusqlite::params![record.id.saturating_add(1), record.id.saturating_add(1)],
    )
    .map_err(super::sqlite_error)?;
    conn.execute(novel_query::COMMIT_ID_RESERVATION_SQL, [record.id])
        .map_err(super::sqlite_error)?;
    Ok(())
}

pub(crate) fn refresh_status_sort(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(&novel_query::refresh_status_sort_sql(), [id])
        .map_err(super::sqlite_error)?;
    Ok(())
}

/// Project the freeze inventory (`app_state('inv','freeze')` / `freeze.yaml`)
/// onto the relational `frozen_novels` table.
///
/// The payload stays canonical — native reads freeze state through
/// `Inventory` (see `NativeFreezeStore`) — while the shared status *search*
/// expression (`crate::db::novel_query::STATUS_SEARCH_EXPRESSION`, also used
/// by the Worker D1 adapter where `frozen_novels` *is* the store) queries the
/// table. Without
/// this projection the table stayed empty and status filtering silently
/// disagreed with `narou list`.
///
/// `novels.status_sort` needs no refresh: the sort expression deliberately
/// covers only 完結 / 削除 / 中断, not 凍結.
///
/// IDs without a `novels` row are dropped: the table has a foreign key to
/// `novels`, and the payload may outlive a removed novel.
pub(crate) fn sync_frozen_novels(conn: &rusqlite::Transaction<'_>, frozen_ids: &[i64]) -> Result<()> {
    let mut current: Vec<i64> = Vec::new();
    {
        let mut statement = conn
            .prepare("SELECT novel_id FROM frozen_novels")
            .map_err(super::sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(super::sqlite_error)?;
        for row in rows {
            current.push(row.map_err(|error| NarouError::Platform(error.to_string()))?);
        }
    }

    let desired: HashSet<i64> = frozen_ids.iter().copied().collect();
    let current_set: HashSet<i64> = current.iter().copied().collect();
    if desired == current_set {
        return Ok(());
    }

    for id in current_set.difference(&desired) {
        conn.execute("DELETE FROM frozen_novels WHERE novel_id = ?", [id])
            .map_err(super::sqlite_error)?;
    }
    for id in desired.difference(&current_set) {
        conn.execute(
            "INSERT OR IGNORE INTO frozen_novels (novel_id)
             SELECT id FROM novels WHERE id = ?",
            [id],
        )
        .map_err(super::sqlite_error)?;
    }
    Ok(())
}

impl NovelRepository for SqliteNovelRepository {
    fn get<'a>(
        &'a self,
        id: NovelId,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(self.one(select_by_id_sql(), vec![rusqlite::types::Value::Integer(id.0)]))
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(self.one(
            select_by_toc_url_sql(),
            vec![rusqlite::types::Value::Text(fold(url))],
        ))
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(self.one(
            select_by_title_sql(),
            vec![rusqlite::types::Value::Text(fold(title))],
        ))
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> crate::platform::PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let folded = fold(ncode);
        Box::pin(self.one(
            select_by_ncode_sql(),
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
        let SqlQuery { sql, binds } = novel_query::count_sql(filter, query_bind);
        Box::pin(blocking(self.conn.clone(), move |conn| {
            let count: i64 = conn
                .query_row(&sql, params_from_iter(binds.iter()), |row| row.get(0))
                .map_err(super::sqlite_error)?;
            u64::try_from(count)
                .map_err(|_| NarouError::Platform("sqlite count was negative".to_string()))
        }))
    }

    fn query<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> crate::platform::PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        let SqlQuery { sql, binds } = novel_query::query_sql(query, query_bind);
        Box::pin(self.many(sql, binds))
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> crate::platform::PlatformFuture<'a, Result<Vec<NovelId>>> {
        let SqlQuery { sql, binds } = novel_query::scan_ids_sql(filter, after_id, limit, query_bind);
        Box::pin(blocking(self.conn.clone(), move |conn| {
            let mut statement = conn.prepare(&sql).map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(params_from_iter(binds.iter()))
                .map_err(super::sqlite_error)?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                ids.push(NovelId(column(row, 0)?));
            }
            Ok(ids)
        }))
    }

    fn allocate_id(&self) -> crate::platform::PlatformFuture<'_, Result<crate::platform::NovelIdReservation>> {
        Box::pin(blocking(self.conn.clone(), |conn| {
            let tx = conn.transaction().map_err(super::sqlite_error)?;
            tx.execute(novel_query::ALLOCATE_STEP_SQL, [])
                .map_err(super::sqlite_error)?;
            tx.execute(novel_query::ALLOCATE_BUMP_SQL, [])
                .map_err(super::sqlite_error)?;
            let (id, generation): (i64, i64) = tx
                .query_row(novel_query::ALLOCATE_READ_SQL, [], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(super::sqlite_error)?;
            tx.commit().map_err(super::sqlite_error)?;
            Ok(crate::platform::NovelIdReservation::new(NovelId(id), generation))
        }))
    }

    fn release_id(&self, reservation: crate::platform::NovelIdReservation) -> crate::platform::PlatformFuture<'_, Result<()>> {
        Box::pin(blocking(self.conn.clone(), move |conn| {
            conn.execute(novel_query::RELEASE_ID_SQL, [reservation.id().0, reservation.generation()])
                .map_err(super::sqlite_error)?;
            Ok(())
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
            let tx = conn.transaction().map_err(super::sqlite_error)?;
            for mutation in mutations {
                match mutation {
                    NovelMutation::Upsert(record) => upsert_record_conn(&tx, &record)?,
                    NovelMutation::Remove(id) => {
                        tx.execute(novel_query::DELETE_NOVEL_SQL, [id.0])
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
    use crate::platform::{NovelFilter, NovelQuery, NovelSort, NovelSortKey, SearchField};
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
            general_all_no: Some(100 + id),
            length: Some(1000 * id),
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
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
    async fn attribute_search_is_separate_and_obeys_global_switch() {
        let sqlite = SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap());
        let memory = MemoryNovelRepository::new();
        let mut manga = record(1, "絵", "作者", "https://example.test/1", &["手動"]);
        manga.set_attribute_tag("漫画");
        let mut novel = record(2, "文", "作者", "https://example.test/2", &["手動"]);
        novel.set_attribute_tag("小説");
        let writes = vec![NovelMutation::Upsert(manga), NovelMutation::Upsert(novel)];
        sqlite.apply_batch(writes.clone()).await.unwrap();
        memory.apply_batch(writes).await.unwrap();

        for enabled in [false, true] {
            let direct = NovelFilter { tag: Some("漫画".into()), attribute_tags_enabled: enabled, ..Default::default() };
            let search = NovelFilter {
                terms: vec![crate::platform::SearchTerm::new(SearchField::Tag, false, vec!["漫画".into()])],
                attribute_tags_enabled: enabled, ..Default::default()
            };
            let any = NovelFilter {
                terms: vec![crate::platform::SearchTerm::new(SearchField::Any, false, vec!["漫画".into()])],
                attribute_tags_enabled: enabled, ..Default::default()
            };
            let negated = NovelFilter {
                terms: vec![crate::platform::SearchTerm::new(SearchField::Tag, true, vec!["漫画".into()])],
                attribute_tags_enabled: enabled, ..Default::default()
            };
            let excluded = NovelFilter { exclude_tags: vec!["漫画".into()], attribute_tags_enabled: enabled, ..Default::default() };
            for (filter, expected) in [(direct, u64::from(enabled)), (search, u64::from(enabled)), (any, u64::from(enabled)), (negated, if enabled { 1 } else { 2 }), (excluded, if enabled { 1 } else { 2 })] {
                assert_eq!(sqlite.count(&filter).await.unwrap(), expected);
                assert_eq!(memory.count(&filter).await.unwrap(), expected);
            }
        }
        let stored = sqlite.get(NovelId(1)).await.unwrap().unwrap();
        assert_eq!(stored.attribute_tag(), Some("漫画"));
        assert_eq!(stored.tags, vec!["手動"]);
    }

    #[tokio::test]
    async fn sqlite_search_matches_url_ncode_and_numeric_id() {
        let sqlite = SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap());
        sqlite
            .apply_batch(vec![NovelMutation::Upsert(record(
                42,
                "検索対象",
                "作者",
                "https://ncode.syosetu.com/n1980en/",
                &[],
            ))])
            .await
            .unwrap();

        for keyword in [
            "https://ncode.syosetu.com/n1980en/",
            "n1980en",
            "42",
        ] {
            let mut filter = NovelFilter::all();
            filter.terms.push(crate::platform::SearchTerm::new(
                SearchField::Any,
                false,
                vec![keyword.to_string()],
            ));
            assert_eq!(
                sqlite.count(&filter).await.unwrap(),
                1,
                "search value={keyword}"
            );
        }
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

    /// 状態ソートは SQL 側の materialized 値 (`status_sort` =
    /// `novel_query::STATUS_SORT_EXPRESSION`) と可搬な比較器 (`db::sort` の
    /// `status` = `record_status_key`) が同じ順序を返すこと。片方だけ更新すると
    /// native の `list --sort-by status` と Web/Worker 側の並びがずれる。
    #[tokio::test]
    async fn dual_run_status_sort_matches_the_materialized_key() {
        let sqlite = SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap());
        let memory = MemoryNovelRepository::new();
        // 完結 (tag / end フラグ) ・削除 (404) ・中断 (suspend) の組み合わせを用意する。
        let variants: [(Vec<&str>, bool, bool); 6] = [
            (vec!["end"], false, false),
            (vec!["404"], false, false),
            (vec!["end", "404"], true, false),
            (vec![], false, true),
            (vec!["404"], false, true),
            (vec![], false, false),
        ];
        let mutations: Vec<NovelMutation> = variants
            .iter()
            .enumerate()
            .map(|(index, (tags, end, suspend))| {
                let mut record = record(
                    index as i64 + 1,
                    &format!("status-{index}"),
                    "作者",
                    &format!("https://ncode.syosetu.com/n{:06}/", index + 1),
                    tags,
                );
                record.end = *end;
                record.suspend = *suspend;
                NovelMutation::Upsert(record)
            })
            .collect();
        sqlite.apply_batch(mutations.clone()).await.unwrap();
        memory.apply_batch(mutations).await.unwrap();

        for reverse in [false, true] {
            let query = NovelQuery {
                filter: NovelFilter::all(),
                sort: NovelSort { key: NovelSortKey::Status, reverse },
                offset: 0,
                limit: 10,
            };
            let from_sqlite: Vec<i64> = sqlite.query(&query).await.unwrap().iter().map(|r| r.id).collect();
            let from_memory: Vec<i64> = memory.query(&query).await.unwrap().iter().map(|r| r.id).collect();
            assert_eq!(from_sqlite, from_memory, "status sort reverse={reverse}");
        }
    }

    fn reservation_backends() -> [Arc<dyn NovelRepository>; 2] {
        [
            Arc::new(MemoryNovelRepository::new()),
            Arc::new(SqliteNovelRepository::new(crate::native::sqlite::open_in_memory().unwrap())),
        ]
    }

    #[tokio::test]
    async fn unused_ids_are_reused_without_freeing_live_or_committed_owners() {
        for repository in reservation_backends() {
            repository.apply_batch(vec![NovelMutation::Upsert(record(5,"seed","a","seed",&[]))]).await.unwrap();
            let failed = repository.allocate_id().await.unwrap();
            let stale = crate::platform::NovelIdReservation::new(failed.id(),failed.generation());
            let live = repository.allocate_id().await.unwrap();
            repository.release_id(failed).await.unwrap();
            let retry = repository.allocate_id().await.unwrap();
            assert_eq!(retry.id(),NovelId(6));
            assert_eq!(live.id(),NovelId(7));
            assert_ne!(retry.generation(),stale.generation());
            repository.release_id(stale).await.unwrap();
            assert_eq!(repository.allocate_id().await.unwrap().id(),NovelId(8));
            let after_commit = crate::platform::NovelIdReservation::new(retry.id(),retry.generation());
            repository.apply_batch(vec![NovelMutation::Upsert(record(retry.id().0,"registered","a","registered",&[]))]).await.unwrap();
            repository.release_id(retry).await.unwrap();
            assert_eq!(repository.get(NovelId(6)).await.unwrap().unwrap().title,"registered");
            repository.apply_batch(vec![NovelMutation::Remove(NovelId(6))]).await.unwrap();
            repository.release_id(after_commit).await.unwrap();
            assert_eq!(repository.allocate_id().await.unwrap().id(),NovelId(9));
            repository.apply_batch(vec![NovelMutation::Upsert(record(30,"manual","a","manual",&[]))]).await.unwrap();
            assert_eq!(repository.allocate_id().await.unwrap().id(),NovelId(31));
            assert_eq!(repository.get(NovelId(5)).await.unwrap().unwrap().title,"seed");
        }
    }

    #[tokio::test]
    async fn concurrent_allocations_only_reuse_released_unpublished_ids() {
        for repository in reservation_backends() {
            let reservations = futures::future::try_join_all((0..16).map(|_|repository.allocate_id())).await.unwrap();
            let ids: std::collections::BTreeSet<_> = reservations.iter().map(|reservation|reservation.id().0).collect();
            assert_eq!(ids.len(),16,"simultaneous reservations must be distinct");
            let mut released = std::collections::BTreeSet::new();
            for reservation in reservations {
                if reservation.id().0%2==0 {
                    released.insert(reservation.id().0);
                    repository.release_id(reservation).await.unwrap();
                } else {
                    repository.apply_batch(vec![NovelMutation::Upsert(record(reservation.id().0,"committed","a","committed",&[]))]).await.unwrap();
                }
            }
            let reused = futures::future::try_join_all((0..released.len()).map(|_|repository.allocate_id())).await.unwrap();
            let ids: std::collections::BTreeSet<_> = reused.iter().map(|reservation|reservation.id().0).collect();
            assert_eq!(ids,released);
        }
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
            let guard = conn.lock();
            guard.query_row("SELECT COUNT(*) FROM novel_tags WHERE novel_id = 9", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(tags_left, 0);
        assert!(freeze.frozen_ids().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn removed_novel_can_be_reregistered_with_duplicate_tags() {
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let sqlite = SqliteNovelRepository::new(conn.clone());
        let original = record(9, "以前の登録", "a", "u9", &["end", "favorite"]);
        sqlite
            .apply_batch(vec![NovelMutation::Upsert(original)])
            .await
            .unwrap();
        sqlite
            .apply_batch(vec![NovelMutation::Remove(NovelId(9))])
            .await
            .unwrap();

        // Registration paths may merge default tags with tags retained from
        // the previous download. SQLite must not reject the duplicate names.
        let replacement = record(
            9,
            "再登録",
            "a",
            "u9",
            &["favorite", "end", "favorite", "end"],
        );
        sqlite
            .apply_batch(vec![NovelMutation::Upsert(replacement)])
            .await
            .unwrap();

        let loaded = sqlite.get(NovelId(9)).await.unwrap().unwrap();
        assert_eq!(loaded.title, "再登録");
        assert_eq!(loaded.tags, vec!["favorite", "end"]);
        let indexed_tags: Vec<(i64, String)> = {
            let guard = conn.lock();
            let mut stmt = guard
                .prepare("SELECT position, tag FROM novel_tags WHERE novel_id = 9 ORDER BY position")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(
            indexed_tags,
            vec![(0, "favorite".to_string()), (1, "end".to_string())]
        );
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
