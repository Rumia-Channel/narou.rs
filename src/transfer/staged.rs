//! Durable, immutable archive input. Validation completes before callers may
//! publish any file, and decoded bodies are staged one bounded stream at a time.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::{pin::Pin, task::{Context, Poll}};

use async_zip::base::read1::seek::ZipArchiveReader;
use futures::{StreamExt, io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncSeek}};

use crate::platform::{AssetReader, AssetStore, AssetStream, ObjectKey, StagedAsset};
use super::{ArchiveManifest, ArchiveLimits, Result, TransferError};
use super::validated::index_archive;
#[cfg(test)]
use super::validate_archive;

pub struct StagedNovel {
    pub index: usize,
    pub toc_url: String,
    pub title: String,
    pub files: Vec<(String, StagedAsset)>,
}

pub struct StagedArchive {
    /// Novel metadata for planning. File paths and verified sizes/digests are
    /// owned only by `novels[*].files`; manifest file arrays are empty here.
    pub manifest: ArchiveManifest,
    pub novels: Vec<StagedNovel>,
}

/// Apply the archive limit while receiving, not after buffering the request.
pub fn bounded_asset_stream(stream: AssetStream, limit: u64) -> AssetStream {
    Box::pin(futures::stream::try_unfold((stream, 0u64), move |(mut stream, read)| async move {
        match stream.next().await {
            None => Ok(None),
            Some(chunk) => {
                let chunk = chunk?;
                let size = read.checked_add(chunk.len() as u64)
                    .filter(|size| *size <= limit)
                    .ok_or_else(|| crate::error::NarouError::Platform("transfer stream exceeds size limit".into()))?;
                Ok(Some((chunk, (stream, size))))
            }
        }
    }))
}

impl StagedArchive {
    /// `cleanup` must be owned by the caller's cancellation guard. Every
    /// completed staging reservation is registered before another await.
    /// In-flight reservations are reconciled by the store's durable journal.
    pub async fn read(
        store: Arc<dyn AssetStore>, source: &StagedAsset, cleanup: &mut Vec<ObjectKey>,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Self> {
        if source.size == 0 {
            return Err(TransferError::TooLarge("empty or oversized archive".into()));
        }
        let reader = CancelReader { inner: AssetReader::new(store.clone(), source.key.clone(), source.size), cancelled: cancelled.clone() };
        let (archive, mut manifest, matches) = index_archive(reader, source.size, &ArchiveLimits::default()).await?;
        let inner = archive.inner().clone();
        drop(archive);
        let mut novels: Vec<_> = manifest.novels.iter().enumerate().map(|(index, novel)| StagedNovel {
            index, toc_url: novel.toc_url.clone(), title: novel.title.clone(), files: Vec::new(),
        }).collect();
        let mut total = 0u64;
        for matched in matches {
            let (novel, position, index, size) = (matched.novel, matched.position, matched.zip_index, matched.declared_size);
            total = total.checked_add(size).ok_or_else(|| TransferError::TooLarge("uncompressed total overflow".into()))?;
            let reader = CancelReader { inner: AssetReader::new(store.clone(), source.key.clone(), source.size), cancelled: cancelled.clone() };
            let file = ZipArchiveReader::new_with_inner(reader, inner.clone()).file_oneshot(index).await?;
            let stream: AssetStream = Box::pin(futures::stream::try_unfold(file, |mut file| async move {
                let mut bytes = vec![0u8; 64 * 1024];
                let count = file.read(&mut bytes).await?;
                if count == 0 { return Ok(None); }
                bytes.truncate(count);
                Ok(Some((bytes, file)))
            }));
            let staged = store.stage_stream(stream).await.map_err(storage)?;
            cleanup.push(staged.key.clone());
            let expected = &mut manifest.novels[novel].files[position];
            if staged.size != size { return Err(TransferError::InvalidArchive(format!("size mismatch for {:?}", expected.path))); }
            if let Some(hash) = &expected.sha256 && !hash.eq_ignore_ascii_case(&staged.sha256) {
                return Err(TransferError::HashMismatch { path: expected.path.clone(), expected: hash.clone(), actual: staged.sha256 });
            }
            novels[novel].files.push((std::mem::take(&mut expected.path), staged));
        }
        for novel in &mut manifest.novels { novel.files = Vec::new(); }
        Ok(Self { manifest, novels })
    }
}

fn storage(error: crate::error::NarouError) -> TransferError { TransferError::Storage(error.to_string()) }

struct CancelReader<R> { inner: R, cancelled: Option<Arc<AtomicBool>> }
impl<R> CancelReader<R> {
    fn check(&self) -> std::io::Result<()> {
        if self.cancelled.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(std::io::Error::other("transfer cancelled"));
        }
        Ok(())
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for CancelReader<R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &mut [u8]) -> Poll<std::io::Result<usize>> {
        self.check()?;
        Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl<R: AsyncBufRead + Unpin> AsyncBufRead for CancelReader<R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        let this = self.get_mut();
        this.check()?;
        Pin::new(&mut this.inner).poll_fill_buf(cx)
    }
    fn consume(mut self: Pin<&mut Self>, count: usize) { Pin::new(&mut self.inner).consume(count); }
}
impl<R: AsyncSeek + Unpin> AsyncSeek for CancelReader<R> {
    fn poll_seek(mut self: Pin<&mut Self>, cx: &mut Context<'_>, position: std::io::SeekFrom) -> Poll<std::io::Result<u64>> {
        self.check()?;
        Pin::new(&mut self.inner).poll_seek(cx, position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{ArchiveFile, ArchiveNovel, AsyncArchiveStream};

    #[test]
    fn async_archive_roundtrips_an_entry_larger_than_small_object_cap() {
        futures::executor::block_on(async {
            struct Generated { remaining: usize, state: u32 }
            impl AsyncRead for Generated {
                fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &mut [u8]) -> Poll<std::io::Result<usize>> {
                    assert!(bytes.len() <= 64 * 1024, "encoder requested an entry-sized read");
                    let count = bytes.len().min(self.remaining);
                    for byte in &mut bytes[..count] {
                        self.state ^= self.state << 13; self.state ^= self.state >> 17; self.state ^= self.state << 5;
                        *byte = self.state as u8;
                    }
                    self.remaining -= count;
                    Poll::Ready(Ok(count))
                }
            }
            // Use an explicitly collecting test sink, not a production path.
            let output = Arc::new(parking_lot::Mutex::new(Vec::new()));
            struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
            impl futures::io::AsyncWrite for Sink {
                fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
                    self.0.lock().extend_from_slice(bytes); Poll::Ready(Ok(bytes.len()))
                }
                fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> { Poll::Ready(Ok(())) }
                fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> { Poll::Ready(Ok(())) }
            }
            let mut writer = AsyncArchiveStream::new(Sink(output.clone()), "test".into(), "fixed".into()).unwrap();
            writer.begin_novel(ArchiveNovel { title: "large".into(), ..Default::default() }).unwrap();
            writer.add_reader("toc.yaml", &mut futures::io::Cursor::new(b"title: large\n")).await.unwrap();
            let size = 17 * 1024 * 1024;
            writer.add_reader("挿絵/large.bin", &mut Generated { remaining: size, state: 0x12345678 }).await.unwrap();
            writer.finish().await.unwrap();
            let zip = std::mem::take(&mut *output.lock());
            assert!(zip.len() > 16 * 1024 * 1024);
            let len = zip.len() as u64;
            let metadata = validate_archive(futures::io::Cursor::new(zip), len).await.unwrap();
            assert_eq!(metadata.novels[0].files[1].size, Some(size as u64));
            assert!(metadata.novels[0].files.iter().all(|file| file.bytes.is_empty()));
        });
    }

    #[test]
    fn late_hash_failure_leaves_existing_objects_untouched_and_stages_cleanable() {
        futures::executor::block_on(async {
            use std::io::Write;
            use crate::platform::{ObjectStore, mocks::MemoryObjectStore};
            let store = Arc::new(MemoryObjectStore::default());
            let live = ObjectKey::try_new("novels/site/book/toc.yaml").unwrap();
            ObjectStore::write_small(store.as_ref(), &live, b"existing".to_vec()).await.unwrap();
            let manifest = ArchiveManifest { novels: vec![ArchiveNovel {
                files: vec![
                    ArchiveFile { path: "toc.yaml".into(), size: Some(3), ..Default::default() },
                    ArchiveFile { path: "last.bin".into(), size: Some(3), sha256: Some("0".repeat(64)), ..Default::default() },
                ], ..Default::default()
            }], ..Default::default() };
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("manifest.json", options).unwrap();
            zip.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
            zip.start_file("novels/0/toc.yaml", options).unwrap(); zip.write_all(b"new").unwrap();
            zip.start_file("novels/0/last.bin", options).unwrap(); zip.write_all(b"bad").unwrap();
            let bytes = zip.finish().unwrap().into_inner();
            let source = store.stage_stream(Box::pin(futures::stream::once(async move { Ok(bytes) }))).await.unwrap();
            let mut cleanup = vec![source.key.clone()];
            assert!(matches!(StagedArchive::read(store.clone(), &source, &mut cleanup, None).await,
                Err(TransferError::HashMismatch { .. })));
            assert_eq!(ObjectStore::read_small(store.as_ref(), &live).await.unwrap(), Some(b"existing".to_vec()));
            assert!(cleanup.len() >= 2, "validated earlier entries stay unpublished");
            for key in cleanup {
                AssetStore::delete(store.as_ref(), &key).await.unwrap();
                assert!(AssetStore::stat(store.as_ref(), &key).await.unwrap().is_none());
            }
        });
    }

    #[test]
    fn streamed_zip_sizes_and_descriptors_match_an_independent_reader() {
        futures::executor::block_on(async {
            // Exercise ZIP64 descriptor records with unequal compressed and
            // uncompressed sizes, including the separately streamed manifest.
            let toc = b"title: strict ZIP\nbody: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
            let mut output = futures::io::Cursor::new(Vec::new());
            let mut writer = AsyncArchiveStream::new(&mut output, "test".into(), "fixed".into()).unwrap();
            writer.begin_novel(ArchiveNovel { title: "strict ZIP".into(), ..Default::default() }).unwrap();
            writer.add_reader("toc.yaml", &mut futures::io::Cursor::new(toc)).await.unwrap();
            writer.finish().await.unwrap();
            let bytes = output.into_inner();
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
            assert_eq!(archive.len(), 2);
            for index in 0..archive.len() {
                let mut file = archive.by_index(index).unwrap();
                let mut decoded = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut decoded).unwrap();
                assert_eq!(file.size(), decoded.len() as u64);
                assert_ne!(file.compressed_size(), file.size());
                if file.name().unwrap() == "novels/0/toc.yaml" { assert_eq!(decoded, toc); }
                let offset = (file.data_start().unwrap() + file.compressed_size()) as usize;
                let descriptor = &bytes[offset..offset + 24];
                assert_eq!(&descriptor[..4], b"PK\x07\x08");
                assert_eq!(u32::from_le_bytes(descriptor[4..8].try_into().unwrap()), file.crc32());
                let value = |start| u64::from_le_bytes(descriptor[start..start + 8].try_into().unwrap());
                assert_eq!(value(8), file.compressed_size());
                assert_eq!(value(16), file.size());
            }
        });
    }
}
