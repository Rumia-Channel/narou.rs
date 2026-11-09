//! Pull-driven ZIP encoding with working-set admission before metadata growth.
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_zip::{Compression, ZipEntryBuilder, base::write::ZipFileWriter};
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use sha2::{Digest, Sha256};

use super::archive::validate_relative_path;
use super::memory::{MetadataBudget, allocation_limit, default_limit};
use super::{ArchiveFile, ArchiveManifest, ArchiveNovel, FORMAT_VERSION, MANIFEST_PATH,
    MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_FILE_BYTES, MAX_MANIFEST_BYTES, MAX_NOVELS,
    MAX_TOTAL_UNCOMPRESSED_BYTES, Result, TransferError};

struct LimitedWriter<W> { inner: W, written: u64 }
impl<W: AsyncWrite + Unpin> AsyncWrite for LimitedWriter<W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
        if self.written.saturating_add(bytes.len() as u64) > MAX_ARCHIVE_BYTES {
            return Poll::Ready(Err(std::io::Error::other("transfer ZIP exceeds 512 MiB")));
        }
        match Pin::new(&mut self.inner).poll_write(cx, bytes) {
            Poll::Ready(Ok(count)) => { self.written += count as u64; Poll::Ready(Ok(count)) }
            other => other,
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> { Pin::new(&mut self.inner).poll_flush(cx) }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> { Pin::new(&mut self.inner).poll_close(cx) }
}

/// A salted digest set backed only by explicitly admitted Vec allocations.
/// Tracking occupied slots makes per-novel clear proportional to that novel,
/// not to a previous novel's largest table. No opaque HashSet allocation occurs.
#[derive(Default)]
struct PathIndex {
    slots: Vec<Option<[u8; 32]>>, occupied: Vec<usize>, hasher: RandomState,
}
impl PathIndex {
    fn slot(&self, slots: &[Option<[u8; 32]>], key: &[u8; 32]) -> usize {
        let mut index = self.hasher.hash_one(key) as usize & (slots.len() - 1);
        while slots[index].is_some_and(|value| value != *key) { index = (index + 1) & (slots.len() - 1); }
        index
    }
    fn contains(&self, key: &[u8; 32]) -> bool {
        !self.slots.is_empty() && self.slots[self.slot(&self.slots, key)].is_some()
    }
    fn insert(&mut self, key: [u8; 32], memory: &mut MetadataBudget) -> Result<bool> {
        if self.contains(&key) { return Ok(false); }
        memory.reserve(&mut self.occupied, 1)?;
        if self.slots.is_empty() || (self.occupied.len() + 1) * 8 > self.slots.len() * 7 {
            let length = self.slots.len().checked_mul(2).ok_or_else(allocation_limit)?.max(8);
            let mut slots = Vec::new();
            memory.reserve(&mut slots, length)?;
            slots.resize(length, None);
            for position in 0..self.occupied.len() {
                let old = self.occupied[position];
                let value = self.slots[old].take().expect("occupied path slot");
                let new = self.slot(&slots, &value);
                slots[new] = Some(value);
                self.occupied[position] = new;
            }
            memory.release(&self.slots);
            self.slots = slots;
        }
        let index = self.slot(&self.slots, &key);
        self.slots[index] = Some(key);
        self.occupied.push(index);
        Ok(true)
    }
    fn clear(&mut self) {
        for index in self.occupied.drain(..) { self.slots[index] = None; }
    }
}

/// Sink writes are awaited within every entry. Wasm's metadata admission covers
/// the retained manifest, the ZIP writer's duplicate filenames/record capacity,
/// and the path index, including old+new storage during growth. Native retains
/// its existing format limits instead of receiving a new heap limit.
pub struct AsyncArchiveStream<W: AsyncWrite + Unpin> {
    writer: ZipFileWriter<LimitedWriter<W>>, manifest: ArchiveManifest,
    paths: PathIndex, memory: MetadataBudget, files: usize, uncompressed: u64,
}
impl<W: AsyncWrite + Unpin> AsyncArchiveStream<W> {
    pub fn new(writer: W, app_version: String, exported_at: String) -> Result<Self> {
        Self::with_manifest(writer, ArchiveManifest { format_version: FORMAT_VERSION,
            app_version: Some(app_version), exported_at: Some(exported_at), novels: Vec::new() }, default_limit())
    }

    fn with_manifest(writer: W, mut manifest: ArchiveManifest, limit: usize) -> Result<Self> {
        if manifest.format_version != FORMAT_VERSION { return Err(TransferError::UnsupportedVersion(manifest.format_version)); }
        if manifest.novels.len() > MAX_NOVELS { return Err(TransferError::TooLarge("too many novels".into())); }
        let files = manifest.novels.iter().try_fold(1usize, |count, novel| count.checked_add(novel.files.len()))
            .ok_or_else(allocation_limit)?;
        if files > MAX_ENTRIES { return Err(TransferError::TooLarge("too many files".into())); }
        let mut memory = MetadataBudget { used: 0, limit };
        memory.adopt_manifest(&manifest)?;
        // Legacy classification normalization replaces an existing tag string;
        // it cannot grow the retained model beyond the admitted original.
        for novel in &mut manifest.novels { super::attribute::normalize(novel)?; }
        memory.used = 0;
        memory.adopt_manifest(&manifest)?;
        Ok(Self { writer: ZipFileWriter::new(LimitedWriter { inner: writer, written: 0 }).force_no_zip64(),
            manifest, paths: PathIndex::default(), memory, files: 1, uncompressed: 0 })
    }

    pub fn begin_novel(&mut self, mut novel: ArchiveNovel) -> Result<()> {
        if let Some(index) = self.manifest.novels.len().checked_sub(1) { self.end_novel(index)?; }
        if self.manifest.novels.len() >= MAX_NOVELS { return Err(TransferError::TooLarge("too many novels".into())); }
        // Incoming file bodies/records aren't part of incremental metadata.
        novel.files = Vec::new();
        let mut owned = MetadataBudget { used: 0, limit: self.memory.limit };
        owned.adopt_novel(&novel)?;
        self.memory.check(owned.used)?;
        super::attribute::normalize(&mut novel)?;
        self.memory.adopt_novel(&novel)?;
        self.memory.reserve(&mut self.manifest.novels, 1)?;
        self.manifest.novels.push(novel);
        self.paths.clear();
        Ok(())
    }

    fn normalized_path(&mut self, path: &str) -> Result<String> {
        // The format validator rejects paths over 4096 bytes before copying.
        self.memory.check(MetadataBudget::allocation::<u8>(path.len().min(4096))?)?;
        let path = validate_relative_path(path)?;
        self.memory.charge(MetadataBudget::allocation::<u8>(path.capacity())?)?;
        Ok(path)
    }
    fn folded_path(&self, path: &str) -> Result<[u8; 32]> {
        // Unicode lowercase may expand and relocate its buffer. These short
        // path temporaries are admitted too, without changing case semantics.
        let peak = MetadataBudget::allocation::<u8>(path.len().saturating_mul(6))?
            .checked_add(MetadataBudget::allocation::<u8>(path.len().saturating_mul(3))?).ok_or_else(allocation_limit)?;
        self.memory.check(peak)?;
        Ok(Sha256::digest(path.to_lowercase().as_bytes()).into())
    }
    fn reserve_directory_entry(&mut self) -> Result<()> {
        let length = self.writer.entry_metadata_len();
        let previous = self.writer.entry_metadata_capacity();
        let size = self.writer.entry_metadata_record_size();
        let capacity = self.memory.growth(length, previous, 1, size)?;
        if capacity != previous {
            self.writer.try_reserve_entry_metadata(capacity - length).map_err(|_| allocation_limit())?;
            self.memory.replace_allocation(previous, self.writer.entry_metadata_capacity(), size)?;
        }
        Ok(())
    }

    pub async fn add_reader<R: AsyncRead + Unpin>(&mut self, path: &str, reader: &mut R) -> Result<()> {
        let index = self.manifest.novels.len().checked_sub(1)
            .ok_or_else(|| TransferError::InvalidArchive("missing novel".into()))?;
        let path = self.normalized_path(path)?;
        self.memory.reserve(&mut self.manifest.novels[index].files, 1)?;
        let (size, sha256) = self.entry(index, &path, reader).await?;
        self.manifest.novels[index].files.push(ArchiveFile { path, bytes: Vec::new(), sha256: Some(sha256), size: Some(size) });
        Ok(())
    }

    async fn entry<R: AsyncRead + Unpin>(&mut self, index: usize, path: &str, reader: &mut R) -> Result<(u64, String)> {
        let digest = self.folded_path(path)?;
        if !self.paths.insert(digest, &mut self.memory)? {
            return Err(TransferError::InvalidArchive(format!("duplicate file path {path:?}")));
        }
        self.files += 1;
        if self.files > MAX_ENTRIES { return Err(TransferError::TooLarge("too many files".into())); }
        let mut number = [0; 20];
        let index = super::manifest_write::decimal(index as u64, &mut number);
        let mut filename = self.memory.string(b"novels/".len() + index.len() + 1 + path.len())?;
        filename.push_str("novels/");
        filename.push_str(std::str::from_utf8(index).expect("decimal index"));
        filename.push('/'); filename.push_str(path);
        let mut sha256 = self.memory.string(64)?;
        self.reserve_directory_entry()?;
        let mut entry = self.writer.write_entry_stream(ZipEntryBuilder::new(filename.into(), Compression::Deflate)).await?;
        let mut bytes = vec![0; 64 * 1024];
        let mut hash = Sha256::new();
        let mut size = 0u64;
        loop {
            let count = reader.read(&mut bytes).await?;
            if count == 0 { break; }
            size += count as u64;
            self.uncompressed += count as u64;
            if size > MAX_FILE_BYTES || self.uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES {
                return Err(TransferError::TooLarge(format!("{path}: uncompressed size exceeds limit")));
            }
            hash.update(&bytes[..count]);
            entry.write_all(&bytes[..count]).await?;
        }
        entry.close().await?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in hash.finalize() { sha256.push(HEX[(byte >> 4) as usize] as char); sha256.push(HEX[(byte & 15) as usize] as char); }
        Ok((size, sha256))
    }

    fn end_novel(&self, index: usize) -> Result<()> {
        let novel = &self.manifest.novels[index];
        for file in &novel.files {
            for (slash, _) in file.path.match_indices('/') {
                let prefix = &file.path[..slash];
                if self.paths.contains(&self.folded_path(prefix)?) {
                    return Err(TransferError::InvalidArchive(format!("novel {index} file {prefix:?} is also used as a directory")));
                }
            }
        }
        if !novel.files.iter().any(|file| file.path == "toc.yaml") {
            return Err(TransferError::InvalidArchive(format!("novel {index} に toc.yaml がありません")));
        }
        Ok(())
    }

    /// Adopt a decoded metadata model once instead of retaining a source model
    /// beside a second generated manifest. File readers must be immutable and
    /// supply exactly the declared size. No whole-file bytes are accepted here.
    pub async fn write_manifest<R: AsyncRead + Unpin, F>(writer: W, manifest: ArchiveManifest, open: F) -> Result<()>
    where F: FnMut(usize, &str, u64) -> Result<R> {
        Self::write_manifest_with_limit(writer, manifest, open, default_limit()).await
    }
    async fn write_manifest_with_limit<R: AsyncRead + Unpin, F>(writer: W, manifest: ArchiveManifest, mut open: F, limit: usize) -> Result<()>
    where F: FnMut(usize, &str, u64) -> Result<R> {
        let mut writer = Self::with_manifest(writer, manifest, limit)?;
        for index in 0..writer.manifest.novels.len() {
            writer.paths.clear();
            for position in 0..writer.manifest.novels[index].files.len() {
                let file = &mut writer.manifest.novels[index].files[position];
                if !file.bytes.is_empty() { return Err(TransferError::InvalidArchive("manifest must contain metadata, not file bodies".into())); }
                let size = file.size.ok_or_else(|| TransferError::InvalidArchive("missing file size".into()))?;
                if size > MAX_FILE_BYTES { return Err(TransferError::TooLarge("file exceeds size limit".into())); }
                let original = std::mem::take(&mut file.path);
                let path = writer.normalized_path(&original)?;
                writer.memory.release_charge(MetadataBudget::allocation::<u8>(original.capacity())?);
                drop(original);
                // A reader factory may retain its own bounded path string.
                let reader_path = MetadataBudget::allocation::<u8>(path.len())?;
                writer.memory.charge(reader_path)?;
                let mut reader = open(index, &path, size)?;
                let (actual, sha256) = writer.entry(index, &path, &mut reader).await?;
                drop(reader);
                writer.memory.release_charge(reader_path);
                if actual != size { return Err(TransferError::InvalidArchive(format!("size mismatch for {path:?}"))); }
                let file = &mut writer.manifest.novels[index].files[position];
                if let Some(old) = &file.sha256 { writer.memory.release_charge(MetadataBudget::allocation::<u8>(old.capacity())?); }
                file.path = path; file.sha256 = Some(sha256); file.size = Some(actual);
            }
            writer.end_novel(index)?;
        }
        writer.finish().await
    }

    pub async fn finish(mut self) -> Result<()> {
        if let Some(index) = self.manifest.novels.len().checked_sub(1) { self.end_novel(index)?; }
        let mut count = super::stream::CountingWriter(0);
        serde_json::to_writer(&mut count, &self.manifest)?;
        if count.0 > MAX_MANIFEST_BYTES || self.uncompressed.saturating_add(count.0) > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(TransferError::TooLarge("manifest and files exceed uncompressed limit".into()));
        }
        let mut filename = self.memory.string(MANIFEST_PATH.len())?;
        filename.push_str(MANIFEST_PATH);
        self.reserve_directory_entry()?;
        let mut entry = self.writer.write_entry_stream(ZipEntryBuilder::new(filename.into(), Compression::Deflate)).await?;
        {
            let mut buffered = futures::io::BufWriter::with_capacity(64 * 1024, &mut entry);
            super::manifest_write::write(&mut buffered, &self.manifest).await?;
            buffered.flush().await?;
        }
        entry.close().await?;
        let mut sink = self.writer.close().await?;
        sink.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata() -> ArchiveManifest {
        ArchiveManifest { novels: vec![ArchiveNovel { title: "budget".into(),
            files: vec![ArchiveFile { path: "toc.yaml".into(), size: Some(3), ..Default::default() }],
            ..Default::default() }], ..Default::default() }
    }
    #[test]
    fn adopted_manifest_and_directory_growth_share_one_budget() {
        futures::executor::block_on(async {
            let manifest = metadata();
            let mut measured = MetadataBudget { used: 0, limit: usize::MAX };
            measured.adopt_manifest(&manifest).unwrap();
            let mut opened = false;
            let mut output = futures::io::Cursor::new(Vec::new());
            let result = AsyncArchiveStream::write_manifest_with_limit(&mut output, manifest, |_, _, _| {
                opened = true; Ok(futures::io::Cursor::new(b"toc"))
            }, measured.used - 1).await;
            assert!(matches!(result, Err(TransferError::TooLarge(_))));
            assert!(!opened);
            assert!(output.into_inner().is_empty());
        });
    }
    #[test]
    fn directory_record_growth_is_admitted_before_allocation_or_output() {
        let mut output = futures::io::Cursor::new(Vec::new());
        let mut writer = AsyncArchiveStream::with_manifest(&mut output, ArchiveManifest::default(), usize::MAX).unwrap();
        let bytes = MetadataBudget::allocation_bytes(1, writer.writer.entry_metadata_record_size()).unwrap();
        writer.memory.limit = bytes - 1;
        assert!(matches!(writer.reserve_directory_entry(), Err(TransferError::TooLarge(_))));
        assert_eq!(writer.writer.entry_metadata_capacity(), 0);
        drop(writer);
        assert!(output.into_inner().is_empty());
    }
    #[test]
    fn path_index_checks_reallocation_peak_and_preserves_duplicates() {
        let mut memory = MetadataBudget { used: 0, limit: 4096 };
        let mut paths = PathIndex::default();
        for byte in 0..7 { assert!(paths.insert([byte; 32], &mut memory).unwrap()); }
        assert!(!paths.insert([0; 32], &mut memory).unwrap());
        memory.limit = memory.used;
        let before = paths.slots.capacity();
        assert!(matches!(paths.insert([7; 32], &mut memory), Err(TransferError::TooLarge(_))));
        assert_eq!(paths.slots.capacity(), before);
        for byte in 0..7 { assert!(paths.contains(&[byte; 32])); }
        paths.clear();
        assert!(paths.insert([0; 32], &mut memory).unwrap());
        assert!(!paths.insert([0; 32], &mut memory).unwrap());
    }
    #[test]
    fn retained_filename_copies_eventually_exhaust_metadata_not_payload_limits() {
        futures::executor::block_on(async {
            struct ReadProbe<'a>(&'a mut usize);
            impl AsyncRead for ReadProbe<'_> {
                fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut [u8]) -> Poll<std::io::Result<usize>> {
                    *self.get_mut().0 += 1;
                    Poll::Ready(Ok(0))
                }
            }
            let mut output = futures::io::Cursor::new(Vec::new());
            let mut writer = AsyncArchiveStream::with_manifest(&mut output, ArchiveManifest::default(), 16 * 1024).unwrap();
            writer.begin_novel(ArchiveNovel::default()).unwrap();
            writer.add_reader("toc.yaml", &mut futures::io::Cursor::new(b"toc")).await.unwrap();
            let mut admitted = 0;
            loop {
                let path = format!("{admitted:04}-{}.bin", "a".repeat(120));
                let mut reads = 0;
                match writer.add_reader(&path, &mut ReadProbe(&mut reads)).await {
                    Ok(()) => { admitted += 1; assert_eq!(reads, 1); }
                    Err(TransferError::TooLarge(_)) => { assert_eq!(reads, 0); break; }
                    Err(error) => panic!("unexpected rejection: {error}"),
                }
                assert!(admitted < 100, "retained metadata was not charged");
            }
            assert!(admitted >= 4, "test must exercise cumulative metadata, not a single long path");
        });
    }
    #[test]
    fn adopted_manifest_roundtrips_without_a_second_file_record_model() {
        futures::executor::block_on(async {
            let mut output = futures::io::Cursor::new(Vec::new());
            AsyncArchiveStream::write_manifest_with_limit(&mut output, metadata(), |_, path, size| {
                assert_eq!(path, "toc.yaml"); assert_eq!(size, 3);
                Ok(futures::io::Cursor::new(b"toc"))
            }, 16 * 1024).await.unwrap();
            let bytes = output.into_inner();
            let size = bytes.len() as u64;
            let parsed = super::super::validate_archive(futures::io::Cursor::new(bytes), size).await.unwrap();
            assert_eq!(parsed.novels[0].files[0].size, Some(3));
        });
    }
}
