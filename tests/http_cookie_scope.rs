use narou_rs::downloader::http_policy::{FetchPolicy, fetch_bytes, resolve_final_url_with_body};
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};
use narou_rs::platform::HttpResponse;

fn setting() -> SiteSetting {
    serde_yaml::from_str(r#"
name: Example
domain: example.com
top_url: https://example.com
toc_url: https://example.com/story
sitename: Example
cookie: session=canary
headers:
  Referer: https://example.com/
"#).unwrap()
}

#[test]
fn first_request_limits_cookie_to_its_site_but_keeps_image_headers() {
    let http = MockHttpClient::new();
    let policy = FetchPolicy::for_site(&setting());
    for (url, expected_cookie) in [
        ("https://example.com/story", true),
        ("https://img.example.com/image.png", true),
        ("https://collector.example.net/image.png", false),
        ("https://example.com.attacker.test/image.png", false),
    ] {
        http.add_text(url, 200, "image");
        futures::executor::block_on(fetch_bytes(&http, &FakeRateLimiter::new(), url, &policy)).unwrap();
        let requests = http.sent_requests();
        let headers = &requests.last().unwrap().1;
        assert_eq!(headers.iter().any(|(name, value)| name.eq_ignore_ascii_case("cookie") && value == "session=canary"), expected_cookie, "destination: {url}");
        assert!(headers.iter().any(|(name, value)| name == "Referer" && value == "https://example.com/"));
    }
}

#[test]
fn direct_redirect_resolution_does_not_leak_a_site_cookie_to_an_image_host() {
    let http = MockHttpClient::new();
    let image = "https://collector.example.net/image.png";
    http.add_response(image, HttpResponse {
        status: 302,
        headers: vec![("Location".into(), "https://example.com/image.png".into())],
        body: Vec::new(),
    });
    http.add_text("https://example.com/image.png", 200, "image");
    futures::executor::block_on(resolve_final_url_with_body(&http, &FakeRateLimiter::new(), image, &FetchPolicy::for_site(&setting()))).unwrap();
    for (url, headers) in http.sent_requests() {
        assert!(!headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("cookie")), "cookie leaked on {url}");
    }
}
