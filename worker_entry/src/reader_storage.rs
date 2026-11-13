//! Reader-side storage guard for the delivery Worker.
//!
//! The delivery Worker shares the source library (D1 objects, S3 content and
//! illustrations) with the update Worker, so every source write has to be
//! refused before it reaches a store. The only writable area is private
//! staging (`.narou-staging/…`): the EPUB stream needs immutable temporary
//! objects, and the S3 stage implementation owns their reservation journal
//! and expiry (`.narou-stage-journal/…` is written inside that
//! implementation, never through these adapters).
//!
//! Reads are delegated unchanged, so delivery keeps the same content,
//! streaming and consistency behaviour as the writer.

use std::sync::Arc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, ObjectUpdateResult, PlatformFuture, SmallObjectMutator, StagedAsset,
    ObjectFingerprint, ObjectFingerprintMismatch,
    staging::is_staging_key,
};

/// Source data belongs to the writer: delivery may read it, never change it.
fn source_write_denied(key: &ObjectKey) -> NarouError {
    NarouError::Platform(format!(
        "reader delivery may not write source object: {key}"
    ))
}

/// Private staging needs an S3 target: the reservation journal and the expiry
/// of a stage live next to the object in the same bucket.
fn staging_unavailable() -> NarouError {
    NarouError::Platform("reader delivery has no S3 private staging target".into())
}

/// The private staging area of one deployment.
pub(crate) struct StagingTarget {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
}

impl StagingTarget {
    pub(crate) fn new(objects: Arc<dyn ObjectStore>, assets: Arc<dyn AssetStore>) -> Self {
        Self { objects, assets }
    }
}

/// Read-only view of the shared source objects, with private staging allowed.
struct ReaderObjectStore {
    inner: Arc<dyn ObjectStore>,
    staging: Option<Arc<dyn ObjectStore>>,
}

/// Read-only view of the shared source assets, with private staging allowed.
struct ReaderAssetStore {
    inner: Arc<dyn AssetStore>,
    staging: Option<Arc<dyn AssetStore>>,
}

/// The stores a reader may use: shared source reads plus private staging.
pub(crate) struct ReaderStores {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
}

impl ReaderStores {
    /// Guard the composed source stores. `staging` is the private staging
    /// target; without it every staging write is refused instead of falling
    /// back to a source store.
    pub(crate) fn new(
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        staging: Option<StagingTarget>,
    ) -> Self {
        let staging_objects = staging.as_ref().map(|target| target.objects.clone());
        let staging_assets = staging.map(|target| target.assets);
        Self {
            objects: Arc::new(ReaderObjectStore {
                inner: objects,
                staging: staging_objects,
            }),
            assets: Arc::new(ReaderAssetStore {
                inner: assets,
                staging: staging_assets,
            }),
        }
    }

    pub(crate) fn objects(&self) -> Arc<dyn ObjectStore> {
        self.objects.clone()
    }

    pub(crate) fn assets(&self) -> Arc<dyn AssetStore> {
        self.assets.clone()
    }
}

impl ObjectStore for ReaderObjectStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        self.inner.stat(key)
    }

    fn exists<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        self.inner.exists(key)
    }

    fn read_small<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        self.inner.read_small(key)
    }

    fn verify_fingerprints<'a>(
        &'a self,
        fingerprints: &'a [ObjectFingerprint],
    ) -> PlatformFuture<'a, Result<Option<ObjectFingerprintMismatch<'a>>>> {
        self.inner.verify_fingerprints(fingerprints)
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_staging_key(key) {
                return Err(source_write_denied(key));
            }
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            staging.write_small(key, data).await
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        _mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move { Err(source_write_denied(key)) })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_staging_key(key) {
                return Err(source_write_denied(key));
            }
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            staging.delete(key).await
        })
    }

    fn delete_prefix<'a>(&'a self, prefix: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_staging_key(prefix) {
                return Err(source_write_denied(prefix));
            }
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            staging.delete_prefix(prefix).await
        })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        self.inner.list_page(request)
    }
}

impl AssetStore for ReaderAssetStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        self.inner.stat(key)
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        self.inner.read_stream(key)
    }

    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        self.inner.read_range(key, range)
    }

    fn stage_stream<'a>(
        &'a self,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<StagedAsset>> {
        Box::pin(async move {
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            staging.stage_stream(stream).await
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_staging_key(key) {
                return Err(source_write_denied(key));
            }
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            staging.write_stream(key, stream).await
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_staging_key(key) {
                return Err(source_write_denied(key));
            }
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            staging.delete(key).await
        })
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_staging_key(destination) {
                return Err(source_write_denied(destination));
            }
            let staging = self.staging.as_ref().ok_or_else(staging_unavailable)?;
            let stream = self.inner.read_stream(source).await?
                .ok_or_else(|| NarouError::Platform(format!("reader copy source is missing: {source}")))?;
            staging.write_stream(destination, stream).await
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        // A caller may accept either a move or a copy, but delivery must never
        // remove a source object, even when the destination is private staging.
        self.copy(source, destination)
    }
}
