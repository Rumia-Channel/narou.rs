//! Authentication recovery in the download pipeline.
//!
//! A login wall must not end in a truncated listing, must not mix one
//! account's cookies into another account's request, and must not turn a
//! failure that has nothing to do with authentication into a retry.

#![cfg(feature = "native-runtime")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use narou_rs::downloader::persistence::PersistenceService;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{Downloader, DownloaderPlatform, TocFile, UpdateStatus};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::mocks::{FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore};
use narou_rs::platform::{
    AssetStore, CookieStore, HostCookie, HttpClient, HttpRequest, HttpResponse, LoginGroup,
    NovelMutation, NovelObjectKeys, NovelRepository, ObjectStore, PlatformFuture, SystemClock,
};

/// The pipeline reads the process-wide database and the working directory,
/// so the tests in this binary must not run at the same time.
static PIPELINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A site whose table of contents is public and whose bodies may need a
/// stored login.
const SITE_YAML_BASE: &str = r#"
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
next_toc: '<a class="next" href="(?<next_url>[^"]+)">次へ</a>'
next_url: '\k<next_url>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
version: 1.0
"#;

/// The site definition with the meaning of a login wall.
///
/// `login` reports an authentication error and keeps the record; without the
/// key narou.rb's default applies and the novel is reported as unavailable.
fn site_yaml(login_wall_error: Option<&str>) -> String {
    let mut yaml = SITE_YAML_BASE.to_string();
    if let Some(mode) = login_wall_error {
        yaml.push_str(&format!("login_wall_error: {mode}\n"));
    }
    yaml
}

const TOC_URL: &str = "https://example.com/novel/n1234ab/";
const TOC_PAGE2_URL: &str = "https://example.com/novel/n1234ab/page/2";
const BODY1_URL: &str = "https://example.com/novel/n1234ab/1/";
const BODY2_URL: &str = "https://example.com/novel/n1234ab/2/";

/// The login wall the site answers with.
const LOGIN_WALL: &str = "<h1 class=\"title\">ログインが必要です</h1>";

/// One request as the fixture server saw it.
#[derive(Clone, Debug)]
struct Seen {
    url: String,
    cookie: String,
}

/// The fixture server: it records every request and answers from a script.
struct Fixture {
    /// Whether the table of contents links to a second page.
    two_pages: AtomicBool,
    /// Whether the table of contents needs a stored login.
    private_toc: bool,
    /// Whether a body needs a stored login.
    private_body: bool,
    /// The cookie the server accepts; every other credential gets the wall.
    accepted: Option<String>,
    /// Whether every request is a login wall.
    reject_all: AtomicBool,
    /// Whether every request fails with a plain server error.
    server_error: bool,
    /// Every request the server saw.
    seen: std::sync::Mutex<Vec<Seen>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            two_pages: AtomicBool::new(false),
            private_toc: false,
            private_body: false,
            accepted: None,
            reject_all: AtomicBool::new(false),
            server_error: false,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn with_private_toc(mut self) -> Self {
        self.private_toc = true;
        self
    }

    fn with_private_body(mut self) -> Self {
        self.private_body = true;
        self
    }

    fn with_accepted(mut self, cookie: &str) -> Self {
        self.accepted = Some(cookie.to_string());
        self
    }

    /// Reject every request with the login wall from now on.
    fn reject_all(&self) {
        self.reject_all.store(true, Ordering::SeqCst);
    }

    fn with_server_error(mut self) -> Self {
        self.server_error = true;
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("request log").clone()
    }

    fn count(&self, url: &str) -> usize {
        self.seen().iter().filter(|seen| seen.url == url).count()
    }

    fn count_with_cookie(&self, url: &str, cookie: &str) -> usize {
        self.seen()
            .iter()
            .filter(|seen| seen.url == url && seen.cookie.contains(cookie))
            .count()
    }
}

fn html(body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
        body: body.as_bytes().to_vec(),
    }
}

/// The table of contents: two episodes, and a link to a second page when the
/// story grew.
fn toc_page(two_pages: bool) -> String {
    let mut body =
        String::from(r#"<h1 class="title">壁のある作品</h1><span class="author">作者</span>"#);
    body.push_str(r#"<a href="/novel/n1234ab/1/" class="subtitle">第1話</a>"#);
    body.push_str(r#"<a href="/novel/n1234ab/2/" class="subtitle">第2話</a>"#);
    if two_pages {
        body.push_str(&format!(
            r#"<a class="next" href="{TOC_PAGE2_URL}">次へ</a>"#
        ));
    }
    body
}

impl HttpClient for Fixture {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let cookie = request.header("Cookie").unwrap_or("").to_string();
            self.seen.lock().expect("request log").push(Seen {
                url: request.url.clone(),
                cookie: cookie.clone(),
            });
            if self.server_error {
                return Ok(HttpResponse {
                    status: 500,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
            }
            let authorized = self
                .accepted
                .as_ref()
                .is_none_or(|accepted| cookie.contains(accepted));
            let response = if self.reject_all.load(Ordering::SeqCst) {
                html(LOGIN_WALL)
            } else if request.url == TOC_URL {
                if self.private_toc && !authorized {
                    html(LOGIN_WALL)
                } else {
                    html(&toc_page(self.two_pages.load(Ordering::SeqCst)))
                }
            } else if request.url == TOC_PAGE2_URL {
                // The second page is always the login wall.
                html(LOGIN_WALL)
            } else if request.url == BODY1_URL || request.url == BODY2_URL {
                if self.private_body && !authorized {
                    html(LOGIN_WALL)
                } else {
                    html(r#"<div class="body">本文</div>"#)
                }
            } else {
                return Err(NarouError::Http(format!(
                    "unexpected fixture request: {}",
                    request.url
                )));
            };
            Ok(response)
        })
    }

    fn send_stream<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, Result<narou_rs::platform::HttpStreamResponse>> {
        let url = request.url.clone();
        Box::pin(async move {
            let response = self.send(request).await?;
            Ok(narou_rs::platform::HttpStreamResponse {
                status: response.status,
                url,
                headers: response.headers,
                body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
            })
        })
    }
}

/// A cookie store that counts how often the stored logins are read.
struct CountingCookieStore {
    groups: std::sync::Mutex<std::collections::BTreeMap<String, Vec<LoginGroup>>>,
    loads: AtomicUsize,
}

impl CountingCookieStore {
    fn new() -> Self {
        Self {
            groups: std::sync::Mutex::new(Default::default()),
            loads: AtomicUsize::new(0),
        }
    }

    /// How often the stored logins were read.
    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }
}

impl CookieStore for CountingCookieStore {
    fn load_groups<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<Vec<LoginGroup>>> {
        Box::pin(async move {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .groups
                .lock()
                .expect("cookie store")
                .get(site)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn save_groups<'a>(
        &'a self,
        site: &'a str,
        groups: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.groups
                .lock()
                .expect("cookie store")
                .insert(site.to_string(), groups.to_vec());
            Ok(())
        })
    }

    fn compare_save_groups<'a>(
        &'a self,
        _site: &'a str,
        _expected: &'a [LoginGroup],
        _replacement: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(self
                .groups
                .lock()
                .expect("cookie store")
                .remove(site)
                .is_some())
        })
    }

    fn list_groups(
        &self,
    ) -> PlatformFuture<'_, Result<std::collections::BTreeMap<String, Vec<LoginGroup>>>> {
        Box::pin(async move { Ok(self.groups.lock().expect("cookie store").clone()) })
    }

    fn save_all_groups(
        &self,
        _groups: &std::collections::BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn merge_all_groups(
        &self,
        _groups: &std::collections::BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'_, Result<usize>> {
        Box::pin(async { Ok(0) })
    }

    fn renew_response_cookies<'a>(
        &'a self,
        _sent_cookie: &'a str,
        _set_cookie: &'a [String],
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
}

fn token_group(id: &str, cookie: &str) -> LoginGroup {
    LoginGroup::new(
        "example.com",
        vec![HostCookie {
            host: "example.com".to_string(),
            cookie: cookie.to_string(),
        }],
    )
    .with_id(id)
}

/// Build a downloader over the fixture server.
fn build_downloader(
    http: Arc<dyn HttpClient>,
    cookies: Arc<CountingCookieStore>,
    yaml: &str,
) -> (Downloader, Arc<MemoryNovelRepository>, Arc<MemoryObjectStore>) {
    let settings = SiteSetting::load_bundled(&[yaml]).expect("site definition");
    let store = Arc::new(MemoryObjectStore::new());
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store.clone();
    let repository = Arc::new(MemoryNovelRepository::new());
    let downloader = Downloader::with_platform_and_storage_and_settings(
        DownloaderPlatform {
            http,
            rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels: repository.clone(),
            objects,
            assets,
            clock: Arc::new(SystemClock),
        },
        settings,
        Default::default(),
    )
    .expect("downloader")
    .with_cookie_store(cookies);
    (downloader, repository, store)
}

/// The table of contents the downloader saved for a record.
async fn saved_toc(
    store: &Arc<MemoryObjectStore>,
    record: &narou_rs::db::NovelRecord,
) -> Option<TocFile> {
    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
        .expect("novel object keys");
    let objects: Arc<dyn ObjectStore> = store.clone();
    let assets: Arc<dyn AssetStore> = store.clone();
    PersistenceService::new(objects, assets)
        .load_toc(&keys)
        .await
        .expect("load saved toc")
}

/// Restore the CWD before TempDir removes the library, including on panic.
struct ScratchLibrary {
    temp: tempfile::TempDir,
    previous: std::path::PathBuf,
}

impl ScratchLibrary {
    fn path(&self) -> &std::path::Path {
        self.temp.path()
    }
}

impl Drop for ScratchLibrary {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

fn scratch_library() -> ScratchLibrary {
    let previous = std::env::current_dir().expect("previous working directory");
    let temp = tempfile::tempdir().expect("scratch library");
    std::fs::create_dir_all(temp.path().join(".narou")).expect("scratch .narou dir");
    ScratchLibrary { temp, previous }
}

/// A short description of what a run did, without any credential value.
fn describe(outcome: &Result<narou_rs::downloader::DownloadResult>) -> String {
    match outcome {
        Ok(result) => format!("ok(status={:?})", result.status),
        Err(NarouError::Login(_)) => "login-error".to_string(),
        Err(error) => format!("error({error})"),
    }
}

/// A wall on a later page of the table of contents must abort the update
/// instead of storing the truncated listing the walk ended up with.
#[tokio::test]
async fn a_wall_on_a_later_toc_page_aborts_the_update_without_saving_a_truncated_listing() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new().with_private_body().with_accepted("session=seed"));
    let cookies = Arc::new(CountingCookieStore::new());
    cookies
        .save_groups("example.com", &[token_group("seed", "session=seed")])
        .await
        .unwrap();
    let (mut downloader, repository, store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(Some("login")));

    // First acquisition: one page, two episodes, both bodies stored.
    let acquired = downloader.download_novel(TOC_URL).await.unwrap();
    assert_eq!(acquired.status, UpdateStatus::Ok);
    let record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert_eq!(record.general_all_no, Some(2));
    assert!(record.requires_login);
    let last_update = record.last_update;

    // The story grew: the first page now links to a second page, and that
    // page is a login wall.
    http.two_pages.store(true, Ordering::SeqCst);
    let bodies_before = http.count(BODY1_URL);

    let outcome = downloader.download_novel(TOC_URL).await;

    let outcome_text = describe(&outcome);
    assert!(
        matches!(outcome, Err(NarouError::Login(_))),
        "a login wall must abort the update, got {outcome_text}"
    );

    // The aborted update leaves the saved library exactly as it was.
    let after = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert_eq!(
        after.general_all_no,
        Some(2),
        "the truncated listing must not be stored"
    );
    assert_eq!(
        after.last_update, last_update,
        "an aborted update must not rewrite the record"
    );
    let saved = saved_toc(&store, &after).await.expect("saved toc");
    assert_eq!(saved.subtitles.len(), 2, "the saved toc must keep both episodes");
    assert_eq!(
        http.count(BODY1_URL),
        bodies_before,
        "no body may be fetched while the table of contents is walled"
    );
}

/// A stored login whose token expired is replaced by another stored login,
/// and the accounts' cookies never mix.
#[tokio::test]
async fn a_stale_selected_account_is_rescued_by_another_stored_login_without_mixing_cookies() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new().with_private_toc().with_accepted("session=fresh"));
    let cookies = Arc::new(CountingCookieStore::new());
    cookies
        .save_groups("example.com", &[token_group("fresh", "session=fresh")])
        .await
        .unwrap();
    let (mut downloader, repository, _store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(Some("login")));

    // A valid acquisition seeds a record that uses a stored credential.
    let acquired = downloader.download_novel(TOC_URL).await.unwrap();
    assert_eq!(acquired.status, UpdateStatus::Ok);
    let record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert!(record.requires_login);
    assert_eq!(record.login_session.as_deref(), Some("fresh"));

    // The record now points at an expired account that carries a cookie name
    // the working account does not have.
    let mut pointed = record.clone();
    pointed.login_session = Some("expired".to_string());
    repository
        .apply_batch(vec![NovelMutation::Upsert(pointed)])
        .await
        .unwrap();
    cookies
        .save_groups(
            "example.com",
            &[
                token_group("expired", "session=expired; extra=A"),
                token_group("fresh", "session=fresh"),
            ],
        )
        .await
        .unwrap();

    let seen_before = http.seen().len();
    let outcome = downloader.download_novel_with_force(TOC_URL, true).await;
    let outcome_text = describe(&outcome);
    assert!(
        outcome.is_ok(),
        "another stored login must rescue the update, got {outcome_text}"
    );

    let after = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert_eq!(after.login_session.as_deref(), Some("fresh"));
    assert!(after.requires_login);

    let seen = http.seen();
    let update = &seen[seen_before..];
    assert!(
        update
            .first()
            .is_some_and(|request| request.cookie.contains("session=expired")),
        "the account the record points at must be tried first"
    );
    for request in update {
        assert!(
            !(request.cookie.contains("session=fresh") && request.cookie.contains("extra=")),
            "the accounts' cookies must not mix: {}",
            request.cookie
        );
    }
    assert!(
        update.iter().any(|request| {
            request.cookie.contains("session=fresh") && !request.cookie.contains("session=expired")
        }),
        "the working account must be used"
    );
}

/// A public table of contents with a private body uses the saved login for
/// the body and keeps it for the remaining bodies and later runs.
#[tokio::test]
async fn a_private_body_uses_the_saved_login_even_when_the_toc_is_public() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new().with_private_body().with_accepted("session=seed"));
    let cookies = Arc::new(CountingCookieStore::new());
    cookies
        .save_groups("example.com", &[token_group("seed", "session=seed")])
        .await
        .unwrap();
    let (mut downloader, repository, _store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(Some("login")));

    let outcome = downloader.download_novel(TOC_URL).await;
    let outcome_text = describe(&outcome);
    assert!(
        outcome.is_ok(),
        "the saved login must rescue the bodies, got {outcome_text}"
    );

    let record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert!(record.requires_login);
    assert_eq!(record.login_session.as_deref(), Some("seed"));
    assert_eq!(http.count_with_cookie(BODY1_URL, "session=seed"), 1);
    assert_eq!(http.count_with_cookie(BODY2_URL, "session=seed"), 1);
}

/// A novel that needs no login stays anonymous and never reads the stored
/// logins.
#[tokio::test]
async fn a_novel_without_a_stored_login_stays_anonymous() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new());
    let cookies = Arc::new(CountingCookieStore::new());
    let (mut downloader, repository, _store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(Some("login")));

    let result = downloader.download_novel(TOC_URL).await.unwrap();
    assert_eq!(result.status, UpdateStatus::Ok);
    let record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert!(!record.requires_login);
    assert!(record.login_session.is_none());
    for request in http.seen() {
        assert!(
            request.cookie.is_empty(),
            "an anonymous novel must not send a cookie: {}",
            request.cookie
        );
    }
    assert_eq!(
        cookies.loads(),
        0,
        "a successful download must not read the stored logins"
    );
}

/// A failure that has nothing to do with authentication is not retried with a
/// stored login.
#[tokio::test]
async fn a_failure_that_is_not_authentication_is_not_retried_with_a_login() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new().with_server_error());
    let cookies = Arc::new(CountingCookieStore::new());
    cookies
        .save_groups("example.com", &[token_group("seed", "session=seed")])
        .await
        .unwrap();
    let (mut downloader, _repository, _store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(Some("login")));

    let outcome = downloader.download_novel(TOC_URL).await;
    assert!(
        matches!(outcome, Err(NarouError::Http(_))),
        "a server error stays a server error"
    );
    assert_eq!(
        http.seen().len(),
        1,
        "the failed fetch must not be repeated with a login"
    );
    assert_eq!(
        cookies.loads(),
        0,
        "a transport failure must not read the stored logins"
    );
}

/// With the site default, a login wall that no stored login resolves keeps
/// narou.rb's meaning: the novel is reported as unavailable.
#[tokio::test]
async fn the_default_login_wall_keeps_the_not_found_meaning() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new().with_accepted("session=seed"));
    let cookies = Arc::new(CountingCookieStore::new());
    cookies
        .save_groups("example.com", &[token_group("seed", "session=seed")])
        .await
        .unwrap();
    let (mut downloader, repository, _store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(None));

    // A valid acquisition creates the record.
    let acquired = downloader.download_novel(TOC_URL).await.unwrap();
    assert_eq!(acquired.status, UpdateStatus::Ok);
    assert!(repository.find_by_toc_url(TOC_URL).await.unwrap().is_some());

    // Now every stored login is rejected.
    http.reject_all();
    let outcome = downloader.download_novel(TOC_URL).await;
    let outcome_text = describe(&outcome);
    assert!(
        matches!(outcome, Ok(result) if result.status == UpdateStatus::Failed),
        "the default meaning of a login wall is an unavailable novel, got {outcome_text}"
    );
}

/// With `login_wall_error: login`, the same wall is an authentication error
/// and the existing novel is neither deleted nor frozen.
#[tokio::test]
async fn the_login_wall_error_mode_leaves_the_existing_novel_unfrozen() {
    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).expect("enter scratch library");
    narou_rs::db::init_database().unwrap();

    let http = Arc::new(Fixture::new().with_accepted("session=seed"));
    let cookies = Arc::new(CountingCookieStore::new());
    cookies
        .save_groups("example.com", &[token_group("seed", "session=seed")])
        .await
        .unwrap();
    let (mut downloader, repository, _store) =
        build_downloader(http.clone(), cookies.clone(), &site_yaml(Some("login")));

    // A valid acquisition creates the record.
    let acquired = downloader.download_novel(TOC_URL).await.unwrap();
    assert_eq!(acquired.status, UpdateStatus::Ok);
    let record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    let last_update = record.last_update;

    // Now every stored login is rejected.
    http.reject_all();
    let outcome = downloader.download_novel(TOC_URL).await;
    let outcome_text = describe(&outcome);
    assert!(
        matches!(outcome, Err(NarouError::Login(_))),
        "the login wall mode reports an authentication error, got {outcome_text}"
    );

    let after = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert_eq!(
        after.last_update, last_update,
        "an authentication error must not rewrite the record"
    );
}

#[tokio::test]
async fn strong_update_recovers_a_body_login_without_refetching_unchanged_content() {
    struct DatedFixture {
        inner: Arc<Fixture>,
        updated: AtomicBool,
    }
    impl HttpClient for DatedFixture {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let is_toc = request.url == TOC_URL;
                let mut response = self.inner.send(request).await?;
                if is_toc && response.status == 200 {
                    let stamp = if self.updated.load(Ordering::SeqCst) {
                        "2026/01/02 08:30"
                    } else {
                        "2026/01/01 08:00"
                    };
                    response.body = String::from_utf8(response.body).unwrap()
                        .replace("</a>", &format!("</a><time>{stamp}</time>")).into_bytes();
                }
                Ok(response)
            })
        }
        fn send_stream<'a>(
            &'a self,
            request: HttpRequest,
        ) -> PlatformFuture<'a, Result<narou_rs::platform::HttpStreamResponse>> {
            self.inner.send_stream(request)
        }
    }
    impl narou_rs::downloader::settings::DownloaderSettings for DatedFixture {
        fn local_setting_bool(&self, key: &str) -> bool {
            key == "update.strong"
        }
    }
    impl narou_rs::platform::Clock for DatedFixture {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
            let day = if self.updated.load(Ordering::SeqCst) { 2 } else { 1 };
            chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 1, day, 0, 10, 0).unwrap()
        }
    }

    let _serial = PIPELINE.lock().await;
    let temp = scratch_library();
    std::env::set_current_dir(temp.path()).unwrap();
    narou_rs::db::init_database().unwrap();
    let http = Arc::new(DatedFixture {
        inner: Arc::new(Fixture::new().with_private_body().with_accepted("session=seed")),
        updated: AtomicBool::new(false),
    });
    let cookies = Arc::new(CountingCookieStore::new());
    cookies.save_groups("example.com", &[token_group("seed", "session=seed")]).await.unwrap();
    let yaml = site_yaml(Some("login")).replace(
        "(?<subtitle>[^<]+)</a>",
        "(?<subtitle>[^<]+)</a><time>(?<subdate>[^<]+)</time>",
    );
    let yaml = yaml.replace(
        r#"(?<href>[^"]+)"#,
        "(?<href>/novel/n1234ab/(?<index>\\d+)/)",
    );
    let repository = Arc::new(MemoryNovelRepository::new());
    let objects = Arc::new(MemoryObjectStore::new());
    let mut downloader = Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform {
            http: http.clone(),
            rate_limiter: Arc::new(FakeRateLimiter::new()),
            novels: repository.clone(),
            objects: objects.clone(),
            assets: objects,
            clock: http.clone(),
        },
        SiteSetting::load_bundled(&[&yaml]).unwrap(),
        Default::default(),
        http.clone(),
    ).unwrap().with_cookie_store(cookies);
    downloader.download_novel(TOC_URL).await.unwrap();
    let mut record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    record.requires_login = false;
    record.login_session = None;
    repository.apply_batch(vec![NovelMutation::Upsert(record)]).await.unwrap();
    let first = http.inner.count(BODY1_URL);
    let second = http.inner.count(BODY2_URL);
    http.updated.store(true, Ordering::SeqCst);

    let updated = downloader.download_novel(TOC_URL).await.unwrap();
    assert_eq!(updated.status, UpdateStatus::None);
    assert_eq!(http.inner.count(BODY1_URL), first + 2);
    assert_eq!(http.inner.count(BODY2_URL), second + 1);
    let record = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
    assert!(record.requires_login);
    assert_eq!(record.login_session.as_deref(), Some("seed"));
}

#[tokio::test]
async fn failed_optional_logins_preserve_the_public_listing_and_its_credentials() {
    #[derive(Clone, Copy, Debug)]
    enum Failure {
        NotFound,
        LaterWall,
        ServerError,
    }
    struct PartialFixture {
        inner: Arc<Fixture>,
        partial: AtomicBool,
        failure: Failure,
    }
    impl HttpClient for PartialFixture {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let is_toc = request.url == TOC_URL;
                let authenticated = request.header("Cookie").is_some();
                let mut response = self.inner.send(request).await?;
                if is_toc && self.partial.load(Ordering::SeqCst) {
                    if !authenticated {
                        response = html(&format!("{}login_partial::1", toc_page(false)));
                    } else {
                        match self.failure {
                            Failure::LaterWall => response = html(&toc_page(true)),
                            Failure::NotFound | Failure::ServerError => {
                                response.status = match self.failure {
                                    Failure::NotFound => 404,
                                    _ => 500,
                                };
                                response.body.clear();
                            }
                        }
                    }
                }
                Ok(response)
            })
        }
        fn send_stream<'a>(
            &'a self,
            request: HttpRequest,
        ) -> PlatformFuture<'a, Result<narou_rs::platform::HttpStreamResponse>> {
            self.inner.send_stream(request)
        }
    }

    let _serial = PIPELINE.lock().await;
    for failure in [Failure::NotFound, Failure::LaterWall, Failure::ServerError] {
        let temp = scratch_library();
        std::env::set_current_dir(temp.path()).unwrap();
        narou_rs::db::init_database().unwrap();
        let http = Arc::new(PartialFixture {
            inner: Arc::new(Fixture::new()),
            partial: AtomicBool::new(false),
            failure,
        });
        let cookies = Arc::new(CountingCookieStore::new());
        cookies.save_groups("example.com", &[token_group("expired", "session=expired")]).await.unwrap();
        let mut yaml = site_yaml(None).replace(
            r#"(?<href>[^"]+)"#,
            "(?<href>/novel/n1234ab/(?<index>\\d+)/)",
        );
        yaml.push_str("login_partial_pattern: login_partial::1\n");
        let (mut downloader, repository, store) =
            build_downloader(http.clone(), cookies.clone(), &yaml);
        let acquired = downloader.download_novel(TOC_URL).await.unwrap();
        let before = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
        let before_toc = saved_toc(&store, &before).await.unwrap();
        let first_body = http.inner.count(BODY1_URL);
        let second_body = http.inner.count(BODY2_URL);
        http.partial.store(true, Ordering::SeqCst);

        let updated = downloader.download_novel_with_force(TOC_URL, true).await
            .unwrap_or_else(|error| panic!("{failure:?}: {error}"));
        assert_eq!(updated.status, UpdateStatus::Ok, "{failure:?}");
        assert_eq!(updated.id, acquired.id, "{failure:?}");
        let after = repository.find_by_toc_url(TOC_URL).await.unwrap().unwrap();
        assert_eq!(after.general_all_no, Some(2), "{failure:?}");
        assert!(!after.requires_login, "{failure:?}");
        assert!(after.login_session.is_none(), "{failure:?}");
        let after_toc = saved_toc(&store, &after).await.unwrap();
        // Forced downloads advance bookkeeping; the published entries must survive unchanged.
        assert!(
            after_toc.subtitles.iter()
                .map(|entry| (&entry.index, &entry.href, &entry.subtitle))
                .eq(before_toc.subtitles.iter()
                    .map(|entry| (&entry.index, &entry.href, &entry.subtitle))),
            "{failure:?}"
        );
        assert_eq!(http.inner.count(BODY1_URL), first_body + 1, "{failure:?}");
        assert_eq!(http.inner.count(BODY2_URL), second_body + 1, "{failure:?}");
        for url in [BODY1_URL, BODY2_URL] {
            assert_eq!(http.inner.count_with_cookie(url, "session=expired"), 0, "{failure:?}");
        }
        assert_eq!(cookies.loads(), 1, "{failure:?}");
    }
}
