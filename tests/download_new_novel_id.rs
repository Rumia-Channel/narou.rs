//! 新規ダウンロードの ID 採番。
//!
//! 予約した ID をそのままレコードの ID に使う。以前はコミット時にもう一度
//! 採番していたため、DL開始行の ID と実際のレコード ID が食い違い
//! (`ID:1　タイトル のDL開始` → `タイトル のDL完了 (ID:2, …)`)、使われない
//! ID が 1 つ消費されていた (一覧の ID が飛ぶ)。
//!
//! Lives in its own binary because the download pipeline touches the
//! process-wide database and working directory.

use std::sync::Arc;

use narou_rs::downloader::Downloader;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::platform::mocks::{
    FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore, MockHttpClient,
};
use narou_rs::platform::{AssetStore, NovelRepository, ObjectStore};

/// The pipeline reads the process-wide working directory and database, so the
/// tests in this binary must not run at the same time.
static PIPELINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SITE_YAML: &str = r#"
name: TestSite
domain: example.com
top_url: https://\k<domain>
url: https://\k<domain>/novel/(?<ncode>n\d+[a-z]+)
toc_url: https://\k<domain>/novel/\k<ncode>/
encoding: UTF-8
sitename: TestSite
title: '<h1 class="title">(?<title>.+?)</h1>'
author: '<span class="author">(?<author>.+?)</span>'
subtitles: '<a href="(?<href>[^"]+)" class="subtitle">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
version: 1.0
"#;

#[tokio::test]
async fn a_new_download_consumes_exactly_one_id() {
    let _serial = PIPELINE.lock().await;
    let temp = tempfile::tempdir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
    let site_dir = temp.path().join("webnovel");
    std::fs::create_dir_all(&site_dir).unwrap();
    std::fs::write(site_dir.join("test.yaml"), SITE_YAML).unwrap();
    narou_rs::db::init_database().unwrap();

    let settings: Vec<SiteSetting> = SiteSetting::load_all()
        .unwrap()
        .into_iter()
        .filter(|setting| setting.domain == "example.com")
        .collect();
    assert_eq!(settings.len(), 1, "the test site definition must load");

    let http = Arc::new(MockHttpClient::new());
    for ncode in ["n1111aa", "n2222bb"] {
        http.add_text(
            &format!("https://example.com/novel/{ncode}/"),
            200,
            &format!(
                concat!(
                    r#"<h1 class="title">作品 {ncode}</h1><span class="author">作者</span>"#,
                    r#"<a href="/novel/{ncode}/1/" class="subtitle">第1話</a>"#
                ),
                ncode = ncode
            ),
        );
        http.add_text(
            &format!("https://example.com/novel/{ncode}/1/"),
            200,
            r#"<div class="body">本文</div>"#,
        );
    }

    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store;
    let repository = Arc::new(MemoryNovelRepository::new());
    let mut downloader = Downloader::with_platform_and_storage_and_settings(
        narou_rs::downloader::DownloaderPlatform {
            http,
            rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels: repository.clone(),
            objects,
            assets,
            clock: Arc::new(narou_rs::platform::SystemClock),
        },
        settings,
        Default::default(),
    )
    .unwrap();

    let first = downloader
        .download_novel("https://example.com/novel/n1111aa/")
        .await
        .unwrap();
    assert_eq!(first.status, narou_rs::downloader::UpdateStatus::Ok);
    let second = downloader
        .download_novel("https://example.com/novel/n2222bb/")
        .await
        .unwrap();
    assert_eq!(second.status, narou_rs::downloader::UpdateStatus::Ok);

    let first_record = repository
        .get(first.id.into())
        .await
        .unwrap()
        .expect("the first novel is saved under the id its DL開始 line printed");
    let second_record = repository
        .get(second.id.into())
        .await
        .unwrap()
        .expect("the second novel is saved under the id its DL開始 line printed");
    assert_eq!(first_record.id, first.id);
    assert_eq!(second_record.id, second.id);
    assert_eq!(
        second_record.id,
        first_record.id + 1,
        "the reserved id must be reused at commit time, so ids stay consecutive"
    );
}
