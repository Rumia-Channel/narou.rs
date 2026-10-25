#![cfg(feature = "native-runtime")]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use narou_rs::downloader::preprocess::PreprocessJobs;
use narou_rs::downloader::section::{SectionCache, download_section};
use narou_rs::downloader::settings::SnapshotDownloaderSettings;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::types::{SubtitleInfo, UpdateStatus};
use narou_rs::downloader::{Downloader, DownloaderPlatform};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::mocks::{FakeRateLimiter, MemoryCookieStore, MemoryNovelRepository, MemoryObjectStore};
use narou_rs::platform::{CookieStore, HostCookie, HttpClient, HttpRequest, HttpResponse, LoginGroup, NovelFilter, NovelRepository, ObjectListRequest, ObjectPrefix, ObjectStore, PlatformFuture, SystemClock};
use serde_json::json;

const WORK: &str = "2912051601247823656";
const TOP: &str = "https://kakuyomu.jp/works/2912051601247823656";
const EPISODES: [&str; 3] = ["99", "7", "5"];

fn site() -> SiteSetting {
    SiteSetting::load_bundled(&[include_str!("../webnovel/kakuyomu.jp.yaml")]).unwrap().remove(0)
}

fn work_html(next: bool, subscribed: bool, free_count: usize) -> String {
    let mut data = json!({
        format!("Work:{WORK}"): {
            "title": "作品", "author": {"__ref": "UserAccount:1"},
            "kakuyomuNextWork": if next { json!({"__ref": format!("KakuyomuNextWork:{WORK}")}) } else { serde_json::Value::Null },
            "introduction": "あらすじ", "serialStatus": "RUNNING", "tagLabels": [],
            "publicEpisodeCount": 3, "totalCharacterCount": 300,
            "publishedAt": "2026-06-12T03:30:28Z", "editedAt": "2026-10-17T03:30:18Z",
            "lastEpisodePublishedAt": "2026-10-17T03:30:18Z",
            "tableOfContentsV2": [{"__ref": "TableOfContentsChapter:"}]
        },
        "UserAccount:1": {"activityName": "作者"},
        "TableOfContentsChapter:": {"chapter": null, "episodeUnions": []}
    });
    let mut refs = Vec::new();
    for (index, id) in EPISODES.iter().enumerate() {
        let kind = if next && !subscribed && index >= free_count { "EmptyEpisode" } else { "Episode" };
        let key = format!("{kind}:{id}");
        data[&key] = json!({"__typename": kind, "id": id, "title": format!("第{}話", index + 1),
            "publishedAt": "2026-06-12T03:30:28Z"});
        refs.push(json!({"__ref": key}));
    }
    data["TableOfContentsChapter:"]["episodeUnions"] = json!(refs);
    let bootstrap = json!({"props": {"pageProps": {"__APOLLO_STATE__": data}}, "query": {"workId": WORK}});
    format!(r#"<script id="__NEXT_DATA__" type="application/json">{bootstrap}</script>"#)
}

struct ReaderFixture {
    next: bool,
    free_count: usize,
    subscription_active: AtomicBool,
    requests: Mutex<Vec<(String, Option<String>)>>,
}

impl ReaderFixture {
    fn new(next: bool) -> Self {
        Self { next, free_count: 1, subscription_active: AtomicBool::new(true), requests: Mutex::new(Vec::new()) }
    }
}

impl HttpClient for ReaderFixture {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let cookie = request.header("Cookie").map(str::to_string);
            let subscribed = cookie.as_deref() == Some("session=subscriber")
                && self.subscription_active.load(Ordering::SeqCst);
            self.requests.lock().unwrap().push((request.url.clone(), cookie));
            let body = if request.url == TOP {
                work_html(self.next, subscribed, self.free_count)
            } else if let Some(id) = request.url.strip_prefix(&format!("{TOP}/episodes/")) {
                if !EPISODES.contains(&id) {
                    return Err(NarouError::Http("unexpected episode".into()));
                }
                if self.next && !subscribed && id != EPISODES[0] {
                    // The anonymous NEXT reader redirects to the work page,
                    // which has a TOC but no episode body.
                    work_html(true, false, self.free_count)
                } else {
                    format!(r#"<div class="widget-episodeBody js-episode-body"><p>本文{id}</p></div>"#)
                }
            } else {
                return Err(NarouError::Http("unexpected request".into()));
            };
            Ok(HttpResponse { status: 200, headers: Vec::new(), body: body.into_bytes() })
        })
    }
}

async fn logins(values: &[&str]) -> Arc<MemoryCookieStore> {
    let cookies = Arc::new(MemoryCookieStore::new());
    let groups = values.iter().map(|value| LoginGroup::new("kakuyomu.jp", vec![HostCookie {
        host: "kakuyomu.jp".into(), cookie: format!("session={value}")
    }]).with_id(*value)).collect::<Vec<_>>();
    cookies.save_groups("kakuyomu.jp", &groups).await.unwrap();
    cookies
}

fn downloader(http: Arc<ReaderFixture>, novels: Arc<MemoryNovelRepository>, store: Arc<MemoryObjectStore>) -> Downloader {
    Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform { http, rate_limiter: Arc::new(FakeRateLimiter::new()), novels,
            objects: store.clone(), assets: store, clock: Arc::new(SystemClock) },
        vec![site()], HashMap::new(),
        Arc::new(SnapshotDownloaderSettings::new(HashMap::new(), Some(false), HashMap::new())),
    ).unwrap()
}

async fn snapshot(store: &MemoryObjectStore) -> BTreeMap<String, Vec<u8>> {
    let request = ObjectListRequest::new(ObjectPrefix::new("novels/").unwrap(), std::num::NonZeroUsize::new(100).unwrap());
    let page = ObjectStore::list_page(store, &request).await.unwrap();
    assert!(page.next_cursor.is_none());
    let mut objects = BTreeMap::new();
    for object in page.objects {
        let bytes = ObjectStore::read_small(store, &object.key).await.unwrap().unwrap();
        objects.insert(object.key.as_ref().to_string(), bytes);
    }
    objects
}

#[tokio::test]
async fn next_chooses_an_entitled_login_and_reuses_it_without_losing_paid_chapters() {
    let http = Arc::new(ReaderFixture::new(true));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone())
        .with_cookie_store(logins(&["expired", "member", "subscriber"]).await);
    let result = downloader.download_novel(TOP).await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    assert_eq!(result.total_count, 3, "restricted chapters must not silently disappear");
    assert_eq!(result.updated_count, 3);
    let record = novels.get(result.id.into()).await.unwrap().unwrap();
    assert!(record.requires_login);
    assert_eq!(record.login_session.as_deref(), Some("subscriber"));
    let anonymous_before = http.requests.lock().unwrap().iter().filter(|(_, cookie)| cookie.is_none()).count();
    let update = downloader.download_novel(TOP).await.unwrap();
    assert_eq!(update.status, UpdateStatus::None);
    assert_eq!(http.requests.lock().unwrap().iter().filter(|(_, cookie)| cookie.is_none()).count(), anonymous_before);

    let saved = snapshot(store.as_ref()).await;
    let section_count = saved.keys().filter(|key| key.contains("/本文/") && key.ends_with(".yaml")).count();
    assert_eq!(section_count, 3);
    http.subscription_active.store(false, Ordering::SeqCst);
    for force in [false, true] {
        let unavailable = downloader.download_novel_with_force(TOP, force).await.unwrap();
        // preserve_missing_sections: an expired entitlement keeps the paid
        // chapters and only the still-accessible free chapter is refreshed.
        assert!(matches!(unavailable.status, UpdateStatus::None | UpdateStatus::Ok));
        assert_eq!(unavailable.total_count, 3, "paid chapters must be retained");
        assert!(!unavailable.sections_deleted);
        let after = snapshot(store.as_ref()).await;
        let section_keys = |objects: &BTreeMap<String, Vec<u8>>| {
            objects.keys().filter(|key| key.contains("/本文/") && !key.contains("/cache/") && key.ends_with(".yaml")).cloned().collect::<Vec<_>>()
        };
        assert_eq!(section_keys(&after), section_keys(&saved));
        assert_eq!(novels.get(result.id.into()).await.unwrap().unwrap().general_all_no, Some(3));
    }
}

#[tokio::test]
async fn next_without_entitlement_downloads_only_the_accessible_free_episodes() {
    for values in [vec![], vec!["expired", "member"]] {
        let http = Arc::new(ReaderFixture::new(true));
        let novels = Arc::new(MemoryNovelRepository::new());
        let store = Arc::new(MemoryObjectStore::new());
        let mut downloader = downloader(http, novels.clone(), store.clone()).with_cookie_store(logins(&values).await);
        let result = downloader.download_novel(TOP).await.unwrap();
        assert_eq!(result.status, UpdateStatus::Ok);
        assert_eq!(result.total_count, 1);
        assert_eq!(result.updated_count, 1);
        let record = novels.get(result.id.into()).await.unwrap().unwrap();
        assert_eq!(record.general_all_no, Some(1));
        assert!(!record.requires_login);
        assert_eq!(record.login_session, None);
        let saved = snapshot(store.as_ref()).await;
        assert_eq!(saved.keys().filter(|key| key.contains("/本文/") && key.ends_with(".yaml")).count(), 1);
    }
}

#[tokio::test]
async fn adding_an_entitled_login_upgrades_a_saved_free_subset() {
    let http = Arc::new(ReaderFixture::new(true));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http, novels.clone(), store.clone());
    let free = downloader.download_novel(TOP).await.unwrap();
    assert_eq!(free.total_count, 1);
    let mut downloader = downloader.with_cookie_store(logins(&["subscriber"]).await);
    let full = downloader.download_novel(TOP).await.unwrap();
    assert_eq!(full.id, free.id);
    assert_eq!(full.total_count, 3);
    assert_eq!(full.updated_count, 2);
    let record = novels.get(full.id.into()).await.unwrap().unwrap();
    assert!(record.requires_login);
    assert_eq!(record.login_session.as_deref(), Some("subscriber"));
    assert_eq!(record.general_all_no, Some(3));
}

#[tokio::test]
async fn a_partial_listing_without_any_accessible_episode_does_not_create_an_empty_novel() {
    let http = Arc::new(ReaderFixture { free_count: 0, ..ReaderFixture::new(true) });
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http, novels.clone(), store.clone());
    let result = downloader.download_novel(TOP).await.unwrap();
    assert_eq!(result.status, UpdateStatus::Canceled);
    assert_eq!(result.total_count, 0);
    assert_eq!(novels.count(&NovelFilter::default()).await.unwrap(), 0);
    assert!(snapshot(store.as_ref()).await.is_empty());
}

#[tokio::test]
async fn public_kakuyomu_novels_stay_anonymous_with_saved_logins() {
    let http = Arc::new(ReaderFixture::new(false));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store)
        .with_cookie_store(logins(&["subscriber"]).await);
    let result = downloader.download_novel(TOP).await.unwrap();
    assert_eq!(result.total_count, 3);
    assert_eq!(result.updated_count, 3);
    let record = novels.get(result.id.into()).await.unwrap().unwrap();
    assert!(!record.requires_login);
    assert_eq!(record.login_session, None);
    assert!(http.requests.lock().unwrap().iter().all(|(_, cookie)| cookie.is_none()));
}

#[tokio::test]
async fn a_next_work_page_returned_instead_of_a_reader_is_not_cached_as_an_empty_body() {
    let setting = site();
    let subtitle = SubtitleInfo { index: "7".into(), href: format!("/works/{WORK}/episodes/7"),
        chapter: String::new(), subchapter: String::new(), subtitle: "制限された話".into(),
        file_subtitle: "制限された話".into(), subdate: String::new(), subupdate: None, download_time: None };
    let mut cache = SectionCache::new();
    let result = download_section(&ReaderFixture::new(true), &FakeRateLimiter::new(), &mut cache,
        &setting, &subtitle, TOP, &mut PreprocessJobs::new()).await;
    assert!(matches!(result, Err(NarouError::SiteSetting(_))));
    assert!(cache.get(&format!("{TOP}/episodes/7")).is_none());
}
