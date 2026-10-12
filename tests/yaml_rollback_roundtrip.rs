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

/// A payload that cannot be read must not be reported as a completed cutover:
/// the export fails and the library stays on SQLite.
#[test]
fn failed_export_keeps_sqlite_selected() {
    let _guard = LIBRARY_LOCK.lock();
    let (_temp, root) = temp_library();
    let _cwd = CwdGuard::enter(&root);
    let store = db_only_store(&root);
    let keys = seed_novel(&store);

    // Corrupt the stored CRC-32 of one object, as a damaged database would be.
    let connection = narou_rs::native::sqlite::open(&root.join(".narou/db.sqlite")).unwrap();
    {
        let guard = connection.lock().expect("sqlite mutex poisoned");
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
    // The object walk runs in key order and writes before it flips anything:
    // the objects that sort before the corrupt one (`setting.ini` <
    // `toc.yaml`) are already on disk, and the marker is untouched.
    assert!(
        root.join("小説データ/site/title/setting.ini").is_file(),
        "content is written before the marker switches"
    );
    assert!(root.join(".narou/database.yaml").is_file());
}
