#![cfg(feature = "native-runtime")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use narou_rs::application::webui::diff::{plan_diff_cache_clean, read_diff_cache_catalog};
use narou_rs::native::object_store::NativeObjectStore;
use narou_rs::native::sqlite::object_store::SqliteObjectStore;
use narou_rs::native::sqlite::state::{self, StorageMode};
use narou_rs::platform::{NovelObjectKeys, ObjectKey, ObjectStore};

static LIBRARY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

struct CwdGuard(PathBuf);

impl CwdGuard {
    fn enter(root: &Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(root).unwrap();
        Self(previous)
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

fn prepare_library() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".narousetting")).unwrap();
    state::write_mode(&temp.path().join(".narou"), StorageMode::Sqlite).unwrap();
    temp
}

fn db_only_store(root: &Path) -> SqliteObjectStore {
    narou_rs::db::init_database().unwrap();
    let handle = state::active_for(&root.join(".narou")).unwrap();
    let mirror = NativeObjectStore::from_root(root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR)).unwrap();
    SqliteObjectStore::with_mirror_files(handle.conn_ref().clone(), mirror, false)
}

#[test]
fn empty_legacy_versions_survive_real_import_and_yaml_export() {
    let _guard = LIBRARY_LOCK.lock();
    let temp = prepare_library();
    let root = temp.path();
    let _cwd = CwdGuard::enter(root);
    let archive = root.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    for relative in [
        "site/title/本文/cache/v1",
        "site/title/本文/.cache/v2",
        "site/title/本文/cache/v3",
        "site/title/raw/cache/not-a-version",
        "site/title/本文/cache/not-a-version/nested",
    ] {
        std::fs::create_dir_all(archive.join(relative)).unwrap();
    }
    let original_marker = archive.join("site/title/本文/cache/v3/.version");
    std::fs::write(&original_marker, b"existing version record").unwrap();
    let keys = NovelObjectKeys::new("site", "title", false).unwrap();
    let store = db_only_store(root);
    futures::executor::block_on(async {
        let versions = read_diff_cache_catalog(&store, keys.prefix(), &[]).await.unwrap();
        assert_eq!(versions.iter().map(|version| version.version.as_str()).collect::<Vec<_>>(), ["v3", "v2", "v1"]);
        assert!(versions.iter().all(|version| version.only_latest_update));
        assert_eq!(plan_diff_cache_clean(&store, keys.prefix()).await.unwrap().len(), 3,
            "only recognized empty cache-version directories become rows");
        assert_eq!(store.read_small(&keys.cached_version_marker("v3")).await.unwrap().unwrap(), b"existing version record");
    });
    assert!(!archive.join("site/title/本文/cache/v1/.version").exists(), "import only writes database rows");
    assert!(!archive.join("site/title/本文/.cache/v2/.version").exists());
    std::fs::remove_dir_all(archive.join("site")).unwrap();

    let bundle = root.join("exported");
    narou_rs::native::sqlite::export_yaml::export_yaml(Some(bundle.clone()), false).unwrap();
    assert_eq!(state::read_mode(&root.join(".narou")), StorageMode::Sqlite);
    let exported_archive = bundle.join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    for (relative, expected) in [
        ("site/title/本文/cache/v1/.version", b"".as_slice()),
        ("site/title/本文/.cache/v2/.version", b"".as_slice()),
        ("site/title/本文/cache/v3/.version", b"existing version record".as_slice()),
    ] {
        assert_eq!(std::fs::read(exported_archive.join(relative)).unwrap(), expected);
    }
    assert!(!exported_archive.join("site/title/raw/cache/not-a-version").exists());
    assert!(!exported_archive.join("site/title/本文/cache/not-a-version").exists());
    let exported = NativeObjectStore::from_root(exported_archive).unwrap();
    let versions = futures::executor::block_on(read_diff_cache_catalog(&exported, keys.prefix(), &[])).unwrap();
    assert_eq!(versions.iter().map(|version| version.version.as_str()).collect::<Vec<_>>(), ["v3", "v2", "v1"]);
    assert!(versions.iter().all(|version| version.only_latest_update));
}

#[test]
fn cli_clean_and_all_clean_remove_db_only_objects_and_legacy_empty_directories() {
    let _guard = LIBRARY_LOCK.lock();
    let temp = prepare_library();
    let root = temp.path();
    let _cwd = CwdGuard::enter(root);
    let store = db_only_store(root);
    let record: narou_rs::db::NovelRecord = serde_yaml::from_str(
        "id: 7\nauthor: author\ntitle: Fixture\nfile_title: title\ntoc_url: 'https://example.test/7'\nsitename: site\nlast_update: '2026-10-13T00:00:00Z'\n",
    ).unwrap();
    narou_rs::native::novel_repository::NativeNovelRepository::new()
        .apply_batch_sync(vec![narou_rs::platform::NovelMutation::Upsert(record)]).unwrap();
    let keys = NovelObjectKeys::new("site", "title", false).unwrap();
    futures::executor::block_on(async {
        store.write_small(&keys.toc(), b"title: Fixture\nauthor: author\ntoc_url: https://example.test/7\nsubtitles: []\n".to_vec()).await.unwrap();
        store.write_small(&keys.section("1", "keep"), b"current body".to_vec()).await.unwrap();
    });
    assert!(!root.join("小説データ/site/title").exists());
    let legacy_marker = ObjectKey::try_new(format!("{}/本文/.cache/v1/.version", keys.prefix())).unwrap();
    for args in [&["diff", "--clean", "7"][..], &["diff", "--all-clean"][..]] {
        futures::executor::block_on(async {
            store.write_small(&legacy_marker, Vec::new()).await.unwrap();
            store.write_small(&keys.cached_version_marker("v2"), Vec::new()).await.unwrap();
            store.write_small(&keys.cached_section("v2", "1", "old"), b"cached body".to_vec()).await.unwrap();
        });
        let empty_dir = root.join("小説データ/site/title/本文/.cache/legacy-empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_narou_rs"))
            .args(args)
            .current_dir(root)
            .env("USERPROFILE", root)
            .env("HOME", root)
            .env("NAROU_RS_MIRROR_FILES", "false")
            .stdin(Stdio::null())
            .output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(!empty_dir.exists(), "legacy empty versions must be removed by native I/O");
        futures::executor::block_on(async {
            assert!(plan_diff_cache_clean(&store, keys.prefix()).await.unwrap().is_empty());
            assert!(store.read_small(&keys.toc()).await.unwrap().is_some());
            assert_eq!(store.read_small(&keys.section("1", "keep")).await.unwrap().unwrap(), b"current body");
        });
    }
}
