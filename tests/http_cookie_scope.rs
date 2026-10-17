#![cfg(feature = "native-runtime")]

use narou_rs::downloader::http_policy::{FetchPolicy, fetch_bytes, resolve_final_url_with_body};
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::Result;
use narou_rs::native::http::NativeHttpClient;
use narou_rs::platform::http::validate_http_url_syntax;
use narou_rs::platform::mocks::FakeRateLimiter;
use narou_rs::platform::{HttpClient, HttpRequest, HttpResponse, PlatformFuture};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

/// These servers are trusted loopback infrastructure, not public site URLs.
/// The actual native transport still executes every request and redirect mode.
struct LocalHttp(NativeHttpClient);

impl LocalHttp {
    fn new() -> Self {
        Self(NativeHttpClient::new("transport-regression").unwrap())
    }
}

impl HttpClient for LocalHttp {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        self.0.send(request.with_trusted_endpoint())
    }

    fn validate_url<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { validate_http_url_syntax(url) })
    }
}

fn policy() -> FetchPolicy {
    let setting: SiteSetting = serde_yaml::from_str(r#"
name: Local test
sitename: Local test
domain: 127.0.0.1
top_url: http://127.0.0.1
toc_url: http://127.0.0.1/story
cookie: session=canary
headers:
  Referer: https://example.com/
"#).unwrap();
    FetchPolicy::for_site(&setting)
}

fn request_head(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        assert_ne!(count, 0, "request ended before its head");
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
        assert!(bytes.len() <= 64 * 1024);
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

#[tokio::test]
async fn first_fetch_limits_cookie_scope_but_keeps_noncredential_site_headers() {
    for (host, authenticated) in [("127.0.0.1", true), ("localhost", false)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{host}:{}/image", listener.local_addr().unwrap().port());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let head = request_head(&mut stream);
            assert_eq!(header(&head, "Referer"), Some("https://example.com/"));
            let body = if header(&head, "Cookie") == Some("session=canary") {
                "authenticated"
            } else {
                "anonymous"
            };
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
            ).as_bytes()).unwrap();
        });
        let http = LocalHttp::new();
        let response = fetch_bytes(&http, &FakeRateLimiter::new(), &url, &policy()).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.body, if authenticated { b"authenticated".as_slice() } else { b"anonymous".as_slice() });
    }
}

#[tokio::test]
async fn redirect_resolution_strips_credentials_and_never_restores_them_on_return() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let original = format!("http://127.0.0.1:{port}/start");
    let expected = format!("http://127.0.0.1:{port}/finish");
    let server = thread::spawn(move || {
        for hop in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let head = request_head(&mut stream);
            if hop == 0 {
                assert_eq!(header(&head, "Cookie"), Some("session=canary"));
                assert_eq!(header(&head, "Authorization"), Some("Bearer secret"));
            } else {
                assert!(header(&head, "Cookie").is_none(), "{head}");
                assert!(header(&head, "Authorization").is_none(), "{head}");
            }
            let response = match hop {
                0 => format!("HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/between\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                1 => format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{port}/finish\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                _ => "HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nanonymous".to_string(),
            };
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    let http = LocalHttp::new();
    let limiter = FakeRateLimiter::new();
    let (final_url, reusable) = resolve_final_url_with_body(
        &http, &limiter, &original, &policy().with_header("Authorization", "Bearer secret"),
    ).await.unwrap();
    server.join().unwrap();
    assert_eq!(final_url, expected);
    assert!(reusable.is_none(), "redirected bodies must not be reused with a different cookie scope");
    assert_eq!(limiter.acquisition_count(), 3);
}

#[tokio::test]
async fn only_an_immediate_content_response_is_reusable_and_304_is_terminal() {
    for status in [200, 304] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/story", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            request_head(&mut stream);
            let response = if status == 200 {
                "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncontent"
            } else {
                // A Location on 304 is not a redirect and must not be sent.
                "HTTP/1.1 304 Not Modified\r\nLocation: http://127.0.0.1:1/unused\r\nConnection: close\r\n\r\n"
            };
            stream.write_all(response.as_bytes()).unwrap();
        });
        let http = LocalHttp::new();
        let limiter = FakeRateLimiter::new();
        let (final_url, reusable) = resolve_final_url_with_body(&http, &limiter, &url, &policy()).await.unwrap();
        server.join().unwrap();
        assert_eq!(final_url, url);
        assert_eq!(limiter.acquisition_count(), 1);
        if status == 200 {
            assert_eq!(reusable.unwrap().body, b"content");
        } else {
            assert!(reusable.is_none());
        }
    }
}

#[tokio::test]
async fn opt_in_session_survives_form_post_but_not_paths_origins_or_expiration() {
    use narou_rs::downloader::http_policy::fetch_form_text;
    let setting: SiteSetting = serde_yaml::from_str(r#"
name: Form session
sitename: Form session
domain: 127.0.0.1
top_url: http://127.0.0.1
toc_url: http://127.0.0.1/reader
cookie_session: true
"#).unwrap();
    let policy = FetchPolicy::for_site(&setting);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for hop in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let head = request_head(&mut stream);
            let cookie = header(&head, "Cookie");
            let (status, body, extra) = match hop {
                0 => (200, "reader", "Set-Cookie: csrf-session=canary; Path=/reader; HttpOnly\r\n"),
                1 | 3 => (200, if cookie.is_none() { "anonymous" } else { "leaked" }, ""),
                _ => if head.starts_with("POST /reader/body ")
                    && cookie == Some("csrf-session=canary")
                    && header(&head, "X-CSRF-TOKEN") == Some("token")
                {
                    (200, "novel-text", "Set-Cookie: csrf-session=; Path=/reader; Max-Age=0\r\n")
                } else {
                    (419, "denied", "")
                },
            };
            stream.write_all(format!(
                "HTTP/1.1 {status} OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
            ).as_bytes()).unwrap();
        }
    });
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let other_url = format!("http://{}/reader/private", other.local_addr().unwrap());
    let foreign = thread::spawn(move || {
        let (mut stream, _) = other.accept().unwrap();
        let head = request_head(&mut stream);
        let body = if header(&head, "Cookie").is_none() { "anonymous" } else { "leaked" };
        stream.write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
        ).as_bytes()).unwrap();
    });
    let http = LocalHttp::new();
    let limiter = FakeRateLimiter::new();
    fetch_bytes(&http, &limiter, &format!("{root}/reader/start"), &policy).await.unwrap();
    let outside = fetch_bytes(&http, &limiter, &format!("{root}/outside"), &policy).await.unwrap();
    assert_eq!(outside.body, b"anonymous");
    let outside = fetch_bytes(&http, &limiter, &other_url, &policy).await.unwrap();
    assert_eq!(outside.body, b"anonymous");
    let text = fetch_form_text(&http, &limiter, &format!("{root}/reader/body"), "episode=1".into(),
        vec![("X-CSRF-TOKEN".into(), "token".into())], &policy, Some("UTF-8")).await.unwrap();
    assert_eq!(text, "novel-text");
    let expired = fetch_bytes(&http, &limiter, &format!("{root}/reader/check"), &policy).await.unwrap();
    assert_eq!(expired.body, b"anonymous");
    server.join().unwrap();
    foreign.join().unwrap();
}
