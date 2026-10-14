use std::sync::Arc;

use narou_rs::application::{
    AutoUpdateSchedule, JobKind, JobRequest, JobService, JobTarget, Schedule, SchedulerService,
};
use narou_rs::platform::SystemClock;

#[test]
fn public_application_services_plan_targetless_auto_update() {
    let result = JobService.plan(&JobRequest {
        kind: JobKind::AutoUpdate,
        targets: Vec::new(),
        options: Vec::new(),
    });

    assert_eq!(result.invalid, Vec::<String>::new());
    assert_eq!(result.plans.len(), 1);
    assert_eq!(result.plans[0].target, JobTarget::All);
}

#[test]
fn scheduler_policy_is_composed_without_web_or_native_types() {
    let scheduler = SchedulerService::new(Arc::new(SystemClock));
    let policy = scheduler.policy(
        true,
        scheduler.parse_schedule("0800,1800"),
        vec!["modified".to_string()],
    );

    assert!(policy.enabled);
    assert!(matches!(policy.schedule, AutoUpdateSchedule::Daily(Schedule { times }) if times == vec![(8, 0), (18, 0)]));
}

fn series_definition() -> Vec<narou_rs::downloader::site_setting::SiteSetting> {
    narou_rs::downloader::site_setting::SiteSetting::load_bundled(&[r#"
name: Example
domain: example.com
top_url: https://example.com
url: https://example\.com/novels/(?<ncode>\d+)
series_url: https://example\.com/series/\d+
series_item_url: href="(?<href>/novels/\d+(?:\?[^"]*)?)"
sitename: Example
toc_url: https://example.com/novels/\k<ncode>
"#]).unwrap()
}

#[tokio::test]
async fn series_expansion_preserves_page_order_and_decodes_relative_links() {
    use narou_rs::application::download_targets::DownloadTargetExpander;
    use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};

    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let sites = series_definition();
    let target = "https://example.com/series/7";
    http.add_text(target, 200, r#"
<a href="/novels/9?x=1&amp;y=2">first</a>
<a href="/novels/2">second</a>
<a href="/novels/9?x=1&amp;y=2">duplicate</a>
"#);
    let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
    assert_eq!(expander.expand(target).await.unwrap(), Some(vec![
        "https://example.com/novels/9?x=1&y=2".to_string(),
        "https://example.com/novels/2".to_string(),
    ]));
}

#[tokio::test]
async fn series_download_plans_keep_force_options_and_do_not_expand_updates() {
    use narou_rs::application::{JobPlan, download_targets::DownloadTargetExpander};
    use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};

    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let sites = series_definition();
    let target = "https://example.com/series/7";
    http.add_text(target, 200, r#"<a href="/novels/9">a</a><a href="/novels/2">b</a>"#);
    let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
    let plans = expander.expand_plan(&JobPlan {
        kind: JobKind::Download,
        target: JobTarget::Url(target.to_string()),
        options: vec!["--force".to_string()],
    }).await.unwrap().unwrap();
    assert_eq!(plans, vec![
        JobPlan { kind: JobKind::Download, target: JobTarget::Url("https://example.com/novels/9".into()), options: vec!["--force".into()] },
        JobPlan { kind: JobKind::Download, target: JobTarget::Url("https://example.com/novels/2".into()), options: vec!["--force".into()] },
    ]);
    let update = JobPlan { kind: JobKind::Update, target: JobTarget::Url(target.into()), options: vec![] };
    assert_eq!(expander.expand_plan(&update).await.unwrap(), None);
    assert_eq!(http.request_count(), 1);
}

#[tokio::test]
async fn series_fetch_or_extraction_errors_never_become_single_novel_plans() {
    use narou_rs::application::{JobPlan, download_targets::DownloadTargetExpander};
    use narou_rs::error::NarouError;
    use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};

    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let sites = series_definition();
    let target = "https://example.com/series/7";
    let plan = JobPlan { kind: JobKind::Download, target: JobTarget::Url(target.into()), options: vec![] };
    let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
    http.add_text(target, 404, "not found");
    assert!(expander.expand_plan(&plan).await.is_err());
    http.add_text(target, 200, "<p>no novels</p>");
    assert!(matches!(expander.expand_plan(&plan).await, Err(NarouError::NotFound(message)) if message == format!("No novels found in series URL: {target}")));
}

#[tokio::test]
async fn series_expansion_uses_user_yaml_instead_of_site_specific_rules() {
    use narou_rs::application::download_targets::DownloadTargetExpander;
    use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};

    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let sites = narou_rs::downloader::site_setting::SiteSetting::load_bundled(&[r#"
name: Custom
domain: example.com
top_url: https://example.com
url: https://example\.com/book/(?<ncode>\d+)
series_url: https://example\.com/shelf/\d+
series_item_url: data-book="(?<ncode>\d+)"
sitename: Custom
toc_url: https://example.com/book/\k<ncode>
"#]).unwrap();
    http.add_text("https://example.com/shelf/3", 200, r#"<p data-book="42"></p>"#);
    let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
    assert_eq!(expander.expand("https://example.com/shelf/3").await.unwrap(), Some(vec!["https://example.com/book/42".into()]));
    assert_eq!(expander.expand("https://example.com/series/3").await.unwrap(), None);
    assert_eq!(http.request_count(), 1);
}
