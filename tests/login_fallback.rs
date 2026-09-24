//! Login fallback: a novel that only fetches with a stored login cookie is
//! retried once with that cookie and then flagged, so later runs send it from
//! the start. Novels without a stored cookie keep the previous behavior.
//!
//! Lives in its own binary because the download pipeline touches the
//! process-wide database and working directory.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use narou_rs::downloader::Downloader;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::platform::mocks::{
    FakeRateLimiter, MemoryCookieStore, MemoryNovelRepository, MemoryObjectStore,
};
use narou_rs::platform::{
    AssetStore, CookieStore, HttpClient, HttpRequest, HttpResponse, NovelRepository, ObjectStore,
    PlatformFuture,
};

/// The pipeline reads the process-wide working directory and database, so the
/// tests in this binary must not run at the same time.
static PIPELINE: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SITE_YAML: &str = r#"
name: TestSite
domain: example.com
top_url: https://\k<domain>
url: https://\k<domain>/novel/(?<ncode>n\d+[a-z]+)
toc_url: https://\k<domain>/novel/\k<ncode>/
encoding: UTF-8
sitename: TestSite
login_url: https://\k<domain>/login
login_pattern: ログインが必要です
title: '<h1 class="title">(?<title>.+?)</h1>'
author: '<span class="author">(?<author>.+?)</span>'
subtitles: '<a href="(?<href>[^"]+)" class="subtitle">(?<subtitle>[^<]+)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
version: 1.0
"#;

/// Serves a login wall (404) until the stored login cookie is sent.
struct LoginWallClient {
    toc_url: String,
    section_url: String,
    anonymous_requests: AtomicUsize,
    authenticated_requests: AtomicUsize,
}

impl HttpClient for LoginWallClient {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, narou_rs::error::Result<HttpResponse>> {
        Box::pin(async move {
            let cookie = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                .map(|(_, value)| value.clone());
            let logged_in = cookie
                .as_deref()
                .is_some_and(|value| value.contains("session=abc"));
            if logged_in {
                self.authenticated_requests.fetch_add(1, Ordering::SeqCst);
            } else {
                self.anonymous_requests.fetch_add(1, Ordering::SeqCst);
            }
            let (status, body) = match (logged_in, request.url.as_str()) {
                (false, _) => (404, "ログインが必要です".to_string()),
                (true, url) if url == self.toc_url => (
                    200,
                    concat!(
                        r#"<h1 class="title">ログイン限定作品</h1><span class="author">作者</span>"#,
                        r#"<a href="/n1234ab/1/" class="subtitle">第1話</a>"#
                    )
                    .to_string(),
                ),
                (true, url) if url == self.section_url => {
                    (200, r#"<div class="body">本文</div>"#.to_string())
                }
                (true, _) => (404, String::new()),
            };
            Ok(HttpResponse {
                status,
                headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                body: body.into_bytes(),
            })
        })
    }
}

#[tokio::test]
async fn login_fallback_retries_with_the_stored_cookie_and_flags_the_novel() {
    let _serial = PIPELINE.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
    let site_dir = temp.path().join("webnovel");
    std::fs::create_dir_all(&site_dir).unwrap();
    std::fs::write(site_dir.join("test.yaml"), SITE_YAML).unwrap();
    narou_rs::db::init_database().unwrap();

    // `load_all` reads the working directory's `webnovel/` (plus the bundled
    // definitions) exactly like the CLI does.
    let settings: Vec<SiteSetting> = SiteSetting::load_all()
        .unwrap()
        .into_iter()
        .filter(|setting| setting.domain == "example.com")
        .collect();
    assert_eq!(settings.len(), 1, "the test site definition must load");

    let toc_url = "https://example.com/novel/n1234ab/";
    let http = Arc::new(LoginWallClient {
        toc_url: toc_url.to_string(),
        section_url: "https://example.com/n1234ab/1/".to_string(),
        anonymous_requests: AtomicUsize::new(0),
        authenticated_requests: AtomicUsize::new(0),
    });
    let cookies = Arc::new(MemoryCookieStore::new());
    cookies
        .save_groups(
            "example.com",
            &[narou_rs::platform::LoginGroup::new(
                "example.com",
                vec![narou_rs::platform::HostCookie {
                    host: "example.com".to_string(),
                    cookie: "session=abc".to_string(),
                }],
            )],
        )
        .await
        .unwrap();

    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store;
    let repository = Arc::new(MemoryNovelRepository::new());
    let mut downloader = Downloader::with_platform_and_storage_and_settings(
        http.clone(),
        Arc::new(FakeRateLimiter::new()),
        repository.clone(),
        objects,
        assets,
        Arc::new(narou_rs::platform::SystemClock),
        settings,
        Default::default(),
    )
    .unwrap()
    .with_cookie_store(cookies.clone());

    let result = downloader.download_novel(toc_url).await.unwrap();
    assert_eq!(result.status, narou_rs::downloader::UpdateStatus::Ok);

    let record = repository.get(result.id.into()).await.unwrap().unwrap();
    assert!(
        record.requires_login,
        "the novel must be flagged once the login cookie rescued the fetch"
    );
    let session = record
        .login_session
        .clone()
        .expect("the credential that worked is remembered by id");
    let stored = cookies.load_groups("example.com").await.unwrap();
    assert_eq!(
        stored.iter().filter(|group| group.id == session).count(),
        1,
        "the recorded id must address the stored login: {session}"
    );

    assert!(
        http.anonymous_requests.load(Ordering::SeqCst) > 0,
        "the first attempt must stay anonymous"
    );
    assert!(
        http.authenticated_requests.load(Ordering::SeqCst) > 0,
        "the retry must send the stored login cookie"
    );
}

/// Serves the open novel anonymously and the walled one only with a login.
struct PerNovelClient {
    anonymous_by_ncode: std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
    authenticated_by_ncode: std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
}

impl PerNovelClient {
    fn new() -> Self {
        Self {
            anonymous_by_ncode: std::sync::Mutex::new(Default::default()),
            authenticated_by_ncode: std::sync::Mutex::new(Default::default()),
        }
    }

    fn count(counter: &std::sync::Mutex<std::collections::BTreeMap<String, usize>>, ncode: &str) {
        *counter.lock().unwrap().entry(ncode.to_string()).or_default() += 1;
    }

    fn calls(
        counter: &std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
        ncode: &str,
    ) -> usize {
        counter
            .lock()
            .unwrap()
            .get(ncode)
            .copied()
            .unwrap_or_default()
    }

    fn ncode(url: &str) -> String {
        // `/novel/n1111bb/` でも `n1111bb/1/` でも `n1111bb` を取り出す。
        url.split(['/', '?'])
            .find(|part| {
                part.starts_with('n') && part.chars().any(|c| c.is_ascii_digit())
            })
            .unwrap_or("")
            .to_string()
    }
}

impl HttpClient for PerNovelClient {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, narou_rs::error::Result<HttpResponse>> {
        Box::pin(async move {
            let cookie = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                .map(|(_, value)| value.clone());
            let logged_in = cookie
                .as_deref()
                .is_some_and(|value| value.contains("session=abc"));
            let ncode = Self::ncode(&request.url);
            if logged_in {
                Self::count(&self.authenticated_by_ncode, &ncode);
            } else {
                Self::count(&self.anonymous_by_ncode, &ncode);
            }
            let walled = ncode == "n9999aa";
            let is_toc = request.url.contains("/novel/");
            let (status, body) = match (walled, logged_in) {
                (true, false) => (404, "ログインが必要です".to_string()),
                (_, _) if is_toc => (
                    200,
                    format!(
                        concat!(
                            r#"<h1 class="title">作品 {ncode}</h1><span class="author">作者</span>"#,
                            r#"<a href="/{ncode}/1/" class="subtitle">第1話</a>"#
                        ),
                        ncode = ncode
                    ),
                ),
                _ => (200, r#"<div class="body">本文</div>"#.to_string()),
            };
            Ok(HttpResponse {
                status,
                headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                body: body.into_bytes(),
            })
        })
    }
}

/// The unit is the novel: a work that fetches anonymously must never carry a
/// cookie, even when the site has stored logins and another work used one.
#[tokio::test]
async fn novels_that_do_not_need_login_are_never_fetched_logged_in() {
    let _serial = PIPELINE.lock().unwrap_or_else(|error| error.into_inner());
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

    let http = Arc::new(PerNovelClient::new());
    let cookies = Arc::new(MemoryCookieStore::new());
    cookies
        .save_groups(
            "example.com",
            &[narou_rs::platform::LoginGroup::new(
                "example.com",
                vec![narou_rs::platform::HostCookie {
                    host: "example.com".to_string(),
                    cookie: "session=abc".to_string(),
                }],
            )],
        )
        .await
        .unwrap();
    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store;
    let repository = Arc::new(MemoryNovelRepository::new());
    let mut downloader = Downloader::with_platform_and_storage_and_settings(
        http.clone(),
        Arc::new(FakeRateLimiter::new()),
        repository.clone(),
        objects,
        assets,
        Arc::new(narou_rs::platform::SystemClock),
        settings,
        Default::default(),
    )
    .unwrap()
    .with_cookie_store(cookies.clone());

    // 1. ログインが要る作品を先に落とす (サイトには Cookie が保存されている)。
    let walled = downloader
        .download_novel("https://example.com/novel/n9999aa/")
        .await
        .unwrap();
    assert_eq!(walled.status, narou_rs::downloader::UpdateStatus::Ok);
    assert!(
        PerNovelClient::calls(&http.authenticated_by_ncode, "n9999aa") > 0,
        "the walled novel must have been retried with the stored login"
    );

    // 2. ログインが要らない作品は、その後でも匿名のままにする。
    let open = downloader
        .download_novel("https://example.com/novel/n1111bb/")
        .await
        .unwrap();
    assert_eq!(open.status, narou_rs::downloader::UpdateStatus::Ok);
    assert_eq!(
        PerNovelClient::calls(&http.authenticated_by_ncode, "n1111bb"),
        0,
        "a novel that does not need login must never be fetched logged in"
    );
    assert!(
        PerNovelClient::calls(&http.anonymous_by_ncode, "n1111bb") > 0,
        "it must have been fetched anonymously"
    );
    let record = repository.get(open.id.into()).await.unwrap().unwrap();
    assert!(
        !record.requires_login,
        "a novel that worked anonymously must not be flagged"
    );
    assert!(record.login_session.is_none(), "and it stores no session");
}
