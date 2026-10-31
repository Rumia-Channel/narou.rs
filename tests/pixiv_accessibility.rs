#![cfg(feature = "native-runtime")]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use narou_rs::downloader::settings::SnapshotDownloaderSettings;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::types::{SectionFile, TocFile, UpdateStatus};
use narou_rs::downloader::{Downloader, DownloaderPlatform};
use narou_rs::error::{NarouError, Result};
use narou_rs::illustration_store::IllustrationIndex;
use narou_rs::platform::mocks::{
    FakeRateLimiter, MemoryCookieStore, MemoryNovelRepository, MemoryObjectStore,
};
use narou_rs::platform::{
    CookieStore, HostCookie, HttpClient, HttpMethod, HttpRequest, HttpResponse, LoginGroup,
    NovelFilter, NovelRepository, ObjectListRequest, ObjectPrefix, ObjectStore, PlatformFuture,
    SystemClock,
};
use serde_json::{Value, json};

const COMIC: &str = "https://www.pixiv.net/user/7/series/91";
const PUBLIC_COMIC: &str = "https://www.pixiv.net/user/7/series/92";
const COMIC_TOC: &str = "https://www.pixiv.net/ajax/series/91?p=1&lang=ja";
const PUBLIC_COMIC_TOC: &str = "https://www.pixiv.net/ajax/series/92?p=1&lang=ja";
const ILLUST: &str = "https://www.pixiv.net/ajax/illust/";
const NOVEL: &str = "https://www.pixiv.net/ajax/novel/";
const IMAGE: &str = "https://i.pximg.net/img/";
const DATE: &str = "2026-10-17T11:12:00+09:00";
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6mP0AAAAASUVORK5CYII=";

type SavedObjects = BTreeMap<String, Vec<u8>>;

#[derive(Clone, Copy, Debug)]
enum PrivateReply {
    ApplicationDenial,
    MaskedMetadata,
    MaskedThumbnail,
    Omitted,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    MetadataForbidden,
    MetadataMalformed,
    PagesForbidden,
}

#[derive(Clone)]
struct Visibility {
    total: usize,
    private_orders: Vec<usize>,
    member_has_access: bool,
    private_reply: PrivateReply,
    fault: Option<Fault>,
}

impl Default for Visibility {
    fn default() -> Self {
        Self {
            total: 3,
            private_orders: Vec::new(),
            member_has_access: false,
            private_reply: PrivateReply::ApplicationDenial,
            fault: None,
        }
    }
}

#[derive(Clone, Debug)]
struct SeenRequest {
    url: String,
    cookie: Option<String>,
    referer: Option<String>,
}

struct ReaderFixture {
    visibility: Mutex<Visibility>,
    requests: Mutex<Vec<SeenRequest>>,
}

impl ReaderFixture {
    fn new(visibility: Visibility) -> Self {
        Self { visibility: Mutex::new(visibility), requests: Mutex::new(Vec::new()) }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }

    fn calls(&self, url: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|request| request.url == url).count()
    }

    fn calls_with_cookie(&self, url: &str, cookie: Option<&str>) -> usize {
        self.requests.lock().unwrap().iter()
            .filter(|request| request.url == url && request.cookie.as_deref() == cookie).count()
    }
}

fn response(status: u16, body: impl Into<Vec<u8>>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())],
        body: body.into(),
    }
}

fn json_response(body: Value) -> HttpResponse {
    response(200, body.to_string().into_bytes())
}

fn metadata_url(id: usize) -> String {
    format!("{ILLUST}{id}")
}

fn pages_url(id: usize) -> String {
    format!("{ILLUST}{id}/pages")
}

fn image_url(id: usize) -> String {
    format!("{IMAGE}{id}_p0.png")
}

fn artwork(id: usize, masked: bool) -> Value {
    json!({"error": false, "message": "", "body": {
        "id": id.to_string(), "title": format!("漫画{}", id - 100),
        "illustTitle": format!("漫画{}", id - 100), "userName": "fixture作者",
        "description": format!("キャプション{id}"), "illustType": 0, "pageCount": 1,
        "createDate": DATE, "uploadDate": DATE, "restrict": if masked { 2 } else { 0 },
        "xRestrict": 0, "seriesNavData": {"seriesId": "91"},
        "urls": {"original": if masked { Value::Null } else { json!(image_url(id)) }},
        "tags": {"tags": [{"tag": "共通タグ"}, {"tag": format!("タグ{}", id - 100)}]},
    }})
}

fn comic_toc(series_id: usize, total: usize) -> Value {
    let entries: Vec<_> = (1..=total).rev().map(|order| json!({
        "workId": if series_id == 92 { 104 } else { 100 + order }.to_string(), "order": order,
    })).collect();
    json!({"error": false, "message": "", "body": {
        "illustSeries": [{"id": series_id.to_string(), "title": format!("漫画シリーズ{series_id}"),
            "description": "シリーズのあらすじ", "total": total,
            "createDate": DATE, "updateDate": DATE}],
        "page": {"seriesId": series_id.to_string(), "total": total, "series": entries},
    }})
}

fn novel_series_url(series_id: usize) -> String {
    format!("{NOVEL}series/{series_id}")
}

fn novel_page_url(series_id: usize, last_order: usize) -> String {
    format!("{NOVEL}series_content/{series_id}?limit=30&last_order={last_order}&order_by=asc")
}

fn novel_id(series_id: usize, order: usize) -> usize {
    (series_id - 91) * 100 + order
}

fn novel_series(series_id: usize) -> Value {
    let total = match series_id { 93 => 4, 94 => 31, 95 => 30, _ => unreachable!() };
    json!({"error": false, "message": "", "body": {
        "id": series_id.to_string(), "title": format!("小説シリーズ{series_id}"),
        "userName": "fixture作者", "caption": "小説のあらすじ", "total": total,
        "isConcluded": false, "createDate": DATE, "updateDate": DATE,
        "publishedTotalCharacterCount": total * 5, "tags": [],
    }})
}

fn novel_page(series_id: usize, last_order: usize) -> Result<Value> {
    let orders: Vec<usize> = match (series_id, last_order) {
        (93, 0) => (1..=4).collect(),
        (94 | 95, 0) => (1..=30).collect(),
        (94, 30) => vec![31],
        (95, 30) => Vec::new(),
        _ => return Err(NarouError::Http("unexpected novel TOC page".into())),
    };
    let contents: Vec<_> = orders.iter().map(|&order| json!({
        "id": novel_id(series_id, order).to_string(),
        "series": {"id": series_id, "contentOrder": order,
            "viewableType": if (series_id == 93 && order == 3) || (series_id == 94 && order == 30) { 2 } else { 0 }},
    })).collect();
    let entries: Vec<_> = orders.iter().copied()
        .filter(|&order| !(series_id == 94 && order == 30))
        .map(|order| json!({
            "id": novel_id(series_id, order).to_string(), "title": format!("小説第{order}話"),
            "seriesId": series_id.to_string(), "seriesContentOrder": order,
            "createDate": DATE, "updateDate": DATE, "tags": [],
            "isMasked": series_id == 93 && order == 2,
        })).collect();
    Ok(json!({"error": false, "message": "", "body": {
        "page": {"seriesContents": contents}, "thumbnails": {"novel": entries},
    }}))
}

impl HttpClient for ReaderFixture {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let cookie = request.header("Cookie").map(str::to_string);
            self.requests.lock().unwrap().push(SeenRequest {
                url: request.url.clone(), cookie: cookie.clone(),
                referer: request.header("Referer").map(str::to_string),
            });
            if request.method != HttpMethod::Get {
                return Err(NarouError::Http("fixture only accepts GET".into()));
            }
            let visibility = self.visibility.lock().unwrap().clone();
            let entitled = visibility.member_has_access && cookie.as_deref().is_some_and(|cookie| {
                cookie.split(';').any(|part| part.trim() == "session=member")
            });
            if request.url == COMIC_TOC {
                let mut toc = comic_toc(91, visibility.total);
                if matches!(visibility.private_reply, PrivateReply::MaskedThumbnail) {
                    toc["body"]["thumbnails"] = json!({"illust": (1..=visibility.total).map(|order| json!({
                        "id": (100 + order).to_string(),
                        "isMasked": visibility.private_orders.contains(&order) && !entitled,
                    })).collect::<Vec<_>>()});
                }
                if matches!(visibility.private_reply, PrivateReply::Omitted) && !entitled {
                    let entries = toc["body"]["page"]["series"].as_array_mut().unwrap();
                    entries.retain(|entry| !visibility.private_orders.contains(&(entry["order"].as_u64().unwrap() as usize)));
                    let total = entries.len();
                    toc["body"]["page"]["total"] = json!(total);
                }
                return Ok(json_response(toc));
            }
            if request.url == PUBLIC_COMIC_TOC {
                return Ok(json_response(comic_toc(92, 1)));
            }
            if let Some(suffix) = request.url.strip_prefix(ILLUST) {
                let is_pages = suffix.ends_with("/pages");
                let id: usize = suffix.strip_suffix("/pages").unwrap_or(suffix).parse()
                    .map_err(|_| NarouError::Http("unexpected artwork URL".into()))?;
                if !(101..=104).contains(&id) {
                    return Err(NarouError::Http("unexpected artwork id".into()));
                }
                let private = visibility.private_orders.contains(&(id - 100)) && !entitled;
                if is_pages {
                    if private || (id == 102 && visibility.fault == Some(Fault::PagesForbidden)) {
                        return Ok(response(403, b"forbidden pages".to_vec()));
                    }
                    return Ok(json_response(json!({"error": false, "message": "", "body": [
                        {"urls": {"original": image_url(id)}, "width": 1, "height": 1},
                    ]})));
                }
                if id == 102 {
                    match visibility.fault {
                        Some(Fault::MetadataForbidden) => return Ok(response(403, b"forbidden metadata".to_vec())),
                        Some(Fault::MetadataMalformed) => return Ok(response(200, b"{not valid JSON".to_vec())),
                        _ => {}
                    }
                }
                if private {
                    return Ok(match visibility.private_reply {
                        PrivateReply::ApplicationDenial => json_response(json!({
                            "error": true, "message": "この作品はマイピクにのみ公開されています", "body": [],
                        })),
                        PrivateReply::MaskedMetadata => json_response(artwork(id, true)),
                        PrivateReply::NotFound => response(404, b"not found".to_vec()),
                        PrivateReply::MaskedThumbnail | PrivateReply::Omitted => response(403, b"private metadata must not be requested".to_vec()),
                    });
                }
                let mut work = artwork(id, false);
                if visibility.private_orders.contains(&(id - 100)) {
                    work["body"]["restrict"] = json!(2);
                }
                return Ok(json_response(work));
            }
            if let Some(suffix) = request.url.strip_prefix(IMAGE) {
                suffix.strip_suffix("_p0.png").and_then(|id| id.parse::<usize>().ok())
                    .filter(|id| (101..=104).contains(id))
                    .ok_or_else(|| NarouError::Http("unexpected image URL".into()))?;
                return Ok(HttpResponse {
                    status: 200, headers: vec![("Content-Type".into(), "image/png".into())],
                    body: base64::Engine::decode(&base64::engine::general_purpose::STANDARD, PNG).unwrap(),
                });
            }
            for series_id in [93, 94, 95] {
                if request.url == novel_series_url(series_id) {
                    return Ok(json_response(novel_series(series_id)));
                }
                for last_order in [0, 30] {
                    if request.url == novel_page_url(series_id, last_order) {
                        return Ok(json_response(novel_page(series_id, last_order)?));
                    }
                }
            }
            if let Some(id) = request.url.strip_prefix(NOVEL).and_then(|id| id.parse::<usize>().ok()) {
                if [202, 203, 330].contains(&id) {
                    return Err(NarouError::Http("private novel body must not be fetched".into()));
                }
                let series_id = match id {
                    201..=204 => 93, 301..=331 => 94, 401..=430 => 95,
                    _ => return Err(NarouError::Http("unexpected novel id".into())),
                };
                let order = id % 100;
                return Ok(json_response(json!({"error": false, "message": "", "body": {
                    "id": id.to_string(), "title": format!("小説第{order}話"), "userName": "fixture作者",
                    "description": format!("前書き{id}"), "content": format!("本文{id}"),
                    "characterCount": 5, "createDate": DATE, "uploadDate": DATE,
                    "seriesNavData": {"seriesId": series_id.to_string()}, "tags": {"tags": []},
                }})));
            }
            Err(NarouError::Http(format!("unexpected fixture request: {}", request.url)))
        })
    }
}

async fn logins() -> Arc<MemoryCookieStore> {
    let cookies = Arc::new(MemoryCookieStore::new());
    let group = LoginGroup::new("www.pixiv.net", vec![HostCookie {
        host: "www.pixiv.net".into(), cookie: "session=member".into(),
    }]).with_id("member");
    cookies.save_groups("www.pixiv.net", &[group]).await.unwrap();
    cookies
}

fn downloader(http: Arc<ReaderFixture>, novels: Arc<MemoryNovelRepository>, store: Arc<MemoryObjectStore>) -> Downloader {
    downloader_with_settings(http, novels, store, HashMap::new())
}

fn downloader_with_settings(
    http: Arc<ReaderFixture>,
    novels: Arc<MemoryNovelRepository>,
    store: Arc<MemoryObjectStore>,
    local_settings: HashMap<String, bool>,
) -> Downloader {
    let sites = SiteSetting::load_bundled(&[include_str!("../webnovel/www.pixiv.net.yaml")]).unwrap();
    Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform {
            http, rate_limiter: Arc::new(FakeRateLimiter::new()), novels,
            objects: store.clone(), assets: store, clock: Arc::new(SystemClock),
        },
        sites, HashMap::new(),
        Arc::new(SnapshotDownloaderSettings::new(local_settings, Some(false), HashMap::new())),
    ).unwrap()
}

async fn snapshot(store: &MemoryObjectStore) -> SavedObjects {
    let request = ObjectListRequest::new(
        ObjectPrefix::new("novels/").unwrap(), std::num::NonZeroUsize::new(1000).unwrap(),
    );
    let page = ObjectStore::list_page(store, &request).await.unwrap();
    assert!(page.next_cursor.is_none());
    let mut saved = BTreeMap::new();
    for object in page.objects {
        let bytes = ObjectStore::read_small(store, &object.key).await.unwrap().unwrap();
        saved.insert(object.key.as_ref().to_string(), bytes);
    }
    saved
}

fn saved_toc(saved: &SavedObjects) -> TocFile {
    let tocs: Vec<_> = saved.iter().filter(|(key, _)| key.ends_with("/toc.yaml")).collect();
    assert_eq!(tocs.len(), 1, "exactly one saved consumer TOC");
    serde_yaml::from_slice(tocs[0].1).unwrap()
}

fn saved_sections(saved: &SavedObjects) -> BTreeMap<String, SectionFile> {
    saved.iter().filter_map(|(key, bytes)| {
        let (_, name) = key.rsplit_once("/本文/")?;
        if name.contains('/') || !name.ends_with(".yaml") {
            return None;
        }
        let section: SectionFile = serde_yaml::from_slice(bytes).unwrap();
        Some((section.index.clone(), section))
    }).collect()
}

fn section_bytes<'a>(saved: &'a SavedObjects, index: &str) -> (&'a str, &'a [u8]) {
    let prefix = format!("{index} ");
    saved.iter().find_map(|(key, bytes)| {
        let (_, name) = key.rsplit_once("/本文/")?;
        (name.starts_with(&prefix) && !name.contains('/') && name.ends_with(".yaml"))
            .then_some((key.as_str(), bytes.as_slice()))
    }).expect("saved canonical chapter")
}

fn assert_saved_comic(saved: &SavedObjects, series_id: usize, chapters: &[(usize, usize)]) {
    let toc = saved_toc(saved);
    assert_eq!(toc.title, format!("漫画シリーズ{series_id}"));
    assert_eq!(toc.author, "fixture作者");
    assert_eq!(toc.story.as_deref(), Some("シリーズのあらすじ"));
    let indices: Vec<_> = chapters.iter().map(|(order, _)| order.to_string()).collect();
    assert_eq!(toc.subtitles.iter().map(|row| row.index.clone()).collect::<Vec<_>>(), indices);
    let sections = saved_sections(saved);
    assert_eq!(sections.len(), chapters.len());
    let (cache_key, cache_bytes) = saved.iter().find(|(key, _)| key.ends_with("/.illustration_cache.yaml"))
        .expect("saved illustration index");
    let index: IllustrationIndex = serde_yaml::from_slice(cache_bytes).unwrap();
    assert_eq!(index.source_count(), chapters.len());
    let prefix = cache_key.strip_suffix("/.illustration_cache.yaml").unwrap();
    let png = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, PNG).unwrap();
    for (order, id) in chapters {
        let row = toc.subtitles.iter().find(|row| row.index == order.to_string()).unwrap();
        assert_eq!(row.href, metadata_url(*id));
        assert_eq!(row.subtitle, format!("漫画{}", id - 100));
        let section = &sections[&order.to_string()];
        assert_eq!(section.href, row.href);
        assert_eq!(section.subtitle, row.subtitle);
        assert_eq!(section.element.body.trim(), format!("<img src=\"{}\">", image_url(*id)));
        assert_eq!(section.element.introduction.trim(), format!("キャプション{id}"));
        let filename = index.filename_for_source(&image_url(*id)).expect("chapter illustration mapping");
        assert_eq!(saved.get(&format!("{prefix}/挿絵/{filename}")), Some(&png));
    }
    for id in 101..=104 {
        if !chapters.iter().any(|(_, chapter_id)| *chapter_id == id) {
            assert_eq!(index.filename_for_source(&image_url(id)), None);
        }
    }
}

fn assert_saved_novels(saved: &SavedObjects, series_id: usize, orders: &[usize]) {
    let toc = saved_toc(saved);
    assert_eq!(toc.title, format!("小説シリーズ{series_id}"));
    assert_eq!(toc.author, "fixture作者");
    assert_eq!(toc.story.as_deref(), Some("小説のあらすじ"));
    assert_eq!(toc.subtitles.iter().map(|row| row.index.clone()).collect::<Vec<_>>(),
        orders.iter().map(usize::to_string).collect::<Vec<_>>());
    let sections = saved_sections(saved);
    assert_eq!(sections.len(), orders.len());
    for order in orders {
        let id = novel_id(series_id, *order);
        let row = toc.subtitles.iter().find(|row| row.index == order.to_string()).unwrap();
        assert_eq!(row.href, format!("{NOVEL}{id}"));
        assert_eq!(row.subtitle, format!("小説第{order}話"));
        let section = &sections[&order.to_string()];
        assert_eq!(section.href, row.href);
        assert_eq!(section.element.body.trim(), format!("本文{id}"));
    }
}

fn assert_no_comic_bodies_requested(http: &ReaderFixture) {
    assert!(http.requests().iter().all(|request| {
        !request.url.ends_with("/pages") && !request.url.starts_with(IMAGE)
    }), "inaccessible chapters must not reach pages or image requests");
}

#[tokio::test]
async fn mixed_private_comic_metadata_saves_only_readable_chapters() {
    for private_reply in [PrivateReply::ApplicationDenial, PrivateReply::MaskedMetadata, PrivateReply::NotFound] {
        let http = Arc::new(ReaderFixture::new(Visibility {
            private_orders: vec![2], private_reply, ..Default::default()
        }));
        let novels = Arc::new(MemoryNovelRepository::new());
        let store = Arc::new(MemoryObjectStore::new());
        let mut downloader = downloader(http.clone(), novels.clone(), store.clone());
        let result = downloader.download_novel(COMIC).await.unwrap();
        assert_eq!(result.status, UpdateStatus::Ok, "{private_reply:?}");
        assert_eq!(result.total_count, 2);
        assert_eq!(result.updated_count, 2);
        let record = novels.get(result.id.into()).await.unwrap().unwrap();
        assert_eq!(record.general_all_no, Some(2));
        assert!(!record.requires_login);
        assert_eq!(record.login_session, None);
        assert_saved_comic(&snapshot(store.as_ref()).await, 91, &[(1, 101), (3, 103)]);
        for id in 101..=103 {
            assert_eq!(http.calls(&metadata_url(id)), 1, "TOC metadata must be reused for chapter {id}");
            let expected = usize::from(id != 102);
            assert_eq!(http.calls(&pages_url(id)), expected);
            assert_eq!(http.calls(&image_url(id)), expected);
        }
        assert!(http.requests().iter().all(|request| request.cookie.is_none()));
        assert!(http.requests().iter().filter(|request| request.url.starts_with(IMAGE))
            .all(|request| request.referer.as_deref().is_some_and(|value| value.starts_with("https://www.pixiv.net/"))));
    }
}

#[tokio::test]
async fn entitled_login_recovers_private_metadata_and_persists_the_winning_session() {
    let http = Arc::new(ReaderFixture::new(Visibility {
        private_orders: vec![2], member_has_access: true, ..Default::default()
    }));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone()).with_cookie_store(logins().await);
    let result = downloader.download_novel(COMIC).await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    assert_eq!(result.total_count, 3);
    assert_eq!(result.updated_count, 3);
    let record = novels.get(result.id.into()).await.unwrap().unwrap();
    assert_eq!(record.general_all_no, Some(3));
    assert!(record.requires_login);
    assert_eq!(record.login_session.as_deref(), Some("member"));
    assert_saved_comic(&snapshot(store.as_ref()).await, 91, &[(1, 101), (2, 102), (3, 103)]);
    for id in 101..=103 {
        assert_eq!(http.calls_with_cookie(&metadata_url(id), None), 1);
        assert_eq!(http.calls_with_cookie(&metadata_url(id), Some("session=member")), 1,
            "the winning account's JSON must not be replaced by the anonymous cache");
        assert_eq!(http.calls(&pages_url(id)), 1);
        assert_eq!(http.calls_with_cookie(&pages_url(id), Some("session=member")), 1);
        assert_eq!(http.calls(&image_url(id)), 1);
    }
    http.clear_requests();
    let update = downloader.download_novel(COMIC).await.unwrap();
    assert_eq!(update.id, result.id);
    assert_eq!(update.status, UpdateStatus::None);
    assert_eq!(update.total_count, 3);
    assert_eq!(update.updated_count, 0);
    assert_eq!(http.calls_with_cookie(COMIC_TOC, None), 0);
    assert_eq!(http.calls_with_cookie(COMIC_TOC, Some("session=member")), 1);
    assert!(http.requests().iter().filter(|request| request.url.starts_with(ILLUST))
        .all(|request| request.cookie.as_deref() == Some("session=member")));
    assert_saved_comic(&snapshot(store.as_ref()).await, 91, &[(1, 101), (2, 102), (3, 103)]);
}

#[tokio::test]
async fn stored_nonmember_is_tried_without_discarding_the_public_subset() {
    let http = Arc::new(ReaderFixture::new(Visibility { private_orders: vec![2], ..Default::default() }));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone()).with_cookie_store(logins().await);
    let result = downloader.download_novel(COMIC).await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    assert_eq!(result.total_count, 2);
    let record = novels.get(result.id.into()).await.unwrap().unwrap();
    assert_eq!(record.general_all_no, Some(2));
    assert!(!record.requires_login);
    assert_eq!(record.login_session, None);
    assert!(http.calls_with_cookie(&metadata_url(102), Some("session=member")) > 0);
    for id in [101, 103] {
        assert_eq!(http.calls(&pages_url(id)), 1);
        assert_eq!(http.calls_with_cookie(&pages_url(id), None), 1,
            "an unimproved account attempt must not replace the anonymous winner");
    }
    assert_eq!(http.calls(&pages_url(102)), 0);
    assert_eq!(http.calls(&image_url(102)), 0);
    assert_saved_comic(&snapshot(store.as_ref()).await, 91, &[(1, 101), (3, 103)]);
}

#[tokio::test]
async fn all_private_after_login_fallback_cancels_without_objects_or_an_id_gap() {
    let http = Arc::new(ReaderFixture::new(Visibility {
        private_orders: vec![1, 2, 3], ..Default::default()
    }));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone()).with_cookie_store(logins().await);
    let canceled = downloader.download_novel(COMIC).await.unwrap();
    assert_eq!(canceled.status, UpdateStatus::Canceled);
    assert_eq!(canceled.total_count, 0);
    assert_eq!(canceled.updated_count, 0);
    assert!(!canceled.sections_deleted);
    assert_eq!(novels.count(&NovelFilter::default()).await.unwrap(), 0);
    assert!(snapshot(store.as_ref()).await.is_empty(), "no empty TOC or body is committed");
    assert!(http.calls_with_cookie(&metadata_url(102), None) > 0);
    assert!(http.calls_with_cookie(&metadata_url(102), Some("session=member")) > 0);
    assert_no_comic_bodies_requested(http.as_ref());

    http.clear_requests();
    let public = downloader.download_novel(PUBLIC_COMIC).await.unwrap();
    assert_eq!(public.status, UpdateStatus::Ok);
    assert_eq!(public.id, 0, "an inaccessible unregistered series must not consume ID 0");
    assert_eq!(public.id, canceled.id);
    assert_eq!(novels.count(&NovelFilter::default()).await.unwrap(), 1);
    let record = novels.get(public.id.into()).await.unwrap().unwrap();
    assert!(!record.requires_login);
    assert_eq!(record.login_session, None);
    assert!(http.requests().iter().all(|request| request.cookie.is_none()),
        "saved logins must not make an unrelated public series authenticated");
    assert_saved_comic(&snapshot(store.as_ref()).await, 92, &[(1, 104)]);
}

#[tokio::test]
async fn cached_private_chapter_survives_new_readable_content_even_under_force() {
    for force in [false, true] {
        let http = Arc::new(ReaderFixture::new(Visibility::default()));
        let novels = Arc::new(MemoryNovelRepository::new());
        let store = Arc::new(MemoryObjectStore::new());
        let mut downloader = downloader(http.clone(), novels.clone(), store.clone());
        let first = downloader.download_novel(COMIC).await.unwrap();
        let before = snapshot(store.as_ref()).await;
        assert_saved_comic(&before, 91, &[(1, 101), (2, 102), (3, 103)]);
        let (private_key, private_bytes) = section_bytes(&before, "2");
        let old_toc = saved_toc(&before);
        *http.visibility.lock().unwrap() = Visibility {
            total: 4, private_orders: vec![2], private_reply: PrivateReply::MaskedMetadata,
            ..Default::default()
        };
        http.clear_requests();
        let result = downloader.download_novel_with_force(COMIC, force).await.unwrap();
        assert_eq!(result.status, UpdateStatus::Ok);
        assert_eq!(result.id, first.id);
        assert_eq!(result.total_count, 4);
        assert_eq!(result.updated_count, if force { 3 } else { 1 });
        assert!(!result.sections_deleted);
        assert!(result.new_arrival_subtitles.iter().any(|row| row.index == "4"));
        let after = snapshot(store.as_ref()).await;
        assert_saved_comic(&after, 91, &[(1, 101), (2, 102), (3, 103), (4, 104)]);
        assert_eq!(after.get(private_key).map(Vec::as_slice), Some(private_bytes),
            "force={force}: private cached chapter bytes must be untouched");
        let new_toc = saved_toc(&after);
        assert_eq!(new_toc.subtitles[1].download_time, old_toc.subtitles[1].download_time);
        assert_eq!(novels.get(first.id.into()).await.unwrap().unwrap().general_all_no, Some(4));
        assert_eq!(http.calls(&metadata_url(102)), 1);
        assert_eq!(http.calls(&pages_url(102)), 0);
        assert_eq!(http.calls(&image_url(102)), 0);
        assert_eq!(http.calls(&pages_url(104)), 1);
        assert_eq!(http.calls(&image_url(104)), 1);
    }
}

#[tokio::test]
async fn all_saved_chapters_becoming_private_preserves_the_record_toc_and_every_byte() {
    let http = Arc::new(ReaderFixture::new(Visibility::default()));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone()).with_cookie_store(logins().await);
    let first = downloader.download_novel(COMIC).await.unwrap();
    let before = snapshot(store.as_ref()).await;
    let record = serde_json::to_value(novels.get(first.id.into()).await.unwrap().unwrap()).unwrap();
    *http.visibility.lock().unwrap() = Visibility { private_orders: vec![1, 2, 3], ..Default::default() };
    for force in [false, true] {
        http.clear_requests();
        let result = downloader.download_novel_with_force(COMIC, force).await.unwrap();
        assert_eq!(result.status, UpdateStatus::Canceled);
        assert_eq!(result.id, first.id);
        assert_eq!(result.total_count, 3);
        assert_eq!(result.updated_count, 0);
        assert!(!result.sections_deleted);
        assert_eq!(snapshot(store.as_ref()).await, before, "force={force}: no partial replacement");
        assert_eq!(serde_json::to_value(novels.get(first.id.into()).await.unwrap().unwrap()).unwrap(), record);
        assert!(http.calls_with_cookie(&metadata_url(102), Some("session=member")) > 0);
        assert_no_comic_bodies_requested(http.as_ref());
    }
}

#[tokio::test]
async fn novel_series_filters_both_masked_thumbnails_and_denied_contents_without_body_gets() {
    let http = Arc::new(ReaderFixture::new(Visibility::default()));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone());
    let result = downloader.download_novel("https://www.pixiv.net/novel/series/93").await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    assert_eq!(result.total_count, 2);
    assert_eq!(result.updated_count, 2);
    assert_eq!(novels.get(result.id.into()).await.unwrap().unwrap().general_all_no, Some(2));
    assert_saved_novels(&snapshot(store.as_ref()).await, 93, &[1, 4]);
    for id in [201, 204] {
        assert_eq!(http.calls(&format!("{NOVEL}{id}")), 1);
    }
    for id in [202, 203] {
        assert_eq!(http.calls(&format!("{NOVEL}{id}")), 0);
    }
}

#[tokio::test]
async fn masked_thirtieth_novel_does_not_truncate_the_next_contents_page() {
    let http = Arc::new(ReaderFixture::new(Visibility::default()));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone());
    let result = downloader.download_novel("https://www.pixiv.net/novel/series/94").await.unwrap();
    let orders: Vec<_> = (1..=29).chain(std::iter::once(31)).collect();
    assert_eq!(result.status, UpdateStatus::Ok);
    assert_eq!(result.total_count, 30);
    assert_eq!(result.updated_count, 30);
    assert_eq!(novels.get(result.id.into()).await.unwrap().unwrap().general_all_no, Some(30));
    assert_saved_novels(&snapshot(store.as_ref()).await, 94, &orders);
    assert_eq!(http.calls(&novel_page_url(94, 0)), 1);
    assert_eq!(http.calls(&novel_page_url(94, 30)), 1,
        "pagination follows thirty contents, not twenty-nine visible thumbnails");
    assert_eq!(http.calls(&format!("{NOVEL}330")), 0);
    for order in orders {
        assert_eq!(http.calls(&format!("{NOVEL}{}", novel_id(94, order))), 1);
    }
}

#[tokio::test]
async fn empty_novel_contents_at_eof_is_not_a_partial_listing_or_login_retry() {
    let http = Arc::new(ReaderFixture::new(Visibility::default()));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone()).with_cookie_store(logins().await);
    let result = downloader.download_novel("https://www.pixiv.net/novel/series/95").await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    assert_eq!(result.total_count, 30);
    assert_eq!(result.updated_count, 30);
    let record = novels.get(result.id.into()).await.unwrap().unwrap();
    assert_eq!(record.general_all_no, Some(30));
    assert!(!record.requires_login);
    assert_eq!(record.login_session, None);
    assert_saved_novels(&snapshot(store.as_ref()).await, 95, &(1..=30).collect::<Vec<_>>());
    assert_eq!(http.calls(&novel_page_url(95, 30)), 1);
    assert!(http.requests().iter().all(|request| request.cookie.is_none()),
        "a real empty EOF must not trigger a saved-account retry");
}

#[tokio::test]
async fn essential_comic_metadata_http403_and_malformed_json_remain_errors() {
    for fault in [Fault::MetadataForbidden, Fault::MetadataMalformed] {
        let http = Arc::new(ReaderFixture::new(Visibility { fault: Some(fault), ..Default::default() }));
        let novels = Arc::new(MemoryNovelRepository::new());
        let store = Arc::new(MemoryObjectStore::new());
        let mut downloader = downloader(http.clone(), novels.clone(), store.clone()).with_cookie_store(logins().await);
        let error = downloader.download_novel(COMIC).await.unwrap_err();
        assert!(matches!(&error, NarouError::Http(_)), "{fault:?}: {error}");
        let message = error.to_string();
        assert!(message.contains(&metadata_url(102)), "{message}");
        if fault == Fault::MetadataForbidden { assert!(message.contains("403"), "{message}"); }
        assert_eq!(novels.count(&NovelFilter::default()).await.unwrap(), 0);
        assert!(snapshot(store.as_ref()).await.is_empty(), "{fault:?}: no shortened consumer TOC");
        assert_eq!(http.calls(&metadata_url(102)), 1);
        assert!(http.requests().iter().all(|request| request.cookie.is_none()),
            "HTTP/JSON failure is not an application-level login denial");
        assert_no_comic_bodies_requested(http.as_ref());
    }
}

#[tokio::test]
async fn required_artwork_pages_http403_does_not_register_a_shortened_series() {
    let http = Arc::new(ReaderFixture::new(Visibility { fault: Some(Fault::PagesForbidden), ..Default::default() }));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader(http.clone(), novels.clone(), store.clone());
    let error = downloader.download_novel(COMIC).await.unwrap_err();
    assert!(matches!(&error, NarouError::Http(_)), "{error}");
    assert!(error.to_string().contains("403"));
    assert!(error.to_string().contains(&pages_url(102)));
    assert_eq!(novels.count(&NovelFilter::default()).await.unwrap(), 0);
    let saved = snapshot(store.as_ref()).await;
    assert!(saved.keys().all(|key| !key.ends_with("/toc.yaml")));
    let sections = saved_sections(&saved);
    assert_eq!(sections.keys().map(String::as_str).collect::<Vec<_>>(), ["1"]);
    assert_eq!(sections["1"].element.body.trim(), format!("<img src=\"{}\">", image_url(101)));
    assert_eq!(http.calls(&pages_url(101)), 1);
    assert_eq!(http.calls(&image_url(101)), 1);
    assert_eq!(http.calls(&pages_url(102)), 1);
    assert_eq!(http.calls(&image_url(102)), 0);
    assert_eq!(http.calls(&pages_url(103)), 0);
}

#[tokio::test]
async fn masked_or_omitted_middle_chapters_use_listing_fallback_before_private_metadata() {
    for private_reply in [PrivateReply::MaskedThumbnail, PrivateReply::Omitted] {
        for has_login in [false, true] {
            let http = Arc::new(ReaderFixture::new(Visibility {
                private_orders: vec![2], private_reply, member_has_access: true, ..Default::default()
            }));
            let novels = Arc::new(MemoryNovelRepository::new());
            let store = Arc::new(MemoryObjectStore::new());
            let mut downloader = downloader(http.clone(), novels.clone(), store.clone());
            if has_login { downloader = downloader.with_cookie_store(logins().await); }
            let result = downloader.download_novel(COMIC).await.unwrap();
            let chapters = if has_login { vec![(1, 101), (2, 102), (3, 103)] }
                else { vec![(1, 101), (3, 103)] };
            assert_eq!(result.total_count, chapters.len());
            assert_saved_comic(&snapshot(store.as_ref()).await, 91, &chapters);
            let record = novels.get(result.id.into()).await.unwrap().unwrap();
            assert_eq!(record.requires_login, has_login);
            assert_eq!(record.login_session.as_deref(), has_login.then_some("member"));
            assert_eq!(http.calls_with_cookie(&metadata_url(102), None), 0);
        }
    }
}

#[tokio::test]
async fn comic_series_merges_episode_tags_without_duplicates() {
    // 漫画シリーズは各話の /ajax/illust/{id} が持つタグを統合する。
    // 同じタグが複数話にあっても 1 つにまとめ、初出順を保つ。
    let http = Arc::new(ReaderFixture::new(Visibility::default()));
    let novels = Arc::new(MemoryNovelRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let mut downloader = downloader_with_settings(
        http.clone(), novels.clone(), store.clone(),
        HashMap::from([("auto-add-tags".to_string(), true)]),
    );
    let result = downloader.download_novel(COMIC).await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    let record = novels.get(result.id.into()).await.unwrap().unwrap();
    // 分類は属性として保存し、作品側のタグとは分ける。
    assert_eq!(record.attribute_tag(), Some("漫画"));
    assert_eq!(record.tags, vec!["共通タグ", "タグ1", "タグ2", "タグ3"]);
    assert_saved_comic(&snapshot(store.as_ref()).await, 91, &[(1, 101), (2, 102), (3, 103)]);
}
