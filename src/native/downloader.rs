//! Native compatibility constructors for the downloader.
//!
//! The downloader core accepts explicit platform capabilities. These wrappers
//! retain the historical CLI constructors while keeping native adapters out of
//! the core implementation.

use std::path::PathBuf;
use std::sync::Arc;

use crate::downloader::{Downloader, types};
use crate::platform::{
    AssetStore, HttpClient, NovelRepository, ObjectStore, RateLimiter, SystemClock,
};

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
        Self::with_platform_and_storage(
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            Arc::new(SystemClock),
        )
    }

    pub fn with_user_agent(user_agent: Option<&str>) -> crate::error::Result<Self> {
        let ua = crate::downloader::resolve_user_agent(
            user_agent,
            crate::compat::load_local_setting_string("user-agent"),
        );
        let http = Arc::new(crate::native::http::NativeHttpClient::new(&ua)?);
        let rate_limiter = Arc::new(crate::downloader::rate_limit::RateLimiter::new(false));
        let novels = Arc::new(crate::native::novel_repository::NativeNovelRepository::new());
        let store = Arc::new(crate::native::object_store::NativeStore::for_current_root()?);
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        Self::with_platform_and_storage(
            http,
            rate_limiter,
            novels,
            objects,
            assets,
            Arc::new(SystemClock),
        )
    }
}
