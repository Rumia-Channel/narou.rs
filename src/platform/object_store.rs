//! Object and asset storage abstractions.
//!
//! Domain-level files are addressed by canonical logical keys, never by OS
//! `PathBuf`. Native adapters map keys onto the existing `.narou/` and
//! `小説データ/` layout; Worker adapters can map the same keys onto S3.

use std::fmt;
use std::num::NonZeroUsize;
use std::pin::Pin;

use futures::Stream;

use crate::error::{NarouError, Result};

use super::{PlatformFuture, PlatformService};

/// Logical object key. It is not an operating-system path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// Construct a validated logical object key from external input.
    pub fn try_new(key: impl Into<String>) -> Result<Self> {
        let key = Self(key.into());
        key.validate()?;
        Ok(key)
    }

    /// Construct a key from an internal generator that already enforces the
    /// logical-key invariant.
    pub(crate) fn new_unchecked(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn validate(&self) -> Result<()> {
        validate_logical_key(&self.0)
    }

    pub fn join(&self, component: impl AsRef<str>) -> Result<Self> {
        let component = component.as_ref();
        if component.is_empty() || component.contains('/') || component.contains('\\') {
            return Err(NarouError::Platform(format!(
                "invalid object key component: {component:?}"
            )));
        }
        Self::try_new(format!("{}/{}", self.0.trim_end_matches('/'), component))
    }
}


impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ObjectKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Logical identity for generated converter output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GeneratedAssetKey(ObjectKey);

impl GeneratedAssetKey {
    pub fn new(namespace: &str, filename: &str) -> Result<Self> {
        let namespace = sanitize_key_component(namespace);
        let filename = sanitize_key_component(filename);
        Ok(Self(ObjectKey::try_new(format!(
            "generated/{namespace}/{filename}"
        ))?))
    }

    pub fn as_object_key(&self) -> &ObjectKey {
        &self.0
    }
}

/// 拡張子から保存時の `Content-Type` を決める。
///
/// ObjectStore / AssetStore の実装 (D1 / S3) で同じ判断を使うため core に置く。
pub fn content_type_for_key(key: &ObjectKey) -> Option<&'static str> {
    match key.as_ref().rsplit('.').next() {
        Some("yaml") | Some("yml") => Some("application/yaml"),
        Some("txt") | Some("ini") => Some("text/plain; charset=utf-8"),
        Some("html") => Some("text/html; charset=utf-8"),
        Some("epub") => Some("application/epub+zip"),
        Some("zip") => Some("application/zip"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("gif") => Some("image/gif"),
        _ => None,
    }
}

/// A validated prefix used for paginated listing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectPrefix(String);

impl ObjectPrefix {
    pub fn new(prefix: impl Into<String>) -> Result<Self> {
        let prefix = prefix.into();
        if prefix.is_empty() {
            return Ok(Self(prefix));
        }
        validate_logical_key(prefix.trim_end_matches('/'))?;
        Ok(Self(prefix))
    }

    pub fn matches(&self, key: &ObjectKey) -> bool {
        let prefix = self.0.trim_end_matches('/');
        prefix.is_empty()
            || key.as_ref() == prefix
            || (key.as_ref().starts_with(prefix)
                && key.as_ref().as_bytes().get(prefix.len()) == Some(&b'/'))
    }
}

impl AsRef<str> for ObjectPrefix {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<ObjectKey> for ObjectPrefix {
    fn from(key: ObjectKey) -> Self {
        Self(key.0)
    }
}

/// Metadata returned by object and asset stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub key: ObjectKey,
    pub size: u64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// One bounded chunk in an asset stream.
pub type AssetChunk = Vec<u8>;

#[cfg(not(target_arch = "wasm32"))]
pub type AssetStream = Pin<Box<dyn Stream<Item = Result<AssetChunk>> + Send + 'static>>;

#[cfg(target_arch = "wasm32")]
pub type AssetStream = Pin<Box<dyn Stream<Item = Result<AssetChunk>> + 'static>>;

/// A bounded, cursor-based object listing request.
#[derive(Debug, Clone)]
pub struct ObjectListRequest {
    pub prefix: ObjectPrefix,
    pub cursor: Option<String>,
    pub limit: NonZeroUsize,
}

impl ObjectListRequest {
    pub fn new(prefix: ObjectPrefix, limit: NonZeroUsize) -> Self {
        Self {
            prefix,
            cursor: None,
            limit,
        }
    }

    pub fn after(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectListPage {
    pub objects: Vec<ObjectMetadata>,
    pub next_cursor: Option<String>,
}

/// Apply the shared `ObjectListRequest` cursor/limit contract to a listing
/// already sorted by key ascending.
///
/// Every backend (filesystem, SQLite, in-memory, D1) resolves its own key
/// set, then delegates the page cut here so the contract lives once: the
/// cursor is the key of the **last object returned** by the previous page
/// (`next_cursor`), and resumption skips to the first key strictly greater
/// than the cursor. Because the cursor names a returned key, deleting the
/// object it points to does not lose the next page — unlike a cursor that
/// names the first *unreturned* key, which is never emitted and would drop
/// one object at every page boundary.
///
/// Returns the page plus the cursor for the next request (`None` when this
/// page reached the end of the listing).
pub fn paginate_object_listing<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    limit: usize,
    key_of: impl Fn(&T) -> &str,
) -> (Vec<T>, Option<String>) {
    let start = match cursor {
        Some(cursor) => items
            .iter()
            .position(|item| key_of(item) > cursor)
            .unwrap_or(items.len()),
        None => 0,
    };
    let end = start.saturating_add(limit).min(items.len());
    let next_cursor = (end < items.len())
        .then(|| &items[end - 1])
        .map(|item| key_of(item).to_string());
    (
        items
            .into_iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect(),
        next_cursor,
    )
}

/// String-prefix range bound for a `WHERE key >= ? AND key < ?` scan.
///
/// Appending the highest code point makes the bound cover every key that starts
/// with the prefix — including keys whose next character sorts above `'0'`
/// (a naive `{prefix}0` bound hides every CJK or letter-leading name).
pub fn prefix_upper_bound(prefix: &str) -> String {
    format!("{prefix}\u{10FFFF}")
}

/// Blob store for bounded control objects.
///
/// `read_small`/`write_small` deliberately make the buffering contract
/// explicit. Large or generated binary assets use [`AssetStore`] instead.
pub trait ObjectStore: PlatformService {
    fn stat<'a>(&'a self, key: &'a ObjectKey)
    -> PlatformFuture<'a, Result<Option<ObjectMetadata>>>;

    fn exists<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.stat(key).await?.is_some()) })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>>;

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>>;

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>>;

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>>;
}

/// Storage boundary for potentially large binary assets.
///
/// The interface never exposes `File`, `Path`, an SDK stream, or an
/// unbounded `Vec`. Implementations may use chunked filesystem, HTTP, or
/// object-storage streams internally.
pub trait AssetStore: PlatformService {
    fn stat<'a>(&'a self, key: &'a ObjectKey)
    -> PlatformFuture<'a, Result<Option<ObjectMetadata>>>;

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>>;

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>>;

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>>;

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Move semantics are logical; native may rename while S3 may copy/delete.
    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>>;
}

/// Canonical keys for novel-owned objects.
///
/// The native mapping strips the `novels/` namespace and joins the remainder
/// below the existing archive root. Worker mappings can use the full key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NovelObjectKeys {
    prefix: ObjectKey,
}

impl NovelObjectKeys {
    pub fn new(sitename: &str, file_title: &str, use_subdirectory: bool) -> Result<Self> {
        let safe_site = sanitize_key_component(sitename);
        let safe_title = sanitize_key_component(file_title);
        let mut parts = vec!["novels".to_string(), safe_site];
        if use_subdirectory {
            let subdirectory = create_subdirectory_name(&safe_title);
            if !subdirectory.is_empty() {
                parts.push(subdirectory);
            }
        }
        parts.push(safe_title);
        ObjectKey::try_new(parts.join("/")).map(|prefix| Self { prefix })
    }

    pub fn prefix(&self) -> &ObjectKey {
        &self.prefix
    }

    pub fn toc(&self) -> ObjectKey {
        self.child("toc.yaml")
    }

    pub fn section(&self, index: &str, file_subtitle: &str) -> ObjectKey {
        self.child(&format!(
            "本文/{} {}.yaml",
            index,
            sanitize_key_component_with_limit(file_subtitle, Some(80))
        ))
    }

    pub fn raw_section(&self, index: &str, file_subtitle: &str) -> ObjectKey {
        let section = self.section(index, file_subtitle);
        let filename = section
            .as_ref()
            .rsplit('/')
            .next()
            .unwrap_or("section.yaml")
            .strip_suffix(".yaml")
            .unwrap_or("section");
        self.child(&format!("raw/{filename}.html"))
    }

    pub fn setting(&self) -> ObjectKey {
        self.child("setting.ini")
    }

    pub fn replace(&self) -> ObjectKey {
        self.child("replace.txt")
    }

    pub fn diff(&self) -> ObjectKey {
        self.child("diff.txt")
    }

    /// Fully converted 青空文庫 text produced by a text-only convert run
    /// (Worker path). Native layouts keep their per-title output file names;
    /// this fixed key is the portable download-time EPUB source.
    pub fn converted_text(&self) -> ObjectKey {
        self.child("novel.txt")
    }

    pub fn illustration(&self, filename: &str) -> Result<ObjectKey> {
        let filename = sanitize_key_component(filename);
        self.child_checked(&format!("挿絵/{filename}"))
    }

    pub fn illustration_cache(&self) -> ObjectKey {
        self.child(".illustration_cache.yaml")
    }

    pub fn cached_section(&self, timestamp: &str, index: &str, file_subtitle: &str) -> ObjectKey {
        self.child(&format!(
            "本文/.cache/{timestamp}/{} {}.yaml",
            index,
            sanitize_key_component_with_limit(file_subtitle, Some(80))
        ))
    }

    fn child(&self, component: &str) -> ObjectKey {
        ObjectKey::new_unchecked(format!("{}/{}", self.prefix.0, component))
    }

    fn child_checked(&self, component: &str) -> Result<ObjectKey> {
        ObjectKey::try_new(format!("{}/{}", self.prefix.0, component))
    }
}

pub fn sanitize_key_component(value: &str) -> String {
    sanitize_key_component_with_limit(value, None)
}

pub fn sanitize_key_component_with_limit(value: &str, limit: Option<usize>) -> String {
    let sanitized = value
        .chars()
        .filter_map(|ch| {
            if ch.is_control() {
                None
            } else if matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                Some('_')
            } else {
                Some(ch)
            }
        })
        .collect::<String>();
    let limited = limit
        .map(|limit| sanitized.chars().take(limit).collect::<String>())
        .unwrap_or(sanitized);
    let mut candidate = limited.trim_end_matches([' ', '.']).to_string();
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        candidate = "_".to_string();
    }
    if is_windows_reserved_name(&candidate) {
        candidate.insert(0, '_');
    }
    candidate
}

/// Stored-payload codec for the `objects`/`object_chunks`/`section_bodies`
/// tables.
///
/// `None` stores raw bytes; `Brotli` stores brotli-compressed bytes (pure
/// Rust, wasm32-safe — zstd needs clang and cannot build for the Worker).
/// `Deflate` is decode-only legacy support for rows written by the first
/// revision of this schema. Compression is applied to the whole payload
/// *before* chunking so chunk boundaries never split a compressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectEncoding {
    None,
    Brotli,
    Deflate,
}

impl ObjectEncoding {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Brotli => "brotli",
            Self::Deflate => "deflate",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "brotli" => Ok(Self::Brotli),
            "deflate" => Ok(Self::Deflate),
            other => Err(NarouError::Platform(format!(
                "unknown object encoding: {other}"
            ))),
        }
    }
}

/// Below this size compression costs more CPU than the bytes it saves.
const COMPRESS_MIN_BYTES: usize = 512;

/// CRC-32 of an uncompressed payload, stored alongside the encoding so
/// reads and `db verify` can detect corruption independent of the codec.
pub fn object_crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Compress `data` when it is large enough and brotli actually shrinks it.
/// Returns the stored bytes plus the encoding marker to persist.
pub fn compress_object_payload(data: &[u8]) -> (Vec<u8>, ObjectEncoding) {
    if data.len() < COMPRESS_MIN_BYTES {
        return (data.to_vec(), ObjectEncoding::None);
    }
    let compressed = brotli_compress(data);
    if compressed.len() < data.len() {
        (compressed, ObjectEncoding::Brotli)
    } else {
        (data.to_vec(), ObjectEncoding::None)
    }
}

/// Reverse [`compress_object_payload`]. `encoding` comes from the object's
/// `encoding` column.
pub fn decompress_object_payload(data: &[u8], encoding: ObjectEncoding) -> Result<Vec<u8>> {
    match encoding {
        ObjectEncoding::None => Ok(data.to_vec()),
        ObjectEncoding::Brotli => brotli_decompress(data),
        ObjectEncoding::Deflate => {
            miniz_oxide::inflate::decompress_to_vec(data).map_err(|error| {
                NarouError::Platform(format!("invalid deflate object payload: {error}"))
            })
        }
    }
}

/// Verify `data` (uncompressed) against a stored CRC-32.
pub fn verify_object_crc32(data: &[u8], expected: u32) -> Result<()> {
    let actual = object_crc32(data);
    if actual != expected {
        return Err(NarouError::Platform(format!(
            "object payload CRC mismatch: expected {expected:08x}, got {actual:08x}"
        )));
    }
    Ok(())
}

fn brotli_compress(data: &[u8]) -> Vec<u8> {
    // Quality 6: near-best ratio for text at a fraction of q11's CPU cost.
    let mut output = Vec::new();
    let params = brotli::enc::BrotliEncoderParams {
        quality: 6,
        ..Default::default()
    };
    let mut input = std::io::Cursor::new(data);
    if brotli::BrotliCompress(&mut input, &mut output, &params).is_err() {
        return data.to_vec();
    }
    output
}

fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut input = std::io::Cursor::new(data);
    brotli::BrotliDecompress(&mut input, &mut output)
        .map_err(|error| NarouError::Platform(format!("invalid brotli object payload: {error}")))?;
    Ok(output)
}

fn create_subdirectory_name(file_title: &str) -> String {
    let chars = if file_title.starts_with('n') {
        file_title.chars().skip(1).take(2).collect::<String>()
    } else {
        file_title.chars().take(2).collect::<String>()
    };
    chars.trim().to_string()
}

fn is_windows_reserved_name(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches([' ', '.']);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn validate_logical_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.starts_with('/')
        || key.ends_with('/')
        || key.contains('\\')
        || key.contains("//")
    {
        return Err(NarouError::Platform(format!(
            "invalid logical object key: {key:?}"
        )));
    }
    for component in key.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(NarouError::Platform(format!(
                "invalid logical object key component: {component:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefix_upper_bound_covers_every_child() {
        let bound = super::prefix_upper_bound("webnovel/");
        assert!("webnovel/contract-test.yaml" < bound.as_str());
        assert!("webnovel/挿絵/0001.jpg" < bound.as_str());
        assert!(super::prefix_upper_bound("").as_str() > "novels/site/title/本文/1 x.yaml");
    }

    use super::*;

    #[test]
    fn object_key_display_and_validation() {
        assert_eq!(
            ObjectKey::try_new("novels/123/toc.yaml").unwrap().to_string(),
            "novels/123/toc.yaml"
        );
        assert_eq!(ObjectKey::try_new("a").unwrap().as_ref(), "a");
        assert!(ObjectKey::try_new("../escape").is_err());
        assert!(ObjectKey::try_new(r"C:\escape").is_err());
    }

    #[test]
    fn novel_keys_preserve_native_layout_components() {
        let keys = NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        assert_eq!(keys.prefix().as_ref(), "novels/site/12/n1234ab");
        assert_eq!(
            keys.section("1", "第1話"),
            ObjectKey::try_new("novels/site/12/n1234ab/本文/1 第1話.yaml").unwrap()
        );
        assert_eq!(
            keys.raw_section("1", "第1話"),
            ObjectKey::try_new("novels/site/12/n1234ab/raw/1 第1話.html").unwrap()
        );
    }

    #[test]
    fn list_request_has_nonzero_limit() {
        let prefix = ObjectPrefix::new("novels/site").unwrap();
        let request = ObjectListRequest::new(prefix, NonZeroUsize::new(10).unwrap());
        assert_eq!(request.limit.get(), 10);
    }

    #[test]
    fn generated_keys_are_platform_neutral() {
        let key = GeneratedAssetKey::new("epub", r#"book\final.zip"#).unwrap();
        assert_eq!(
            key.as_object_key().as_ref(),
            "generated/epub/book_final.zip"
        );
        assert!(ObjectPrefix::new("novels\\bad").is_err());
    }

    #[test]
    fn object_prefix_matches_components_not_string_prefixes() {
        let prefix = ObjectPrefix::new("novels/site").unwrap();
        assert!(prefix.matches(&ObjectKey::try_new("novels/site/book/toc.yaml").unwrap()));
        assert!(prefix.matches(&ObjectKey::try_new("novels/site").unwrap()));
        assert!(!prefix.matches(
            &ObjectKey::try_new("novels/site-archive/book/toc.yaml").unwrap()
        ));
    }

    /// The D1 store used to emit the first *unreturned* key as
    /// `next_cursor`, silently dropping one object at every page boundary.
    /// These tests pin the shared contract every backend now delegates to:
    /// the cursor is the last returned key and paging yields each object
    /// exactly once.
    fn metas(keys: &[&str]) -> Vec<ObjectMetadata> {
        keys.iter()
            .map(|key| ObjectMetadata {
                key: ObjectKey::try_new(*key).unwrap(),
                size: 1,
                etag: None,
                content_type: None,
                last_modified: None,
            })
            .collect()
    }

    fn keys_of(page: &[ObjectMetadata]) -> Vec<String> {
        page.iter()
            .map(|meta| meta.key.as_ref().to_string())
            .collect()
    }

    fn meta_key(meta: &ObjectMetadata) -> &str {
        meta.key.as_ref()
    }

    #[test]
    fn paginated_listing_yields_every_object_exactly_once() {
        let all = metas(&["generated/a", "generated/b", "generated/c"]);
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let (page, next) = paginate_object_listing(all.clone(), cursor.as_deref(), 2, meta_key);
            seen.extend(keys_of(&page));
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, vec!["generated/a", "generated/b", "generated/c"]);
    }

    #[test]
    fn paginate_next_cursor_is_last_returned_key() {
        let (page, next) = paginate_object_listing(
            metas(&["generated/a", "generated/b", "generated/c"]),
            None,
            2,
            meta_key,
        );
        assert_eq!(keys_of(&page), vec!["generated/a", "generated/b"]);
        assert_eq!(next.as_deref(), Some("generated/b"));
    }

    #[test]
    fn paginate_continues_when_cursor_object_is_deleted() {
        let (page, next) = paginate_object_listing(
            metas(&["generated/a", "generated/b", "generated/c"]),
            None,
            1,
            meta_key,
        );
        assert_eq!(keys_of(&page), vec!["generated/a"]);
        let cursor = next.unwrap();

        // Delete the cursor's object before resuming, as a concurrent
        // `delete` (or a failed migration entry) can do.
        let remaining = metas(&["generated/b", "generated/c"]);
        let (page, next) = paginate_object_listing(remaining, Some(&cursor), 1, meta_key);
        assert_eq!(keys_of(&page), vec!["generated/b"]);
        let (page, next) = paginate_object_listing(
            metas(&["generated/c"]),
            next.as_deref(),
            1,
            meta_key,
        );
        assert_eq!(keys_of(&page), vec!["generated/c"]);
        assert_eq!(next, None);
    }

    #[test]
    fn paginate_past_the_end_and_empty_listing() {
        let (page, next) =
            paginate_object_listing(metas(&["generated/a"]), Some("generated/z"), 2, meta_key);
        assert!(page.is_empty());
        assert_eq!(next, None);
        let (page, next) = paginate_object_listing(metas(&[]), None, 2, meta_key);
        assert!(page.is_empty());
        assert_eq!(next, None);
    }
}
