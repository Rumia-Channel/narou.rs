//! サイト定義による必須の作品属性。

use std::collections::HashMap;
use std::sync::Arc;

use narou_rs::downloader::settings::SnapshotDownloaderSettings;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{Downloader, DownloaderPlatform};
use narou_rs::platform::mocks::{
    FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore, MockHttpClient,
};
use narou_rs::platform::{AssetStore, NovelMutation, NovelRepository, ObjectStore, SystemClock};

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
name: Tagged
domain: tagged.example
top_url: https://\k<domain>
url: https://\k<domain>/novel/(?<ncode>[a-z]\d+)
toc_url: https://\k<domain>/novel/\k<ncode>/
encoding: UTF-8
sitename: Tagged
title: '<h1 class="title">(?<title>.+?)</h1>'
author: '<span class="author">(?<author>.+?)</span>'
subtitles: '<a href="(?<href>[^"]+)" class="subtitle">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
work_type_rules:
  - field: ncode
    match: '^[ac]'
    type: manga
version: 1.0
"#;

fn downloader_for(
    http: Arc<MockHttpClient>,
    novels: Arc<MemoryNovelRepository>,
    sites: Vec<SiteSetting>,
    auto_add_tags: bool,
) -> Downloader {
    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store;
    let settings = SnapshotDownloaderSettings::new(
        HashMap::from([("auto-add-tags".to_string(), auto_add_tags)]),
        Some(true),
        HashMap::new(),
    );
    Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform {
            http,
            rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels,
            objects,
            assets,
            clock: Arc::new(SystemClock),
        },
        sites,
        HashMap::new(),
        Arc::new(settings),
    )
    .unwrap()
}

fn sites() -> Vec<SiteSetting> {
    SiteSetting::load_bundled(&[SITE_YAML]).unwrap()
}

/// ncode ごとの作品ページと本文を用意する。
fn mock_site(http: &Arc<MockHttpClient>, ncodes: &[&str]) {
    for ncode in ncodes {
        http.add_text(
            format!("https://tagged.example/novel/{ncode}/"),
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
            format!("https://tagged.example/novel/{ncode}/1/"),
            200,
            r#"<div class="body">本文</div>"#,
        );
    }
}

#[tokio::test]
async fn configured_tags_are_applied_to_new_and_updated_records() {
    let _serial = PIPELINE.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _directory = RestoreDirectory(std::env::current_dir().unwrap());
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir(temp.path().join(".narou")).unwrap();
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(MockHttpClient::new());
    mock_site(&http, &["a111", "c222", "n333"]);
    let novels = Arc::new(MemoryNovelRepository::new());
    // 抽出したサイトタグは使わない (`auto-add-tags` 無効)。
    let mut downloader = downloader_for(http.clone(), novels.clone(), sites(), false);

    let manga = downloader
        .download_novel("https://tagged.example/novel/a111/")
        .await
        .unwrap();
    let comic = downloader
        .download_novel("https://tagged.example/novel/c222/")
        .await
        .unwrap();
    let novel = downloader
        .download_novel("https://tagged.example/novel/n333/")
        .await
        .unwrap();

    let manga_record = novels.get(manga.id.into()).await.unwrap().unwrap();
    assert_eq!(manga_record.tags, vec!["漫画"]);
    let comic_record = novels.get(comic.id.into()).await.unwrap().unwrap();
    assert_eq!(comic_record.tags, vec!["漫画"]);
    let novel_record = novels.get(novel.id.into()).await.unwrap().unwrap();
    assert_eq!(novel_record.tags, vec!["小説"]);
}

#[tokio::test]
async fn an_update_keeps_existing_tags_and_does_not_duplicate_them() {
    let _serial = PIPELINE.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _directory = RestoreDirectory(std::env::current_dir().unwrap());
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir(temp.path().join(".narou")).unwrap();
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(MockHttpClient::new());
    mock_site(&http, &["a111"]);
    let novels = Arc::new(MemoryNovelRepository::new());
    let mut downloader = downloader_for(http.clone(), novels.clone(), sites(), false);

    let first = downloader
        .download_novel("https://tagged.example/novel/a111/")
        .await
        .unwrap();
    let mut record = novels.get(first.id.into()).await.unwrap().unwrap();
    assert_eq!(record.tags, vec!["漫画"]);
    record.tags.push("手動".to_string());
    novels
        .apply_batch(vec![NovelMutation::Upsert(record)])
        .await
        .unwrap();

    let updated = downloader
        .download_novel("https://tagged.example/novel/a111/")
        .await
        .unwrap();
    assert_eq!(updated.id, first.id);
    let record = novels.get(first.id.into()).await.unwrap().unwrap();
    assert_eq!(
        record.tags,
        vec!["漫画", "手動"],
        "更新ではタグが重複せず、手動タグも残る"
    );
}

#[tokio::test]
async fn site_rules_can_classify_by_preprocessed_page_and_stable_url_on_update() {
    let _serial = PIPELINE.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _directory = RestoreDirectory(std::env::current_dir().unwrap());
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir(temp.path().join(".narou")).unwrap();
    narou_rs::db::init_database().unwrap();

    let yaml = SITE_YAML.replace("version: 1.0", r#"
  - field: source
    match: '^category::comic$'
    type: manga
  - field: url
    match: '/novel/n333/'
    type: novel
version: 1.0
preprocess: |-
  let kind = extract(/<meta name="kind" content="([^"]+)">/)
  if kind
    emit "\ncategory::${kind}\n"
    insert_at_match
  end
"#);
    let sites = SiteSetting::load_bundled(&[&yaml]).unwrap();
    let http = Arc::new(MockHttpClient::new());
    mock_site(&http, &["n333"]);
    let novels = Arc::new(MemoryNovelRepository::new());
    let mut downloader = downloader_for(http.clone(), novels.clone(), sites, false);

    let first = downloader.download_novel("https://tagged.example/novel/n333/").await.unwrap();
    let mut record = novels.get(first.id.into()).await.unwrap().unwrap();
    assert_eq!(record.tags, vec!["小説"]);
    record.tags.push("手動".to_string());
    novels.apply_batch(vec![NovelMutation::Upsert(record)]).await.unwrap();

    http.add_text(
        "https://tagged.example/novel/n333/",
        200,
        concat!(
            r#"<meta name="kind" content="comic">"#,
            r#"<h1 class="title">作品 n333</h1><span class="author">作者</span>"#,
            r#"<a href="/novel/n333/1/" class="subtitle">第1話</a>"#
        ),
    );
    let updated = downloader.download_novel(&first.id.to_string()).await.unwrap();
    assert_eq!(updated.id, first.id);
    let record = novels.get(first.id.into()).await.unwrap().unwrap();
    assert_eq!(record.tags, vec!["手動", "漫画"]);
}
