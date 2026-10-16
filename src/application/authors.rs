//! Persistent author watch targets shared by native and Workers.
use std::sync::Arc;

use crate::author::AuthorRecord;
use crate::downloader::site_setting::SiteSetting;
use crate::error::{NarouError, Result};
use crate::platform::{NovelRepository, PlatformFuture, PlatformService};
use super::download_targets::DownloadTargetExpander;
use super::target_lookup::first_matching_toc_url;

/// Mutations affect one URL atomically, never replace a stale whole list.
pub trait AuthorStore: PlatformService {
    /// Sorted by site then URL, matching the native inventory.
    fn load(&self) -> PlatformFuture<'_, Result<Vec<AuthorRecord>>>;
    fn add<'a>(&'a self, author: &'a AuthorRecord) -> PlatformFuture<'a, Result<bool>>;
    fn remove<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<bool>>;
}

pub struct AuthorService {
    store: Arc<dyn AuthorStore>,
}

impl AuthorService {
    pub fn new(store: Arc<dyn AuthorStore>) -> Self { Self { store } }

    pub async fn list(&self) -> Result<Vec<AuthorRecord>> {
        self.store.load().await
    }
    pub async fn remove_url(&self, url: &str) -> Result<bool> { self.store.remove(url).await }

    pub async fn add(&self, url: &str, sites: &[SiteSetting]) -> Result<bool> {
        let author = author_for_url(url, sites)?;
        self.store.add(&author).await
    }

}

pub fn author_for_url(url: &str, sites: &[SiteSetting]) -> Result<AuthorRecord> {
    let valid_url = url::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
    let site = sites.iter().find(|site| valid_url && site.matches_author_url(url))
        .ok_or_else(|| NarouError::SiteSetting(format!("{url} を作者ページとして扱うサイト定義がありません (author_url 未定義)")))?;
    Ok(AuthorRecord::new(&site.domain, url))
}

/// Author discovery uses the same site rules, credentials and URL identity as
/// an ordinary download. A lookup failure must not mean "new work".
pub async fn new_work_urls(
    author_url: &str,
    expander: &DownloadTargetExpander<'_>,
    sites: &[SiteSetting],
    novels: &dyn NovelRepository,
) -> Result<Vec<String>> {
    let urls = expander.author_works(author_url).await?;
    let mut toc_urls = Vec::new();
    let mut positions = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        if let Some(toc) = first_matching_toc_url(sites, url) {
            positions.push(index);
            toc_urls.push(toc);
        }
    }
    if toc_urls.is_empty() { return Ok(urls); }
    let known = novels.contains_toc_urls(&toc_urls).await?;
    if known.len() != positions.len() { return Err(NarouError::Platform("invalid URL membership result".into())); }
    let mut registered = vec![false; urls.len()];
    for (index, known) in positions.into_iter().zip(known) { registered[index] = known; }
    Ok(urls.into_iter().zip(registered).filter_map(|(url, known)| (!known).then_some(url)).collect())
}
