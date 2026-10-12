//! Author-page listing shared by the CLI author check and target expansion.
//!
//! [`AuthorListing`] borrows the same transport, rate limiter and login store
//! the downloader uses, so an author page is fetched exactly like a novel page
//! (definition headers, encoding, spacing) without constructing a
//! [`Downloader`](super::Downloader). Everything site-specific comes from the
//! definition: `author_url` recognises the page, `author_api_url` /
//! `author_novel_pattern` / `preprocess:` list its works, `author_next_pattern`
//! or the `author_page_*` start-offset settings follow further pages, and
//! `author_series*` markers name works to keep out of the listing.
//!
//! A definition without a listing mechanism is an error, not an empty page. A
//! page that answers with a login wall is retried with every stored login in
//! order, and reported as an error when none of them makes the listing
//! readable — it must never look like an author with no works. A listing the
//! site declared more works for than could be fetched (a total that is not
//! reached, or a start offset past the range the API accepts) is an error too,
//! not a short success.

use std::collections::{HashMap, HashSet};

use crate::error::{NarouError, Result};
use crate::platform::{CookieStore, HttpClient, LoginGroup, RateLimiter};

use super::http_policy::{FetchPolicy, fetch_text};
use super::preprocess::PreprocessJobs;
use super::site_setting::SiteSetting;
use super::util::{PretreatmentRequest, pretreatment_source_with_jobs};
use super::{apply_login_cookie, url_mentions_id};

/// One author page as read from the site definitions.
pub(crate) struct AuthorListing<'a> {
    http: &'a dyn HttpClient,
    rate_limiter: &'a dyn RateLimiter,
    /// Stored logins tried when the page needs authentication. `None` keeps
    /// the listing anonymous (tests and callers without a store).
    cookies: Option<&'a dyn CookieStore>,
}

impl<'a> AuthorListing<'a> {
    pub(crate) fn new(
        http: &'a dyn HttpClient,
        rate_limiter: &'a dyn RateLimiter,
        cookies: Option<&'a dyn CookieStore>,
    ) -> Self {
        Self {
            http,
            rate_limiter,
            cookies,
        }
    }

    /// Every work URL the definitions list for `page_url`, in definition order.
    ///
    /// A readable page without works is an empty listing (an author may simply
    /// have nothing yet). Failed or login-walled definitions contribute
    /// nothing, but when *no* definition could be read the first failure is
    /// returned instead of an empty success.
    pub(crate) async fn works_for(
        &self,
        settings: &[&SiteSetting],
        page_url: &str,
    ) -> Result<Vec<String>> {
        if settings.is_empty() {
            return Err(NarouError::SiteSetting(format!(
                "{page_url} を作者ページとして扱うサイト定義がありません (author_url 未定義)"
            )));
        }
        let mut urls: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut failure: Option<NarouError> = None;
        for setting in settings {
            match self.works(setting, page_url).await {
                Ok(found) => {
                    for url in found {
                        if seen.insert(url.clone()) {
                            urls.push(url);
                        }
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        if urls.is_empty()
            && let Some(error) = failure
        {
            return Err(error);
        }
        Ok(urls)
    }

    /// One definition's listing: fetch, login fallback, series exclusion.
    async fn works(&self, setting: &SiteSetting, page_url: &str) -> Result<Vec<String>> {
        if setting.author_novel_urls("").is_none() {
            return Err(NarouError::SiteSetting(format!(
                "{} に author_novel_pattern がありません",
                setting.sitename
            )));
        }
        let groups = self.stored_groups(setting).await;
        let base_cookie = setting.cookie.clone();
        // 追加取得 (`request` / `fetch_json`) の結果は試行間で共有する
        // (novel 取得と同じ扱い: 同じ URL は 1 回だけ).
        let mut jobs = PreprocessJobs::new();

        let mut best = Attempt {
            setting: setting.clone(),
            listing: self.fetch_listing(setting, page_url, &mut jobs).await,
        };
        if !is_fully_readable(&best.listing) && !groups.is_empty() {
            for group in &groups {
                let mut with_login = setting.clone();
                with_login.cookie = base_cookie.clone();
                apply_login_cookie(&mut with_login, &group.merged_cookie(), &group.cookies);
                let listing = self.fetch_listing(&with_login, page_url, &mut jobs).await;
                let attempt = Attempt {
                    setting: with_login,
                    listing,
                };
                if better(&attempt.listing, &best.listing) {
                    let resolved = is_fully_readable(&attempt.listing);
                    best = attempt;
                    if resolved {
                        break;
                    }
                }
            }
        }

        let Attempt {
            setting: winner,
            listing,
        } = best;
        let listing = listing?;
        if listing.login_walled {
            return Err(NarouError::SiteSetting(format!(
                "{} の作者ページにはログインが必要です: {page_url}",
                winner.sitename
            )));
        }
        if listing.partial && listing.urls.is_empty() {
            // 定義が「一覧は欠けている」と言っているのに 1 件も無い = 空の
            // 作者ではなく取得失敗。黙って空を返さない。
            return Err(NarouError::SiteSetting(format!(
                "{} の作者ページの一覧を取得できませんでした: {page_url}",
                winner.sitename
            )));
        }
        let Listing {
            mut urls,
            series,
            comic_series,
            ..
        } = listing;
        self.exclude_series_members(&winner, page_url, &series, &comic_series, &mut urls)
            .await;
        Ok(urls)
    }

    /// Stored logins for the definition's site, in the order they are tried.
    async fn stored_groups(&self, setting: &SiteSetting) -> Vec<LoginGroup> {
        let Some(cookies) = self.cookies else {
            return Vec::new();
        };
        cookies
            .load_groups(&setting.domain)
            .await
            .unwrap_or_default()
    }

    /// Read every page of the listing the definition declares.
    ///
    /// Pages are followed through `author_next_pattern` (an explicit next-page
    /// link) or the `author_page_*` start-offset settings, until the listing
    /// ends: no next link / no next offset, an already visited URL, nothing
    /// new, a page shorter than `author_page_size`, or the declared total.
    /// A listing that the site declared more works for than could be fetched
    /// (the declared total is not reached, or the next start offset is past
    /// the API's range) is an error, not a short success.
    async fn fetch_listing(
        &self,
        setting: &SiteSetting,
        page_url: &str,
        jobs: &mut PreprocessJobs,
    ) -> Result<Listing> {
        let policy = FetchPolicy::for_site(setting);
        let captures = setting.extract_author_url_captures(page_url).unwrap_or_default();
        let mut listing = Listing::default();
        let mut fetch_url = setting.author_fetch_url(page_url);
        // 一覧が宣言した総件数 (定義がある場合)。取り切れずに終わったら
        // 途中までの一覧を成功にしない。
        let mut declared_total: Option<usize> = None;
        // 訪問済みの URL に戻ったら終わり。ページャが先頭へ戻る定義でも止まる。
        let mut visited: HashSet<String> = HashSet::new();
        loop {
            if !visited.insert(fetch_url.clone()) {
                break;
            }
            let mut body = fetch_text(
                self.http,
                self.rate_limiter,
                &fetch_url,
                &policy,
                Some(setting.encoding()),
            )
            .await?;
            // `preprocess:` の DSL が `author_novel::` / `author_series::` を
            // 出せるように、本文を定義に通してから読み取る。取得元 URL を渡す
            // ので、定義は続きの URL を組み立てられる。`request()` は同じ
            // policy で実行し、結果は `fetched[<url>]` として見える。
            pretreatment_source_with_jobs(PretreatmentRequest {
                http: self.http,
                rate_limiter: self.rate_limiter,
                policy: &policy,
                src: &mut body,
                encoding: setting.encoding(),
                setting: Some(setting),
                jobs,
                url: &fetch_url,
            })
            .await?;
            if setting
                .compiled_login_pattern()
                .is_some_and(|pattern| pattern.is_match(&body))
            {
                listing.login_walled = true;
            }
            if setting.is_partial_login_view(&body) {
                listing.partial = true;
            }
            let total = setting.author_page_total(&body);
            if let Some(total) = total {
                declared_total = Some(total);
            }
            let before = listing.urls.len();
            for url in setting.author_novel_urls(&body).unwrap_or_default() {
                if listing.seen.insert(url.clone()) {
                    listing.urls.push(url);
                }
            }
            for id in setting.author_series_ids(&body) {
                if !listing.series.contains(&id) {
                    listing.series.push(id);
                }
            }
            for id in setting.author_comic_series_ids(&body) {
                if !listing.comic_series.contains(&id) {
                    listing.comic_series.push(id);
                }
            }
            let next = match setting.author_next_url(&body) {
                Some(next) => Some(next),
                None => {
                    next_offset_url(setting, &captures, page_url, &listing, before, total)?
                }
            };
            let Some(next) = next else {
                break;
            };
            fetch_url = next;
        }
        if let Some(total) = declared_total
            && listing.urls.len() < total
        {
            // 応答が総件数を宣言しているのに読み切れなかった = 一覧は欠けて
            // いる。空/不足を成功として返さない。
            return Err(NarouError::SiteSetting(format!(
                "{} の作者一覧を最後まで取得できませんでした ({} / {total} 件): {page_url}",
                setting.sitename,
                listing.urls.len()
            )));
        }
        Ok(listing)
    }

    /// Keep the works and pages of the series the listing named out of it.
    ///
    /// Best effort: a series list that cannot be read contributes nothing, so
    /// its members stay in the listing instead of silently vanishing.
    async fn exclude_series_members(
        &self,
        setting: &SiteSetting,
        page_url: &str,
        series: &[String],
        comic_series: &[String],
        urls: &mut Vec<String>,
    ) {
        if urls.is_empty() {
            return;
        }
        let captures = setting.extract_author_url_captures(page_url).unwrap_or_default();
        let policy = FetchPolicy::for_site(setting);
        let mut inside: Vec<String> = Vec::new();
        if !series.is_empty() && setting.author_series_episodes_url.is_some() {
            inside.extend(
                self.series_episodes(setting, &captures, &policy, series)
                    .await,
            );
        }
        if !comic_series.is_empty() && setting.author_comic_series_pages_url.is_some() {
            inside.extend(
                self.comic_series_pages(setting, &captures, &policy, comic_series)
                    .await,
            );
        }
        if !inside.is_empty() {
            urls.retain(|url| !inside.iter().any(|id| url_mentions_id(url, id)));
        }
    }

    /// Episode ids of the given series, fetched through the definition.
    async fn series_episodes(
        &self,
        setting: &SiteSetting,
        captures: &HashMap<String, String>,
        policy: &FetchPolicy,
        series: &[String],
    ) -> Vec<String> {
        let mut episodes: Vec<String> = Vec::new();
        for series_id in series {
            let Some(url) = setting.author_series_episodes_fetch_url(captures, series_id) else {
                continue;
            };
            let Ok(body) = fetch_text(
                self.http,
                self.rate_limiter,
                &url,
                policy,
                Some(setting.encoding()),
            )
            .await
            else {
                continue;
            };
            for id in setting.author_series_episode_ids(&body) {
                if !episodes.contains(&id) {
                    episodes.push(id);
                }
            }
        }
        episodes
    }

    /// Artwork ids of the given comic series, page by page (Pixiv pages them
    /// in batches of 12) until a page adds nothing.
    async fn comic_series_pages(
        &self,
        setting: &SiteSetting,
        captures: &HashMap<String, String>,
        policy: &FetchPolicy,
        series: &[String],
    ) -> Vec<String> {
        let mut pages: Vec<String> = Vec::new();
        for series_id in series {
            // ページ数の上限は設けない: 空のページ・新しい項目の無いページ・
            // 訪問済みの URL のいずれかで止まる (narou.rb の page 送りと同じ)。
            let mut visited: HashSet<String> = HashSet::new();
            let mut page_number = 1usize;
            loop {
                let Some(url) =
                    setting.author_comic_series_pages_fetch_url(captures, series_id, page_number)
                else {
                    break;
                };
                if !visited.insert(url.clone()) {
                    break;
                }
                let Ok(body) = fetch_text(
                    self.http,
                    self.rate_limiter,
                    &url,
                    policy,
                    Some(setting.encoding()),
                )
                .await
                else {
                    break;
                };
                let ids = setting.author_comic_series_page_ids(&body);
                if ids.is_empty() {
                    break;
                }
                let before = pages.len();
                for id in ids {
                    if !pages.contains(&id) {
                        pages.push(id);
                    }
                }
                if pages.len() == before {
                    // 同じページが返り続ける定義でも止まる。
                    break;
                }
                page_number += 1;
            }
        }
        pages
    }
}

/// One fetch of a listing, and the setting (with or without a login) it used.
struct Attempt {
    setting: SiteSetting,
    listing: Result<Listing>,
}

/// What one listing read produced before series members are excluded.
#[derive(Debug, Default)]
struct Listing {
    urls: Vec<String>,
    seen: HashSet<String>,
    series: Vec<String>,
    comic_series: Vec<String>,
    /// Any page matched `login_pattern` (a login wall served with HTTP 200).
    login_walled: bool,
    /// Any page matched `login_partial_pattern` (the definition reports the
    /// listing is incomplete without a stored login).
    partial: bool,
}

/// A page read without a login wall or an incomplete-listing marker.
fn is_fully_readable(listing: &Result<Listing>) -> bool {
    matches!(listing, Ok(listing) if !listing.login_walled && !listing.partial)
}

/// Whether `candidate` is worth keeping over `best`.
///
/// A readable page beats an error, a login wall or a partial listing; among
/// readable pages more works win (a tie keeps the earlier attempt, which is
/// the definition's own cookie before any stored login).
fn better(candidate: &Result<Listing>, best: &Result<Listing>) -> bool {
    let (candidate, best) = match (rank(candidate), rank(best)) {
        (Some(candidate), Some(best)) => (candidate, best),
        (Some(_), None) => return true,
        _ => return false,
    };
    // 壁なし (0) > 壁あり (1)、完全 (0) > 部分 (1)、件数は多い方。
    (candidate.0, candidate.1, std::cmp::Reverse(candidate.2))
        < (best.0, best.1, std::cmp::Reverse(best.2))
}

type ListingRank = (u8, u8, usize);

fn rank(listing: &Result<Listing>) -> Option<ListingRank> {
    let listing = listing.as_ref().ok()?;
    Some((
        u8::from(listing.login_walled),
        u8::from(listing.partial),
        listing.urls.len(),
    ))
}

/// Next page of an offset-paged listing, or `None` when this page ended it.
///
/// `before` is the number of works that were already read when the page
/// started, so `st` continues exactly after them. The page ends the listing
/// when it adds nothing; when no total is declared, a page shorter than
/// `author_page_size` is the last one; when a total is declared, it decides.
/// If the listing plainly continues (total not reached, or an unknown total
/// with a full page) but the next start offset is past
/// `author_page_max_start` — the range the site's API accepts — the rest
/// cannot be fetched: that is an error, never a silently truncated listing.
fn next_offset_url(
    setting: &SiteSetting,
    captures: &HashMap<String, String>,
    page_url: &str,
    listing: &Listing,
    before: usize,
    total: Option<usize>,
) -> Result<Option<String>> {
    if setting.author_page_url.is_none() {
        return Ok(None);
    }
    let read = listing.urls.len();
    if read - before == 0 {
        // 新しい項目の無いページ = これ以上列挙できない (同じページが
        // 返り続ける定義でも止まる)。
        return Ok(None);
    }
    if total.is_some_and(|total| read >= total) {
        return Ok(None);
    }
    if total.is_none() {
        let size = setting.author_page_size.unwrap_or(0);
        if size > 0 && read - before < size {
            // 総件数が分からない定義は、1 ページ分に満たないページで終端。
            return Ok(None);
        }
    }
    let st = read + 1;
    if let Some(max) = setting.author_page_max_start
        && st > max
    {
        return Err(NarouError::SiteSetting(format!(
            "{} の作者一覧は取得範囲 (st<={max}) を超えています: {page_url}",
            setting.sitename
        )));
    }
    Ok(setting.author_page_fetch_url(captures, st))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::HostCookie;
    use crate::platform::mocks::{FakeRateLimiter, MemoryCookieStore, MockHttpClient};
    use crate::platform::{HttpRequest, HttpResponse, PlatformFuture};
    use parking_lot::Mutex;

    fn load(domain: &str) -> SiteSetting {
        SiteSetting::load_all()
            .expect("site definitions")
            .into_iter()
            .find(|setting| setting.domain == domain)
            .unwrap_or_else(|| panic!("no definition for {domain}"))
    }

    fn works(
        http: &dyn HttpClient,
        limiter: &dyn RateLimiter,
        setting: &SiteSetting,
        page_url: &str,
    ) -> Result<Vec<String>> {
        futures::executor::block_on(
            AuthorListing::new(http, limiter, None).works_for(&[setting], page_url),
        )
    }

    #[test]
    fn paged_author_listings_are_followed_to_the_end() {
        // ハーメルンの作者ページは検索ページの一覧を page=2, 3… と辿る。
        let setting = load("syosetu.org");
        let page = "https://syosetu.org/user/495125/";
        let first = "https://syosetu.org/search/?mode=search_user_novel_list&uid=495125";
        assert_eq!(setting.author_fetch_url(page), first);
        assert!(setting.matches_author_url(page));
        assert!(!setting.matches_author_url("https://syosetu.org/novel/388533/"));

        let http = MockHttpClient::new();
        http.add_text(
            first,
            200,
            r#"<a href="https://h.syosetu.org/novel/388533/">作品1</a>
               <a href="https://h.syosetu.org/novel/388533/1.html">第1話</a>
               <li><a href="https://syosetu.org/search/?mode=search_user_novel_list&amp;word=&amp;uid=495125&amp;page=2">>></a></li>"#,
        );
        http.add_text(
            "https://syosetu.org/search/?mode=search_user_novel_list&word=&uid=495125&page=2",
            200,
            r#"<a href="https://syosetu.org/novel/388855/">作品2</a>
               <li><a href="https://syosetu.org/search/?mode=search_user_novel_list&amp;word=&amp;uid=495125&amp;page=1"><<</a></li>
               <li><a href="https://syosetu.org/search/?mode=search_user_novel_list&amp;word=&amp;uid=495125&amp;page=3">>></a></li>"#,
        );
        http.add_text(
            "https://syosetu.org/search/?mode=search_user_novel_list&word=&uid=495125&page=3",
            200,
            r#"<a href="https://syosetu.org/novel/400001/">作品3</a>"#,
        );
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://h.syosetu.org/novel/388533/".to_string(),
                "https://syosetu.org/novel/388855/".to_string(),
                "https://syosetu.org/novel/400001/".to_string()
            ],
            "話ページを作品として数えず、>> の付いた次ページだけを辿る (上限なしで終端まで)"
        );
    }

    #[test]
    fn a_pager_that_links_back_does_not_loop() {
        // 終端で先頭に戻るページャでも、訪問済みなら止まる (上限は設けない)。
        let setting = load("syosetu.org");
        let page = "https://syosetu.org/user/495125/";
        let first = "https://syosetu.org/search/?mode=search_user_novel_list&uid=495125";
        let second =
            "https://syosetu.org/search/?mode=search_user_novel_list&word=&uid=495125&page=2";

        let http = MockHttpClient::new();
        http.add_text(
            first,
            200,
            r#"<a href="https://syosetu.org/novel/1/">作品1</a>
               <a href="https://syosetu.org/search/?mode=search_user_novel_list&amp;word=&amp;uid=495125&amp;page=2">>></a>"#,
        );
        http.add_text(
            second,
            200,
            r#"<a href="https://syosetu.org/novel/2/">作品2</a>
               <a href="https://syosetu.org/search/?mode=search_user_novel_list&amp;uid=495125">>></a>"#,
        );
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://syosetu.org/novel/1/".to_string(),
                "https://syosetu.org/novel/2/".to_string()
            ]
        );
    }

    #[test]
    fn the_narou_api_lists_author_works() {
        // なろうの作者ページは小説API から作品を取る: 定義の `author_api_url`
        // に作者ページの capture が流し込まれ、応答の `ncode` から作品 URL を
        // 組み立てる (大文字の ncode は小文字化する)。
        let setting = load("ncode.syosetu.com");
        let page = "https://mypage.syosetu.com/2842627/";
        assert_eq!(
            setting.author_fetch_url(page),
            "https://api.syosetu.com/novelapi/api/?out=json&userid=2842627&lim=500"
        );

        let http = MockHttpClient::new();
        http.add_text(
            setting.author_fetch_url(page),
            200,
            r#"[{"allcount":2},{"ncode":"N5181MT","title":"作品1"},{"ncode":"N6275MR","title":"作品2"}]"#,
        );
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://ncode.syosetu.com/n5181mt/".to_string(),
                "https://ncode.syosetu.com/n6275mr/".to_string()
            ]
        );
    }

    #[test]
    fn narou_author_api_pages_beyond_the_first_batch_are_followed() {
        // なろう小説API は 1 回に 500 件まで。500 件を超える作者は st を進めて
        // 続きを取り、総件数 (allcount) に達したら終わる。
        let setting = load("ncode.syosetu.com");
        let page = "https://mypage.syosetu.com/2842627/";
        let captures = setting.extract_author_url_captures(page).unwrap();
        let first = setting.author_fetch_url(page);
        let second = setting.author_page_fetch_url(&captures, 501).unwrap();
        let third = setting.author_page_fetch_url(&captures, 1001).unwrap();
        assert_eq!(
            second,
            "https://api.syosetu.com/novelapi/api/?out=json&userid=2842627&lim=500&st=501"
        );

        let batch = |range: std::ops::RangeInclusive<usize>| {
            let mut items = vec![r#"{"allcount":1200}"#.to_string()];
            for index in range {
                items.push(format!(r#"{{"ncode":"N{index:04}AA"}}"#));
            }
            format!("[{}]", items.join(","))
        };
        let http = MockHttpClient::new();
        http.add_text(first.as_str(), 200, batch(1..=500));
        http.add_text(second.as_str(), 200, batch(501..=1000));
        http.add_text(third.as_str(), 200, batch(1001..=1200));

        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(urls.len(), 1200, "got {} urls", urls.len());
        assert_eq!(urls[0], "https://ncode.syosetu.com/n0001aa/");
        assert_eq!(urls[1199], "https://ncode.syosetu.com/n1200aa/");
        assert_eq!(http.request_count(), 3, "one request per page");
    }

    const PAGED: &str = r#"
name: Paged
domain: paged.example
top_url: https://paged.example
url: https://paged\.example/book/(?<ncode>\d+)
author_url: ^https?://paged\.example/user/(?<author_id>\d+)/?
author_api_url: https://paged.example/user/\k<author_id>
author_novel_pattern: '(?<novel_url>https?://paged\.example/book/\d+)'
author_page_url: https://paged.example/user/\k<author_id>?st=\k<st>
author_page_size: 2
author_page_max_start: 4
sitename: Paged
toc_url: https://paged.example/book/\k<ncode>
"#;

    const PAGED_TOTAL: &str = r#"
name: PagedTotal
domain: paged-total.example
top_url: https://paged-total.example
url: https://paged-total\.example/book/(?<ncode>\d+)
author_url: ^https?://paged-total\.example/user/(?<author_id>\d+)/?
author_api_url: https://paged-total.example/user/\k<author_id>
author_novel_pattern: '(?<novel_url>https?://paged-total\.example/book/\d+)'
author_page_url: https://paged-total.example/user/\k<author_id>?st=\k<st>
author_page_size: 2
author_page_total_pattern: '"allcount":(?<count>\d+)'
author_page_max_start: 4
sitename: PagedTotal
toc_url: https://paged-total.example/book/\k<ncode>
"#;

    fn full_paged_body(url_prefix: &str, first: &str, second: &str, total: Option<usize>) -> String {
        let head = total.map_or(String::new(), |total| format!(r#"{{"allcount":{total}}}"#));
        format!(
            r#"{head} <a href="{url_prefix}/book/{first}"></a>
               <a href="{url_prefix}/book/{second}"></a>"#
        )
    }

    #[test]
    fn offset_pages_past_the_api_range_are_an_error() {
        // st の上限 (author_page_max_start) を越える続きが残っているなら、
        // 途中までの一覧を成功にしない。
        let definitions = SiteSetting::load_bundled(&[PAGED]).unwrap();
        let setting = &definitions[0];
        let page = "https://paged.example/user/5";
        let http = MockHttpClient::new();
        http.add_text(page, 200, full_paged_body("https://paged.example", "1", "2", None));
        http.add_text(
            "https://paged.example/user/5?st=3",
            200,
            full_paged_body("https://paged.example", "3", "4", None),
        );
        let error = works(&http, &FakeRateLimiter::new(), setting, page).unwrap_err();
        assert!(
            matches!(error, NarouError::SiteSetting(_)),
            "got {error:?}"
        );
        assert!(error.to_string().contains("取得範囲"), "got {error}");
        assert_eq!(http.request_count(), 2, "st=5 is never requested");
    }

    #[test]
    fn a_short_offset_page_ends_the_listing() {
        // 総件数が分からない定義は、1 ページの件数 (author_page_size) に
        // 満たないページで終端。
        let definitions = SiteSetting::load_bundled(&[PAGED]).unwrap();
        let setting = &definitions[0];
        let page = "https://paged.example/user/5";
        let http = MockHttpClient::new();
        http.add_text(page, 200, full_paged_body("https://paged.example", "1", "2", None));
        http.add_text(
            "https://paged.example/user/5?st=3",
            200,
            r#"<a href="https://paged.example/book/3"></a>"#,
        );
        let urls = works(&http, &FakeRateLimiter::new(), setting, page).unwrap();
        assert_eq!(urls.len(), 3, "got {urls:?}");
        assert_eq!(http.request_count(), 2);
    }

    #[test]
    fn offset_pages_stop_at_the_declared_total_without_error() {
        // 総件数にちょうど達したら、開始位置が上限の先でも正常終了。
        let definitions = SiteSetting::load_bundled(&[PAGED_TOTAL]).unwrap();
        let setting = &definitions[0];
        let page = "https://paged-total.example/user/5";
        let http = MockHttpClient::new();
        http.add_text(
            page,
            200,
            full_paged_body("https://paged-total.example", "1", "2", Some(4)),
        );
        http.add_text(
            "https://paged-total.example/user/5?st=3",
            200,
            full_paged_body("https://paged-total.example", "3", "4", Some(4)),
        );
        let urls = works(&http, &FakeRateLimiter::new(), setting, page).unwrap();
        assert_eq!(urls.len(), 4, "got {urls:?}");
        assert_eq!(http.request_count(), 2);
    }

    #[test]
    fn a_declared_total_that_cannot_be_reached_is_an_error() {
        // 総件数に届かずに一覧が尽きたら、欠けた一覧を成功にしない。
        let definitions = SiteSetting::load_bundled(&[PAGED_TOTAL]).unwrap();
        let setting = &definitions[0];
        let page = "https://paged-total.example/user/5";
        let http = MockHttpClient::new();
        http.add_text(
            page,
            200,
            full_paged_body("https://paged-total.example", "1", "2", Some(10)),
        );
        http.add_text(
            "https://paged-total.example/user/5?st=3",
            200,
            r#"{"allcount":10}"#,
        );
        let error = works(&http, &FakeRateLimiter::new(), setting, page).unwrap_err();
        assert!(
            error.to_string().contains("最後まで取得できませんでした"),
            "got {error}"
        );
        assert_eq!(http.request_count(), 2);
    }

    #[test]
    fn pixiv_comic_series_pages_are_not_capped_at_one_hundred() {
        // 漫画シリーズの作品は 12 件ずつページ分けされる。100 ページで打ち切らず、
        // 空のページまで辿る。
        let setting = load("www.pixiv.net");
        let page = "https://www.pixiv.net/users/6519870";
        let profile = setting.author_fetch_url(page);
        let series = "9001";
        let http = MockHttpClient::new();
        http.add_text(
            profile,
            200,
            format!(
                r#"{{"error":false,"message":"","body":{{"novels":{{}},"novelSeries":[],"mangaSeries":[{{"id":"{series}","userId":"1"}}],"illusts":{{}},"manga":[]}}}}"#
            ),
        );
        const PAGES: usize = 110;
        for index in 1..=PAGES {
            http.add_text(
                format!("https://www.pixiv.net/ajax/series/{series}?p={index}&lang=ja"),
                200,
                format!(
                    r#"{{"error":false,"body":{{"page":{{"series":[{{"workId":"{}","order":{index}}}]}}}}}}"#,
                    1000 + index
                ),
            );
        }
        // 111 ページ目が空 = 終端。
        http.add_text(
            format!("https://www.pixiv.net/ajax/series/{series}?p=111&lang=ja"),
            200,
            r#"{"error":false,"body":{"page":{"series":[]}}}"#,
        );

        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec!["https://www.pixiv.net/ajax/series/9001".to_string()],
            "漫画シリーズ自身は作品、各ページは一覧から除かれる: {urls:?}"
        );
        assert_eq!(http.request_count(), 1 + PAGES + 1);
    }

    #[test]
    fn series_exclusion_is_best_effort_when_the_episode_list_is_missing() {
        // シリーズの話一覧が取得できなくても、一覧の作品は残す (取りこぼしより
        // 余分に足さない方が害が小さい、という既存方針のまま)。
        let setting = load("www.pixiv.net");
        let page = "https://www.pixiv.net/users/6519870";
        let http = MockHttpClient::new();
        http.add_text(
            setting.author_fetch_url(page),
            200,
            r#"{"error":false,"message":"","body":{"novels":{},"novelSeries":[{"id":"272850","userId":"1"}],"mangaSeries":[],"illusts":{},"manga":[]}}"#,
        );
        http.add_text(
            "https://www.pixiv.net/ajax/novel/series/272850/content_titles",
            404,
            "",
        );
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec!["https://www.pixiv.net/novel/series/272850".to_string()],
            "got {urls:?}"
        );
    }

    #[test]
    fn an_author_without_works_is_an_empty_listing_not_a_login_wall() {
        // 作品を持たない作者の応答は空の一覧であって、ログイン壁ではない。
        let setting = load("www.pixiv.net");
        let page = "https://www.pixiv.net/users/6519870";
        let http = MockHttpClient::new();
        http.add_text(
            setting.author_fetch_url(page),
            200,
            r#"{"error":false,"message":"","body":{"novels":{},"novelSeries":[],"mangaSeries":[],"illusts":[],"manga":[]}}"#,
        );
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert!(urls.is_empty(), "got {urls:?}");
    }

    const GATED: &str = r#"
name: Gated
domain: gated.example
top_url: https://gated.example
url: https://gated\.example/book/(?<ncode>\d+)
author_url: ^https?://gated\.example/user/(?<author_id>\d+)/?
author_api_url: https://gated.example/user/\k<author_id>
author_novel_pattern: '(?<novel_url>https?://gated\.example/book/\d+)'
sitename: Gated
toc_url: https://gated.example/book/\k<ncode>
login_pattern: 'ログインが必要です'
"#;

    /// Serves the listing only when the request carries `session=ok`, recording
    /// every `Cookie:` header it saw.
    struct GatedAuthorClient {
        listing: String,
        wall: String,
        cookies: Mutex<Vec<Option<String>>>,
    }

    impl HttpClient for GatedAuthorClient {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let cookie = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone());
                self.cookies.lock().push(cookie.clone());
                let authorized = cookie
                    .as_deref()
                    .is_some_and(|value| value.contains("session=ok"));
                let body = if authorized {
                    self.listing.clone()
                } else {
                    self.wall.clone()
                };
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                    body: body.into_bytes(),
                })
            })
        }
    }

    fn gated_client() -> GatedAuthorClient {
        GatedAuthorClient {
            listing: r#"<a href="https://gated.example/book/1">作品</a>"#.to_string(),
            wall: "ログインが必要です".to_string(),
            cookies: Mutex::new(Vec::new()),
        }
    }

    fn login(cookie: &str) -> LoginGroup {
        LoginGroup::new(
            "gated.example",
            vec![HostCookie {
                host: "gated.example".to_string(),
                cookie: cookie.to_string(),
            }],
        )
    }

    #[test]
    fn login_walls_retry_every_stored_login_in_order() {
        let definitions = SiteSetting::load_bundled(&[GATED]).unwrap();
        let setting = &definitions[0];
        let page = "https://gated.example/user/5/";
        let http = gated_client();
        let cookies = MemoryCookieStore::new();
        futures::executor::block_on(cookies.save_groups(
            "gated.example",
            &[login("session=bad"), login("session=ok")],
        ))
        .unwrap();

        let urls = futures::executor::block_on(
            AuthorListing::new(&http, &FakeRateLimiter::new(), Some(&cookies))
                .works_for(&[setting], page),
        )
        .unwrap();
        assert_eq!(urls, vec!["https://gated.example/book/1".to_string()]);

        let seen = http.cookies.lock().clone();
        assert_eq!(
            seen.len(),
            3,
            "定義の cookie、保存済みログイン 2 件を順に 1 回ずつ試す: {seen:?}"
        );
        assert_eq!(seen[0], None);
        assert_eq!(seen[1].as_deref(), Some("session=bad"));
        assert_eq!(seen[2].as_deref(), Some("session=ok"));
    }

    #[test]
    fn an_unreadable_login_wall_is_an_error_not_an_empty_author() {
        let definitions = SiteSetting::load_bundled(&[GATED]).unwrap();
        let setting = &definitions[0];
        let page = "https://gated.example/user/5/";
        let http = gated_client();
        let cookies = MemoryCookieStore::new();
        futures::executor::block_on(cookies.save_groups("gated.example", &[login("session=bad")]))
            .unwrap();

        let error = futures::executor::block_on(
            AuthorListing::new(&http, &FakeRateLimiter::new(), Some(&cookies))
                .works_for(&[setting], page),
        )
        .unwrap_err();
        assert!(matches!(error, NarouError::SiteSetting(_)), "got {error:?}");
        assert!(
            error.to_string().contains("ログイン"),
            "got {error}"
        );
        assert_eq!(http.cookies.lock().len(), 2);
    }

    #[test]
    fn preprocess_runs_with_the_fetched_url_and_fulfills_request_jobs() {
        // `preprocess:` は取得元 URL (`url`) を受け取り、`fetch_json` の
        // 取得要求は同じ policy で実行される。
        const PREPROCESSED: &str = r#"
name: Preprocessed
domain: pre.example
top_url: https://pre.example
url: https://pre\.example/book/(?<ncode>\d+)
author_url: ^https?://pre\.example/user/(?<author_id>\d+)/?
author_api_url: https://pre.example/user/\k<author_id>
preprocess: |-
  let extra = fetch_json("${url}/extra")
  if extra.title
    emit "author_novel::https://pre.example/book/${extra.id}\n"
  end
  insert_at_match
sitename: Preprocessed
toc_url: https://pre.example/book/\k<ncode>
"#;
        let definitions = SiteSetting::load_bundled(&[PREPROCESSED]).unwrap();
        let setting = &definitions[0];
        let http = MockHttpClient::new();
        http.add_text("https://pre.example/user/5", 200, "<p>no works in the html</p>");
        http.add_text(
            "https://pre.example/user/5/extra",
            200,
            r#"{"title":"x","id":7}"#,
        );

        let urls = works(&http, &FakeRateLimiter::new(), setting, "https://pre.example/user/5")
            .unwrap();
        assert_eq!(urls, vec!["https://pre.example/book/7".to_string()]);
        assert_eq!(http.request_count(), 2);
    }
}
