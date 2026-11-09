//! Bundled site definitions (Phase 8).
//!
//! `build.rs` embeds the checked-in `../webnovel/*.yaml` files as static
//! strings; this module parses them with the shared [`SiteSetting`] type and
//! compiles the shared regex / preprocess DSL at composition time. There is
//! no filesystem access at runtime.

use std::sync::Arc;
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
pub fn load_bundled_site_settings() -> Result<Arc<Vec<SiteSetting>>> {
    static SITE_SETTINGS: std::sync::LazyLock<
        std::result::Result<Arc<Vec<SiteSetting>>, String>,
    > = std::sync::LazyLock::new(|| {
        let contents: Vec<&str> =
            BUNDLED_SITE_YAML.iter().map(|(_, content)| *content).collect();
        SiteSetting::load_bundled(&contents).map(Arc::new).map_err(|error| error.to_string())
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

/// Snapshot user YAML for 30 seconds, not its potentially large compiled regex
/// state. The immutable deployment bundle remains compiled once per isolate;
/// user definitions compile into invocation-owned `Arc`s and die with it.
pub async fn load_site_settings(
    objects: &std::sync::Arc<dyn narou_rs::platform::ObjectStore>,
) -> Result<Arc<Vec<SiteSetting>>> {
    let definitions = if let Some(cached) = SITE_SETTINGS_CACHE.get("effective") {
        cached
    } else {
        let store = ObjectStoreSiteDefinitions::new(objects.clone());
        let definitions = Arc::new(if store.list().await?.is_empty() {
            Vec::new()
        } else {
            site_definitions(objects.clone()).effective_runtime().await?
        });
        SITE_SETTINGS_CACHE.put("effective", definitions.clone());
        definitions
    };
    if definitions.is_empty() {
        return load_bundled_site_settings();
    }
    let contents: Vec<&str> = definitions.iter().map(|(_, yaml)| yaml.as_str()).collect();
    SiteSetting::load_bundled(&contents).map(Arc::new)
}

/// 更新系ハンドラ向けの非キャッシュ版。キュー投入 / convert のターゲット
/// 解決とオプション計算は「いま保存されている定義」を見る必要があるので、
/// 30 秒 L1 に乗せず常に primary 側のストアを読み直す。
pub async fn load_site_settings_uncached(
    objects: &std::sync::Arc<dyn narou_rs::platform::ObjectStore>,
) -> Result<Arc<Vec<SiteSetting>>> {
    let store = ObjectStoreSiteDefinitions::new(objects.clone());
    if store.list().await?.is_empty() {
        return load_bundled_site_settings();
    }
    let effective = site_definitions(objects.clone()).effective_runtime().await?;
    let contents: Vec<&str> = effective.iter().map(|(_, yaml)| yaml.as_str()).collect();
    SiteSetting::load_bundled(&contents).map(Arc::new)
}

/// L1 キャッシュの寿命。書き込みは同じ isolate なら即時、他の isolate は
/// この時間で追従する (サイト定義は頻繁に変わらないので十分)。
const SITE_SETTINGS_TTL_MS: f64 = 30_000.0;

static SITE_SETTINGS_CACHE: std::sync::LazyLock<
    crate::isolate_cache::TtlMap<Arc<Vec<(String, String)>>>,
> = std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(
    SITE_SETTINGS_TTL_MS, 512 * 1024, |definitions| {
        definitions.capacity() * std::mem::size_of::<(String, String)>()
            + definitions.iter().map(|(name, yaml)| name.capacity() + yaml.capacity()).sum::<usize>()
    },
));

/// サイト定義を書き換えた直後に呼ぶ (同じ isolate のキャッシュを落とす)。
pub fn invalidate_site_settings() {
    SITE_SETTINGS_CACHE.invalidate("effective");
}
