//! Series-to-novel target expansion shared by CLI downloads and Worker planning.

use std::collections::HashSet;

use crate::application::{JobKind, JobPlan, JobTarget};
use crate::downloader::http_policy::{FetchPolicy, fetch_text};
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::util::pretreatment_source;
use crate::error::{NarouError, Result};
use crate::platform::{HttpClient, RateLimiter};

/// Borrows the same transport, rate limiter and effective YAML definitions as
/// the downloader, without constructing its storage or per-novel state.
pub struct DownloadTargetExpander<'a> {
    http: &'a dyn HttpClient,
    rate_limiter: &'a dyn RateLimiter,
    site_settings: &'a [SiteSetting],
}

impl<'a> DownloadTargetExpander<'a> {
    pub fn new(
        http: &'a dyn HttpClient,
        rate_limiter: &'a dyn RateLimiter,
        site_settings: &'a [SiteSetting],
    ) -> Self {
        Self { http, rate_limiter, site_settings }
    }

    pub fn recognizes(&self, target: &str) -> bool {
        self.site_settings.iter().any(|s| s.matches_series_url(target))
    }

    /// `None` leaves an ordinary URL, ID, ncode or title unchanged. A matching
    /// series must yield at least one novel; errors must not fall through to
    /// the single-novel downloader.
    pub async fn expand(&self, target: &str) -> Result<Option<Vec<String>>> {
        if !target.starts_with("http://") && !target.starts_with("https://") {
            return Ok(None);
        }
        let Some(setting) = self.site_settings.iter().find(|s| s.matches_series_url(target)) else {
            return Ok(None);
        };
        let pattern = setting.compile_series_item_pattern().ok_or_else(|| {
            NarouError::SiteSetting(format!("No series_item_url pattern defined: {}", setting.name))
        })?;
        let mut body = fetch_text(
            self.http,
            self.rate_limiter,
            target,
            &FetchPolicy::for_site(setting),
            Some(setting.encoding()),
        ).await?;
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
        Ok(Some(targets))
    }

    /// Borrowing leaves an ordinary plan untouched, without cloning its target
    /// or options. Expanded members each own the original download options.
    pub async fn expand_plan(&self, plan: &JobPlan) -> Result<Option<Vec<JobPlan>>> {
        let urls = match (&plan.kind, &plan.target) {
            (JobKind::Download, JobTarget::Url(url)) => self.expand(url).await?,
            _ => None,
        };
        Ok(urls.map(|urls| urls.into_iter().map(|url| JobPlan {
            kind: plan.kind,
            target: JobTarget::Url(url),
            options: plan.options.clone(),
        }).collect()))
    }
}