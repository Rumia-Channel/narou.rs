//! 新規ダウンロードの ID 採番。
//!
//! 404 や年齢制限への同意拒否で登録に至らなかった予約は、次の取得で再利用する。
//! 成功時は予約 ID と保存レコード ID を揃え、次の新規作品だけ採番を進める。
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

struct RestoreDirectory(std::path::PathBuf);

impl Drop for RestoreDirectory {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore the original test directory");
    }
}

const SITE_YAML: &str = r#"
name: TestSite
domain: example.com
top_url: https://\k<domain>
url: https://\k<domain>/novel/(?<ncode>n\d+[a-z]+)
toc_url: https://\k<domain>/novel/\k<ncode>/
encoding: UTF-8
over18_pattern: adult-only
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
    let _directory = RestoreDirectory(std::env::current_dir().unwrap());
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
    http.add_text("https://example.com/novel/n0000zz/",404,"not found");
    http.add_text("https://example.com/novel/n0001xx/",200,
        r#"<h1 class="title">年齢制限作品</h1><span class="author">作者</span>adult-only<a href="/novel/n0001xx/1/" class="subtitle">第1話</a>"#);
    for ncode in ["n1111aa", "n2222bb"] {
        http.add_text(
            format!("https://example.com/novel/{ncode}/"),
            200,
            format!(
                concat!(
                    r#"<h1 class="title">作品 {ncode}</h1><span class="author">作者</span>"#,
                    r#"<a href="/novel/{ncode}/1/" class="subtitle">第1話</a>"#
                ),
                ncode = ncode
            ),
        );
        http.add_text(
            format!("https://example.com/novel/{ncode}/1/"),
            200,
            r#"<div class="body">本文</div>"#,
        );
    }

    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store;
    let repository = Arc::new(MemoryNovelRepository::new());
    let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
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
        Arc::new(narou_rs::downloader::settings::SnapshotDownloaderSettings::new(
            Default::default(), Some(false), Default::default(),
        )),
    )
    .unwrap();

    let failed = downloader.download_novel("https://example.com/novel/n0000zz/").await;
    assert!(matches!(failed,Err(narou_rs::error::NarouError::NotFound(_))));
    assert_eq!(repository.count(&Default::default()).await.unwrap(),0);
    let canceled = downloader.download_novel("https://example.com/novel/n0001xx/").await.unwrap();
    assert_eq!(canceled.status,narou_rs::downloader::UpdateStatus::Canceled);
    assert_eq!(repository.count(&Default::default()).await.unwrap(),0);

    let first = downloader
        .download_novel("https://example.com/novel/n1111aa/")
        .await
        .unwrap();
    assert_eq!(first.status, narou_rs::downloader::UpdateStatus::Ok);
    assert_eq!(first.id,0,"a failed unregistered download must not consume an id");
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
    assert_eq!(first_record.title,"作品 n1111aa");
    assert_eq!(second_record.title,"作品 n2222bb");
    assert_eq!(
        second_record.id,
        first_record.id + 1,
        "the reserved id must be reused at commit time, so ids stay consecutive"
    );
}
