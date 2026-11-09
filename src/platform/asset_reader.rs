//! Seekable pull reader backed by bounded, exact object ranges.
//!
//! Two fixed 64 KiB windows retain both a ZIP directory page and its current
//! entry page. Misses discard the least-recent window before issuing a range.

use std::{io::{self, SeekFrom}, pin::Pin, sync::Arc, task::{Context, Poll}};
use futures::io::{AsyncBufRead, AsyncRead, AsyncSeek};
use super::{AssetStore, ObjectKey, PlatformFuture};
use super::staging::ASSET_CHUNK_BYTES;

type PendingRead = PlatformFuture<'static, crate::error::Result<Option<Vec<u8>>>>;

pub struct AssetReader {
    store: Arc<dyn AssetStore>,
    key: ObjectKey,
    size: u64,
    position: u64,
    cache: Vec<u8>,
    cache_start: u64,
    spare: Vec<u8>,
    spare_start: u64,
    pending: Option<PendingRead>,
    pending_start: u64,
    pending_length: usize,
}

impl AssetReader {
    pub fn new(store: Arc<dyn AssetStore>, key: ObjectKey, size: u64) -> Self {
        Self { store, key, size, position: 0, cache: Vec::new(), cache_start: 0,
            spare: Vec::new(), spare_start: 0, pending: None, pending_start: 0, pending_length: 0 }
    }

    fn poll_cache(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.position >= self.size { return Poll::Ready(Ok(())); }
        if self.position >= self.cache_start
            && self.position - self.cache_start < self.cache.len() as u64 {
            return Poll::Ready(Ok(()));
        }
        if self.position >= self.spare_start
            && self.position - self.spare_start < self.spare.len() as u64 {
            std::mem::swap(&mut self.cache, &mut self.spare);
            std::mem::swap(&mut self.cache_start, &mut self.spare_start);
            return Poll::Ready(Ok(()));
        }
        if self.pending.is_none() {
            if !self.cache.is_empty() {
                self.spare = std::mem::take(&mut self.cache);
                self.spare_start = self.cache_start;
            }
            let start = self.position / ASSET_CHUNK_BYTES as u64 * ASSET_CHUNK_BYTES as u64;
            let end = start.saturating_add(ASSET_CHUNK_BYTES as u64).min(self.size);
            self.pending_start = start;
            self.pending_length = (end - start) as usize;
            let store = self.store.clone();
            let key = self.key.clone();
            self.pending = Some(Box::pin(async move { store.read_range(&key, start..end).await }));
        }
        let result = match self.pending.as_mut().expect("pending range").as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        self.pending = None;
        let bytes = match result {
            Ok(Some(bytes)) if bytes.len() == self.pending_length => bytes,
            Ok(_) => return Poll::Ready(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "asset disappeared or range was truncated"))),
            Err(error) => return Poll::Ready(Err(io::Error::other(error.to_string()))),
        };
        self.cache_start = self.pending_start;
        self.cache = bytes;
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for AssetReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        futures::ready!(this.poll_cache(cx))?;
        if this.position >= this.size { return Poll::Ready(Ok(&[])); }
        Poll::Ready(Ok(&this.cache[(this.position - this.cache_start) as usize..]))
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        let available = this.cache.len().saturating_sub(this.position.saturating_sub(this.cache_start) as usize);
        this.position += amount.min(available) as u64;
    }
}

impl AsyncRead for AssetReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, output: &mut [u8]) -> Poll<io::Result<usize>> {
        if output.is_empty() { return Poll::Ready(Ok(0)); }
        let bytes = futures::ready!(self.as_mut().poll_fill_buf(cx))?;
        let length = bytes.len().min(output.len());
        output[..length].copy_from_slice(&bytes[..length]);
        self.consume(length);
        Poll::Ready(Ok(length))
    }
}

impl AsyncSeek for AssetReader {
    fn poll_seek(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, from: SeekFrom) -> Poll<io::Result<u64>> {
        let position = match from {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
            SeekFrom::End(value) => i128::from(self.size) + i128::from(value),
        };
        if !(0..=u64::MAX as i128).contains(&position) {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid asset seek")));
        }
        self.pending = None;
        self.position = position as u64;
        Poll::Ready(Ok(self.position))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::io::{AsyncReadExt, AsyncSeekExt};
    use crate::platform::{AssetStream, ObjectMetadata, ObjectStore, mocks::MemoryObjectStore};
    use crate::error::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) struct RangeCountingStore {
        inner: MemoryObjectStore,
        ranges: AtomicUsize,
    }

    impl RangeCountingStore {
        pub(crate) fn new() -> Self {
            Self { inner: MemoryObjectStore::new(), ranges: AtomicUsize::new(0) }
        }
        pub(crate) fn range_count(&self) -> usize { self.ranges.load(Ordering::Relaxed) }
    }

    impl AssetStore for RangeCountingStore {
        fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            AssetStore::stat(&self.inner, key)
        }
        fn read_stream<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
            self.inner.read_stream(key)
        }
        fn read_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
            assert!(range.end >= range.start && range.end - range.start <= ASSET_CHUNK_BYTES as u64);
            self.ranges.fetch_add(1, Ordering::Relaxed);
            self.inner.read_range(key, range)
        }
        fn write_stream<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
            self.inner.write_stream(key, stream)
        }
        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            AssetStore::delete(&self.inner, key)
        }
        fn copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            self.inner.copy(source, destination)
        }
        fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            self.inner.move_or_copy(source, destination)
        }
    }

    #[test]
    fn alternating_seeks_reuse_two_bounded_windows() {
        futures::executor::block_on(async {
            let store = Arc::new(RangeCountingStore::new());
            let key = ObjectKey::try_new("reader/alternating").unwrap();
            let bytes: Vec<u8> = (0..ASSET_CHUNK_BYTES * 4 + 29).map(|value| (value % 251) as u8).collect();
            store.inner.write_small(&key, bytes.clone()).await.unwrap();
            let mut reader = AssetReader::new(store.clone(), key, bytes.len() as u64);
            let mut chunk = [0; 17];
            for _ in 0..100 {
                for offset in [100, ASSET_CHUNK_BYTES * 3 + 123] {
                    reader.seek(SeekFrom::Start(offset as u64)).await.unwrap();
                    reader.read_exact(&mut chunk).await.unwrap();
                    assert_eq!(&chunk, &bytes[offset..offset + chunk.len()]);
                    assert!(reader.cache.len() + reader.spare.len() <= ASSET_CHUNK_BYTES * 2);
                }
            }
            assert_eq!(store.range_count(), 2);
            reader.seek(SeekFrom::Start(ASSET_CHUNK_BYTES as u64 + 90)).await.unwrap();
            reader.read_exact(&mut chunk).await.unwrap();
            assert_eq!(&chunk, &bytes[ASSET_CHUNK_BYTES + 90..ASSET_CHUNK_BYTES + 107]);
            assert_eq!(store.range_count(), 3);
            reader.seek(SeekFrom::End(-17)).await.unwrap();
            reader.read_exact(&mut chunk).await.unwrap();
            assert_eq!(&chunk, &bytes[bytes.len() - chunk.len()..]);
            assert_eq!(reader.read(&mut chunk).await.unwrap(), 0);
            assert!(reader.seek(SeekFrom::Current(-(bytes.len() as i64) - 1)).await.is_err());
        });
    }

    #[test]
    fn reads_across_cache_and_seeks_backwards() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            let key = ObjectKey::try_new("reader/test").unwrap();
            let bytes: Vec<u8> = (0..200_000).map(|value| value as u8).collect();
            store.write_small(&key, bytes.clone()).await.unwrap();
            let mut reader = AssetReader::new(store, key, bytes.len() as u64);
            reader.seek(SeekFrom::Start(65_530)).await.unwrap();
            let mut chunk = [0; 100];
            reader.read_exact(&mut chunk).await.unwrap();
            assert_eq!(&chunk, &bytes[65_530..65_630]);
            reader.seek(SeekFrom::End(-40)).await.unwrap();
            let mut end = Vec::new();
            reader.read_to_end(&mut end).await.unwrap();
            assert_eq!(end, bytes[bytes.len()-40..]);
            assert!(reader.seek(SeekFrom::Current(-300_000)).await.is_err());
        });
    }
}
