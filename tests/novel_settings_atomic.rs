#![cfg(feature = "native-runtime")]

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier, atomic::{AtomicBool, Ordering}};

use narou_rs::application::{NovelSettingsPatch, NovelSettingsService};
use narou_rs::converter::ini::{IniData, IniValue};
use narou_rs::db::NovelRecord;
use narou_rs::error::Result;
use narou_rs::native::object_store::NativeObjectStore;
use narou_rs::native::sqlite::object_store::SqliteObjectStore;
use narou_rs::platform::mocks::MemoryNovelRepository;
use narou_rs::platform::{
    NovelId, NovelObjectKeys, ObjectKey, ObjectListPage, ObjectListRequest,
    ObjectMetadata, ObjectStore, ObjectUpdateResult, PlatformFuture, SmallObjectMutator,
};

fn record() -> NovelRecord {
    let mut records: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(include_str!(
        "golden/library-a/.narou/database.yaml"
    )).unwrap();
    records.remove(&1).unwrap()
}

fn fixture() -> tempfile::TempDir {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new().prefix("novel-settings-").tempdir_in(base).unwrap()
}

// Force both legacy service reads to see the same pre-save snapshot. The
// atomic path goes straight into the real adapter's transaction instead.
struct ReadBarrierStore {
    inner: Arc<dyn ObjectStore>,
    gate: Arc<AtomicBool>,
    barrier: Arc<Barrier>,
}

impl ObjectStore for ReadBarrierStore {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        self.inner.stat(key)
    }
    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let value = self.inner.read_small(key).await?;
            if key.as_ref().ends_with("/setting.ini") && self.gate.load(Ordering::SeqCst) {
                self.barrier.wait();
            }
            Ok(value)
        })
    }
    fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        self.inner.write_small(key, data)
    }
    fn update_small<'a, 'data: 'a>(
        &'a self, key: &'a ObjectKey, mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        self.inner.update_small(key, mutate)
    }
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        self.inner.delete(key)
    }
    fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
        self.inner.list_page(request)
    }
}

fn concurrent_save(sqlite: bool) {
    let temp = fixture();
    let archive = temp.path().join("小説データ");
    let stores: [Arc<dyn ObjectStore>; 2] = if sqlite {
        let directory = temp.path().join(".narou");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("db.sqlite");
        [
            Arc::new(SqliteObjectStore::with_mirror_files(
                narou_rs::native::sqlite::open(&path).unwrap(),
                NativeObjectStore::from_root(archive.clone()).unwrap(), false,
            )),
            Arc::new(SqliteObjectStore::with_mirror_files(
                narou_rs::native::sqlite::open(&path).unwrap(),
                NativeObjectStore::from_root(archive.clone()).unwrap(), false,
            )),
        ]
    } else {
        [
            Arc::new(NativeObjectStore::from_root(archive.clone()).unwrap()),
            Arc::new(NativeObjectStore::from_root(archive.clone()).unwrap()),
        ]
    };
    let record = record();
    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).unwrap();
    futures::executor::block_on(stores[0].write_small(&keys.setting(),
        b"enable_yokogaki = false\nenable_convert_num_to_kanji = false\nunknown = \"123\"\n".to_vec(),
    )).unwrap();
    let gate = Arc::new(AtomicBool::new(true));
    let barrier = Arc::new(Barrier::new(2));
    let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
    let workers: Vec<_> = stores.iter().zip(["enable_yokogaki", "enable_convert_num_to_kanji"])
        .map(|(store, name)| {
            let objects = Arc::new(ReadBarrierStore {
                inner: Arc::clone(store), gate: Arc::clone(&gate), barrier: Arc::clone(&barrier),
            });
            let service = NovelSettingsService::new(repo.clone(), objects);
            std::thread::spawn(move || {
                let mut patch = NovelSettingsPatch::default();
                patch.settings.insert(name.into(), Some(IniValue::Boolean(true)));
                let result = futures::executor::block_on(service.save(NovelId(1), &patch)).unwrap();
                assert!(result.failure.is_none());
            })
        }).collect();
    for worker in workers { worker.join().unwrap(); }
    gate.store(false, Ordering::SeqCst);
    let bytes = futures::executor::block_on(stores[0].read_small(&keys.setting())).unwrap().unwrap();
    let restored = IniData::load(std::str::from_utf8(&bytes).unwrap());
    assert_eq!(restored.get_global("enable_yokogaki"), Some(&IniValue::Boolean(true)));
    assert_eq!(restored.get_global("enable_convert_num_to_kanji"), Some(&IniValue::Boolean(true)));
    assert_eq!(restored.get_global("unknown"), Some(&IniValue::String("123".into())));
}

#[test]
fn unrelated_fields_survive_concurrent_filesystem_saves() { concurrent_save(false); }

#[test]
fn unrelated_fields_survive_concurrent_sqlite_connection_saves() { concurrent_save(true); }

#[test]
fn committed_sqlite_settings_are_acknowledged_when_the_mirror_cannot_publish() {
    let temp = fixture();
    let directory = temp.path().join(".narou");
    std::fs::create_dir_all(&directory).unwrap();
    let connection = narou_rs::native::sqlite::open(&directory.join("db.sqlite")).unwrap();
    let archive = temp.path().join("小説データ");
    let mirror = NativeObjectStore::from_root(archive.clone()).unwrap();
    let record = record();
    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).unwrap();
    let seed = SqliteObjectStore::with_mirror_files(connection.clone(), mirror.clone(), false);
    futures::executor::block_on(seed.write_small(&keys.setting(), b"enable_yokogaki = false\n".to_vec())).unwrap();
    // A directory in place of the mirror file makes publication fail after
    // the authoritative object transaction commits, on every platform.
    let native_prefix = keys.prefix().as_ref().strip_prefix("novels/").unwrap();
    std::fs::create_dir_all(archive.join(native_prefix).join("setting.ini")).unwrap();
    let store = Arc::new(SqliteObjectStore::with_mirror_files(connection, mirror, true));
    let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
    let service = NovelSettingsService::new(repo, store);
    let mut patch = NovelSettingsPatch::default();
    patch.settings.insert("enable_yokogaki".into(), Some(IniValue::Boolean(true)));
    let result = futures::executor::block_on(service.save(NovelId(1), &patch)).unwrap();
    assert_eq!(result.settings.get("enable_yokogaki"), Some(&Some(IniValue::Boolean(true))));
    assert!(result.failure.is_some());
    assert_eq!(result.to_json_value()["partial"], true);
    let view = futures::executor::block_on(service.load(NovelId(1))).unwrap();
    assert_eq!(view.settings.iter().find(|entry| entry.name == "enable_yokogaki").unwrap().value,
        Some(IniValue::Boolean(true)));

    futures::executor::block_on(seed.write_small(&keys.replace(), b"old\tnew\n".to_vec())).unwrap();
    std::fs::create_dir_all(archive.join(native_prefix).join("replace.txt")).unwrap();
    let replace_only = NovelSettingsPatch {
        settings: Default::default(),
        replace_patterns: Some(Vec::new()),
    };
    let result = futures::executor::block_on(service.save(NovelId(1), &replace_only)).unwrap();
    assert!(result.settings.is_empty());
    assert_eq!(result.replace_patterns, Some(Vec::new()));
    assert!(result.failure.is_some());
    assert_eq!(result.to_json_value()["partial"], true);
    let view = futures::executor::block_on(service.load(NovelId(1))).unwrap();
    assert!(view.replace_patterns.is_empty());
}
