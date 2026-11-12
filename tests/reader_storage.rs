#![cfg(feature = "native-runtime")]

#[path = "../worker_entry/src/reader_storage.rs"]
mod reader_storage;

use std::sync::Arc;
use narou_rs::platform::{AssetStream, ObjectKey, ObjectStore, mocks::MemoryObjectStore};
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
        let reader = ReaderStores::new(source.clone(), source.clone(), Some(StagingTarget::new(staging.clone(), staging.clone())));

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
    let reader = ReaderStores::new(source.clone(), source.clone(), Some(StagingTarget::new(staging.clone(), staging.clone())));
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
