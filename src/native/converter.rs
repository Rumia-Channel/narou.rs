//! Native compatibility constructors for the converter.
//!
//! The converter pipeline itself remains platform-neutral. These constructors
//! preserve the historical zero-argument capability wiring for the native CLI.

use std::path::Path;
use std::sync::Arc;

use crate::converter::{
    ConverterCapabilities, NovelConverter, settings::NovelSettings, user_converter::UserConverter,
};
use crate::downloader::rate_limit::RateLimiter;
use crate::illustration_store::IllustrationIndex;
use crate::native::http::NativeHttpClient;
use crate::platform::ObjectStore;

fn native_http_and_rate_limiter() -> Option<(
    Arc<dyn crate::platform::HttpClient>,
    Arc<dyn crate::platform::RateLimiter>,
)> {
    let user_agent = ua_generator::ua::spoof_firefox_ua().to_string();
    let http: Arc<dyn crate::platform::HttpClient> =
        Arc::new(NativeHttpClient::new(&user_agent).ok()?);
    let rate_limiter: Arc<dyn crate::platform::RateLimiter> = Arc::new(RateLimiter::new());
    Some((http, rate_limiter))
}

fn native_capabilities() -> Option<ConverterCapabilities> {
    let (http, rate_limiter) = native_http_and_rate_limiter()?;
    Some(ConverterCapabilities {
        http,
        rate_limiter,
        fetch_policy: crate::downloader::http_policy::FetchPolicy::default(),
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
        fetch_policy_resolver: Some(Arc::new(|record| {
            let settings = crate::downloader::site_setting::effective_site_settings();
            crate::downloader::http_policy::FetchPolicy::for_record(&settings, record)
        })),
    })
}

/// Capabilities that persist the illustration index and newly fetched images
/// through the active [`ObjectStore`]/[`AssetStore`](crate::platform::AssetStore) boundary.
///
/// Returns `None` when the plain filesystem layout is the only storage (YAML
/// mode without S3), where the legacy `挿絵/` directory scan and
/// `.illustration_cache.yaml` handling stays authoritative. SQLite mode (even
/// with the mirror disabled) and `s3.asset-backend=s3` store illustrations
/// outside the novel folder, so a conversion that only wrote filesystem files
/// would clobber the stored index and make the web readers see stale data.
pub fn store_capabilities_for_novel(novel_dir: &Path) -> Option<ConverterCapabilities> {
    let inventory = crate::db::inventory::Inventory::with_default_root().ok()?;
    let narou_dir = inventory.root_dir().join(".narou");
    let sqlite_active = crate::native::sqlite::state::active_for(&narou_dir).is_some();
    let s3_active = crate::native::s3::illustration_store().ok().flatten().is_some();
    if !sqlite_active && !s3_active {
        return None;
    }

    let archive_root = inventory
        .root_dir()
        .join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let prefix = crate::native::object_store::logical_key_for_native_path(&archive_root, novel_dir)?;
    let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir()).ok()?;
    let index = load_stored_illustration_index(stores.objects.as_ref(), &prefix);
    let mut capabilities = native_capabilities()?;
    capabilities.assets = Some(stores.assets);
    capabilities.objects = Some(stores.objects);
    capabilities.illustration_index = Some(index);
    capabilities.illustration_prefix = Some(prefix);
    Some(capabilities)
}

/// Read `<prefix>/.illustration_cache.yaml` from the object store.
///
/// Missing or unparsable entries yield an empty index (same contract as the
/// downloader's `PersistenceService::load_cache`): the conversion re-fetches
/// what it cannot map instead of failing.
fn load_stored_illustration_index(objects: &dyn ObjectStore, prefix: &crate::platform::ObjectKey) -> IllustrationIndex {
    let Ok(key) = prefix.join(".illustration_cache.yaml") else {
        return IllustrationIndex::default();
    };
    let Some(bytes) = run_store_io(objects.read_small(&key)).ok().flatten() else {
        return IllustrationIndex::default();
    };
    match serde_yaml::from_slice::<crate::illustration_store::IllustrationStore>(&bytes) {
        Ok(store) => IllustrationIndex::from_store(store),
        Err(_) => IllustrationIndex::default(),
    }
}

/// Read the canonical per-novel INI, not a stale pre-migration mirror.
pub(crate) fn load_novel_ini(novel_dir: &Path) -> crate::error::Result<crate::converter::ini::IniData> {
    use crate::converter::ini::IniData;
    use crate::native::object_store::{NativeStore, logical_key_for_native_path};

    let path = novel_dir.join("setting.ini");
    let Some(root) = novel_dir.ancestors().find(|root| root.join(".narou").is_dir()) else {
        return IniData::load_file(&path);
    };
    let archive_root = root.join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let Some(key) = logical_key_for_native_path(&archive_root, &path) else {
        return IniData::load_file(&path);
    };
    let store = NativeStore::for_narou_root(root)?;
    if matches!(&store, NativeStore::Fs(_)) {
        return IniData::load_file(&path);
    }
    let Some(bytes) = futures::executor::block_on(store.read_small(&key))? else {
        return Ok(IniData::new());
    };
    let text = String::from_utf8(bytes).map_err(|error| {
        crate::error::NarouError::Platform(format!("setting.ini is not UTF-8: {error}"))
    })?;
    Ok(IniData::load(&text))
}

/// Drive store I/O from a synchronous converter, including callers already on Tokio.
/// Borrowed futures move to a scoped thread before entering a separate runtime.
pub(crate) fn run_store_io<F, T>(future: F) -> crate::error::Result<T>
where
    F: Future<Output = crate::error::Result<T>> + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            std::thread::Builder::new()
                .spawn_scoped(scope, move || run_store_io(future))?
                .join()
                .map_err(|_| crate::error::NarouError::Platform("converter store I/O thread panicked".to_string()))?
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(future)
}

impl NovelConverter {
    pub fn new(settings: NovelSettings) -> Self {
        let capabilities = store_capabilities_for_novel(&settings.archive_path).or_else(native_capabilities);
        Self::build(settings, None, capabilities)
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        let capabilities = store_capabilities_for_novel(&settings.archive_path).or_else(native_capabilities);
        Self::build(settings, Some(user_converter), capabilities)
    }
}
