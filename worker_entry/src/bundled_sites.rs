//! Bundled site definitions (Phase 8).
//!
//! `build.rs` embeds the checked-in `../webnovel/*.yaml` files as static
//! strings; this module parses them with the shared [`SiteSetting`] type and
//! compiles the shared regex / preprocess DSL at composition time. There is
//! no filesystem access at runtime.

use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::Result;

include!(concat!(env!("OUT_DIR"), "/bundled_sites.rs"));

/// Parse and compile the bundled site definitions.
///
/// The YAML payloads are embedded at build time and immutable for the life
/// of the isolate, so the parse + regex/preprocess compile runs once per
/// isolate instead of once per queue message / cron tick / API request.
/// A bundled YAML that fails to parse is a deployment error: the failure is
/// cached too, so every caller still sees composition fail (readiness 503)
/// rather than silently shrinking the site set.
pub fn load_bundled_site_settings() -> Result<Vec<SiteSetting>> {
    static SITE_SETTINGS: std::sync::LazyLock<
        std::result::Result<Vec<SiteSetting>, String>,
    > = std::sync::LazyLock::new(|| {
        let contents: Vec<&str> =
            BUNDLED_SITE_YAML.iter().map(|(_, content)| *content).collect();
        SiteSetting::load_bundled(&contents).map_err(|error| error.to_string())
    });
    match &*SITE_SETTINGS {
        Ok(settings) => Ok(settings.clone()),
        Err(message) => Err(narou_rs::error::NarouError::SiteSetting(message.clone())),
    }
}
