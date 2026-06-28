use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;

const CACHE_FILE_NAME: &str = ".illustration_cache.yaml";
const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IllustrationStore {
    #[serde(default = "default_store_version")]
    version: u32,
    #[serde(default)]
    sources: BTreeMap<String, String>,
    #[serde(default)]
    mitemin_ids: BTreeMap<String, String>,
    #[serde(default)]
    hashes: BTreeMap<String, String>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIllustration {
    pub filename: String,
    pub hash: String,
    pub created: bool,
}

impl Default for IllustrationStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            sources: BTreeMap::new(),
            mitemin_ids: BTreeMap::new(),
            hashes: BTreeMap::new(),
            dirty: false,
        }
    }
}

impl IllustrationStore {
    pub fn load(archive_path: &Path) -> Result<Self> {
        let path = cache_path(archive_path);
        match std::fs::read_to_string(&path) {
            Ok(raw) if !raw.trim().is_empty() => {
                let mut store: Self = serde_yaml::from_str(&raw)?;
                if store.version == 0 {
                    store.version = STORE_VERSION;
                }
                store.dirty = false;
                Ok(store)
            }
            Ok(_) => Ok(Self::default()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn flush(&mut self, archive_path: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let path = cache_path(archive_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut content = serde_yaml::to_string(self)?;
        if content.starts_with("---\n") {
            content.drain(..4);
        }
        crate::db::inventory::atomic_write(&path, &content)?;
        self.dirty = false;
        Ok(())
    }

    pub fn cached_filename_for_source(&self, source: &str, illust_dir: &Path) -> Option<String> {
        if let Some(id) = mitemin_illustration_id(source)
            && let Some(filename) = self.mitemin_ids.get(&id)
            && saved_filename_exists(illust_dir, filename)
        {
            return Some(filename.clone());
        }

        let normalized = normalize_illustration_url(source);
        self.sources
            .get(&normalized)
            .filter(|filename| saved_filename_exists(illust_dir, filename))
            .cloned()
    }

    pub fn cached_filename_for_hash(&self, hash: &str, illust_dir: &Path) -> Option<String> {
        if let Some(filename) = self.hashes.get(hash)
            && saved_filename_exists(illust_dir, filename)
        {
            return Some(filename.clone());
        }
        find_saved_illustration_filename(illust_dir, hash)
    }

    pub fn store_bytes(
        &mut self,
        illust_dir: &Path,
        source: &str,
        bytes: &[u8],
        ext: &str,
    ) -> Result<StoredIllustration> {
        let hash = hash_bytes(bytes);
        if let Some(filename) = self.cached_filename_for_hash(&hash, illust_dir) {
            self.remember(source, &hash, &filename);
            return Ok(StoredIllustration {
                filename,
                hash,
                created: false,
            });
        }

        std::fs::create_dir_all(illust_dir)?;
        let filename = format!("{}.{}", hash, normalize_extension(ext));
        let path = illust_dir.join(&filename);
        let created = if path.exists() {
            false
        } else {
            std::fs::write(&path, bytes)?;
            true
        };
        self.remember(source, &hash, &filename);
        Ok(StoredIllustration {
            filename,
            hash,
            created,
        })
    }

    pub fn store_existing_file_as_hash(
        &mut self,
        illust_dir: &Path,
        source: &str,
        filename: &str,
    ) -> Result<StoredIllustration> {
        let bytes = std::fs::read(illust_dir.join(filename))?;
        let ext = Path::new(filename)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("bin");
        self.store_bytes(illust_dir, source, &bytes, ext)
    }

    pub fn remember(&mut self, source: &str, hash: &str, filename: &str) {
        self.version = STORE_VERSION;
        let normalized = normalize_illustration_url(source);
        self.remember_source(&normalized, filename);
        if let Some(id) = mitemin_illustration_id(source) {
            self.remember_mitemin_id(&id, filename);
        }
        self.remember_hash(hash, filename);
    }

    pub fn filename_for_source(&self, source: &str) -> Option<&str> {
        self.sources
            .get(&normalize_illustration_url(source))
            .map(String::as_str)
    }

    pub fn filename_for_mitemin_id(&self, id: &str) -> Option<&str> {
        self.mitemin_ids.get(id).map(String::as_str)
    }

    pub fn filename_for_hash(&self, hash: &str) -> Option<&str> {
        self.hashes.get(hash).map(String::as_str)
    }

    fn remember_source(&mut self, source: &str, filename: &str) {
        if self.sources.get(source).map(String::as_str) != Some(filename) {
            self.sources.insert(source.to_string(), filename.to_string());
            self.dirty = true;
        }
    }

    fn remember_mitemin_id(&mut self, id: &str, filename: &str) {
        if self.mitemin_ids.get(id).map(String::as_str) != Some(filename) {
            self.mitemin_ids.insert(id.to_string(), filename.to_string());
            self.dirty = true;
        }
    }

    fn remember_hash(&mut self, hash: &str, filename: &str) {
        if self.hashes.get(hash).map(String::as_str) != Some(filename) {
            self.hashes.insert(hash.to_string(), filename.to_string());
            self.dirty = true;
        }
    }
}

pub fn cache_path(archive_path: &Path) -> std::path::PathBuf {
    archive_path.join(CACHE_FILE_NAME)
}

pub fn normalize_illustration_url(source: &str) -> String {
    let prefixed = if source.starts_with("//") {
        format!("https:{}", source)
    } else {
        source.to_string()
    };
    if prefixed.contains(".mitemin.net") {
        prefixed.replace("viewimagebig", "viewimage")
    } else {
        prefixed
    }
}

pub fn mitemin_illustration_id(source: &str) -> Option<String> {
    let normalized = normalize_illustration_url(source);
    let parsed = reqwest::Url::parse(&normalized).ok()?;
    let host = parsed.host_str()?;
    if !host.ends_with(".mitemin.net") {
        return None;
    }
    parsed
        .path_segments()?
        .find(|part| is_mitemin_id(part))
        .map(ToString::to_string)
}

pub fn legacy_basename_from_source(source: &str) -> Option<String> {
    if let Some(id) = mitemin_illustration_id(source) {
        return Some(id);
    }
    let normalized = normalize_illustration_url(source);
    let parsed = reqwest::Url::parse(&normalized).ok()?;
    let segment = parsed
        .path_segments()?
        .filter(|part| !part.is_empty())
        .next_back()?;
    let stem = segment
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(segment);
    let sanitized = crate::downloader::util::sanitize_filename(stem);
    (!sanitized.is_empty()).then_some(sanitized)
}

pub fn find_saved_illustration_filename(illust_dir: &Path, basename: &str) -> Option<String> {
    let prefix = format!("{}.", basename);
    let mut filenames: Vec<String> = std::fs::read_dir(illust_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|filename| filename.starts_with(&prefix))
        .collect();
    filenames.sort();
    filenames.into_iter().next()
}

pub fn is_remote_illustration_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://") || source.starts_with("//")
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn guessed_extension_from_url(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    if lower.contains(".png") {
        "png"
    } else if lower.contains(".gif") {
        "gif"
    } else if lower.contains(".webp") {
        "webp"
    } else if lower.contains(".bmp") {
        "bmp"
    } else {
        "jpg"
    }
}

fn default_store_version() -> u32 {
    STORE_VERSION
}

fn is_mitemin_id(segment: &str) -> bool {
    segment
        .strip_prefix('i')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

fn saved_filename_exists(illust_dir: &Path, filename: &str) -> bool {
    !filename.is_empty()
        && !filename.contains('/')
        && !filename.contains('\\')
        && illust_dir.join(filename).is_file()
}

fn normalize_extension(ext: &str) -> String {
    let normalized: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if normalized.is_empty() {
        "bin".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_bytes_uses_hash_filename_and_mitemin_id_table() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let source = "https://29644.mitemin.net/userpageimage/viewimagebig/icode/i422674/";
        let mut store = IllustrationStore::default();

        let stored = store
            .store_bytes(&illust_dir, source, b"dummy", "jpg")
            .unwrap();
        let expected = format!("{}.jpg", hash_bytes(b"dummy"));

        assert_eq!(stored.filename, expected);
        assert!(stored.created);
        assert!(illust_dir.join(&expected).is_file());
        assert_eq!(store.filename_for_mitemin_id("i422674"), Some(expected.as_str()));
        assert_eq!(store.filename_for_hash(&hash_bytes(b"dummy")), Some(expected.as_str()));
        assert_eq!(
            store.cached_filename_for_source(source, &illust_dir).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn store_bytes_reuses_hash_for_non_mitemin_sources() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let mut store = IllustrationStore::default();

        let first = store
            .store_bytes(&illust_dir, "https://example.com/a.jpg", b"same", "jpg")
            .unwrap();
        let second = store
            .store_bytes(&illust_dir, "https://cdn.example.com/b.png", b"same", "png")
            .unwrap();

        assert_eq!(first.filename, second.filename);
        assert!(first.created);
        assert!(!second.created);
        assert_eq!(
            store
                .cached_filename_for_source("https://cdn.example.com/b.png", &illust_dir)
                .as_deref(),
            Some(first.filename.as_str())
        );
    }
}
