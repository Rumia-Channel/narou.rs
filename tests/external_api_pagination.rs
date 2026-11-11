//! The external API's unbounded list (`limit=all`) with a start offset must
//! return the remaining records.
//!
//! This case runs against the **real native repository** (the cwd-based
//! inventory), not the in-memory mock: the paging arithmetic that used to
//! overflow lives in the native query implementation, so a mock-only
//! regression would not have caught it.
//!
//! The library root is the process cwd, so this binary keeps its tests
//! serialized and points the cwd at a temp library before anything resolves
//! the root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use narou_rs::application::external_api::ExternalListRequest;
use narou_rs::db::NovelRecord;
use narou_rs::db::inventory::Inventory;
use narou_rs::native::application::NativeAppServices;
use narou_rs::native::novel_repository::NativeNovelRepository;
use narou_rs::platform::NovelMutation;

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

/// A scoped library root with an empty `.narou` directory.
fn temp_library() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    (temp, root)
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
        requires_login: false,
        login_session: None,
        extra_fields: BTreeMap::new(),
    }
}

/// `GET /api/v1/novels?limit=all&offset=1` returns the second and third
/// records of a three-record library.
#[test]
fn an_unbounded_external_list_starts_at_the_requested_offset() {
    let _lock = LIBRARY_LOCK.lock();
    let (_temp, root) = temp_library();
    let _cwd = CwdGuard::enter(&root);
    narou_rs::db::init_database().unwrap();

    let novels = NativeNovelRepository::new();
    novels
        .apply_batch_sync(vec![
            NovelMutation::Upsert(sample_record(1)),
            NovelMutation::Upsert(sample_record(2)),
            NovelMutation::Upsert(sample_record(3)),
        ])
        .unwrap();

    let inventory = Arc::new(Inventory::new(root.clone()));
    let services = NativeAppServices::new(inventory).unwrap();
    let api = &services.services.external_api;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // 無制限 (limit=all) かつ開始位置 1。
    let request = ExternalListRequest {
        limit: None,
        offset: 1,
        ..ExternalListRequest::default()
    };
    let list = runtime.block_on(api.list(&request, None)).unwrap();

    assert_eq!(list.records_total, 3);
    assert_eq!(list.records_filtered, 3);
    let ids: Vec<i64> = list.novels.iter().map(|novel| novel.id).collect();
    assert_eq!(ids, vec![2, 3]);

    // 開始位置が総数を超える場合も空で戻る (範囲外で落ちない)。
    let request = ExternalListRequest {
        limit: None,
        offset: 99,
        ..ExternalListRequest::default()
    };
    let list = runtime.block_on(api.list(&request, None)).unwrap();
    assert_eq!(list.records_filtered, 3);
    assert!(list.novels.is_empty());
}
