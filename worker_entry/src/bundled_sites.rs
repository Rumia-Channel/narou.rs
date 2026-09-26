//! Bundled site definitions (Phase 8).
//!
//! `build.rs` embeds the checked-in `../webnovel/*.yaml` files as static
//! strings; this module parses them with the shared [`SiteSetting`] type and
//! compiles the shared regex / preprocess DSL at composition time. There is
//! no filesystem access at runtime.

use narou_rs::application::site_definitions::{
    ObjectStoreSiteDefinitions, SiteDefinitionStore, SiteDefinitions,
};
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

/// bundle を `(ファイル名, YAML)` の並びで返す（core の `SiteDefinitions` に渡す形）。
pub fn bundled_definitions() -> Vec<(String, String)> {
    BUNDLED_SITE_YAML
        .iter()
        .map(|(name, yaml)| ((*name).to_string(), (*yaml).to_string()))
        .collect()
}

/// サイト定義の管理サービス（bundle + オブジェクトストア）。
///
/// 保存も差し替えも core の [`SiteDefinitions`] 越しに行うので、native の
/// `/api/sites*` と同じ規則・同じ応答形になる。
pub fn site_definitions(
    objects: std::sync::Arc<dyn narou_rs::platform::ObjectStore>,
) -> SiteDefinitions {
    SiteDefinitions::new(
        bundled_definitions(),
        std::sync::Arc::new(ObjectStoreSiteDefinitions::new(objects)),
    )
}

/// Downloader に渡す実効定義を組み立てる。
///
/// ユーザー定義が無ければ bundle のキャッシュ済み結果をそのまま返す
/// （isolate ごとに 1 回の parse + compile で済む）。1 件でもあればマージして
/// 読み直す。
pub async fn load_site_settings(
    objects: &std::sync::Arc<dyn narou_rs::platform::ObjectStore>,
) -> Result<Vec<SiteSetting>> {
    let store = ObjectStoreSiteDefinitions::new(objects.clone());
    if store.list().await?.is_empty() {
        return load_bundled_site_settings();
    }
    let effective = site_definitions(objects.clone()).effective().await?;
    let contents: Vec<&str> = effective.iter().map(|(_, yaml)| yaml.as_str()).collect();
    SiteSetting::load_bundled(&contents)
}
