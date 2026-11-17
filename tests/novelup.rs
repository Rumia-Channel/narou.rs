//! novelup.plus definition: work/episode URLs, TOC pagination and chapters,
//! the short story's blank display number, body/foreword/afterword extraction
//! with illustration scope, the work metadata, the author's work list (every
//! page), the two-digit-year dates and a user's own definition overriding the
//! bundled rules.

#![cfg(feature = "native-runtime")]

use std::collections::HashMap;

use narou_rs::application::download_targets::DownloadTargetExpander;
use narou_rs::downloader::novel_info::NovelInfo;
use narou_rs::downloader::preprocess::PreprocessJobs;
use narou_rs::downloader::section::parse_section_html;
use narou_rs::downloader::site_setting::{SiteSetting, merge_user_definition_yaml};
use narou_rs::downloader::toc::{MultipageTocRequest, parse_subtitles, parse_subtitles_multipage};
use narou_rs::downloader::util::{
    PretreatmentRequest, compile_html_pattern, pretreatment_source_with_jobs,
};
use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};

#[allow(dead_code)]
#[path = "fixtures/novelup.rs"]
mod fixtures;

use fixtures::*;

const BUNDLED: &str = include_str!("../webnovel/novelup.plus.yaml");
const AUTHOR_PROFILE_URL: &str = "https://novelup.plus/user/813181327/profile";
const AUTHOR_WORKS_URL: &str = "https://novelup.plus/user/813181327/story";
/// The site's pager links the next page with `?sort=1&p=N` (the HTML escapes
/// the `&` as `&amp;`).
const AUTHOR_WORKS_PAGE2_URL: &str = "https://novelup.plus/user/813181327/story?sort=1&p=2";

fn site() -> SiteSetting {
    SiteSetting::load_bundled(&[BUNDLED]).unwrap().remove(0)
}

/// Run the definition's `preprocess:` over a fetched page.
async fn preprocess(setting: &SiteSetting, source: &str) -> String {
    let mut source = source.to_string();
    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let policy = narou_rs::downloader::http_policy::FetchPolicy::for_site(setting);
    let mut jobs = PreprocessJobs::new();
    pretreatment_source_with_jobs(PretreatmentRequest {
        http: &http,
        rate_limiter: &limiter,
        policy: &policy,
        src: &mut source,
        encoding: setting.encoding(),
        setting: Some(setting),
        jobs: &mut jobs,
        url: "https://novelup.plus/story/759726165",
    })
    .await
    .unwrap();
    source
}

#[test]
fn the_definition_recognizes_work_and_author_urls() {
    let setting = site();

    // 作品 URL と話 URL の両方。
    assert!(setting.matches_url("https://novelup.plus/story/759726165"));
    assert!(setting.matches_url("https://novelup.plus/story/759726165/907418396"));
    assert!(!setting.matches_url("https://novelup.plus/user/813181327/profile"));

    // 作者ページ (プロフィール頁と作品一覧頁)。
    assert!(setting.matches_author_url(AUTHOR_PROFILE_URL));
    assert!(setting.matches_author_url(AUTHOR_WORKS_URL));
    assert!(!setting.matches_author_url("https://novelup.plus/story/759726165"));

    let captures = setting
        .extract_author_url_captures(AUTHOR_PROFILE_URL)
        .expect("the author id");
    assert_eq!(captures.get("author_id").map(String::as_str), Some("813181327"));
    assert_eq!(setting.author_fetch_url(AUTHOR_PROFILE_URL), AUTHOR_WORKS_URL);
}

#[tokio::test]
async fn the_toc_reads_chapters_pagination_and_the_work_metadata() {
    let setting = site();
    let source = preprocess(&setting, TOC_PAGE1).await;

    // 100 話ごとの頁送り: 最後の頁へのリンクから総頁数を読む。
    let page_max = setting
        .toc_page_max_pattern()
        .expect("toc_page_max")
        .captures(&source)
        .expect("the last page link")
        .get(1)
        .expect("the page count")
        .as_str()
        .to_string();
    assert_eq!(page_max, "2");

    // 現在の頁の次が次の頁。
    let next = setting
        .next_toc_pattern()
        .expect("next_toc")
        .captures(&source)
        .expect("the next page link");
    assert_eq!(next.name("next_page").unwrap().as_str(), "2");
    assert_eq!(
        next.name("next_url").unwrap().as_str(),
        "https://novelup.plus/story/759726165?p=2"
    );
    let last_page = preprocess(&setting, TOC_PAGE2).await;
    assert!(
        setting
            .next_toc_pattern()
            .expect("next_toc")
            .captures(&last_page)
            .is_none(),
        "the last page has no next link"
    );

    let subtitles = parse_subtitles(&setting, &source, &HashMap::new()).unwrap();
    assert_eq!(subtitles.len(), 2);
    assert_eq!(subtitles[0].index, "001");
    assert_eq!(subtitles[0].chapter, "第一部");
    assert_eq!(subtitles[0].subtitle, "心臓に悪い");
    assert_eq!(
        subtitles[0].href,
        "https://novelup.plus/story/759726165/907418396"
    );
    assert_eq!(subtitles[0].subdate, "24/5/29 7:36");
    assert_eq!(subtitles[1].chapter, "", "章は前の話から引き継がない");
    assert_eq!(subtitles[1].index, "002");
    assert_eq!(subtitles[1].subtitle, "可愛いんだかなんだか……");
    assert_eq!(subtitles[1].subdate, "26/11/16 8:15");

    let info = NovelInfo::from_novel_info_source(&setting, &source).await;
    assert_eq!(info.title.as_deref(), Some("白衣のすきま日常録"));
    assert_eq!(info.author.as_deref(), Some("灯里"));
    assert_eq!(info.novel_type, Some(1), "連載中 is a serial novel");
    assert_eq!(info.end, Some(false));
    assert_eq!(info.length, Some(46844));
    assert_eq!(info.tags.as_deref(), Some("医療\nホラー"));
    assert_eq!(
        info.raw_captures.get("ga").map(String::as_str),
        Some("2"),
        "the episode count"
    );
    let firstup = info.general_firstup.expect("the first publication date");
    assert_eq!(
        firstup.with_timezone(&chrono_tz::Asia::Tokyo).format("%Y-%m-%d %H:%M").to_string(),
        "2024-05-29 07:34"
    );
    let lastup = info.general_lastup.expect("the last update date");
    assert_eq!(
        lastup.with_timezone(&chrono_tz::Asia::Tokyo).format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-16 08:15"
    );
    let story = info.story.expect("the synopsis");
    assert!(
        story.contains("医療現場のドタバタ、しんみり、ホラー<br>二行目"),
        "story: {story}"
    );
}

#[tokio::test]
async fn the_toc_walk_reads_every_hundred_episode_page() {
    let setting = site();
    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let toc_url = "https://novelup.plus/story/759726165";
    http.add_text(toc_url, 200, TOC_PAGE1);
    http.add_text("https://novelup.plus/story/759726165?p=2", 200, TOC_PAGE2);

    let first_page = preprocess(&setting, TOC_PAGE1).await;
    let mut jobs = PreprocessJobs::new();
    let mut captures = HashMap::new();
    captures.insert("ncode".to_string(), "759726165".to_string());
    let listing = parse_subtitles_multipage(MultipageTocRequest {
        http: &http,
        rate_limiter: &limiter,
        setting: &setting,
        toc_source: &first_page,
        url_captures: &captures,
        title: "白衣のすきま日常録",
        progress: None,
        jobs: &mut jobs,
    })
    .await
    .unwrap();

    assert_eq!(
        listing.subtitles.len(),
        3,
        "both pages are read: {:?}",
        listing
            .subtitles
            .iter()
            .map(|subtitle| subtitle.subtitle.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(listing.subtitles[2].index, "003");
    assert_eq!(
        listing.subtitles[2].href,
        "https://novelup.plus/story/759726165/122387897"
    );
    assert_eq!(http.request_count(), 1, "the second page is fetched");
    assert!(
        http.requested_urls()
            .iter()
            .any(|url| url.ends_with("https://novelup.plus/story/759726165?p=2")),
        "the second page is requested: {:?}",
        http.requested_urls()
    );
}

#[tokio::test]
async fn a_short_story_uses_its_episode_id_when_the_display_number_is_empty() {
    let setting = site();
    let source = preprocess(&setting, SHORT_TOC).await;

    let subtitles = parse_subtitles(&setting, &source, &HashMap::new()).unwrap();
    assert_eq!(subtitles.len(), 1);
    assert_eq!(
        subtitles[0].index, "766439560",
        "an empty data-number falls back to the episode id"
    );
    assert_eq!(
        subtitles[0].href,
        "https://novelup.plus/story/874968302/766439560"
    );
    assert_eq!(subtitles[0].subtitle, "心");

    let info = NovelInfo::from_novel_info_source(&setting, &source).await;
    assert_eq!(info.title.as_deref(), Some("自惚れ"));
    assert_eq!(info.author.as_deref(), Some("ぴろわんこ"));
    assert_eq!(info.novel_type, Some(1));
    assert_eq!(info.end, Some(true), "完結済 ends the novel");
}

#[tokio::test]
async fn the_body_keeps_paragraph_breaks_and_only_body_illustrations() {
    let setting = site();
    let source = preprocess(&setting, EPISODE).await;
    let (section, _raw) = parse_section_html(&setting, source).unwrap();

    assert!(
        section.body.contains("本文の一行目<br><br>本文の二行目"),
        "body: {}",
        section.body
    );
    assert!(
        section.body.contains("<ruby><rb>漢字</rb>"),
        "ruby stays in the body: {}",
        section.body
    );
    assert!(
        !section.body.contains('\n'),
        "the body is one marked-up line: {:?}",
        section.body
    );
    assert!(
        section
            .introduction
            .contains("前書きの一行目<br>前書きの二行目"),
        "introduction: {}",
        section.introduction
    );
    assert!(
        section
            .postscript
            .contains("後書きの一行目<br>後書きの二行目"),
        "postscript: {}",
        section.postscript
    );

    // 挿絵は本文の中だけを見る (コメント欄のスタンプは拾わない)。
    let pattern = compile_html_pattern(setting.illust_grep_pattern.as_deref().unwrap()).unwrap();
    let mut sources = pattern
        .captures_iter(&section.body)
        .map(|caps| caps.name("src").unwrap().as_str().to_string());
    assert_eq!(sources.next().as_deref(), Some("https://novelup.plus/uploads/1/2.jpg"));
    assert_eq!(sources.next(), None, "the body has one illustration");
    assert!(
        !section.body.contains("stamp/8007.png"),
        "the comment stamp is not part of the body"
    );
}

#[tokio::test]
async fn the_two_digit_year_dates_are_read_like_ruby() {
    // 目次の掲載日 (`26/11/16 8:15`) は 2 桁年。
    let parsed =
        narou_rs::downloader::parse_datetime_with_timezone("26/11/16 8:15", Some("Asia/Tokyo"))
            .expect("the episode date");
    assert_eq!(
        parsed.with_timezone(&chrono_tz::Asia::Tokyo).format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-16 08:15"
    );
    let parsed =
        narou_rs::downloader::parse_datetime_with_timezone("24/5/29 7:36", Some("Asia/Tokyo"))
            .expect("the episode date");
    assert_eq!(
        parsed.with_timezone(&chrono_tz::Asia::Tokyo).format("%Y-%m-%d %H:%M").to_string(),
        "2024-05-29 07:36"
    );
    // 4 桁年の日付は今まで通り。
    let parsed =
        narou_rs::downloader::parse_datetime_with_timezone("2024/5/29 7:36", Some("Asia/Tokyo"))
            .expect("the four-digit date");
    assert_eq!(
        parsed.with_timezone(&chrono_tz::Asia::Tokyo).format("%Y-%m-%d %H:%M").to_string(),
        "2024-05-29 07:36"
    );
}

#[tokio::test]
async fn the_author_work_list_is_read_from_every_page() {
    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let settings = vec![site()];
    http.add_text(AUTHOR_PROFILE_URL, 200, AUTHOR_PROFILE);
    http.add_text(AUTHOR_WORKS_URL, 200, AUTHOR_WORKS_PAGE1);
    http.add_text(AUTHOR_WORKS_PAGE2_URL, 200, AUTHOR_WORKS_PAGE2);

    let expander = DownloadTargetExpander::new(&http, &limiter, &settings);
    assert!(
        expander.recognizes(AUTHOR_PROFILE_URL),
        "the author page must be recognized"
    );

    let works = expander.author_works(AUTHOR_PROFILE_URL).await.unwrap();
    assert_eq!(
        works,
        vec![
            "https://novelup.plus/story/759726165".to_string(),
            "https://novelup.plus/story/590182848".to_string(),
            "https://novelup.plus/story/439593115".to_string(),
        ],
        "the listing must not stop at the first page"
    );
    assert_eq!(
        http.request_count(),
        2,
        "both work-list pages are fetched"
    );
    assert!(
        http.requested_urls()
            .iter()
            .any(|url| url.ends_with(AUTHOR_WORKS_PAGE2_URL)),
        "the second page is requested: {:?}",
        http.requested_urls()
    );
    assert!(
        !http
            .requested_urls()
            .iter()
            .any(|url| url.ends_with(AUTHOR_PROFILE_URL)),
        "the profile page only carries a part of the works: {:?}",
        http.requested_urls()
    );
}

#[tokio::test]
async fn an_empty_author_page_does_not_hide_later_works() {
    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    let settings = vec![site()];
    let empty_page = AUTHOR_WORKS_PAGE1.replace("class=\"story_name\"", "class=\"omitted_story\"");
    http.add_text(AUTHOR_WORKS_URL, 200, &empty_page);
    http.add_text(AUTHOR_WORKS_PAGE2_URL, 200, AUTHOR_WORKS_PAGE2);

    let works = DownloadTargetExpander::new(&http, &limiter, &settings)
        .author_works(AUTHOR_PROFILE_URL).await.unwrap();
    assert_eq!(works, ["https://novelup.plus/story/439593115"]);
}

#[tokio::test]
async fn a_user_edited_author_dsl_controls_which_works_are_discovered() {
    let user = format!(r#"
name: ノベルアップ＋
version: {}
preprocess: |-
  guard "FilteredAuthorDsl2026"
  let works = extract_all(/<p class="story_name">\s*<a[^>]*href="(https:\/\/novelup\.plus\/story\/\d+)"/)
  emit "FilteredAuthorDsl2026"
  for work_url in works
    if work_url == "https://novelup.plus/story/590182848"
      emit "novelup_author_novel::${{work_url}}"
    end
  end
  emit ""
  insert_at_match
"#, site().version);
    let merged = merge_user_definition_yaml(BUNDLED, &user);
    let settings = SiteSetting::load_bundled(&[merged.as_str()]).unwrap();
    let http = MockHttpClient::new();
    let limiter = FakeRateLimiter::new();
    http.add_text(AUTHOR_WORKS_URL, 200, AUTHOR_WORKS_PAGE1);

    let works = DownloadTargetExpander::new(&http, &limiter, &settings)
        .author_works(AUTHOR_PROFILE_URL).await.unwrap();
    assert_eq!(works, ["https://novelup.plus/story/590182848"]);
}

#[tokio::test]
async fn a_user_edited_definition_overrides_the_bundled_rules() {
    // ユーザーが自分の YAML を編集して、間隔と本文の取出方を変える。
    let user = format!(r#"
name: ノベルアップ＋
version: {}
min_interval: 45
body_pattern: '<p id="episode_content">(?<body>.+?)</p>'
"#, site().version);
    let merged = merge_user_definition_yaml(BUNDLED, &user);
    let setting = SiteSetting::load_bundled(&[merged.as_str()]).unwrap().remove(0);

    assert_eq!(
        setting.min_interval,
        Some(45.0),
        "the user's interval wins"
    );

    // 本文の取出はユーザーの規則 (生の <p>) になる。
    let source = preprocess(&setting, EPISODE).await;
    let (section, _raw) = parse_section_html(&setting, source).unwrap();
    assert!(
        section.body.contains("本文の一行目\n\n本文の二行目"),
        "body: {:?}",
        section.body
    );
    assert!(
        !section.body.contains("<br>"),
        "the user's body pattern keeps the raw newlines: {:?}",
        section.body
    );

    // 使わなかった規則は bundle のまま有効。
    let toc = preprocess(&setting, TOC_PAGE1).await;
    let subtitles = parse_subtitles(&setting, &toc, &HashMap::new()).unwrap();
    assert_eq!(subtitles.len(), 2);
    assert_eq!(subtitles[0].subtitle, "心臓に悪い");
    assert_eq!(subtitles[0].index, "001");
}
