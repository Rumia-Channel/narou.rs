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
//!
//! A page the site refuses (a browser-verification answer, or a login wall
//! served with HTTP 200) is read again with a browser-issued token, once per
//! page. The token comes from the definition (`browser_token`) and the store,
//! so no site is special-cased here. The pages read before stay: a renewed
//! token continues the listing instead of starting it over, and the request
//! that follows the renewal — not the issued token — decides whether it was
//! accepted.

use std::collections::{HashMap, HashSet};

use crate::error::{NarouError, Result};
use crate::platform::{CookieStore, HttpClient, LoginGroup, RateLimiter};

use super::auth::AuthState;
use super::http_policy::{FetchPolicy, fetch_text};
use super::is_login_required;
use super::preprocess::PreprocessJobs;
use super::site_setting::SiteSetting;
use super::util::{PretreatmentRequest, pretreatment_source_with_jobs};

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
        // 資格情報は必要なときに 1 回だけ読む (読み終えた一覧は保存済み
        // ログインを読まない)。
        let mut auth = AuthState::new(setting.cookie.clone());
        // 追加取得 (`request` / `fetch_json`) の結果は試行間で共有する
        // (novel 取得と同じ扱い: 同じ URL は 1 回だけ).
        let mut jobs = PreprocessJobs::new();
        let mut token_attempts: HashMap<String, HashSet<Option<String>>> = HashMap::new();

        let mut anonymous = setting.clone();
        let listing = self
            .fetch_listing(&mut anonymous, &mut auth, page_url, &mut jobs, &mut token_attempts)
            .await;
        let mut best = Attempt {
            setting: anonymous,
            listing,
        };
        let merge_logins = setting.author_login_merge;
        if merge_logins || !is_fully_readable(&best.listing) {
            // 保存済みログインは必要なときに 1 回だけ読む。
            self.ensure_groups(&mut auth, setting).await;
            // 匿名の一覧に見えている分を種にして、ログインごとの一覧を
            // 足し合わせる (`author_login_merge`)。匿名応答が完全に見えても
            // ログイン限定作品が欠けるサイト (Pixiv の R-18) のための経路。
            let mut merged = Listing::default();
            if let Ok(listing) = &best.listing {
                merge_listing(&mut merged, listing);
            }
            let mut any_readable = false;
            let mut winner: Option<Attempt> = None;
            // 採用した資格情報。失効したトークンを更新した状態も保つ。
            let mut original = auth.active().cloned();
            while let Some(index) = auth.next_unused() {
                let mut with_login = setting.clone();
                if !auth.apply(&mut with_login, index) {
                    // Cookie を持たないログインは何も認証できない。
                    continue;
                }
                let listing = self
                    .fetch_listing(&mut with_login, &mut auth, page_url, &mut jobs, &mut token_attempts)
                    .await;
                if let Ok(found) = &listing {
                    if !found.login_walled {
                        any_readable = true;
                    }
                    if merge_logins {
                        merge_listing(&mut merged, found);
                        let winner_len = winner
                            .as_ref()
                            .and_then(|attempt| attempt.listing.as_ref().ok())
                            .map_or(0, |listing| listing.urls.len());
                        if winner.is_none() || found.urls.len() > winner_len {
                            winner = Some(Attempt {
                                setting: with_login.clone(),
                                listing: Ok(found.clone()),
                            });
                            original = auth.active().cloned();
                        }
                    }
                }
                if !merge_logins {
                    let attempt = Attempt {
                        setting: with_login,
                        listing,
                    };
                    if better(&attempt.listing, &best.listing) {
                        let resolved = is_fully_readable(&attempt.listing);
                        best = attempt;
                        original = auth.active().cloned();
                        if resolved {
                            break;
                        }
                    } else {
                        // 採用されなかった候補は元の資格情報へ戻す (その間に
                        // 更新されたトークンも失わない)。
                        auth.restore(&mut best.setting, original.as_ref());
                    }
                }
            }
            if merge_logins
                && let Some(mut winner) = winner
            {
                if any_readable {
                    merged.login_walled = false;
                }
                winner.listing = Ok(merged);
                auth.restore(&mut winner.setting, original.as_ref());
                best = winner;
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

    /// Read the stored logins of the site once, when a credential is needed.
    ///
    /// A listing that reads fine never touches the store, and a listing that
    /// needs several credentials reads them once, not once per page.
    async fn ensure_groups(&self, auth: &mut AuthState, setting: &SiteSetting) {
        if auth.groups().is_none() {
            let groups = self.stored_groups(setting).await;
            auth.set_groups(groups);
        }
    }

    /// Ask the store for a fresh browser-issued token and send it.
    ///
    /// Returns `false` when no fresh token was obtained: the site defines no
    /// browser token, the store cannot renew one, or this page already used
    /// its renewal. The caller then keeps its legacy behavior of walking the
    /// stored candidates.
    ///
    /// The token is renewed for the account in effect (by its stable id), so
    /// one account's token never replaces another account's. Only the
    /// request that follows the renewal says whether the site accepted it.
    async fn renew_token(
        &self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        target_url: &str,
        token_attempts: &mut HashMap<String, HashSet<Option<String>>>,
    ) -> bool {
        let Some(cookies) = self.cookies else {
            return false;
        };
        let Some(config) = setting.browser_token.as_ref() else {
            return false;
        };
        // 1 ページの読み直しにつき 1 回だけ。拒否されたトークンが次の更新を
        // 誘発しない。
        if !auth.claim_token_refresh() {
            return false;
        }
        // 保存済みログインは必要なときに 1 回だけ読む。
        self.ensure_groups(auth, setting).await;
        // 匿名の更新も Native と同じトークン保持ログインへ解決してから記録する。
        // 発行に失敗しても、後続の同じログインで予算が復活しないようにする。
        let session_id = auth
            .active()
            .map(|login| login.id.as_str())
            .filter(|id| !id.is_empty())
            .or_else(|| auth.groups().and_then(|groups| {
                groups.iter().find(|login| !login.id.is_empty() && login.has_cookie(&config.cookie_name))
                    .map(|login| login.id.as_str())
            }))
            .map(str::to_owned);
        if let Some(attempted) = token_attempts.get_mut(target_url) {
            if attempted.contains(&session_id) {
                return false;
            }
            attempted.insert(session_id.clone());
        } else {
            token_attempts.insert(target_url.to_owned(), HashSet::from([session_id.clone()]));
        }
        let renewed = cookies
            .refresh_browser_token(
                &setting.domain,
                target_url,
                &config.cookie_name,
                session_id.as_deref(),
            )
            .await;
        match renewed {
            Ok(Some(group)) => {
                tracing::debug!("author listing: browser token renewed, retrying {target_url}");
                auth.apply_refreshed(setting, group)
            }
            // 能力を持たないストア (保存のみ / Worker) や、発行できない
            // ときは更新できない。保存済みログインを従来どおり試す。
            Ok(None) => false,
            Err(error) => {
                // トークンも資格情報もエラー文に現れない。
                tracing::warn!("author listing: browser token renewal failed: {error}");
                false
            }
        }
    }

    /// One page of the listing: fetch it and run the definition's
    /// preprocessing over the body.
    async fn fetch_page(
        &self,
        setting: &SiteSetting,
        policy: &FetchPolicy,
        fetch_url: &str,
        jobs: &mut PreprocessJobs,
    ) -> Result<String> {
        let mut body = fetch_text(
            self.http,
            self.rate_limiter,
            fetch_url,
            policy,
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
            policy,
            src: &mut body,
            encoding: setting.encoding(),
            setting: Some(setting),
            jobs,
            url: fetch_url,
        })
        .await?;
        Ok(body)
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
    ///
    /// A page the site refuses (a browser-verification answer, or a login
    /// wall served with HTTP 200) is read again with a browser-issued token,
    /// once per page read. The pages read before stay: the listing continues
    /// where it was instead of starting over.
    async fn fetch_listing(
        &self,
        setting: &mut SiteSetting,
        auth: &mut AuthState,
        page_url: &str,
        jobs: &mut PreprocessJobs,
        token_attempts: &mut HashMap<String, HashSet<Option<String>>>,
    ) -> Result<Listing> {
        let mut policy = FetchPolicy::for_site(setting);
        let captures = setting.extract_author_url_captures(page_url).unwrap_or_default();
        let mut listing = Listing::default();
        let mut fetch_url = setting.author_fetch_url(page_url);
        let mut declared_total: Option<usize> = None;
        // 宣言件数は並んだエントリを数えるので、重複ありの生件数と突き合わせる。
        let mut listed_works: usize = 0;
        // 訪問済みの URL に戻ったら終わり。ページャが先頭へ戻る定義でも止まる。
        let mut visited: HashSet<String> = HashSet::new();
        loop {
            if !visited.insert(fetch_url.clone()) {
                break;
            }
            // 失効したトークンの更新は 1 ページの読み直しにつき 1 回だけ。
            auth.reset_token_refresh();
            let fetched = self.fetch_page(setting, &policy, &fetch_url, jobs).await;
            let mut body = match fetched {
                Ok(body) => body,
                Err(error) => {
                    // ブラウザ確認の答えだけがトークン更新で直りうる。404 や
                    // 其它の失敗は更新しても直らない。
                    if !is_login_required(&error)
                        || !self.renew_token(setting, auth, &fetch_url, token_attempts).await
                    {
                        return Err(error);
                    }
                    // 更新した資格情報で同じページを読み直す (前のページは
                    // 捨てない)。
                    policy = FetchPolicy::for_site(setting);
                    self.fetch_page(setting, &policy, &fetch_url, jobs).await?
                }
            };
            if setting.is_partial_login_view(&body) {
                listing.partial = true;
            }
            if setting.is_login_wall(&body) {
                // ログイン壁 (HTTP 200) も、更新したトークンで直りうる。
                if !self.renew_token(setting, auth, &fetch_url, token_attempts).await {
                    listing.login_walled = true;
                    break;
                }
                policy = FetchPolicy::for_site(setting);
                let renewed = self.fetch_page(setting, &policy, &fetch_url, jobs).await?;
                if setting.is_partial_login_view(&renewed) {
                    listing.partial = true;
                }
                if setting.is_login_wall(&renewed) {
                    // 更新したトークンでも壁が残る = この資格情報では読めない。
                    listing.login_walled = true;
                    break;
                }
                body = renewed;
            }
            let total = setting.author_page_total(&body);
            if let Some(total) = total {
                declared_total = Some(total);
            }
            let before = listing.urls.len();
            // 宣言件数は並んだエントリを数えるので、重複ありの生件数を追う。
            listed_works += setting.author_novel_entry_count(&body);
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
        // ログイン壁で切れた分は読めない一覧であって欠けではない (壁自体が
        // より正しいエラーを持つ)。部分一覧は欠けの検出対象に含める。
        if let Some(total) = declared_total
            && !listing.login_walled
            && listed_works < total
        {
            // 宣言数より少なく読み切れた = 一覧が欠けている。空/不足を成功にしない。
            return Err(NarouError::SiteSetting(format!(
                "{} の作者一覧を最後まで取得できませんでした ({} / {total} 件): {page_url}",
                setting.sitename,
                listed_works
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
        let mut inside: HashSet<String> = HashSet::new();
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
        urls.retain(|url| !inside.contains(url));
    }

    /// Full episode URLs of the given series, fetched through the definition.
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
            for episode_url in setting.author_series_episode_urls(&body) {
                if !episodes.contains(&episode_url) {
                    episodes.push(episode_url);
                }
            }
        }
        episodes
    }

    /// Full artwork URLs of the given comic series, page by page (Pixiv pages
    /// them in batches of 12) until a page adds nothing.
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
            while let Some(url) =
                setting.author_comic_series_pages_fetch_url(captures, series_id, page_number)
            {
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
                let work_urls = setting.author_comic_series_page_urls(&body);
                if work_urls.is_empty() {
                    break;
                }
                let before = pages.len();
                for work_url in work_urls {
                    if !pages.contains(&work_url) {
                        pages.push(work_url);
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
#[derive(Debug, Default, Clone)]
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

/// Merge one attempt's works into an accumulated listing (`author_login_merge`).
///
/// The anonymous listing and every stored login may expose different subsets
/// (Pixiv hides R-18 works from anonymous requests), so the author's works are
/// the union. `login_walled` is decided by the caller from the attempts that
/// were actually readable.
fn merge_listing(into: &mut Listing, from: &Listing) {
    for url in &from.urls {
        if into.seen.insert(url.clone()) {
            into.urls.push(url.clone());
        }
    }
    for id in &from.series {
        if !into.series.contains(id) {
            into.series.push(id.clone());
        }
    }
    for id in &from.comic_series {
        if !into.comic_series.contains(id) {
            into.comic_series.push(id.clone());
        }
    }
    into.partial |= from.partial;
    into.login_walled |= from.login_walled;
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
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    fn r18_author_lists_all_works_from_novel_list_pages() {
        struct AgeCookieClient(MockHttpClient);
        impl HttpClient for AgeCookieClient {
            fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
                let adult = request.headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("cookie")
                        && value.split(';').any(|cookie| cookie.trim() == "over18=yes")
                });
                if adult {
                    self.0.send(request)
                } else {
                    Box::pin(async {
                        Ok(HttpResponse {
                            status: 200,
                            headers: vec![],
                            body: "年齢確認が必要です".as_bytes().to_vec(),
                        })
                    })
                }
            }

            fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<crate::platform::HttpStreamResponse>> {
                let url = request.url.clone();
                Box::pin(async move {
                    let response = self.send(request).await?;
                    Ok(crate::platform::HttpStreamResponse {
                        status: response.status, url, headers: response.headers,
                        body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
                    })
                })
            }
        }
        let setting = load("novel18.syosetu.com");
        let page = "https://xmypage.syosetu.com/x1525cp/";
        let first = "https://xmypage.syosetu.com/mypage/novellist/xid/x1525cp/";
        let second = "https://xmypage.syosetu.com/mypage/novellist/xid/x1525cp/?xid=671458&all=1&p=2";
        let http = MockHttpClient::new();
        http.add_text(page, 200, r#"<a href="https://novel18.syosetu.com/n0001aa/">最新作</a>"#);
        http.add_text(
            first,
            200,
            r#"<a href="https://novel18.syosetu.com/n0001aa/">作品1</a>
               <a href="https://novel18.syosetu.com/n0002aa/">作品2</a>
               <a href="?xid=671458&amp;all=1&amp;p=1" class="c-pager__item" title="1ページ">1</a>
               <a href="?xid=671458&amp;all=1&amp;p=2" class="c-pager__item" title="次へ">次へ</a>"#,
        );
        http.add_text(
            second,
            200,
            r#"<a href="https://novel18.syosetu.com/n0003aa/">作品3</a>
               <a href="?xid=671458&amp;all=1&amp;p=1" class="c-pager__item" title="前へ">前へ</a>"#,
        );
        let http = AgeCookieClient(http);
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://novel18.syosetu.com/n0001aa/".to_string(),
                "https://novel18.syosetu.com/n0002aa/".to_string(),
                "https://novel18.syosetu.com/n0003aa/".to_string(),
            ],
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
    fn series_exclusion_keeps_other_work_types_with_the_same_id() {
        let setting = load("www.pixiv.net");
        let page = "https://www.pixiv.net/users/6519870";
        let http = MockHttpClient::new();
        http.add_text(
            setting.author_fetch_url(page), 200,
            r#"{"error":false,"body":{"novels":{"42":{},"55":{}},"novelSeries":[{"id":"10"},{"id":"42"}],"mangaSeries":[{"id":"20"}],"illusts":{"42":{},"55":{}},"manga":[]}}"#,
        );
        http.add_text(
            "https://www.pixiv.net/ajax/novel/series/10/content_titles", 200,
            r#"{"error":false,"body":[{"id":"42"}]}"#,
        );
        http.add_text(
            "https://www.pixiv.net/ajax/series/20?p=1&lang=ja", 200,
            r#"{"error":false,"body":{"page":{"series":[{"workId":"55","order":1}]}}}"#,
        );
        http.add_text(
            "https://www.pixiv.net/ajax/series/20?p=2&lang=ja", 200,
            r#"{"error":false,"body":{"page":{"series":[]}}}"#,
        );
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(urls, vec![
            "https://www.pixiv.net/novel/show.php?id=55",
            "https://www.pixiv.net/novel/series/10",
            "https://www.pixiv.net/novel/series/42",
            "https://www.pixiv.net/ajax/series/20",
            "https://www.pixiv.net/artworks/42",
        ]);
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

        fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<crate::platform::HttpStreamResponse>> {
            let url = request.url.clone();
            Box::pin(async move {
                let response = self.send(request).await?;
                Ok(crate::platform::HttpStreamResponse {
                    status: response.status, url, headers: response.headers,
                    body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
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
        login_for("gated.example", cookie)
    }

    fn login_for(site: &str, cookie: &str) -> LoginGroup {
        LoginGroup::new(
            site,
            vec![HostCookie {
                host: site.to_string(),
                cookie: cookie.to_string(),
            }],
        )
    }

    /// Anonymous requests see only `book/1`; a logged-in request also sees
    /// `book/2` (Pixiv hides R-18 works from anonymous responses the same way).
    struct MergeAuthorClient {
        cookies: Mutex<Vec<Option<String>>>,
    }

    impl HttpClient for MergeAuthorClient {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let cookie = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone());
                self.cookies.lock().push(cookie.clone());
                let body = if cookie.is_some() {
                    r#"<a href="https://merge.example/book/1">1</a><a href="https://merge.example/book/2">2</a>"#
                } else {
                    r#"<a href="https://merge.example/book/1">1</a>"#
                };
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/html".into())],
                    body: body.as_bytes().to_vec(),
                })
            })
        }

        fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<crate::platform::HttpStreamResponse>> {
            let url = request.url.clone();
            Box::pin(async move {
                let response = self.send(request).await?;
                Ok(crate::platform::HttpStreamResponse {
                    status: response.status, url, headers: response.headers,
                    body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
                })
            })
        }
    }

    const MERGE: &str = r#"
name: Merge
domain: merge.example
top_url: https://merge.example
url: https://merge\.example/book/(?<ncode>\d+)
author_url: ^https?://merge\.example/user/(?<author_id>\d+)/?
author_api_url: https://merge.example/user/\k<author_id>
author_novel_pattern: '(?<novel_url>https?://merge\.example/book/\d+)'
author_login_merge: yes
sitename: Merge
toc_url: https://merge.example/book/\k<ncode>
"#;

    #[test]
    fn author_login_merge_adds_works_visible_only_with_a_login() {
        let definitions = SiteSetting::load_bundled(&[MERGE]).unwrap();
        let setting = &definitions[0];
        let page = "https://merge.example/user/5/";
        let http = MergeAuthorClient {
            cookies: Mutex::new(Vec::new()),
        };
        let cookies = MemoryCookieStore::new();
        futures::executor::block_on(
            cookies.save_groups("merge.example", &[login_for("merge.example", "session=ok")]),
        )
        .unwrap();

        let urls = futures::executor::block_on(
            AuthorListing::new(&http, &FakeRateLimiter::new(), Some(&cookies))
                .works_for(&[setting], page),
        )
        .unwrap();
        assert_eq!(
            urls,
            vec![
                "https://merge.example/book/1".to_string(),
                "https://merge.example/book/2".to_string(),
            ]
        );
        let seen = http.cookies.lock().clone();
        assert_eq!(seen.len(), 2, "匿名 + 保存ログイン: {seen:?}");
        assert_eq!(seen[0], None);
        assert_eq!(seen[1].as_deref(), Some("session=ok"));
    }

    #[test]
    fn author_login_merge_is_opt_in() {
        let without_merge = MERGE.replace("author_login_merge: yes", "");
        let definitions = SiteSetting::load_bundled(&[without_merge.as_str()]).unwrap();
        let setting = &definitions[0];
        let page = "https://merge.example/user/5/";
        let http = MergeAuthorClient {
            cookies: Mutex::new(Vec::new()),
        };
        let cookies = MemoryCookieStore::new();
        futures::executor::block_on(
            cookies.save_groups("merge.example", &[login_for("merge.example", "session=ok")]),
        )
        .unwrap();

        let urls = futures::executor::block_on(
            AuthorListing::new(&http, &FakeRateLimiter::new(), Some(&cookies))
                .works_for(&[setting], page),
        )
        .unwrap();
        assert_eq!(urls, vec!["https://merge.example/book/1".to_string()]);
        assert_eq!(http.cookies.lock().len(), 1, "匿名一覧が完全ならログインは試さない");
    }

    /// Anonymous Pixiv responses omit R-18 novels; the saved login sees them.
    struct PixivAuthorClient {
        cookies: Mutex<Vec<Option<String>>>,
    }

    impl HttpClient for PixivAuthorClient {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let cookie = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone());
                self.cookies.lock().push(cookie.clone());
                let novels = if cookie.is_some() {
                    r#"{"111":{},"222":{}}"#
                } else {
                    r#"{"111":{}}"#
                };
                let body = format!(
                    r#"{{"error":false,"body":{{"novels":{novels},"novelSeries":[],"mangaSeries":[],"illusts":[],"manga":[]}}}}"#
                );
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "application/json".into())],
                    body: body.into_bytes(),
                })
            })
        }

        fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<crate::platform::HttpStreamResponse>> {
            let url = request.url.clone();
            Box::pin(async move {
                let response = self.send(request).await?;
                Ok(crate::platform::HttpStreamResponse {
                    status: response.status, url, headers: response.headers,
                    body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
                })
            })
        }
    }

    #[test]
    fn bundled_pixiv_author_listing_merges_r18_works_from_a_stored_login() {
        let setting = load("www.pixiv.net");
        assert!(setting.author_login_merge, "Pixiv 定義でマージを有効にする");
        let page = "https://www.pixiv.net/users/69616058";
        let http = PixivAuthorClient {
            cookies: Mutex::new(Vec::new()),
        };
        let cookies = MemoryCookieStore::new();
        futures::executor::block_on(cookies.save_groups(
            "www.pixiv.net",
            &[login_for("www.pixiv.net", "PHPSESSID=x")],
        ))
        .unwrap();

        let urls = futures::executor::block_on(
            AuthorListing::new(&http, &FakeRateLimiter::new(), Some(&cookies))
                .works_for(&[&setting], page),
        )
        .unwrap();
        assert_eq!(
            urls,
            vec![
                "https://www.pixiv.net/novel/show.php?id=111".to_string(),
                "https://www.pixiv.net/novel/show.php?id=222".to_string(),
            ],
            "匿名分とログイン分を統合する: {urls:?}"
        );
        assert_eq!(http.cookies.lock().len(), 2);
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

    /// A site whose author listing is paged and can be refused by a browser
    /// check. The token itself is site data: the definition only says which
    /// cookie carries it.
    const TOKEN_SITE: &str = r#"
name: TokenSite
domain: token.example
top_url: https://token.example
url: https://token\.example/story/(?<ncode>\d+)
author_url: ^https?://token\.example/user/(?<author_id>\d+)
author_api_url: https://token.example/user/\k<author_id>/story
author_novel_pattern: '(?<novel_url>https://token\.example/story/\d+)'
author_next_pattern: '(?<author_next>https://token\.example/user/\d+/story\?p=\d+)'
login_pattern: 'ログインが必要'
login_partial_pattern: 'partial'
browser_token:
  script_url: https://token.example/token.js
  function: issueToken
  cookie_name: token
sitename: TokenSite
toc_url: https://token.example/story/\k<ncode>
"#;

    /// One page of the token site's listing.
    #[derive(Clone)]
    struct TokenPage {
        /// The browser token the request must carry. `None` = the page does
        /// not ask for one.
        token: Option<String>,
        /// The page refuses a request that carries a login cookie.
        wall_with_cookie: bool,
        /// The works the page lists.
        works: Vec<u32>,
        /// The next page, when there is one.
        next: Option<String>,
        /// The body says the listing is incomplete without a login.
        partial: bool,
    }

    impl TokenPage {
        fn new(works: &[u32]) -> Self {
            Self {
                token: None,
                wall_with_cookie: false,
                works: works.to_vec(),
                next: None,
                partial: false,
            }
        }

        /// The page demands this browser token.
        fn guarded(mut self, token: &str) -> Self {
            self.token = Some(token.to_string());
            self
        }

        /// The page refuses every request that carries a login cookie.
        fn walled_for_logins(mut self) -> Self {
            self.wall_with_cookie = true;
            self
        }

        /// The page says the listing is incomplete without a login.
        fn partial(mut self) -> Self {
            self.partial = true;
            self
        }

        /// The page links to the next one.
        fn with_next(mut self, next: &str) -> Self {
            self.next = Some(next.to_string());
            self
        }
    }

    /// The body of one listing page.
    fn token_page_body(page: &TokenPage) -> String {
        let mut body = String::new();
        if page.partial {
            body.push_str("<p>partial</p>");
        }
        for work in &page.works {
            body.push_str(&format!(
                r#"<a href="https://token.example/story/{work}">作品</a>"#
            ));
        }
        if let Some(next) = &page.next {
            body.push_str(&format!(r#"<a href="{next}">次へ</a>"#));
        }
        body
    }

    /// A listing server whose answer depends on the cookie of the request.
    struct TokenAuthorClient {
        pages: HashMap<String, TokenPage>,
        /// The URL and the cookie of every request, in order.
        seen: Mutex<Vec<(String, Option<String>)>>,
    }

    impl TokenAuthorClient {
        fn new(pages: &[(&str, TokenPage)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(url, page)| ((*url).to_string(), page.clone()))
                    .collect(),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// The cookies the server saw for `url`, in order.
        fn cookies_for(&self, url: &str) -> Vec<Option<String>> {
            self.seen
                .lock()
                .iter()
                .filter(|(seen, _)| seen == url)
                .map(|(_, cookie)| cookie.clone())
                .collect()
        }
    }

    impl HttpClient for TokenAuthorClient {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let cookie = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone());
                self.seen.lock().push((request.url.clone(), cookie.clone()));
                let Some(page) = self.pages.get(&request.url) else {
                    return Ok(HttpResponse {
                        status: 404,
                        headers: vec![],
                        body: Vec::new(),
                    });
                };
                let accepted = match &page.token {
                    Some(token) => cookie
                        .as_deref()
                        .is_some_and(|value| value.contains(&format!("token={token}"))),
                    None => !page.wall_with_cookie || cookie.is_none(),
                };
                let body = if accepted {
                    token_page_body(page)
                } else {
                    "ログインが必要".to_string()
                };
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                    body: body.into_bytes(),
                })
            })
        }

        fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<crate::platform::HttpStreamResponse>> {
            let url = request.url.clone();
            Box::pin(async move {
                let response = self.send(request).await?;
                Ok(crate::platform::HttpStreamResponse {
                    status: response.status, url, headers: response.headers,
                    body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
                })
            })
        }
    }

    /// A store that hands out browser tokens and counts what it was asked.
    struct TokenStore {
        groups: Mutex<BTreeMap<String, Vec<LoginGroup>>>,
        /// How often the stored logins were read.
        loads: AtomicUsize,
        /// How often a browser token was renewed.
        renewals: AtomicUsize,
        /// The tokens handed out, in order. `None` = no token available.
        handouts: Mutex<VecDeque<Option<String>>>,
        /// The login id every renewal was asked for.
        sessions: Mutex<Vec<Option<String>>>,
        fail_first: bool,
    }

    impl TokenStore {
        fn new(logins: &[LoginGroup], handouts: &[Option<&str>]) -> Self {
            let mut groups: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
            for login in logins {
                groups
                    .entry(login.site.clone())
                    .or_default()
                    .push(login.clone());
            }
            Self {
                groups: Mutex::new(groups),
                loads: AtomicUsize::new(0),
                renewals: AtomicUsize::new(0),
                handouts: Mutex::new(
                    handouts
                        .iter()
                        .map(|token| token.map(str::to_string))
                        .collect(),
                ),
                sessions: Mutex::new(Vec::new()),
                fail_first: false,
            }
        }

        fn renewals(&self) -> usize {
            self.renewals.load(Ordering::SeqCst)
        }

        fn loads(&self) -> usize {
            self.loads.load(Ordering::SeqCst)
        }

        /// The login id every renewal was asked for, in order.
        fn sessions(&self) -> Vec<Option<String>> {
            self.sessions.lock().clone()
        }
    }

    impl CookieStore for TokenStore {
        fn load_groups<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<Vec<LoginGroup>>> {
            Box::pin(async move {
                self.loads.fetch_add(1, Ordering::SeqCst);
                Ok(self
                    .groups
                    .lock()
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
                self.groups.lock().insert(site.to_string(), groups.to_vec());
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
            Box::pin(async move { Ok(self.groups.lock().remove(site).is_some()) })
        }

        fn list_groups(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>> {
            Box::pin(async move { Ok(self.groups.lock().clone()) })
        }

        fn save_all_groups<'a>(
            &'a self,
            groups: &'a BTreeMap<String, Vec<LoginGroup>>,
        ) -> PlatformFuture<'a, Result<()>> {
            Box::pin(async move {
                *self.groups.lock() = groups.clone();
                Ok(())
            })
        }

        fn merge_all_groups<'a>(
            &'a self,
            groups: &'a BTreeMap<String, Vec<LoginGroup>>,
        ) -> PlatformFuture<'a, Result<usize>> {
            Box::pin(async move { Ok(groups.len()) })
        }

        fn renew_response_cookies<'a>(
            &'a self,
            _sent_cookie: &'a str,
            _set_cookie: &'a [String],
        ) -> PlatformFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }

        /// Hand out the next token for the login in effect.
        ///
        /// The account keeps its identity and its other cookies; only the
        /// token is replaced.
        fn refresh_browser_token<'a>(
            &'a self,
            site: &'a str,
            _target_url: &'a str,
            cookie_name: &'a str,
            session_id: Option<&'a str>,
        ) -> PlatformFuture<'a, Result<Option<LoginGroup>>> {
            Box::pin(async move {
                let attempt = self.renewals.fetch_add(1, Ordering::SeqCst);
                self.sessions.lock().push(session_id.map(str::to_string));
                if self.fail_first && attempt == 0 {
                    self.handouts.lock().pop_front();
                    return Err(NarouError::Login("fixture browser failure".to_string()));
                }
                let Some(token) = self.handouts.lock().pop_front() else {
                    return Ok(None);
                };
                let Some(token) = token else {
                    // トークンを発行できない。
                    return Ok(None);
                };
                let mut groups = self.groups.lock();
                let Some(list) = groups.get_mut(site) else {
                    return Ok(None);
                };
                let index = session_id
                    .and_then(|id| list.iter().position(|group| group.id == id))
                    .or_else(|| {
                        list.iter().position(|group| {
                            group
                                .cookies
                                .iter()
                                .any(|entry| entry.cookie.contains(cookie_name))
                        })
                    });
                let Some(index) = index else {
                    return Ok(None);
                };
                let group = &mut list[index];
                for entry in group.cookies.iter_mut() {
                    entry.cookie = replace_cookie_value(&entry.cookie, cookie_name, &token);
                }
                Ok(Some(group.clone()))
            })
        }
    }

    /// Replace one cookie's value and keep the other ones.
    fn replace_cookie_value(header: &str, name: &str, value: &str) -> String {
        let mut pairs = Vec::new();
        for part in header.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((key, old)) = part.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = if key == name { value } else { old.trim() };
            pairs.push(format!("{key}={value}"));
        }
        pairs.join("; ")
    }

    /// A stored login of the token site.
    fn token_login(id: &str, cookie: &str) -> LoginGroup {
        LoginGroup::new(
            "token.example",
            vec![HostCookie {
                host: "token.example".to_string(),
                cookie: cookie.to_string(),
            }],
        )
        .with_id(id)
    }

    /// The listing of the token site, read with the store in effect.
    fn token_works(
        http: &dyn HttpClient,
        store: &TokenStore,
        setting: &SiteSetting,
        page: &str,
    ) -> Result<Vec<String>> {
        futures::executor::block_on(
            AuthorListing::new(http, &FakeRateLimiter::new(), Some(store)).works_for(&[setting], page),
        )
    }

    #[test]
    fn a_later_author_page_is_read_again_with_a_fresh_browser_token() {
        // 2 頁目だけがブラウザ確認を要求する。1 頁目で終端にせず、前のページの
        // 作品を捨てずに、同じ一覧をトークン更新後まで読み続ける。
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let second = "https://token.example/user/5/story?p=2";
        let third = "https://token.example/user/5/story?p=3";
        let http = TokenAuthorClient::new(&[
            (page, TokenPage::new(&[1, 2]).with_next(second)),
            (second, TokenPage::new(&[3, 4]).guarded("fresh-1").with_next(third)),
            (third, TokenPage::new(&[5, 6]).guarded("fresh-1")),
        ]);
        let store = TokenStore::new(
            &[token_login("acct-1", "sid=keep; token=stale")],
            &[Some("fresh-1")],
        );

        let urls = token_works(&http, &store, setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://token.example/story/1".to_string(),
                "https://token.example/story/2".to_string(),
                "https://token.example/story/3".to_string(),
                "https://token.example/story/4".to_string(),
                "https://token.example/story/5".to_string(),
                "https://token.example/story/6".to_string(),
            ],
            "更新前の作品も残す: {urls:?}"
        );
        assert_eq!(store.renewals(), 1, "2 頁目のために 1 回だけ更新する");
        assert_eq!(store.loads(), 1, "資格情報は必要なときに 1 回だけ読む");
        let seen = http.cookies_for(third);
        assert_eq!(seen.len(), 1, "3 頁目は 1 回だけ要求する: {seen:?}");
        assert!(
            seen[0]
                .as_deref()
                .is_some_and(|cookie| cookie.contains("token=fresh-1")),
            "3 頁目は更新した資格情報で読む: {seen:?}"
        );
    }

    #[test]
    fn two_invalidated_pages_are_recovered_with_one_renewal_each() {
        // 2 頁目と 3 頁目が別々に失効する。1 ページにつき 1 回だけ更新する。
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let second = "https://token.example/user/5/story?p=2";
        let third = "https://token.example/user/5/story?p=3";
        let http = TokenAuthorClient::new(&[
            (page, TokenPage::new(&[1]).with_next(second)),
            (second, TokenPage::new(&[2]).guarded("fresh-1").with_next(third)),
            (third, TokenPage::new(&[3]).guarded("fresh-2")),
        ]);
        let store = TokenStore::new(
            &[token_login("acct-1", "token=stale")],
            &[Some("fresh-1"), Some("fresh-2")],
        );

        let urls = token_works(&http, &store, setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://token.example/story/1".to_string(),
                "https://token.example/story/2".to_string(),
                "https://token.example/story/3".to_string(),
            ],
            "失効したページも読み直す: {urls:?}"
        );
        assert_eq!(store.renewals(), 2, "失効したページごとに 1 回ずつ更新する");
        assert_eq!(
            http.cookies_for(third),
            vec![Some("token=fresh-1".to_string()), Some("token=fresh-2".to_string())],
            "3 頁目の確認要求後は 2 つ目のトークンで再試行する"
        );
    }

    #[test]
    fn a_renewal_keeps_the_account_and_its_stable_id() {
        // 生效中のログインの ID で更新し、そのアカウントの他の cookie は残す。
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let second = "https://token.example/user/5/story?p=2";
        let http = TokenAuthorClient::new(&[
            (page, TokenPage::new(&[1]).guarded("stale").with_next(second)),
            (second, TokenPage::new(&[2]).guarded("fresh")),
        ]);
        // 1 頁目は保存済み Cookie で読め、別 URL の 2 頁目でトークンを更新する。
        let store = TokenStore::new(
            &[token_login("acct-1", "sid=keep; token=stale")],
            &[None, Some("fresh")],
        );

        let urls = token_works(&http, &store, setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://token.example/story/1".to_string(),
                "https://token.example/story/2".to_string(),
            ],
            "保存済みログインの資格情報で最後まで読む: {urls:?}"
        );
        assert_eq!(
            store.sessions(),
            vec![Some("acct-1".to_string()), Some("acct-1".to_string())],
            "生效中のログインの安定 ID で更新する"
        );
        let seen = http.cookies_for(second);
        let last = seen.last().and_then(|cookie| cookie.as_deref()).unwrap_or("");
        assert!(
            last.contains("sid=keep") && last.contains("token=fresh"),
            "アカウントの cookie を残したままトークンだけ差し替える: {seen:?}"
        );
    }

    #[test]
    fn a_rejected_renewal_stops_the_page() {
        // 発行されたトークンが拒否されても、同じページで更新し直さない。
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let second = "https://token.example/user/5/story?p=2";
        let http = TokenAuthorClient::new(&[
            (page, TokenPage::new(&[1]).with_next(second)),
            (second, TokenPage::new(&[2]).guarded("wanted")),
        ]);
        // 保存済みログインはない: 匿名の試行だけになる。
        let store = TokenStore::new(&[], &[Some("fresh")]);

        let error = token_works(&http, &store, setting, page).unwrap_err();
        assert!(matches!(error, NarouError::SiteSetting(_)), "got {error:?}");
        assert!(error.to_string().contains("ログイン"), "got {error}");
        assert_eq!(store.renewals(), 1, "同じページで更新し直さない");
    }

    #[test]
    fn a_missing_page_is_not_renewed() {
        // ページが無い (404) のは認証の問題ではない: 更新しない。
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let http = TokenAuthorClient::new(&[]);
        let store = TokenStore::new(
            &[token_login("acct-1", "token=stale")],
            &[Some("fresh")],
        );

        let error = token_works(&http, &store, setting, page).unwrap_err();
        assert!(matches!(error, NarouError::NotFound(_)), "got {error:?}");
        assert_eq!(store.renewals(), 0, "ページが無いのは更新で直らない");
    }

    #[test]
    fn a_readable_partial_listing_survives_a_failed_optional_renewal() {
        // 匿名では部分一覧しか読めない。保存済みログインの試行が更新に失敗して
        // 終わっても、読み終えた一覧は捨てない。
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let http = TokenAuthorClient::new(&[(
            page,
            TokenPage::new(&[1]).partial().walled_for_logins(),
        )]);
        let store = TokenStore::new(&[token_login("acct-1", "sid=keep")], &[]);

        let urls = token_works(&http, &store, setting, page).unwrap();
        assert_eq!(
            urls,
            vec!["https://token.example/story/1".to_string()],
            "読める部分一覧は残す: {urls:?}"
        );
        assert_eq!(store.renewals(), 1, "任意の試行でも更新は 1 回だけ試す");
    }

    #[test]
    fn anonymous_renewal_failure_does_not_launch_again_for_the_same_login() {
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let http = TokenAuthorClient::new(&[(page, TokenPage::new(&[1]).guarded("fresh"))]);
        let mut store = TokenStore::new(
            &[token_login("acct-1", "sid=primary; token=stale")],
            &[None, Some("fresh")],
        );
        store.fail_first = true;

        let result = token_works(&http, &store, setting, page);
        assert_eq!(store.renewals(), 1, "発行失敗も同じページ・ログインの予算を消費する");
        assert!(result.is_err(), "同じログインで発行を繰り返さない");
    }

    #[test]
    fn anonymous_renewal_failure_still_allows_a_different_login() {
        let definitions = SiteSetting::load_bundled(&[TOKEN_SITE]).unwrap();
        let setting = &definitions[0];
        let page = "https://token.example/user/5/story";
        let http = TokenAuthorClient::new(&[(page, TokenPage::new(&[1]).guarded("fresh-b"))]);
        let mut store = TokenStore::new(
            &[
                token_login("acct-1", "sid=primary; token=stale-a"),
                token_login("acct-2", "sid=secondary; token=stale-b"),
            ],
            &[None, Some("fresh-b")],
        );
        store.fail_first = true;

        let urls = token_works(&http, &store, setting, page).unwrap();
        assert_eq!(urls, ["https://token.example/story/1"]);
        assert_eq!(store.sessions(), [Some("acct-1".to_string()), Some("acct-2".to_string())]);
        assert!(http.cookies_for(page).last().unwrap().as_deref().unwrap().contains("sid=secondary"));
    }

    #[test]
    fn akatsuki_author_listing_follows_relative_next_links() {
        // 「次へ」はサイトルート相対の rel=next リンク。これが無かった頃は
        // 1 ページ目だけで終端と判定し、残りを黙って落としていた。
        let setting = load("www.akatsuki-novels.com");
        let page = "https://www.akatsuki-novels.com/users/aknf43btk";

        let http = MockHttpClient::new();
        http.add_text(
            page,
            200,
            concat!(
                r#"<h2 id="novels-heading">投稿作品</h2>"#,
                r#"<a href="/novels/view/2795" class="min-w-0 text-base font-semibold">作品1</a>"#,
                r#"<a href="/users/aknf43btk?page=2" rel="next">次へ</a>"#,
            ),
        );
        http.add_text(
            "https://www.akatsuki-novels.com/users/aknf43btk?page=2",
            200,
            concat!(
                r#"<a href="/novels/view/23837" class="min-w-0 text-base font-semibold">作品2</a>"#,
                r#"<a href="/users/aknf43btk?page=1" rel="prev">前へ</a>"#,
                r#"<a href="/users/aknf43btk?page=3" rel="next">次へ</a>"#,
            ),
        );
        http.add_text(
            "https://www.akatsuki-novels.com/users/aknf43btk?page=3",
            200,
            r#"<a href="/novels/view/32207" class="min-w-0 text-base font-semibold">作品3</a>"#,
        );

        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://www.akatsuki-novels.com/novels/view/2795".to_string(),
                "https://www.akatsuki-novels.com/novels/view/23837".to_string(),
                "https://www.akatsuki-novels.com/novels/view/32207".to_string(),
            ],
            "相対 rel=next を絶対 URL へ解決して終端まで辿る"
        );
    }

    #[test]
    fn a_declared_total_above_the_read_count_fails_instead_of_truncating() {
        // 読み切った一覧が宣言件数より少ない = 一覧が欠けている。
        let setting = load("www.akatsuki-novels.com");
        let page = "https://www.akatsuki-novels.com/users/aknf43btk";

        let http = MockHttpClient::new();
        http.add_text(
            page,
            200,
            concat!(
                r#"<h2 id="novels-heading">投稿作品</h2>"#,
                r#"<p class="mt-1 text-sm text-muted">8,597作品（1 / 430ページ）</p>"#,
                r#"<a href="/novels/view/2795" class="min-w-0 text-base font-semibold">作品1</a>"#,
                r#"<a href="/novels/view/23837" class="min-w-0 text-base font-semibold">作品2</a>"#,
            ),
        );

        let error = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap_err();
        assert!(
            error.to_string().contains("2 / 8597"),
            "宣言件数との不足を報告する: {error}"
        );
    }

    #[test]
    fn a_work_listed_twice_still_satisfies_the_declared_total() {
        // 同じ作品が一覧に 2 度出ても、宣言件数は重複ありのエントリ数と一致する。
        let setting = load("www.akatsuki-novels.com");
        let page = "https://www.akatsuki-novels.com/users/aknf43btk";

        let http = MockHttpClient::new();
        http.add_text(
            page,
            200,
            concat!(
                r#"<h2 id="novels-heading">投稿作品</h2>"#,
                r#"<p class="mt-1 text-sm text-muted">3作品（1 / 1ページ）</p>"#,
                r#"<a href="/novels/view/2795" class="min-w-0 text-base font-semibold">作品1</a>"#,
                // 同じ作品が一覧に 2 度並ぶ (宣言件数 3 = エントリ 3)。
                r#"<a href="/novels/view/2795" class="min-w-0 text-base font-semibold">作品1</a>"#,
                r#"<a href="/novels/view/23837" class="min-w-0 text-base font-semibold">作品2</a>"#,
            ),
        );

        // 宣言 3 件 = エントリ 3 件なのでエラーにならない。一意な作品は 2 件。
        let urls = works(&http, &FakeRateLimiter::new(), &setting, page).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://www.akatsuki-novels.com/novels/view/2795".to_string(),
                "https://www.akatsuki-novels.com/novels/view/23837".to_string(),
            ],
            "同じ作品の重複エントリは宣言件数に含めて数え、一意な一覧へ落とす"
        );
    }

    #[test]
    fn a_login_wall_mid_walk_stays_a_login_error_not_a_truncation() {
        // ページ送りの途中のログイン壁は欠けではなく読めない一覧。件数チェックで隠さない。
        const WALLED_PAGED: &str = r#"
name: Walled
domain: walled.example
top_url: https://walled.example
url: https://walled\.example/book/(?<ncode>\d+)
author_url: ^https?://walled\.example/user/(?<author_id>\d+)/?
author_api_url: https://walled.example/user/\k<author_id>
author_novel_pattern: '(?<novel_url>https?://walled\.example/book/\d+)'
author_next_pattern: '(?<author_next>https?://walled\.example/user/\d+\?page=\d+)'
author_page_total_pattern: '(?<count>\d+)作品'
sitename: Walled
toc_url: https://walled.example/book/\k<ncode>
login_pattern: 'ログインが必要です'
"#;
        let definitions = SiteSetting::load_bundled(&[WALLED_PAGED]).unwrap();
        let setting = &definitions[0];
        let page = "https://walled.example/user/7/";
        let first = setting.author_fetch_url(page);

        let http = MockHttpClient::new();
        http.add_text(
            &first,
            200,
            concat!(
                r#"<p>5作品</p>"#,
                r#"<a href="https://walled.example/book/1">w1</a>"#,
                r#"<a href="https://walled.example/book/2">w2</a>"#,
                r#"<a href="https://walled.example/user/7?page=2">next</a>"#,
            ),
        );
        http.add_text(
            "https://walled.example/user/7?page=2",
            200,
            "ログインが必要です",
        );

        let error = works(&http, &FakeRateLimiter::new(), setting, page).unwrap_err();
        assert!(
            error.to_string().contains("ログイン"),
            "壁の報告が件数不足のエラーに化けない: {error}"
        );
        assert!(
            !error.to_string().contains("最後まで取得"),
            "壁を欠けた一覧と報告しない: {error}"
        );
    }
}

