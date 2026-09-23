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
        .save_all(
            "example.com",
            &[narou_rs::platform::LoginCredential::new(
                "example.com",
                "session=abc",
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
    let stored = cookies.load_all("example.com").await.unwrap();
    assert_eq!(
        stored.iter().filter(|credential| credential.id == session).count(),
        1,
        "the recorded id must address the stored credential: {session}"
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
