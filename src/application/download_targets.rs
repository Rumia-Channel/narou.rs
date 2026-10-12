//! Author- and series-to-novel target expansion shared by CLI downloads and
//! Worker planning.
//!
//! The expander borrows the same transport, rate limiter, effective YAML
//! definitions and login store as the downloader, so CLI downloads and Worker
//! request/execution paths expand exactly the same targets through one
//! implementation. Only URLs the definitions declare as author pages or series
//! are touched: a plain novel URL is left to the ordinary single-novel path.

use std::collections::HashSet;

use crate::application::{JobKind, JobPlan, JobTarget};
use crate::downloader::author_listing::AuthorListing;
use crate::downloader::http_policy::{FetchPolicy, fetch_text};
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::util::pretreatment_source;
use crate::error::{NarouError, Result};
use crate::platform::{CookieStore, HttpClient, RateLimiter};

/// Borrows the downloader's transport, rate limiter and effective definitions,
/// without constructing its storage or per-novel state.
pub struct DownloadTargetExpander<'a> {
    http: &'a dyn HttpClient,
    rate_limiter: &'a dyn RateLimiter,
    site_settings: &'a [SiteSetting],
    cookies: Option<&'a dyn CookieStore>,
}

impl<'a> DownloadTargetExpander<'a> {
    pub fn new(
        http: &'a dyn HttpClient,
        rate_limiter: &'a dyn RateLimiter,
        site_settings: &'a [SiteSetting],
    ) -> Self {
        Self {
            http,
            rate_limiter,
            site_settings,
            cookies: None,
        }
    }

    /// Inject the login store author pages may need (CLI: the downloader's
    /// inventory; Worker: D1). Callers that never expand author URLs can omit
    /// it and keep the previous anonymous behavior.
    pub fn with_cookie_store(mut self, cookies: Option<&'a dyn CookieStore>) -> Self {
        self.cookies = cookies;
        self
    }

    /// Whether this expander does anything with `target`: a series URL, or an
    /// author page the definitions recognise. A plain novel URL is handled by
    /// the ordinary single-novel path and is not "recognised" here.
    pub fn recognizes(&self, target: &str) -> bool {
        self.site_settings
            .iter()
            .any(|s| s.matches_series_url(target) || s.matches_author_url(target))
    }

    /// `None` leaves an ordinary URL, ID, ncode or title unchanged. A matching
    /// series must yield at least one novel, and an author page whose listing
    /// cannot be read (login wall, fetch failure, definition without a listing)
    /// is an error — neither may fall through to the single-novel downloader.
    /// An author listing that reads fine and simply has no works is
    /// `Some(vec![])`: the site says the author has nothing yet.
    pub async fn expand(&self, target: &str) -> Result<Option<Vec<String>>> {
        if !target.starts_with("http://") && !target.starts_with("https://") {
            return Ok(None);
        }
        // 小説 URL (単体取得) とシリーズ URL を作者 URL の規則より先に解決する:
        // 定義が重なっていても、既存の解釈を変えない。
        if let Some(setting) = self
            .site_settings
            .iter()
            .find(|s| s.matches_series_url(target))
        {
            return self.expand_series(setting, target).await.map(Some);
        }
        if self.site_settings.iter().any(|s| s.matches_url(target)) {
            return Ok(None);
        }
        let matching = self.author_settings(target);
        if matching.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.discover(&matching, target).await?))
    }

    /// Every work URL of an author page, from all definitions that recognise it
    /// (`author_url`), merged in definition order. Definitions the page does not
    /// match contribute nothing; when no definition can be read at all the
    /// failure is returned instead of an empty author.
    pub async fn author_works(&self, page_url: &str) -> Result<Vec<String>> {
        let matching = self.author_settings(page_url);
        self.discover(&matching, page_url).await
    }

    async fn discover(&self, settings: &[&SiteSetting], page_url: &str) -> Result<Vec<String>> {
        AuthorListing::new(self.http, self.rate_limiter, self.cookies)
            .works_for(settings, page_url)
            .await
    }

    fn author_settings(&self, page_url: &str) -> Vec<&'a SiteSetting> {
        self.site_settings
            .iter()
            .filter(|setting| setting.matches_author_url(page_url))
            .collect()
    }

    /// Series members, fetched through the definition. `series_item_url`
    /// decides what counts; a definition without it cannot expand anything.
    async fn expand_series(&self, setting: &SiteSetting, target: &str) -> Result<Vec<String>> {
        let pattern = setting.compile_series_item_pattern().ok_or_else(|| {
            NarouError::SiteSetting(format!("No series_item_url pattern defined: {}", setting.name))
        })?;
        let mut body = fetch_text(
            self.http,
            self.rate_limiter,
            target,
            &FetchPolicy::for_site(setting),
            Some(setting.encoding()),
        )
        .await?;
        pretreatment_source(&mut body, setting.encoding(), None);

        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for caps in pattern.captures_iter(&body) {
            if let Some(url) = setting.series_item_url_from_captures(target, &pattern, &caps)
                && seen.insert(url.clone())
            {
                targets.push(url);
            }
        }
        if targets.is_empty() {
            return Err(NarouError::NotFound(format!("No novels found in series URL: {target}")));
        }
        Ok(targets)
    }

    /// Borrowing leaves an ordinary plan untouched, without cloning its target
    /// or options. Expanded members each own the original download options.
    pub async fn expand_plan(&self, plan: &JobPlan) -> Result<Option<Vec<JobPlan>>> {
        let urls = match (&plan.kind, &plan.target) {
            (JobKind::Download, JobTarget::Url(url)) => self.expand(url).await?,
            _ => None,
        };
        Ok(urls.map(|urls| {
            urls.into_iter()
                .map(|url| JobPlan {
                    kind: plan.kind,
                    target: JobTarget::Url(url),
                    options: plan.options.clone(),
                })
                .collect()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::{FakeRateLimiter, MockHttpClient};

    fn definition(yaml: &str) -> Vec<SiteSetting> {
        SiteSetting::load_bundled(&[yaml]).expect("test definition")
    }

    const AUTHOR_SITE: &str = r#"
name: AuthorSite
domain: author.example
top_url: https://author.example
url: https://author\.example/book/(?<ncode>\d+)
series_url: https://author\.example/series/(?<series>\d+)
series_item_url: (?:"|')(?<href>/book/\d+)(?:"|')
author_url: ^https?://author\.example/users/(?<user_id>\d+)
author_novel_pattern: '(?<novel_url>https?://author\.example/book/\d+)'
sitename: AuthorSite
toc_url: https://author.example/book/\k<ncode>
"#;

    #[tokio::test]
    async fn author_urls_expand_to_their_works_without_duplicates() {
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        let sites = definition(AUTHOR_SITE);
        let target = "https://author.example/users/7";
        http.add_text(
            target,
            200,
            r#"<a href="https://author.example/book/1">first</a>
               <a href="https://author.example/book/2">second</a>
               <a href="https://author.example/book/1">duplicate</a>"#,
        );
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
        assert!(expander.recognizes(target));
        assert_eq!(
            expander.expand(target).await.unwrap(),
            Some(vec![
                "https://author.example/book/1".to_string(),
                "https://author.example/book/2".to_string(),
            ])
        );
        assert_eq!(http.request_count(), 1);
    }

    const OVERLAP_SITE: &str = r#"
name: Overlap
domain: overlap.example
top_url: https://overlap.example
url: https://overlap\.example/users/(?<ncode>\d+)/book
series_url: https://overlap\.example/users/\d+/list
series_item_url: href="(?<href>/book/\d+)"
author_url: ^https?://overlap\.example/users/(?<user_id>\d+)
author_novel_pattern: '(?<novel_url>https?://overlap\.example/book/\d+)'
sitename: Overlap
toc_url: https://overlap.example/book/\k<ncode>
"#;

    #[tokio::test]
    async fn series_and_novel_patterns_win_over_overlapping_author_patterns() {
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        let sites = definition(OVERLAP_SITE);
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);

        // series_url と author_url の両方に一致する URL はシリーズとして展開する。
        let series = "https://overlap.example/users/5/list";
        http.add_text(series, 200, r#"<a href="/book/9">x</a>"#);
        assert_eq!(
            expander.expand(series).await.unwrap(),
            Some(vec!["https://overlap.example/book/9".to_string()])
        );
        assert_eq!(http.request_count(), 1, "作者一覧は取得しない");

        // 作品 URL に一致する形は単体取得のまま (展開しない)。
        let novel = "https://overlap.example/users/5/book";
        assert_eq!(expander.expand(novel).await.unwrap(), None);
        assert_eq!(expander.expand("n1234ab").await.unwrap(), None);
        assert_eq!(expander.expand("https://unrelated.example/x").await.unwrap(), None);
        assert_eq!(http.request_count(), 1, "取得は増えない");
    }

    #[tokio::test]
    async fn author_expansion_keeps_options_and_deduplicates_works() {
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        let sites = definition(AUTHOR_SITE);
        let target = "https://author.example/users/7";
        http.add_text(
            target,
            200,
            r#"<a href="https://author.example/book/2">b</a>
               <a href="https://author.example/book/2">b</a>"#,
        );
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
        let plans = expander
            .expand_plan(&JobPlan {
                kind: JobKind::Download,
                target: JobTarget::Url(target.to_string()),
                options: vec!["--force".to_string()],
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            plans,
            vec![JobPlan {
                kind: JobKind::Download,
                target: JobTarget::Url("https://author.example/book/2".into()),
                options: vec!["--force".to_string()],
            }]
        );
        // ダウンロード以外の種別は対象をそのままにする。
        let update = JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Url(target.into()),
            options: vec![],
        };
        assert_eq!(expander.expand_plan(&update).await.unwrap(), None);
        assert_eq!(http.request_count(), 1);
    }

    #[tokio::test]
    async fn an_author_without_works_expands_to_no_plans() {
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        let sites = definition(AUTHOR_SITE);
        let target = "https://author.example/users/7";
        http.add_text(target, 200, "<p>no works</p>");
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
        assert_eq!(expander.expand(target).await.unwrap(), Some(Vec::new()));
        let plan = JobPlan {
            kind: JobKind::Download,
            target: JobTarget::Url(target.into()),
            options: vec![],
        };
        assert_eq!(expander.expand_plan(&plan).await.unwrap(), Some(Vec::new()));
    }

    const GATED_SITE: &str = r#"
name: Gated
domain: gated.example
top_url: https://gated.example
url: https://gated\.example/book/(?<ncode>\d+)
author_url: ^https?://gated\.example/user/(?<author_id>\d+)/?
author_api_url: https://gated.example/user/\k<author_id>/
author_novel_pattern: '(?<novel_url>https?://gated\.example/book/\d+)'
sitename: Gated
login_pattern: 'ログインが必要です'
toc_url: https://gated.example/book/\k<ncode>
"#;

    #[tokio::test]
    async fn an_author_login_wall_is_an_error_not_an_empty_expansion() {
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        let sites = definition(GATED_SITE);
        let target = "https://gated.example/user/5/";
        http.add_text(target, 200, "ログインが必要です");
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
        let error = expander.expand(target).await.unwrap_err();
        assert!(error.to_string().contains("ログイン"), "got {error}");
    }

    const NO_LISTING_SITE: &str = r#"
name: NoListing
domain: nolist.example
top_url: https://nolist.example
url: https://nolist\.example/book/(?<ncode>\d+)
author_url: ^https?://nolist\.example/user/(?<author_id>\d+)/?
sitename: NoListing
toc_url: https://nolist.example/book/\k<ncode>
"#;

    #[tokio::test]
    async fn an_author_definition_without_a_listing_is_an_error() {
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        let sites = definition(NO_LISTING_SITE);
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
        let error = expander
            .expand("https://nolist.example/user/5/")
            .await
            .unwrap_err();
        assert!(
            matches!(error, NarouError::SiteSetting(_)),
            "got {error:?}"
        );
        assert_eq!(http.request_count(), 0);
    }

    #[tokio::test]
    async fn the_bundled_narou_definition_expands_the_mypage_url() {
        let sites = SiteSetting::load_all().expect("site definitions");
        let http = MockHttpClient::new();
        let limiter = FakeRateLimiter::new();
        http.add_text(
            "https://api.syosetu.com/novelapi/api/?out=json&userid=2842627&lim=500",
            200,
            r#"[{"allcount":2},{"ncode":"N5181MT"},{"ncode":"N6275MR"}]"#,
        );
        let expander = DownloadTargetExpander::new(&http, &limiter, &sites);
        assert_eq!(
            expander
                .expand("https://mypage.syosetu.com/2842627/")
                .await
                .unwrap(),
            Some(vec![
                "https://ncode.syosetu.com/n5181mt/".to_string(),
                "https://ncode.syosetu.com/n6275mr/".to_string(),
            ])
        );
    }
}
