//! DB-only (no filesystem mirror) rollback: `db export-yaml --in-place` must
//! render the stored content, settings and user site definitions back into the
//! narou.rb layout, and only then switch the storage marker.
//!
//! SQLite mode is process-wide here (the cwd-based library lookup, the
//! `db.sqlite` handle cache and the `Database` slot all belong to the process),
//! so this file keeps its tests serialized and points the cwd at a temp
//! library before anything resolves the root.

use std::future::Future;
use std::path::{Path, PathBuf};

use narou_rs::native::object_store::NativeObjectStore;
use narou_rs::native::sqlite::object_store::SqliteObjectStore;
use narou_rs::native::sqlite::state::{self, StorageMode};
use narou_rs::platform::{NovelObjectKeys, ObjectKey, ObjectStore};

/// One lock for the whole binary: the library root is the process cwd.
static LIBRARY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Restore the previous working directory when the test ends, so the temp
/// library can be removed afterwards.
struct CwdGuard(PathBuf);

impl CwdGuard {
    fn enter(root: &Path) -> Self {
        let previous = std::env::current_dir().expect("cwd is readable");
        std::env::set_current_dir(root).expect("temp library is reachable");
        Self(previous)
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// A scoped library root. These cases never seed the legacy global-setting row,
/// so rollback does not write into the user's shared global-setting directory.
fn temp_library() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".narousetting")).unwrap();
    (temp, root)
}

/// SQLite mode with the mirror disabled: every byte lives in `db.sqlite`.
///
/// The cwd must already point at `root` — `init_database` resolves the library
/// from the working directory.
fn db_only_store(root: &Path) -> SqliteObjectStore {
    let narou_dir = root.join(".narou");
    state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
    narou_rs::db::init_database().unwrap();

    let state = state::active_for(&narou_dir).expect("sqlite mode is active");
    let mirror =
        NativeObjectStore::from_root(root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR)).unwrap();
    SqliteObjectStore::with_mirror_files(state.conn_ref().clone(), mirror, false)
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    futures::executor::block_on(future)
}

/// Write one novel's stored content (toc, settings, one section) plus a user
/// site definition, all with the mirror disabled.
fn seed_novel(store: &SqliteObjectStore) -> NovelObjectKeys {
    let keys = NovelObjectKeys::new("site", "title", false).unwrap();
    block_on(async {
        store
            .write_small(&keys.toc(), b"---\ntitle: title\nauthor: author\n".to_vec())
            .await
            .unwrap();
        store
            .write_small(
                &keys.setting(),
                b"[global]\nforce-title=1\n; user edit\n".to_vec(),
            )
            .await
            .unwrap();
        store
            .write_small(
                &keys.section("1", "第一話"),
                "---\nsubtitle: 第一話\nbody: |\n  本文\n".as_bytes().to_vec(),
            )
            .await
            .unwrap();
        let definition = ObjectKey::try_new("webnovel/custom.example.com.yaml").unwrap();
        store
            .write_small(&definition, b"# user site definition\n".to_vec())
            .await
            .unwrap();
    });
    keys
}

/// The rollback renders everything the database holds, and the marker flips
/// only once all of it is on disk.
#[test]
fn in_place_export_restores_db_only_content_and_definitions() {
    let _guard = LIBRARY_LOCK.lock();
    let (_temp, root) = temp_library();
    let _cwd = CwdGuard::enter(&root);
    let store = db_only_store(&root);
    seed_novel(&store);

    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    let toc = archive.join("site/title/toc.yaml");
    assert!(
        !toc.exists(),
        "the seeding must leave the archive root empty (mirror disabled)"
    );

    let out = narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap();
    assert_eq!(out, root.join(".narou"));
    assert_eq!(
        state::read_mode(&root.join(".narou")),
        StorageMode::Yaml,
        "an in-place export switches the marker"
    );
    assert_eq!(
        std::fs::read(&toc).unwrap(),
        b"---\ntitle: title\nauthor: author\n"
    );
    assert_eq!(
        std::fs::read(archive.join("site/title/setting.ini")).unwrap(),
        b"[global]\nforce-title=1\n; user edit\n"
    );
    assert_eq!(
        std::fs::read(archive.join("site/title/本文/1 第一話.yaml")).unwrap(),
        "---\nsubtitle: 第一話\nbody: |\n  本文\n".as_bytes()
    );
    assert_eq!(
        std::fs::read(root.join("webnovel/custom.example.com.yaml")).unwrap(),
        b"# user site definition\n",
        "custom site definitions must land where YAML mode reads them"
    );
    assert!(root.join(".narou/database.yaml").is_file());
}

/// A running Web process can hold a stale Database cache while a CLI process
/// changes SQLite. Export both records and their index from the authoritative
/// source, not from that older cache.
#[test]
fn export_uses_current_sqlite_records_and_matching_index() {
    let _guard = LIBRARY_LOCK.lock();
    for in_place in [false, true] {
        let (_temp, root) = temp_library();
        let _cwd = CwdGuard::enter(&root);
        let _store = db_only_store(&root);
        let record = |id, title: &str| -> narou_rs::db::NovelRecord {
            serde_yaml::from_str(&format!(
                "id: {id}\nauthor: author\ntitle: {title}\nfile_title: title-{id}\ntoc_url: https://example.com/{id}/\nsitename: site\nlast_update: '2026-10-23T00:00:00Z'\n"
            )).unwrap()
        };
        narou_rs::db::with_database_mut(|db| db.update_records(|mut records| {
            records.insert(1, record(1, "Before"));
            records.insert(3, record(3, "Removed"));
            Ok((records, ()))
        })).unwrap();
        let mut external = narou_rs::db::Database::with_root(root.clone()).unwrap();
        external.update_records(|mut records| {
            records.get_mut(&1).unwrap().title = "After".into();
            records.insert(2, record(2, "New"));
            records.remove(&3);
            Ok((records, ()))
        }).unwrap();
        narou_rs::db::with_database(|db| {
            assert_eq!(db.get(1).unwrap().title, "Before", "fixture must retain a stale server cache");
            assert!(db.get(2).is_none());
            assert!(db.get(3).is_some());
            Ok(())
        }).unwrap();

        let out = narou_rs::native::sqlite::export_yaml::export_yaml(
            Some(root.join("bundle")), in_place,
        ).unwrap();
        let exported: std::collections::BTreeMap<i64, narou_rs::db::NovelRecord> =
            serde_yaml::from_slice(&std::fs::read(out.join("database.yaml")).unwrap()).unwrap();
        assert_eq!(exported[&1].title, "After");
        assert!(exported.contains_key(&2), "an acknowledged new novel was omitted from export");
        assert!(!exported.contains_key(&3), "export resurrected an externally removed novel");
        let index: narou_rs::db::index_store::IndexStoreData =
            serde_yaml::from_slice(&std::fs::read(out.join("database_index.yaml")).unwrap()).unwrap();
        assert_eq!(index.meta[&1].title.as_deref(), Some("After"));
        assert!(index.meta.contains_key(&2));
        assert!(!index.meta.contains_key(&3));
    }
}

/// Failed rollback projections must not replace writes acknowledged afterwards.
#[test]
fn failed_export_preserves_subsequent_state_and_record_writes() {
    let _guard = LIBRARY_LOCK.lock();
    let (_temp, root) = temp_library();
    let _cwd = CwdGuard::enter(&root);
    let store = db_only_store(&root);
    let keys = seed_novel(&store);
    let state = state::active_for(&root.join(".narou")).unwrap();
    state.set_raw("inv", "notepad", "before").unwrap();
    state.set_raw("inv", "local_setting", "download.interval: 1\n").unwrap();
    state.set_raw("inv", "alias", "before: 7\n").unwrap();
    state.set_raw("meta", "database_yaml_sha", "previous-mirror-fingerprint").unwrap();
    let mut record: narou_rs::db::NovelRecord = serde_yaml::from_str(
        "id: 7\nauthor: author\ntitle: Before\nfile_title: title\ntoc_url: https://example.com/7/\nsitename: site\nlast_update: 2026-10-14 00:00:00.000000000 +00:00\n",
    ).unwrap();
    narou_rs::db::with_database_mut(|db| {
        db.insert(record.clone());
        db.save()
    }).unwrap();

    // Corrupt the stored CRC-32 of one object, as a damaged database would be.
    let connection = narou_rs::native::sqlite::open(&root.join(".narou/db.sqlite")).unwrap();
    {
        let guard = connection.lock();
        guard
            .execute(
                "UPDATE objects SET crc32 = crc32 + 1 WHERE object_key = ?",
                rusqlite::params![keys.toc().as_ref()],
            )
            .unwrap();
    }

    let error = narou_rs::native::sqlite::export_yaml::export_yaml(None, true)
        .expect_err("a corrupt payload must fail the export");
    assert!(error.to_string().contains("toc.yaml"), "{error}");
    assert_eq!(
        state::read_mode(&root.join(".narou")),
        StorageMode::Sqlite,
        "a failed export must leave SQLite selected"
    );
    state.set_raw("inv", "notepad", "after").unwrap();
    state.set_raw("inv", "local_setting", "download.interval: 2\n").unwrap();
    state.set_raw("inv", "alias", "after: 7\n").unwrap();
    record.title = "After".to_string();
    narou_rs::db::with_database_mut(|db| {
        db.insert(record);
        db.save()
    }).unwrap();

    let reopened = state::configure(&root.join(".narou")).unwrap();
    assert_eq!(reopened.get_raw("inv", "notepad").unwrap().as_deref(), Some("after"));
    assert_eq!(reopened.get_raw("inv", "local_setting").unwrap().as_deref(), Some("download.interval: 2\n"));
    assert_eq!(reopened.get_raw("inv", "alias").unwrap().as_deref(), Some("after: 7\n"));
    let db = narou_rs::db::Database::with_root(root).unwrap();
    assert_eq!(db.get(7).unwrap().title, "After");
}

#[test]
fn failed_metadata_or_marker_publish_preserves_later_writes() {
    let _guard = LIBRARY_LOCK.lock();
    for conflict in ["notepad.txt", "storage-backend.lock"] {
        let (_temp, root) = temp_library();
        let _cwd = CwdGuard::enter(&root);
        let store = db_only_store(&root);
        seed_novel(&store);
        let managed = root.join(".narou");
        let state = state::active_for(&managed).unwrap();
        state.set_raw("inv", "alias", "before: 7\n").unwrap();
        state.set_raw("inv", "notepad", "before").unwrap();
        let path = managed.join(conflict);
        if path.is_file() {
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::create_dir(&path).unwrap();

        narou_rs::native::sqlite::export_yaml::export_yaml(None, true)
            .expect_err("a conflicting output must prevent cutover");
        assert_eq!(state::read_mode(&managed), StorageMode::Sqlite);
        state.set_raw("inv", "alias", "after: 7\n").unwrap();
        state.set_raw("inv", "notepad", "after").unwrap();
        let reopened = state::configure(&managed).unwrap();
        assert_eq!(reopened.get_raw("inv", "alias").unwrap().as_deref(), Some("after: 7\n"), "{conflict}");
        assert_eq!(reopened.get_raw("inv", "notepad").unwrap().as_deref(), Some("after"), "{conflict}");
    }
}

#[test]
fn edited_failed_export_file_remains_a_legacy_import_source() {
    let _guard = LIBRARY_LOCK.lock();
    let (_temp, root) = temp_library();
    let _cwd = CwdGuard::enter(&root);
    let store = db_only_store(&root);
    seed_novel(&store);
    let managed = root.join(".narou");
    let state = state::active_for(&managed).unwrap();
    state.set_raw("inv", "alias", "before: 7\n").unwrap();
    std::fs::create_dir_all(root.join("小説データ/site/title/toc.yaml")).unwrap();
    narou_rs::native::sqlite::export_yaml::export_yaml(None, true).unwrap_err();

    std::fs::write(managed.join("alias.yaml"), "edited: 7\n").unwrap();
    let reopened = state::configure(&managed).unwrap();
    assert_eq!(reopened.get_raw("inv", "alias").unwrap().as_deref(), Some("edited: 7\n"));
}

#[test]
fn bundle_export_into_live_management_directory_does_not_reimport_stale_state() {
    let _guard = LIBRARY_LOCK.lock();
    let (_temp, root) = temp_library();
    let _cwd = CwdGuard::enter(&root);
    let store = db_only_store(&root);
    seed_novel(&store);
    let managed = root.join(".narou");
    let state = state::active_for(&managed).unwrap();
    state.set_raw("inv", "notepad", "before").unwrap();

    narou_rs::native::sqlite::export_yaml::export_yaml(Some(managed.clone()), false).unwrap();
    assert_eq!(state::read_mode(&managed), StorageMode::Sqlite);
    state.set_raw("inv", "notepad", "after").unwrap();
    let reopened = state::configure(&managed).unwrap();
    assert_eq!(reopened.get_raw("inv", "notepad").unwrap().as_deref(), Some("after"));
}
