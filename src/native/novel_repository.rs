//! Native novel repository over the shared YAML database state.
//!
//! This is the native implementation of [`NovelRepository`]. It shares the
//! single `db::DATABASE` global with the legacy Inventory access — there is
//! exactly one in-memory `Database` per YAML file, so cache/index/save state
//! can never diverge (Phase 3 §14).
//!
//! All blocking work (YAML serialize, file lock, atomic rename, fsync) runs
//! inside `spawn_blocking` when a tokio runtime is available, so long
//! operations never block an async executor worker directly. When called
//! outside a runtime (sync CLI tests), the operation runs inline — the
//! `parking_lot` lock is never held across an `.await`.
//!
//! The repository only owns novel-record operations. Inventory/settings and
//! archive-root path access remain on `db::DATABASE` / `db::paths` (Phase 4
//! targets).

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::repository::record_matches_filter;
use crate::platform::{
    NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository, PlatformFuture,
};

/// Native repository. Stateless: every operation locks the shared
/// [`crate::db::DATABASE`] global for the duration of its (short, blocking)
/// section.
pub struct NativeNovelRepository;

impl Default for NativeNovelRepository {
    fn default() -> Self {
        Self
    }

}

impl NativeNovelRepository {
    pub fn new() -> Self {
        Self
    }

    /// Synchronous entry points for native-only callers (CLI commands).
    /// These never spawn blocking work — they lock the shared database
    /// state directly, matching the old `with_database` behaviour.
    pub fn get_sync(&self, id: NovelId) -> Result<Option<NovelRecord>> {
        crate::db::with_database(|db| Ok(db.get(id.0).cloned()))
    }

    pub fn find_by_toc_url_sync(&self, url: &str) -> Result<Option<NovelRecord>> {
        crate::db::with_database(|db| Ok(db.get_by_toc_url(url).cloned()))
    }

    pub fn find_by_title_sync(&self, title: &str) -> Result<Option<NovelRecord>> {
        crate::db::with_database(|db| Ok(db.find_by_title(title).cloned()))
    }

    pub fn find_by_ncode_sync(&self, ncode: &str) -> Result<Option<NovelRecord>> {
        Self::find_by_ncode_sync_impl(ncode)
    }

    pub fn allocate_id_sync(&self) -> Result<NovelId> {
        crate::db::with_database_mut(|db| Ok(NovelId::from(db.allocate_id())))
    }

    pub fn apply_batch_sync(&self, mutations: Vec<NovelMutation>) -> Result<()> {
        crate::db::with_database_mut(|db| {
            db.update_records(
                |mut records: std::collections::BTreeMap<i64, NovelRecord>| {
                    for mutation in &mutations {
                        match mutation {
                            NovelMutation::Upsert(record) => {
                                let id = record.id;
                                records.insert(id, record.clone());
                            }
                            NovelMutation::Remove(id) => {
                                records.remove(&id.0);
                            }
                        }
                    }
                    Ok((records, ()))
                },
            )
        })
    }

    pub fn query_sync(&self, query: &NovelQuery) -> Result<Vec<NovelRecord>> {
        crate::db::with_database(|db| {
            let mut records: Vec<NovelRecord> = db
                .all_records()
                .values()
                .filter(|r| record_matches_filter(r, &query.filter))
                .cloned()
                .collect();
            records.sort_by(|a, b| {
                crate::db::compare_records_by_key(a, b, query.sort.key.as_db_key())
                    .then_with(|| a.id.cmp(&b.id))
            });
            if query.sort.reverse {
                records.reverse();
            }
            let start = query.offset.min(records.len());
            let end = (start + query.limit).min(records.len());
            Ok(records[start..end].to_vec())
        })
    }

    pub fn scan_ids_sync(
        &self,
        filter: &NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> Result<Vec<NovelId>> {
        let after = after_id.map(|id| id.0).unwrap_or(i64::MIN);
        crate::db::with_database(|db| {
            let mut ids: Vec<NovelId> = db
                .all_records()
                .values()
                .filter(|r| r.id > after && record_matches_filter(r, filter))
                .map(|r| NovelId::from(r.id))
                .collect();
            ids.sort();
            ids.truncate(limit);
            Ok(ids)
        })
    }

    pub fn count_sync(&self, filter: &NovelFilter) -> Result<u64> {
        crate::db::with_database(|db| {
            Ok(db
                .all_records()
                .values()
                .filter(|r| record_matches_filter(r, filter))
                .count() as u64)
        })
    }

    /// Resolve by ncode with the legacy `toc_url` suffix fallback used by
    /// `commands::update::resolve_ncode_to_id`.
    fn find_by_ncode_sync_impl(ncode: &str) -> Result<Option<NovelRecord>> {
        let ncode = ncode.to_lowercase();
        crate::db::with_database(|db| {
            Ok(db.all_records().values().find(|record| {
                record.ncode.as_deref().is_some_and(|nc| nc.eq_ignore_ascii_case(&ncode))
                    || record
                        .toc_url
                        .to_lowercase()
                        .trim_end_matches('/')
                        .ends_with(&format!("/{ncode}"))
            }).cloned())
        })
    }
}

/// Run a blocking database operation, isolating it on a blocking thread when
/// a tokio runtime is available. Never holds the `parking_lot` database lock
/// across an `.await`.
async fn run_db<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle
            .spawn_blocking(f)
            .await
            .map_err(|e| NarouError::Database(format!("repository task failed: {e}")))?,
        Err(_) => f(),
    }
}

impl NovelRepository for NativeNovelRepository {
    fn get<'a>(&'a self, id: NovelId) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| Ok(db.get(id.0).cloned()))
            })
            .await
        })
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let url = url.to_string();
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| Ok(db.get_by_toc_url(&url).cloned()))
            })
            .await
        })
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let title = title.to_string();
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| Ok(db.find_by_title(&title).cloned()))
            })
            .await
        })
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let ncode = ncode.to_string();
        Box::pin(async move {
            run_db(move || Self::find_by_ncode_sync_impl(&ncode)).await
        })
    }

    fn count<'a>(
        &'a self,
        filter: &'a NovelFilter,
    ) -> PlatformFuture<'a, Result<u64>> {
        let filter = filter.clone();
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| {
                    Ok(db.all_records().values().filter(|r| record_matches_filter(r, &filter)).count() as u64)
                })
            })
            .await
        })
    }

    fn query<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        let query = query.clone();
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| {
                    let mut records: Vec<NovelRecord> = db
                        .all_records()
                        .values()
                        .filter(|r| record_matches_filter(r, &query.filter))
                        .cloned()
                        .collect();
                    records.sort_by(|a, b| {
                        crate::db::compare_records_by_key(a, b, query.sort.key.as_db_key())
                            .then_with(|| a.id.cmp(&b.id))
                    });
                    if query.sort.reverse {
                        records.reverse();
                    }
                    let start = query.offset.min(records.len());
                    let end = (start + query.limit).min(records.len());
                    Ok(records[start..end].to_vec())
                })
            })
            .await
        })
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<NovelId>>> {
        let filter = filter.clone();
        let after_id = after_id.map(|id| id.0).unwrap_or(i64::MIN);
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| {
                    let mut ids: Vec<NovelId> = db
                        .all_records()
                        .values()
                        .filter(|r| {
                            r.id > after_id && record_matches_filter(r, &filter)
                        })
                        .map(|r| NovelId::from(r.id))
                        .collect();
                    ids.sort();
                    ids.truncate(limit);
                    Ok(ids)
                })
            })
            .await
        })
    }

    fn allocate_id(&self) -> PlatformFuture<'_, Result<NovelId>> {
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database_mut(|db| Ok(NovelId::from(db.allocate_id())))
            })
            .await
        })
    }

    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database_mut(|db| {
                    db.update_records(
                        |mut records: std::collections::BTreeMap<i64, NovelRecord>| {
                            for mutation in &mutations {
                                match mutation {
                                    NovelMutation::Upsert(record) => {
                                        let id = record.id;
                                        records.insert(id, record.clone());
                                    }
                                    NovelMutation::Remove(id) => {
                                        records.remove(&id.0);
                                    }
                                }
                            }
                            Ok((records, ()))
                        },
                    )
                })
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::NativeNovelRepository;
    use crate::db::{self, Database};
    use crate::db::novel_record::NovelRecord;
    use crate::platform::{NovelFilter, NovelMutation, NovelQuery, NovelRepository};
    use chrono::Utc;
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Arc;

    struct DatabaseGuard(Option<Database>);

    impl Drop for DatabaseGuard {
        fn drop(&mut self) {
            *db::DATABASE.lock() = self.0.take();
        }
    }

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: format!("author-{id}"),
            title: format!("title-{id}"),
            file_title: format!("file-{id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "Example".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["test".to_string()],
            ncode: Some(format!("n{id}")),
            domain: Some("example.com".to_string()),
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            extra_fields: BTreeMap::new(),
        }
    }

    fn isolated_database() -> (tempfile::TempDir, crate::test_support::CurrentDirGuard, DatabaseGuard) {
        let temp = tempfile::tempdir().unwrap();
        let cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let mut slot = db::DATABASE.lock();
        let previous = slot.take();
        drop(slot);
        let guard = DatabaseGuard(previous);
        db::init_database().unwrap();
        (temp, cwd_guard, guard)
    }

    #[test]
    fn yaml_round_trip_preserves_record_shape_and_unknown_fields() {
        let (temp, _cwd_guard, _db_guard) = isolated_database();
        std::fs::write(
            temp.path().join(".narou").join("database.yaml"),
            r#"0:
  id: 999
  author: author
  title: title
  file_title: file title
  toc_url: https://example.com/0/
  sitename: Example
  novel_type: 1
  end:
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  suspend: "yes"
  is_narou: 1
  tags:
    - test
  raw_title: raw title
  custom_flag: true
  nested:
    answer: 42
"#,
        )
        .unwrap();
        db::with_database_mut(|db| db.refresh()).unwrap();

        let novels = NativeNovelRepository::new();
        let mut record = novels.get_sync(0.into()).unwrap().unwrap();
        assert_eq!(record.id, 0);
        assert!(!record.end);
        assert!(record.suspend);
        assert!(record.is_narou);
        assert_eq!(record.raw_title(), "raw title");
        assert_eq!(
            record.extra_fields.get("nested").and_then(|value| {
                value
                    .get("answer")
                    .and_then(serde_yaml::Value::as_i64)
            }),
            Some(42)
        );

        record.title = "updated".to_string();
        novels
            .apply_batch_sync(vec![NovelMutation::Upsert(record)])
            .unwrap();

        let raw = db::with_database(|db| {
            db.inventory()
                .load_raw("database", crate::db::inventory::InventoryScope::Local)
        })
        .unwrap();
        assert!(raw.contains("custom_flag: true"));
        assert!(raw.contains("answer: 42"));
        assert!(raw.contains("title: updated"));

        *db::DATABASE.lock() = None;
        db::init_database().unwrap();
        let reloaded = novels.get_sync(0.into()).unwrap().unwrap();
        assert_eq!(reloaded.title, "updated");
        assert!(!reloaded.end);
        assert!(reloaded.suspend);
        assert_eq!(reloaded.raw_title(), "raw title");
    }

    #[test]
    fn async_allocations_are_unique_and_persisted_under_concurrency() {
        let (_temp, _cwd_guard, _db_guard) = isolated_database();
        let novels = Arc::new(NativeNovelRepository::new());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let ids = runtime.block_on(async {
            futures::future::join_all((0..32).map(|_| {
                let novels = Arc::clone(&novels);
                async move {
                    let id = novels.allocate_id().await.map_err(|err| err.to_string())?;
                    novels
                        .apply_batch(vec![NovelMutation::Upsert(sample_record(id.0))])
                        .await
                        .map_err(|err| err.to_string())?;
                    Ok::<i64, String>(id.0)
                }
            }))
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
        })
        .unwrap();
        drop(runtime);

        let unique: HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 32);
        assert_eq!(unique, (0..32).collect());

        let records = novels
            .query_sync(&NovelQuery::page(
                NovelFilter::all(),
                Default::default(),
                0,
                64,
            ))
            .unwrap();
        assert_eq!(records.len(), 32);
    }
}
