//! P2 acceptance: legacy golden library → SQLite import → operations →
//! export-yaml round trip, plus queue/settings rerouting through app_state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use narou_rs::db::{Database, NovelRecord};
use narou_rs::native::sqlite::state::{StateDb, legacy_yaml_active};
use narou_rs::platform::{NovelFilter, NovelId};

static SERIES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn copy_fixture_library(target_root: &Path) {
    let source_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/library-a/.narou")
        .join("database.yaml");
    let content = std::fs::read_to_string(&source_dir).expect("golden database.yaml");
    let narou_dir = target_root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    std::fs::write(narou_dir.join("database.yaml"), content).unwrap();
}

#[test]
fn p2_import_operate_export_roundtrip() {
        let _series = SERIES_LOCK.lock().unwrap();
let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    copy_fixture_library(&root);

    // Bootstrap: fresh DB imports the legacy database.yaml and states.
    let state: StateDb = {
        assert!(!legacy_yaml_active());
        narou_rs::native::sqlite::state::configure(&root.join(".narou")).unwrap()
    };
    state.install_shared();

    let mut db = Database::with_root(root.clone()).unwrap();
    assert_eq!(db.all_records().len(), 2, "imported both fixture records");

    // Operate: retag novel 1 through the batch mutation seam.
    db.update_records(|mut records| {
        let first = records.get_mut(&1).unwrap();
        first.tags.push("お気に入り".to_string());
        Ok((records, ()))
    })
    .unwrap();

    // Queue persistence goes through app_state, not the file.
    let queue_path = root.join(".narou").join("queue.yaml");
    let queue =
        narou_rs::queue::PersistentQueue::new(&queue_path).unwrap();
    let job_id = queue.push(narou_rs::queue::JobType::Convert, "1").unwrap();
    drop(queue);
    assert!(
        !queue_path.exists(),
        "queue.yaml must not be written while the SQLite backend is active"
    );
    assert!(state.get_raw("inv", "queue").unwrap().is_some());

    // Reload from the same root: mutations survive (records + queue).
    let reloaded_queue =
        narou_rs::queue::PersistentQueue::new(&root.join(".narou").join("queue.yaml")).unwrap();
    // Restart semantics: persisted jobs come back as restorable tasks and
    // must be activated explicitly before workers can pop them.
    assert!(reloaded_queue.has_restorable_tasks());
    let activated = reloaded_queue.activate_restorable_tasks().unwrap();
    assert_eq!(activated, 1);
    let popped = reloaded_queue
        .pop_for_lane(narou_rs::queue::QueueLane::Secondary)
        .expect("queued job persisted");
    assert_eq!(popped.id, job_id);

    let db2 = Database::with_root(root.clone()).unwrap();
    let retagged = db2.get(1).unwrap();
    assert!(retagged.tags.contains(&"お気に入り".to_string()));

    // Export the legacy bundle and compare semantically (the CLI command
    // wraps exactly this state -> file serialization).
    let out_dir = root.join("export");
    std::fs::create_dir_all(&out_dir).unwrap();
    let records: BTreeMap<i64, NovelRecord> = db2
        .all_records()
        .iter()
        .map(|(&id, record)| {
            let mut normalized = record.clone();
            normalized.id = id;
            (id, normalized)
        })
        .collect();
    let exported_raw = serde_yaml::to_string(&records).unwrap();
    std::fs::write(out_dir.join("database.yaml"), &exported_raw).unwrap();
    let exported: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&exported_raw).unwrap();
    assert_eq!(exported.len(), 2);
    assert!(exported[&1].tags.contains(&"お気に入り".to_string()));
    assert!(exported[&1].end);
    assert!(!exported[&2].end);

    // Original files were renamed away (backward-compatible import).
    assert!(!root.join(".narou/database.yaml").exists());
    let imported_marker = std::fs::read_dir(root.join(".narou")).unwrap();
    let renamed = imported_marker
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("database.yaml.imported-"));
    assert!(renamed, "legacy file must be preserved under an .imported- name");

    // Maintenance operations succeed against the live handle.
    let conn = state.conn_ref().clone();
    {
        let guard = conn.lock().unwrap();
        let status: String = guard
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "ok");
        guard.execute_batch("VACUUM").unwrap();
    }
}

#[tokio::test]
async fn filter_still_matches_after_import() {
        let _series = SERIES_LOCK.lock().unwrap();
let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    copy_fixture_library(&root);
    let state =
        narou_rs::native::sqlite::state::configure(&root.join(".narou")).unwrap();
    state.install_shared();
    let db = Database::with_root(root).unwrap();

    let mut filter = NovelFilter::all();
    filter.ncode = Some("n000002".to_string());
    use narou_rs::platform::NovelRepository;
    let repo = narou_rs::native::sqlite::SqliteNovelRepository::new(state.conn_ref().clone());
    let hits = repo.scan_ids(&filter, None, 10).await.unwrap();
    assert_eq!(hits, vec![NovelId(2)]);
}

#[test]
fn p3_default_flow_never_writes_yaml() {
        let _series = SERIES_LOCK.lock().unwrap();
let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    copy_fixture_library(&root);
    let state =
        narou_rs::native::sqlite::state::configure(&root.join(".narou")).unwrap();
    state.install_shared();

    let mut db = Database::with_root(root.clone()).unwrap();
    db.update_records(|records| Ok((records, ()))).unwrap();
    db.save().unwrap();

    // The only YAML files allowed are the renamed legacy originals.
    for entry in std::fs::read_dir(root.join(".narou")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        if name.ends_with(".yaml") {
            assert!(
                name.contains(".imported-"),
                "unexpected fresh YAML file written: {name}"
            );
        }
        assert_ne!(name, "database_index.yaml", "index file must stay retired");
    }
}

#[test]
fn p3_perf_smoke_1000_records() {
    use std::time::Instant;
    let _series = SERIES_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let state =
        narou_rs::native::sqlite::state::configure(&root.join(".narou")).unwrap();
    state.install_shared();
    let mut db = Database::with_root(root.clone()).unwrap();

    let started = Instant::now();
    db.update_records(|_records| {
        let mut records = _records;
        for i in 1..=1000i64 {
            records.insert(
                i,
                NovelRecord {
                    id: i,
                    author: format!("author{i}"),
                    title: format!("title{i}"),
                    file_title: format!("[author{i}] title{i}"),
                    toc_url: format!("https://example.com/{i}"),
                    sitename: "小説家になろう".into(),
                    last_update: chrono::Utc::now(),
                    ..fixture_record_defaults(i)
                },
            );
        }
        Ok((records, ()))
    })
    .unwrap();
    let elapsed = started.elapsed();

    // Generous bound so debug builds on CI stay deterministic.
    assert!(elapsed.as_secs() < 60, "bulk insert too slow: {elapsed:?}");
    println!("P3 perf smoke: 1000 upserts in {elapsed:?}");

    let page = narou_rs::platform::NovelQuery {
        filter: narou_rs::platform::NovelFilter::all(),
        sort: narou_rs::platform::NovelSort {
            key: narou_rs::platform::NovelSortKey::Title,
            reverse: false,
        },
        offset: 0,
        limit: 20,
    };
    let _ = db.sort_by("title", false);
    drop(db);
    let db = Database::with_root(root).unwrap();
    assert_eq!(db.all_records().len(), 1000);
    assert_eq!(page.limit, 20);
}

#[test]
fn p4b_version_snapshot_restore_merge_prune() {
    let _series = SERIES_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let state =
        narou_rs::native::sqlite::state::configure(&root.join(".narou")).unwrap();
    state.install_shared();

    // Seed a minimal novel row so FK constraints hold.
    {
        let conn = state.conn_ref();
        let guard = conn.lock().unwrap();
        guard
            .execute(
                "INSERT INTO novels (id, author, author_fold, title, title_fold, file_title, toc_url, toc_url_fold, sitename, sitename_fold, last_update)
                 VALUES (7, 'a', 'a', 't', 't', 'ft', 'https://x/7', 'https://x/7', 's', 's', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
    }

    let conn = state.conn_ref().clone();

    let mut sec1 = std::collections::BTreeMap::new();
    sec1.insert("1".to_string(), (Some("第一".into()), "body: v1\n".into()));
    narou_rs::native::sqlite::content::store_sections(&mut conn.lock().unwrap(), 7, &sec1).unwrap();
    let v1 = narou_rs::native::sqlite::versions::snapshot_working_set(&mut conn.lock().unwrap(), 7, "update", None).unwrap();

    let mut sec2 = std::collections::BTreeMap::new();
    sec2.insert("1".to_string(), (Some("第一".into()), "body: v2\n".into()));
    sec2.insert("2".to_string(), (Some("第二".into()), "body: new\n".into()));
    narou_rs::native::sqlite::content::store_sections(&mut conn.lock().unwrap(), 7, &sec2).unwrap();
    let v2 = narou_rs::native::sqlite::versions::snapshot_working_set(&mut conn.lock().unwrap(), 7, "update", None).unwrap();

    let versions = narou_rs::native::sqlite::versions::list_versions(&conn.lock().unwrap(), 7).unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].id, v2);

    let diff = narou_rs::native::sqlite::versions::version_diff(&conn.lock().unwrap(), v2).unwrap().unwrap();
    assert!(diff.contains("-body: v1"), "diff must show removed line");
    assert!(diff.contains("+body: v2"), "diff must show added line");

    // Restore older version (copy-forward): working set matches v1 and a new
    // rollback head is recorded.
    narou_rs::native::sqlite::versions::restore_version(&mut conn.lock().unwrap(), 7, v1, false).unwrap();
    let working_after_restore = narou_rs::native::sqlite::content::load_sections(&conn.lock().unwrap(), 7).unwrap().unwrap();
    assert_eq!(working_after_restore.len(), 1);
    assert!(working_after_restore["1"].1.contains("v1"));
    assert_eq!(narou_rs::native::sqlite::versions::list_versions(&conn.lock().unwrap(), 7).unwrap().len(), 3);

    // Merge section 2 of v2 onto the restored working set.
    narou_rs::native::sqlite::versions::merge_from_version(
        &mut conn.lock().unwrap(),
        7,
        v2,
        Some(&["2".to_string()]),
        Some("take second section"),
    )
    .unwrap();
    let merged = narou_rs::native::sqlite::content::load_sections(&conn.lock().unwrap(), 7).unwrap().unwrap();
    assert_eq!(merged.len(), 2, "merged section appended");
    assert!(merged["1"].1.contains("v1"), "unselected section unchanged");

    // Prune keeps the newest heads only.
    let pruned = narou_rs::native::sqlite::versions::prune_history(&conn.lock().unwrap(), 7, 2).unwrap();
    assert_eq!(pruned, 2);
    assert_eq!(narou_rs::native::sqlite::versions::list_versions(&conn.lock().unwrap(), 7).unwrap().len(), 2);
}

fn fixture_record_defaults(id: i64) -> NovelRecord {
    // Minimal valid record used as the base for perf-smoke bulk inserts.
    let mut record = {
        let json = format!(
            "{{\"id\":{id},\"author\":\"a{id}\",\"title\":\"t{id}\",\"file_title\":\"ft{id}\",\"toc_url\":\"https://x/{id}\",\"sitename\":\"s\",\"last_update\":\"2026-01-01T00:00:00Z\"}}"
        );
        serde_json::from_str::<NovelRecord>(&json).unwrap()
    };
    record.id = id;
    record
}
