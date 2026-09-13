//! Native compatibility constructors for the converter.
//!
//! The converter pipeline itself remains platform-neutral. These constructors
//! preserve the historical zero-argument capability wiring for the native CLI.

use std::sync::Arc;

use crate::converter::{
    ConverterCapabilities, NovelConverter, settings::NovelSettings, user_converter::UserConverter,
};
use crate::downloader::rate_limit::RateLimiter;
use crate::native::http::NativeHttpClient;

fn native_capabilities() -> Option<ConverterCapabilities> {
    let user_agent = ua_generator::ua::spoof_firefox_ua().to_string();
    let http = Arc::new(NativeHttpClient::new(&user_agent).ok()?);
    let rate_limiter = Arc::new(RateLimiter::new(false));
    Some(ConverterCapabilities {
        http,
        rate_limiter,
        assets: None,
        objects: None,
        illustration_index: None,
        illustration_prefix: None,
        novel_record_resolver: Some(Arc::new(|id| {
            crate::native::novel_repository::NativeNovelRepository::new()
                .get_sync(id.into())
                .ok()
                .flatten()
        })),
    })
}

impl NovelConverter {
    pub fn new(settings: NovelSettings) -> Self {
        Self::build(settings, None, native_capabilities())
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        Self::build(settings, Some(user_converter), native_capabilities())
    }
}
