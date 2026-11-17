#![cfg(feature = "native-runtime")]

#[path = "../worker_entry/src/reader_storage.rs"]
mod reader_storage;

use std::sync::Arc;
use narou_rs::application::novel_access::NovelReadLease;
use narou_rs::platform::{
    AssetStream, GateState, NovelAccessFence, ObjectKey, ObjectPrefix,
    ObjectStore, mocks::MemoryObjectStore,
};
use reader_storage::{ReaderStores, StagingTarget};

fn stream(bytes: &[u8]) -> AssetStream {
    Box::pin(futures::stream::iter([Ok(bytes.to_vec())]))
}

#[tokio::test]
async fn copying_into_private_staging_never_moves_or_reinterprets_source_data() {
    for shared_target in [false, true] {
        let source = Arc::new(MemoryObjectStore::new());
        let staging = if shared_target { source.clone() } else { Arc::new(MemoryObjectStore::new()) };
        let original = ObjectKey::try_new("novels/site/book/本文/1.yaml").unwrap();
        let destination = ObjectKey::try_new(".narou-staging/copy").unwrap();
        ObjectStore::write_small(source.as_ref(), &original, b"saved episode".to_vec()).await.unwrap();
        let reader = ReaderStores::new(source.clone(), source.clone(), Some(StagingTarget::new(staging.clone(), staging.clone())), None);

        reader.assets().move_or_copy(&original, &destination).await.unwrap();

        assert_eq!(ObjectStore::read_small(source.as_ref(), &original).await.unwrap().as_deref(), Some(b"saved episode".as_slice()));
        assert_eq!(ObjectStore::read_small(staging.as_ref(), &destination).await.unwrap().as_deref(), Some(b"saved episode".as_slice()));
        reader.assets().delete(&destination).await.unwrap();
        assert!(ObjectStore::read_small(staging.as_ref(), &destination).await.unwrap().is_none());
        assert_eq!(ObjectStore::read_small(source.as_ref(), &original).await.unwrap().as_deref(), Some(b"saved episode".as_slice()));
    }
}

#[tokio::test]
async fn source_mutations_are_rejected_without_changing_existing_objects() {
    let source = Arc::new(MemoryObjectStore::new());
    let staging = Arc::new(MemoryObjectStore::new());
    let original = ObjectKey::try_new("novels/site/book/toc.yaml").unwrap();
    let sibling = ObjectKey::try_new("novels/site/book/setting.ini").unwrap();
    let prefix = ObjectKey::try_new("novels/site/book").unwrap();
    let private = ObjectKey::try_new(".narou-staging/prepared").unwrap();
    ObjectStore::write_small(source.as_ref(), &original, b"original toc".to_vec()).await.unwrap();
    ObjectStore::write_small(source.as_ref(), &sibling, b"original settings".to_vec()).await.unwrap();
    let reader = ReaderStores::new(source.clone(), source.clone(), Some(StagingTarget::new(staging.clone(), staging.clone())), None);
    let objects = reader.objects();
    let assets = reader.assets();
    assets.write_stream(&private, stream(b"temporary xhtml")).await.unwrap();

    assert!(objects.write_small(&original, b"replacement".to_vec()).await.is_err());
    assert!(objects.delete(&original).await.is_err());
    assert!(objects.delete_prefix(&prefix).await.is_err());
    assert!(assets.write_stream(&original, stream(b"replacement")).await.is_err());
    assert!(assets.delete(&original).await.is_err());
    assert!(assets.copy(&private, &original).await.is_err());
    assert!(assets.move_or_copy(&private, &original).await.is_err());

    assert_eq!(ObjectStore::read_small(source.as_ref(), &original).await.unwrap().as_deref(), Some(b"original toc".as_slice()));
    assert_eq!(ObjectStore::read_small(source.as_ref(), &sibling).await.unwrap().as_deref(), Some(b"original settings".as_slice()));
    assert_eq!(ObjectStore::read_small(staging.as_ref(), &private).await.unwrap().as_deref(), Some(b"temporary xhtml".as_slice()));
}


fn novel_prefix() -> ObjectPrefix {
    ObjectPrefix::new("novels/site/book").unwrap()
}

fn section_key() -> ObjectKey {
    ObjectKey::try_new("novels/site/book/本文/1.yaml").unwrap()
}

/// The reader delegates admission to its own fence authority, never to the
/// source store's own fence: the source fence rows must stay untouched so a
/// miswired delivery store cannot lease (and therefore write) through it.
#[tokio::test]
async fn reader_admission_uses_the_delegated_fence_not_the_source_store() {
    let source = Arc::new(MemoryObjectStore::new());
    let staging = Arc::new(MemoryObjectStore::new());
    let authority = Arc::new(MemoryObjectStore::new());
    let prefix = novel_prefix();
    let key = section_key();
    ObjectStore::write_small(source.as_ref(), &key, b"leased body".to_vec()).await.unwrap();
    let reader = ReaderStores::new(
        source.clone(),
        source.clone(),
        Some(StagingTarget::new(staging.clone(), staging)),
        Some(authority.clone()),
    );
    let objects = reader.objects();

    // Behavioral proof of delegation, not a pointer check: only the
    // authority may observe admission state; the byte source stays silent.
    let mut lease = NovelReadLease::admit(&objects, &prefix, "reader-test").await.unwrap();
    assert_eq!(authority.gate_info(&prefix).await.unwrap().pending_readers, 1,
        "the delegated authority must hold the reader lease");
    assert_eq!(source.gate_info(&prefix).await.unwrap().pending_readers, 0,
        "the source store must not receive reader lease state");

    let body = lease.read_small(&key).await.unwrap();
    assert_eq!(body.as_deref(), Some(b"leased body".as_slice()),
        "leased reads still come from the source bytes");
    assert_eq!(authority.gate_info(&prefix).await.unwrap().pending_readers, 1);

    lease.release().await.unwrap();
    assert_eq!(authority.gate_info(&prefix).await.unwrap().pending_readers, 0,
        "a completed read must release its lease at the authority");
}

/// Without a control authority the delivery store must fail closed rather
/// than silently leasing through (and mutating) the shared source.
#[tokio::test]
async fn missing_fence_fails_closed_and_leaves_the_source_untouched() {
    let source = Arc::new(MemoryObjectStore::new());
    let prefix = novel_prefix();
    let key = section_key();
    ObjectStore::write_small(source.as_ref(), &key, b"canonical".to_vec()).await.unwrap();
    let reader = ReaderStores::new(source.clone(), source.clone(), None, None);
    let objects = reader.objects();

    assert!(objects.access_fence().is_none());
    assert!(NovelReadLease::admit(&objects, &prefix, "reader-test").await.is_err());
    let lease = narou_rs::platform::GateLease {
        reader_id: "fabricated".into(),
        epoch: 0,
        expires_at: i64::MAX,
    };
    assert!(objects
        .read_stream_leased(&prefix, &lease, &key, 0..u64::MAX)
        .await
        .is_err());
    assert_eq!(source.gate_info(&prefix).await.unwrap().pending_readers, 0);
    assert_eq!(ObjectStore::read_small(source.as_ref(), &key).await.unwrap().as_deref(),
        Some(b"canonical".as_slice()));
}

/// A gate closed under a new epoch ends every lease pinned to the old one:
/// renewal, the next logical open and an in-flight leased read must all
/// reject, and reopening must not resurrect the stale lease.
#[tokio::test]
async fn closed_or_replaced_gate_denies_the_lease_everywhere() {
    let source = Arc::new(MemoryObjectStore::new());
    let authority = Arc::new(MemoryObjectStore::new());
    let prefix = novel_prefix();
    let key = section_key();
    ObjectStore::write_small(source.as_ref(), &key, b"version one".to_vec()).await.unwrap();
    let reader = ReaderStores::new(
        source.clone(),
        source.clone(),
        None,
        Some(authority.clone()),
    );
    let objects = reader.objects();
    let mut lease = NovelReadLease::admit(&objects, &prefix, "reader-test").await.unwrap();
    let pinned = lease.lease().clone();
    assert_eq!(
        pinned.epoch,
        authority.gate_info(&prefix).await.unwrap().epoch,
        "admission pins the exact gate epoch",
    );

    let epoch = authority.close_gate(&prefix, "writer-op").await.unwrap();
    assert!(authority.gate_info(&prefix).await.unwrap().state == GateState::Closed);
    assert!(lease.validate().await.is_err(), "authoritative renewal must fail once the gate closed");
    assert!(objects
        .read_stream_leased(&prefix, &pinned, &key, 0..u64::MAX)
        .await
        .is_err(), "a leased read under the old epoch must fail");
    assert!(authority.admit_reader(&prefix, "newcomer", 30).await.is_err(),
        "a closed gate admits nobody");

    authority.open_gate(&prefix, "writer-op", epoch).await.unwrap();
    let reopened = authority.gate_info(&prefix).await.unwrap();
    assert!(reopened.state == GateState::Open && reopened.epoch == epoch);
    // The gate is open again at the same (closed-then-reopened) epoch, so the
    // stale lease stays dead; only a fresh admission can read.
    assert!(objects
        .read_stream_leased(&prefix, &pinned, &key, 0..u64::MAX)
        .await
        .is_err());
    let mut next = NovelReadLease::admit(&objects, &prefix, "reader-test").await.unwrap();
    let body = next.read_small(&key).await.unwrap();
    assert_eq!(body.as_deref(), Some(b"version one".as_slice()));
    next.release().await.unwrap();
}

/// `read_stream_leased` runs the renew → read → renew contract through the
/// delegated fence: an expired or fabricated lease never reaches the source
/// bytes, and each admitted read still streams the real content.
#[tokio::test]
async fn leased_chunk_reads_stay_pinned_to_the_admitted_lease() {
    use futures::StreamExt;
    let source = Arc::new(MemoryObjectStore::new());
    let authority = Arc::new(MemoryObjectStore::new());
    let prefix = novel_prefix();
    let key = section_key();
    ObjectStore::write_small(source.as_ref(), &key, b"pinned bytes".to_vec()).await.unwrap();
    let reader = ReaderStores::new(
        source.clone(),
        source.clone(),
        None,
        Some(authority.clone()),
    );
    let objects = reader.objects();

    let fabricated = narou_rs::platform::GateLease {
        reader_id: "fabricated".into(),
        epoch: authority.gate_info(&prefix).await.unwrap().epoch,
        expires_at: i64::MAX,
    };
    assert!(objects
        .read_stream_leased(&prefix, &fabricated, &key, 0..u64::MAX)
        .await
        .is_err(), "a fabricated lease must not read source bytes");

    let lease = NovelReadLease::admit(&objects, &prefix, "reader-test").await.unwrap();
    let mut stream = objects
        .read_stream_leased(&prefix, lease.lease(), &key, 0..u64::MAX)
        .await
        .unwrap()
        .expect("the section exists");
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(bytes, b"pinned bytes");

    let outside = ObjectKey::try_new("novels/site/other/本文/1.yaml").unwrap();
    assert!(objects
        .read_stream_leased(&prefix, lease.lease(), &outside, 0..u64::MAX)
        .await
        .is_err(), "a lease never covers a different novel prefix");
    lease.release().await.unwrap();
    assert_eq!(authority.gate_info(&prefix).await.unwrap().pending_readers, 0);
}

#[tokio::test]
async fn mutable_fingerprint_fallback_keeps_the_original_admitted_reader() {
    use narou_rs::platform::{ObjectFingerprint, ObjectFingerprintMismatch, ObjectListPage, ObjectListRequest, ObjectMetadata, PlatformFuture};
    use narou_rs::error::{NarouError, Result};
    use sha2::{Digest, Sha256};

    struct LeasedOnlySource(Arc<MemoryObjectStore>);
    impl ObjectStore for LeasedOnlySource {
        fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            ObjectStore::stat(self.0.as_ref(), key)
        }
        fn read_small<'a>(&'a self, _key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
            Box::pin(async { Err(NarouError::Platform("unguarded fingerprint read".into())) })
        }
        fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
            ObjectStore::write_small(self.0.as_ref(), key, data)
        }
        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            ObjectStore::delete(self.0.as_ref(), key)
        }
        fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
            ObjectStore::list_page(self.0.as_ref(), request)
        }
    }

    let source = Arc::new(MemoryObjectStore::new());
    let authority = Arc::new(MemoryObjectStore::new());
    let prefix = novel_prefix();
    let key = section_key();
    ObjectStore::write_small(source.as_ref(), &key, b"original".to_vec()).await.unwrap();
    let reader = ReaderStores::new(Arc::new(LeasedOnlySource(source.clone())), source.clone(), None, Some(authority.clone()));
    let mut lease = NovelReadLease::admit(&reader.objects(), &prefix, "fingerprint-reader").await.unwrap();
    let fingerprints = [ObjectFingerprint { key: key.clone(), sha256: Sha256::digest(b"original").into() }];
    assert_eq!(lease.verify_fingerprints(&fingerprints).await.unwrap(), None);

    ObjectStore::write_small(source.as_ref(), &key, b"changed".to_vec()).await.unwrap();
    assert_eq!(lease.verify_fingerprints(&fingerprints).await.unwrap(), Some(ObjectFingerprintMismatch::Changed(&key)));
    authority.close_gate(&prefix, "replacement").await.unwrap();
    assert!(lease.verify_fingerprints(&fingerprints).await.is_err());
    lease.release().await.unwrap();
}
