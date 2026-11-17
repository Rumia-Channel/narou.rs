#![cfg(feature = "native-runtime")]

use std::collections::BTreeMap;
use std::sync::Arc;

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

// Hold the first save at the real atomic update boundary. The competing
// same-process save must wait, then preserve both edits and unknown fields.
struct PausedWriteStore {
    inner: Arc<dyn ObjectStore>,
    entered: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ObjectStore for PausedWriteStore {
    fn access_fence(&self) -> Option<&dyn narou_rs::platform::NovelAccessFence> {
        self.inner.access_fence()
    }
    fn read_stream_leased<'a>(&'a self, prefix: &'a narou_rs::platform::ObjectPrefix, lease: &'a narou_rs::platform::GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<narou_rs::platform::AssetStream>>> {
        self.inner.read_stream_leased(prefix, lease, key, range)
    }
    fn update_small_guarded<'a, 'data: 'a>(&'a self, fence: &'a narou_rs::platform::WriteFence, key: &'a ObjectKey, mutate: &'a mut SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move {
            let mut paused = |current: Option<&[u8]>| {
                self.entered.send(()).map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?;
                self.resume.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?;
                mutate(current)
            };
            self.inner.update_small_guarded(fence, key, &mut paused).await
        })
    }
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        self.inner.stat(key)
    }
    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        self.inner.read_small(key)
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
    let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
    let (entered, waiting) = std::sync::mpsc::sync_channel(1);
    let (resume, resumed) = std::sync::mpsc::sync_channel(1);
    let first = NovelSettingsService::new(repo.clone(), Arc::new(PausedWriteStore {
        inner: stores[0].clone(), entered, resume: std::sync::Mutex::new(resumed),
    }));
    let second = NovelSettingsService::new(repo, stores[1].clone());
    let mut first_patch = NovelSettingsPatch::default();
    first_patch.settings.insert("enable_yokogaki".into(), Some(IniValue::Boolean(true)));
    let mut second_patch = NovelSettingsPatch::default();
    second_patch.settings.insert("enable_convert_num_to_kanji".into(), Some(IniValue::Boolean(true)));
    let (saved, second_saved) = std::thread::scope(|scope| {
        let writer = scope.spawn(|| futures::executor::block_on(first.save(NovelId(1), &first_patch)));
        waiting.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        let second_saved = futures::executor::block_on(async {
            let pending = second.save(NovelId(1), &second_patch);
            futures::pin_mut!(pending);
            assert!(futures::poll!(pending.as_mut()).is_pending(), "a concurrent edit must wait rather than fail");
            resume.send(()).unwrap();
            pending.await
        });
        (writer.join().unwrap(), second_saved)
    });
    assert!(saved.unwrap().failure.is_none());
    assert!(second_saved.unwrap().failure.is_none());
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
    let mut scope = service.read_scope("committed-settings-view");
    let view = futures::executor::block_on(service.load(&mut scope, NovelId(1))).unwrap();
    futures::executor::block_on(scope.release_all()).unwrap();
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
    let mut scope = service.read_scope("committed-replace-view");
    let view = futures::executor::block_on(service.load(&mut scope, NovelId(1))).unwrap();
    futures::executor::block_on(scope.release_all()).unwrap();
    assert!(view.replace_patterns.is_empty());
}
