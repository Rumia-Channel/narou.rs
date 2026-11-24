//! Explicit native file capabilities and operation-owned converter workspaces.
//! Pure converter constructors perform no filesystem or HTTP initialization;
//! native file entrypoints use the fallible `try_*` constructors below.

use std::path::Path;
use std::sync::Arc;

use crate::converter::{
    ConverterCapabilities, NovelConverter, settings::NovelSettings, user_converter::UserConverter,
};
use crate::downloader::rate_limit::RateLimiter;
use crate::illustration_store::IllustrationIndex;
use crate::native::http::NativeHttpClient;

fn native_http_and_rate_limiter() -> crate::error::Result<(
    Arc<dyn crate::platform::HttpClient>,
    Arc<dyn crate::platform::RateLimiter>,
)> {
    let user_agent = ua_generator::ua::spoof_firefox_ua().to_string();
    let http: Arc<dyn crate::platform::HttpClient> = Arc::new(NativeHttpClient::new(&user_agent)?);
    let rate_limiter: Arc<dyn crate::platform::RateLimiter> = Arc::new(RateLimiter::new());
    Ok((http, rate_limiter))
}

fn native_capabilities() -> crate::error::Result<ConverterCapabilities> {
    let (http, rate_limiter) = native_http_and_rate_limiter()?;
    Ok(ConverterCapabilities {
        http,
        rate_limiter,
        fetch_policy: crate::downloader::http_policy::FetchPolicy::default(),
        assets: None,
        objects: None,
        illustration_index: None,
        illustration_prefix: None,
        write_guard: None,
        novel_record: None,
        fetch_policy_resolver: Some(Arc::new(|record| {
            let settings = crate::downloader::site_setting::effective_site_settings();
            crate::downloader::http_policy::FetchPolicy::for_record(&settings, record)
        })),
    })
}

/// Read only the settings metadata needed by a native constructor. Canonical
/// inputs retain their original epoch; private inputs never touch a library.
pub(crate) fn with_novel_settings_input<T: Send>(
    id: i64,
    directory: &Path,
    configure: impl FnOnce(&Path) -> crate::error::Result<T> + Send,
) -> crate::error::Result<(T, Option<crate::application::novel_access::AccessOrigin>)> {
    use crate::application::novel_access::{AccessOrigin, NovelReadLease};
    use crate::error::NarouError;
    if crate::application::novel_access::is_private_path(directory)? {
        return configure(directory).map(|value| (value, None));
    }
    run_store_io(async {
        let inventory = crate::db::inventory::Inventory::with_default_root()?;
        let archive = inventory.root_dir().join(crate::downloader::ARCHIVE_ROOT_DIR);
        let repository = crate::native::novel_repository::NativeNovelRepository::new();
        let discovered = repository.get_current_sync(id.into())?.ok_or_else(|| NarouError::NotFound(format!("ID: {id}")))?;
        let keys = crate::platform::NovelObjectKeys::new(&discovered.sitename, &discovered.file_title, discovered.use_subdirectory)?;
        let prefix = crate::platform::ObjectPrefix::from(keys.prefix().clone());
        let primary = crate::native::object_store::NativeStore::for_narou_root(inventory.root_dir())?;
        let expected = crate::db::paths::ensure_within_archive_root(&primary.novel_directory(&prefix)?, &archive)?;
        if crate::db::paths::ensure_within_archive_root(directory, &archive)? != expected {
            return Err(NarouError::Platform("plan_stale: settings directory changed its canonical novel identity".into()));
        }
        let objects = crate::native::materialization::read_store(inventory.root_dir())?;
        let mut lease = NovelReadLease::admit(&objects, &prefix, "native-settings-input").await?;
        let result = async {
            let current = repository.get_current_sync(id.into())?.ok_or_else(|| NarouError::NotFound(format!("ID: {id}")))?;
            if current != discovered { return Err(NarouError::Platform("plan_stale: settings metadata changed before admission".into())); }
            crate::native::transfer::assert_source_namespace(&current)?;
            drop(current);
            drop(discovered);
            let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("settings reader fence unavailable".into()))?;
            let before = fence.gate_info(&prefix).await?;
            let temporary = tempfile::Builder::new().prefix(".narou-snapshot-").tempdir_in(inventory.root_dir())?;
            for name in ["setting.ini", "replace.txt"] {
                crate::native::materialization::copy_asset(&objects, &mut lease, &keys.prefix().join(name)?, &temporary.path().join(name), None).await?;
            }
            let configured = configure(temporary.path())?;
            lease.validate().await?;
            if fence.gate_info(&prefix).await?.content_revision != before.content_revision {
                return Err(NarouError::Platform("novel_busy: settings changed during private capture".into()));
            }
            Ok((configured, Some(AccessOrigin { prefix: prefix.as_ref().to_owned(), epoch: lease.epoch() })))
        }.await;
        let released = lease.release().await;
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    })
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

pub(crate) fn private_capabilities(path: &Path) -> crate::error::Result<ConverterCapabilities> {
    crate::application::novel_access::require_private_path(path)?;
    let store = Arc::new(crate::native::object_store::NativeObjectStore::from_private_root(path.to_path_buf())?);
    let objects: Arc<dyn crate::platform::ObjectStore> = store.clone();
    let assets: Arc<dyn crate::platform::AssetStore> = store;
    let write = run_store_io(crate::application::novel_access::NovelWriteGuard::capture(
        &objects, Some(&assets), &crate::platform::ObjectPrefix::new("novels")?,
    ))?;
    let mut capabilities = native_capabilities()?;
    capabilities.objects = Some(objects);
    capabilities.assets = Some(assets);
    capabilities.illustration_prefix = Some(crate::platform::ObjectKey::try_new("novels")?);
    capabilities.write_guard = Some(write);
    capabilities.illustration_index = Some(IllustrationIndex::from_store(crate::illustration_store::IllustrationStore::load(path)?));
    Ok(capabilities)
}

/// Own all native conversion inputs and outputs privately until the original
/// operation token authorizes each canonical install. No canonical helper files
/// are created and subsequently deleted.
pub(crate) struct ConversionWorkspace {
    _temporary: Option<tempfile::TempDir>,
    _original_lock: Option<crate::compat::NovelLockGuard>,
    input: std::path::PathBuf,
    directory: std::path::PathBuf,
    origin: Option<crate::application::novel_access::NovelWriteGuard>,
    primary: Option<Arc<crate::native::object_store::NativeStore>>,
    pub(crate) private_write: crate::application::novel_access::NovelWriteGuard,
    pub(crate) objects: Arc<dyn crate::platform::ObjectStore>,
    pub(crate) assets: Arc<dyn crate::platform::AssetStore>,
}

impl ConversionWorkspace {
    pub(crate) fn begin(
        novel_dir: &Path,
        id: i64,
        supplied: Option<&crate::application::novel_access::NovelWriteGuard>,
        selection: crate::native::materialization::ConversionInputSelection,
    ) -> crate::error::Result<Self> {
        use crate::application::novel_access::NovelWriteGuard;
        use crate::platform::ObjectPrefix;
        let private_input = crate::application::novel_access::is_private_path(novel_dir)?;
        let mut directory = novel_dir.to_path_buf();
        let mut original_lock = None;
        let (origin, primary, temporary) = if !private_input {
            let stores = crate::native::object_store::NativeStores::for_current_root()?;
            let guard = match supplied {
                Some(guard) => guard.clone(),
                None => crate::native::materialization::capture_write_for_id(id)?,
            };
            directory = crate::native::materialization::directory_for_origin(&guard)?;
            original_lock = Some(crate::compat::NovelLockGuard::acquire(Some(id))?);
            let (snapshot, ()) = crate::native::materialization::conversion_snapshot_with_origin(novel_dir, &guard, None, |_| Ok(((), selection)))?;
            (Some(guard), Some(stores.primary), Some(snapshot))
        } else if let Some(guard) = supplied {
            crate::application::novel_access::require_private_path(novel_dir)?;
            let inventory = crate::db::inventory::Inventory::with_default_root()?;
            let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?;
            directory = stores.primary.with_open_write(guard.fence(), || {
                let record = crate::native::novel_repository::NativeNovelRepository::new().get_current_sync(id.into())?
                    .ok_or_else(|| crate::error::NarouError::NotFound(format!("ID: {id}")))?;
                let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
                if keys.prefix().as_ref() != guard.prefix().as_ref() { return Err(crate::error::NarouError::Platform("private conversion changed its operation origin".into())); }
                crate::native::transfer::assert_source_namespace(&record)?;
                stores.primary.novel_directory(guard.prefix())
            })?;
            (Some(guard.clone()), Some(stores.primary), None)
        } else {
            crate::application::novel_access::require_private_path(novel_dir)?;
            (None, None, None)
        };
        let input = temporary.as_ref().map(|temp| temp.path()).unwrap_or(novel_dir).to_path_buf();
        let store = Arc::new(crate::native::object_store::NativeObjectStore::from_private_root(input.clone())?);
        let objects: Arc<dyn crate::platform::ObjectStore> = store.clone();
        let assets: Arc<dyn crate::platform::AssetStore> = store;
        let private_write = run_store_io(NovelWriteGuard::capture(
            &objects, Some(&assets), &ObjectPrefix::new("novels")?,
        ))?;
        Ok(Self { _temporary: temporary, _original_lock: original_lock, input, directory, origin, primary, private_write, objects, assets })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.input
    }

    pub(crate) fn origin(&self) -> Option<&crate::application::novel_access::NovelWriteGuard> {
        self.origin.as_ref()
    }

    pub(crate) fn output_path(&self, path: &Path) -> crate::error::Result<std::path::PathBuf> {
        if self.origin.is_none() { return Ok(path.to_path_buf()); }
        let relative = match path.strip_prefix(self.path()) {
            Ok(relative) => relative,
            Err(_) => { crate::application::novel_access::require_private_path(path)?; return Ok(path.to_path_buf()); }
        };
        Ok(self.directory.join(relative))
    }

    pub(crate) fn publish(&self, path: &Path) -> crate::error::Result<std::path::PathBuf> {
        let Some(origin) = &self.origin else { return Ok(path.to_path_buf()); };
        let relative = match path.strip_prefix(self.path()) {
            Ok(relative) => relative,
            Err(_) => { crate::application::novel_access::require_private_path(path)?; return Ok(path.to_path_buf()); }
        };
        let destination = self.directory.join(relative);
        crate::native::materialization::publish_file(origin, path, &destination)?;
        Ok(destination)
    }

    pub(crate) fn discard_intermediate(&self, _id: i64, text: Option<&Path>, output: Option<&Path>) -> crate::error::Result<()> {
        if crate::converter::keep_converted_text_file() { return Ok(()); }
        // These bytes were rendered privately by this invocation. Never turn
        // private cleanup into deletion of a pre-existing canonical user file
        // or a previous generated output that this operation did not install.
        if let Some(text) = text.filter(|text| Some(*text) != output) {
            crate::application::novel_access::require_private_path(text)?;
            match std::fs::remove_file(text) {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn finish(
        &self, id: i64, output: &Path, text: Option<&Path>, illustrations: &std::collections::HashSet<String>,
    ) -> crate::error::Result<std::path::PathBuf> {
        let Some(origin) = &self.origin else { return Ok(output.to_path_buf()); };
        // Settings use this same SQLite connection; resolve them before the
        // fenced transaction instead of recursively locking it from inside.
        let keep_text = crate::converter::keep_converted_text_file();
        for name in illustrations {
            let path = self.path().join("挿絵").join(name);
            if path.is_file() { self.publish(&path)?; }
        }
        for name in [".illustration_cache.yaml", crate::converter::inspector::INSPECT_LOG_NAME] {
            let path = self.path().join(name);
            if path.is_file() { self.publish(&path)?; }
        }
        if let Some(text) = text {
            if output != text && keep_text { self.publish(text)?; }
            if let Some(primary) = &self.primary
                && matches!(primary.as_ref(), crate::native::object_store::NativeStore::Sqlite(_))
            {
                let toc: crate::downloader::TocFile = serde_yaml::from_slice(&std::fs::read(self.path().join("toc.yaml"))?)?;
                let sections = crate::converter::load_sections_from_dir(self.path(), &toc.subtitles)?;
                let mut map = std::collections::BTreeMap::new();
                for section in sections {
                    map.insert(section.index.clone(), (Some(section.subtitle.clone()), serde_yaml::to_string(&section)?));
                }
                let bytes = std::fs::read(text)?;
                primary.with_open_fence_transaction(origin.fence(), |connection| {
                    crate::native::sqlite::content::store_sections_in_tx(connection, id, &map)?;
                    if keep_text {
                        crate::native::sqlite::content::store_output(connection, id, "converted_text", &bytes)?;
                    } else {
                        crate::native::sqlite::content::delete_output(connection, id, "converted_text")?;
                    }
                    crate::native::sqlite::versions::snapshot_working_set_in_tx(connection, id, "update", Some("convert"))?;
                    Ok(())
                })?;
            }
        }
        let published = self.publish(output)?;
        if let Some(text) = text
            && !text.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case("novel.txt"))
        {
            // Retire the old canonical mirror only after publishing, under the original origin.
            crate::native::materialization::delete_file(origin, &self.directory.join("novel.txt"))?;
        }
        self.discard_intermediate(id, text, Some(output))?;
        Ok(published)
    }
}

impl NovelConverter {
    /// Pure rendering constructor: no filesystem or native transport setup.
    pub fn new(settings: NovelSettings) -> Self {
        Self::build(settings, None, None)
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        Self::build(settings, Some(user_converter), None)
    }

    /// Native file conversion admits only explicit, fallible capabilities.
    pub fn try_new(settings: NovelSettings) -> crate::error::Result<Self> {
        let capabilities = if crate::application::novel_access::is_private_path(&settings.archive_path)? {
            private_capabilities(&settings.archive_path)?
        } else {
            native_capabilities()?
        };
        Ok(Self::build(settings, None, Some(capabilities)))
    }

    pub fn try_with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> crate::error::Result<Self> {
        let capabilities = if crate::application::novel_access::is_private_path(&settings.archive_path)? {
            private_capabilities(&settings.archive_path)?
        } else {
            native_capabilities()?
        };
        Ok(Self::build(settings, Some(user_converter), Some(capabilities)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_constructor_propagates_private_index_io_errors() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".illustration_cache.yaml")).unwrap();
        let settings = NovelSettings { archive_path: directory.path().to_path_buf(), ..NovelSettings::default() };
        assert!(NovelConverter::try_new(settings.clone()).is_err());
        assert!(NovelConverter::try_with_user_converter(settings, UserConverter::default()).is_err());
    }
}
