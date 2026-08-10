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
pub struct ObjectKey(pub String);

impl ObjectKey {
    /// Construct a key at an existing call site.
    ///
    /// New external input should use [`Self::try_new`] so invalid traversal
    /// components are rejected at the boundary.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn try_new(key: impl Into<String>) -> Result<Self> {
        let key = Self(key.into());
        key.validate()?;
        Ok(key)
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
    pub fn as_ref(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, key: &ObjectKey) -> bool {
        let prefix = self.0.trim_end_matches('/');
        prefix.is_empty()
            || key.as_ref() == prefix
            || (key.as_ref().starts_with(prefix)
                && key.as_ref().as_bytes().get(prefix.len()) == Some(&b'/'))
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
        ObjectKey::new(format!("{}/{}", self.prefix.0, component))
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
    use super::*;

    #[test]
    fn object_key_display_and_validation() {
        assert_eq!(
            ObjectKey::new("novels/123/toc.yaml").to_string(),
            "novels/123/toc.yaml"
        );
        assert_eq!(ObjectKey::new("a").as_ref(), "a");
        assert!(ObjectKey::try_new("../escape").is_err());
        assert!(ObjectKey::try_new(r"C:\escape").is_err());
    }

    #[test]
    fn novel_keys_preserve_native_layout_components() {
        let keys = NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        assert_eq!(keys.prefix().as_ref(), "novels/site/12/n1234ab");
        assert_eq!(
            keys.section("1", "第1話"),
            ObjectKey::new("novels/site/12/n1234ab/本文/1 第1話.yaml")
        );
        assert_eq!(
            keys.raw_section("1", "第1話"),
            ObjectKey::new("novels/site/12/n1234ab/raw/1 第1話.html")
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
        assert!(prefix.matches(&ObjectKey::new("novels/site/book/toc.yaml")));
        assert!(prefix.matches(&ObjectKey::new("novels/site")));
        assert!(!prefix.matches(&ObjectKey::new("novels/site-archive/book/toc.yaml")));
    }
}
