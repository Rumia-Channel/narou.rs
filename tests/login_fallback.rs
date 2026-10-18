//! Login fallback: a novel that only fetches with a stored login cookie is
//! retried once with that cookie and then flagged, so later runs send it from
//! the start. Novels without a stored cookie keep the previous behavior.
//!
//! Lives in its own binary because the download pipeline touches the
//! process-wide database and working directory.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
static PIPELINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _serial = PIPELINE.lock().await;
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
        narou_rs::downloader::DownloaderPlatform {
            http: http.clone(),
            rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels: repository.clone(),
            objects,
            assets,
            clock: Arc::new(narou_rs::platform::SystemClock),
        },
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
                (false, false) if is_toc && ncode == "n8888cc" => (
                    200,
                    r#"<h1 class="title">作品 n8888cc</h1><span class="author">作者</span>"#.to_string(),
                ),
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
        narou_rs::downloader::DownloaderPlatform {
            http: http.clone(),
            rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels: repository.clone(),
            objects,
            assets,
            clock: Arc::new(narou_rs::platform::SystemClock),
        },
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
    let initially_empty = downloader
        .download_novel("https://example.com/novel/n8888cc/")
        .await
        .unwrap();
    assert_eq!(initially_empty.total_count, 1);
    let authenticated = repository.get(initially_empty.id.into()).await.unwrap().unwrap();
    assert!(authenticated.requires_login);
    assert!(authenticated.login_session.is_some());


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

struct RedirectCookieClient {
    redirect: AtomicBool,
    same_site: bool,
    destination_open: bool,
    requests: std::sync::Mutex<Vec<(String, String)>>,
}

impl RedirectCookieClient {
    fn landing(&self) -> &str {
        if self.same_site { "https://example.com/novel/n5678bb/" }
        else { "https://destination.example/novel/n1234aa/" }
    }
}

impl HttpClient for RedirectCookieClient {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, narou_rs::error::Result<HttpResponse>> {
        Box::pin(async move {
            let cookie = request.header("Cookie").unwrap_or("").to_string();
            self.requests.lock().unwrap().push((request.url.clone(), cookie.clone()));
            let source = "https://example.com/novel/n1234aa/";
            let redirected = self.redirect.load(Ordering::SeqCst);
            if request.url == source && redirected {
                return Ok(HttpResponse { status: 302,
                    headers: vec![("Location".into(), self.landing().into())], body: Vec::new() });
            }
            let source_login = cookie.contains("origin_only=canary") && cookie.contains("source_session=source");
            let destination_login = if self.same_site { source_login }
                else { cookie.contains("consent=accepted") && cookie.contains("session=destination") };
            let (status, body) = if request.url == source {
                if source_login { (200, concat!(
                    r#"<h1 class="title">転送前の作品</h1><span class="author">作者</span>"#,
                    r#"<a href="/novel/n1234aa/1/" class="subtitle">第1話</a>"#
                ).to_string()) } else { (404, "ログインが必要です".into()) }
            } else if request.url == self.landing() {
                let full = self.destination_open || destination_login;
                let mut body = format!(
                    r#"<h1 class="title">転送先の作品</h1><span class="author">作者</span><a href="{}1/" class="subtitle">第1話</a>"#,
                    self.landing(),
                );
                if full {
                    body.push_str(&format!(r#"<a href="{}2/" class="subtitle">第2話</a>"#, self.landing()));
                } else {
                    body.push_str("login_partial::1");
                }
                (200, body)
            } else if request.url == format!("{source}1/")
                || request.url == format!("{}1/", self.landing())
                || request.url == format!("{}2/", self.landing())
            {
                (200, r#"<div class="body">本文</div>"#.into())
            } else {
                return Err(narou_rs::error::NarouError::Http(format!("unexpected fixture URL: {}", request.url)));
            };
            Ok(HttpResponse { status,
                headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                body: body.into_bytes() })
        })
    }
}

#[tokio::test]
async fn redirected_sites_use_their_own_consent_and_login_scope() {
    let _serial = PIPELINE.lock().await;
    for (flagged_source, destination_open, same_site) in [
        (false, false, false), (true, false, false), (true, true, false), (true, false, true),
    ] {
        let source_yaml = format!("{SITE_YAML}\ncookie: origin_only=canary\nlogin_partial_pattern: 'login_partial::1'\n");
        let destination_yaml = SITE_YAML.replace("TestSite", "DestinationSite")
            .replace("example.com", "destination.example");
        let destination_yaml = format!("{destination_yaml}\ncookie: consent=accepted\nlogin_partial_pattern: 'login_partial::1'\n");
        let definitions = [source_yaml.as_str(), destination_yaml.as_str()];
        let sites = SiteSetting::load_bundled(if same_site { &definitions[..1] } else { &definitions }).unwrap();
        let http = Arc::new(RedirectCookieClient {
            redirect: AtomicBool::new(!flagged_source), same_site, destination_open,
            requests: Default::default(),
        });
        let cookies = Arc::new(MemoryCookieStore::new());
        for (domain, cookie, id) in [
            ("example.com", "source_session=source", "source-session"),
            ("destination.example", "session=destination", "destination-session"),
        ] {
            cookies.save_groups(domain, &[narou_rs::platform::LoginGroup::new(domain, vec![
                narou_rs::platform::HostCookie { host: domain.into(), cookie: cookie.into() },
            ]).with_id(id)]).await.unwrap();
        }
        let store = Arc::new(MemoryObjectStore::new());
        let repository = Arc::new(MemoryNovelRepository::new());
        let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            narou_rs::downloader::DownloaderPlatform {
                http: http.clone(), rate_limiter: Arc::new(FakeRateLimiter::new()),
                novels: repository.clone(), objects: store.clone(), assets: store,
                clock: Arc::new(narou_rs::platform::SystemClock),
            }, sites, Default::default(),
            Arc::new(narou_rs::downloader::settings::SnapshotDownloaderSettings::new(
                Default::default(), Some(true), Default::default(),
            )),
        ).unwrap().with_cookie_store(cookies);
        let source = "https://example.com/novel/n1234aa/";
        if flagged_source {
            let initial = downloader.download_novel(source).await.unwrap();
            assert!(repository.get(initial.id.into()).await.unwrap().unwrap().requires_login);
            http.redirect.store(true, Ordering::SeqCst);
            http.requests.lock().unwrap().clear();
        }
        let result = downloader.download_novel(source).await.unwrap();
        assert_eq!(result.total_count, 2);
        let record = repository.get(result.id.into()).await.unwrap().unwrap();
        assert_eq!(record.toc_url, http.landing());
        assert_eq!(record.requires_login, !destination_open);
        assert_eq!(record.login_session.as_deref(), if destination_open { None }
            else if same_site { Some("source-session") } else { Some("destination-session") });
        if !same_site {
            for (url, cookie) in http.requests.lock().unwrap().iter()
                .filter(|(url, _)| url.starts_with("https://destination.example/"))
            {
                assert!(!cookie.contains("origin_only=") && !cookie.contains("source_session="),
                    "source credentials escaped to {url}");
                if destination_open {
                    assert!(!cookie.contains("session=destination"), "public destination used a saved login");
                }
            }
        }
    }
}
