//! Durable, immutable archive input. Validation completes before callers may
//! publish any file, and decoded bodies are staged one bounded stream at a time.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::{pin::Pin, task::{Context, Poll}};

use async_zip::base::read1::{ZipOptions, seek::ZipArchiveReader};
use futures::{StreamExt, io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncSeek}};
use sha2::{Digest, Sha256};

use crate::platform::{AssetReader, AssetStore, AssetStream, ObjectKey, StagedAsset};
use super::archive::{reject_file_directory_conflicts, validate_relative_path};
use super::{ArchiveManifest, FORMAT_VERSION, MANIFEST_PATH, MAX_ARCHIVE_BYTES,
    MAX_ENTRIES, MAX_FILE_BYTES, MAX_MANIFEST_BYTES, MAX_NOVELS, MAX_TOTAL_UNCOMPRESSED_BYTES,
    Result, TransferError};

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
        if source.size == 0 || source.size > MAX_ARCHIVE_BYTES {
            return Err(TransferError::TooLarge("empty or oversized archive".into()));
        }
        let reader = CancelReader { inner: AssetReader::new(store.clone(), source.key.clone(), source.size), cancelled: cancelled.clone() };
        let (archive, mut manifest, matches) = index_archive(reader).await?;
        let inner = archive.inner().clone();
        drop(archive);
        let mut novels: Vec<_> = manifest.novels.iter().enumerate().map(|(index, novel)| StagedNovel {
            index, toc_url: novel.toc_url.clone(), title: novel.title.clone(), files: Vec::new(),
        }).collect();
        for (novel, position, index, size) in matches {
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

type Matches = Vec<(usize, usize, usize, u64)>;

async fn index_archive<R: AsyncBufRead + AsyncSeek + Unpin>(reader: R) -> Result<(ZipArchiveReader<R>, ArchiveManifest, Matches)> {
        let mut archive = ZipArchiveReader::open_with_options(reader, ZipOptions {
            max_cd_num_files: MAX_ENTRIES as u64,
            // Keep only offsets; a large central directory is not an input buffer.
            max_cd_num_files_load: 0,
            max_cd_size_in_bytes: MAX_ARCHIVE_BYTES,
            max_uncompressed_size_per_file: MAX_FILE_BYTES,
            max_compressed_size_per_file: MAX_ARCHIVE_BYTES,
            ..Default::default()
        }).await?;
        let mut seen = HashSet::new();
        let mut entries = HashMap::new();
        // Directory names are validated one at a time; retain only ZIP indices,
        // never MAX_ENTRIES copies of potentially 4 KiB paths.
        let mut directories: Vec<usize> = Vec::new();
        let mut manifest_entry = None;
        let mut total = 0u64;
        let count = archive.ceocdr().num_entries()? as usize;
        for index in 0..count {
            let file = archive.file(index).await?;
            let entry = file.cdr().expect("seek reader has central directory metadata");
            let raw = zip_name(entry.insecure_file_name.as_bytes(), entry.cdrh.gpf.language_encoding_flag())?;
            let directory = raw.ends_with('/');
            let path = validate_relative_path(if directory { &raw[..raw.len() - 1] } else { raw.as_ref() })?;
            if (entry.cdrh.v_made_by >> 8 == 3) && ((entry.cdrh.exter_attr >> 16) & 0o170000 == 0o120000) {
                return Err(TransferError::UnsafePath(format!("symlink {raw:?}")));
            }
            if !seen.insert(path_digest(&path.to_lowercase())) {
                return Err(TransferError::InvalidArchive(format!("duplicate entry {raw:?}")));
            }
            let size = entry.uncompressed_size()?;
            if size > MAX_FILE_BYTES { return Err(TransferError::TooLarge(format!("{path}: file too large"))); }
            total = total.checked_add(size).filter(|size| *size <= MAX_TOTAL_UNCOMPRESSED_BYTES)
                .ok_or_else(|| TransferError::TooLarge("total uncompressed size exceeds limit".into()))?;
            if directory {
                directories.push(index);
            } else if path == MANIFEST_PATH {
                if size > MAX_MANIFEST_BYTES { return Err(TransferError::TooLarge("manifest too large".into())); }
                manifest_entry = Some(index);
            } else {
                entries.insert(path_digest(&path), (index, size));
            }
        }
        drop(seen);
        let index = manifest_entry.ok_or_else(|| TransferError::InvalidArchive("manifest.json is missing".into()))?;
        let mut manifest = super::read_manifest(archive.file(index).await?).await?;
        if manifest.format_version != FORMAT_VERSION { return Err(TransferError::UnsupportedVersion(manifest.format_version)); }
        if manifest.novels.len() > MAX_NOVELS { return Err(TransferError::TooLarge("too many novels".into())); }
        let mut matches = Vec::new();
        let mut files = 1usize;
        for (novel_index, novel) in manifest.novels.iter_mut().enumerate() {
            super::attribute::normalize(novel)?;
            let mut paths = HashSet::new();
            for (position, file) in novel.files.iter_mut().enumerate() {
                files += 1;
                if files > MAX_ENTRIES { return Err(TransferError::TooLarge("too many manifest files".into())); }
                file.path = validate_relative_path(&file.path)?;
                if !paths.insert(path_digest(&file.path.to_lowercase())) {
                    return Err(TransferError::InvalidArchive(format!("duplicate file {:?}", file.path)));
                }
                if let Some(hash) = &file.sha256
                    && (hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())) {
                    return Err(TransferError::InvalidArchive("invalid sha256".into()));
                }
                let full = format!("novels/{novel_index}/{}", file.path);
                let (entry, size) = entries.remove(&path_digest(&full))
                    .ok_or_else(|| TransferError::InvalidArchive(format!("missing archive entry {full:?}")))?;
                if file.size.is_some_and(|declared| declared != size) {
                    return Err(TransferError::InvalidArchive(format!("size mismatch for {full:?}")));
                }
                matches.push((novel_index, position, entry, size));
            }
            reject_file_directory_conflicts(novel.files.iter().map(|file| file.path.as_str()), novel_index)?;
        }
        if let Some((index, _)) = entries.values().next() {
            return Err(TransferError::InvalidArchive(format!("unlisted archive entry at index {index}")));
        }
        // Directories also have headers/CRC and must not hide corrupt bodies.
        for index in directories {
            let mut file = archive.file(index).await?;
            let mut buffer = [0u8; 64 * 1024];
            while file.read(&mut buffer).await? != 0 {}
        }
    Ok((archive, manifest, matches))
}

/// Validate a seekable immutable source without retaining decoded file bodies.
/// Browser callers provide bounded Blob.slice/range readers, never ZIP bytes.
pub async fn validate_archive<R: AsyncBufRead + AsyncSeek + Unpin>(reader: R, size: u64) -> Result<ArchiveManifest> {
    if size == 0 || size > MAX_ARCHIVE_BYTES { return Err(TransferError::TooLarge("empty or oversized archive".into())); }
    let (mut archive, mut manifest, matches) = index_archive(reader).await?;
    let mut bytes = vec![0; 64 * 1024];
    for (novel, position, index, size) in matches {
        let mut entry = archive.file(index).await?;
        let mut hash = Sha256::new();
        let mut read = 0u64;
        loop {
            let count = entry.read(&mut bytes).await?;
            if count == 0 { break; }
            read += count as u64;
            hash.update(&bytes[..count]);
        }
        let expected = &mut manifest.novels[novel].files[position];
        if read != size { return Err(TransferError::InvalidArchive(format!("size mismatch for {:?}", expected.path))); }
        if let Some(expected_hash) = &expected.sha256 {
            let actual = hex::encode(hash.finalize());
            if !actual.eq_ignore_ascii_case(expected_hash) {
                return Err(TransferError::HashMismatch { path: expected.path.clone(), expected: expected_hash.clone(), actual });
            }
        }
        expected.size = Some(read);
    }
    Ok(manifest)
}

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

fn zip_name(bytes: &[u8], utf8: bool) -> Result<std::borrow::Cow<'_, str>> {
    if utf8 || bytes.is_ascii() {
        return std::str::from_utf8(bytes).map(std::borrow::Cow::Borrowed)
            .map_err(|_| TransferError::UnsafePath("invalid UTF-8 ZIP path".into()));
    }
    // ZIP's legacy filename encoding, matching the synchronous parser.
    const CP437: [char; 128] = [
        'Ç','ü','é','â','ä','à','å','ç','ê','ë','è','ï','î','ì','Ä','Å',
        'É','æ','Æ','ô','ö','ò','û','ù','ÿ','Ö','Ü','¢','£','¥','₧','ƒ',
        'á','í','ó','ú','ñ','Ñ','ª','º','¿','⌐','¬','½','¼','¡','«','»',
        '░','▒','▓','│','┤','╡','╢','╖','╕','╣','║','╗','╝','╜','╛','┐',
        '└','┴','┬','├','─','┼','╞','╟','╚','╔','╩','╦','╠','═','╬','╧',
        '╨','╤','╥','╙','╘','╒','╓','╫','╪','┘','┌','█','▄','▌','▐','▀',
        'α','ß','Γ','π','Σ','σ','µ','τ','Φ','Θ','Ω','δ','∞','φ','ε','∩',
        '≡','±','≥','≤','⌠','⌡','÷','≈','°','∙','·','√','ⁿ','²','■','\u{a0}',
    ];
    Ok(std::borrow::Cow::Owned(bytes.iter().map(|byte| {
        if *byte < 128 { *byte as char } else { CP437[(*byte - 128) as usize] }
    }).collect()))
}

fn path_digest(path: &str) -> [u8; 32] { Sha256::digest(path.as_bytes()).into() }

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
            // Exercise ZIP32 descriptor records with unequal compressed and
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
                let descriptor = &bytes[offset..offset + 16];
                assert_eq!(&descriptor[..4], b"PK\x07\x08");
                let value = |start| u32::from_le_bytes(descriptor[start..start + 4].try_into().unwrap());
                assert_eq!(value(4), file.crc32());
                assert_eq!(value(8) as u64, file.compressed_size());
                assert_eq!(value(12) as u64, file.size());
            }
        });
    }
}
