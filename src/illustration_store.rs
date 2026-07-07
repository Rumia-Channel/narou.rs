use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if has_legacy_illustration_inputs(archive_path) {
                    Self::legacy_default()
                } else {
                    return Ok(Self::default());
                }
            }
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

fn has_legacy_illustration_inputs(archive_path: &Path) -> bool {
    archive_path.join("挿絵").is_dir() || archive_path.join(crate::downloader::RAW_DATA_DIR).is_dir()
}

/// Find files in `挿絵/` not reachable from the cache or `raw/*.html` references.
///
/// A file is considered reachable if:
/// - It appears as a value in `IllustrationStore::sources`, `mitemin_ids`, or `hashes`.
/// - Its stem is a key in `IllustrationStore::hashes` / `source_hashes` / `mitemin_hashes`
///   (i.e. the file is the canonical on-disk file for a known hash or mitemin ID).
/// - It is the on-disk file that `cached_filename_for_source` / `legacy_basename_from_source`
///   would resolve to for any `<img src>` URL referenced in `raw/*.html`.
///
/// All other files in `挿絵/` are returned as orphans. This is the source of truth used by
/// `narou illust orphan` for both dry-run listing and `-f` deletion.
pub fn find_orphan_illustrations(archive_path: &Path) -> Result<Vec<PathBuf>> {
    let illust_dir = archive_path.join("挿絵");
    if !illust_dir.is_dir() {
        return Ok(Vec::new());
    }

    let store = IllustrationStore::load(archive_path)?;
    let mut reachable: HashSet<String> = HashSet::new();
    for filename in store.sources.values() {
        if !filename.is_empty() {
            reachable.insert(filename.clone());
        }
    }
    for filename in store.mitemin_ids.values() {
        if !filename.is_empty() {
            reachable.insert(filename.clone());
        }
    }
    for filename in store.hashes.values() {
        if !filename.is_empty() {
            reachable.insert(filename.clone());
        }
    }

    let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);
    if raw_dir.is_dir() {
        for entry in std::fs::read_dir(&raw_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for source in extract_all_img_sources(&raw) {
                if let Some(filename) = store.cached_filename_for_source(&source, &illust_dir) {
                    reachable.insert(filename);
                    continue;
                }
                if let Some(basename) = legacy_basename_from_source(&source)
                    && let Some(filename) = find_saved_illustration_filename(&illust_dir, &basename)
                {
                    reachable.insert(filename);
                }
            }
        }
    }

    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_self_referencing_canonical(&store, filename) {
            continue;
        }
        if !reachable.contains(filename) {
            orphans.push(path);
        }
    }
    orphans.sort();
    Ok(orphans)
}

fn is_self_referencing_canonical(store: &IllustrationStore, filename: &str) -> bool {
    let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    // Hash-named file (64-char hex) is always self-referencing: the converter looks
    // for `挿絵/<hash>.<ext>` on disk during `cached_filename_for_hash` regardless of
    // whether the cache currently has a hash entry, so the file would be reused.
    if stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    // mitemin ID-named file is reachable if its stem is a known mitemin ID.
    if is_mitemin_id(stem) && store.mitemin_ids.contains_key(stem) {
        return true;
    }
    false
}

/// A planned legacy filename migration.
#[derive(Debug, Clone)]
pub struct IllustrationMigration {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    /// Source URL the file was linked to, if known. Used to refresh the cache.
    pub source: Option<String>,
}

/// Compute planned legacy filename migrations without touching the filesystem.
///
/// Migrations handled:
/// - mitemin legacy filenames (`<iNNNN>.<ext>` already canonical) are skipped.
/// - Section-index-count names like `16-0.jpg` are renamed to `<hash>.jpg`.
/// - Other non-canonical names (URL basename leftovers like `image001.jpg`) are hashed
///   and renamed to `<hash>.<ext>` when a corresponding raw HTML source URL is found.
/// - Files already in canonical naming (`<hex64>.<ext>` or `iNNNN.<ext>`) are left alone.
pub fn plan_legacy_illustration_migrations(
    archive_path: &Path,
) -> Result<Vec<IllustrationMigration>> {
    let illust_dir = archive_path.join("挿絵");
    if !illust_dir.is_dir() {
        return Ok(Vec::new());
    }

    let raw_sources = collect_raw_source_basename_map(archive_path);
    let mut plans = Vec::new();
    for entry in std::fs::read_dir(&illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_canonical_illustration_filename(filename) {
            continue;
        }
        let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Section-index-count legacy: "<index>-<count>".
        if stem.contains('-') && looks_like_section_index_count(stem) {
            if let Some((new_path, source)) =
                plan_section_index_count_migration(&path, stem, &raw_sources, &illust_dir)?
            {
                plans.push(IllustrationMigration {
                    old_path: path.clone(),
                    new_path,
                    source,
                });
            }
            continue;
        }
        // URL-basename legacy: match against any source's legacy basename.
        if let Some(source_entry) = raw_sources.stem_to_source.get(stem)
            && let Some(new_path) = plan_url_basename_migration(&path, &illust_dir)?
        {
            plans.push(IllustrationMigration {
                old_path: path.clone(),
                new_path,
                source: Some(source_entry.source.clone()),
            });
            continue;
        }
    }
    plans.sort_by(|a, b| a.old_path.cmp(&b.old_path));
    Ok(plans)
}

/// Apply planned legacy migrations: rename files on disk and update the cache.
/// Returns the number of files actually renamed (plans referencing missing files are skipped).
pub fn apply_legacy_illustration_migrations(archive_path: &Path) -> Result<usize> {
    let illust_dir = archive_path.join("挿絵");
    if !illust_dir.is_dir() {
        return Ok(0);
    }
    let plans = plan_legacy_illustration_migrations(archive_path)?;
    if plans.is_empty() {
        return Ok(0);
    }
    let mut store = IllustrationStore::load(archive_path)?;
    let mut renamed = 0usize;
    for plan in &plans {
        if !plan.old_path.is_file() {
            continue;
        }
        let Some(new_filename) = plan.new_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if plan.new_path.exists() {
            if hash_file(&plan.old_path).ok().as_deref()
                == hash_file(&plan.new_path).ok().as_deref()
            {
                let _ = std::fs::remove_file(&plan.old_path);
                renamed += 1;
                if let Some(source) = &plan.source {
                    let Ok(hash) = hash_file(&plan.new_path) else {
                        continue;
                    };
                    if let Some(id) = mitemin_illustration_id(source) {
                        store.remember_mitemin(source, &id, &hash, new_filename);
                    } else {
                        store.remember_hash_source(source, &hash, new_filename);
                    }
                }
            }
            continue;
        }
        std::fs::rename(&plan.old_path, &plan.new_path)?;
        renamed += 1;
        if let Some(source) = &plan.source {
            let Ok(hash) = hash_file(&plan.new_path) else {
                continue;
            };
            if let Some(id) = mitemin_illustration_id(source) {
                store.remember_mitemin(source, &id, &hash, new_filename);
            } else {
                store.remember_hash_source(source, &hash, new_filename);
            }
        }
    }
    if renamed > 0 {
        store.flush(archive_path)?;
    }
    Ok(renamed)
}

/// Detect image extensions from magic bytes and compute planned renames.
///
/// For each file in `挿絵/`, the actual format is detected from the leading bytes.
/// If the current extension does not match the detected format, a planned rename is returned.
pub fn plan_extension_fixes(illust_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    if !illust_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    for entry in std::fs::read_dir(illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Some(detected) = detect_image_extension(&bytes) else {
            continue;
        };
        let current_ext = Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if current_ext.eq_ignore_ascii_case(detected) {
            continue;
        }
        let stem = Path::new(filename).file_stem().and_then(|s| s.to_str()).unwrap_or(filename);
        let new_path = illust_dir.join(format!("{}.{}", stem, detected));
        if new_path == path {
            continue;
        }
        plans.push((path, new_path));
    }
    plans.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(plans)
}

/// Apply planned extension fixes: rename files. Returns count actually renamed.
pub fn apply_extension_fixes(
    illust_dir: &Path,
    fixes: &[(PathBuf, PathBuf)],
) -> Result<usize> {
    if !illust_dir.is_dir() {
        return Ok(0);
    }
    let mut renamed = 0usize;
    for (old_path, new_path) in fixes {
        if !old_path.is_file() || new_path.exists() {
            continue;
        }
        std::fs::rename(old_path, new_path)?;
        renamed += 1;
    }
    Ok(renamed)
}

/// Rebuild the `.illustration_cache.yaml` from scratch by scanning `挿絵/` + `raw/*.html`.
///
/// The rebuilt store is also flushed to disk. Returns the number of cache entries
/// (sources + mitemin_ids + hashes) recorded in the new cache.
pub fn rebuild_illustration_cache(archive_path: &Path) -> Result<usize> {
    let illust_dir = archive_path.join("挿絵");
    if !illust_dir.is_dir() {
        // Even when the directory is missing, flush an empty store so users can recover.
        let mut store = IllustrationStore::default();
        store.flush(archive_path)?;
        return Ok(0);
    }

    let mut store = IllustrationStore::default();
    let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);

    // Walk illust_dir and hash each file.
    let mut hashes: BTreeMap<String, String> = BTreeMap::new();
    for entry in std::fs::read_dir(&illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let hash = hash_bytes(&bytes);
        if let Some(existing) = hashes.get(&hash) {
            // Same content under a different name: keep the first, drop duplicates later via orphan.
            if existing != &filename {
                // Skip recording duplicate; orphan detection will surface it.
            }
            continue;
        }
        hashes.insert(hash.clone(), filename.to_string());

        if let Some(id) = filename
            .strip_suffix(&format!(
                ".{}",
                Path::new(filename)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
            ))
            .and_then(|stem| stem.strip_prefix('i'))
            .and_then(|digits| {
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    Some(format!("i{digits}"))
                } else {
                    None
                }
            })
        {
            store.remember_mitemin("", &id, &hash, filename);
        }
    }

    // Walk raw/*.html and link sources to the saved file we just hashed.
    if raw_dir.is_dir() {
        for entry in std::fs::read_dir(&raw_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let section_index = raw_section_index(&path);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (illust_index, source) in extract_all_img_sources(&raw).into_iter().enumerate() {
                let normalized = normalize_illustration_url(&source);
                if let Some(filename) = resolve_existing_filename(
                    &normalized,
                    section_index.as_deref(),
                    illust_index,
                    &illust_dir,
                ) {
                    let hash = hash_file(&illust_dir.join(&filename))?;
                    if let Some(id) = mitemin_illustration_id(&source) {
                        store.remember_mitemin(&source, &id, &hash, &filename);
                    } else {
                        store.remember_hash_source(&source, &hash, &filename);
                    }
                }
            }
        }
    }

    let entry_count =
        store.sources.len() + store.mitemin_ids.len() + store.hashes.len();
    store.flush(archive_path)?;
    Ok(entry_count)
}

/// Detect image format from magic bytes. Returns the canonical extension
/// (`jpg`, `png`, `gif`, `webp`, `bmp`) when the leading bytes match a known signature.
pub fn detect_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("jpg");
    }
    if bytes.len() >= 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
        && bytes[4] == 0x0D
        && bytes[5] == 0x0A
        && bytes[6] == 0x1A
        && bytes[7] == 0x0A
    {
        return Some("png");
    }
    if bytes.len() >= 4
        && bytes[0] == 0x47
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x38
    {
        return Some("gif");
    }
    if bytes.len() >= 12
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
    {
        return Some("webp");
    }
    if bytes.len() >= 2 && bytes[0] == 0x42 && bytes[1] == 0x4D {
        return Some("bmp");
    }
    None
}

fn is_canonical_illustration_filename(filename: &str) -> bool {
    let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    // 64-char hex (SHA-256) or mitemin ID like i12345.
    (stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()))
        || is_mitemin_id(stem)
}

fn looks_like_section_index_count(stem: &str) -> bool {
    let mut parts = stem.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(second) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    !first.is_empty()
        && first.chars().all(|c| c.is_ascii_digit())
        && !second.is_empty()
        && second.chars().all(|c| c.is_ascii_digit())
}

struct RawSourceMap {
    /// stem -> (normalized_source_url, raw_html_path, illust_index)
    stem_to_source: BTreeMap<String, RawSourceEntry>,
}

struct RawSourceEntry {
    source: String,
}

fn collect_raw_source_basename_map(archive_path: &Path) -> RawSourceMap {
    let mut map = RawSourceMap {
        stem_to_source: BTreeMap::new(),
    };
    let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);
    let Ok(entries) = std::fs::read_dir(&raw_dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        for source in extract_all_img_sources(&raw) {
            if let Some(basename) = legacy_basename_from_source(&source)
                && !basename.is_empty()
                && !map.stem_to_source.contains_key(&basename)
            {
                map.stem_to_source.insert(
                    basename,
                    RawSourceEntry {
                        source: source.clone(),
                    },
                );
            }
        }
    }
    map
}

fn plan_section_index_count_migration(
    old_path: &Path,
    stem: &str,
    raw_sources: &RawSourceMap,
    illust_dir: &Path,
) -> Result<Option<(PathBuf, Option<String>)>> {
    let Ok(bytes) = std::fs::read(old_path) else {
        return Ok(None);
    };
    let hash = hash_bytes(&bytes);
    let ext = old_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let new_filename = format!("{}.{}", hash, normalize_extension(ext));
    let new_path = illust_dir.join(&new_filename);
    if new_path == *old_path {
        return Ok(None);
    }
    // Try to find a raw HTML source whose stem matches the legacy basename, so the
    // cache can be updated after migration.
    let source = raw_sources.stem_to_source.get(stem).map(|e| e.source.clone());
    Ok(Some((new_path, source)))
}

fn plan_url_basename_migration(
    old_path: &Path,
    illust_dir: &Path,
) -> Result<Option<PathBuf>> {
    let Ok(bytes) = std::fs::read(old_path) else {
        return Ok(None);
    };
    let hash = hash_bytes(&bytes);
    let ext = old_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let new_filename = format!("{}.{}", hash, normalize_extension(ext));
    let new_path = illust_dir.join(&new_filename);
    if new_path == *old_path {
        Ok(None)
    } else {
        Ok(Some(new_path))
    }
}

fn resolve_existing_filename(
    source: &str,
    section_index: Option<&str>,
    illust_index: usize,
    illust_dir: &Path,
) -> Option<String> {
    if let Some(id) = mitemin_illustration_id(source)
        && let Some(filename) = find_saved_illustration_filename(illust_dir, &id)
    {
        return Some(filename);
    }
    if let Some(basename) = legacy_basename_from_source(source)
        && let Some(filename) = find_saved_illustration_filename(illust_dir, &basename)
    {
        return Some(filename);
    }
    if let Some(index) = section_index {
        let candidate = format!("{}-{}", index, illust_index);
        if let Some(filename) = find_saved_illustration_filename(illust_dir, &candidate) {
            return Some(filename);
        }
    }
    None
}

fn extract_all_img_sources(raw: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"(?is)<img\b[^>]*\bsrc=["']([^"']+)["']"#).unwrap();
    re.captures_iter(raw)
        .filter_map(|caps| caps.get(1).map(|m| normalize_illustration_url(m.as_str())))
        .collect()
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
    fn load_without_cache_or_illustration_inputs_does_not_create_cache_file() {
        let temp = tempfile::tempdir().unwrap();

        let store = IllustrationStore::load(temp.path()).unwrap();

        assert_eq!(store.version, STORE_VERSION);
        assert!(!cache_path(temp.path()).exists());
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

    fn png_bytes(seed: u8) -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend(std::iter::repeat(seed).take(32));
        bytes
    }

    #[test]
    fn find_orphan_illustrations_marks_unreachable_files() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let bytes = png_bytes(0x42);
        let hash = hash_bytes(&bytes);
        let hash_filename = format!("{}.png", hash);
        std::fs::write(illust_dir.join(&hash_filename), &bytes).unwrap();
        std::fs::write(illust_dir.join("stray.png"), &bytes).unwrap();

        let orphans = find_orphan_illustrations(temp.path()).unwrap();
        let names: Vec<String> = orphans
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["stray.png".to_string()]);
    }

    #[test]
    fn find_orphan_illustrations_returns_empty_when_no_illust_dir() {
        let temp = tempfile::tempdir().unwrap();
        assert!(find_orphan_illustrations(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn plan_legacy_migrations_detects_section_index_count_and_url_basename() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let raw_dir = temp.path().join(crate::downloader::RAW_DATA_DIR);
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();
        let bytes = png_bytes(0x33);
        std::fs::write(illust_dir.join("16-0.png"), &bytes).unwrap();
        std::fs::write(illust_dir.join("image001.png"), &bytes).unwrap();
        std::fs::write(
            raw_dir.join("16 subtitle.html"),
            r#"<p><img src="https://example.com/path/image001.png" /></p>"#,
        )
        .unwrap();

        let plans = plan_legacy_illustration_migrations(temp.path()).unwrap();
        let stems: Vec<String> = plans
            .iter()
            .map(|p| {
                p.old_path
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(stems.contains(&"16-0".to_string()));
        assert!(stems.contains(&"image001".to_string()));
        for plan in &plans {
            let ext = plan.old_path.extension().unwrap().to_string_lossy().to_string();
            assert_eq!(
                plan.new_path.extension().unwrap().to_string_lossy().to_string(),
                ext
            );
            assert_eq!(plan.new_path.file_name().unwrap().to_string_lossy().len(), 64 + 1 + ext.len());
        }
    }

    #[test]
    fn apply_legacy_migrations_renames_files_and_updates_cache() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let raw_dir = temp.path().join(crate::downloader::RAW_DATA_DIR);
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();
        let bytes = png_bytes(0x55);
        std::fs::write(illust_dir.join("image001.png"), &bytes).unwrap();
        std::fs::write(
            raw_dir.join("16 subtitle.html"),
            r#"<p><img src="https://example.com/path/image001.png" /></p>"#,
        )
        .unwrap();

        let renamed = apply_legacy_illustration_migrations(temp.path()).unwrap();
        assert_eq!(renamed, 1);
        assert!(!illust_dir.join("image001.png").exists());
        let new_filename = std::fs::read_dir(&illust_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find_map(|e| e.file_name().into_string().ok())
            .expect("renamed file present");
        assert!(new_filename.ends_with(".png"));

        let store = IllustrationStore::load(temp.path()).unwrap();
        assert_eq!(
            store
                .cached_filename_for_source("https://example.com/path/image001.png", &illust_dir)
                .as_deref(),
            Some(new_filename.as_str())
        );
    }

    #[test]
    fn plan_extension_fixes_detects_mismatches_from_magic_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let png = png_bytes(0x77);
        std::fs::write(illust_dir.join("cover.jpg"), &png).unwrap();
        std::fs::write(illust_dir.join("ok.png"), &png).unwrap();

        let plans = plan_extension_fixes(&illust_dir).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].0.file_name().unwrap().to_string_lossy(),
            "cover.jpg"
        );
        assert_eq!(
            plans[0].1.file_name().unwrap().to_string_lossy(),
            "cover.png"
        );
    }

    #[test]
    fn apply_extension_fixes_renames_files() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let png = png_bytes(0x88);
        std::fs::write(illust_dir.join("mismatch.jpg"), &png).unwrap();

        let plans = plan_extension_fixes(&illust_dir).unwrap();
        assert_eq!(plans.len(), 1);
        let renamed = apply_extension_fixes(&illust_dir, &plans).unwrap();
        assert_eq!(renamed, 1);
        assert!(!illust_dir.join("mismatch.jpg").exists());
        assert!(illust_dir.join("mismatch.png").is_file());
    }

    #[test]
    fn detect_image_extension_returns_none_for_unknown_format() {
        assert_eq!(detect_image_extension(b"hello world"), None);
        assert_eq!(detect_image_extension(&[]), None);
    }

    #[test]
    fn rebuild_illustration_cache_persists_hash_and_source_mappings() {
        let temp = tempfile::tempdir().unwrap();
        let illust_dir = temp.path().join("挿絵");
        let raw_dir = temp.path().join(crate::downloader::RAW_DATA_DIR);
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();
        let bytes = png_bytes(0xAB);
        std::fs::write(illust_dir.join("16-0.png"), &bytes).unwrap();
        std::fs::write(
            raw_dir.join("16 subtitle.html"),
            r#"<p><img src="https://example.com/path/16-0.png" /></p>"#,
        )
        .unwrap();

        let count = rebuild_illustration_cache(temp.path()).unwrap();
        assert!(count > 0);
        let cache_file = cache_path(temp.path());
        assert!(cache_file.is_file());
    }
}
