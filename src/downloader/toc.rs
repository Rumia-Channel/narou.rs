use std::collections::{HashMap, HashSet};

use crate::error::{NarouError, Result};
use crate::platform::{HttpClient, PlatformFuture, ProgressReporter, RateLimiter};

use super::html::{delete_ruby_tag, restore_entities, slim_subtitle};
use super::http_policy;
use super::novel_info::NovelInfo;
use super::preprocess::PreprocessJobs;
use super::site_setting::SiteSetting;
use super::types::SubtitleInfo;
use super::util::{
    load_length_limit, pretreatment_source_with_jobs, ruby_filename_special_chars,
    sanitize_filename_with_limit,
};

pub async fn fetch_toc(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    setting: &SiteSetting,
    toc_url: &str,
    jobs: &mut PreprocessJobs,
) -> Result<String> {
    let policy = crate::downloader::http_policy::FetchPolicy::for_site(setting);
    let mut body = http_policy::fetch_text(
        http,
        rate_limiter,
        toc_url,
        &policy,
        Some(setting.encoding()),
    )
    .await?;
    pretreatment_source_with_jobs(super::util::PretreatmentRequest {
        http,
        rate_limiter,
        policy: &policy,
        src: &mut body,
        encoding: setting.encoding(),
        setting: Some(setting),
        jobs,
        url: toc_url,
    })
    .await?;

    if let Some(re) = setting.compiled_error_message_pattern()
        && re.is_match(&body)
    {
        return Err(NarouError::NotFound("Novel deleted or private".into()));
    }

    Ok(body)
}

pub fn parse_subtitles(
    setting: &SiteSetting,
    toc_source: &str,
    url_captures: &HashMap<String, String>,
) -> Result<Vec<SubtitleInfo>> {
    let subtitles_pattern = setting
        .subtitles_pattern()
        .ok_or_else(|| NarouError::SiteSetting("No subtitles pattern defined".into()))?;
    let filename_length_limit = load_length_limit("filename-length-limit", Some(50));

    let mut subtitles = Vec::new();
    let mut remaining = toc_source;

    while let Some(caps) = subtitles_pattern.captures(remaining) {
        let full_match = caps.get(0).unwrap();
        remaining = &remaining[full_match.end()..];

        let index = caps
            .name("index")
            .map(|m| m.as_str().trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| caps.name("episode_id").map(|m| m.as_str().to_string()))
            .unwrap_or_default();
        let href = caps
            .name("href")
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| {
                if let Some(href_tpl) = &setting.href {
                    setting.interpolate_subtitles_href(href_tpl, &index, url_captures)
                } else {
                    String::new()
                }
            });
        // narou.rb は TOC ソース全体を `Helper.restore_entity` で復号してから
        // multi_match する。ここではキャプチャ値だけ同じ順序の復号を掛ける
        // (本文側の前処理は実体を残す必要があるため、ソース全体は変更しない)。
        // 汎用の `decode_html_text` は &amp; を先に復号するため &amp;quot; が
        // `"` に化けるなど Ruby と差が出るので使わない。
        let chapter = caps
            .name("chapter")
            .map(|m| restore_entities(m.as_str()))
            .unwrap_or_default();
        let subchapter = caps
            .name("subchapter")
            .map(|m| restore_entities(m.as_str()))
            .unwrap_or_default();
        let subtitle_raw = caps
            .name("subtitle")
            .map(|m| restore_entities(m.as_str()))
            .unwrap_or_default();
        let subdate = caps
            .name("subdate")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let subupdate = caps.name("subupdate").map(|m| m.as_str().to_string());
        let subdate = if subdate.is_empty() {
            subupdate.clone().unwrap_or_default()
        } else {
            subdate
        };

        // narou.rb 準拠: 永続化する subtitle は ruby タグと改行を除去した
        // クリーンな文字列にする。file_subtitle は `title_to_filename` と
        // 同じ順番で、ruby タグ落とし → 特殊文字の全角化 → 長さ制限 →
        // Windows ファイル名用サニタイズを行う。
        let subtitle_clean = slim_subtitle(&subtitle_raw);
        let subtitle_for_filename =
            ruby_filename_special_chars(&delete_ruby_tag(&subtitle_raw));

        let file_subtitle = match filename_length_limit {
            Some(limit) => {
                let reserved = index.chars().count() + 1;
                let subtitle_limit = limit.saturating_sub(reserved);
                sanitize_filename_with_limit(&subtitle_for_filename, Some(subtitle_limit))
            }
            None => sanitize_filename_with_limit(&subtitle_for_filename, None),
        };

        subtitles.push(SubtitleInfo {
            index,
            href,
            chapter,
            subchapter,
            subtitle: subtitle_clean,
            file_subtitle,
            subdate,
            subupdate,
            download_time: None,
            download_revision: 0,
        });

        if full_match.end() == 0 {
            break;
        }
    }

    Ok(subtitles)
}

/// Inputs shared by every page of a multipage TOC walk: the first page's
/// source and captures plus the platform handles used to fetch follow-ups.
pub struct MultipageTocRequest<'a> {
    pub http: &'a dyn HttpClient,
    pub rate_limiter: &'a dyn RateLimiter,
    pub setting: &'a SiteSetting,
    pub toc_source: &'a str,
    pub url_captures: &'a HashMap<String, String>,
    pub title: &'a str,
    pub progress: Option<&'a dyn ProgressReporter>,
    pub jobs: &'a mut PreprocessJobs,
}

pub struct TocListing {
    pub subtitles: Vec<SubtitleInfo>,
    pub partial: bool,
    /// Site tag captures from every visited TOC page, before normalization.
    pub tag_sources: Vec<String>,
    /// The walk stopped at a login wall, so the listing is incomplete.
    ///
    /// A page that was served but asks for authentication is not an error of
    /// its own: the caller retries the walk with another stored credential
    /// instead of storing the truncated listing.
    pub wall: bool,
}

pub async fn parse_subtitles_multipage(
    request: MultipageTocRequest<'_>,
) -> Result<TocListing> {
    parse_subtitles_multipage_with(request, |http, rate_limiter, setting, url, jobs| {
        Box::pin(async move { fetch_toc(http, rate_limiter, setting, &url, jobs).await })
    })
    .await
}

fn parse_subtitles_multipage_with<'a, F>(
    request: MultipageTocRequest<'a>,
    mut fetch_next_toc: F,
) -> PlatformFuture<'a, Result<TocListing>>
where
    F: for<'b> FnMut(
            &'b dyn HttpClient,
            &'b dyn RateLimiter,
            &'b SiteSetting,
            String,
            &'b mut PreprocessJobs,
        ) -> PlatformFuture<'b, Result<String>>
        + Send
        + 'a,
{
    Box::pin(async move {
        let MultipageTocRequest {
            http,
            rate_limiter,
            setting,
            toc_source,
            url_captures,
            title,
            progress,
            jobs,
        } = request;
        let mut all_subtitles = Vec::new();
        // 本文一覧のページが「未ログインで欠けている」と報告したか。
        let mut partial_listing = false;
        // 途中ページがログイン壁だったか。壁は「続きがある」ことを意味するので、
        // 欠けた一覧を成功として保存しない。
        let mut login_wall = false;
        let mut page_tags = Vec::new();
        let mut visited_pages = HashSet::new();
        visited_pages.insert(setting.get_toc_url_with_captures(url_captures));
        let mut current_toc_source = toc_source.to_string();
        let mut page = 0;
        let (page_limit, total_pages) = if let Some(pattern) = setting.toc_page_max_pattern() {
            let total = pattern
                .captures(&current_toc_source)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .filter(|total| *total > 0);
            (Some(total.unwrap_or(1)), total)
        } else {
            (None, None)
        };
        let show_progress = !title.is_empty() && page_limit.is_none_or(|limit| limit >= 5);
        if show_progress && let Some(progress) = progress {
            progress.set_position_async(0).await;
            progress.set_length_async(total_pages.unwrap_or(0) as u64).await;
            progress.set_message_async(&format!("目次 {}", title)).await;
        }

        loop {
            if setting.is_partial_login_view(&current_toc_source) {
                partial_listing = true;
            }
            let page_subs = parse_subtitles(setting, &current_toc_source, url_captures)?;
            all_subtitles.extend(page_subs);
            if let Some(tags) = setting.resolve_info_pattern("tags", &current_toc_source).await {
                page_tags.push(tags);
            }

            page += 1;
            if show_progress && let Some(progress) = progress {
                progress.inc_async(1).await;
            }
            if page_limit.is_some_and(|limit| page >= limit) {
                break;
            }

            let next_toc_pattern = match setting.next_toc_pattern() {
                Some(p) => p,
                None => break,
            };

            let caps = match next_toc_pattern.captures(&current_toc_source) {
                Some(c) => c,
                None => break,
            };

            let mut next_captures: HashMap<String, String> = HashMap::new();
            for name in next_toc_pattern.capture_names().flatten() {
                if let Some(m) = caps.name(name) {
                    next_captures.insert(name.to_string(), m.as_str().to_string());
                }
            }

            let next_url_val = match &setting.next_url {
                Some(u) => u.clone(),
                None => break,
            };
            let next_url = setting.get_next_url_with_captures(&next_url_val, &next_captures);
            if !visited_pages.insert(next_url.clone()) {
                break;
            }

            current_toc_source =
                fetch_next_toc(http, rate_limiter, setting, next_url, &mut *jobs).await?;
            if setting.is_login_wall(&current_toc_source) {
                // 続きのページがログイン壁だった。壁は「まだ続きがある」ことを
                // 意味するので、欠けた一覧を成功として保存しない。
                login_wall = true;
                break;
            }
        }

        Ok(TocListing {
            subtitles: all_subtitles,
            partial: partial_listing,
            tag_sources: page_tags,
            wall: login_wall,
        })
    })
}

pub async fn create_short_story_subtitles(
    setting: &SiteSetting,
    toc_source: &str,
    info: &NovelInfo,
) -> Result<Vec<SubtitleInfo>> {
    let title = match &info.title {
        Some(title) => title.clone(),
        None => setting.resolve_info_pattern("t", toc_source).await.unwrap_or_else(|| "短編".to_string()),
    };
    let subdate = info.raw_captures.get("gf").cloned().unwrap_or_default();
    let subupdate = info
        .raw_captures
        .get("nu")
        .cloned()
        .or_else(|| info.raw_captures.get("gl").cloned())
        .or_else(|| info.raw_captures.get("gf").cloned());

    // narou.rb の `create_short_story_subtitles` と同じく、title に対しても
    // ruby タグ落としと改行除去を行う。
    let subtitle_clean = slim_subtitle(&title);
    let title_for_filename = ruby_filename_special_chars(&delete_ruby_tag(&title));

    Ok(vec![SubtitleInfo {
        index: "1".to_string(),
        href: String::new(),
        chapter: String::new(),
        subchapter: String::new(),
        subtitle: subtitle_clean,
        file_subtitle: match load_length_limit("filename-length-limit", Some(50)) {
            Some(limit) => {
                let reserved = "1".chars().count() + 1;
                sanitize_filename_with_limit(
                    &title_for_filename,
                    Some(limit.saturating_sub(reserved)),
                )
            }
            None => sanitize_filename_with_limit(&title_for_filename, None),
        },
        subdate,
        subupdate,
        download_time: None,
        download_revision: 0,
    }])
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use crate::platform::mocks::{FakeRateLimiter, MockHttpClient};

    use super::*;

    #[derive(Default)]
    struct MockProgress {
        lengths: Mutex<Vec<u64>>,
        increments: Mutex<Vec<u64>>,
    }

    impl ProgressReporter for MockProgress {
        fn set_length(&self, len: u64) {
            self.lengths.lock().push(len);
        }

        fn set_position(&self, _pos: u64) {}

        fn inc(&self, delta: u64) {
            self.increments.lock().push(delta);
        }

        fn set_message(&self, _msg: &str) {}

        fn finish_with_message(&self, _msg: &str) {}

        fn println(&self, _msg: &str) {}
    }

    #[test]
    fn multipage_toc_uses_existing_progress_for_long_series() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "ncode.syosetu.com")
            .unwrap();
        let toc_source = r#"
<a href="/n1234aa/?p=5" class="c-pager__item c-pager__item--last">5</a>
<div class="p-eplist__sublist">
<a href="/n1234aa/1/" class="p-eplist__subtitle">
第1話
</a>
<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
"#;
        let http = MockHttpClient::new();
        let rate_limiter = FakeRateLimiter::new();
        let progress = MockProgress::default();

        futures::executor::block_on(parse_subtitles_multipage(MultipageTocRequest {
                http: &http,
                rate_limiter: &rate_limiter,
                setting,
                toc_source,
                url_captures: &HashMap::new(),
                title: "テスト作品",
                progress: Some(&progress),
                jobs: &mut PreprocessJobs::new(),
            }))
            .unwrap();

        assert_eq!(*progress.lengths.lock(), vec![5]);
        assert_eq!(*progress.increments.lock(), vec![1]);
    }

    #[test]
    fn toc_captures_decode_entities_and_use_ruby_filename_mapping() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "ncode.syosetu.com").unwrap();
        let toc_source = r#"
<div class="p-eplist__chapter-title">第&amp;章</div>

<div class="p-eplist__sublist">
<a href="/n1234aa/1/" class="p-eplist__subtitle">
プロローグ：なぜか江戸時代へ転移　&lt;C201&gt;
</a>

<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
"#;
        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();
        assert_eq!(subtitles.len(), 1);
        assert_eq!(subtitles[0].subtitle, "プロローグ：なぜか江戸時代へ転移　<C201>");
        assert_eq!(subtitles[0].file_subtitle, "プロローグ：なぜか江戸時代へ転移　〈C201〉");
        assert_eq!(subtitles[0].chapter, "第&章");
    }

    #[test]
    fn toc_capture_entity_restore_follows_ruby_order() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "ncode.syosetu.com").unwrap();
        // Ruby の restore_entity は quot → amp → nbsp → lt → gt ... の順。
        // &amp;quot; は amp の後に quot を再検査しないので `"` ではなく
        // `&quot;` のまま残る (汎用 decode_html_text とここが異なる)。
        let toc_source = r#"
<div class="p-eplist__chapter-title">第&amp;quot;章</div>

<div class="p-eplist__sublist">
<a href="/n1234aa/1/" class="p-eplist__subtitle">
&amp;apos; 始まり
</a>

<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
"#;
        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();
        assert_eq!(subtitles[0].chapter, "第&quot;章");
        assert_eq!(subtitles[0].subtitle, "&apos; 始まり");
        assert_eq!(subtitles[0].file_subtitle, "&apos; 始まり");
    }

    #[test]
    fn unknown_toc_totals_count_pages_without_using_the_traversal_cap() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "www.pixiv.net").unwrap();
        let first_page = "Episode;1;https://www.pixiv.net/ajax/novel/1;2020-01-01T00:00:00+00:00;一話\n\
             next::/ajax/novel/series_content/1?limit=30&last_order=1&order_by=asc\n";
        let second_page = "Episode;2;https://www.pixiv.net/ajax/novel/2;2020-01-01T00:00:00+00:00;二話\n";
        let progress = MockProgress::default();
        let subtitles = futures::executor::block_on(parse_subtitles_multipage_with(
            MultipageTocRequest {
                http: &MockHttpClient::new(),
                rate_limiter: &FakeRateLimiter::new(),
                setting,
                toc_source: first_page,
                url_captures: &HashMap::new(),
                title: "シリーズ",
                progress: Some(&progress),
                jobs: &mut PreprocessJobs::new(),
            },
            |_, _, _, _, _| Box::pin(async { Ok(second_page.to_string()) }),
        )).unwrap().subtitles;
        assert_eq!(subtitles.iter().map(|subtitle| subtitle.index.as_str()).collect::<Vec<_>>(), vec!["1", "2"]);
        assert_eq!(*progress.lengths.lock(), vec![0]);
        assert_eq!(progress.increments.lock().iter().sum::<u64>(), 2);
    }

    #[test]
    fn unknown_pagination_collects_tags_past_fifty_pages_and_stops_on_cycles() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "www.pixiv.net").unwrap();
        for cyclic in [false, true] {
            let make_page = |index: usize| {
                let mut page = format!("Episode;{index};https://www.pixiv.net/ajax/novel/{index};2020-01-01T00:00:00+00:00;話\n\
                    tag::tag{index}\n");
                if index < 51 {
                    let next = if cyclic { 1 } else { index };
                    page.push_str(&format!("next::/ajax/novel/series_content/1?limit=30&last_order={next}&order_by=asc\n"));
                }
                page
            };
            let TocListing { subtitles, tag_sources: tags, .. } = futures::executor::block_on(parse_subtitles_multipage_with(
                MultipageTocRequest {
                    http: &MockHttpClient::new(),
                    rate_limiter: &FakeRateLimiter::new(),
                    setting,
                    toc_source: &make_page(1),
                    url_captures: &HashMap::new(),
                    title: "",
                    progress: None,
                    jobs: &mut PreprocessJobs::new(),
                },
                |_, _, _, next_url, _| {
                    let order = url::Url::parse(&next_url).unwrap().query_pairs()
                        .find(|(key, _)| key == "last_order").unwrap().1.parse::<usize>().unwrap();
                    let page = make_page(order + 1);
                    Box::pin(async move { Ok(page) })
                },
            )).unwrap();
            let last = if cyclic { 2 } else { 51 };
            assert_eq!(subtitles.len(), last);
            assert_eq!(tags.last(), Some(&format!("tag{last}")));
        }
    }

    /// 暁 (`www.akatsuki-novels.com`): 日付を欠いた `<li>` でも `</li>` を越えて
    /// 次の話を飲み込んだり章見出しを取り違えたりせず、日付なしの話を
    /// `subupdate == None` で保持したまま次の話も正しく抽出すること。
    /// また `<a>` の属性順違いや `next_toc` の追加クエリパラメータも許容する。
    #[test]
    fn akatsuki_subtitles_do_not_cross_li_boundaries() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "www.akatsuki-novels.com")
            .unwrap();
        let toc_source = r#"
<ul>
<li class="font-bold">第一章</li>
<li class="items-baseline justify-between"><a href="/stories/view/1/novel_id~10">日付なし話</a><time>no date</time></li>
<li class="items-baseline justify-between"><a class="link" href="/stories/view/2/novel_id~10">正常な話</a><span>2025年10月24日</span></li>
</ul>
<a class="pager" href="/stories/index?foo=1&amp;novel_id=10&amp;page=2" rel="next">次へ</a>
"#;
        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();
        assert_eq!(subtitles.len(), 2);
        assert_eq!(subtitles[0].chapter, "第一章");
        assert_eq!(subtitles[0].index, "1");
        assert_eq!(subtitles[0].subtitle, "日付なし話");
        assert_eq!(subtitles[0].subupdate, None);
        assert_eq!(subtitles[1].chapter, "");
        assert_eq!(subtitles[1].index, "2");
        assert_eq!(subtitles[1].subtitle, "正常な話");
        assert_eq!(subtitles[1].subupdate.as_deref(), Some("2025年10月24日"));

        let caps = setting
            .next_toc_pattern()
            .unwrap()
            .captures(toc_source)
            .unwrap();
        assert_eq!(&caps["next_page"], "2");
        assert_eq!(&caps["ncode"], "10");
    }

    #[test]
    fn multipage_reports_a_partial_listing_from_any_page() {
        // 目次は複数ページに分かれるので、2 ページ目以降の目印も拾う。
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "www.pixiv.net")
            .unwrap();
        assert!(setting.is_partial_login_view("login_partial::1"));
        // 429 を避けるための間隔の下限 (サイト定義由来)。
        assert_eq!(setting.min_interval, Some(5.0));
        let scope = crate::downloader::http_policy::FetchPolicy::for_site(setting);
        assert_eq!(scope.min_interval(), Some(5.0));

        let first_page = "Episode;1;https://www.pixiv.net/ajax/novel/1;2020-01-01T00:00:00+00:00;一話\n\
             next::/ajax/novel/series_content/1?limit=30&last_order=1&order_by=asc\n";
        let second_page = "login_partial::1\n\
             Episode;2;https://www.pixiv.net/ajax/novel/2;2020-01-01T00:00:00+00:00;二話\n";
        let http = MockHttpClient::new();
        let rate_limiter = FakeRateLimiter::new();

        let TocListing { subtitles, partial, .. } = futures::executor::block_on(parse_subtitles_multipage_with(
            MultipageTocRequest {
                http: &http,
                rate_limiter: &rate_limiter,
                setting,
                toc_source: first_page,
                url_captures: &HashMap::new(),
                title: "",
                progress: None,
                jobs: &mut PreprocessJobs::new(),
            },
            {
                let page = second_page.to_string();
                move |_, _, _, _, _| {
                    let page = page.clone();
                    Box::pin(async move { Ok(page) })
                }
            },
        ))
        .unwrap();

        assert_eq!(subtitles.len(), 2);
        assert!(partial, "2 ページ目の目印を拾えていない");
    }

    #[test]
    fn r18_multipage_toc_fetches_following_pages_via_next_url() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "novel18.syosetu.com")
            .unwrap();
        let first_page = r#"
<a href="/n1234aa/?p=2" class="c-pager__item c-pager__item--last">2</a>
<div class="p-eplist__sublist">
<a href="/n1234aa/1/" class="p-eplist__subtitle">
第1話
</a>

<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
<a href="/n1234aa/?p=2" class="c-pager__item c-pager__item--next">
"#;
        let second_page = r#"
<div class="p-eplist__sublist">
<a href="/n1234aa/2/" class="p-eplist__subtitle">
第2話
</a>

<div class="p-eplist__update">
2024年01月02日 00時00分
</div>
</div>
"#;
        let http = MockHttpClient::new();
        let rate_limiter = FakeRateLimiter::new();
        let fetched_urls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let subtitles = futures::executor::block_on(parse_subtitles_multipage_with(
            MultipageTocRequest {
                http: &http,
                rate_limiter: &rate_limiter,
                setting,
                toc_source: first_page,
                url_captures: &HashMap::new(),
                title: "",
                progress: None,
                jobs: &mut PreprocessJobs::new(),
            },
            {
                let fetched_urls = std::sync::Arc::clone(&fetched_urls);
                move |_, _, _, next_url, _| {
                    fetched_urls.lock().unwrap().push(next_url.clone());
                    let page = second_page.to_string();
                    Box::pin(async move { Ok(page) })
                }
            },
        ))
        .unwrap()
        .subtitles;

        assert_eq!(
            *fetched_urls.lock().unwrap(),
            vec!["https://novel18.syosetu.com/n1234aa/?p=2".to_string()]
        );
        assert_eq!(subtitles.len(), 2);
        assert_eq!(subtitles[0].index, "1");
        assert_eq!(subtitles[1].index, "2");
        assert_eq!(subtitles[1].href, "/n1234aa/2/");
    }

    #[test]
    fn akatsuki_toc_pattern_extracts_new_markup_sections() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "暁").unwrap();
        let toc_source = r#"
<ol class="mt-3 divide-y divide-border">
  <li class="pt-4 pb-1 text-sm font-bold">ゼンヒ巡査部長編</li>
  <li class="flex flex-wrap items-baseline justify-between gap-2 py-2">
    <span class="flex min-w-0 items-baseline gap-2">
      <span class="w-8 shrink-0 text-right text-xs text-subtle tabular-nums">1</span>
      <a href="/stories/view/313728/novel_id~31149"
         class="min-w-0 hover:text-brand-600 dark:hover:text-brand-300">
        プロローグ:新任巡査部長、扇皇 ゼンヒ
      </a>
    </span>
    <span class="flex flex-wrap items-baseline gap-x-3 text-xs text-subtle">
      <span>4,845文字</span>
      <span>2025年10月24日</span>
    </span>
  </li>
  <li class="flex flex-wrap items-baseline justify-between gap-2 py-2">
    <span class="flex min-w-0 items-baseline gap-2">
      <span class="w-8 shrink-0 text-right text-xs text-subtle tabular-nums">2</span>
      <a href="/stories/view/313729/novel_id~31149"
         class="min-w-0 hover:text-brand-600 dark:hover:text-brand-300">
        Case1:三毛猫の捜索
      </a>
    </span>
    <span class="flex flex-wrap items-baseline gap-x-3 text-xs text-subtle">
      <span>4,855文字</span>
      <span>2025年10月24日</span>
    </span>
  </li>
</ol>
"#;

        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();

        assert_eq!(subtitles.len(), 2);
        assert_eq!(subtitles[0].chapter, "ゼンヒ巡査部長編");
        assert_eq!(subtitles[0].index, "313728");
        assert_eq!(
            subtitles[0].subtitle,
            "プロローグ:新任巡査部長、扇皇 ゼンヒ"
        );
        assert_eq!(
            subtitles[0].href,
            "/stories/view/313728/novel_id~31149".to_string()
        );
        assert_eq!(subtitles[0].subupdate.as_deref(), Some("2025年10月24日"));
        assert_eq!(subtitles[1].chapter, "");
        assert_eq!(subtitles[1].index, "313729");
        assert_eq!(subtitles[1].subtitle, "Case1:三毛猫の捜索");
    }

    #[test]
    fn akatsuki_info_patterns_extract_new_markup_header() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "暁").unwrap();
        let toc_source = r#"
<header>
  <h1 class="min-w-0 text-xl font-bold">シャーレ前交番のヴァルキューレ警官</h1>
  <p class="mt-1 text-sm text-muted">
    <a href="/users/akntejfnq"
       class="hover:text-brand-600 dark:hover:text-brand-300">欄干</a>
  </p>
  <p class="mt-3 whitespace-pre-line text-sm text-neutral-700 dark:text-neutral-300">あらすじです。</p>
</header>
"#;

        let info = futures::executor::block_on(NovelInfo::from_toc_source(setting, toc_source));

        assert_eq!(info.title.as_deref(), Some("シャーレ前交番のヴァルキューレ警官"));
        assert_eq!(info.author.as_deref(), Some("欄干"));
        assert_eq!(info.story.as_deref(), Some("あらすじです。"));
    }

    #[test]
    fn akatsuki_work_page_url_resolves_to_toc_url() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "暁").unwrap();
        let expected = "https://www.akatsuki-novels.com/stories/index/novel_id~10044";

        // 作品ページ URL はユーザーが目にする形なので、目次ページと同じく
        // キャプチャから toc_url を組み立てて重複判定にも使える必要がある。
        for url in [
            "https://www.akatsuki-novels.com/novels/view/10044",
            "https://www.akatsuki-novels.com/stories/index/novel_id~10044",
            "https://www.akatsuki-novels.com/stories/index?novel_id=10044",
        ] {
            assert!(setting.matches_url(url), "{url}");
            assert_eq!(setting.toc_url_with_url_captures(url).as_deref(), Some(expected), "{url}");
            assert_eq!(
                crate::application::target_lookup::first_matching_toc_url(&settings, url).as_deref(),
                Some(expected),
                "{url}"
            );
        }
    }

    #[test]
    fn akatsuki_multipage_toc_fetches_all_pages() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "暁").unwrap();
        let page = |current: usize, max: usize, next: &str, id: &str, title: &str, date: &str| {
            format!(
                r#"
<nav role="navigation" aria-label="ページ送り（上）">
  <p class="text-subtle tabular-nums sm:hidden">{current} / {max} ページ</p>
  {next}
</nav>
<ol class="mt-3 divide-y divide-border">
  <li class="flex flex-wrap items-baseline justify-between gap-2 py-2">
    <span class="flex min-w-0 items-baseline gap-2">
      <span class="w-8 shrink-0 text-right text-xs text-subtle tabular-nums">1</span>
      <a href="/stories/view/{id}/novel_id~27990"
         class="min-w-0 hover:text-brand-600 dark:hover:text-brand-300">{title}</a>
    </span>
    <span class="flex flex-wrap items-baseline gap-x-3 text-xs text-subtle">
      <span>930文字</span><span>{date}</span>
    </span>
  </li>
</ol>
"#
            )
        };
        let first_page = page(
            1,
            3,
            r#"<a href="/stories/index?novel_id=27990&amp;page=2" rel="next"
   class="tap">次へ</a>"#,
            "280978",
            "第一話",
            "2022年12月30日",
        );
        let second_page = page(
            2,
            3,
            r#"<a href="/stories/index?novel_id=27990&amp;page=3" rel="next"
   class="tap">次へ</a>"#,
            "281288",
            "第三話",
            "2023年1月5日",
        );
        let third_page = page(3, 3, "", "281445", "第四話", "2023年1月9日");
        let http = MockHttpClient::new();
        let rate_limiter = FakeRateLimiter::new();
        let fetched_urls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let subtitles = futures::executor::block_on(parse_subtitles_multipage_with(
            MultipageTocRequest {
                http: &http,
                rate_limiter: &rate_limiter,
                setting,
                toc_source: &first_page,
                url_captures: &HashMap::new(),
                title: "",
                progress: None,
                jobs: &mut PreprocessJobs::new(),
            },
            {
                let fetched_urls = std::sync::Arc::clone(&fetched_urls);
                move |_, _, _, next_url, _| {
                    fetched_urls.lock().unwrap().push(next_url.clone());
                    let page = if fetched_urls.lock().unwrap().len() == 1 {
                        second_page.clone()
                    } else {
                        third_page.clone()
                    };
                    Box::pin(async move { Ok(page) })
                }
            },
        ))
        .unwrap()
        .subtitles;

        let fetched_urls = fetched_urls.lock().unwrap();
        assert_eq!(fetched_urls.len(), 2);
        assert_eq!(
            fetched_urls[0],
            "https://www.akatsuki-novels.com/stories/index?novel_id=27990&page=2"
        );
        assert_eq!(
            fetched_urls[1],
            "https://www.akatsuki-novels.com/stories/index?novel_id=27990&page=3"
        );
        assert_eq!(subtitles.len(), 3);
        assert_eq!(subtitles[0].index, "280978");
        assert_eq!(subtitles[0].subtitle, "第一話");
        assert_eq!(subtitles[1].index, "281288");
        assert_eq!(subtitles[1].subtitle, "第三話");
        assert_eq!(subtitles[2].index, "281445");
        assert_eq!(subtitles[2].subtitle, "第四話");
    }

    #[test]
    fn akatsuki_page_count_without_a_slash_still_fetches_every_page() {
        // ページ数が取れなければ 1 ページ目で止まる (目次が途中で切れる)。
        // 「1 / 3 ページ」だけでなく「3 ページ」だけの表示でも全ページを読む。
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "暁").unwrap();
        let page = |pager: &str, next: &str, id: &str, title: &str| {
            format!(
                r#"
<nav role="navigation" aria-label="ページ送り（上）">
  <p class="text-subtle tabular-nums sm:hidden">{pager}</p>
  {next}
</nav>
<ol class="mt-3 divide-y divide-border">
  <li class="flex flex-wrap items-baseline justify-between gap-2 py-2">
    <span class="flex min-w-0 items-baseline gap-2">
      <a href="/stories/view/{id}/novel_id~27990"
         class="min-w-0 hover:text-brand-600 dark:hover:text-brand-300">{title}</a>
    </span>
  </li>
</ol>
"#
            )
        };
        let first_page = page(
            "3 ページ",
            r#"<a href="/stories/index?novel_id=27990&amp;page=2" rel="next"
   class="tap">次へ</a>"#,
            "280978",
            "第一話",
        );
        let second_page = page(
            "3 ページ",
            r#"<a href="/stories/index?novel_id=27990&amp;page=3" rel="next"
   class="tap">次へ</a>"#,
            "281288",
            "第三話",
        );
        let third_page = page("3 ページ", "", "281445", "第四話");
        let http = MockHttpClient::new();
        let rate_limiter = FakeRateLimiter::new();
        let fetched = std::sync::Arc::new(std::sync::Mutex::new(0usize));

        let subtitles = futures::executor::block_on(parse_subtitles_multipage_with(
            MultipageTocRequest {
                http: &http,
                rate_limiter: &rate_limiter,
                setting,
                toc_source: &first_page,
                url_captures: &HashMap::new(),
                title: "",
                progress: None,
                jobs: &mut PreprocessJobs::new(),
            },
            {
                let fetched = std::sync::Arc::clone(&fetched);
                move |_, _, _, _next_url, _| {
                    let mut count = fetched.lock().unwrap();
                    *count += 1;
                    let body = if *count == 1 {
                        second_page.clone()
                    } else {
                        third_page.clone()
                    };
                    Box::pin(async move { Ok(body) })
                }
            },
        ))
        .unwrap()
        .subtitles;

        assert_eq!(*fetched.lock().unwrap(), 2, "2 ページ目以降も取得する");
        assert_eq!(subtitles.len(), 3);
    }

    #[test]
    fn parse_subtitles_strips_ruby_from_subtitle_and_file_subtitle() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "ncode.syosetu.com")
            .unwrap();
        // なろうの目次に稀に現れるルビ入り各話タイトル。
        let toc_source = r#"
<div class="p-eplist__sublist">
<a href="/n8858hb/1/" class="p-eplist__subtitle">
<ruby><rb>2人</rb><rp>(</rp><rt>兄妹</rt><rp>)</rp></ruby>の出会い
</a>

<div class="p-eplist__update">
2024年01月01日 00時00分
</div>
</div>
<div class="p-eplist__sublist">
<a href="/n8858hb/2/" class="p-eplist__subtitle">
第2話
</a>

<div class="p-eplist__update">
2024年01月02日 00時00分
</div>
</div>
"#;

        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();

        assert_eq!(subtitles.len(), 2);
        // narou.rb 互換: ruby タグだけ落とし、中身 (base + ルビ + rp 括弧) は残す。
        // 永続化される subtitle は改行も除去したクリーンな文字列。
        assert_eq!(subtitles[0].subtitle, "2人(兄妹)の出会い");
        assert_eq!(subtitles[0].file_subtitle, "2人(兄妹)の出会い");
        assert_eq!(subtitles[0].index, "1");
        // ルビなしの通常の話は変化しない。
        assert_eq!(subtitles[1].subtitle, "第2話");
        assert_eq!(subtitles[1].file_subtitle, "第2話");
    }

    #[test]
    fn hameln_episode_list_parses_ruby_titles() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.domain == "syosetu.org").unwrap();
        let r18_url = "https://h.syosetu.org/novel/420138/";
        assert!(setting.matches_url(r18_url));
        assert_eq!(
            setting.toc_url_with_url_captures(r18_url).as_deref(),
            Some("https://h.syosetu.org/novel/420138/")
        );
        // 2026 年以降のハーメルン目次形式。章名・各話タイトルの双方に
        // HTML ruby が含まれても、保存用の subtitle ではタグだけ除去する。
        let toc_source = r#"
<li class="episode-list__chapter">
  <div class="episode-list__chapter-title">『<ruby><rb>双花魔人譚</rb><rp>(</rp><rt>モンストルム・オラトリア</rt><rp>)</rp></ruby>』編</div>
</li>
<li class="episode-list__item">
<a href="./1.html" class="episode-list__link">
  <span class="episode-list__mark"></span>
  <span class="episode-list__title" >〖<ruby><rb>神々の給仕</rb><rp>(</rp><rt>ゴッズプライド</rt><rp>)</rp></ruby>〗</span>
  <time class="episode-list__date" itemprop="datePublished" datetime="2023-06-04T11:14Z">2023/06/04 11:14</time>
  <span class="episode-list__revision" title="2023/10/21 21:29改稿">(<u>改</u>)</span>
</a>
</li>
<li class="episode-list__item">
<a href="./2.html" class="episode-list__link">
  <span class="episode-list__mark"></span>
  <span class="episode-list__title" >通常タイトル</span>
  <time class="episode-list__date">2023/06/11 14:21</time>
  <span class="episode-list__revision"> </span>
</a>
</li>
"#;

        let subtitles = parse_subtitles(setting, toc_source, &HashMap::new()).unwrap();

        assert_eq!(subtitles.len(), 2);
        assert_eq!(
            subtitles[0].chapter,
            "『<ruby><rb>双花魔人譚</rb><rp>(</rp><rt>モンストルム・オラトリア</rt><rp>)</rp></ruby>』編"
        );
        assert_eq!(subtitles[0].subtitle, "〖神々の給仕(ゴッズプライド)〗");
        assert_eq!(subtitles[0].file_subtitle, "〖神々の給仕(ゴッズプライド)〗");
        assert_eq!(subtitles[0].subdate, "2023/06/04 11:14");
        assert_eq!(subtitles[0].subupdate.as_deref(), Some("2023/10/21 21:29"));
        assert_eq!(subtitles[1].chapter, "");
        assert_eq!(subtitles[1].subtitle, "通常タイトル");
    }

    #[test]
    fn an_empty_display_number_falls_back_to_the_episode_id() {
        let setting = SiteSetting::load_bundled(&[r#"
name: Example
domain: example.com
top_url: https://example.com
sitename: Example
toc_url: https://example.com/story/\k<ncode>
subtitles: |-
  <a href="(?<href>https://example\.com/story/\d+/(?<episode_id>\d+))" data-number="(?<index>[^"]*)">(?<subtitle>[^<]+)</a>
"#])
        .unwrap()
        .remove(0);

        let short = r#"<a href="https://example.com/story/367896071/200796571" data-number="">短編の話</a>"#;
        let subtitles = parse_subtitles(&setting, short, &HashMap::new()).unwrap();
        assert_eq!(subtitles.len(), 1);
        assert_eq!(subtitles[0].index, "200796571");
        assert_eq!(
            subtitles[0].href,
            "https://example.com/story/367896071/200796571"
        );

        let numbered = r#"<a href="https://example.com/story/204312035/657715863" data-number="001">第一話</a>"#;
        let subtitles = parse_subtitles(&setting, numbered, &HashMap::new()).unwrap();
        assert_eq!(subtitles[0].index, "001", "the display number wins when present");
    }
}
