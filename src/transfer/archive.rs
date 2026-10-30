use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{
    FORMAT_VERSION, MANIFEST_PATH, MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_FILE_BYTES,
    MAX_MANIFEST_BYTES, MAX_NOVELS, MAX_TOTAL_UNCOMPRESSED_BYTES, Result, TransferError,
};

/// A complete transfer manifest.
///
/// `manifest.json` is written from this structure. [`ArchiveFile::bytes`] is a
/// memory-only field and is never serialised; the file payload lives in the ZIP
/// entry instead.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchiveManifest {
    /// Archive format version (currently [`FORMAT_VERSION`]).
    pub format_version: u32,
    /// Version of the application that produced the archive, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    /// Caller-provided export timestamp (RFC 3339 or similar), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<String>,
    /// Selected novels, in manifest order. Entry paths use this index.
    #[serde(default)]
    pub novels: Vec<ArchiveNovel>,
}

impl Default for ArchiveManifest {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            app_version: None,
            exported_at: None,
            novels: Vec::new(),
        }
    }
}

/// Metadata and files for one novel inside an archive.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveNovel {
    /// Canonical table-of-contents URL. This is the import conflict key.
    pub toc_url: String,
    pub title: String,
    pub author: String,
    pub sitename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Work-type classification (`小説` / `漫画`).
    ///
    /// The classification is metadata, not an ordinary tag, so it is kept out
    /// of `tags`. Archives written before this field existed carried it in
    /// `tags`; those are migrated on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribute_tag: Option<String>,
    pub novel_type: u8,
    /// Files belonging to this novel, sorted by path when built.
    #[serde(default)]
    pub files: Vec<ArchiveFile>,
    /// Ncode / work identifier, when the site provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ncode: Option<String>,
    /// Subtitle / catchphrase from the novel metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Last update time as reported by the site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub general_lastup: Option<String>,
    /// Last local update time, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<String>,
}

/// One file belonging to a novel.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveFile {
    /// Path relative to the novel directory, using `/` separators.
    pub path: String,
    /// File contents. Never serialised into `manifest.json`.
    #[serde(skip)]
    pub bytes: Vec<u8>,
    /// SHA-256 of `bytes`, filled in by [`build_archive`] and verified on parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Uncompressed size of `bytes`, filled in by [`build_archive`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl ArchiveFile {
    /// Creates a manifest file entry from a relative path and its bytes.
    pub fn new(path: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            bytes,
            sha256: None,
            size: None,
        }
    }
}

/// A validated archive returned by [`parse_archive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArchive {
    /// Manifest as stored in the archive, normalised: the work-type
    /// classification lives in [`ArchiveNovel::attribute_tag`] and never in
    /// [`ArchiveNovel::tags`] (see the `attribute` module). File entries carry
    /// digests and sizes, but [`ArchiveFile::bytes`] is empty; the payloads are
    /// in `novels`.
    pub manifest: ArchiveManifest,
    /// Decoded novels in manifest order.
    pub novels: Vec<ParsedNovel>,
}

/// One decoded novel from a parsed archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNovel {
    /// Position in [`ParsedArchive::manifest`] and in the ZIP paths.
    pub index: usize,
    /// Conflict key copied from the manifest.
    pub toc_url: String,
    /// Title copied from the manifest.
    pub title: String,
    /// Files as `(path relative to the novel directory, contents)`.
    pub files: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) max_archive_bytes: u64,
    pub(crate) max_total_uncompressed: u64,
    pub(crate) max_file_bytes: u64,
    pub(crate) max_entries: usize,
    pub(crate) max_novels: usize,
    pub(crate) max_manifest_bytes: u64,
}

pub(crate) const DEFAULT_LIMITS: Limits = Limits {
    max_archive_bytes: MAX_ARCHIVE_BYTES,
    max_total_uncompressed: MAX_TOTAL_UNCOMPRESSED_BYTES,
    max_file_bytes: MAX_FILE_BYTES,
    max_entries: MAX_ENTRIES,
    max_novels: MAX_NOVELS,
    max_manifest_bytes: MAX_MANIFEST_BYTES,
};

/// Maximum total byte length of a relative archive path.
const MAX_PATH_BYTES: usize = 4096;

/// Maximum number of characters in one path component.
const MAX_PATH_COMPONENT_CHARS: usize = 255;

/// Builds a deterministic ZIP archive from `manifest`.
///
/// Novels are sorted by `(toc_url, title)` and files by `path`; timestamps are
/// fixed at the ZIP epoch. File sizes and SHA-256 digests are computed and
/// written into `manifest.json`, so callers do not have to fill them in.
///
/// Paths are validated before anything is written. The archive is rejected if
/// it exceeds [`MAX_ENTRIES`], [`MAX_NOVELS`], [`MAX_FILE_BYTES`] or
/// [`MAX_TOTAL_UNCOMPRESSED_BYTES`].
pub fn build_archive(manifest: &ArchiveManifest) -> Result<Vec<u8>> {
    build_archive_with_limits(manifest, DEFAULT_LIMITS)
}

pub(crate) fn build_archive_with_limits(
    manifest: &ArchiveManifest,
    limits: Limits,
) -> Result<Vec<u8>> {
    if manifest.format_version != FORMAT_VERSION {
        return Err(TransferError::UnsupportedVersion(manifest.format_version));
    }
    if manifest.novels.len() > limits.max_novels {
        return Err(TransferError::TooLarge(format!(
            "{} novels, limit {}",
            manifest.novels.len(),
            limits.max_novels
        )));
    }

    let mut normalized = manifest.clone();
    // Canonical ordering makes the output independent of the caller's
    // collection order. `sort_by` is stable, so fully equal novels keep their
    // relative order.
    normalized.novels.sort_by(|a, b| {
        a.toc_url
            .cmp(&b.toc_url)
            .then_with(|| a.title.cmp(&b.title))
    });

    let mut file_count = 1usize; // manifest.json
    let mut total_uncompressed = 0u64;

    for (novel_index, novel) in normalized.novels.iter_mut().enumerate() {
        // The classification is metadata, not a tag: keep it out of `tags`
        // and migrate legacy archives that embedded it there.
        super::attribute::normalize(novel)?;
        novel.files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut seen_paths = HashSet::new();
        for file in &mut novel.files {
            let path = validate_relative_path(&file.path)?;
            if !seen_paths.insert(path.to_lowercase()) {
                return Err(TransferError::InvalidArchive(format!(
                    "duplicate file path {:?}",
                    file.path
                )));
            }

            let size = file.bytes.len() as u64;
            if size > limits.max_file_bytes {
                return Err(TransferError::TooLarge(format!(
                    "file {:?} is {size} bytes, limit {}",
                    path, limits.max_file_bytes
                )));
            }
            total_uncompressed = total_uncompressed
                .checked_add(size)
                .ok_or_else(|| TransferError::TooLarge("uncompressed size overflow".into()))?;
            if total_uncompressed > limits.max_total_uncompressed {
                return Err(TransferError::TooLarge(format!(
                    "uncompressed data exceeds {} bytes",
                    limits.max_total_uncompressed
                )));
            }

            file_count += 1;
            if file_count > limits.max_entries {
                return Err(TransferError::TooLarge(format!(
                    "{file_count} entries, limit {}",
                    limits.max_entries
                )));
            }

            file.path = path;
            file.size = Some(size);
            file.sha256 = Some(sha256_hex(&file.bytes));
        }
        reject_file_directory_conflicts(
            novel.files.iter().map(|file| file.path.as_str()),
            novel_index,
        )?;
    }

    let manifest_bytes = serde_json::to_vec(&normalized)?;
    if manifest_bytes.len() as u64 > limits.max_manifest_bytes {
        return Err(TransferError::TooLarge(format!(
            "manifest is {} bytes, limit {}",
            manifest_bytes.len(),
            limits.max_manifest_bytes
        )));
    }
    total_uncompressed = total_uncompressed
        .checked_add(manifest_bytes.len() as u64)
        .ok_or_else(|| TransferError::TooLarge("uncompressed size overflow".into()))?;
    if total_uncompressed > limits.max_total_uncompressed {
        return Err(TransferError::TooLarge(format!(
            "uncompressed data exceeds {} bytes",
            limits.max_total_uncompressed
        )));
    }

    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file(MANIFEST_PATH, options)?;
    writer.write_all(&manifest_bytes).map_err(zip_io)?;

    for (index, novel) in normalized.novels.iter().enumerate() {
        for file in &novel.files {
            let name = format!("novels/{index}/{}", file.path);
            writer.start_file(name, options)?;
            writer.write_all(&file.bytes).map_err(zip_io)?;
        }
    }

    let bytes = writer.finish()?.into_inner();
    if bytes.len() as u64 > limits.max_archive_bytes {
        return Err(TransferError::TooLarge(format!(
            "archive is {} bytes, limit {}",
            bytes.len(),
            limits.max_archive_bytes
        )));
    }
    Ok(bytes)
}

/// Validates and decodes a transfer archive.
///
/// The parser enforces the size, entry-count and novel-count caps, rejects
/// unsafe paths and duplicate (case-insensitively normalised) paths, requires
/// exactly the files listed in `manifest.json`, and verifies each file's size
/// and SHA-256 when the manifest stores them.
pub fn parse_archive(bytes: &[u8]) -> Result<ParsedArchive> {
    parse_archive_with_limits(bytes, DEFAULT_LIMITS)
}

pub(crate) fn parse_archive_with_limits(bytes: &[u8], limits: Limits) -> Result<ParsedArchive> {
    if bytes.len() as u64 > limits.max_archive_bytes {
        return Err(TransferError::TooLarge(format!(
            "archive is {} bytes, limit {}",
            bytes.len(),
            limits.max_archive_bytes
        )));
    }

    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > limits.max_entries {
        return Err(TransferError::TooLarge(format!(
            "{} zip entries, limit {}",
            archive.len(),
            limits.max_entries
        )));
    }

    let mut file_entries = Vec::new();
    let mut manifest_entry = None;
    let mut total_declared = 0u64;
    let mut seen_names = HashSet::new();

    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let raw_name = file.name()?.into_owned();
        let is_dir = file.is_dir();
        let size = file.size();

        if file.is_symlink() {
            return Err(TransferError::UnsafePath(format!(
                "symlink entry {raw_name:?}"
            )));
        }

        // Directory entries carry a trailing slash. Strip exactly one so that
        // `dir//` still fails validation instead of being silently repaired.
        let name = if is_dir {
            raw_name.strip_suffix('/').unwrap_or(&raw_name).to_owned()
        } else {
            raw_name.clone()
        };
        let normalized = validate_relative_path(&name)?;
        if !seen_names.insert(normalized.to_lowercase()) {
            return Err(TransferError::InvalidArchive(format!(
                "duplicate archive entry {raw_name:?}"
            )));
        }
        if is_dir {
            continue;
        }

        if size > limits.max_file_bytes {
            return Err(TransferError::TooLarge(format!(
                "entry {:?} declares {size} bytes, limit {}",
                normalized, limits.max_file_bytes
            )));
        }
        total_declared = total_declared
            .checked_add(size)
            .ok_or_else(|| TransferError::TooLarge("uncompressed size overflow".into()))?;
        if total_declared > limits.max_total_uncompressed {
            return Err(TransferError::TooLarge(format!(
                "declared uncompressed data exceeds {} bytes",
                limits.max_total_uncompressed
            )));
        }

        let entry = EntryInfo {
            index,
            name: normalized.clone(),
            size,
        };
        if normalized == MANIFEST_PATH {
            if manifest_entry.replace(entry).is_some() {
                return Err(TransferError::InvalidArchive(
                    "duplicate manifest.json entry".into(),
                ));
            }
        } else {
            file_entries.push(entry);
        }
    }

    let manifest_entry = manifest_entry
        .ok_or_else(|| TransferError::InvalidArchive("manifest.json is missing".into()))?;
    if manifest_entry.size > limits.max_manifest_bytes {
        return Err(TransferError::TooLarge(format!(
            "manifest declares {} bytes, limit {}",
            manifest_entry.size, limits.max_manifest_bytes
        )));
    }

    let manifest_bytes = {
        let entry = archive.by_index(manifest_entry.index)?;
        let mut data = Vec::new();
        entry
            .take(limits.max_manifest_bytes.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(zip_io)?;
        data
    };
    if manifest_bytes.len() as u64 > limits.max_manifest_bytes {
        return Err(TransferError::TooLarge(format!(
            "manifest is {} bytes, limit {}",
            manifest_bytes.len(),
            limits.max_manifest_bytes
        )));
    }

    let mut manifest: ArchiveManifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(TransferError::UnsupportedVersion(manifest.format_version));
    }
    if manifest.novels.len() > limits.max_novels {
        return Err(TransferError::TooLarge(format!(
            "{} novels, limit {}",
            manifest.novels.len(),
            limits.max_novels
        )));
    }

    let mut expected: HashMap<String, ExpectedFile> = HashMap::new();
    let mut expected_folded = HashSet::new();
    let mut normalized_paths: Vec<Vec<String>> =
        (0..manifest.novels.len()).map(|_| Vec::new()).collect();

    for (novel_index, novel) in manifest.novels.iter_mut().enumerate() {
        // Legacy archives carried the classification inside `tags`; migrate it
        // into `attribute_tag` so every consumer sees the same shape.
        super::attribute::normalize(novel)?;
        let mut seen_paths = HashSet::new();
        for (position, file) in novel.files.iter().enumerate() {
            let path = validate_relative_path(&file.path)?;
            if !seen_paths.insert(path.to_lowercase()) {
                return Err(TransferError::InvalidArchive(format!(
                    "novel {novel_index} has duplicate file path {:?}",
                    file.path
                )));
            }
            if let Some(size) = file.size
                && size > limits.max_file_bytes
            {
                return Err(TransferError::TooLarge(format!(
                    "manifest file {:?} declares {size} bytes, limit {}",
                    path, limits.max_file_bytes
                )));
            }
            if let Some(sha256) = &file.sha256 {
                validate_sha256(sha256)?;
            }

            let full_name = format!("novels/{novel_index}/{path}");
            if !expected_folded.insert(full_name.to_lowercase()) {
                return Err(TransferError::InvalidArchive(format!(
                    "duplicate normalised archive path {full_name:?}"
                )));
            }
            expected.insert(
                full_name,
                ExpectedFile {
                    novel_index,
                    position,
                    path: path.clone(),
                    size: file.size,
                    sha256: file.sha256.clone(),
                },
            );
            normalized_paths[novel_index].push(path);
        }
        reject_file_directory_conflicts(
            normalized_paths[novel_index].iter().map(String::as_str),
            novel_index,
        )?;
    }

    if expected.len().saturating_add(1) > limits.max_entries {
        return Err(TransferError::TooLarge(format!(
            "manifest lists {} files, limit {}",
            expected.len(),
            limits.max_entries.saturating_sub(1)
        )));
    }

    let mut matched = Vec::with_capacity(expected.len());
    for entry in file_entries {
        let Some(expected_file) = expected.remove(&entry.name) else {
            return Err(TransferError::InvalidArchive(format!(
                "archive entry {:?} is not listed in the manifest",
                entry.name
            )));
        };
        if let Some(size) = expected_file.size
            && size != entry.size
        {
            return Err(TransferError::InvalidArchive(format!(
                "entry {:?} is {size} bytes in the manifest but {} bytes in the zip",
                entry.name, entry.size
            )));
        }
        matched.push((entry.index, expected_file));
    }
    if let Some(missing) = expected.keys().next() {
        return Err(TransferError::InvalidArchive(format!(
            "manifest lists missing archive entry {missing:?}"
        )));
    }

    let mut total_actual = manifest_bytes.len() as u64;
    if total_actual > limits.max_total_uncompressed {
        return Err(TransferError::TooLarge(format!(
            "uncompressed data exceeds {} bytes",
            limits.max_total_uncompressed
        )));
    }

    let mut parsed_novels: Vec<ParsedNovel> = manifest
        .novels
        .iter()
        .enumerate()
        .map(|(index, novel)| ParsedNovel {
            index,
            toc_url: novel.toc_url.clone(),
            title: novel.title.clone(),
            files: normalized_paths[index]
                .iter()
                .map(|path| (path.clone(), Vec::new()))
                .collect(),
        })
        .collect();

    for (entry_index, expected_file) in matched {
        let file = archive.by_index(entry_index)?;
        if file.size() > limits.max_file_bytes {
            return Err(TransferError::TooLarge(format!(
                "entry {:?} declares {} bytes, limit {}",
                expected_file.path,
                file.size(),
                limits.max_file_bytes
            )));
        }

        let mut data = Vec::new();
        file.take(limits.max_file_bytes.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(zip_io)?;

        let actual_size = data.len() as u64;
        if actual_size > limits.max_file_bytes {
            return Err(TransferError::TooLarge(format!(
                "entry {:?} expands to {actual_size} bytes, limit {}",
                expected_file.path, limits.max_file_bytes
            )));
        }
        total_actual = total_actual
            .checked_add(actual_size)
            .ok_or_else(|| TransferError::TooLarge("uncompressed size overflow".into()))?;
        if total_actual > limits.max_total_uncompressed {
            return Err(TransferError::TooLarge(format!(
                "uncompressed data exceeds {} bytes",
                limits.max_total_uncompressed
            )));
        }

        if let Some(size) = expected_file.size
            && actual_size != size
        {
            return Err(TransferError::InvalidArchive(format!(
                "entry {:?} is {size} bytes in the manifest but {actual_size} bytes in the archive",
                expected_file.path
            )));
        }
        if let Some(expected_sha) = &expected_file.sha256 {
            let actual = sha256_hex(&data);
            if !actual.eq_ignore_ascii_case(expected_sha) {
                return Err(TransferError::HashMismatch {
                    path: expected_file.path.clone(),
                    expected: expected_sha.clone(),
                    actual,
                });
            }
        }

        parsed_novels[expected_file.novel_index].files[expected_file.position].1 = data;
    }

    Ok(ParsedArchive {
        manifest,
        novels: parsed_novels,
    })
}

struct EntryInfo {
    index: usize,
    name: String,
    size: u64,
}

struct ExpectedFile {
    novel_index: usize,
    position: usize,
    path: String,
    size: Option<u64>,
    sha256: Option<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn zip_io(error: std::io::Error) -> TransferError {
    TransferError::Zip(zip::result::ZipError::Io(error))
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TransferError::InvalidArchive(format!(
            "invalid sha256 digest {value:?}"
        )));
    }
    Ok(())
}

/// Validates a path that must stay inside the destination directory.
///
/// Rejects absolute paths, drive prefixes, backslashes, NULs, `.`/`..`
/// components, empty components, control characters, Windows-invalid
/// characters, trailing dots/spaces and Windows reserved device names.
pub(crate) fn validate_relative_path(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(TransferError::UnsafePath("empty path".into()));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(TransferError::UnsafePath(format!(
            "path is {} bytes, limit {MAX_PATH_BYTES}",
            path.len()
        )));
    }
    if path.contains('\0') {
        return Err(TransferError::UnsafePath(format!("NUL in path {path:?}")));
    }
    if path.contains('\\') {
        return Err(TransferError::UnsafePath(format!(
            "backslash in path {path:?}"
        )));
    }
    if path.starts_with('/') {
        return Err(TransferError::UnsafePath(format!("absolute path {path:?}")));
    }

    let mut normalized = String::with_capacity(path.len());
    for (index, component) in path.split('/').enumerate() {
        if component.is_empty() {
            return Err(TransferError::UnsafePath(format!(
                "empty path component in {path:?}"
            )));
        }
        if component == "." || component == ".." {
            return Err(TransferError::UnsafePath(format!(
                "path traversal in {path:?}"
            )));
        }
        if index == 0 {
            let bytes = component.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                return Err(TransferError::UnsafePath(format!(
                    "drive-relative path {path:?}"
                )));
            }
        }
        validate_component(component, path)?;

        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Ok(normalized)
}

fn validate_component(component: &str, path: &str) -> Result<()> {
    if component.chars().count() > MAX_PATH_COMPONENT_CHARS {
        return Err(TransferError::UnsafePath(format!(
            "path component too long in {path:?}"
        )));
    }
    if component.ends_with('.') || component.ends_with(' ') {
        return Err(TransferError::UnsafePath(format!(
            "trailing dot or space in {path:?}"
        )));
    }
    for character in component.chars() {
        if character.is_control() {
            return Err(TransferError::UnsafePath(format!(
                "control character in {path:?}"
            )));
        }
        if matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return Err(TransferError::UnsafePath(format!(
                "character {character:?} is not allowed on Windows in {path:?}"
            )));
        }
    }

    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    if is_reserved_device_name(&stem) {
        return Err(TransferError::UnsafePath(format!(
            "reserved Windows device name in {path:?}"
        )));
    }
    Ok(())
}

fn is_reserved_device_name(stem: &str) -> bool {
    matches!(
        stem,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "CLOCK$"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit())
}

/// Rejects a novel that contains both `a` and `a/b`: extracting such an
/// archive would require `a` to be a file and a directory at the same time.
fn reject_file_directory_conflicts<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    novel_index: usize,
) -> Result<()> {
    let paths: Vec<&str> = paths.into_iter().collect();
    if paths.len() < 2 {
        return Ok(());
    }
    let folded: HashSet<String> = paths.iter().map(|path| path.to_lowercase()).collect();
    for path in paths {
        for (slash, _) in path.match_indices('/') {
            let prefix = &path[..slash];
            if folded.contains(&prefix.to_lowercase()) {
                return Err(TransferError::InvalidArchive(format!(
                    "novel {novel_index} file {prefix:?} is also used as a directory"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::{CompressionMethod, ZipArchive, ZipWriter};

    use super::*;
    use crate::transfer::{
        ArchiveFile, ArchiveManifest, ArchiveNovel, ConflictPolicy, ExistingNovel, TransferError,
        build_archive, parse_archive, plan_import,
    };

    fn file(path: &str, bytes: &[u8]) -> ArchiveFile {
        ArchiveFile::new(path, bytes.to_vec())
    }

    fn manifest_with_files(files: Vec<ArchiveFile>) -> ArchiveManifest {
        ArchiveManifest {
            format_version: FORMAT_VERSION,
            app_version: Some("0.5.0".into()),
            exported_at: None,
            novels: vec![ArchiveNovel {
                toc_url: "https://example.com/n1234ab/".into(),
                title: "Novel One".into(),
                author: "Author".into(),
                sitename: "Example".into(),
                domain: Some("example.com".into()),
                tags: vec!["fantasy".into()],
                novel_type: 1,
                files,
                ..ArchiveNovel::default()
            }],
        }
    }

    fn craft_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn craft_with_manifest(manifest: &ArchiveManifest, files: &[(&str, &[u8])]) -> Vec<u8> {
        let json = serde_json::to_vec(manifest).unwrap();
        let mut entries: Vec<(&str, &[u8])> = vec![(MANIFEST_PATH, &json)];
        entries.extend_from_slice(files);
        craft_zip(&entries)
    }

    #[test]
    fn error_implements_std_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<TransferError>();
    }

    #[test]
    fn build_parse_roundtrip() {
        let manifest = manifest_with_files(vec![
            file("toc.txt", "目次".as_bytes()),
            file("section/001.txt", "本文".as_bytes()),
        ]);
        let bytes = build_archive(&manifest).unwrap();
        let parsed = parse_archive(&bytes).unwrap();

        assert_eq!(parsed.manifest.format_version, FORMAT_VERSION);
        assert_eq!(parsed.manifest.app_version.as_deref(), Some("0.5.0"));
        assert_eq!(parsed.manifest.novels[0].title, "Novel One");
        assert_eq!(parsed.manifest.novels.len(), 1);
        // Files are sorted by path in the written manifest.
        assert_eq!(parsed.manifest.novels[0].files[0].path, "section/001.txt");
        assert_eq!(parsed.manifest.novels[0].files[1].path, "toc.txt");
        assert!(parsed.manifest.novels[0].files[0].sha256.is_some());
        assert!(parsed.manifest.novels[0].files[0].bytes.is_empty());

        assert_eq!(parsed.novels.len(), 1);
        assert_eq!(parsed.novels[0].index, 0);
        assert_eq!(parsed.novels[0].toc_url, "https://example.com/n1234ab/");
        assert_eq!(
            parsed.novels[0].files,
            vec![
                ("section/001.txt".into(), "本文".as_bytes().to_vec()),
                ("toc.txt".into(), "目次".as_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn attribute_tag_travels_in_its_own_field() {
        let mut manifest = manifest_with_files(vec![file("toc.txt", b"toc")]);
        manifest.novels[0].attribute_tag = Some("漫画".into());
        let bytes = build_archive(&manifest).unwrap();
        let parsed = parse_archive(&bytes).unwrap();
        let novel = &parsed.manifest.novels[0];
        assert_eq!(novel.attribute_tag.as_deref(), Some("漫画"));
        // The classification never leaks into the ordinary tag list.
        assert!(novel.tags.iter().all(|tag| tag != "漫画"));
    }

    /// A manifest written before `attribute_tag` existed: the classification
    /// lived inside `tags`.
    fn legacy_manifest(tags: &[&str]) -> String {
        serde_json::json!({
            "format_version": FORMAT_VERSION,
            "novels": [{
                "toc_url": "https://example.com/n1/",
                "title": "T",
                "author": "A",
                "sitename": "S",
                "tags": tags,
                "novel_type": 1,
                "files": [{ "path": "toc.txt", "size": 3 }],
            }],
        })
        .to_string()
    }

    fn legacy_archive(tags: &[&str]) -> Vec<u8> {
        craft_zip(&[
            (MANIFEST_PATH, legacy_manifest(tags).as_bytes()),
            ("novels/0/toc.txt", b"toc"),
        ])
    }

    #[test]
    fn legacy_attribute_in_tags_is_migrated_on_read() {
        let bytes = legacy_archive(&["fantasy", "小説"]);
        let parsed = parse_archive(&bytes).unwrap();
        let novel = &parsed.manifest.novels[0];
        assert_eq!(novel.attribute_tag.as_deref(), Some("小説"));
        assert_eq!(novel.tags, vec!["fantasy".to_string()]);
    }

    #[test]
    fn conflicting_legacy_attributes_are_dropped() {
        let bytes = legacy_archive(&["小説", "漫画"]);
        let parsed = parse_archive(&bytes).unwrap();
        let novel = &parsed.manifest.novels[0];
        assert_eq!(novel.attribute_tag, None);
        assert!(novel.tags.is_empty());
    }

    #[test]
    fn unknown_attribute_tag_is_rejected() {
        let mut manifest = manifest_with_files(vec![file("toc.txt", b"toc")]);
        manifest.novels[0].attribute_tag = Some("ゲーム".into());
        // Both directions refuse an unknown classification.
        assert!(matches!(
            build_archive(&manifest),
            Err(TransferError::InvalidArchive(_))
        ));
        // A hand-written archive is rejected on read as well.
        let json = serde_json::json!({
            "format_version": FORMAT_VERSION,
            "novels": [{
                "toc_url": "https://example.com/n1/",
                "title": "T",
                "author": "A",
                "sitename": "S",
                "attribute_tag": "ゲーム",
                "novel_type": 1,
                "files": [{ "path": "toc.txt", "size": 3 }],
            }],
        });
        let bytes = craft_zip(&[(MANIFEST_PATH, json.to_string().as_bytes()), ("novels/0/toc.txt", b"toc")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));
    }

    #[test]
    fn empty_archive_roundtrips() {
        let manifest = ArchiveManifest::default();
        let bytes = build_archive(&manifest).unwrap();
        let parsed = parse_archive(&bytes).unwrap();
        assert!(parsed.manifest.novels.is_empty());
        assert!(parsed.novels.is_empty());
    }

    #[test]
    fn output_is_deterministic() {
        let manifest = manifest_with_files(vec![
            file("toc.txt", b"toc"),
            file("section/001.txt", b"body"),
        ]);
        let first = build_archive(&manifest).unwrap();

        let mut reversed = manifest.clone();
        reversed.novels[0].files.reverse();
        let second = build_archive(&reversed).unwrap();
        assert_eq!(first, second);

        let mut archive = ZipArchive::new(Cursor::new(&first)).unwrap();
        assert_eq!(archive.by_index(0).unwrap().name().unwrap(), MANIFEST_PATH);
    }

    #[test]
    fn novels_are_sorted_by_toc_url() {
        let mut manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        manifest.novels.push(ArchiveNovel {
            toc_url: "https://example.com/b/".into(),
            title: "B".into(),
            files: vec![file("b.txt", b"b")],
            ..ArchiveNovel::default()
        });
        manifest.novels.push(ArchiveNovel {
            toc_url: "https://example.com/a/".into(),
            title: "A".into(),
            files: vec![file("c.txt", b"c")],
            ..ArchiveNovel::default()
        });
        let parsed = parse_archive(&build_archive(&manifest).unwrap()).unwrap();
        let urls: Vec<&str> = parsed
            .manifest
            .novels
            .iter()
            .map(|novel| novel.toc_url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/a/",
                "https://example.com/b/",
                "https://example.com/n1234ab/",
            ]
        );
    }

    #[test]
    fn rejects_unsafe_paths_when_building() {
        for path in [
            "../escape.txt",
            "dir/../../escape.txt",
            "/absolute.txt",
            "a\\b.txt",
            "bad\0name.txt",
            "CON",
            "nul.txt",
            "COM1.log",
            "LPT9",
            "COM¹.txt",
            "LPT³",
            "C:/windows.txt",
            "trailing.",
            "trailing ",
            "bad:name.txt",
            "dir//empty.txt",
        ] {
            let manifest = manifest_with_files(vec![file(path, b"x")]);
            match build_archive(&manifest) {
                Err(TransferError::UnsafePath(_)) => {}
                other => panic!("{path:?} should be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_unsafe_entry_names_when_parsing() {
        let valid = manifest_with_files(vec![file("toc.txt", b"x")]);
        for name in [
            "novels/0/../escape.txt",
            "/absolute.txt",
            "novels/0/a\\b.txt",
            "novels/0/bad\0name.txt",
            "novels/0/CON",
            "C:/windows.txt",
        ] {
            let bytes = craft_with_manifest(&valid, &[(name, b"x")]);
            match parse_archive(&bytes) {
                Err(TransferError::UnsafePath(_)) => {}
                other => panic!("{name:?} should be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_manifest_unsafe_paths_when_parsing() {
        let manifest = manifest_with_files(vec![file("../escape.txt", b"x")]);
        let bytes = craft_with_manifest(&manifest, &[("novels/0/escape.txt", b"x")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::UnsafePath(_))
        ));
    }

    #[test]
    fn rejects_duplicate_paths() {
        for files in [
            vec![file("same.txt", b"a"), file("same.txt", b"b")],
            vec![file("same.txt", b"a"), file("SAME.TXT", b"b")],
        ] {
            let manifest = manifest_with_files(files);
            assert!(matches!(
                build_archive(&manifest),
                Err(TransferError::InvalidArchive(_))
            ));
        }

        // The manifest itself may not list the same normalised path twice.
        let manifest = manifest_with_files(vec![file("same.txt", b"a"), file("SAME.TXT", b"b")]);
        let bytes = craft_with_manifest(
            &manifest,
            &[("novels/0/same.txt", b"a"), ("novels/0/SAME.TXT", b"b")],
        );
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));
    }

    #[test]
    fn rejects_duplicate_actual_entries() {
        let manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        let bytes = craft_with_manifest(
            &manifest,
            &[("novels/0/a.txt", b"a"), ("novels/0/A.TXT", b"b")],
        );
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));
    }

    #[test]
    fn rejects_file_directory_conflicts() {
        for files in [
            vec![file("a", b"x"), file("a/b.txt", b"y")],
            vec![file("A", b"x"), file("a/b.txt", b"y")],
        ] {
            let manifest = manifest_with_files(files);
            assert!(matches!(
                build_archive(&manifest),
                Err(TransferError::InvalidArchive(_))
            ));
        }

        let manifest = manifest_with_files(vec![file("a", b"x"), file("a/b.txt", b"y")]);
        let bytes = craft_with_manifest(
            &manifest,
            &[("novels/0/a", b"x"), ("novels/0/a/b.txt", b"y")],
        );
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));
    }

    #[test]
    fn rejects_version_mismatch() {
        let mut manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        manifest.format_version = FORMAT_VERSION + 1;
        assert!(matches!(
            build_archive(&manifest),
            Err(TransferError::UnsupportedVersion(_))
        ));

        let bytes = craft_with_manifest(&manifest, &[("novels/0/a.txt", b"a")]);
        match parse_archive(&bytes) {
            Err(TransferError::UnsupportedVersion(version)) => {
                assert_eq!(version, FORMAT_VERSION + 1);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_and_mismatched_manifest_entries() {
        // Missing manifest.json.
        let bytes = craft_zip(&[("novels/0/a.txt", b"a")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));

        // Manifest lists a file that is missing from the zip.
        let manifest = manifest_with_files(vec![file("a.txt", b"a"), file("b.txt", b"b")]);
        let bytes = craft_with_manifest(&manifest, &[("novels/0/a.txt", b"a")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));

        // Zip has an entry that the manifest does not list.
        let manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        let bytes = craft_with_manifest(
            &manifest,
            &[("novels/0/a.txt", b"a"), ("novels/0/b.txt", b"b")],
        );
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));

        // Manifest size disagrees with the zip entry.
        let mut manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        manifest.novels[0].files[0].size = Some(99);
        let bytes = craft_with_manifest(&manifest, &[("novels/0/a.txt", b"a")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));
    }

    #[test]
    fn rejects_hash_mismatch_and_invalid_digest() {
        let mut manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        manifest.novels[0].files[0].sha256 = Some("0".repeat(64));
        manifest.novels[0].files[0].size = Some(1);
        let bytes = craft_with_manifest(&manifest, &[("novels/0/a.txt", b"a")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::HashMismatch { .. })
        ));

        let mut manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        manifest.novels[0].files[0].sha256 = Some("not-a-digest".into());
        let bytes = craft_with_manifest(&manifest, &[("novels/0/a.txt", b"a")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::InvalidArchive(_))
        ));
    }

    #[test]
    fn rejects_malformed_json_and_missing_manifest_fields() {
        let bytes = craft_zip(&[(MANIFEST_PATH, b"{ not json")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::Serde(_))
        ));

        let bytes = craft_zip(&[(MANIFEST_PATH, b"{}")]);
        assert!(matches!(
            parse_archive(&bytes),
            Err(TransferError::Serde(_))
        ));
    }

    #[test]
    fn enforces_archive_parse_limits() {
        let manifest = manifest_with_files(vec![file("a.txt", b"12345"), file("b.txt", b"67890")]);
        let bytes = build_archive(&manifest).unwrap();

        assert!(matches!(
            parse_archive_with_limits(
                &bytes,
                Limits {
                    max_file_bytes: 4,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            parse_archive_with_limits(
                &bytes,
                Limits {
                    max_total_uncompressed: 5,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            parse_archive_with_limits(
                &bytes,
                Limits {
                    max_entries: 2,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            parse_archive_with_limits(
                &bytes,
                Limits {
                    max_archive_bytes: 1,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            parse_archive_with_limits(
                &bytes,
                Limits {
                    max_manifest_bytes: 8,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            parse_archive_with_limits(
                &bytes,
                Limits {
                    max_novels: 0,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
    }

    #[test]
    fn enforces_archive_build_limits() {
        let manifest = manifest_with_files(vec![file("a.txt", b"12345"), file("b.txt", b"67890")]);
        assert!(matches!(
            build_archive_with_limits(
                &manifest,
                Limits {
                    max_file_bytes: 4,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            build_archive_with_limits(
                &manifest,
                Limits {
                    max_entries: 1,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
        assert!(matches!(
            build_archive_with_limits(
                &manifest,
                Limits {
                    max_total_uncompressed: 5,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));

        let mut too_many = manifest.clone();
        too_many.novels = vec![ArchiveNovel::default(); 2];
        assert!(matches!(
            build_archive_with_limits(
                &too_many,
                Limits {
                    max_novels: 1,
                    ..DEFAULT_LIMITS
                }
            ),
            Err(TransferError::TooLarge(_))
        ));
    }

    #[test]
    fn plan_import_marks_new_novels_for_creation() {
        let manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        let parsed = parse_archive(&build_archive(&manifest).unwrap()).unwrap();
        let plan = plan_import(&parsed, &[], ConflictPolicy::Skip);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].index, 0);
        assert_eq!(
            plan.entries[0].action,
            crate::transfer::ImportAction::Create
        );
        assert!(!plan.entries[0].reason.is_empty());
    }

    #[test]
    fn plan_import_resolves_conflicts_by_toc_url() {
        let manifest = manifest_with_files(vec![file("a.txt", b"a")]);
        let parsed = parse_archive(&build_archive(&manifest).unwrap()).unwrap();
        let existing = vec![ExistingNovel {
            toc_url: manifest.novels[0].toc_url.clone(),
            title: manifest.novels[0].title.clone(),
        }];

        let skip = plan_import(&parsed, &existing, ConflictPolicy::Skip);
        assert_eq!(skip.entries[0].action, crate::transfer::ImportAction::Skip);
        let overwrite = plan_import(&parsed, &existing, ConflictPolicy::Overwrite);
        assert_eq!(
            overwrite.entries[0].action,
            crate::transfer::ImportAction::Overwrite
        );

        // Title alone is never the conflict key.
        let title_only = vec![ExistingNovel {
            toc_url: "https://example.com/other/".into(),
            title: manifest.novels[0].title.clone(),
        }];
        let plan = plan_import(&parsed, &title_only, ConflictPolicy::Skip);
        assert_eq!(
            plan.entries[0].action,
            crate::transfer::ImportAction::Create
        );
    }
}
