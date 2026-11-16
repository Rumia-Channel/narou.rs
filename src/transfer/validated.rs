//! Validated, reusable archive handles over an immutable range source.
//!
//! [`ValidatedArchive`] indexes a ZIP via bounded range reads, verifies every
//! listed file (size + SHA-256), then serves catalog pages, the projected
//! NDJSON wire manifest, and per-entry byte streams with resume-prefix
//! verification — without retaining file bodies or re-reading the central
//! directory. The browser WASM bindings wrap this type; native callers can
//! use it directly.

use std::sync::Arc;

use async_zip::base::read1::ZipFileReader;
use async_zip::base::read1::{ZipOptions, seek::ZipArchiveReader};
use futures::io::{AsyncBufRead, AsyncReadExt, AsyncSeek, AsyncWrite, AsyncWriteExt};
use sha2::{Digest, Sha256};

use super::archive::validate_relative_path;
use super::memory::{MetadataBudget, default_limit};
use super::projection::{ClassifyContext, Send as SendDecision, TransferSelection};
use super::protocol::{IO_CHUNK_BYTES, PAGE_MAX_BYTES, PAGE_MAX_ITEMS};
use super::{
    ArchiveManifest, MANIFEST_PATH, MAX_ENTRIES, MAX_MANIFEST_BYTES, MAX_NOVELS,
    Result, TransferError,
};

/// Per-source limits for indexing/validation.
///
/// Byte-size caps were deliberately removed from streaming paths: entries are
/// bounded by the count budgets, the metadata budget, the manifest budget and
/// the destination's storage reservation — never by a fixed MiB constant.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    /// Maximum ZIP entries (manifest and novel files).
    pub max_entries: usize,
    /// Maximum novels in the manifest.
    pub max_novels: usize,
    /// Maximum serialized size of `manifest.json`.
    pub max_manifest_bytes: u64,
    /// Working-metadata admission budget (`usize::MAX` on native).
    pub metadata_budget: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_novels: MAX_NOVELS,
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            metadata_budget: default_limit(),
        }
    }
}

impl ArchiveLimits {
    /// Parse the optional `limits_json` document used by the WASM binding.
    /// Unknown fields are ignored; limits stay within sane bounds.
    pub fn from_json(json: Option<&str>) -> Result<ArchiveLimits> {
        let mut limits = ArchiveLimits::default();
        let Some(json) = json else { return Ok(limits) };
        if json.len() > super::protocol::CONTROL_BODY_MAX_BYTES as usize {
            return Err(TransferError::TooLarge("limits document exceeds control limit".into()));
        }
        let json = json.trim();
        if json.is_empty() || json == "null" {
            return Ok(limits);
        }
        #[derive(serde::Deserialize, Default)]
        struct Raw {
            max_entries: Option<u64>,
            max_novels: Option<u64>,
            max_archive_manifest_bytes: Option<u64>,
            metadata_budget: Option<u64>,
        }
        let raw: Raw = serde_json::from_str(json)
            .map_err(|error| TransferError::InvalidArchive(format!("invalid limits json: {error}")))?;
        if let Some(value) = raw.max_entries {
            limits.max_entries = usize::try_from(value).unwrap_or(usize::MAX).clamp(1, MAX_ENTRIES);
        }
        if let Some(value) = raw.max_novels {
            limits.max_novels = usize::try_from(value).unwrap_or(usize::MAX).clamp(1, MAX_NOVELS);
        }
        if let Some(value) = raw.max_archive_manifest_bytes {
            limits.max_manifest_bytes = value.clamp(1, MAX_MANIFEST_BYTES);
        }
        if let Some(value) = raw.metadata_budget {
            limits.metadata_budget = usize::try_from(value).unwrap_or(usize::MAX).clamp(1, default_limit());
        }
        Ok(limits)
    }
}

/// One manifest-listed file and its archive position.
#[derive(Debug, Clone)]
pub struct EntryMatch {
    /// Index in `manifest.novels`.
    pub novel: usize,
    /// Index in `manifest.novels[novel].files`.
    pub position: usize,
    /// ZIP entry index.
    pub zip_index: usize,
    /// Uncompressed size declared by the central directory.
    pub declared_size: u64,
}

/// An indexed (and optionally fully validated) archive over an immutable
/// range source.
///
/// The stored factory produces a fresh reader per bounded operation, so
/// validation, catalog paging and entry streaming each use independent local
/// range reads and never hold the whole archive or a decoded file in memory.
pub struct ValidatedArchive<R> {
    inner: Arc<async_zip::base::read1::seek::ZipArchiveInner>,
    factory: Box<dyn Fn() -> Result<R>>,
    manifest: ArchiveManifest,
    /// Manifest-listed files mapped to ZIP indices (manifest order).
    matches: Vec<EntryMatch>,
    /// Shared metadata admission limit, including retained ZIP indices.
    metadata_budget: usize,
    /// Source fingerprint once validated (hex64).
    fingerprint: Option<String>,
    /// Uncompressed bytes total measured during validation.
    total_uncompressed: u64,
}

/// Central-directory indexing shared by `ValidatedArchive::open` and the
/// single-reader `validate_archive` compatibility path.
pub(super) async fn index_archive<R: AsyncBufRead + AsyncSeek + Unpin>(
    reader: R, source_size: u64, limits: &ArchiveLimits,
) -> Result<(ZipArchiveReader<R>, ArchiveManifest, Vec<EntryMatch>)> {
    if limits.max_entries == 0 || limits.max_entries > MAX_ENTRIES
        || limits.max_novels == 0 || limits.max_novels > MAX_NOVELS
        || limits.max_manifest_bytes == 0 || limits.max_manifest_bytes > MAX_MANIFEST_BYTES
        || limits.metadata_budget == 0 || limits.metadata_budget > default_limit() {
        return Err(TransferError::InvalidArchive("invalid archive admission limits".into()));
    }
    // Admit the ZIP offset vector before the library allocates it. The
    // remainder is shared with the incremental manifest decoder below.
    let mut memory = MetadataBudget { used: 0, limit: limits.metadata_budget };
    memory.charge(MetadataBudget::allocation::<u64>(limits.max_entries.next_power_of_two().max(4))?)?;
    // A local+central header, UTF-8 path normalization and extra-field scratch.
    memory.charge(512 * 1024)?;
    let mut archive = ZipArchiveReader::open_with_options(reader, ZipOptions {
        max_cd_num_files: limits.max_entries as u64, max_cd_num_files_load: 0,
        max_cd_size_in_bytes: source_size, max_uncompressed_size_per_file: u64::MAX,
        max_compressed_size_per_file: source_size, ..Default::default()
    }).await?;
    let count = usize::try_from(archive.ceocdr().num_entries()?)
        .map_err(|_| TransferError::TooLarge("entry count overflow".into()))?;
    if count == 0 || count > limits.max_entries {
        return Err(TransferError::TooLarge(format!("{count} zip entries, limit {}", limits.max_entries)));
    }
    let mut seen = Vec::new();
    let mut entries: Vec<([u8; 32], usize, u64)> = Vec::new();
    let mut matches = Vec::new();
    memory.reserve(&mut seen, count)?;
    memory.reserve(&mut entries, count)?;
    memory.reserve(&mut matches, count)?;
    let mut manifest_entry = None;
    for index in 0..count {
        let mut file = archive.file(index).await?;
        let entry = file.cdr().expect("seek reader has central metadata");
        let raw = zip_name(entry.insecure_file_name.as_bytes(), entry.cdrh.gpf.language_encoding_flag())?;
        let directory = raw.ends_with('/');
        let path = validate_relative_path(if directory { &raw[..raw.len() - 1] } else { raw.as_ref() })?;
        if (entry.cdrh.v_made_by >> 8 == 3) && ((entry.cdrh.exter_attr >> 16) & 0o170000 == 0o120000) {
            return Err(TransferError::UnsafePath(format!("symlink {raw:?}")));
        }
        seen.push(path_digest(&path.to_lowercase()));
        let size = entry.uncompressed_size()?;
        if directory {
            if size != 0 { return Err(TransferError::InvalidArchive("nonempty directory entry".into())); }
            let mut byte = [0];
            if file.read(&mut byte).await? != 0 {
                return Err(TransferError::InvalidArchive("nonempty directory entry".into()));
            }
        } else if path == MANIFEST_PATH {
            if size > limits.max_manifest_bytes { return Err(TransferError::TooLarge("manifest too large".into())); }
            manifest_entry = Some(index);
        } else { entries.push((path_digest(&path), index, size)); }
    }
    seen.sort_unstable();
    if seen.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(TransferError::InvalidArchive("duplicate ZIP entry".into()));
    }
    memory.release(&seen);
    drop(seen);
    entries.sort_unstable_by_key(|entry| entry.0);
    let index = manifest_entry.ok_or_else(|| TransferError::InvalidArchive("manifest.json is missing".into()))?;
    let mut manifest = super::manifest_json::read_with_budget(archive.file(index).await?,
        memory.limit.saturating_sub(memory.used)).await?;
    memory.adopt_manifest(&manifest)?;
    super::structure::validate_with_budget(&manifest, &mut memory)?;
    if manifest.novels.len() > limits.max_novels { return Err(TransferError::TooLarge("too many novels".into())); }
    for (novel_index, novel) in manifest.novels.iter_mut().enumerate() {
        super::attribute::normalize(novel)?;
        if novel.virtual_members.is_none() && !novel.files.iter().any(|file| file.path == "toc.yaml") {
            return Err(TransferError::InvalidArchive(format!("novel {novel_index} is missing toc.yaml")));
        }
        let mut paths = Vec::new();
        memory.reserve(&mut paths, novel.files.len())?;
        for (position, file) in novel.files.iter_mut().enumerate() {
            if matches.len() >= limits.max_entries - 1 { return Err(TransferError::TooLarge("too many manifest files".into())); }
            let normalized = validate_relative_path(&file.path)?;
            if normalized != file.path { return Err(TransferError::UnsafePath(file.path.clone())); }
            paths.push(path_digest(&file.path.to_lowercase()));
            if let Some(hash) = &file.sha256 { super::protocol::check_sha256_hex(hash)?; }
            // Reserve measured size/hash storage while the decoded model is
            // still admitted, before validation can fill legacy missing SHA.
            if file.sha256.is_none() { memory.charge(MetadataBudget::allocation::<u8>(64)?)?; }
            let full = format!("novels/{novel_index}/{}", file.path);
            let digest = path_digest(&full);
            let entry = entries.binary_search_by_key(&digest, |entry| entry.0)
                .ok().map(|index| &mut entries[index])
                .filter(|entry| entry.1 != usize::MAX)
                .ok_or_else(|| TransferError::InvalidArchive(format!("missing archive entry {full:?}")))?;
            let (_, zip_index, size) = *entry;
            entry.1 = usize::MAX;
            if file.size.is_some_and(|declared| declared != size) {
                return Err(TransferError::InvalidArchive(format!("size mismatch for {full:?}")));
            }
            matches.push(EntryMatch { novel: novel_index, position, zip_index, declared_size: size });
        }
        paths.sort_unstable();
        if paths.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(TransferError::InvalidArchive("duplicate manifest file".into()));
        }
        // A file cannot also be a directory; use the admitted digest index
        // rather than allocating another set of all paths.
        for file in &novel.files {
            for (slash, _) in file.path.match_indices('/') {
                if paths.binary_search(&path_digest(&file.path[..slash].to_lowercase())).is_ok() {
                    return Err(TransferError::InvalidArchive("file/directory path conflict".into()));
                }
            }
        }
        memory.release(&paths);
    }
    if entries.iter().any(|entry| entry.1 != usize::MAX) {
        return Err(TransferError::InvalidArchive("unlisted archive entry".into()));
    }
    Ok((archive, manifest, matches))
}

impl<R: AsyncBufRead + AsyncSeek + Unpin + 'static> ValidatedArchive<R> {
    /// Index the archive: locate the central directory, decode the manifest,
    /// validate paths/structure and match every entry against the manifest.
    /// No file bytes are read and nothing is verified yet.
    ///
    /// `size` is the source's total byte length; `factory` must return a fresh
    /// reader over the same immutable source on every call.
    pub async fn open<F>(factory: F, size: u64, limits: ArchiveLimits) -> Result<Self>
    where
        F: Fn() -> Result<R> + 'static,
    {
        if size == 0 {
            return Err(TransferError::TooLarge("empty archive".into()));
        }
        let reader = factory()?;
        let (archive, manifest, matches) = index_archive(reader, size, &limits).await?;
        let inner = archive.inner().clone();
        drop(archive);
        Ok(Self {
            inner,
            factory: Box::new(factory),
            manifest,
            matches,
            metadata_budget: limits.metadata_budget,
            fingerprint: None,
            total_uncompressed: 0,
        })
    }

    /// The decoded, normalized manifest (metadata only; `size`/`sha256` are
    /// filled in by [`Self::validate`] when the archive omitted them).
    pub fn manifest(&self) -> &ArchiveManifest {
        &self.manifest
    }

    /// Manifest-listed entries in manifest order (match ordinals).
    pub fn matches(&self) -> &[EntryMatch] {
        &self.matches
    }

    /// Whether every entry passed `validate`.
    pub fn is_validated(&self) -> bool {
        self.fingerprint.is_some()
    }

    /// Source fingerprint (stable over manifest order + verified values).
    /// Only available after `validate`.
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    /// Total uncompressed bytes measured during validation.
    pub fn total_uncompressed(&self) -> u64 {
        self.total_uncompressed
    }

    /// A fresh entry reader over the archive's ZIP index.
    async fn entry_reader(&self, zip_index: usize) -> Result<ZipFileReader<R>> {
        let reader = (self.factory)()?;
        let archive = ZipArchiveReader::new_with_inner(reader, self.inner.clone());
        Ok(archive.file_oneshot(zip_index).await?)
    }

    /// Full validation: read every listed file, enforce actual size == declared
    /// size, compute SHA-256, compare against manifest digests when present,
    /// and fill missing `size`/`sha256` with measured values. Directories and
    /// the manifest were already verified during `open`.
    ///
    /// `progress` is invoked with (completed entries, total) after each entry
    /// verifies; an error cancels validation without authorizing any output.
    /// Repeated successful validation reuses the immutable-source identity.
    pub async fn validate<F>(&mut self, mut progress: F) -> Result<()>
    where
        F: FnMut(usize, usize) -> Result<()>,
    {
        let total = self.matches.len();
        if self.is_validated() { return progress(total, total); }
        let mut uncompressed = 0u64;
        let mut bytes = vec![0u8; IO_CHUNK_BYTES];
        for (ordinal, matched) in self.matches.iter().enumerate() {
            let mut entry = self.entry_reader(matched.zip_index).await?;
            let mut hash = Sha256::new();
            let mut read = 0u64;
            loop {
                let count = entry.read(&mut bytes).await?;
                if count == 0 {
                    break;
                }
                read = read.checked_add(count as u64).filter(|read| *read <= matched.declared_size)
                    .ok_or_else(|| TransferError::InvalidArchive("entry exceeds declared size".into()))?;
                hash.update(&bytes[..count]);
            }
            if read != matched.declared_size {
                let path = &self.manifest.novels[matched.novel].files[matched.position].path;
                return Err(TransferError::InvalidArchive(format!(
                    "size mismatch for {path:?}"
                )));
            }
            let sha256 = hex::encode(hash.finalize());
            let expected = &mut self.manifest.novels[matched.novel].files[matched.position];
            if let Some(expected_hash) = &expected.sha256 {
                if !sha256.eq_ignore_ascii_case(expected_hash) {
                    return Err(TransferError::HashMismatch {
                        path: expected.path.clone(),
                        expected: expected_hash.clone(),
                        actual: sha256,
                    });
                }
            } else {
                expected.sha256 = Some(sha256.clone());
            }
            expected.size = Some(read);
            uncompressed = uncompressed
                .checked_add(read)
                .ok_or_else(|| TransferError::TooLarge("uncompressed size overflow".into()))?;
            progress(ordinal + 1, total)?;
        }
        self.total_uncompressed = uncompressed;
        self.fingerprint = Some(source_fingerprint(&self.manifest)?);
        Ok(())
    }

    fn require_validated(&self) -> Result<()> {
        if !self.is_validated() {
            return Err(TransferError::InvalidArchive("archive is not validated".into()));
        }
        Ok(())
    }

    /// One page of the validated catalog (max [`PAGE_MAX_ITEMS`] items and
    /// [`PAGE_MAX_BYTES`] serialized).
    ///
    /// Items alternate novel headers and their files in manifest order.
    /// `entry` on a file item is the match ordinal within this handle (stable
    /// across pages); it is not the wire `file_id`. `cursor` is the opaque
    /// `next_cursor` from a previous page (`None`/`""` for the first page).
    /// Returns (items, next_cursor) with `next_cursor == None` at the end.
    pub fn catalog_page(
        &self,
        cursor: Option<&str>,
        ctx: &ClassifyContext,
    ) -> Result<(Vec<CatalogItem>, Option<u64>)> {
        self.require_validated()?;
        let start = match cursor {
            None | Some("") => 0usize,
            Some(text) => text
                .parse::<usize>()
                .map_err(|_| TransferError::InvalidSelection(format!("invalid cursor {text:?}")))?,
        };
        let mut items = Vec::new();
        // Include the JSON wrapper, commas and next cursor, not just items.
        let mut used = 96usize;
        let mut emitted = 0usize;
        let mut next: Option<u64> = None;
        let mut flat_index = 0usize;
        let mut match_ordinal = 0usize;
        'outer: for (novel_index, novel) in self.manifest.novels.iter().enumerate() {
            for is_novel in [true].into_iter().chain(std::iter::repeat(false)) {
                if !is_novel {
                    break;
                }
                if flat_index >= start {
                    if emitted >= PAGE_MAX_ITEMS {
                        next = Some(flat_index as u64);
                        break 'outer;
                    }
                    let virtual_parent = novel.virtual_members.is_some();
                    let (toc_url, title, truncated) =
                        display_pair(&novel.toc_url, &novel.title);
                    let item = CatalogItem::Novel {
                        index: novel_index,
                        toc_url,
                        title,
                        virtual_parent,
                        files: novel.files.len(),
                        truncated,
                    };
                    let estimate = serde_json::to_vec(&item)?.len() + 1;
                    if emitted > 0 && used + estimate > PAGE_MAX_BYTES {
                        next = Some(flat_index as u64);
                        break 'outer;
                    }
                    used += estimate;
                    emitted += 1;
                    items.push(item);
                }
                flat_index += 1;
            }
            for file in &novel.files {
                if flat_index >= start {
                    if emitted >= PAGE_MAX_ITEMS {
                        next = Some(flat_index as u64);
                        break 'outer;
                    }
                    let matched = &self.matches[match_ordinal];
                    let role = super::projection::classify(
                        &file.path,
                        novel.virtual_members.is_some(),
                        ctx,
                    );
                    let item = CatalogItem::File {
                        entry: match_ordinal,
                        novel: novel_index,
                        path: file.path.clone(),
                        size: file.size.unwrap_or(matched.declared_size),
                        sha256: file.sha256.clone().unwrap_or_default(),
                        role,
                        runtime: send_json(super::projection::send_decision(
                            super::projection::TransferProfile::Runtime,
                            role,
                            &[],
                        )),
                        complete: send_json(super::projection::send_decision(
                            super::projection::TransferProfile::Complete,
                            role,
                            &[],
                        )),
                    };
                    let estimate = serde_json::to_vec(&item)?.len() + 1;
                    if emitted > 0 && used + estimate > PAGE_MAX_BYTES {
                        next = Some(flat_index as u64);
                        break 'outer;
                    }
                    used += estimate;
                    emitted += 1;
                    items.push(item);
                }
                flat_index += 1;
                match_ordinal += 1;
            }
        }
        Ok((items, next))
    }

    /// Match ordinal for a (novel, file position) pair.
    fn match_ordinal(&self, novel: usize, position: usize) -> Option<usize> {
        self.matches.binary_search_by_key(&(novel, position), |matched| (matched.novel, matched.position)).ok()
    }

    /// Stream one entry's uncompressed bytes to `sink` in ≤64 KiB pieces.
    ///
    /// `file_id` is the wire ordinal assigned by the current projection
    /// (`wire_files[file_id]`); `confirmed_offset` resumes a previous upload:
    /// the stream first re-reads and hashes `confirmed_offset` bytes, verifies
    /// them against `expected_prefix_sha256`, then emits the remainder. A
    /// nonzero offset without the expected prefix digest is an error.
    ///
    /// Returns the whole-file size/digest plus the bytes emitted this call.
    pub async fn stream_entry<W: AsyncWrite + Unpin>(
        &self,
        file_id: u32,
        wire_files: &[(usize, usize)],
        confirmed_offset: u64,
        expected_prefix_sha256: Option<&str>,
        sink: &mut W,
    ) -> Result<StreamedEntry> {
        self.require_validated()?;
        let (novel, position) = *wire_files
            .get(file_id as usize)
            .ok_or_else(|| TransferError::InvalidSelection(format!("unknown file_id {file_id}")))?;
        let match_ordinal = self
            .match_ordinal(novel, position)
            .ok_or_else(|| TransferError::InvalidArchive("entry match missing".into()))?;
        let matched = &self.matches[match_ordinal];
        let expected = &self.manifest.novels[novel].files[position];
        let expected_sha256 = expected.sha256.as_deref().expect("validated file digest");
        let file_size = expected.size.expect("validated file size");
        if confirmed_offset > file_size {
            return Err(TransferError::Protocol("confirmed offset exceeds file size".into()));
        }
        if confirmed_offset != 0 && expected_prefix_sha256.is_none() {
            return Err(TransferError::Protocol("confirmed offset requires prefix sha256".into()));
        }
        if let Some(prefix) = expected_prefix_sha256 { super::protocol::check_sha256_hex(prefix)?; }
        let mut entry = self.entry_reader(matched.zip_index).await?;
        let mut bytes = vec![0u8; IO_CHUNK_BYTES];
        let mut whole = Sha256::new();
        let mut remaining = confirmed_offset;
        while remaining > 0 {
            let want = remaining.min(bytes.len() as u64) as usize;
            let count = entry.read(&mut bytes[..want]).await?;
            if count == 0 { return Err(TransferError::InvalidArchive("entry ended before confirmed offset".into())); }
            whole.update(&bytes[..count]);
            remaining -= count as u64;
        }
        if let Some(prefix) = expected_prefix_sha256 {
            let actual = hex::encode(whole.clone().finalize());
            if !actual.eq_ignore_ascii_case(prefix) {
                return Err(TransferError::HashMismatch { path: expected.path.clone(), expected: prefix.to_owned(), actual });
            }
        }
        let mut emitted = 0u64;
        loop {
            let count = entry.read(&mut bytes).await?;
            if count == 0 { break; }
            emitted = emitted.checked_add(count as u64).filter(|emitted| *emitted <= file_size - confirmed_offset)
                .ok_or_else(|| TransferError::InvalidArchive("entry exceeds validated size".into()))?;
            whole.update(&bytes[..count]);
            sink.write_all(&bytes[..count]).await?;
        }
        if emitted != file_size - confirmed_offset {
            return Err(TransferError::InvalidArchive("entry differs from validated size".into()));
        }
        let sha256 = hex::encode(whole.finalize());
        if !sha256.eq_ignore_ascii_case(expected_sha256) {
            return Err(TransferError::HashMismatch { path: expected.path.clone(), expected: expected_sha256.to_owned(), actual: sha256 });
        }
        sink.flush().await?;
        Ok(StreamedEntry { file_size, emitted, sha256 })
    }

    /// Project the validated manifest into wire order (`wire file_id` order).
    ///
    /// Returns the shared [`super::projection::TransferProjection`]; the selection is
    /// validated (unknown roles, required exclusions, out-of-range indexes all
    /// fail) and `wire_files` is the `stream_entry` input.
    pub fn project(&self, selection: &TransferSelection) -> Result<super::projection::TransferProjection> {
        self.require_validated()?;
        let mut memory = MetadataBudget { used: 0, limit: self.metadata_budget };
        memory.adopt_manifest(&self.manifest)?;
        memory.charge(MetadataBudget::allocation::<EntryMatch>(self.matches.capacity())?)?;
        memory.charge(MetadataBudget::allocation::<u64>((self.inner.ceocdr().num_entries()? as usize).next_power_of_two().max(4))?)?;
        memory.charge(512 * 1024)?;
        memory.charge(MetadataBudget::allocation::<super::projection::ProjectedNovel>(self.manifest.novels.len() * 2)?)?;
        memory.charge(MetadataBudget::allocation::<super::projection::ProjectedFile>(self.matches.len())?)?;
        memory.charge(MetadataBudget::allocation::<(usize, usize)>(self.matches.len() * 2)?)?;
        memory.charge(MetadataBudget::allocation::<usize>(self.manifest.novels.len() * 8)?)?;
        for novel in &self.manifest.novels {
            for file in &novel.files {
                memory.charge(MetadataBudget::allocation::<u8>(file.path.len())?)?;
                memory.charge(MetadataBudget::allocation::<u8>(64)?)?;
            }
        }
        let ctx = selection.context();
        super::projection::project(
            &self.manifest,
            selection.profile,
            &selection.extra_roles,
            selection.novels.as_deref(),
            &selection.excluded_files,
            &ctx,
        )
    }

    /// Emit the normalized NDJSON wire manifest for `projection`.
    ///
    /// Writes ≤64 KiB chunks to `sink`, computes the exact byte count and
    /// SHA-256 of the emitted document (for the manifest seal), and returns
    /// the summary. Requires validation.
    pub async fn write_wire_manifest<W: AsyncWrite + Unpin>(
        &self,
        selection: &TransferSelection,
        projection: &super::projection::TransferProjection,
        sink: &mut W,
    ) -> Result<WireSummary> {
        self.require_validated()?;
        use super::protocol::{
            NovelKind, WireEnd, WireFile, WireHeader, WireNovel, WireRecord,
        };
        let fingerprint = self.fingerprint().expect("validated").to_string();
        let selection_digest = projection.digest(selection.policy_version, &fingerprint)?;
        let mut novel_records = 0u32;
        let mut text_records = 0u32;
        let mut file_records = 0u32;
        for projected in &projection.novels {
            let manifest_novel = &self.manifest.novels[projected.index];
            let sent = projected.files.iter().filter(|file| file.send.is_send()).count() as u32;
            novel_records += 1;
            file_records += sent;
            text_records += count_novel_texts(manifest_novel);
        }
        let header = WireHeader {
            format_version: super::FORMAT_VERSION,
            protocol_version: super::protocol::PROTOCOL_VERSION,
            policy_version: selection.policy_version,
            profile: projection.profile,
            extra_roles: projection.extra_roles.iter().map(|role| role.as_str().to_string()).collect(),
            fingerprint: fingerprint.clone(),
            selection_digest: selection_digest.clone(),
            novels: novel_records,
            files: file_records,
            texts: text_records,
        };
        let mut out = ManifestSink { sink, hash: Sha256::new(), bytes: 0, buffer: Vec::new() };
        out.record(&WireRecord::Header(header)).await?;
        let mut file_id = 0u32;
        for (wire_novel, projected) in projection.novels.iter().enumerate() {
            let manifest_novel = &self.manifest.novels[projected.index];
            let sent = projected.files.iter().filter(|file| file.send.is_send()).count() as u32;
            out.record(&WireRecord::Novel(WireNovel {
                index: wire_novel as u32,
                kind: if projected.virtual_parent {
                    NovelKind::VirtualParent
                } else {
                    NovelKind::Normal
                },
                novel_type: manifest_novel.novel_type,
                files: sent,
            }))
            .await?;
            write_novel_texts(&mut out, wire_novel as u32, manifest_novel).await?;
            for file in &projected.files {
                if !file.send.is_send() {
                    continue;
                }
                out.record(&WireRecord::File(WireFile {
                    file_id,
                    novel: wire_novel as u32,
                    path: file.path.clone(),
                    size: file.size.ok_or_else(|| {
                        TransferError::InvalidArchive(format!("missing size for {:?}", file.path))
                    })?,
                    sha256: file.sha256.clone().ok_or_else(|| {
                        TransferError::InvalidArchive(format!(
                            "missing sha256 for {:?}",
                            file.path
                        ))
                    })?,
                }))
                .await?;
                file_id += 1;
            }
        }
        out.record(&WireRecord::End(WireEnd {
            novels: novel_records,
            files: file_records,
            texts: text_records,
        }))
        .await?;
        out.sink.flush().await?;
        Ok(WireSummary {
            wire_size: out.bytes,
            wire_sha256: hex::encode(out.hash.finalize()),
            fingerprint,
            selection_digest,
            novels: novel_records,
            files: file_records,
            texts: text_records,
        })
    }
}

/// Emitted wire-manifest summary (what the manifest seal verifies).
#[derive(Debug, Clone, serde::Serialize)]
pub struct WireSummary {
    /// Exact NDJSON byte length.
    #[serde(with = "super::protocol::decimal_u64")]
    pub wire_size: u64,
    /// SHA-256 of the emitted document (hex64).
    pub wire_sha256: String,
    /// Validated source fingerprint.
    pub fingerprint: String,
    /// Digest binding the profile/policy and selected file set.
    pub selection_digest: String,
    /// Record counts.
    pub novels: u32,
    pub files: u32,
    pub texts: u32,
}

/// Result of `stream_entry`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamedEntry {
    /// Whole uncompressed entry size.
    #[serde(with = "super::protocol::decimal_u64")]
    pub file_size: u64,
    /// Bytes emitted this call (after the confirmed prefix).
    #[serde(with = "super::protocol::decimal_u64")]
    pub emitted: u64,
    /// Whole-file SHA-256 (verified digest).
    pub sha256: String,
}

/// Wire-shaped send decision inside catalog file items.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SendJson {
    /// `"required"`, `"optional"` or `"excluded"`.
    pub send: &'static str,
    /// Default stance for `optional` (whether it is selected by default).
    pub default: bool,
    /// Stable reason code for `excluded` (`""` otherwise).
    pub reason: &'static str,
}

fn send_json(send: SendDecision) -> SendJson {
    match send {
        SendDecision::Required => SendJson { send: "required", default: true, reason: "" },
        SendDecision::Optional { default } => SendJson { send: "optional", default, reason: "" },
        SendDecision::Excluded { reason } => SendJson { send: "excluded", default: false, reason },
    }
}

/// One catalog page item.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogItem {
    /// A novel header.
    Novel {
        /// Manifest index.
        index: usize,
        /// Truncated TOC URL.
        toc_url: String,
        /// Truncated title.
        title: String,
        /// Virtual parent flag.
        virtual_parent: bool,
        /// Total files in this novel.
        files: usize,
        /// Whether display strings were truncated for the page.
        truncated: bool,
    },
    /// A file entry (`entry` = match ordinal, not the wire `file_id`).
    File {
        /// Match ordinal within the handle.
        entry: usize,
        /// Owning novel index.
        novel: usize,
        /// Path relative to the novel directory.
        path: String,
        /// Verified uncompressed size (decimal string).
        #[serde(with = "super::protocol::decimal_u64")]
        size: u64,
        /// Verified SHA-256.
        sha256: String,
        /// Classified role.
        role: super::projection::FileRole,
        /// Send decision under the runtime profile.
        runtime: SendJson,
        /// Send decision under the complete profile.
        complete: SendJson,
    },
}

struct ManifestSink<'a, W: AsyncWrite + Unpin> {
    sink: &'a mut W,
    hash: Sha256,
    bytes: u64,
    buffer: Vec<u8>,
}

impl<'a, W: AsyncWrite + Unpin> ManifestSink<'a, W> {
    async fn record<S: serde::Serialize>(&mut self, record: &S) -> Result<()> {
        self.buffer.clear();
        serde_json::to_writer(&mut self.buffer, record)?;
        self.buffer.push(b'\n');
        let bytes = &self.buffer;
        if bytes.len() > super::protocol::WIRE_RECORD_MAX_BYTES {
            return Err(TransferError::TooLarge("wire record exceeds limit".into()));
        }
        self.hash.update(bytes);
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| TransferError::TooLarge("wire manifest overflow".into()))?;
        if self.bytes > super::protocol::WIRE_MANIFEST_MAX_BYTES {
            return Err(TransferError::TooLarge("wire manifest exceeds budget".into()));
        }
        // Emit in bounded chunks; the sink decides its own pacing.
        for chunk in bytes.chunks(IO_CHUNK_BYTES) {
            self.sink.write_all(chunk).await?;
        }
        Ok(())
    }
}

fn count_novel_texts(novel: &super::ArchiveNovel) -> u32 {
    let mut count = 0u32;
    for value in [
        novel.toc_url.as_str(),
        novel.title.as_str(),
        novel.author.as_str(),
        novel.sitename.as_str(),
    ] {
        count += super::protocol::text_fragments(value).count() as u32;
    }
    for value in [
        novel.domain.as_deref(),
        novel.attribute_tag.as_deref(),
        novel.ncode.as_deref(),
        novel.subtitle.as_deref(),
        novel.general_lastup.as_deref(),
        novel.last_update.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        count += super::protocol::text_fragments(value).count() as u32;
    }
    for tag in &novel.tags {
        count += super::protocol::text_fragments(tag).count() as u32;
    }
    if let Some(members) = &novel.virtual_members {
        for member in members {
            count += super::protocol::text_fragments(member).count() as u32;
        }
    }
    count
}

async fn write_novel_texts<W: AsyncWrite + Unpin>(
    out: &mut ManifestSink<'_, W>,
    index: u32,
    novel: &super::ArchiveNovel,
) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Text<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        novel: u32,
        field: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        item: Option<u32>,
        part: u32,
        last: bool,
        value: &'a str,
    }
    async fn field<W: AsyncWrite + Unpin>(
        out: &mut ManifestSink<'_, W>,
        novel: u32,
        field: &str,
        item: Option<u32>,
        value: &str,
    ) -> Result<()> {
        let mut parts = super::protocol::text_fragments(value).enumerate().peekable();
        while let Some((part, value)) = parts.next() {
            let last = parts.peek().is_none();
            out.record(&Text { kind: "text", novel, field, item, part: part as u32, last, value })
            .await?;
        }
        Ok(())
    }
    field(out, index, "toc_url", None, &novel.toc_url).await?;
    field(out, index, "title", None, &novel.title).await?;
    field(out, index, "author", None, &novel.author).await?;
    field(out, index, "sitename", None, &novel.sitename).await?;
    if let Some(value) = &novel.domain {
        field(out, index, "domain", None, value).await?;
    }
    if let Some(value) = &novel.attribute_tag {
        field(out, index, "attribute_tag", None, value).await?;
    }
    if let Some(value) = &novel.ncode {
        field(out, index, "ncode", None, value).await?;
    }
    if let Some(value) = &novel.subtitle {
        field(out, index, "subtitle", None, value).await?;
    }
    if let Some(value) = &novel.general_lastup {
        field(out, index, "general_lastup", None, value).await?;
    }
    if let Some(value) = &novel.last_update {
        field(out, index, "last_update", None, value).await?;
    }
    for (item, tag) in novel.tags.iter().enumerate() {
        field(out, index, "tag", Some(item as u32), tag).await?;
    }
    if let Some(members) = &novel.virtual_members {
        for (item, member) in members.iter().enumerate() {
            field(out, index, "member", Some(item as u32), member).await?;
        }
    }
    Ok(())
}

/// Truncate display strings for a catalog page (storage keeps full values).
fn display_pair(toc_url: &str, title: &str) -> (String, String, bool) {
    const DISPLAY_MAX: usize = 192;
    fn cut(value: &str) -> (String, bool) {
        if value.len() <= DISPLAY_MAX {
            return (value.to_string(), false);
        }
        let mut end = DISPLAY_MAX;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        (value[..end].to_string(), true)
    }
    let (toc_url, a) = cut(toc_url);
    let (title, b) = cut(title);
    (toc_url, title, a || b)
}

/// Validate a seekable immutable source without retaining decoded file bodies.
/// Browser callers provide bounded Blob.slice/range readers, never ZIP bytes.
///
/// Single-reader validation for CLI/local callers that only need metadata.
/// Browser callers retain a [`ValidatedArchive`] to reuse its index and entry mapping.
pub async fn validate_archive<R: AsyncBufRead + AsyncSeek + Unpin>(
    reader: R,
    size: u64,
) -> Result<ArchiveManifest> {
    if size == 0 {
        return Err(TransferError::TooLarge("empty archive".into()));
    }
    let limits = ArchiveLimits::default();
    let (mut archive, mut manifest, matches) = index_archive(reader, size, &limits).await?;
    let mut bytes = vec![0u8; IO_CHUNK_BYTES];
    for matched in matches {
        let mut entry = archive.file(matched.zip_index).await?;
        let mut hash = Sha256::new();
        let mut read = 0u64;
        loop {
            let count = entry.read(&mut bytes).await?;
            if count == 0 {
                break;
            }
            read = read.checked_add(count as u64).filter(|read| *read <= matched.declared_size)
                .ok_or_else(|| TransferError::InvalidArchive("entry exceeds declared size".into()))?;
            hash.update(&bytes[..count]);
        }
        let expected = &mut manifest.novels[matched.novel].files[matched.position];
        if read != matched.declared_size {
            return Err(TransferError::InvalidArchive(format!(
                "size mismatch for {:?}",
                expected.path
            )));
        }
        let actual = hex::encode(hash.finalize());
        if let Some(expected_hash) = &expected.sha256 {
            if !actual.eq_ignore_ascii_case(expected_hash) {
                return Err(TransferError::HashMismatch {
                    path: expected.path.clone(),
                    expected: expected_hash.clone(),
                    actual,
                });
            }
        } else {
            expected.sha256 = Some(actual);
        }
        expected.size = Some(read);
    }
    Ok(manifest)
}

/// Compute the resume fingerprint over all normalized metadata, paths and
/// measured file sizes/digests, including metadata-only virtual parents.
pub fn source_fingerprint(manifest: &ArchiveManifest) -> Result<String> {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut writer = HashWriter(Sha256::new());
    writer.0.update(b"narou-transfer-fingerprint-v2\0");
    // Canonical struct field order, complete normalized metadata, file paths
    // and measured size/digests. No JSON buffer or second manifest is built.
    serde_json::to_writer(&mut writer, manifest)?;
    Ok(hex::encode(writer.0.finalize()))
}

fn zip_name(bytes: &[u8], utf8: bool) -> Result<std::borrow::Cow<'_, str>> {
    if utf8 || bytes.is_ascii() {
        return std::str::from_utf8(bytes)
            .map(std::borrow::Cow::Borrowed)
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
    Ok(std::borrow::Cow::Owned(
        bytes
            .iter()
            .map(|byte| if *byte < 128 { *byte as char } else { CP437[(*byte - 128) as usize] })
            .collect(),
    ))
}

fn path_digest(path: &str) -> [u8; 32] {
    Sha256::digest(path.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{ArchiveFile, ArchiveNovel, build_archive};
    use super::super::projection::{ProjectionDigest, TransferProfile};
    use super::super::protocol::{WireManifestDecoder, WireRecord};
    use futures::io::Cursor;

    fn manifest() -> ArchiveManifest {
        ArchiveManifest { novels: ["one", "two"].into_iter().map(|name| ArchiveNovel {
            toc_url: format!("https://example.test/{name}/"), title: name.into(),
            files: vec![ArchiveFile::new("toc.yaml", b"current table of contents".to_vec()),
                ArchiveFile::new("raw/1.html", b"raw source".to_vec()),
                ArchiveFile::new("empty.dat", Vec::new())],
            ..Default::default()
        }).collect(), ..Default::default() }
    }
    async fn open(manifest: &ArchiveManifest) -> ValidatedArchive<Cursor<Arc<[u8]>>> {
        let bytes: Arc<[u8]> = build_archive(manifest).unwrap().into();
        let size = bytes.len() as u64;
        ValidatedArchive::open(move || Ok(Cursor::new(bytes.clone())), size, ArchiveLimits::default()).await.unwrap()
    }

    #[test]
    fn unvalidated_handles_cannot_emit_and_validation_reuses_identity() {
        futures::executor::block_on(async {
            let mut archive = open(&manifest()).await;
            assert!(archive.catalog_page(None, &Default::default()).is_err());
            assert!(archive.project(&TransferSelection::default()).is_err());
            let mut completed = 0;
            archive.validate(|done, _| { completed = done; Ok(()) }).await.unwrap();
            assert_eq!(completed, archive.matches.len());
            let identity = archive.fingerprint().unwrap().to_owned();
            archive.validate(|done, total| { assert_eq!(done, total); Ok(()) }).await.unwrap();
            assert_eq!(archive.fingerprint(), Some(identity.as_str()));
            assert!(archive.manifest.novels.iter().flat_map(|novel| &novel.files).all(|file| file.size.is_some() && file.sha256.is_some() && file.bytes.is_empty()));
        });
    }

    #[test]
    fn projected_wire_indices_are_dense_and_digest_matches_independent_consumer() {
        futures::executor::block_on(async {
            let mut input = manifest(); input.novels[1].title = "\0".repeat(70_000);
            let mut archive = open(&input).await;
            archive.validate(|_, _| Ok(())).await.unwrap();
            let selection = TransferSelection { novels: Some(vec![1]), ..Default::default() };
            let projection = archive.project(&selection).unwrap();
            assert_eq!(projection.wire_files.len(), 2);
            let mut bytes = Cursor::new(Vec::new());
            let summary = archive.write_wire_manifest(&selection, &projection, &mut bytes).await.unwrap();
            let bytes = bytes.into_inner();
            assert_eq!(summary.wire_size, bytes.len() as u64);
            assert_eq!(summary.wire_sha256, hex::encode(Sha256::digest(&bytes)));
            let mut decoder = WireManifestDecoder::new();
            let mut digest = None;
            let mut header_digest = String::new();
            for record in decoder.feed(&bytes).unwrap() {
                match record {
                    WireRecord::Header(header) => {
                        header_digest = header.selection_digest.clone();
                        digest = Some(ProjectionDigest::new(header.policy_version, header.profile, &header.extra_roles, &header.fingerprint).unwrap());
                    }
                    WireRecord::Novel(novel) => { assert_eq!(novel.index, 0); digest.as_mut().unwrap().novel(novel.index); }
                    WireRecord::Text(text) => { assert_eq!(text.novel, 0); assert!(text.value.len() <= 32 * 1024); }
                    WireRecord::File(file) => { assert_eq!(file.novel, 0); assert!(!file.path.starts_with("raw/")); digest.as_mut().unwrap().file(&file.path, file.size, &file.sha256).unwrap(); }
                    WireRecord::End(_) => {}
                }
            }
            decoder.finish().unwrap();
            assert_eq!(digest.unwrap().finish(), header_digest);
            assert_eq!(summary.selection_digest, header_digest);
        });
    }

    #[test]
    fn resume_verifies_prefix_before_output_and_rehashes_complete_entry() {
        futures::executor::block_on(async {
            let mut archive = open(&manifest()).await;
            archive.validate(|_, _| Ok(())).await.unwrap();
            let projection = archive.project(&TransferSelection::default()).unwrap();
            let (id, &(novel, file)) = projection.wire_files.iter().enumerate().find(|(_, pair)| { let (novel, file) = **pair; archive.manifest.novels[novel].files[file].path == "toc.yaml" }).unwrap();
            let body = b"current table of contents";
            let prefix = hex::encode(Sha256::digest(&body[..7]));
            let mut sink = Cursor::new(Vec::new());
            assert!(archive.stream_entry(id as u32, &projection.wire_files, 7, None, &mut sink).await.is_err());
            assert!(archive.stream_entry(id as u32, &projection.wire_files, 7, Some(&"0".repeat(64)), &mut sink).await.is_err());
            assert!(sink.get_ref().is_empty());
            let result = archive.stream_entry(id as u32, &projection.wire_files, 7, Some(&prefix), &mut sink).await.unwrap();
            assert_eq!(sink.into_inner(), &body[7..]);
            assert_eq!(result.sha256, archive.manifest.novels[novel].files[file].sha256.as_deref().unwrap());
            assert_eq!(result.emitted + 7, result.file_size);
            let empty = projection.wire_files.iter().position(|&(novel, file)| archive.manifest.novels[novel].files[file].path == "empty.dat").unwrap();
            let result = archive.stream_entry(empty as u32, &projection.wire_files, 0, None, &mut Cursor::new(Vec::new())).await.unwrap();
            assert_eq!(result.emitted, 0);
            assert_eq!(result.sha256, hex::encode(Sha256::digest([])));
        });
    }

    #[test]
    fn fingerprint_covers_metadata_paths_empty_novels_and_file_hashes() {
        let original = manifest();
        let expected = source_fingerprint(&original).unwrap();
        let mut changed = original.clone(); changed.novels[0].title.push('!');
        assert_ne!(expected, source_fingerprint(&changed).unwrap());
        let mut changed = original.clone(); changed.novels[0].files[0].path.push('!');
        assert_ne!(expected, source_fingerprint(&changed).unwrap());
        let mut changed = original.clone(); changed.novels[0].files[0].sha256 = Some("a".repeat(64));
        assert_ne!(expected, source_fingerprint(&changed).unwrap());
    }

    #[test]
    fn low_metadata_budget_rejects_before_source_reads() {
        futures::executor::block_on(async {
            let limits = ArchiveLimits { metadata_budget: 1024, ..Default::default() };
            let result = ValidatedArchive::open(|| Ok(Cursor::new(&b"not read"[..])), 8, limits).await;
            assert!(matches!(result, Err(TransferError::TooLarge(_))));
            assert!(ArchiveLimits::from_json(Some(r#"{"max_archive_manifest_bytes":9}"#)).unwrap().max_manifest_bytes == 9);
            let _ = TransferProfile::Runtime;
        });
    }
}

