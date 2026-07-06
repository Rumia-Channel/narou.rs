use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;

const CACHE_FILE_NAME: &str = ".illustration_cache.yaml";
const LEGACY_STORE_VERSION: u32 = 1;
const STORE_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IllustrationStore {
    #[serde(default = "default_store_version")]
    version: u32,
    #[serde(default)]
    sources: BTreeMap<String, String>,
    #[serde(default)]
    mitemin_ids: BTreeMap<String, String>,
    #[serde(default)]
    mitemin_hashes: BTreeMap<String, String>,
    #[serde(default)]
    source_hashes: BTreeMap<String, String>,
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
            mitemin_hashes: BTreeMap::new(),
            source_hashes: BTreeMap::new(),
            hashes: BTreeMap::new(),
            dirty: false,
        }
    }
}

impl IllustrationStore {
    pub fn load(archive_path: &Path) -> Result<Self> {
        let path = cache_path(archive_path);
        let mut store = match std::fs::read_to_string(&path) {
            Ok(raw) if !raw.trim().is_empty() => {
                let mut store: Self = serde_yaml::from_str(&raw)?;
                if store.version == 0 {
                    store.version = LEGACY_STORE_VERSION;
                }
                store.dirty = false;
                store
            }
            Ok(_) => Self::legacy_default(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::legacy_default(),
            Err(err) => return Err(err.into()),
        };
        let _ = store.migrate_mitemin_id_filenames(archive_path);
        let _ = store.flush(archive_path);
        Ok(store)
    }

    fn legacy_default() -> Self {
        Self {
            version: LEGACY_STORE_VERSION,
            dirty: true,
            ..Self::default()
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
            && filename_has_basename(filename, &id)
        {
            return Some(filename.clone());
        }

        if mitemin_illustration_id(source).is_some() {
            return None;
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
        if let Some(id) = mitemin_illustration_id(source) {
            if let Some(filename) = find_saved_illustration_filename(illust_dir, &id) {
                self.remember_mitemin(source, &id, &hash, &filename);
                return Ok(StoredIllustration {
                    filename,
                    hash,
                    created: false,
                });
            }

            std::fs::create_dir_all(illust_dir)?;
            let filename = format!("{}.{}", id, normalize_extension(ext));
            let path = illust_dir.join(&filename);
            let created = if path.exists() {
                false
            } else {
                std::fs::write(&path, bytes)?;
                true
            };
            self.remember_mitemin(source, &id, &hash, &filename);
            return Ok(StoredIllustration {
                filename,
                hash,
                created,
            });
        }

        if let Some(filename) = self.cached_filename_for_hash(&hash, illust_dir) {
            self.remember_hash_source(source, &hash, &filename);
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
        self.remember_hash_source(source, &hash, &filename);
        Ok(StoredIllustration {
            filename,
            hash,
            created,
        })
    }

    pub fn store_existing_file(
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
        let hash = hash_bytes(&bytes);
        if let Some(id) = mitemin_illustration_id(source)
            && filename_has_basename(filename, &id)
        {
            self.remember_mitemin(source, &id, &hash, filename);
            return Ok(StoredIllustration {
                filename: filename.to_string(),
                hash,
                created: false,
            });
        }
        self.store_bytes(illust_dir, source, &bytes, ext)
    }

    pub fn remember_hash_source(&mut self, source: &str, hash: &str, filename: &str) {
        self.version = STORE_VERSION;
        let normalized = normalize_illustration_url(source);
        self.remember_source(&normalized, filename);
        self.remember_source_hash(&normalized, hash);
        self.remember_hash(hash, filename);
    }

    pub fn remember_mitemin(&mut self, source: &str, id: &str, hash: &str, filename: &str) {
        self.version = STORE_VERSION;
        let normalized = normalize_illustration_url(source);
        self.remember_source(&normalized, filename);
        self.remember_source_hash(&normalized, hash);
        self.remember_mitemin_id(id, filename);
        self.remember_mitemin_hash(id, hash);
    }

    pub fn filename_for_source(&self, source: &str) -> Option<&str> {
        self.sources
            .get(&normalize_illustration_url(source))
            .map(String::as_str)
    }

    pub fn filename_for_mitemin_id(&self, id: &str) -> Option<&str> {
        self.mitemin_ids.get(id).map(String::as_str)
    }

    pub fn hash_for_mitemin_id(&self, id: &str) -> Option<&str> {
        self.mitemin_hashes.get(id).map(String::as_str)
    }

    pub fn filename_for_hash(&self, hash: &str) -> Option<&str> {
        self.hashes.get(hash).map(String::as_str)
    }

    pub fn migrate_mitemin_id_filenames(&mut self, archive_path: &Path) -> Result<usize> {
        if self.version >= STORE_VERSION {
            return Ok(0);
        }

        let illust_dir = archive_path.join("挿絵");
        let mut migrated = 0usize;
        if illust_dir.is_dir() {
            let cached_sources: Vec<(String, String)> = self
                .sources
                .iter()
                .map(|(source, filename)| (source.clone(), filename.clone()))
                .collect();
            for (source, filename) in cached_sources {
                if let Some((target, hash)) =
                    migrate_mitemin_filename(&illust_dir, &source, &filename)?
                {
                    if target != filename {
                        migrated += 1;
                    }
                    if let Some(id) = mitemin_illustration_id(&source) {
                        self.remember_mitemin(&source, &id, &hash, &target);
                    }
                }
            }
            migrated += self.migrate_mitemin_filenames_from_raw(archive_path, &illust_dir)?;
        }

        if self.version != STORE_VERSION {
            self.version = STORE_VERSION;
            self.dirty = true;
        }
        Ok(migrated)
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

    fn remember_mitemin_hash(&mut self, id: &str, hash: &str) {
        if self.mitemin_hashes.get(id).map(String::as_str) != Some(hash) {
            self.mitemin_hashes.insert(id.to_string(), hash.to_string());
            self.dirty = true;
        }
    }

    fn remember_source_hash(&mut self, source: &str, hash: &str) {
        if self.source_hashes.get(source).map(String::as_str) != Some(hash) {
            self.source_hashes
                .insert(source.to_string(), hash.to_string());
            self.dirty = true;
        }
    }

    fn remember_hash(&mut self, hash: &str, filename: &str) {
        if self.hashes.get(hash).map(String::as_str) != Some(filename) {
            self.hashes.insert(hash.to_string(), filename.to_string());
            self.dirty = true;
        }
    }

    fn migrate_mitemin_filenames_from_raw(
        &mut self,
        archive_path: &Path,
        illust_dir: &Path,
    ) -> Result<usize> {
        let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);
        if !raw_dir.is_dir() {
            return Ok(0);
        }

        let mut migrated = 0usize;
        for entry in std::fs::read_dir(raw_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("html") {
                continue;
            }
            let Some(section_index) = raw_section_index(&path) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (illust_index, source) in extract_mitemin_img_sources(&raw).into_iter().enumerate()
            {
                let Some(id) = mitemin_illustration_id(&source) else {
                    continue;
                };
                let basename = format!("{}-{}", section_index, illust_index);
                if let Some(filename) = find_saved_illustration_filename(illust_dir, &basename) {
                    if let Some((target, hash)) =
                        migrate_file_to_mitemin_id(illust_dir, &id, &filename)?
                    {
                        if target != filename {
                            migrated += 1;
                        }
                        self.remember_mitemin(&source, &id, &hash, &target);
                    }
                } else if let Some(filename) = find_saved_illustration_filename(illust_dir, &id) {
                    let hash = match self.mitemin_hashes.get(&id) {
                        Some(hash) => hash.clone(),
                        None => hash_file(&illust_dir.join(&filename))?,
                    };
                    self.remember_mitemin(&source, &id, &hash, &filename);
                }
            }
        }
        Ok(migrated)
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
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()?
                .next_back()
                .and_then(extension_from_path_segment)
        })
        .unwrap_or("jpg")
}

pub fn illustration_extension_from_content_type(content_type: &str) -> Option<&'static str> {
    match content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn extension_from_path_segment(segment: &str) -> Option<&'static str> {
    let ext = segment.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("jpg"),
        "png" => Some("png"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "bmp" => Some("bmp"),
        _ => None,
    }
}

fn migrate_mitemin_filename(
    illust_dir: &Path,
    source: &str,
    filename: &str,
) -> Result<Option<(String, String)>> {
    let Some(id) = mitemin_illustration_id(source) else {
        return Ok(None);
    };
    migrate_file_to_mitemin_id(illust_dir, &id, filename)
}

fn migrate_file_to_mitemin_id(
    illust_dir: &Path,
    id: &str,
    filename: &str,
) -> Result<Option<(String, String)>> {
    if !saved_filename_exists(illust_dir, filename) {
        return Ok(None);
    }
    let old_path = illust_dir.join(filename);
    let hash = hash_file(&old_path)?;
    if filename_has_basename(filename, id) {
        return Ok(Some((filename.to_string(), hash)));
    }

    let ext = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin");
    let target = format!("{}.{}", id, normalize_extension(ext));
    let target_path = illust_dir.join(&target);
    if target_path.is_file() {
        if hash_file(&target_path).ok().as_deref() == Some(hash.as_str()) {
            let _ = std::fs::remove_file(&old_path);
            return Ok(Some((target, hash)));
        }
        return Ok(None);
    }

    std::fs::rename(&old_path, &target_path)?;
    Ok(Some((target, hash)))
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hash_bytes(&bytes))
}

fn raw_section_index(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let index = stem.split_whitespace().next().unwrap_or(stem).trim();
    (!index.is_empty()).then_some(index.to_string())
}

fn extract_mitemin_img_sources(raw: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"(?is)<img\b[^>]*\bsrc=["']([^"']+)["']"#).unwrap();
    re.captures_iter(raw)
        .filter_map(|caps| caps.get(1).map(|m| normalize_illustration_url(m.as_str())))
        .filter(|source| mitemin_illustration_id(source).is_some())
        .collect()
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

fn filename_has_basename(filename: &str, basename: &str) -> bool {
    Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        == Some(basename)
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
    fn store_bytes_uses_mitemin_id_filename_and_hash_table() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let source = "https://29644.mitemin.net/userpageimage/viewimagebig/icode/i422674/";
        let mut store = IllustrationStore::default();

        let stored = store
            .store_bytes(&illust_dir, source, b"dummy", "jpg")
            .unwrap();
        let hash = hash_bytes(b"dummy");
        let expected = "i422674.jpg";

        assert_eq!(stored.filename, expected);
        assert!(stored.created);
        assert!(illust_dir.join(expected).is_file());
        assert_eq!(store.filename_for_mitemin_id("i422674"), Some(expected));
        assert_eq!(store.hash_for_mitemin_id("i422674"), Some(hash.as_str()));
        assert_eq!(
            store.cached_filename_for_source(source, &illust_dir).as_deref(),
            Some(expected)
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

    #[test]
    fn migrate_mitemin_id_filenames_uses_raw_img_order_for_legacy_section_names() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let raw_dir = temp.path().join(crate::downloader::RAW_DATA_DIR);
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();
        std::fs::write(illust_dir.join("16-0.jpg"), b"dummy").unwrap();
        std::fs::write(
            raw_dir.join("16 subtitle.html"),
            r#"<p><img src="//29644.mitemin.net/userpageimage/viewimagebig/icode/i422674/" /></p>"#,
        )
        .unwrap();

        let mut store = IllustrationStore {
            version: LEGACY_STORE_VERSION,
            ..IllustrationStore::default()
        };
        let migrated = store.migrate_mitemin_id_filenames(temp.path()).unwrap();

        assert_eq!(migrated, 1);
        assert!(!illust_dir.join("16-0.jpg").exists());
        assert!(illust_dir.join("i422674.jpg").is_file());
        assert_eq!(store.filename_for_mitemin_id("i422674"), Some("i422674.jpg"));
        assert_eq!(store.hash_for_mitemin_id("i422674"), Some(hash_bytes(b"dummy").as_str()));
    }

    #[test]
    fn load_migrates_cached_mitemin_hash_filename_to_id_filename() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let hash = hash_bytes(b"dummy");
        let hash_filename = format!("{hash}.jpg");
        std::fs::write(illust_dir.join(&hash_filename), b"dummy").unwrap();
        std::fs::write(
            cache_path(temp.path()),
            format!(
                "version: 1\nsources:\n  https://29644.mitemin.net/userpageimage/viewimage/icode/i422674/: {hash_filename}\n"
            ),
        )
        .unwrap();

        let store = IllustrationStore::load(temp.path()).unwrap();

        assert!(!illust_dir.join(&hash_filename).exists());
        assert!(illust_dir.join("i422674.jpg").is_file());
        assert_eq!(store.filename_for_mitemin_id("i422674"), Some("i422674.jpg"));
        assert_eq!(store.hash_for_mitemin_id("i422674"), Some(hash.as_str()));
    }

    #[test]
    fn load_skips_raw_migration_after_store_version_is_current() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let raw_dir = temp.path().join(crate::downloader::RAW_DATA_DIR);
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();
        std::fs::write(illust_dir.join("16-0.jpg"), b"dummy").unwrap();
        std::fs::write(
            raw_dir.join("16 subtitle.html"),
            r#"<p><img src="//29644.mitemin.net/userpageimage/viewimagebig/icode/i422674/" /></p>"#,
        )
        .unwrap();
        std::fs::write(cache_path(temp.path()), "version: 2\n").unwrap();

        let store = IllustrationStore::load(temp.path()).unwrap();

        assert!(illust_dir.join("16-0.jpg").is_file());
        assert_eq!(store.filename_for_mitemin_id("i422674"), None);
    }

    #[test]
    fn extension_detection_prefers_content_type_and_url_path() {
        assert_eq!(
            illustration_extension_from_content_type("image/png; charset=binary"),
            Some("png")
        );
        assert_eq!(
            guessed_extension_from_url("https://example.com/image.jpg?format=.png"),
            "jpg"
        );
        assert_eq!(
            guessed_extension_from_url("https://example.com/view/without-extension"),
            "jpg"
        );
    }
}
