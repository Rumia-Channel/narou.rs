use std::collections::BTreeMap;
#[cfg(feature = "native-runtime")]
use std::collections::HashSet;
#[cfg(feature = "native-runtime")]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::application::novel_access::NovelWriteGuard;
use crate::platform::{AssetStore, AssetStream, NovelObjectKeys, ObjectKey, StagedAsset};

#[cfg(feature = "native-runtime")]
const CACHE_FILE_NAME: &str = ".illustration_cache.yaml";
#[cfg(feature = "native-runtime")]
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

/// Pure metadata index for illustration URL/hash-to-filename mappings.
///
/// The transparent representation preserves the legacy flat
/// `.illustration_cache.yaml` schema. Filesystem compatibility methods remain
/// on `IllustrationStore`; this type exposes only metadata operations to core
/// storage services.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Default)]
pub struct IllustrationIndex(IllustrationStore);


impl IllustrationIndex {
    pub fn filename_for_source(&self, source: &str) -> Option<&str> {
        self.0.filename_for_source(source)
    }

    pub fn filename_for_mitemin_id(&self, id: &str) -> Option<&str> {
        self.0.filename_for_mitemin_id(id)
    }

    pub fn filename_for_hash(&self, hash: &str) -> Option<&str> {
        self.0.filename_for_hash(hash)
    }

    /// 索引が持つ取得元 (URL) の件数。デバッグ表示用。
    pub fn source_count(&self) -> usize {
        self.0.sources.len()
    }

    /// 索引が知っているファイル名 (重複なし、昇順)。
    ///
    /// 実体の確認 (`stat`) のように、一覧 (LIST) を使わずに保存物を突き合わせたい
    /// ときに使う。ストアの一覧が prefix の解釈差などで空を返す環境でも、保存側と
    /// 同じキー生成で 1 枚ずつ確かめられる。
    pub fn filenames(&self) -> std::collections::BTreeSet<String> {
        self.0
            .sources
            .values()
            .chain(self.0.mitemin_ids.values())
            .chain(self.0.hashes.values())
            .cloned()
            .collect()
    }

    pub fn hash_for_mitemin_id(&self, id: &str) -> Option<&str> {
        self.0.hash_for_mitemin_id(id)
    }

    pub fn remember_hash_source(&mut self, source: &str, hash: &str, filename: &str) {
        self.0.remember_hash_source(source, hash, filename);
    }

    pub fn remember_mitemin(&mut self, source: &str, id: &str, hash: &str, filename: &str) {
        self.0.remember_mitemin(source, id, hash, filename);
    }

    pub fn to_cache_yaml(&self) -> Result<Vec<u8>> {
        let mut content = serde_yaml::to_string(self)?;
        if content.starts_with("---\n") {
            content.drain(..4);
        }
        Ok(content.into_bytes())
    }

    pub fn mark_clean(&mut self) {
        self.0.mark_clean();
    }

    /// 読み込み後に内容が変わったか。
    ///
    /// ダウンロードは挿絵が 1 枚も無い小説でも索引を読むので、変化が無い
    /// (空の) 索引を書き戻さないための判定に使う。
    pub fn is_dirty(&self) -> bool {
        self.0.dirty
    }

    #[cfg(feature = "native-runtime")]
    pub(crate) fn from_store(store: IllustrationStore) -> Self {
        Self(store)
    }

    #[cfg(feature = "native-runtime")]
    pub(crate) fn legacy(&self) -> &IllustrationStore {
        &self.0
    }

    #[cfg(feature = "native-runtime")]
    pub(crate) fn legacy_mut(&mut self) -> &mut IllustrationStore {
        &mut self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIllustration {
    pub filename: String,
    pub hash: String,
    pub created: bool,
}

/// Platform-neutral illustration binary storage boundary.
///
/// The index decides canonical filenames without probing the backing store.
/// The storage service writes the asset first and updates the index only after
/// that write succeeds.
#[derive(Clone)]
pub struct IllustrationStorageService {
    assets: Arc<dyn AssetStore>,
    /// SQLite+S3 モード: 新規挿絵のファイル名を `<sha256-hex>.<ext>` に揃え、
    /// S3 側の `illustrations/<base64url>` プールへ寄せられる形にする。
    dedup: bool,
}

impl IllustrationStorageService {
    pub fn new(assets: Arc<dyn AssetStore>) -> Self {
        Self { assets, dedup: false }
    }

    /// 新規ファイル名を内容ハッシュに揃えて S3 dedup プールと整合させる。
    pub fn with_dedup(mut self, dedup: bool) -> Self {
        self.dedup = dedup;
        self
    }

    /// Stage the source, assemble declared animations without collecting their
    /// output, then publish the verified content hash only after promotion.
    pub async fn store_stream(
        &self,
        access: &NovelWriteGuard,
        keys: &NovelObjectKeys,
        index: &mut IllustrationIndex,
        source: &str,
        stream: AssetStream,
        ext: &str,
    ) -> Result<StoredIllustration> {
        self.store_stream_at(access, keys.prefix(), index, source, stream, ext).await
    }

    pub async fn store_stream_at(
        &self,
        access: &NovelWriteGuard,
        prefix: &ObjectKey,
        index: &mut IllustrationIndex,
        source: &str,
        stream: AssetStream,
        ext: &str,
    ) -> Result<StoredIllustration> {
        if access.prefix() != &crate::platform::ObjectPrefix::from(prefix.clone()) {
            return Err(crate::error::NarouError::Platform("illustration writer belongs to another novel".into()));
        }
        let input = self.assets.stage_stream(stream).await?;
        let result = async {
            match crate::illustration_animation::assemble_animation_stream(
                source, self.assets.clone(), &input,
            ).await? {
                Some(animation) => {
                    let output = self.assets.stage_stream(animation).await?;
                    let result = self.promote(access, prefix, index, source, &output, "png").await;
                    self.remove_stage(&output).await;
                    result
                }
                None => self.promote(access, prefix, index, source, &input, ext).await,
            }
        }.await;
        self.remove_stage(&input).await;
        result
    }

    async fn remove_stage(&self, stage: &StagedAsset) {
        if let Err(error) = self.assets.delete(&stage.key).await {
            // Stages are durably registered for expiry before upload. An
            // isolate cancellation or failed immediate delete is reclaimable.
            tracing::warn!(key = %stage.key, %error, "illustration stage cleanup deferred");
        }
    }

    async fn promote(
        &self,
        access: &NovelWriteGuard,
        prefix: &ObjectKey,
        index: &mut IllustrationIndex,
        source: &str,
        stage: &StagedAsset,
        ext: &str,
    ) -> Result<StoredIllustration> {
        let hash = &stage.sha256;
        let normalized_ext = normalize_extension(ext);
        let id = mitemin_illustration_id(source);
        // A deduplicated pool key must describe these bytes, never an older
        // URL-to-filename mapping whose content may have changed.
        let filename = if self.dedup {
            index.filename_for_hash(hash)
                .filter(|filename| filename.rsplit_once('.').is_some_and(|(stem, _)| stem == hash))
                .map(str::to_string)
                .unwrap_or_else(|| format!("{hash}.{normalized_ext}"))
        } else {
            id.as_deref()
                .and_then(|id| index.filename_for_mitemin_id(id))
                .or_else(|| index.filename_for_source(source))
                .or_else(|| index.filename_for_hash(hash))
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}.{normalized_ext}", id.as_deref().unwrap_or(hash)))
        };
        let key = crate::platform::illustration_key(prefix, &filename)?;
        let matching_hash = index.filename_for_hash(hash) == Some(filename.as_str());
        let created = if matching_hash && self.assets.stat(&key).await?.is_some() {
            false
        } else {
            access.install_staged(&stage.key, &key).await?;
            true
        };
        if let Some(id) = id {
            index.remember_mitemin(source, &id, hash, &filename);
        } else {
            index.remember_hash_source(source, hash, &filename);
        }
        Ok(StoredIllustration { filename, hash: hash.clone(), created })
    }
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
    /// Legacy filesystem cache load (`.illustration_cache.yaml` under the
    /// novel directory). Native compatibility API; Worker builds use
    /// `IllustrationIndex` + `IllustrationStorageService` against the
    /// injected `AssetStore`.
    #[cfg(feature = "native-runtime")]
    pub fn load(archive_path: &Path) -> Result<Self> {
        crate::application::novel_access::require_private_path(archive_path)?;
        crate::application::novel_access::require_private_path(&archive_path.join("挿絵"))?;
        crate::application::novel_access::require_private_path(&archive_path.join(crate::downloader::RAW_DATA_DIR))?;
        let path = cache_path(archive_path);
        crate::application::novel_access::require_private_path(&path)?;
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
        store.migrate_mitemin_id_filenames(archive_path)?;
        store.flush(archive_path)?;
        Ok(store)
    }

    #[cfg(feature = "native-runtime")]
    fn legacy_default() -> Self {
        Self {
            version: LEGACY_STORE_VERSION,
            dirty: true,
            ..Self::default()
        }
    }

    #[cfg(feature = "native-runtime")]
    pub fn flush(&mut self, archive_path: &Path) -> Result<()> {
        crate::application::novel_access::require_private_path(archive_path)?;
        if !self.dirty {
            return Ok(());
        }
        let path = cache_path(archive_path);
        crate::application::novel_access::require_private_path(&path)?;
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

    pub fn to_cache_yaml(&self) -> Result<Vec<u8>> {
        let mut content = serde_yaml::to_string(self)?;
        if content.starts_with("---\n") {
            content.drain(..4);
        }
        Ok(content.into_bytes())
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    #[cfg(feature = "native-runtime")]
    pub fn cached_filename_for_source(&self, source: &str, illust_dir: &Path) -> Result<Option<String>> {
        crate::application::novel_access::require_private_path(illust_dir)?;
        if let Some(id) = mitemin_illustration_id(source)
            && let Some(filename) = self.mitemin_ids.get(&id)
            && saved_filename_exists(illust_dir, filename)?
            && filename_has_basename(filename, &id)
        {
            return Ok(Some(filename.clone()));
        }
        if mitemin_illustration_id(source).is_some() { return Ok(None); }
        let normalized = normalize_illustration_url(source);
        if let Some(filename) = self.sources.get(&normalized)
            && saved_filename_exists(illust_dir, filename)?
        {
            return Ok(Some(filename.clone()));
        }
        Ok(None)
    }

    #[cfg(feature = "native-runtime")]
    pub fn cached_filename_for_hash(&self, hash: &str, illust_dir: &Path) -> Result<Option<String>> {
        crate::application::novel_access::require_private_path(illust_dir)?;
        if let Some(filename) = self.hashes.get(hash)
            && saved_filename_exists(illust_dir, filename)?
        {
            return Ok(Some(filename.clone()));
        }
        find_saved_illustration_filename(illust_dir, hash)
    }

    #[cfg(feature = "native-runtime")]
    pub fn store_bytes(
        &mut self,
        illust_dir: &Path,
        source: &str,
        bytes: &[u8],
        ext: &str,
    ) -> Result<StoredIllustration> {
        crate::application::novel_access::require_private_path(illust_dir)?;
        let hash = hash_bytes(bytes);
        if let Some(id) = mitemin_illustration_id(source) {
            if let Some(filename) = find_saved_illustration_filename(illust_dir, &id)? {
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
            crate::application::novel_access::require_private_path(&path)?;
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

        if let Some(filename) = self.cached_filename_for_hash(&hash, illust_dir)? {
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
        crate::application::novel_access::require_private_path(&path)?;
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

    #[cfg(feature = "native-runtime")]
    pub fn store_existing_file(
        &mut self,
        illust_dir: &Path,
        source: &str,
        filename: &str,
    ) -> Result<StoredIllustration> {
        crate::application::novel_access::require_private_path(illust_dir)?;
        crate::application::novel_access::require_private_path(&illust_dir.join(filename))?;
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

    #[cfg(feature = "native-runtime")]
    pub fn migrate_mitemin_id_filenames(&mut self, archive_path: &Path) -> Result<usize> {
        crate::application::novel_access::require_private_path(archive_path)?;
        if self.version >= STORE_VERSION {
            return Ok(0);
        }

        let illust_dir = archive_path.join("挿絵");
        crate::application::novel_access::require_private_path(&illust_dir)?;
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
            self.sources
                .insert(source.to_string(), filename.to_string());
            self.dirty = true;
        }
    }

    fn remember_mitemin_id(&mut self, id: &str, filename: &str) {
        if self.mitemin_ids.get(id).map(String::as_str) != Some(filename) {
            self.mitemin_ids
                .insert(id.to_string(), filename.to_string());
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

    #[cfg(feature = "native-runtime")]
    fn migrate_mitemin_filenames_from_raw(
        &mut self,
        archive_path: &Path,
        illust_dir: &Path,
    ) -> Result<usize> {
        crate::application::novel_access::require_private_path(archive_path)?;
        crate::application::novel_access::require_private_path(illust_dir)?;
        let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);
        crate::application::novel_access::require_private_path(&raw_dir)?;
        if !raw_dir.is_dir() {
            return Ok(0);
        }

        let mut migrated = 0usize;
        for entry in std::fs::read_dir(raw_dir)? {
            let entry = entry?;
            let path = entry.path();
            crate::application::novel_access::require_private_path(&path)?;
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
                if let Some(filename) = find_saved_illustration_filename(illust_dir, &basename)? {
                    if let Some((target, hash)) =
                        migrate_file_to_mitemin_id(illust_dir, &id, &filename)?
                    {
                        if target != filename {
                            migrated += 1;
                        }
                        self.remember_mitemin(&source, &id, &hash, &target);
                    }
                } else if let Some(filename) = find_saved_illustration_filename(illust_dir, &id)? {
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

#[cfg(feature = "native-runtime")]
pub fn cache_path(archive_path: &Path) -> std::path::PathBuf {
    archive_path.join(CACHE_FILE_NAME)
}

#[cfg(feature = "native-runtime")]
fn has_legacy_illustration_inputs(archive_path: &Path) -> bool {
    archive_path.join("挿絵").is_dir()
        || archive_path.join(crate::downloader::RAW_DATA_DIR).is_dir()
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
#[cfg(feature = "native-runtime")]
pub fn find_orphan_illustrations(archive_path: &Path) -> Result<Vec<PathBuf>> {
    crate::application::novel_access::require_private_path(archive_path)?;
    let illust_dir = archive_path.join("挿絵");
    crate::application::novel_access::require_private_path(&illust_dir)?;
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
    crate::application::novel_access::require_private_path(&raw_dir)?;
    if raw_dir.is_dir() {
        for entry in std::fs::read_dir(&raw_dir)? {
            let entry = entry?;
            let path = entry.path();
            crate::application::novel_access::require_private_path(&path)?;
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for source in extract_all_img_sources(&raw) {
                if let Some(filename) = store.cached_filename_for_source(&source, &illust_dir)? {
                    reachable.insert(filename);
                    continue;
                }
                if let Some(basename) = legacy_basename_from_source(&source)
                    && let Some(filename) = find_saved_illustration_filename(&illust_dir, &basename)?
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
        crate::application::novel_access::require_private_path(&path)?;
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

#[cfg(feature = "native-runtime")]
fn is_self_referencing_canonical(store: &IllustrationStore, filename: &str) -> bool {
    let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    // Hash-named file (64-char hex) is always self-referencing: the converter looks
    // for `挿絵/<hash>.<ext>` on disk during `cached_filename_for_hash` regardless of
    // whether the cache currently has a hash entry, so the file would be reused.
    if is_sha256_hex_stem(stem) {
        return true;
    }
    // mitemin ID-named file is reachable if its stem is a known mitemin ID.
    if is_mitemin_id(stem) && store.mitemin_ids.contains_key(stem) {
        return true;
    }
    false
}

/// A planned legacy filename migration.
#[cfg(feature = "native-runtime")]
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
#[cfg(feature = "native-runtime")]
pub fn plan_legacy_illustration_migrations(
    archive_path: &Path,
) -> Result<Vec<IllustrationMigration>> {
    crate::application::novel_access::require_private_path(archive_path)?;
    let illust_dir = archive_path.join("挿絵");
    crate::application::novel_access::require_private_path(&illust_dir)?;
    if !illust_dir.is_dir() {
        return Ok(Vec::new());
    }

    let raw_sources = collect_raw_source_basename_map(archive_path)?;
    let mut plans = Vec::new();
    for entry in std::fs::read_dir(&illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        crate::application::novel_access::require_private_path(&path)?;
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
#[cfg(feature = "native-runtime")]
pub fn apply_legacy_illustration_migrations(archive_path: &Path) -> Result<usize> {
    crate::application::novel_access::require_private_path(archive_path)?;
    let illust_dir = archive_path.join("挿絵");
    crate::application::novel_access::require_private_path(&illust_dir)?;
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
        crate::application::novel_access::require_private_path(&plan.old_path)?;
        crate::application::novel_access::require_private_path(&plan.new_path)?;
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
#[cfg(feature = "native-runtime")]
pub fn plan_extension_fixes(illust_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    crate::application::novel_access::require_private_path(illust_dir)?;
    if !illust_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    for entry in std::fs::read_dir(illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        crate::application::novel_access::require_private_path(&path)?;
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
        let stem = Path::new(filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(filename);
        let new_path = illust_dir.join(format!("{}.{}", stem, detected));
        crate::application::novel_access::require_private_path(&new_path)?;
        if new_path == path {
            continue;
        }
        plans.push((path, new_path));
    }
    plans.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(plans)
}

/// Apply planned extension fixes: rename files. Returns count actually renamed.
#[cfg(feature = "native-runtime")]
pub fn apply_extension_fixes(illust_dir: &Path, fixes: &[(PathBuf, PathBuf)]) -> Result<usize> {
    crate::application::novel_access::require_private_path(illust_dir)?;
    for (old_path, new_path) in fixes {
        crate::application::novel_access::require_private_path(old_path)?;
        crate::application::novel_access::require_private_path(new_path)?;
    }
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
#[cfg(feature = "native-runtime")]
pub fn rebuild_illustration_cache(archive_path: &Path) -> Result<usize> {
    crate::application::novel_access::require_private_path(archive_path)?;
    let illust_dir = archive_path.join("挿絵");
    crate::application::novel_access::require_private_path(&illust_dir)?;
    if !illust_dir.is_dir() {
        // Even when the directory is missing, flush an empty store so users can recover.
        let mut store = IllustrationStore::default();
        store.flush(archive_path)?;
        return Ok(0);
    }

    let mut store = IllustrationStore::default();
    let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);
    crate::application::novel_access::require_private_path(&raw_dir)?;

    // Walk illust_dir and hash each file.
    let mut hashes: BTreeMap<String, String> = BTreeMap::new();
    for entry in std::fs::read_dir(&illust_dir)? {
        let entry = entry?;
        let path = entry.path();
        crate::application::novel_access::require_private_path(&path)?;
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
            if existing != filename {
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
            crate::application::novel_access::require_private_path(&path)?;
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
                )? {
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

    let entry_count = store.sources.len() + store.mitemin_ids.len() + store.hashes.len();
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

#[cfg(feature = "native-runtime")]
fn is_canonical_illustration_filename(filename: &str) -> bool {
    let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    // 64-char hex (SHA-256) or mitemin ID like i12345.
    is_sha256_hex_stem(stem) || is_mitemin_id(stem)
}

#[cfg(feature = "native-runtime")]
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

#[cfg(feature = "native-runtime")]
struct RawSourceMap {
    /// stem -> (normalized_source_url, raw_html_path, illust_index)
    stem_to_source: BTreeMap<String, RawSourceEntry>,
}

#[cfg(feature = "native-runtime")]
struct RawSourceEntry {
    source: String,
}

#[cfg(feature = "native-runtime")]
fn collect_raw_source_basename_map(archive_path: &Path) -> Result<RawSourceMap> {
    crate::application::novel_access::require_private_path(archive_path)?;
    let mut map = RawSourceMap {
        stem_to_source: BTreeMap::new(),
    };
    let raw_dir = archive_path.join(crate::downloader::RAW_DATA_DIR);
    crate::application::novel_access::require_private_path(&raw_dir)?;
    let entries = match std::fs::read_dir(&raw_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        crate::application::novel_access::require_private_path(&path)?;
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
    Ok(map)
}

#[cfg(feature = "native-runtime")]
fn plan_section_index_count_migration(
    old_path: &Path,
    stem: &str,
    raw_sources: &RawSourceMap,
    illust_dir: &Path,
) -> Result<Option<(PathBuf, Option<String>)>> {
    crate::application::novel_access::require_private_path(old_path)?;
    crate::application::novel_access::require_private_path(illust_dir)?;
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
    crate::application::novel_access::require_private_path(&new_path)?;
    if new_path == *old_path {
        return Ok(None);
    }
    // Try to find a raw HTML source whose stem matches the legacy basename, so the
    // cache can be updated after migration.
    let source = raw_sources
        .stem_to_source
        .get(stem)
        .map(|e| e.source.clone());
    Ok(Some((new_path, source)))
}

#[cfg(feature = "native-runtime")]
fn plan_url_basename_migration(old_path: &Path, illust_dir: &Path) -> Result<Option<PathBuf>> {
    crate::application::novel_access::require_private_path(old_path)?;
    crate::application::novel_access::require_private_path(illust_dir)?;
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
    crate::application::novel_access::require_private_path(&new_path)?;
    if new_path == *old_path {
        Ok(None)
    } else {
        Ok(Some(new_path))
    }
}

#[cfg(feature = "native-runtime")]
fn resolve_existing_filename(
    source: &str,
    section_index: Option<&str>,
    illust_index: usize,
    illust_dir: &Path,
) -> Result<Option<String>> {
    crate::application::novel_access::require_private_path(illust_dir)?;
    if let Some(id) = mitemin_illustration_id(source)
        && let Some(filename) = find_saved_illustration_filename(illust_dir, &id)?
    {
        return Ok(Some(filename));
    }
    if let Some(basename) = legacy_basename_from_source(source)
        && let Some(filename) = find_saved_illustration_filename(illust_dir, &basename)?
    {
        return Ok(Some(filename));
    }
    if let Some(index) = section_index {
        let candidate = format!("{}-{}", index, illust_index);
        if let Some(filename) = find_saved_illustration_filename(illust_dir, &candidate)? {
            return Ok(Some(filename));
        }
    }
    Ok(None)
}

#[cfg(feature = "native-runtime")]
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
    let parsed = url::Url::parse(&normalized).ok()?;
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
    let parsed = url::Url::parse(&normalized).ok()?;
    let segment = parsed
        .path_segments()?.rfind(|part| !part.is_empty())?;
    let stem = segment
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(segment);
    let sanitized = crate::downloader::util::sanitize_filename(stem);
    (!sanitized.is_empty()).then_some(sanitized)
}

/// S3 保存時に挿絵の重複除去プールを使うか。
///
/// native は `s3.asset-backend=s3` のとき (storage-backend の YAML / SQLite を
/// 問わない)、Worker は常に真。Worker の資産が D1 の構成でも内容ハッシュ名に
/// しておくことで、後から S3 へ切り替えたときにそのまま共通プールへ載る。
#[cfg(feature = "native-runtime")]
pub fn dedup_active() -> bool {
    crate::native::s3::illustrations_in_s3()
}

#[cfg(not(feature = "native-runtime"))]
pub fn dedup_active() -> bool {
    true
}

#[cfg(feature = "native-runtime")]
pub fn find_saved_illustration_filename(illust_dir: &Path, basename: &str) -> Result<Option<String>> {
    crate::application::novel_access::require_private_path(illust_dir)?;
    let entries = match std::fs::read_dir(illust_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let prefix = format!("{}.", basename);
    let mut first: Option<String> = None;
    for entry in entries {
        let entry = entry?;
        let Ok(filename) = entry.file_name().into_string() else { continue; };
        if !filename.starts_with(&prefix) { continue; }
        crate::application::novel_access::require_private_path(&entry.path())?;
        if first.as_ref().is_none_or(|current| &filename < current) { first = Some(filename); }
    }
    Ok(first)
}

pub fn is_remote_illustration_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://") || source.starts_with("//")
}

/// Whether an illustration filename stem is exactly 64 ASCII hexadecimal bytes.
pub fn is_sha256_hex_stem(stem: &str) -> bool {
    stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn guessed_extension_from_url(url: &str) -> &'static str {
    url::Url::parse(url)
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

#[cfg(feature = "native-runtime")]
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

#[cfg(feature = "native-runtime")]
fn migrate_file_to_mitemin_id(
    illust_dir: &Path,
    id: &str,
    filename: &str,
) -> Result<Option<(String, String)>> {
    if !saved_filename_exists(illust_dir, filename)? {
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
    crate::application::novel_access::require_private_path(&target_path)?;
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

#[cfg(feature = "native-runtime")]
fn hash_file(path: &Path) -> Result<String> {
    crate::application::novel_access::require_private_path(path)?;
    let bytes = std::fs::read(path)?;
    Ok(hash_bytes(&bytes))
}

#[cfg(feature = "native-runtime")]
fn raw_section_index(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let index = stem.split_whitespace().next().unwrap_or(stem).trim();
    (!index.is_empty()).then_some(index.to_string())
}

#[cfg(feature = "native-runtime")]
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

#[cfg(feature = "native-runtime")]
fn saved_filename_exists(illust_dir: &Path, filename: &str) -> Result<bool> {
    crate::application::novel_access::require_private_path(illust_dir)?;
    if filename.is_empty() || filename.contains('/') || filename.contains('\\') { return Ok(false); }
    let path = illust_dir.join(filename);
    crate::application::novel_access::require_private_path(&path)?;
    Ok(path.is_file())
}

#[cfg(feature = "native-runtime")]
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

/// Source and newline-delimited expanded filename spools have separate caps.
const ILLUSTRATION_METADATA_BYTES: usize = 16 * 1024 * 1024;

struct FilenameMapFrame<'a> {
    key_next: bool,
    root: bool,
    selected: bool,
    selected_field: bool,
    anchor: usize,
    /// Only direct scalar VALUES, never source URLs/hash keys or YAML Values.
    projection: Option<Vec<std::borrow::Cow<'a, str>>>,
}

enum FilenameFrame<'a> {
    Mapping(FilenameMapFrame<'a>),
    Sequence,
}

enum FilenameAnchor<'a> {
    Scalar(std::borrow::Cow<'a, str>),
    Map(Vec<std::borrow::Cow<'a, str>>),
    Other,
}

/// Incremental values from the legacy flat illustration-index schema. Only
/// `sources`, `mitemin_ids` and `hashes` mapping VALUES are yielded. Duplicate
/// filenames are deliberate: dedup belongs to the caller's durable lookup.
///
/// A real YAML pull parser preserves quoted/escaped scalars and scalar/map
/// aliases. Anchored maps retain only their scalar-valued filename projection,
/// not URL/hash keys or an entire YAML value tree. Input plus retained caches
/// fit 18 MiB, reserving 5 MiB pending append and 1 MiB slack within 24 MiB.
/// One decoded parser scalar is separate transient codec workspace. Callers
/// must consume EOF before publishing and admit their serialized spool size.
pub struct IllustrationFilenameReader<'a> {
    parser: saphyr_parser::Parser<'a, saphyr_parser::StrInput<'a>>,
    frames: Vec<FilenameFrame<'a>>,
    anchors: BTreeMap<usize, FilenameAnchor<'a>>,
    retained: usize,
    retention_limit: usize,
    expanded_bytes: usize,
    pending_alias: Option<(usize, usize)>,
    documents: usize,
    root_seen: bool,
    root_done: bool,
    oversized: bool,
    finished: bool,
}

impl<'a> IllustrationFilenameReader<'a> {
    pub fn new(yaml: &'a str) -> Self {
        Self {
            parser: saphyr_parser::Parser::new_from_str(yaml), frames: Vec::new(),
            anchors: BTreeMap::new(), retained: 0,
            // Reserve 5 MiB for a multipart append plus 1 MiB for stack,
            // parser/control overhead. Decoded parser token memory is transient.
            retention_limit: (18 * 1024 * 1024usize).saturating_sub(yaml.len()),
            expanded_bytes: 0, pending_alias: None, documents: 0,
            root_seen: false, root_done: false,
            oversized: yaml.len() > ILLUSTRATION_METADATA_BYTES, finished: false,
        }
    }

    fn invalid(message: impl std::fmt::Display) -> crate::error::NarouError {
        crate::error::NarouError::Platform(format!("invalid illustration index: {message}"))
    }

    fn budget(message: &str) -> crate::error::NarouError {
        crate::error::NarouError::Platform(format!("transfer_metadata_budget: illustration index {message}"))
    }

    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.retained = self.retained.checked_add(bytes).ok_or_else(|| Self::budget("projection overflow"))?;
        if self.retained > self.retention_limit { return Err(Self::budget("anchor projection exceeds retained metadata budget")); }
        Ok(())
    }

    fn expanded(&mut self, bytes: usize) -> Result<()> {
        let record_bytes = bytes.checked_add(1).ok_or_else(|| Self::budget("filename spool overflow"))?;
        self.expanded_bytes = self.expanded_bytes.checked_add(record_bytes)
            .ok_or_else(|| Self::budget("filename spool overflow"))?;
        if self.expanded_bytes > ILLUSTRATION_METADATA_BYTES { return Err(Self::budget("expanded filename spool exceeds 16 MiB")); }
        Ok(())
    }

    fn filename(value: &str) -> Result<String> {
        let filename = crate::transfer::validate_relative_path(value).map_err(Self::invalid)?;
        if filename != value || filename.contains('/') {
            return Err(Self::invalid("illustration filename must be one safe path component"));
        }
        Ok(filename)
    }

    fn scalar(&mut self, value: std::borrow::Cow<'a, str>) -> Result<Option<String>> {
        let Some(frame) = self.frames.last_mut() else { return Err(Self::invalid("root must be a mapping")); };
        let FilenameFrame::Mapping(frame) = frame else { return Ok(None); };
        if frame.key_next {
            frame.selected_field = frame.root && matches!(value.as_ref(), "sources" | "mitemin_ids" | "hashes");
            frame.key_next = false;
            return Ok(None);
        }
        frame.key_next = true;
        if frame.selected_field { return Err(Self::invalid("filename field must be a mapping")); }
        let selected = frame.selected;
        let length = value.len();
        let filename = if selected { Some(Self::filename(&value)?) } else { None };
        let retain = frame.anchor != 0 && frame.projection.is_some();
        let retained_bytes = match &value {
            std::borrow::Cow::Owned(value) => value.capacity(),
            std::borrow::Cow::Borrowed(value) => value.len(),
        };
        if retain {
            self.reserve(retained_bytes.saturating_add(64))?;
            if let Some(FilenameFrame::Mapping(frame)) = self.frames.last_mut() {
                frame.projection.as_mut().expect("scalar projection").push(value);
            }
        }
        if selected { self.expanded(length)?; }
        Ok(filename)
    }

    fn start_collection(&mut self, mapping: bool, anchor: usize) -> Result<()> {
        if self.frames.len() >= 128 { return Err(Self::invalid("metadata nesting exceeds limit")); }
        let root = self.frames.is_empty();
        let mut selected = false;
        if root {
            if self.root_seen || !mapping { return Err(Self::invalid("root must be one mapping")); }
            self.root_seen = true;
        } else if let Some(FilenameFrame::Mapping(parent)) = self.frames.last_mut() {
            if parent.key_next { return Err(Self::invalid("mapping key must be a scalar")); }
            if parent.selected { return Err(Self::invalid("filename must be a scalar")); }
            selected = parent.selected_field;
            if selected && !mapping { return Err(Self::invalid("filename field must be a mapping")); }
            parent.key_next = true;
            parent.projection = None;
        }
        if anchor != 0 {
            self.reserve(128)?;
            self.anchors.insert(anchor, FilenameAnchor::Other);
        }
        self.frames.push(if mapping {
            FilenameFrame::Mapping(FilenameMapFrame {
                key_next: true, root, selected, selected_field: false, anchor,
                projection: (anchor != 0).then(Vec::new),
            })
        } else { FilenameFrame::Sequence });
        Ok(())
    }

    fn alias(&mut self, anchor: usize) -> Result<Option<String>> {
        match self.anchors.get(&anchor) {
            Some(FilenameAnchor::Scalar(value)) => return self.scalar(value.clone()),
            Some(FilenameAnchor::Map(_)) => {}
            Some(FilenameAnchor::Other) => {
                // Complex aliases outside filename maps do not affect this
                // projection. A filename/map field must have the right type.
                if let Some(FilenameFrame::Mapping(frame)) = self.frames.last_mut() {
                    if frame.key_next || frame.selected || frame.selected_field {
                        return Err(Self::invalid("alias is not a scalar-valued filename map"));
                    }
                    frame.key_next = true; frame.projection = None;
                }
                return Ok(None);
            }
            None => return Err(Self::invalid("unknown scalar/map alias")),
        }
        match self.frames.last_mut() {
            Some(FilenameFrame::Mapping(frame)) if !frame.key_next && frame.root && frame.selected_field => {
                frame.key_next = true;
                self.pending_alias = Some((anchor, 0));
            }
            Some(FilenameFrame::Mapping(frame)) => {
                if frame.key_next || frame.selected { return Err(Self::invalid("filename/key must be a scalar")); }
                frame.key_next = true; frame.projection = None;
            }
            Some(FilenameFrame::Sequence) => {}
            None => return Err(Self::invalid("root must be a mapping")),
        }
        Ok(None)
    }

    fn step(&mut self) -> Result<Option<String>> {
        use saphyr_parser::Event;
        loop {
            if let Some((anchor, position)) = self.pending_alias {
                let Some(FilenameAnchor::Map(values)) = self.anchors.get(&anchor) else { unreachable!("projected map alias"); };
                if let Some(value) = values.get(position) {
                    let length = value.len();
                    let filename = Self::filename(value)?;
                    self.expanded(length)?;
                    self.pending_alias = Some((anchor, position + 1));
                    return Ok(Some(filename));
                }
                self.pending_alias = None;
            }
            let (event, _) = self.parser.next_event().ok_or_else(|| Self::invalid("unexpected parser EOF"))?
                .map_err(Self::invalid)?;
            match event {
                Event::StreamStart | Event::Nothing => {}
                Event::DocumentStart(_) => {
                    self.documents += 1;
                    if self.documents != 1 { return Err(Self::invalid("multiple YAML documents")); }
                }
                Event::DocumentEnd => {
                    if !self.root_done || !self.frames.is_empty() { return Err(Self::invalid("incomplete index mapping")); }
                }
                Event::StreamEnd => {
                    if self.documents != 1 || !self.root_done { return Err(Self::invalid("missing index mapping")); }
                    self.finished = true; return Ok(None);
                }
                Event::MappingStart(anchor, _) => self.start_collection(true, anchor)?,
                Event::SequenceStart(anchor, _) => self.start_collection(false, anchor)?,
                Event::MappingEnd | Event::SequenceEnd => {
                    let frame = self.frames.pop().ok_or_else(|| Self::invalid("unmatched collection end"))?;
                    if let FilenameFrame::Mapping(frame) = frame {
                        if !frame.key_next { return Err(Self::invalid("mapping value missing")); }
                        if frame.anchor != 0 {
                            let projection = frame.projection.map(FilenameAnchor::Map).unwrap_or(FilenameAnchor::Other);
                            self.anchors.insert(frame.anchor, projection);
                        }
                    }
                    if self.frames.is_empty() { self.root_done = true; }
                }
                Event::Scalar(value, _, anchor, _) => {
                    if anchor != 0 {
                        self.reserve(value.len().saturating_add(128))?;
                        self.anchors.insert(anchor, FilenameAnchor::Scalar(value.clone()));
                    }
                    if let Some(filename) = self.scalar(value)? { return Ok(Some(filename)); }
                }
                Event::Alias(anchor) => if let Some(filename) = self.alias(anchor)? { return Ok(Some(filename)); },
            }
        }
    }
}

impl Iterator for IllustrationFilenameReader<'_> {
    type Item = Result<String>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished { return None; }
        if self.oversized {
            self.finished = true;
            return Some(Err(Self::budget("input exceeds 16 MiB")));
        }
        match self.step() {
            Ok(Some(filename)) => Some(Ok(filename)), Ok(None) => None,
            Err(error) => { self.finished = true; Some(Err(error)) }
        }
    }
}

impl std::iter::FusedIterator for IllustrationFilenameReader<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ObjectStore;

    #[test]
    fn sha256_hex_stem_requires_exactly_64_ascii_hex_bytes() {
        for stem in [
            "0".repeat(64),
            "0123456789abcdef".repeat(4),
            "0123456789ABCDEF".repeat(4),
            "aBcD0123".repeat(8),
        ] {
            assert!(is_sha256_hex_stem(&stem), "{stem:?}");
        }
        for stem in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
            format!("{} ", "a".repeat(63)),
            "é".repeat(32),
            "ａ".repeat(64),
            "i12345".to_string(),
        ] {
            assert!(!is_sha256_hex_stem(&stem), "{stem:?}");
        }
    }

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
            store
                .cached_filename_for_source(source, &illust_dir)
                .unwrap()
                .as_deref(),
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
                .unwrap()
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
        assert_eq!(
            store.filename_for_mitemin_id("i422674"),
            Some("i422674.jpg")
        );
        assert_eq!(
            store.hash_for_mitemin_id("i422674"),
            Some(hash_bytes(b"dummy").as_str())
        );
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
        assert_eq!(
            store.filename_for_mitemin_id("i422674"),
            Some("i422674.jpg")
        );
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
        bytes.extend(std::iter::repeat_n(seed, 32));
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
            let ext = plan
                .old_path
                .extension()
                .unwrap()
                .to_string_lossy()
                .to_string();
            assert_eq!(
                plan.new_path
                    .extension()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                ext
            );
            assert_eq!(
                plan.new_path.file_name().unwrap().to_string_lossy().len(),
                64 + 1 + ext.len()
            );
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
                .unwrap()
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

    #[test]
    fn storage_service_writes_assets_and_repairs_missing_cached_blob() {
        let assets = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
        let service = IllustrationStorageService::new(assets.clone());
        let keys = NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let source = "https://example.com/image.png";
        let bytes = b"image bytes";
        let mut index = IllustrationIndex::default();

        futures::executor::block_on(async {
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let first = service
                .store_stream(&write, &keys, &mut index, source, test_stream(bytes), "png")
                .await
                .unwrap();
            assert!(first.created);
            let second = service
                .store_stream(&write, &keys, &mut index, source, test_stream(bytes), "png")
                .await
                .unwrap();
            assert!(!second.created);
            let key = keys.illustration(&first.filename).unwrap();
            AssetStore::delete(assets.as_ref(), &key).await.unwrap();
            let repaired = service
                .store_stream(&write, &keys, &mut index, source, test_stream(bytes), "png")
                .await
                .unwrap();
            assert!(repaired.created);
            assert_eq!(

                ObjectStore::read_small(assets.as_ref(), &key)
                    .await
                    .unwrap()
                    .unwrap(),
                bytes
            );
        });
    }

    async fn test_write(objects: Arc<dyn crate::platform::ObjectStore>, assets: Arc<dyn AssetStore>, keys: &NovelObjectKeys) -> NovelWriteGuard {
        NovelWriteGuard::capture_for(&objects, Some(&assets), keys).await.unwrap()
    }

    fn test_stream(bytes: &[u8]) -> AssetStream {
        let body = bytes.to_vec();
        Box::pin(futures::stream::once(async move { Ok(body) }))
    }

    #[test]
    fn failed_input_does_not_publish_an_index_or_leave_a_stage() {
        futures::executor::block_on(async {
            let assets = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let service = IllustrationStorageService::new(assets.clone()).with_dedup(true);
            let keys = NovelObjectKeys::new("site", "broken", true).unwrap();
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let mut index = IllustrationIndex::default();
            let input: AssetStream = Box::pin(futures::stream::iter([
                Ok(b"partial image".to_vec()),
                Err(crate::error::NarouError::Platform("source disconnected".into())),
            ]));
            assert!(service.store_stream(&write, &keys, &mut index, "https://e.test/a.png", input, "png").await.is_err());
            assert!(index.filenames().is_empty());
            assert!(!index.is_dirty());
            assert!(assets.is_empty());
        });
    }

    #[test]
    fn changed_source_uses_its_new_content_hash_without_overwriting_old_pool_bytes() {
        futures::executor::block_on(async {
            let assets = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let service = IllustrationStorageService::new(assets.clone()).with_dedup(true);
            let keys = NovelObjectKeys::new("site", "changed", true).unwrap();
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let mut index = IllustrationIndex::default();
            let source = "https://e.test/a.png";
            let old = service.store_stream(&write, &keys, &mut index, source, test_stream(b"old"), "png").await.unwrap();
            let new = service.store_stream(&write, &keys, &mut index, source, test_stream(b"new"), "png").await.unwrap();
            assert_ne!(old.filename, new.filename);
            assert_eq!(new.hash, hash_bytes(b"new"));
            assert_eq!(index.filename_for_source(source), Some(new.filename.as_str()));
            assert_eq!(ObjectStore::read_small(assets.as_ref(), &keys.illustration(&old.filename).unwrap()).await.unwrap().unwrap(), b"old");
            assert_eq!(assets.len(), 2, "completed input stages are removed");
        });
    }

    #[derive(Default)]
    struct FailingPromotion {
        inner: crate::platform::mocks::MemoryObjectStore,
        fail_output: bool,
        stages: std::sync::atomic::AtomicUsize,
    }

    impl crate::platform::ObjectStore for FailingPromotion {
        fn access_fence(&self) -> Option<&dyn crate::platform::NovelAccessFence> { Some(&self.inner) }
        fn stat<'a>(&'a self, key: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<Option<crate::platform::ObjectMetadata>>> { crate::platform::ObjectStore::stat(&self.inner, key) }
        fn read_small<'a>(&'a self, key: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<Option<Vec<u8>>>> { self.inner.read_small(key) }
        fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>) -> crate::platform::PlatformFuture<'a, Result<()>> { self.inner.write_small(key, data) }
        fn delete<'a>(&'a self, key: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<()>> { crate::platform::ObjectStore::delete(&self.inner, key) }
        fn list_page<'a>(&'a self, request: &'a crate::platform::ObjectListRequest) -> crate::platform::PlatformFuture<'a, Result<crate::platform::ObjectListPage>> { self.inner.list_page(request) }
    }

    impl AssetStore for FailingPromotion {
        fn write_stream_guarded<'a>(&'a self, _fence: &'a crate::platform::WriteFence, _key: &'a ObjectKey, _stream: AssetStream) -> crate::platform::PlatformFuture<'a, Result<()>> {
            Box::pin(async { Err(crate::error::NarouError::Platform("promotion failed".into())) })
        }

        fn stat<'a>(&'a self, key: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<Option<crate::platform::ObjectMetadata>>> {
            AssetStore::stat(&self.inner, key)
        }
        fn read_stream<'a>(&'a self, key: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<Option<AssetStream>>> {
            AssetStore::read_stream(&self.inner, key)
        }
        fn read_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> crate::platform::PlatformFuture<'a, Result<Option<Vec<u8>>>> {
            AssetStore::read_range(&self.inner, key, range)
        }
        fn write_stream<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream) -> crate::platform::PlatformFuture<'a, Result<()>> {
            AssetStore::write_stream(&self.inner, key, stream)
        }
        fn stage_stream<'a>(&'a self, stream: AssetStream) -> crate::platform::PlatformFuture<'a, Result<StagedAsset>> {
            use futures::StreamExt;
            let stream = if self.fail_output && self.stages.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Box::pin(stream.take(1).chain(futures::stream::once(async {
                    Err(crate::error::NarouError::Platform("output disconnected".into()))
                }))) as AssetStream
            } else {
                stream
            };
            AssetStore::stage_stream(&self.inner, stream)
        }
        fn delete<'a>(&'a self, key: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<()>> {
            AssetStore::delete(&self.inner, key)
        }
        fn copy<'a>(&'a self, _source: &'a ObjectKey, _destination: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<()>> {
            Box::pin(async { Err(crate::error::NarouError::Platform("promotion failed".into())) })
        }
        fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> crate::platform::PlatformFuture<'a, Result<()>> {
            self.copy(source, destination)
        }
    }

    #[test]
    fn failed_promotion_keeps_the_previous_index_and_removes_source_stage() {
        futures::executor::block_on(async {
            let assets = Arc::new(FailingPromotion::default());
            let service = IllustrationStorageService::new(assets.clone()).with_dedup(true);
            let keys = NovelObjectKeys::new("site", "copy-error", true).unwrap();
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let source = "https://e.test/a.png";
            let mut index = IllustrationIndex::default();
            index.remember_hash_source(source, "old", "old.png");
            index.mark_clean();
            assert!(service.store_stream(&write, &keys, &mut index, source, test_stream(b"new"), "png").await.is_err());
            assert_eq!(index.filename_for_source(source), Some("old.png"));
            assert!(!index.is_dirty());
            assert!(assets.inner.is_empty());
        });
    }

    #[cfg(feature = "illustration-animation")]
    #[test]
    fn corrupt_animation_never_publishes_the_source_zip_as_an_image() {
        futures::executor::block_on(async {
            let assets = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let service = IllustrationStorageService::new(assets.clone());
            let keys = NovelObjectKeys::new("site", "broken-zip", true).unwrap();
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let mut index = IllustrationIndex::default();
            assert!(service.store_stream(&write, &keys, &mut index, "https://e.test/a.zip?ugoira=50", test_stream(b"PK\x03\x04broken"), "jpg").await.is_err());
            assert!(index.filenames().is_empty());
            assert!(assets.is_empty());
        });
    }

    #[cfg(feature = "illustration-animation")]
    fn test_frame_archive() -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for index in 0..2 {
            let frame = image::RgbaImage::from_pixel(2, 2, image::Rgba([index, 10, 20, 128]));
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(frame).write_to(&mut png, image::ImageFormat::Png).unwrap();
            writer.start_file(format!("{index}.png"), zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(png.get_ref()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[cfg(feature = "illustration-animation")]
    #[test]
    fn failed_animation_output_keeps_index_unpublished_and_cleans_source() {
        futures::executor::block_on(async {
            let assets = Arc::new(FailingPromotion { fail_output: true, ..Default::default() });
            let service = IllustrationStorageService::new(assets.clone());
            let keys = NovelObjectKeys::new("site", "output-error", true).unwrap();
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let mut index = IllustrationIndex::default();
            let error = service.store_stream(&write, &keys, &mut index, "https://e.test/a.zip?ugoira=50",
                test_stream(&test_frame_archive()), "jpg",
            ).await.unwrap_err();
            assert!(error.to_string().contains("output disconnected"));
            assert!(!index.is_dirty());
            assert!(index.filenames().is_empty());
            assert!(assets.inner.is_empty());
        });
    }

    #[cfg(feature = "illustration-animation")]
    #[test]
    fn completed_animation_hashes_output_promotes_png_and_removes_both_stages() {
        futures::executor::block_on(async {
            let assets = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let service = IllustrationStorageService::new(assets.clone()).with_dedup(true);
            let keys = NovelObjectKeys::new("site", "animation", true).unwrap();
            let write = test_write(assets.clone(), assets.clone(), &keys).await;
            let source = "https://e.test/a.zip?ugoira=50";
            let mut index = IllustrationIndex::default();
            let archive = test_frame_archive();
            let stored = service.store_stream(&write, &keys, &mut index, source, test_stream(&archive), "jpg").await.unwrap();
            let png = ObjectStore::read_small(assets.as_ref(), &keys.illustration(&stored.filename).unwrap()).await.unwrap().unwrap();
            assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert_eq!(stored.hash, hash_bytes(&png));
            assert_ne!(stored.hash, hash_bytes(&archive));
            assert_eq!(stored.filename, format!("{}.png", stored.hash));
            assert_eq!(index.filename_for_source(source), Some(stored.filename.as_str()));
            assert_eq!(assets.len(), 1);
        });
    }

    #[test]
    fn pure_index_keeps_legacy_cache_serialization() {
        let source = "https://example.com/image.png";
        let mut legacy = IllustrationStore::default();
        legacy.remember_hash_source(source, "abc123", "abc123.png");
        let index = IllustrationIndex::from_store(legacy.clone());
        assert_eq!(index.to_cache_yaml().unwrap(), legacy.to_cache_yaml().unwrap());
    }
    #[test]
    fn filename_reader_preserves_scalars_and_map_aliases_without_hash_values() {
        let yaml = r#"version: 2
sources: &files
  'https://fixture.test/one': &one "escaped\u0020one.png"
  'https://fixture.test/two': 'two''s.png'
mitemin_ids:
  i1: *one
hashes: *files
mitemin_hashes: {i1: ignored-hash}
source_hashes: {url: ignored-source-hash}
"#;
        let filenames = IllustrationFilenameReader::new(yaml).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(filenames, ["escaped one.png", "two's.png", "escaped one.png", "escaped one.png", "two's.png"]);
        let index: IllustrationIndex = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(index.filename_for_mitemin_id("i1"), Some(filenames[0].as_str()));
    }

    #[test]
    fn filename_reader_rejects_malformed_tail_after_values_and_fuses() {
        let mut reader = IllustrationFilenameReader::new("sources: {url: first.png}\nhashes: [broken\n");
        assert_eq!(reader.next().unwrap().unwrap(), "first.png");
        assert!(reader.next().unwrap().is_err());
        assert!(reader.next().is_none());
    }

    #[test]
    fn stale_image_source_cannot_replace_newer_canonical_bytes_or_index() {
        use crate::platform::{NovelAccessFence, ObjectPrefix};
        futures::executor::block_on(async {
            let store = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
            let keys = NovelObjectKeys::new("site", "stale-image", false).unwrap();
            let objects: Arc<dyn crate::platform::ObjectStore> = store.clone();
            let assets: Arc<dyn AssetStore> = store.clone();
            let write = NovelWriteGuard::capture_for(&objects, Some(&assets), &keys).await.unwrap();
            let prefix = ObjectPrefix::from(keys.prefix().clone());
            let canonical = keys.illustration("i123.png").unwrap();
            let input_store = store.clone();
            let input_key = canonical.clone();
            let stream: AssetStream = Box::pin(futures::stream::once(async move {
                let epoch = input_store.close_gate(&prefix, "replacement").await?;
                input_store.open_gate(&prefix, "replacement", epoch).await?;
                let fresh = input_store.capture_writer(&prefix).await?;
                input_store.write_small_guarded(&fresh, &input_key, b"new image".to_vec()).await?;
                Ok(b"stale image".to_vec())
            }));
            let service = IllustrationStorageService::new(assets);
            let mut index = IllustrationIndex::default();
            assert!(service.store_stream(&write, &keys, &mut index, "https://123.mitemin.net/userpageimage/viewimage/icode/i123/", stream, "png").await.is_err());
            assert_eq!(store.read_small(&canonical).await.unwrap().unwrap(), b"new image");
            assert!(!index.is_dirty());
            assert!(index.filenames().is_empty());
        });
    }


    #[test]
    fn filename_reader_accepts_anchored_maps_from_ignored_metadata() {
        let yaml = "templates: {files: &files {url: &file 'escaped one.png', other: *file}}\nsources: *files\nmitemin_ids: {i1: *file}\nhashes: *files\n";
        let names = IllustrationFilenameReader::new(yaml).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(names, ["escaped one.png"; 5]);
    }

    #[test]
    fn filename_reader_rejects_invalid_structure_paths_and_depth_before_success() {
        for yaml in [
            "sources: {url: '../escape.png'}\n",
            "sources: {url: 'directory/file.png'}\n",
            "sources: {url: 'CON.png'}\n",
            "sources: {url: \"bad\\nname.png\"}\n",
            "sources: {url: [nested]}\n",
            "other: &files {url: [nested]}\nsources: *files\n",
            "other: &list [file.png]\nsources: *list\n",
            "sources: {}\n---\nsources: {}\n",
        ] {
            assert!(IllustrationFilenameReader::new(yaml).collect::<Result<Vec<_>>>().is_err(), "{yaml:?}");
        }
        let yaml = format!("other: {}0{}\nsources: {{}}\n", "[".repeat(129), "]".repeat(129));
        assert!(IllustrationFilenameReader::new(&yaml).collect::<Result<Vec<_>>>().is_err());
    }

    #[test]
    fn filename_reader_rejects_retained_anchor_and_expanded_spool_overflow() {
        // The source is under 16 MiB; its owned scalar anchor plus map
        // projection cannot be retained inside the 18 MiB combined budget.
        let yaml = format!("other: &files {{url: &name \"{}\\u0061\"}}\nsources: {{}}\n", "x".repeat(7 * 1024 * 1024));
        let error = IllustrationFilenameReader::new(&yaml).collect::<Result<Vec<_>>>().unwrap_err();
        assert!(error.to_string().contains("transfer_metadata_budget"), "{error}");

        // Three selected fields alias the same scalar-valued map. The
        // source/cache fit, but the newline spool expands beyond 16 MiB.
        let filename = format!("{}.png", "a".repeat(251));
        let mut yaml = String::from("sources: &files\n");
        for index in 0..23_000 { yaml.push_str(&format!("  url{index}: '{filename}'\n")); }
        yaml.push_str("mitemin_ids: *files\nhashes: *files\n");
        assert!(yaml.len() < ILLUSTRATION_METADATA_BYTES);
        let error = IllustrationFilenameReader::new(&yaml).try_for_each(|name| name.map(|_| ())).unwrap_err();
        assert!(error.to_string().contains("transfer_metadata_budget"), "{error}");
    }
}
