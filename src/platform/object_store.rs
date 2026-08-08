//! Object storage abstraction.
//!
//! Domain-level "files" (novel text, raw HTML, illustrations, generated
//! output, settings, caches) are addressed by logical keys, never by OS
//! `PathBuf`. The native implementation maps keys onto the existing
//! `.narou/` / `小説データ/` layout; a Worker implementation maps them onto
//! S3-compatible object keys (WebARENA Wasabi).

use std::fmt;

/// Logical key of a stored object. Opaque to the domain layer; concrete
/// implementations convert it to a filesystem path or S3 key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(pub String);

impl ObjectKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
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

/// Metadata returned by [`ObjectStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub key: ObjectKey,
    pub size: u64,
}

/// A blob store with logical keys.
///
/// Keep this small; per-use-case stores (settings, novel data, illustrations,
/// generated output) may specialize it. `read` returns `Ok(None)` when the key
/// does not exist, so callers do not need `exists` first.
pub trait ObjectStore: Send + Sync {
    /// Whether an object exists.
    fn exists(&self, key: &ObjectKey) -> crate::error::Result<bool>;

    /// Read the full object. `Ok(None)` when missing.
    fn read(&self, key: &ObjectKey) -> crate::error::Result<Option<Vec<u8>>>;

    /// Write (create or overwrite) an object.
    fn write(&self, key: &ObjectKey, data: &[u8]) -> crate::error::Result<()>;

    /// Delete an object. Missing objects are not an error.
    fn delete(&self, key: &ObjectKey) -> crate::error::Result<()>;

    /// List objects under a key prefix. Prefixes are literal string prefixes
    /// of the logical key.
    fn list(&self, prefix: &str) -> crate::error::Result<Vec<ObjectMetadata>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_display() {
        assert_eq!(ObjectKey::new("novel/123/toc.yaml").to_string(), "novel/123/toc.yaml");
        assert_eq!(ObjectKey::new("a").as_ref(), "a");
    }
}
