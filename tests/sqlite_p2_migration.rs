//! P2 acceptance: legacy golden library → SQLite import → operations →
//! export-yaml round trip, plus queue/settings rerouting through app_state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use narou_rs::db::{Database, NovelRecord};
use narou_rs::native::sqlite::state::{StateDb, legacy_yaml_active};
use narou_rs::platform::{NovelFilter, NovelId};

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
