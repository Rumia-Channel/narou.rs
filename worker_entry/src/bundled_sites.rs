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
/// A bundled YAML that fails to parse is a deployment error and fails
/// composition loudly (readiness 503) rather than silently shrinking the
/// site set. `load_bundled` returns `NarouError::Yaml` (classified
/// `Blocked`) for the first malformed definition.
pub fn load_bundled_site_settings() -> Result<Vec<SiteSetting>> {
    let contents: Vec<&str> = BUNDLED_SITE_YAML.iter().map(|(_, content)| *content).collect();
    SiteSetting::load_bundled(&contents)
}
