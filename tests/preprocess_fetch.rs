use std::collections::HashMap;

use narou_rs::downloader::http_policy::FetchPolicy;
use narou_rs::downloader::preprocess::PreprocessJobs;
use narou_rs::downloader::section::{SectionCache, download_section};
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::toc::parse_subtitles;
use narou_rs::downloader::util::{PretreatmentRequest, pretreatment_source_with_jobs};
use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};

const META: &str = "https://example.com/metadata/1";
const TOC: &str = "https://example.com/series/1";

fn setting() -> SiteSetting {
    SiteSetting::load_bundled(&[r#"
name: JSON Reader
sitename: JSON Reader
domain: example.com
top_url: https://example.com
toc_url: https://example.com/series/1
url: https://example\.com/series/(?<ncode>\d+)
subtitles: 'Episode;(?<index>\d+);(?<href>[^;]+);(?<subtitle>[^;\n]+);'
body_pattern: '<article>(?<body>.+?)</article>'
preprocess: |-
  let data = extract_json(/(.+)/s)
  if url == "https://example.com/series/1"
    let work = fetch_json("https://example.com/metadata/1")
    emit "Episode;1;https://example.com/metadata/1;${work.text};"
  else
    emit "<article>${data.text}</article>"
  end
  insert_at_match
    "#]).unwrap().remove(0)
}

async fn toc(http: &MockHttpClient, setting: &SiteSetting, jobs: &mut PreprocessJobs) -> String {
    let mut source = "{}".to_string();
    rewrite(http, setting, jobs, &mut source).await.unwrap();
    source
}

async fn rewrite(
    http: &MockHttpClient, setting: &SiteSetting,
    jobs: &mut PreprocessJobs, source: &mut String,
) -> narou_rs::error::Result<()> {
    pretreatment_source_with_jobs(PretreatmentRequest {
        http, rate_limiter: &FakeRateLimiter::new(), policy: &FetchPolicy::for_site(setting),
        src: source, encoding: "UTF-8", setting: Some(setting), jobs, url: TOC,
    }).await
}

fn scripted(script: String) -> SiteSetting {
    let mut yaml = serde_yaml::to_value(setting()).unwrap();
    yaml["preprocess"] = serde_yaml::Value::String(script);
    SiteSetting::load_bundled(&[&serde_yaml::to_string(&yaml).unwrap()]).unwrap().remove(0)
}

fn required_setting(statuses: &str) -> SiteSetting {
    scripted(format!(r#"
let work = fetch_json_required("{META}"{statuses})
if work == null
  emit "unavailable"
else
  emit "<article>${{work.text}}</article>"
end
insert_at_match
"#))
}

#[tokio::test]
async fn auxiliary_json_does_not_cross_anonymous_or_account_boundaries() {
    let http = MockHttpClient::new();
    let mut setting = setting();
    let mut jobs = PreprocessJobs::new();
    for (cookie, text) in [(None, "公開"), (Some("session=A"), "アカウントA"),
        (Some("session=B"), "アカウントB"), (None, "再び公開")] {
        setting.cookie = cookie.map(str::to_string);
        http.add_text(META, 200, serde_json::json!({"text":text}).to_string());
        let source = toc(&http, &setting, &mut jobs).await;
        let rows = parse_subtitles(&setting, &source, &HashMap::new()).unwrap();
        assert_eq!(rows[0].subtitle, text);
    }
}

#[tokio::test]
async fn successfully_fetched_toc_metadata_is_reused_for_its_section() {
    let http = MockHttpClient::new();
    let setting = scripted(r#"
if url == "https://example.com/series/1"
  let work = fetch_json("https://example.com/metadata/1")
  emit "Episode;1;https://example.com/metadata/1;${work.text};"
else
  let work = extract_json(/^(\{ "z": 1, "text": "[^"]+" \})$/)
  emit "<article>${work.text}</article>"
end
insert_at_match
"#.to_string());
    let mut jobs = PreprocessJobs::new();
    http.add_text(META, 200, r#"{ "z": 1, "text": "取得できた本文" }"#);
    let source = toc(&http, &setting, &mut jobs).await;
    let subtitle = parse_subtitles(&setting, &source, &HashMap::new()).unwrap().remove(0);
    http.add_text(META, 403, "blocked on a redundant request");
    let (section, _) = download_section(
        &http, &FakeRateLimiter::new(), &mut SectionCache::new(), &setting,
        &subtitle, TOC, &mut jobs,
    ).await.unwrap();
    assert_eq!(section.body, "取得できた本文");
    assert_eq!(http.requested_urls(), vec![format!("GET {META}")]);
}

#[tokio::test]
async fn parsed_sections_do_not_cross_credential_boundaries() {
    let http = MockHttpClient::new();
    let mut setting = setting();
    let mut jobs = PreprocessJobs::new();
    let mut cache = SectionCache::new();
    http.add_text(META, 200, r#"{"text":"公開"}"#);
    let source = toc(&http, &setting, &mut jobs).await;
    let subtitle = parse_subtitles(&setting, &source, &HashMap::new()).unwrap().remove(0);
    for (cookie, text) in [(None, "公開"), (Some("session=A"), "限定A"), (Some("session=B"), "限定B")] {
        setting.cookie = cookie.map(str::to_string);
        http.add_text(META, 200, serde_json::json!({"text":text}).to_string());
        let (section, _) = download_section(
            &http, &FakeRateLimiter::new(), &mut cache, &setting, &subtitle, TOC, &mut jobs,
        ).await.unwrap();
        assert_eq!(section.body, text);
    }
}

#[tokio::test]
async fn required_failure_wins_over_optional_use_in_either_order() {
    let http = MockHttpClient::new();
    http.add_text(META, 403, "not an application denial");
    let calls = [
        format!("let optional = fetch_json(\"{META}\")"),
        format!("let required = fetch_json_required(\"{META}\")"),
    ];
    for order in [[0, 1], [1, 0]] {
        let setting = scripted(format!("{}\n{}\nemit \"partial data\"\ninsert_at_match",
            calls[order[0]], calls[order[1]]));
        let mut source = "{}".to_string();
        let error = rewrite(&http, &setting, &mut PreprocessJobs::new(), &mut source).await.unwrap_err();
        assert!(matches!(&error, narou_rs::error::NarouError::Http(message) if message.contains("403")));
        assert_eq!(source, "{}");
    }
    assert_eq!(http.request_count(), 2);
}

#[tokio::test]
async fn requiring_a_cached_optional_failure_does_not_retry_or_lose_its_status() {
    let http = MockHttpClient::new();
    let mut jobs = PreprocessJobs::new();
    http.add_text(META, 403, "blocked");
    toc(&http, &setting(), &mut jobs).await;
    http.add_text(META, 200, r#"{"text":"must not implicitly retry"}"#);
    for _ in 0..2 {
        let mut source = "{}".to_string();
        let error = rewrite(&http, &required_setting(""), &mut jobs, &mut source).await.unwrap_err();
        assert_eq!(narou_rs::application::jobs::classify_failure(&error),
            narou_rs::application::jobs::JobFailureClass::Retryable);
        assert!(error.to_string().contains("403"));
        assert!(error.to_string().contains(META));
        assert_eq!(source, "{}");
    }
    assert_eq!(http.requested_urls(), vec![format!("GET {META}")]);
}

#[tokio::test]
async fn admitted_missing_dependency_is_not_a_whole_novel_deletion() {
    let http = MockHttpClient::new();
    let mut jobs = PreprocessJobs::new();
    http.add_text(META, 404, "unavailable");
    let mut source = "{}".to_string();
    rewrite(&http, &required_setting(", [404]"), &mut jobs, &mut source).await.unwrap();
    assert!(source.contains("unavailable"));
    let mut source = "{}".to_string();
    let error = rewrite(&http, &required_setting(""), &mut jobs, &mut source).await.unwrap_err();
    assert!(matches!(&error, narou_rs::error::NarouError::Http(message) if message.contains("404")));
    assert_eq!(source, "{}");
    assert_eq!(http.requested_urls(), vec![format!("GET {META}")]);
    let mut source = "{}".to_string();
    rewrite(&http, &required_setting(", [404]"), &mut jobs, &mut source).await.unwrap();
    assert!(source.contains("unavailable"));
    assert_eq!(http.requested_urls(), vec![format!("GET {META}")]);
}

#[tokio::test]
async fn required_invalid_or_null_json_cannot_publish_a_partial_rewrite() {
    let http = MockHttpClient::new();
    for body in ["not JSON", "null"] {
        http.add_text(META, 200, body);
        let mut source = "{}".to_string();
        let error = rewrite(&http, &required_setting(""), &mut PreprocessJobs::new(), &mut source).await.unwrap_err();
        assert!(matches!(error, narou_rs::error::NarouError::Http(_)));
        assert_eq!(source, "{}");
    }
}

#[tokio::test]
async fn later_pages_still_resolve_after_the_json_cache_fills() {
    let setting = scripted(r#"
let data = extract_json(/(.+)/s)
let work = fetch_json_required("https://example.com/metadata/${data.id}")
emit "<article>${work.text}</article>"
insert_at_match
"#.to_string());
    let http = MockHttpClient::new();
    let mut jobs = PreprocessJobs::new();
    for index in 0..narou_rs::downloader::preprocess::MAX_PREPROCESS_JOBS + 2 {
        http.add_text(format!("https://example.com/metadata/{index}"), 200,
            serde_json::json!({"text":format!("第{index}話")}).to_string());
        let mut source = serde_json::json!({"id":index}).to_string();
        rewrite(&http, &setting, &mut jobs, &mut source).await.unwrap();
        assert!(source.contains(&format!("<article>第{index}話</article>")));
    }
}

#[tokio::test]
async fn unresolved_required_rounds_do_not_publish_an_incomplete_document() {
    let setting = scripted(r#"
let current = extract_json(/(.+)/s)
for round in [1, 2, 3, 4, 5]
  if current.next
    let current = fetch_json_required("${current.next}")
  end
end
emit "partial document"
insert_at_match
"#.to_string());
    let http = MockHttpClient::new();
    for index in 1..=5 {
        http.add_text(format!("https://example.com/metadata/{index}"), 200,
            serde_json::json!({"next":format!("https://example.com/metadata/{}", index + 1)}).to_string());
    }
    let original = r#"{"next":"https://example.com/metadata/1"}"#;
    let mut source = original.to_string();
    let error = rewrite(&http, &setting, &mut PreprocessJobs::new(), &mut source).await.unwrap_err();
    assert!(matches!(error, narou_rs::error::NarouError::SiteSetting(_)));
    assert_eq!(source, original);
}

#[tokio::test]
async fn private_animation_access_uses_media_responses_not_a_null_original_or_cookie_alone() {
    let mut site = SiteSetting::load_bundled(&[include_str!("../webnovel/www.pixiv.net.yaml")]).unwrap().remove(0);
    let http = MockHttpClient::new();
    let mut jobs = PreprocessJobs::new();
    let url = "https://www.pixiv.net/ajax/series/91?p=1&lang=ja";
    let metadata = "https://www.pixiv.net/ajax/illust/101";
    let pages = "https://www.pixiv.net/ajax/illust/101/pages";
    let animation = "https://www.pixiv.net/ajax/illust/101/ugoira_meta?lang=ja";
    let original = serde_json::json!({"error":false,"body":{
        "illustSeries":[{"id":"91","title":"動画シリーズ","total":1}],
        "page":{"series":[{"workId":"101","order":1}]},
    }}).to_string();
    http.add_text(metadata, 200, serde_json::json!({"error":false,"body":{
        "id":"101","title":"限定動画","illustTitle":"限定動画","userName":"作者",
        "illustType":2,"restrict":2,"xRestrict":0,"urls":{"original":null},
        "uploadDate":"2026-10-18T00:00:00Z","seriesNavData":{"seriesId":"91"},
    }}).to_string());
    http.add_text(pages, 200, r#"{"error":false,"body":[{"urls":{"original":null}}]}"#);
    for (cookie, allowed) in [(None, false), (Some("session=member"), true), (Some("session=other"), false)] {
        site.cookie = cookie.map(str::to_string);
        http.add_text(animation, 200, if allowed {
            r#"{"error":false,"body":{"originalSrc":"https://i.pximg.net/frames.zip","frames":[{"file":"000000.png","delay":120},{"file":"000001.png","delay":80}]}}"#
        } else {
            r#"{"error":true,"message":"この作品はマイピクにのみ公開されています","body":[]}"#
        });
        let mut source = original.clone();
        pretreatment_source_with_jobs(PretreatmentRequest {
            http: &http, rate_limiter: &FakeRateLimiter::new(),
            policy: &FetchPolicy::for_site(&site), src: &mut source,
            encoding: "UTF-8", setting: Some(&site), jobs: &mut jobs, url,
        }).await.unwrap();
        let rows = parse_subtitles(&site, &source, &HashMap::new()).unwrap();
        if allowed {
            assert!(!site.is_partial_login_view(&source));
            let (section, _) = download_section(
                &http, &FakeRateLimiter::new(), &mut SectionCache::new(),
                &site, &rows[0], url, &mut jobs,
            ).await.unwrap();
            assert_eq!(section.body, "<img src=\"https://i.pximg.net/frames.zip?ugoira=120,80\">");
        } else {
            assert!(rows.is_empty());
            assert!(site.is_partial_login_view(&source));
        }
    }
    let media_headers = http.sent_requests().into_iter()
        .filter(|(url, _)| url.ends_with("/pages") || url.contains("/ugoira_meta"))
        .map(|(_, headers)| headers.into_iter().find(|(name, _)| name.eq_ignore_ascii_case("Cookie")).unwrap().1)
        .collect::<Vec<_>>();
    assert_eq!(media_headers, vec!["session=member", "session=member", "session=other", "session=other"]);
}
