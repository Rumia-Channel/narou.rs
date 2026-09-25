//! Native compatibility constructors for the downloader.
//!
//! The downloader core accepts explicit platform capabilities. These wrappers
//! retain the historical CLI constructors while keeping native adapters out of
//! the core implementation.

use std::path::PathBuf;
use std::sync::Arc;

use crate::downloader::Downloader;
use crate::platform::{
    AssetStore, CookieStore, HttpClient, NovelRepository, ObjectStore, RateLimiter, SystemClock,
};

/// Login cookies for the library the current directory belongs to.
///
/// `None` outside a library: the downloader then stays anonymous, exactly like
/// the Worker and the in-memory tests.
fn cookie_store() -> Option<Arc<dyn CookieStore>> {
    crate::native::cookie_store::InventoryCookieStore::for_current_root()
        .ok()
        .map(|store| Arc::new(store) as Arc<dyn CookieStore>)
}

impl Downloader {
    pub fn new() -> crate::error::Result<Self> {
        Self::with_user_agent(None)
    }

    pub fn with_platform(
        http: Arc<dyn HttpClient>,
        rate_limiter: Arc<dyn RateLimiter>,
        novels: Arc<dyn NovelRepository>,
    ) -> crate::error::Result<Self> {
        let store = crate::native::object_store::NativeStore::for_current_root().or_else(|_| {
            crate::native::object_store::NativeStore::for_narou_root(&PathBuf::from("."))
        })?;
        let store = Arc::new(store);
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let cookies = cookie_store();
        let downloader = Self::with_platform_and_storage(
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            Arc::new(SystemClock),
        )?;
        Ok(match cookies {
            Some(cookies) => downloader.with_cookie_store(cookies),
            None => downloader,
        })
    }

    pub fn with_user_agent(user_agent: Option<&str>) -> crate::error::Result<Self> {
        let ua = crate::downloader::resolve_user_agent(
            user_agent,
            crate::compat::load_local_setting_string("user-agent"),
        );
        let cookies = cookie_store();
        let client = crate::native::http::NativeHttpClient::new(&ua)?;
        let http: Arc<dyn HttpClient> = match cookies.clone() {
            Some(cookies) => Arc::new(client.with_cookie_store(cookies)),
            None => Arc::new(client),
        };
        let rate_limiter = Arc::new(crate::downloader::rate_limit::RateLimiter::new(false));
        let novels = Arc::new(crate::native::novel_repository::NativeNovelRepository::new());
        let store = Arc::new(crate::native::object_store::NativeStore::for_current_root()?);
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let downloader = Self::with_platform_and_storage(
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            Arc::new(SystemClock),
        )?;
        Ok(match cookies {
            Some(cookies) => downloader.with_cookie_store(cookies),
            None => downloader,
        })
    }
}
