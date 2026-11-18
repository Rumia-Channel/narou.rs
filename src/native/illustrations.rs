//! Native illustration migration uses the ordinary reader/writer fence. Legacy
//! filesystem maintenance uses `NativeMutationSnapshot`; canonical helper files
//! are never installed just to be deleted after conversion.

use std::sync::Arc;


use crate::application::novel_access::{NovelMigrationOrigins, NovelMigrationStore, NovelPrefixResolver, NovelReadScope};
use crate::error::{NarouError, Result};
use crate::native::object_store::{NativeStore, NativeStores};
use crate::platform::{
    AssetStore, AssetStream, GateLease, NovelAccessFence, NovelFilter, NovelQuery, NovelRepository,
    NovelSort, NovelSortKey, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectPrefix, ObjectStore, ObjectUpdateResult, PlatformFuture, S3Store, SmallObjectMutator,
    WriteFence,
};
use futures::StreamExt;

struct NativePrefixes(Vec<ObjectPrefix>);

impl NativePrefixes {
    async fn load() -> Result<Arc<Self>> {
        let records = crate::native::novel_repository::NativeNovelRepository::new().query_current(
            &NovelQuery::page(NovelFilter::all(), NovelSort::by(NovelSortKey::Id), 0, usize::MAX),
        ).await?;
        let mut prefixes = records.into_iter().map(|record| {
            crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
                .map(|keys| ObjectPrefix::from(keys.prefix().clone()))
        }).collect::<Result<Vec<_>>>()?;
        prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.as_ref().len()));
        Ok(Arc::new(Self(prefixes)))
    }
}

impl NovelPrefixResolver for NativePrefixes {
    fn novel_prefix<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectPrefix>>> {
        Box::pin(async move { Ok(self.0.iter().find(|prefix| prefix.matches(key)).cloned()) })
    }
}

/// Canonical-local-only migration source. `migrate_illustrations` must never
/// serve or overwrite a key backed by an immutable remote reference: the
/// reference check and the byte open share the backend's own lock boundary via
/// `NativeStore::open_local_leased_asset` / `local_asset_stream`, so stale
/// mirror bytes cannot leak into a newer canonical reference. All mutation
/// entry points fail closed: this view is a source, never a writer.
struct LocalMigrationSource {
    local: Arc<NativeStore>,
    prefixes: Arc<NativePrefixes>,
}

const LOCAL_READ_CAP: u64 = 64 * 1024;

impl LocalMigrationSource {
    async fn prefix(&self, key: &ObjectKey) -> Result<Option<ObjectPrefix>> {
        self.prefixes.novel_prefix(key).await
    }
    async fn local_collect(&self, key: &ObjectKey, range: std::ops::Range<u64>, exact: bool) -> Result<Option<Vec<u8>>> {
        let length = range.end.checked_sub(range.start)
            .filter(|length| *length <= LOCAL_READ_CAP)
            .ok_or_else(|| NarouError::Platform("local migration read exceeds 64 KiB".into()))?;
        let Some(prefix) = self.prefix(key).await? else {
            return ObjectStore::read_small(self.local.as_ref(), key).await;
        };
        let open = if exact { range.start..range.end } else { 0..u64::MAX };
        let Some(mut stream) = self.local.local_asset_stream(&prefix, key, open).await? else { return Ok(None); };
        let mut bytes = Vec::with_capacity(length.min(LOCAL_READ_CAP) as usize);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > LOCAL_READ_CAP as usize {
                return Err(NarouError::Platform("local migration object exceeds read cap".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        if exact && bytes.len() as u64 != length {
            return Err(NarouError::Platform("local migration range truncated".into()));
        }
        Ok(Some(bytes))
    }
    fn local_stream<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move {
            match self.prefix(key).await? {
                Some(prefix) => self.local.local_asset_stream(&prefix, key, range).await,
                None => AssetStore::read_stream_range(self.local.as_ref(), key, range).await,
            }
        })
    }
}

impl ObjectStore for LocalMigrationSource {
    fn access_fence(&self) -> Option<&dyn NovelAccessFence> { self.local.access_fence() }
    fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move { Ok(self.local.open_local_leased_asset(prefix, lease, key, range).await?.map(|(_, stream)| stream)) })
    }
    fn is_remote_reference<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        ObjectStore::is_remote_reference(self.local.as_ref(), key)
    }
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(async move {
            if ObjectStore::is_remote_reference(self.local.as_ref(), key).await? { return Ok(None); }
            ObjectStore::stat(self.local.as_ref(), key).await
        })
    }
    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(self.local_collect(key, 0..LOCAL_READ_CAP, false))
    }
    fn write_small<'a>(&'a self, key: &'a ObjectKey, bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        let _ = (key, bytes);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn update_small<'a, 'data: 'a>(&'a self, key: &'a ObjectKey, mutate: &'a mut SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        let _ = (key, mutate);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let _ = key;
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn write_small_guarded<'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey, bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        let _ = (fence, key, bytes);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn update_small_guarded<'a, 'data: 'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey, mutate: &'a mut SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        let _ = (fence, key, mutate);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn delete_guarded<'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let _ = (fence, key);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(async move {
            let mut page = ObjectStore::list_page(self.local.as_ref(), request).await?;
            let mut objects = Vec::with_capacity(page.objects.len());
            for metadata in page.objects {
                if !ObjectStore::is_remote_reference(self.local.as_ref(), &metadata.key).await? {
                    objects.push(metadata);
                }
            }
            page.objects = objects;
            Ok(page)
        })
    }
}

impl AssetStore for LocalMigrationSource {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        ObjectStore::stat(self, key)
    }
    fn read_stream<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        self.local_stream(key, 0..u64::MAX)
    }
    fn read_stream_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        self.local_stream(key, range)
    }
    fn read_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(self.local_collect(key, range, true))
    }
    fn write_stream<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        let _ = (key, stream);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn write_stream_guarded<'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        let _ = (fence, key, stream);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }
    fn copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let _ = (source, destination);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
    fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let _ = (source, destination);
        Box::pin(async { Err(NarouError::Platform("canonical-local migration source is read-only".into())) })
    }
}

/// Dedup lists only legacy physical aliases, reads through the ordinary shared
/// facade, and retires only that legacy alias under the original native lock.
struct LegacyDedupSource {
    store: NovelMigrationStore,
    s3: Arc<S3Store>,
}

impl ObjectStore for LegacyDedupSource {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> { ObjectStore::stat(&self.store, key) }
    fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> { self.s3.list_page(request) }
    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> { self.store.read_small(key) }
    fn write_small<'a>(&'a self, key: &'a ObjectKey, bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> { self.store.write_small(key, bytes) }
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> { ObjectStore::delete(&self.store, key) }
    fn retire_legacy<'a>(&'a self, logical: &'a ObjectKey, pool: &'a ObjectKey, size: u64, sha256: &'a str) -> PlatformFuture<'a, Result<()>> {
        self.store.retire_legacy(logical, pool, size, sha256)
    }
}

impl AssetStore for LegacyDedupSource {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> { AssetStore::stat(&self.store, key) }
    fn read_stream<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<AssetStream>>> { self.store.read_stream(key) }
    fn read_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> { AssetStore::read_range(&self.store, key, range) }
    fn write_stream<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> { self.store.write_stream(key, stream) }
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> { ObjectStore::delete(self, key) }
    fn copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> { self.store.copy(source, destination) }
    fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> { self.store.move_or_copy(source, destination) }
}

/// Copy or verify illustrations with one operation origin shared by both sides.
pub async fn migrate_illustrations(action: &str) -> Result<crate::platform::StoreMigrationState> {
    let stores = NativeStores::for_current_root()?;
    if stores.s3.is_none() { return Err(NarouError::Platform("挿絵の保存先が S3 になっていません".into())); }
    let prefixes = NativePrefixes::load().await?;
    let mut state = crate::platform::StoreMigrationState::default();
    while !state.done {
        let authority: Arc<dyn ObjectStore> = stores.primary.clone();
        let origins = NovelMigrationOrigins::new(authority, prefixes.clone())?;
        // The migration source must read canonical-local objects only: a
        // preferred mirror file could be stale bytes a newer immutable
        // reference already outranks, and remote keys must never resolve to
        // S3 here. The facade performs the ref check and byte open under one
        // lock boundary.
        let facade = Arc::new(LocalMigrationSource { local: stores.primary.clone(), prefixes: prefixes.clone() });
        let local_objects: Arc<dyn ObjectStore> = facade.clone();
        let local_assets: Arc<dyn AssetStore> = facade;
        let source_reads = NovelReadScope::new(&local_objects, "native-migration-source");
        let target_reads = NovelReadScope::new(&stores.objects, "native-migration-target");
        let source = Arc::new(NovelMigrationStore::new(local_objects, local_assets, origins.clone(), source_reads.clone())?);
        let target = Arc::new(NovelMigrationStore::new(stores.objects.clone(), stores.assets.clone(), origins, target_reads.clone())?);
        let result = crate::platform::migrate_page(
            &(source.clone() as Arc<dyn ObjectStore>), &(source.clone() as Arc<dyn AssetStore>),
            &(target.clone() as Arc<dyn AssetStore>), &(target.clone() as Arc<dyn ObjectStore>),
            action, crate::platform::store_migration::DEFAULT_LIMIT, &mut state,
        ).await;
        drop(source); drop(target);
        let source_released = source_reads.release_all().await;
        let target_released = target_reads.release_all().await;
        result?; source_released?; target_released?;
    }
    Ok(state)
}

/// Promote verified hash images to the pool and retire only legacy aliases.
pub async fn deduplicate_illustrations(action: &str) -> Result<crate::platform::IllustrationDedupState> {
    let stores = NativeStores::for_current_root()?;
    let s3 = stores.s3.as_ref().ok_or_else(|| NarouError::Platform("挿絵の保存先が S3 になっていません".into()))?.clone();
    let prefixes = NativePrefixes::load().await?;
    let mut state = crate::platform::IllustrationDedupState::default();
    while !state.done {
        let authority: Arc<dyn ObjectStore> = stores.primary.clone();
        let origins = NovelMigrationOrigins::new(authority, prefixes.clone())?;
        let reads = NovelReadScope::new(&stores.objects, "native-illustration-dedup");
        let source = LegacyDedupSource {
            store: NovelMigrationStore::new(stores.objects.clone(), stores.assets.clone(), origins.clone(), reads.clone())?,
            s3: s3.clone(),
        };
        let result = crate::platform::store_migration::deduplicate_illustrations_page_with_source(
            s3.as_ref(), &source, &source, action, crate::platform::store_migration::MAX_LIMIT, &mut state,
        ).await;
        drop(source);
        let released = reads.release_all().await;
        result?; released?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_prefixes_use_current_metadata_not_process_cache() {
        struct DatabaseRestore(Option<crate::db::Database>);
        impl Drop for DatabaseRestore {
            fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); }
        }
        let root = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        let _legacy = crate::test_support::legacy_yaml_guard();
        std::fs::create_dir(root.path().join(".narou")).unwrap();
        let path = root.path().join(".narou/database.yaml");
        let mut record: crate::db::NovelRecord = serde_yaml::from_str("id: 1\ntitle: book\nauthor: author\nfile_title: book\nsitename: site\ntoc_url: https://example.test/book\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        // Only compare persisted metadata against a loaded cache; publishing a
        // novel and acquiring its mutation locks is not part of this test.
        std::fs::write(&path, serde_yaml::to_string(&std::collections::BTreeMap::from([(record.id, &record)])).unwrap()).unwrap();
        crate::db::init_database().unwrap();
        record.file_title = "moved-book".into();
        std::fs::write(path, serde_yaml::to_string(&std::collections::BTreeMap::from([(record.id, &record)])).unwrap()).unwrap();
        let prefixes = futures::executor::block_on(NativePrefixes::load()).unwrap();
        assert!(prefixes.0.iter().any(|prefix| prefix.as_ref() == "novels/site/moved-book"));
        assert!(!prefixes.0.iter().any(|prefix| prefix.as_ref() == "novels/site/book"));
        assert_eq!(crate::db::with_database(|database| Ok(database.get(record.id).unwrap().file_title.clone())).unwrap(), "book",
            "the regression must exercise a genuinely stale process cache");
    }
}
