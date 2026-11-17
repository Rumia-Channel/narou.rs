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
//! streaming and consistency behaviour as the writer. Per-novel admission
//! (`NovelAccessFence`) is the one source-side mutation a read needs —
//! reader lease rows live next to the gates — so the stores expose an
//! explicitly supplied fence instead of the source store's own: the reader
//! must not write the shared D1, and admission/renewal still has to hit the
//! same durable CAS authority (the writer control path) as the writer uses.
//! Writer-side primitives (close/open/commit) stay unavailable here; a
//! reader deployment has no writer fence.

use std::sync::Arc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, AssetStream, GateLease, NovelAccessFence, ObjectKey, ObjectListPage,
    ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectStore, ObjectUpdateResult,
    PlatformFuture, SmallObjectMutator, StagedAsset,
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

/// Reader admission requires a durable lease mutation; without the delegated
/// control-plane authority there is no safe fallback (the source store's own
/// fence would write the shared D1 from this delivery isolate).
fn unsupported_fence(prefix: &ObjectPrefix) -> NarouError {
    NarouError::Platform(format!(
        "novel access fence unsupported by this store: {}",
        prefix.as_ref()
    ))
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
/// Leased reads keep the primary stores' renew → read → renew contract but
/// renew through the delegated control fence, never the source store's own.
struct ReaderObjectStore {
    inner: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    fence: Option<Arc<dyn NovelAccessFence>>,
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
    /// back to a source store. `fence` is the durable novel-access authority
    /// admitted readers are bound to (the writer's control path on the real
    /// deployment); `None` keeps delivery fail-closed rather than leasing
    /// through a store that would write the shared source.
    pub(crate) fn new(
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        staging: Option<StagingTarget>,
        fence: Option<Arc<dyn NovelAccessFence>>,
    ) -> Self {
        let staging_objects = staging.as_ref().map(|target| target.objects.clone());
        let staging_assets = staging.map(|target| target.assets);
        Self {
            objects: Arc::new(ReaderObjectStore {
                inner: objects,
                assets: assets.clone(),
                fence,
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
    /// The delegated control-plane authority, never the source store's own
    /// fence: leasing through `inner` would write the shared D1 from this
    /// delivery isolate.
    fn access_fence(&self) -> Option<&dyn NovelAccessFence> {
        self.fence.as_deref()
    }

    /// Same renew → read → renew contract as the primary stores, but the
    /// lease CAS runs against the delegated fence while the bytes come from
    /// the source assets (read-only). The renewed lease keeps the admitted
    /// epoch; every logical open still carries the caller's pinned lease.
    fn read_stream_leased<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        lease: &'a GateLease,
        key: &'a ObjectKey,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move {
            if !prefix.matches(key) {
                return Err(NarouError::Platform(format!(
                    "reader outside novel: {key}"
                )));
            }
            let fence = self
                .access_fence()
                .ok_or_else(|| unsupported_fence(prefix))?;
            fence
                .renew_reader(prefix, lease, narou_rs::platform::READER_LEASE_SECS)
                .await?;
            let stream = self.assets.read_stream_range(key, range).await?;
            fence
                .renew_reader(prefix, lease, narou_rs::platform::READER_LEASE_SECS)
                .await?;
            Ok(stream)
        })
    }

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

    fn verify_fingerprints<'a, 'input: 'a>(
        &'a self,
        fingerprints: &'input [ObjectFingerprint],
        reader: &'a mut dyn narou_rs::platform::ObjectFingerprintReader,
    ) -> PlatformFuture<'a, Result<Option<ObjectFingerprintMismatch<'input>>>> {
        self.inner.verify_fingerprints(fingerprints, reader)
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

    fn read_stream_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        self.inner.read_stream_range(key, range)
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
