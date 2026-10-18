//! Series tags include every TOC page, even when an update downloads no bodies.

use std::collections::HashMap;
use std::sync::Arc;

use narou_rs::downloader::settings::SnapshotDownloaderSettings;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{Downloader, DownloaderPlatform};
use narou_rs::platform::mocks::{FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore, MockHttpClient};
use narou_rs::platform::{AssetStore, NovelMutation, NovelRepository, ObjectStore, SystemClock};
use serde_json::json;

const SERIES: &str = "https://www.pixiv.net/ajax/novel/series/854898";
const FIRST_PAGE: &str = "https://www.pixiv.net/ajax/novel/series_content/854898?limit=30&last_order=0&order_by=asc";
const LAST_PAGE: &str = "https://www.pixiv.net/ajax/novel/series_content/854898?limit=30&last_order=30&order_by=asc";
const DATE: &str = "2020-01-01T00:00:00+09:00";

fn page(first: usize, last: usize, added_tag: bool) -> String {
    let entries: Vec<_> = (first..=last).map(|order| {
        let mut tags = vec!["共通", if order <= 30 { "前半" } else { "後半" }];
        if added_tag { tags.push("追加"); }
        json!({
            "id": (20_000_000 + order).to_string(), "title": format!("第{order}話"),
            "seriesId": "854898", "seriesContentOrder": order,
            "createDate": DATE, "updateDate": DATE, "tags": tags,
        })
    }).collect();
    let contents: Vec<_> = (first..=last).map(|order| json!({
        "id": (20_000_000 + order).to_string(), "series": {"id": "854898", "viewableType": 0, "contentOrder": order},
    })).collect();
    json!({"body": {"thumbnails": {"novel": entries}, "page": {"seriesContents": contents}}}).to_string()
}

#[tokio::test]
async fn all_episode_tags_are_merged_once_and_refresh_without_body_changes() {
    let temp = tempfile::tempdir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir(temp.path().join(".narou")).unwrap();
    narou_rs::db::init_database().unwrap();
    let sites: Vec<_> = SiteSetting::load_all().unwrap().into_iter()
        .filter(|site| site.domain == "www.pixiv.net").collect();
    let http = Arc::new(MockHttpClient::new());
    http.add_text(SERIES, 200, json!({"body": {
        "id": "854898", "title": "シリーズ", "userName": "作者", "caption": "",
        "total": 31, "tags": ["シリーズ", "共通"], "firstNovelId": "20000001",
        "createDate": DATE, "updateDate": DATE, "publishedTotalCharacterCount": 62,
    }}).to_string());
    http.add_text(FIRST_PAGE, 200, page(1, 30, false));
    http.add_text(LAST_PAGE, 200, page(31, 31, false));
    for order in 1..=31 {
        let id = (20_000_000 + order).to_string();
        http.add_text(format!("https://www.pixiv.net/ajax/novel/{id}"), 200, json!({"body": {
            "id": id, "title": format!("第{order}話"), "userName": "作者", "description": "",
            "content": "本文", "characterCount": 2, "createDate": DATE, "uploadDate": DATE,
        }}).to_string());
    }
    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store;
    let novels = Arc::new(MemoryNovelRepository::new());
    let settings = SnapshotDownloaderSettings::new(
        HashMap::from([("auto-add-tags".to_string(), true)]), Some(true), HashMap::new(),
    );
    let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform { http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels: novels.clone(), objects, assets, clock: Arc::new(SystemClock) },
        sites, HashMap::new(), Arc::new(settings),
    ).unwrap();
    let first = downloader.download_novel(SERIES).await.unwrap();
    assert_eq!(first.total_count, 31);
    let mut record = novels.get(first.id.into()).await.unwrap().unwrap();
    assert_eq!(record.tags, vec!["シリーズ", "共通", "前半", "後半"]);
    record.tags.push("手動".to_string());
    novels.apply_batch(vec![NovelMutation::Upsert(record)]).await.unwrap();

    http.add_text(LAST_PAGE, 200, page(31, 31, true));
    let updated = downloader.download_novel(SERIES).await.unwrap();
    assert_eq!(updated.id, first.id);
    assert_eq!(updated.updated_count, 0, "tag refresh must not need changed section bodies");
    let record = novels.get(first.id.into()).await.unwrap().unwrap();
    assert_eq!(record.tags, vec!["シリーズ", "共通", "前半", "後半", "手動", "追加"]);
}
