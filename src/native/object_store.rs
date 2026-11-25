//! Native ObjectStore and AssetStore adapters.
//!
//! Logical keys are mapped below the existing archive root. `novels/` is a
//! namespace in the logical key only; the native layout remains
//! `小説データ/<site>/<optional-subdirectory>/<title>/...`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use futures::StreamExt;

use crate::error::{NarouError, Result};
use std::sync::Arc;

use crate::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, PlatformFuture, S3Store, SplitStore,
    ObjectUpdateResult, SmallObjectMutator,
};

pub(crate) const MAX_SMALL_OBJECT_BYTES: u64 = 16 * 1024 * 1024;

fn legacy_flat_parts(key:&ObjectKey)->Option<(&str,&str,&str,Option<&str>)>{
    let mut parts=key.as_ref().strip_prefix("novels/")?.splitn(4,'/');let site=parts.next()?;let partition=parts.next()?;let title=parts.next()?;
    if !title.chars().skip(usize::from(title.starts_with('n'))).take(2).eq(partition.chars()){return None;}Some((site,partition,title,parts.next()))
}
pub(crate) fn legacy_flat_key(key:&ObjectKey)->Result<Option<ObjectKey>>{
    let Some((site,_,title,tail))=legacy_flat_parts(key)else{return Ok(None);};ObjectKey::try_new(match tail{Some(tail)=>format!("novels/{site}/{title}/{tail}"),None=>format!("novels/{site}/{title}")}).map(Some)
}

pub(crate) fn collect_expired_stage_files(root: &Path) -> Result<()> {
    let entries = match fs::read_dir(root.join(".objects").join(".narou-staging")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() { continue; }
        let file = match fs::OpenOptions::new().read(true).write(true).open(entry.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = fs2::FileExt::try_lock_exclusive(&file) {
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() { continue; }
            return Err(error.into());
        }
        if file.metadata()?.modified()?.elapsed().unwrap_or_default().as_secs()
            > crate::platform::staging::STAGE_TTL_SECONDS as u64 {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn renew_stage_file(file: &fs::File, key: &ObjectKey, cache: &crate::platform::staging::StageLeaseCache) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    if cache.needs_renewal(key, now) {
        file.set_modified(std::time::SystemTime::now())?;
        cache.renewed(key, now);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct NativeObjectStore {
    root: PathBuf,
    /// Directories already known to exist this process. `create_dir_all` on
    /// an existing path still costs a syscall per ancestor; section writes
    /// repeat the same `本文/` parent hundreds of times per novel.
    created_dirs: std::sync::Arc<parking_lot::Mutex<std::collections::HashSet<PathBuf>>>,
    stage_leases: Arc<crate::platform::staging::StageLeaseCache>,
    /// Whether this store's gate admission consults the live `.narou` SQLite
    /// authority. Private conversion workspaces (`.narou-snapshot-*` temp
    /// roots) are never a canonical library namespace: consulting the parent
    /// library's `db.sqlite` would both be meaningless (their FS gates are
    /// independent of the library's gates) and contention-prone (issue #50).
    consult_sql_authority: bool,
}

impl NativeObjectStore {
    pub fn new() -> Result<Self> {
        let root = crate::db::with_database(|db| Ok(db.archive_root().to_path_buf()))?;
        Self::from_root(root)
    }

    pub fn from_root(root: PathBuf) -> Result<Self> {
        Self::from_root_inner(root, true)
    }

    /// Same as [`Self::from_root`] but for a private, operation-owned
    /// workspace root (conversion snapshots, private capability stores):
    /// gate admission never consults the parent `.narou/db.sqlite`, so a
    /// contended library handle cannot fabricate a `Conflict` for a
    /// namespace the library does not manage.
    pub(crate) fn from_private_root(root: PathBuf) -> Result<Self> {
        Self::from_root_inner(root, false)
    }

    fn from_root_inner(root: PathBuf, consult_sql_authority: bool) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            stage_leases: Arc::new(crate::platform::staging::StageLeaseCache::default()),
            created_dirs: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashSet::new(),
            )),
            consult_sql_authority,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve_exact_path(&self, key: &ObjectKey) -> Result<PathBuf> {
        key.validate()?;
        let relative = key
            .as_ref()
            .strip_prefix("novels/")
            .unwrap_or_else(|| key.as_ref());
        let relative = if key.as_ref().starts_with("novels/") {
            relative.to_string()
        } else {
            format!(".objects/{relative}")
        };
        let candidate = self
            .root
            .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        // Missing components may only exist under a historical `‛`-escaped
        // spelling; walk toward them so reads and writes land in the
        // directory that already holds the novel's data instead of creating
        // a parallel canonical one. New components stay canonical.
        let resolved =
            crate::db::paths::resolve_existing_path_components(&self.root, &candidate);
        crate::db::paths::ensure_within_archive_root(&resolved, &self.root)
    }

    pub(crate) fn resolve_path(&self,key:&ObjectKey)->Result<PathBuf>{
        let exact=self.resolve_exact_path(key)?;
        if let Some((site,partition,title,tail))=legacy_flat_parts(key){
            let canonical=crate::db::paths::resolve_existing_path_components(&self.root,&self.root.join(site).join(partition).join(title));
            if !canonical.exists(){let flat=crate::db::paths::resolve_existing_path_components(&self.root,&self.root.join(site).join(title));if flat.is_dir(){let target=tail.map_or_else(||flat.clone(),|tail|flat.join(tail));let target=crate::db::paths::resolve_existing_path_components(&self.root,&target);return crate::db::paths::ensure_within_archive_root(&target,&self.root);}}
        }Ok(exact)
    }

    pub(crate) fn resolve_read_path(&self, key: &ObjectKey) -> Result<PathBuf> {
        let exact = self.resolve_path(key)?;
        if exact.exists() {
            return Ok(exact);
        }

        // Existing narou.rb layouts sometimes changed only the subtitle part
        // of a section filename. Keep that fallback native-only; normal core
        // paths always use the canonical ObjectKey generated from the TOC.
        let Some(parent) = exact.parent() else {
            return Ok(exact);
        };
        if parent.file_name().and_then(|name| name.to_str()) != Some("本文") {
            return Ok(exact);
        }
        if !parent.exists() {
            return Ok(exact);
        }
        let Some(filename) = exact.file_name().and_then(|name| name.to_str()) else {
            return Ok(exact);
        };
        let Some(index) = filename.split_once(' ').map(|(index, _)| index) else {
            return Ok(exact);
        };
        let prefix = format!("{index} ");
        for entry in fs::read_dir(parent)?.flatten() {
            let candidate = entry.path();
            let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.ends_with(".yaml")
                && (name == format!("{index}.yaml") || name.starts_with(&prefix))
            {
                return crate::db::paths::ensure_within_archive_root(&candidate, &self.root);
            }
        }
        Ok(exact)
    }

    /// Map a listing prefix to the on-disk directory that contains every
    /// matching object. `novels` alone covers the whole archive root;
    /// `novels/<rest>` maps to `<root>/<rest>`; any other prefix lives under
    /// `<root>/.objects/<prefix>` mirroring `resolve_path`.
    pub(crate) fn resolve_prefix_dir(&self, prefix: &str) -> Result<PathBuf> {
        let trimmed=prefix.trim_end_matches('/');if trimmed.is_empty()||trimmed=="novels"{return crate::db::paths::ensure_within_archive_root(&self.root,&self.root);}self.resolve_path(&ObjectKey::try_new(trimmed)?)
    }

    pub(crate) fn metadata_for_path(&self, key: &ObjectKey, path: &Path) -> Result<Option<ObjectMetadata>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let last_modified = metadata
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from);
        Ok(Some(ObjectMetadata {
            key: key.clone(),
            size: metadata.len(),
            etag: None,
            content_type: content_type_for_path(path).map(str::to_string),
            last_modified,
        }))
    }
    pub(crate) fn metadata_for_open_file(key:&ObjectKey,file:&fs::File)->Result<ObjectMetadata>{let metadata=file.metadata()?;Ok(ObjectMetadata{key:key.clone(),size:metadata.len(),etag:None,content_type:crate::platform::content_type_for_key(key).map(str::to_owned),last_modified:metadata.modified().ok().map(chrono::DateTime::<chrono::Utc>::from)})}
    fn verified_opened_fingerprint(&self,key:&ObjectKey,file:&fs::File)->Result<(u64,String)>{let proof=self.read_file_proof(key)?.ok_or_else(||NarouError::Platform("sealed file proof missing".into()))?;let stamp=file_stamp_opened(file)?;if proof.pending||proof.stamp!=stamp{return Err(NarouError::Platform("sealed file identity/change tag conflict".into()));}Ok((stamp.size,proof.sha256))}

    /// `create_dir_all` with a per-store memo: the first write under a novel
    /// directory pays the syscall, subsequent writes to the same parent skip
    /// it entirely.
    pub(crate) fn ensure_parent_dir(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| NarouError::Platform(format!("object has no parent: {}", path.display())))?;
        if self.created_dirs.lock().contains(parent) {
            return Ok(());
        }
        fs::create_dir_all(parent)?;
        self.created_dirs.lock().insert(parent.to_path_buf());
        Ok(())
    }

    /// Closed-removal directory hygiene. Object deletion leaves empty
    /// directories (`本文/`, then the novel directory itself) behind; upstream
    /// `narou remove --with-file` removes the whole novel directory, so walk
    /// the removed object's ancestors and unlink directories while they sit
    /// inside this novel's prefix directory and turn up empty. A foreign file
    /// (never listed as an object) or a control file outside the prefix stops
    /// the walk without touching anything outside the removed novel. The
    /// caller's gate/transfer lock is still held, so no reader or writer can
    /// admit into the emptied namespace meanwhile.
    pub(crate) fn prune_empty_dirs_in_prefix(&self, prefix: &ObjectPrefix, path: &Path) -> Result<()> {
        let stop = self.resolve_prefix_dir(prefix.as_ref())?;
        let mut current = match path.parent() { Some(parent) => parent, None => return Ok(()) };
        loop {
            if !current.starts_with(&stop) { return Ok(()); }
            match fs::remove_dir(current) {
                Ok(()) => {
                    self.created_dirs.lock().remove(current);
                }
                Err(error) => {
                    match error.kind() {
                        std::io::ErrorKind::NotFound => { self.created_dirs.lock().remove(current); }
                        std::io::ErrorKind::DirectoryNotEmpty => return Ok(()),
                        _ => return Err(error.into()),
                    }
                }
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return Ok(()),
            }
        }
    }

    fn write_small_blocking(&self, key: &ObjectKey, data: Vec<u8>) -> Result<()> {
        if data.len() as u64 > MAX_SMALL_OBJECT_BYTES {
            return Err(NarouError::Platform(format!(
                "small object exceeds {} bytes: {key}",
                MAX_SMALL_OBJECT_BYTES
            )));
        }
        self.write_bytes_blocking(key, &data)
    }

    pub(crate) fn update_small_blocking<'data>(
        &self,
        key: &ObjectKey,
        mutate: &mut SmallObjectMutator<'data>,
    ) -> Result<ObjectUpdateResult> {
        let path = self.resolve_path(key)?;
        self.ensure_parent_dir(&path)?;
        crate::db::inventory::with_exclusive_file_lock(&path, || {
            let current = self.read_small_blocking(key)?;
            if let Some(data) = mutate(current.as_deref())? {
                if data.len() as u64 > MAX_SMALL_OBJECT_BYTES {
                    return Err(NarouError::Platform(format!("small object exceeds {MAX_SMALL_OBJECT_BYTES} bytes: {key}")));
                }
                self.write_bytes_blocking(key, &data)?;
            }
            Ok(ObjectUpdateResult::Committed)
        })
    }

    pub(crate) fn read_small_blocking(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        let path = self.resolve_read_path(key)?;
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(None);
        }
        if metadata.len() > MAX_SMALL_OBJECT_BYTES {
            return Err(NarouError::Platform(format!("small object exceeds {MAX_SMALL_OBJECT_BYTES} bytes: {key}")));
        }
        let mut data = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_SMALL_OBJECT_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_SMALL_OBJECT_BYTES {
            return Err(NarouError::Platform(format!("small object exceeds {MAX_SMALL_OBJECT_BYTES} bytes: {key}")));
        }
        Ok(Some(data))
    }


    /// Write without the small-object cap. Used by the SQLite store's
    /// filesystem mirror and by the one-shot legacy import, where payloads
    /// (EPUB, large sections) may exceed `MAX_SMALL_OBJECT_BYTES`.
    pub(crate) fn write_bytes_blocking(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        let path = self.resolve_path(key)?;
        self.ensure_parent_dir(&path)
            .map_err(|error| object_io_context(key, "create parent directory", error))?;
        self.forget_file_proof(key)
            .map_err(|error| object_io_context(key, "remove file proof", error))?;
        atomic_write_bytes_with(&path, data, |operation, source| object_io_error(key, operation, source))
    }

    pub(crate) fn list_direct_files(&self, prefix: &ObjectKey) -> Result<Vec<ObjectKey>> {
        let dir = self.resolve_prefix_dir(prefix.as_ref())?;
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if !path.is_file() {
                continue;
            }
            let safe_path = crate::db::paths::ensure_within_archive_root(&path, &self.root)?;
            if let Some(key) = self.logical_key_for_listed_path(prefix.as_ref(), &dir, &safe_path) {
                keys.push(key);
            }
        }
        keys.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
        Ok(keys)
    }

    /// Maps a file found under `prefix_dir` back to the logical key callers
    /// asked for. When the physical directory differs from the requested
    /// prefix (historical `‛`-escaped spelling), the emitted key is rebased
    /// under `prefix` so listings stay consistent for the caller; files under
    /// the physical `.objects/` mirror root keep their physical mapping.
    pub(crate) fn logical_key_for_listed_path(
        &self,
        prefix: &str,
        prefix_dir: &Path,
        path: &Path,
    ) -> Option<ObjectKey> {
        let logical = logical_key_for_native_path(&self.root, path)?;
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            // Whole-archive listings keep the physical mapping (`novels/…`
            // vs `.objects/…`).
            return Some(logical);
        }
        let relative = path.strip_prefix(prefix_dir).ok()?;
        if relative.starts_with(".objects") && !logical.as_ref().starts_with("novels/") {
            // `.objects/` already means "outside the novels namespace".
            return Some(logical);
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        ObjectKey::try_new(format!("{prefix}/{relative}")).ok()
    }

    pub(crate) fn recursive_files(root: &Path, current: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
        Self::recursive_files_filtered(root, current, output, &mut |_, _, _| false)
    }

    pub(crate) fn recursive_files_filtered<F>(
        root: &Path,
        current: &Path,
        output: &mut Vec<PathBuf>,
        skip: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&Path, &Path, bool) -> bool,
    {
        let safe_root = crate::db::paths::ensure_within_archive_root(root, root)?;
        let safe_current = crate::db::paths::ensure_within_archive_root(current, &safe_root)?;
        Self::recursive_files_inner(&safe_root, &safe_current, output, &mut Vec::new(), skip)
    }

    #[cfg(unix)]
    fn classify_walk_entry(
        safe_root: &Path,
        safe_current: &Path,
        path: &Path,
        file_name: &std::ffi::OsStr,
        file_type: fs::FileType,
    ) -> Result<(PathBuf, bool, bool)> {
        if !file_type.is_symlink() && (file_type.is_dir() || file_type.is_file()) {
            return Ok((safe_current.join(file_name), file_type.is_dir(), file_type.is_file()));
        }
        let safe_path = crate::db::paths::ensure_within_archive_root(path, safe_root)?;
        Ok((safe_path.clone(), safe_path.is_dir(), safe_path.is_file()))
    }

    #[cfg(windows)]
    fn classify_walk_entry(
        safe_root: &Path,
        safe_current: &Path,
        path: &Path,
        file_name: &std::ffi::OsStr,
        _file_type: fs::FileType,
    ) -> Result<(PathBuf, bool, bool)> {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        let meta = fs::symlink_metadata(path)?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && (meta.is_dir() || meta.is_file())
        {
            return Ok((safe_current.join(file_name), meta.is_dir(), meta.is_file()));
        }
        let safe_path = crate::db::paths::ensure_within_archive_root(path, safe_root)?;
        Ok((safe_path.clone(), safe_path.is_dir(), safe_path.is_file()))
    }

    #[cfg(all(not(unix), not(windows)))]
    fn classify_walk_entry(
        safe_root: &Path,
        _safe_current: &Path,
        path: &Path,
        _file_name: &std::ffi::OsStr,
        _file_type: fs::FileType,
    ) -> Result<(PathBuf, bool, bool)> {
        let safe_path = crate::db::paths::ensure_within_archive_root(path, safe_root)?;
        Ok((safe_path.clone(), safe_path.is_dir(), safe_path.is_file()))
    }

    fn recursive_files_inner<F>(
        safe_root: &Path,
        safe_current: &Path,
        output: &mut Vec<PathBuf>,
        ancestors: &mut Vec<PathBuf>,
        skip: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&Path, &Path, bool) -> bool,
    {
        if ancestors.iter().any(|path| path == safe_current) {
            return Err(NarouError::Platform("cyclic native namespace link".into()));
        }
        ancestors.push(safe_current.to_path_buf());
        let result = (|| {
            let mut entries = Vec::new();
            for entry in fs::read_dir(safe_current)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                entries.push((entry.path(), entry.file_name(), file_type));
            }
            for (path, file_name, file_type) in entries {
                let (safe_path, is_dir, is_file) = Self::classify_walk_entry(
                    safe_root,
                    safe_current,
                    &path,
                    &file_name,
                    file_type,
                )?;
                if skip(safe_root, &safe_path, is_dir) {
                    continue;
                }
                if is_dir {
                    Self::recursive_files_inner(safe_root, &safe_path, output, ancestors, skip)?;
                } else if is_file {
                    output.push(safe_path);
                }
            }
            Ok(())
        })();
        ancestors.pop();
        result
    }
}

pub(crate) fn logical_key_for_native_path(root: &Path, path: &Path) -> Option<ObjectKey> {
    let canonical_root;
    let canonical_path;
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) => {
            canonical_root = crate::db::paths::ensure_within_archive_root(root, root).ok()?;
            canonical_path = crate::db::paths::ensure_within_archive_root(path, root).ok()?;
            canonical_path.strip_prefix(&canonical_root).ok()?
        }
    }.to_string_lossy().replace('\\', "/");
    if let Some(object_relative) = relative.strip_prefix(".objects/") {
        ObjectKey::try_new(object_relative.to_string()).ok()
    } else {
        ObjectKey::try_new(format!("novels/{relative}")).ok()
    }
}

/// Make a path absolute against the current working directory.
///
/// `DownloadResult.novel_dir` is often relative (`小説データ/...`), while the
/// archive root and object keys are absolute. Materialisation must compare
/// absolute paths or the novel prefix is never found (issue #39).
#[cfg(test)]
fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(NarouError::Io)
    }
}

pub(crate) fn run_blocking<T, F>(operation: F) -> PlatformFuture<'static, Result<T>>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        Box::pin(async move {
            tokio::task::spawn_blocking(operation)
                .await
                .map_err(|error| NarouError::Platform(error.to_string()))?
        })
    } else {
        Box::pin(async move { operation() })
    }
}

pub(crate) fn run_scoped_blocking<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
        handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
    }) {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

pub(crate) const ATOMIC_OBJECT_TEMP_PREFIX: &str = ".narou-object-";

fn object_io_error(key: &ObjectKey, operation: &'static str, source: std::io::Error) -> NarouError {
    NarouError::ObjectIo {
        operation,
        key: key.as_ref().to_owned(),
        source,
    }
}

fn object_io_context(key: &ObjectKey, operation: &'static str, error: NarouError) -> NarouError {
    match error {
        NarouError::Io(source) => object_io_error(key, operation, source),
        error => error,
    }
}

pub(crate) fn atomic_write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    atomic_write_bytes_with(path, data, |_, source| NarouError::Io(source))
}

fn atomic_write_bytes_with(
    path: &Path,
    data: &[u8],
    io_error: impl Fn(&'static str, std::io::Error) -> NarouError,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NarouError::Platform(format!("object has no parent: {}", path.display())))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(ATOMIC_OBJECT_TEMP_PREFIX)
        .tempfile_in(parent)
        .map_err(|error| io_error("create temporary file", error))?;
    // NamedTempFile creates exclusively; existing links are never opened.
    for chunk in data.chunks(1024 * 1024) {
        temporary.write_all(chunk).map_err(|error| io_error("write temporary file", error))?;
    }
    temporary.as_file().sync_all().map_err(|error| io_error("sync temporary file", error))?;
    // std::fs::rename replaces an existing destination atomically on every
    // supported platform (rename(2) on Unix, MoveFileExW with
    // MOVEFILE_REPLACE_EXISTING on Windows). A preceding exists+remove would
    // only open a window where the destination is missing.
    temporary.persist(path).map_err(|error| io_error("replace destination", error.error))?;
    Ok(())
}

/// Scan-relevant projection of one durable gate file: `(closed, owner, epoch,
/// origin, migrated)`. `content_revision`, reader leases and `updated_at` never
/// affect namespace-authority scans, so ordinary content writes do not
/// invalidate the scan cache (issue #58).
/// The scan-visible projection of one gate: `(closed, owner, epoch, origin,
/// migrated)`.
type GateScanProjection = (bool, Option<String>, u64, String, bool);
fn scan_relevant(gate: &FsGate) -> GateScanProjection {
    (gate.owner.is_some(), gate.owner.clone(), gate.epoch, gate.origin_url_fold.clone(), gate.migrated)
}
fn scan_relevant_bytes(bytes: &[u8]) -> Result<GateScanProjection> {
    let gate: FsGate = serde_json::from_slice(bytes)
        .map_err(|error| NarouError::Platform(format!("invalid namespace gate: {error}")))?;
    Ok(scan_relevant(&gate))
}
fn scan_relevant_file(path: &Path) -> Result<Option<GateScanProjection>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(scan_relevant_bytes(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Write one durable FS gate file. The scan cache is invalidated only when the
/// scan-relevant projection actually changes; the seqlock turns odd before the
/// rename lands and even after the parent sync, so a reader observing a stable
/// even value across its scan proves every counted mutation is visible.
/// `content_revision`-only writes (ordinary content saves) keep the cache.
pub(crate) fn write_gate_file(archive_root: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    write_gate_file_with(archive_root, path, bytes, None)
}

/// [`write_gate_file`] with the caller's already-known previous projection.
/// `before` avoids re-reading the gate file on the hot content-write path.
pub(crate) fn write_gate_file_with(archive_root: &Path, path: &Path, bytes: &[u8], before: Option<GateScanProjection>) -> Result<()> {
    let previous = match before { Some(before) => Some(before), None => scan_relevant_file(path)? };
    let changed = previous != Some(scan_relevant_bytes(bytes)?);
    // Stage the content OUTSIDE the revision-lock critical section: the temp
    // write and its fsync need no interlock with other gate mutations.
    let parent = path.parent().ok_or_else(|| NarouError::Platform(format!("object has no parent: {}", path.display())))?;
    let mut temporary = tempfile::Builder::new().prefix(ATOMIC_OBJECT_TEMP_PREFIX).tempfile_in(parent)?;
    for chunk in bytes.chunks(1024 * 1024) { temporary.write_all(chunk)?; }
    temporary.as_file().sync_all()?;
    if changed {
        // Critical section: counter transition + rename + counter seal. Only
        // lock acquisitions and memory-speed writes are inside, so parallel
        // mutations contend for milliseconds, not for an fsync chain.
        let revision = begin_gate_revision_write(archive_root)?;
        temporary.persist(path).map_err(|error| NarouError::Io(error.error))?;
        revision.finish()?;
    } else {
        temporary.persist(path).map_err(|error| NarouError::Io(error.error))?;
    }
    sync_transfer_parent(path)?;
    Ok(())
}

/// Archive-root-relative location of the scan-invalidation seqlock. It sits
/// beside the durable gate directory so writers and the scan agree on one
/// file regardless of which root convention the caller holds.
pub(crate) fn gate_revision_path(archive_root: &Path) -> PathBuf {
    archive_root.join(".objects").join(".narou-transfer").join("gate-revision")
}
fn gate_revision_lock_path(archive_root: &Path) -> PathBuf {
    archive_root.join(".objects").join(".narou-transfer").join(".lock").join("_gate_revision")
}

/// Read the current scan-invalidation seqlock. Missing means zero; the file
/// is only ever replaced by atomic rename, so a lock-free read is a
/// consistent snapshot. Even values are stable (every counted mutation is
/// fully published); odd values mean a mutation is in flight or crashed.
pub(crate) fn read_gate_revision(archive_root: &Path) -> Result<u64> {
    match fs::read_to_string(gate_revision_path(archive_root)) {
        Ok(raw) => raw.trim().parse::<u64>().map_err(|error| NarouError::Platform(format!("invalid gate revision: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}
fn write_gate_revision(archive_root: &Path, value: u64) -> Result<()> {
    // Visibility-only write: no fsync chain. The seqlock feeds the
    // process-local scan cache, and both readers and writers agree through
    // the OS page cache on one machine; durability across a power loss is
    // irrelevant because the cache dies with the machine anyway. Every
    // persisted counter state is consistent with the gate state observed at
    // the next boot, so skipping fsyncs cannot serve a stale scan.
    let path = gate_revision_path(archive_root);
    if let Some(directory) = path.parent() { fs::create_dir_all(directory)?; }
    let parent = path.parent().ok_or_else(|| NarouError::Platform(format!("object has no parent: {}", path.display())))?;
    let mut temporary = tempfile::Builder::new().prefix(ATOMIC_OBJECT_TEMP_PREFIX).tempfile_in(parent)?;
    temporary.write_all(value.to_string().as_bytes())?;
    temporary.persist(&path).map_err(|error| NarouError::Io(error.error))?;
    Ok(())
}

/// Begin a scan-relevant gate mutation. The returned guard holds the global
/// revision lock for the entire mutation; `finish` seals the seqlock even
/// after the mutation is fully published. Seqlock protocol: the counter is
/// odd while a mutation is in flight and even once fully published, so a
/// reader observing a stable even value across its scan knows every counted
/// mutation is visible in that scan. A crashed predecessor leaves an odd
/// value; the next writer seals it first — atomic renames mean the on-disk
/// state is consistent regardless of where the crash landed. Never incorrect
/// on crash: worst case is a permanently odd counter, degrading readers to
/// the uncached scan (issue #58).
pub(crate) fn begin_gate_revision_write(archive_root: &Path) -> Result<GateRevisionWrite> {
    let lock_path = gate_revision_lock_path(archive_root);
    if let Some(parent) = lock_path.parent() { fs::create_dir_all(parent)?; }
    let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut current = read_gate_revision(archive_root)?;
    if current % 2 == 1 {
        current = current.checked_add(1).ok_or_else(|| NarouError::Platform("gate revision overflow".into()))?;
        write_gate_revision(archive_root, current)?;
    }
    let odd = current.checked_add(1).ok_or_else(|| NarouError::Platform("gate revision overflow".into()))?;
    write_gate_revision(archive_root, odd)?;
    Ok(GateRevisionWrite { archive_root: archive_root.to_path_buf(), next: odd.checked_add(1).ok_or_else(|| NarouError::Platform("gate revision overflow".into()))?, _lock: lock })
}

/// Holds the global gate-revision lock across one scan-relevant mutation.
/// Dropping without `finish` leaves the seqlock odd: safe (readers bypass
/// the cache) but degraded, so `finish` failures are reported to callers.
pub(crate) struct GateRevisionWrite {
    archive_root: PathBuf,
    next: u64,
    _lock: fs::File,
}
impl GateRevisionWrite {
    pub(crate) fn finish(self) -> Result<()> {
        write_gate_revision(&self.archive_root, self.next)
    }
}

/// Holds the global gate-revision lock across a batch of gate-file mutations
/// (mass cutover / adoption). The seqlock turns odd when the batch begins and
/// even only once at `finish`, so readers see an odd value for the entire
/// batch and never cache a half-written gate directory. Writes in the batch
/// skip the per-file parent sync because the caller performs the single
/// `.gate/` directory sync before `finish` (issue #58).
pub(crate) struct GateRevisionBatch {
    archive_root: PathBuf,
    next: u64,
    _lock: fs::File,
}
impl GateRevisionBatch {
    /// Write one `.gate/` file inside the batch: no counter work, no parent
    /// sync. The batch's odd-to-even transition plus the caller's directory
    /// sync carry the whole batch.
    pub(crate) fn write_gate(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        atomic_write_bytes(path, bytes)
    }
    /// Seal the seqlock even. Must run after the caller's `.gate/` directory
    /// sync so the published even value covers every rename in the batch.
    pub(crate) fn finish(self) -> Result<()> {
        write_gate_revision(&self.archive_root, self.next)
    }
}

/// Begin a batched gate mutation. Like [`begin_gate_revision_write`], the
/// seqlock seals a crashed predecessor first and turns odd for the duration
/// of the batch; the caller must run [`GateRevisionBatch::finish`] exactly
/// once after the `.gate/` directory sync.
pub(crate) fn begin_gate_revision_batch(archive_root: &Path) -> Result<GateRevisionBatch> {
    let lock_path = gate_revision_lock_path(archive_root);
    if let Some(parent) = lock_path.parent() { fs::create_dir_all(parent)?; }
    let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut current = read_gate_revision(archive_root)?;
    if current % 2 == 1 {
        current = current.checked_add(1).ok_or_else(|| NarouError::Platform("gate revision overflow".into()))?;
        write_gate_revision(archive_root, current)?;
    }
    let odd = current.checked_add(1).ok_or_else(|| NarouError::Platform("gate revision overflow".into()))?;
    write_gate_revision(archive_root, odd)?;
    Ok(GateRevisionBatch { archive_root: archive_root.to_path_buf(), next: odd.checked_add(1).ok_or_else(|| NarouError::Platform("gate revision overflow".into()))?, _lock: lock })
}

pub(crate) fn content_type_for_path(path: &Path) -> Option<&'static str> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(crate::platform::object_store::content_type_for_extension)
}

impl ObjectStore for NativeObjectStore {
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a crate::platform::WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(async move{run_scoped_blocking(||self.with_gate(&fence.prefix,|gate|{
        if !fence.prefix.matches(key)||gate.owner.is_some()||gate.epoch!=fence.epoch{return Err(af::gate_closed_error(&fence.prefix));}
        let result=self.update_small_blocking(key,mutate)?;if matches!(result,ObjectUpdateResult::Committed){gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;}Ok(result)
    }))})}
    fn access_fence(&self) -> Option<&dyn NovelAccessFence> { Some(self) }
    fn delete_closed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{af::parse_removal_owner(owner)?;run_scoped_blocking(||self.with_gate(prefix,|gate|{gate.require_writer(prefix,owner,epoch)?;if !prefix.matches(key){return Err(NarouError::Platform("closed deletion outside novel".into()));}let path=self.resolve_path(key)?;match fs::remove_file(&path){Ok(())=>{sync_transfer_parent(&path)?;gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}self.prune_empty_dirs_in_prefix(prefix,&path)}))})}
    fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        NativeObjectStore::read_stream_leased(self, prefix, lease, key, range)
    }
    fn native_source_path(&self, key: &ObjectKey) -> Option<std::path::PathBuf> {
        self.resolve_path(key).ok()
    }
    fn write_small_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(&fence.prefix, |gate| {
            if !fence.prefix.matches(key) || gate.owner.is_some() || gate.epoch != fence.epoch { return Err(af::gate_closed_error(&fence.prefix)); }
            self.write_small_blocking(key, data)?;gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;Ok(())
        })) })
    }
    fn delete_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(&fence.prefix, |gate| {
            if !fence.prefix.matches(key) || gate.owner.is_some() || gate.epoch != fence.epoch { return Err(af::gate_closed_error(&fence.prefix)); }
            let path = self.resolve_path(key)?;
            match fs::remove_file(&path) { Ok(()) => {sync_transfer_parent(&path)?;gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;Ok(())}, Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()), Err(error) => Err(error.into()) }
        })) })
    }

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            let path = this.resolve_read_path(&key)?;
            this.metadata_for_path(&key, &path)
        })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || this.read_small_blocking(&key))
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move{
            if let Some(fence)=run_scoped_blocking(||this.writer_for_key(&key))?{return this.write_small_guarded(&fence,&key,data).await;}
            run_blocking(move || this.write_small_blocking(&key,data)).await
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move {
            run_scoped_blocking(|| self.update_small_blocking(key, mutate))
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            let path = this.resolve_path(&key)?;
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        let this=self.clone();let request=request.clone();run_blocking(move||this.list_window_blocking(&request,request.limit.get().min(100)))
    }
}

impl AssetStore for NativeObjectStore {

    fn read_stream_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        let this = self.clone(); let key = key.clone();
        Box::pin(async move {
            Ok(run_blocking(move || this.open_range(&key, range)).await?.map(|(file, length)| native_range_stream(file, length)))
        })
    }

    fn write_stream_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !fence.prefix.matches(key) { return Err(NarouError::Platform("write outside fence".into())); }
            let stage = self.stage_stream(stream).await?;
            let result = run_scoped_blocking(|| self.with_gate(&fence.prefix, |gate| {
                if gate.owner.is_some() || gate.epoch != fence.epoch { return Err(af::gate_closed_error(&fence.prefix)); }
                self.install_verified_file(&stage.key,key)?;
                gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;Ok(())
            }));
            if result.is_err() { let _ = ObjectStore::delete(self, &stage.key).await; }
            result
        })
    }

    fn write_stream_once<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream, size: u64, sha: &'a str) -> PlatformFuture<'a, Result<WriteOnce>> {
        Box::pin(async move {
            ts::check_hash_named_key(key, sha)?;
            let stage = self.stage_stream(stream).await?;
            let result = run_scoped_blocking(|| {
                if stage.size != size || !stage.sha256.eq_ignore_ascii_case(sha) { return Err(NarouError::Platform("write-once size/SHA mismatch".into())); }
                self.with_transfer_lock(&format!("write-once/{key}"), || {
                    let target = self.resolve_path(key)?; self.ensure_parent_dir(&target)?;
                    if target.exists() {
                        if fs::metadata(&target)?.len() != size || !file_sha256(&target)?.eq_ignore_ascii_case(sha) { return Err(NarouError::Platform("write-once existing bytes differ".into())); }
                        return Ok(WriteOnce::Reused);
                    }
                    fs::hard_link(self.resolve_path(&stage.key)?, &target)?; sync_transfer_parent(&target)?; Ok(WriteOnce::Written)
                })
            });
            let cleanup = ObjectStore::delete(self, &stage.key).await;
            match result { Ok(value) => { cleanup?; Ok(value) }, Err(error) => Err(error) }
        })
    }

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        ObjectStore::stat(self, key)
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move {
            let opened = run_blocking({
                let this = this.clone();
                let key = key.clone();
                move || {
                    let staging = crate::platform::staging::is_staging_key(&key);
                    let file = match fs::OpenOptions::new().read(true).write(staging).open(this.resolve_read_path(&key)?) {
                        Ok(file) => file,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                        Err(error) => return Err(error.into()),
                    };
                    if !file.metadata()?.is_file() { return Ok(None); }
                    if staging {
                        fs2::FileExt::lock_shared(&file)?;
                        renew_stage_file(&file, &key, &this.stage_leases)?;
                    }
                    let expected = file.metadata()?.len();
                    Ok(Some((file, staging, expected)))
                }
            }).await?;
            let Some((file, staging, expected)) = opened else { return Ok(None); };
            let cache = this.stage_leases.clone();
            let stream: AssetStream = Box::pin(futures::stream::try_unfold(
                (file, 0u64, key, cache), move |(mut file, position, key, cache)| run_blocking(move || {
                    use std::io::Read;
                    let mut chunk = vec![0_u8; 64 * 1024];
                    let length = file.read(&mut chunk)?;
                    if length == 0 {
                        if position != expected { return Err(NarouError::Platform("asset file was truncated".into())); }
                        return Ok(None);
                    }
                    if staging {
                        renew_stage_file(&file, &key, &cache)?;
                    }
                    let next = position.checked_add(length as u64).filter(|&value| value <= expected)
                        .ok_or_else(|| NarouError::Platform("asset file changed while reading".into()))?;
                    chunk.truncate(length);
                    Ok(Some((chunk, (file, next, key, cache))))
                }),
            ));
            Ok(Some(stream))
        })
    }

    fn read_range<'a>(
        &'a self, key: &'a ObjectKey, range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            use std::io::{Read, Seek, SeekFrom};
            let length = crate::platform::staging::validate_range(&range)?;
            let path = this.resolve_read_path(&key)?;
            let staging = crate::platform::staging::is_staging_key(&key);
            let mut file = match fs::OpenOptions::new().read(true).write(staging).open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if staging {
                fs2::FileExt::lock_shared(&file)?;
                renew_stage_file(&file, &key, &this.stage_leases)?;
            }
            if range.end > file.metadata()?.len() {
                return Err(NarouError::Platform("asset range exceeds object".into()));
            }
            file.seek(SeekFrom::Start(range.start))?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes)?;
            Ok(Some(bytes))
        })
    }

    fn stage_stream<'a>(&'a self, stream: AssetStream) -> PlatformFuture<'a, Result<crate::platform::staging::StagedAsset>> {
        Box::pin(async move {
            let this = self.clone();
            run_blocking(move || collect_expired_stage_files(&this.root)).await?;
            let stage=crate::platform::staging::stage(self, stream).await?;
            run_scoped_blocking(||self.save_verified_file(&stage.key,stage.size,&stage.sha256))?;Ok(stage)
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        mut stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        Box::pin(async move {
            if let Some(fence)=run_scoped_blocking(||self.writer_for_key(key))?{return self.write_stream_guarded(&fence,key,stream).await;}
            let path = run_blocking({
                let this = this.clone();
                let key = key.clone();
                move || this.resolve_path(&key)
            })
            .await?;
            let parent = path
                .parent()
                .ok_or_else(|| NarouError::Platform(format!("asset has no parent: {key}")))?
                .to_path_buf();
            run_blocking({
                let this = this.clone();
                let parent = parent.clone();
                move || {
                    // ensure_parent_dir takes the file path; pass a synthetic
                    // child so the parent itself is the directory memoized.
                    this.ensure_parent_dir(&parent.join(".narou-dir"))
                }
            })
            .await?;
            let mut temporary = run_blocking(move || {
                let temporary = tempfile::Builder::new().prefix(".narou-asset-").tempfile_in(parent)?;
                // Windows byte-range locks permit writes through the locking
                // handle only. Keep that handle for all spool I/O.
                fs2::FileExt::lock_exclusive(temporary.as_file())?;
                Ok(temporary)
            }).await?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                temporary = run_blocking(move || {
                    temporary.as_file_mut().write_all(&chunk)?;
                    Ok(temporary)
                }).await?;
            }
            // Complete the writes and sync the same handle before publication.
            // No Tokio file buffer or detached pending write survives this point.
            let origin_store=self.clone();let origin_key=key.clone();
            run_blocking(move || {
                if origin_store.writer_for_key(&origin_key)?.is_some(){return Err(NarouError::Platform("novel_epoch_conflict: admission changed during stream".into()));}
                temporary.as_file().sync_all()?;
                temporary.persist(path).map_err(|error| NarouError::Io(error.error))?;
                Ok(())
            }).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let source = source.clone();
        let destination = destination.clone();
        run_blocking(move || {
            let source_path = this.resolve_read_path(&source)?;
            let destination = this.resolve_path(&destination)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let parent = destination.parent().ok_or_else(|| NarouError::Platform("copy destination has no parent".into()))?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            let staging = crate::platform::staging::is_staging_key(&source);
            let mut input = fs::OpenOptions::new().read(true).write(staging).open(source_path)?;
            if staging {
                fs2::FileExt::lock_shared(&input)?;
                renew_stage_file(&input, &source, &this.stage_leases)?;
            }
            std::io::copy(&mut input, temporary.as_file_mut())?;
            temporary.as_file().sync_all()?;
            temporary.persist(destination).map_err(|error| NarouError::Io(error.error))?;
            Ok(())
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let source_key = source.clone();
        let destination_key = destination.clone();
        run_blocking(move || {
            let source = this.resolve_read_path(&source_key)?;
            let destination = this.resolve_path(&destination_key)?;
            if !source.exists() {
                return Ok(());
            }
            let owned_version_dir = if crate::application::webui::diff::is_diff_cache_section(&destination_key) {
                let parent = destination.parent().ok_or_else(|| {
                    NarouError::Platform("cache section has no version directory".to_string())
                })?;
                if let Some(cache_root) = parent.parent() {
                    fs::create_dir_all(cache_root)?;
                }
                // create_dir (not create_dir_all) proves this move owns the
                // new version directory, even when another writer races it.
                match fs::create_dir(parent) {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && parent.is_dir() => false,
                    Err(error) => return Err(error.into()),
                }
            } else {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                false
            };
            let result = match fs::rename(&source, &destination) {
                Ok(()) => Ok(()),
                Err(_) => fs::copy(&source, &destination).and_then(|_| fs::remove_file(&source)),
            };
            if result.is_err() && owned_version_dir
                && let Some(parent) = destination.parent()
            {
                // Never recursively delete or remove a directory that
                // already existed. Nonempty/concurrently written dirs stay.
                let _ = fs::remove_dir(parent);
            }
            result.map_err(NarouError::from)
        })
    }
}

/// Native object storage selected by the storage backend marker.
///
/// `Yaml` mode keeps the pure-filesystem store; `Sqlite` mode wraps the
/// `objects`/`object_chunks` tables with a filesystem mirror so rollback and
/// narou.rb reads keep working without an export step.
#[derive(Debug, Clone)]
pub enum NativeStore {
    Fs(NativeObjectStore),
    Sqlite(crate::native::sqlite::object_store::SqliteObjectStore),
}

impl NativeStore {
    /// Resolve the store for the library rooted at `narou_root` (the
    /// directory that contains `.narou`). Falls back to the filesystem store
    /// when SQLite is unavailable or not opted in.
    pub fn for_narou_root(narou_root: &Path) -> Result<Self> {
        let archive_root = narou_root.join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let mirror = NativeObjectStore::from_root(archive_root)?;
        let narou_dir = narou_root.join(".narou");
        // Converge any interrupted management-backend switch (crash after
        // the marker flip but before/inside its transaction) before the
        // store variant is chosen.
        crate::native::sqlite::export_yaml::reconcile_cutover_intent(&narou_dir)?;
        if let Some(state) = crate::native::sqlite::state::active_for(&narou_dir) {
            return Ok(Self::Sqlite(
                crate::native::sqlite::object_store::SqliteObjectStore::new(
                    state.conn_ref().clone(),
                    mirror,
                ),
            ));
        }
        Ok(Self::Fs(mirror))
    }

    /// Resolve using the process-wide database root (same lookup
    /// `NativeObjectStore::new` performs).
    pub fn for_current_root() -> Result<Self> {
        let root = crate::db::with_database(|db| Ok(db.archive_root().to_path_buf()))?;
        let narou_root = root
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        Self::for_narou_root(&narou_root)
    }


    /// List immediate file children without walking histories or illustrations.
    /// SQLite rows and legacy filesystem-only files are both included.
    pub fn list_direct_files_sync(&self, prefix: &ObjectKey) -> Result<Vec<ObjectKey>> {
        match self {
            Self::Fs(store) => store.list_direct_files(prefix),
            Self::Sqlite(store) => store.list_direct_files(prefix),
        }
    }


    /// Rebuild a deleted mirror file from the stored object.
    ///
    /// Returns `false` when the backend has no second copy (filesystem mode) or
    /// does not know the key.
    pub fn restore_mirror_file(&self, path: &Path) -> Result<bool> {
        match self {
            NativeStore::Fs(_) => Ok(false),
            NativeStore::Sqlite(store) => store.restore_mirror_file(path),
        }
    }
}

/// Map a novel directory below the archive root to its logical object prefix.
///
/// Commands that must reach the same stored objects the downloader wrote
/// (SQLite mode keeps them in `objects` even when `mirror-files=false` and the
/// folder was never written) resolve the prefix from the on-disk layout the
/// record describes, so the logical keys always line up with what
/// [`NovelObjectKeys`](crate::platform::NovelObjectKeys) would produce.
pub fn novel_object_prefix(archive_root: &Path, novel_dir: &Path) -> Option<ObjectKey> {
    logical_key_for_native_path(archive_root, novel_dir)
}

/// Map any file below the archive root to its logical object key.
pub fn object_key_for_file(archive_root: &Path, path: &Path) -> Option<ObjectKey> {
    logical_key_for_native_path(archive_root, path)
}

/// Whether the active storage backend still holds the novel (`toc.yaml`).
///
/// `mirror-files=false` leaves a valid managed novel without any folder under
/// `小説データ/`; deciding "this record is stale" from `Path::exists` alone
/// deletes content that only lives in the database. The filesystem store maps
/// logical keys to the same folder, so YAML mode keeps the previous answer.
///
/// Errors are returned instead of folded into `false`: callers delete the
/// library record when this reports "not stored", so an unreadable store must
/// never look like proven absence.
pub fn novel_content_stored(archive_root: &Path, novel_dir: &Path) -> Result<bool> {
    let prefix = novel_object_prefix(archive_root, novel_dir).ok_or_else(|| {
        NarouError::Platform(format!(
            "novel directory is outside the archive root: {}",
            novel_dir.display()
        ))
    })?;
    let narou_root = archive_root.parent().ok_or_else(|| {
        NarouError::Platform(format!(
            "archive root has no library root: {}",
            archive_root.display()
        ))
    })?;
    let store = NativeStore::for_narou_root(narou_root)?;
    let toc_key = prefix.join("toc.yaml")?;
    Ok(futures::executor::block_on(ObjectStore::stat(&store, &toc_key))?.is_some())
}

/// Ensure a mirror file exists, rebuilding it from storage when the backend
/// still holds the object. Returns `true` when the file is readable afterwards.
///
/// Files deleted outside narou leave SQLite mode with a stored object but no
/// file for the converter and narou.rb to read; read paths call this before
/// reporting a missing file. The library root comes from the working directory
/// rather than the database, so this also works before any command initialized
/// the database.
pub fn ensure_mirror_file(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    let Ok(inventory) = crate::db::inventory::Inventory::with_default_root() else {
        return false;
    };
    NativeStore::for_narou_root(inventory.root_dir())
        .and_then(|store| store.restore_mirror_file(path))
        .unwrap_or(false)
}



#[derive(Debug,Clone,serde::Serialize,serde::Deserialize)]
struct NativeRemoteReference{physical:String,size:u64,sha256:String,logical:String,prefix:String,#[serde(default)]etag:String,#[serde(default,skip_serializing_if="Option::is_none")]legacy_gc:Option<String>}
#[derive(Clone,serde::Serialize,serde::Deserialize)]
struct NativeRemoteReservation{owner:String,epoch:u64,reference:NativeRemoteReference,pool:bool,#[serde(default)]revision:u64,#[serde(default)]preparation:Option<NativeRemotePreparation>}
#[derive(Clone,Copy,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
enum NativeRemotePhase{Probe,Uploading,Verifying,Ready}
#[derive(Clone,Copy,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
enum NativeRemoteAction{Probe,Upload,Seal,Verify,Ready}
#[derive(Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
struct NativeRemoteOperation{id:String,action:NativeRemoteAction,offset:u64,length:u64,done:bool,ready:bool,processed:u64}
#[derive(Clone,serde::Serialize,serde::Deserialize)]
struct NativeRemotePreparation{source:String,source_generation:String,target_url:String,remote_stage:String,phase:NativeRemotePhase,cursor:StageCursor,etag:Option<String>,operation:Option<NativeRemoteOperation>}
fn empty_remote_cursor()->StageCursor{StageCursor{confirmed_bytes:0,hash_state:Some(ts::encode_sha_state(&sha2::Sha256::new())),sealed:false,crc32:0,storage_position:1}}
fn remote_preparation_identity(owner:&str,epoch:u64,stage:&ObjectKey,key:&ObjectKey,sha:&str)->Result<(String,ObjectKey)>{
    let mut hash=sha2::Sha256::new();hash.update(b"native-v2-remote-preparation\0");hash.update(epoch.to_be_bytes());for value in [owner,stage.as_ref(),key.as_ref()]{hash.update((value.len() as u64).to_be_bytes());hash.update(value.as_bytes());}for byte in sha.bytes(){hash.update([byte.to_ascii_lowercase()]);}let id=hex::encode(hash.finalize());let key=ObjectKey::try_new(format!(".narou-transfer/.remote/{id}"))?;Ok((id,key))
}
fn remote_operation_key(id:&str,operation:&str)->Result<ObjectKey>{let digest=sha2::Sha256::digest(operation.as_bytes());ObjectKey::try_new(format!(".narou-transfer/.remote_ops/{id}/{}",hex::encode(digest)))}
/// The one object-key namespace that maps logical keys to their remote
/// (S3/deletion-marker) provenance. Unlike every other `.narou-transfer/`
/// subtree these markers are *published metadata*: both the SQL mirror and
/// the filesystem backend read them verbatim to resolve remote reads, so an
/// export/cutover must carry them across (they are also legitimate
/// fingerprints targets) while sessions, stage ledgers, locks and GC data
/// under the sibling subtrees must never leave the store.
pub(crate) const REMOTE_REFERENCE_PREFIX: &str = ".narou-transfer/.refs/";

/// True exactly for committed remote-reference markers (`physical` alias
/// rows and `physical=""` deletion tombstones); never true for reservations,
/// sessions, locks, proofs or staging payloads.
pub(crate) fn is_remote_reference_key(key:&ObjectKey)->bool{key.as_ref().starts_with(REMOTE_REFERENCE_PREFIX)}

fn remote_reference_key(key:&ObjectKey)->Result<ObjectKey>{ObjectKey::try_new(format!("{REMOTE_REFERENCE_PREFIX}{key}"))}
#[path="object_store_remote_prepare.rs"]
mod remote_prepare;
#[path="object_store_identity.rs"]
mod identity;
#[path="object_store_gate_origin.rs"]
mod gate_origin;
pub(crate) use gate_origin::{read_fs_gate_origin,rebind_fs_gate_origin_locked};
pub(crate) use identity::{bind_namespace_origin,NativeNamespaceKey,NativeNamespacePaths,read_sql_gate_handoff,read_fs_gate_handoff_raw,record_fs_gate_handoff};
#[cfg(test)]
pub(crate) use identity::{namespace_physical_overlap,record_sql_gate_handoff};
impl NativeStore{
    pub fn open_leased_asset<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<(ObjectMetadata,AssetStream)>>>{match self{Self::Fs(store)=>store.open_leased_asset(prefix,lease,key,range),Self::Sqlite(store)=>store.open_leased_asset(prefix,lease,key,range)}}
    pub fn transfer_fingerprint<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<(u64,String)>>>{Box::pin(async move{
        match self{Self::Fs(store)=>run_scoped_blocking(||store.trusted_file_fingerprint(key)),Self::Sqlite(store)=>run_scoped_blocking(||store.transfer_fingerprint(key))}
    })}
    pub fn stored_encoding<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<crate::platform::ObjectEncoding>>>{Box::pin(async move{
        match self{Self::Fs(_)=>Ok(None),Self::Sqlite(store)=>run_scoped_blocking(||store.stored_encoding(key))}
    })}
    pub fn source_generation<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<String>>>{Box::pin(async move{
        match self{Self::Fs(store)=>run_scoped_blocking(||store.file_generation(key)),Self::Sqlite(store)=>run_scoped_blocking(||store.source_generation(key))}
    })}
    pub fn path_for_canonical_key(&self,key:&ObjectKey)->Result<PathBuf>{match self{Self::Fs(store)=>store.resolve_path(key),Self::Sqlite(store)=>store.path_for_canonical_key(key)}}
    pub fn path_for_origin(&self,prefix:&ObjectPrefix,key:&ObjectKey)->Result<PathBuf>{if !prefix.matches(key){return Err(NarouError::Platform("physical path outside original canonical origin".into()));}self.path_for_canonical_key(key)}
    pub fn novel_directory(&self,prefix:&ObjectPrefix)->Result<PathBuf>{self.path_for_canonical_key(&ObjectKey::try_new(prefix.as_ref().trim_end_matches('/'))?)}
    pub fn with_reader_transaction<T>(&self,prefix:&ObjectPrefix,lease:&GateLease,operation:impl FnOnce(&rusqlite::Connection)->Result<T>)->Result<T>{
        match self{Self::Sqlite(store)=>store.with_reader_transaction(prefix,lease,operation),Self::Fs(_)=>Err(NarouError::Platform("SQL reader transaction requires SQLite backend".into()))}
    }
    pub fn with_open_fence_transaction<T>(&self,fence:&crate::platform::WriteFence,operation:impl FnOnce(&rusqlite::Connection)->Result<T>)->Result<T>{
        match self{Self::Sqlite(store)=>store.with_open_fence_transaction(fence,operation),Self::Fs(_)=>Err(NarouError::Platform("SQL transaction requires SQLite backend".into()))}
    }
    pub fn with_open_writes<T>(&self,fences:&[crate::platform::WriteFence],operation:impl FnOnce()->Result<T>)->Result<T>{
        fn locked(store:&NativeStore,fences:&[&crate::platform::WriteFence],operation:&mut dyn FnMut()->Result<()>)->Result<()>{
            if let Some((first,rest))=fences.split_first(){store.with_open_write(first,||locked(store,rest,operation))}else{operation()}
        }
        let mut ordered=fences.iter().collect::<Vec<_>>();ordered.sort_by(|a,b|a.prefix.as_ref().cmp(b.prefix.as_ref()));
        for pair in ordered.windows(2){if pair[0].prefix==pair[1].prefix&&pair[0].epoch!=pair[1].epoch{return Err(NarouError::Platform("conflicting writer epochs".into()));}}ordered.dedup_by(|a,b|a.prefix==b.prefix);
        let mut operation=Some(operation);let mut result=None;locked(self,&ordered,&mut||{result=Some(operation.take().expect("one transaction")()?);Ok(())})?;result.ok_or_else(||NarouError::Platform("metadata transaction produced no result".into()))
    }
    pub fn with_open_write<T>(&self,fence:&crate::platform::WriteFence,operation:impl FnOnce()->Result<T>)->Result<T>{
        match self{Self::Fs(store)=>store.with_transfer_lock(fence.prefix.as_ref().trim_end_matches('/'),||{store.require_open_writer_locked(fence)?;operation()}),Self::Sqlite(store)=>store.with_open_write(fence,operation)}
    }
    pub fn with_closed_write<T>(&self,prefix:&ObjectPrefix,owner:&str,epoch:u64,operation:impl FnOnce()->Result<T>)->Result<T>{
        match self{Self::Fs(store)=>store.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{let mut gate:FsGate=serde_json::from_slice(&fs::read(store.gate_path(prefix)?)?).map_err(|error|NarouError::Platform(error.to_string()))?;match store.live_sql_authority(prefix,gate.epoch,&gate.origin_url_fold)?{SqlGateAuthority::Superseded|SqlGateAuthority::Conflict=>return Err(af::gate_closed_error(prefix)),_=>{}}gate.require_writer(prefix,owner,epoch)?;operation()}),Self::Sqlite(store)=>store.with_closed_write(prefix,owner,epoch,operation)}
    }
    fn commit_remote_reference(&self,prefix:&ObjectPrefix,owner:Option<&str>,epoch:u64,key:&ObjectKey,reference:Option<&NativeRemoteReference>,expected_sha:Option<&str>)->Result<()>{
        if !prefix.matches(key){return Err(NarouError::Platform("remote reference outside fence".into()));}let refkey=remote_reference_key(key)?;
        let bytes=reference.map(serde_json::to_vec).transpose().map_err(|error|NarouError::Platform(error.to_string()))?;
        match self{Self::Fs(store)=>store.with_gate(prefix,|gate|{
            if let Some(owner)=owner{gate.require_writer(prefix,owner,epoch)?;}else if gate.owner.is_some()||gate.epoch!=epoch{return Err(af::gate_epoch_error(prefix,epoch,gate.epoch));}
            if let Some(sha)=expected_sha{let current=store.read_small_blocking(&refkey)?;if let Some(current)=current{let row:NativeRemoteReference=serde_json::from_slice(&current).map_err(|error|NarouError::Platform(error.to_string()))?;if !row.sha256.eq_ignore_ascii_case(sha){return Err(NarouError::Platform("recovery reference fingerprint conflict".into()));}}else{return Ok(());}}
            if let Some(bytes)=bytes{store.write_small_blocking(&refkey,bytes)?;}else{let path=store.resolve_path(&refkey)?;match fs::remove_file(path){Ok(())=>{},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}}gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;Ok(())
        }),Self::Sqlite(store)=>store.commit_remote_reference(prefix,owner,epoch,&refkey,bytes.as_deref(),expected_sha)}
    }
    async fn remote_reference(&self,key:&ObjectKey)->Result<Option<NativeRemoteReference>>{
        ObjectStore::read_small(self,&remote_reference_key(key)?).await?.map(|bytes|serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid remote reference: {error}")))).transpose()
    }

    /// Atomic canonical-local open: never resolves a canonical reference and
    /// never serves stale mirror bytes for a referenced key. The reference
    /// check shares the backend's own lock boundary (`with_gate` +
    /// `with_transfer_lock` on FS, one locked SQLite connection on DB).
    pub fn open_local_leased_asset<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<(ObjectMetadata,AssetStream)>>>{
        match self{Self::Fs(store)=>store.open_local_leased_asset(prefix,lease,key,range),Self::Sqlite(store)=>store.open_local_leased_asset(prefix,lease,key,range)}
    }
    /// Unleased canonical-local open under the prefix's transfer lock.
    /// `Ok(None)` covers missing keys and referenced keys alike.
    pub fn local_asset_stream<'a>(&'a self,prefix:&'a ObjectPrefix,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{
        match self{Self::Fs(store)=>store.local_asset_stream(prefix,key,range),Self::Sqlite(store)=>store.local_asset_stream(prefix,key,range)}
    }
    /// Existence probe: `true` while a canonical reference backs `key`.
    fn has_remote_reference(&self,key:&ObjectKey)->PlatformFuture<'_,Result<bool>>{
        match self{Self::Fs(store)=>{let store=store.clone();let key=key.clone();Box::pin(async move{run_blocking(move||store.ref_marker_exists(&key)).await})},Self::Sqlite(store)=>{let key=key.clone();Box::pin(async move{run_scoped_blocking(move||store.has_remote_reference(&key))})}}
    }

}
/// Borrowed canonical install identity: target key, byte size, and SHA-256.
struct InstallContent<'a>{key:&'a ObjectKey,size:u64,sha:&'a str}

/// Borrowed staged-asset identity: sealed stage key, canonical target key, SHA-256.
pub struct PreparedAsset<'a>{pub stage:&'a ObjectKey,pub key:&'a ObjectKey,pub sha256:&'a str}

#[derive(Clone)]
struct NativeRemoteStore{primary:Arc<NativeStore>,remote:Arc<S3Store>}
impl NativeRemoteStore{
    async fn install(&self,prefix:&ObjectPrefix,owner:Option<&str>,epoch:u64,stage:&ObjectKey,content:InstallContent<'_>)->Result<()>{
        ts::check_hash_named_key(content.key,content.sha)?;let pool=self.remote.illustration_pool_enabled_for(content.key);let nonce=crate::platform::staging::new_staging_key()?;let id=nonce.as_ref().strip_prefix(crate::platform::staging::STAGING_PREFIX).expect("stage prefix");
        let physical=if pool{ts::illustration_pool_key(content.key)?}else{ObjectKey::try_new(format!("content/{id}"))?};
        let mut row=NativeRemoteReference{physical:physical.to_string(),size:content.size,sha256:content.sha.into(),logical:content.key.to_string(),prefix:prefix.as_ref().into(),etag:String::new(),legacy_gc:None};
        let reservation=NativeRemoteReservation{owner:owner.unwrap_or(id).into(),epoch,reference:row.clone(),pool,revision:0,preparation:None};let reservation_key=ObjectKey::try_new(format!(".narou-transfer/.remote/{id}"))?;
        ObjectStore::write_small(self.primary.as_ref(),&reservation_key,serde_json::to_vec(&reservation).map_err(|error|NarouError::Platform(error.to_string()))?).await?;
        let stream=self.primary.read_stream(stage).await?.ok_or_else(||NarouError::NotFound(stage.to_string()))?;
        let(_,etag)=self.remote.write_stream_once_verified(&physical,stream,content.size,content.sha).await?;row.etag=etag;
        run_scoped_blocking(||self.primary.commit_remote_reference(prefix,owner,epoch,content.key,Some(&row),None))?;Ok(())
    }
    async fn read_remote_stream(&self,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<AssetStream>>{
        if let Some(reference)=self.primary.remote_reference(key).await?{if reference.physical.is_empty(){return Ok(None);}
            if reference.logical!=key.as_ref()||reference.etag.is_empty(){return Err(NarouError::Platform("immutable remote alias generation proof missing".into()));}let end=if range.end==u64::MAX{reference.size}else{range.end};if range.start>end||end>reference.size{return Err(NarouError::Platform("remote alias range exceeds object".into()));}return self.remote.read_stream_range_if_match(&ObjectKey::try_new(reference.physical)?,range.start..end,&reference.etag).await;}self.remote.read_stream_range(key,range).await
    }
}
impl ObjectStore for NativeRemoteStore{
    fn delete_closed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{af::parse_removal_owner(owner)?;let row=NativeRemoteReference{physical:String::new(),size:0,sha256:String::new(),logical:key.to_string(),prefix:prefix.as_ref().into(),etag:String::new(),legacy_gc:None};run_scoped_blocking(||self.primary.commit_remote_reference(prefix,Some(owner),epoch,key,Some(&row),None))})}
    fn retire_legacy_guarded<'a>(&'a self,fence:&'a crate::platform::WriteFence,logical:&'a ObjectKey,pool:&'a ObjectKey,size:u64,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(logical){return Err(NarouError::Platform("legacy retirement outside captured fence".into()));}
        if ts::illustration_pool_key(logical)?!=*pool{return Err(NarouError::Platform("explicit illustration pool identity mismatch".into()));}let pool_etag=self.remote.verify_immutable_object(pool,size,sha).await?;
        let etag=self.remote.legacy_etag(logical).await?;
        let row=NativeRemoteReference{physical:pool.to_string(),size,sha256:sha.to_ascii_lowercase(),logical:logical.to_string(),prefix:fence.prefix.as_ref().into(),etag:pool_etag,legacy_gc:etag.clone()};
        // Ref and durable GC intent share this exact JSON/SQL epoch boundary.
        run_scoped_blocking(||self.primary.commit_remote_reference(&fence.prefix,None,fence.epoch,logical,Some(&row),None))?;
        if let Some(etag)=etag{self.remote.delete_legacy_if_match(logical,pool,&etag).await?;}Ok(())
    })}
    fn access_fence(&self)->Option<&dyn NovelAccessFence>{Some(self.primary.as_ref())}
    fn read_stream_leased<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{
        self.primary.renew_reader(prefix,lease,af::READER_LEASE_SECS).await?;if !prefix.matches(key){return Err(NarouError::Platform("reader outside novel".into()));}let stream=self.read_remote_stream(key,range).await?;self.primary.renew_reader(prefix,lease,af::READER_LEASE_SECS).await?;Ok(stream)
    })}
    fn stat<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<ObjectMetadata>>>{Box::pin(async move{
        if let Some(reference)=self.primary.remote_reference(key).await?{if reference.physical.is_empty(){return Ok(None);}return Ok(Some(ObjectMetadata{key:key.clone(),size:reference.size,etag:Some(reference.sha256),content_type:crate::platform::content_type_for_key(key).map(str::to_owned),last_modified:None}));}ObjectStore::stat(self.remote.as_ref(),key).await
    })}
    fn read_small<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<Vec<u8>>>>{Box::pin(async move{
        let Some(mut stream)=self.read_remote_stream(key,0..u64::MAX).await?else{return Ok(None);};let mut bytes=Vec::new();while let Some(chunk)=stream.next().await{let chunk=chunk?;if bytes.len().saturating_add(chunk.len()) as u64>MAX_SMALL_OBJECT_BYTES{return Err(NarouError::Platform("small remote object exceeds admission".into()));}bytes.extend_from_slice(&chunk);}Ok(Some(bytes))
    })}
    fn write_small<'a>(&'a self,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if ts::hash_named_digest(key).is_some()||crate::platform::is_reserved_key(key){return self.remote.write_small(key,data).await;}Err(NarouError::Platform("mutable S3 write requires captured WriteFence".into()))
    })}
    fn write_small_guarded<'a>(&'a self,fence:&'a crate::platform::WriteFence,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{
        Box::pin(async move{if data.len() as u64>MAX_SMALL_OBJECT_BYTES{return Err(NarouError::Platform("small write exceeds cap".into()));}self.write_stream_guarded(fence,key,Box::pin(futures::stream::once(async move{Ok(data)}))).await})
    }
    fn delete<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{if crate::platform::is_reserved_key(key){return ObjectStore::delete(self.remote.as_ref(),key).await;}Err(NarouError::Platform("canonical S3 deletion requires captured WriteFence".into()))})}
    fn delete_guarded<'a>(&'a self,fence:&'a crate::platform::WriteFence,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let row=NativeRemoteReference{physical:String::new(),size:0,sha256:String::new(),logical:key.to_string(),prefix:fence.prefix.as_ref().into(),etag:String::new(),legacy_gc:None};
        run_scoped_blocking(||self.primary.commit_remote_reference(&fence.prefix,None,fence.epoch,key,Some(&row),None))
    })}
    fn list_page<'a>(&'a self,request:&'a ObjectListRequest)->PlatformFuture<'a,Result<ObjectListPage>>{Box::pin(async move{
        type Fields=(u64,Option<String>,Option<String>,Option<chrono::DateTime<chrono::Utc>>);
        let cap=request.limit.get().min(100);let mut window=request.clone();window.limit=std::num::NonZeroUsize::new(cap+1).expect("lookahead");let raw=self.remote.list_page_keyset(&window).await?;
        let mut refs=ObjectListRequest::new(ObjectPrefix::new(format!("{REMOTE_REFERENCE_PREFIX}{}",request.prefix.as_ref()))?,window.limit);if let Some(cursor)=&request.cursor{refs=refs.after(format!("{REMOTE_REFERENCE_PREFIX}{cursor}"));}
        let indexed=self.primary.list_page(&refs).await?;let refs_more=indexed.next_cursor.as_deref().map(|key|key.strip_prefix(REMOTE_REFERENCE_PREFIX).map(str::to_owned).ok_or_else(||NarouError::Platform("remote reference cursor left its index".into()))).transpose()?;
        let cutoff=raw.next_cursor.into_iter().chain(refs_more).min();let mut map=std::collections::BTreeMap::<ObjectKey,Fields>::new();
        let overlay=|map:&mut std::collections::BTreeMap<ObjectKey,Fields>,key:ObjectKey,reference:NativeRemoteReference|->Result<()>{if reference.logical!=key.as_ref(){return Err(NarouError::Platform("remote reference logical identity mismatch".into()));}
            if reference.physical.is_empty(){map.remove(&key);}else{let content_type=crate::platform::content_type_for_key(&key).map(str::to_owned);map.insert(key,(reference.size,Some(reference.sha256),content_type,None));}Ok(())};
        for row in raw.objects{if cutoff.as_deref().is_some_and(|end|row.key.as_ref()>end){continue;}
            if let Some(reference)=self.primary.remote_reference(&row.key).await?{overlay(&mut map,row.key,reference)?;}else{map.insert(row.key,(row.size,row.etag,row.content_type,row.last_modified));}}
        for row in indexed.objects{let logical=row.key.as_ref().strip_prefix(REMOTE_REFERENCE_PREFIX).ok_or_else(||NarouError::Platform("remote reference row left its index".into()))?;if cutoff.as_deref().is_some_and(|end|logical>end){continue;}let key=ObjectKey::try_new(logical)?;if !request.prefix.matches(&key){return Err(NarouError::Platform("remote reference list left its novel scope".into()));}let reference=self.primary.remote_reference(&key).await?.ok_or_else(||NarouError::Platform("remote reference disappeared during inventory".into()))?;overlay(&mut map,key,reference)?;}
        let next_cursor=if map.len()>cap{map.keys().nth(cap-1).map(ToString::to_string)}else{cutoff};let objects=map.into_iter().take(cap).map(|(key,(size,etag,content_type,last_modified))|ObjectMetadata{key,size,etag,content_type,last_modified}).collect();Ok(ObjectListPage{objects,next_cursor})
    })}
}
impl AssetStore for NativeRemoteStore{
    fn stat<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<ObjectMetadata>>>{ObjectStore::stat(self,key)}
    fn read_stream<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(self.read_remote_stream(key,0..u64::MAX))}
    fn read_stream_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(self.read_remote_stream(key,range))}
    fn read_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<Vec<u8>>>>{Box::pin(async move{
        let length=crate::platform::staging::validate_range(&range)?;let Some(mut stream)=self.read_remote_stream(key,range).await?else{return Ok(None);};let mut bytes=Vec::with_capacity(length);while let Some(chunk)=stream.next().await{let chunk=chunk?;if bytes.len().saturating_add(chunk.len())>length{return Err(NarouError::Platform("remote range overflow".into()));}bytes.extend_from_slice(&chunk);}if bytes.len()!=length{return Err(NarouError::Platform("remote range truncated".into()));}Ok(Some(bytes))
    })}
    fn write_stream<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{if ts::hash_named_digest(key).is_some()||crate::platform::is_reserved_key(key){self.remote.write_stream(key,stream).await}else{Err(NarouError::Platform("mutable S3 stream requires captured WriteFence".into()))}})}
    fn write_stream_guarded<'a>(&'a self,fence:&'a crate::platform::WriteFence,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let stage=self.primary.stage_stream(stream).await?;let result=self.install(&fence.prefix,None,fence.epoch,&stage.key,InstallContent{key,size:stage.size,sha:&stage.sha256}).await;let cleanup=ObjectStore::delete(self.primary.as_ref(),&stage.key).await;result.and(cleanup)
    })}
    fn write_stream_once<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream,size:u64,sha:&'a str)->PlatformFuture<'a,Result<WriteOnce>>{self.remote.write_stream_once(key,stream,size,sha)}
    fn delete<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{ObjectStore::delete(self,key)}
    fn copy<'a>(&'a self,source:&'a ObjectKey,target:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{let stream=self.read_stream(source).await?.ok_or_else(||NarouError::NotFound(source.to_string()))?;self.write_stream(target,stream).await})}
    fn move_or_copy<'a>(&'a self,source:&'a ObjectKey,target:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{self.copy(source,target).await?;ObjectStore::delete(self,source).await})}
}

/// native の保存先ペア。
///
/// 挿絵のバイナリだけ S3 互換ストレージへ流し、本文やメタデータは
/// 従来どおりローカル (YAML / SQLite ミラー) に残す。構成は
/// `s3.asset-backend` (`local` | `s3`) で決まり、`s3` のときに接続情報が
/// 欠けていれば [`crate::native::s3::illustration_store`] が失敗する
/// (fail-closed。黙ってローカルへ落とさない)。
pub struct NativeStores {
    pub primary: Arc<NativeStore>,
    pub objects: Arc<dyn ObjectStore>,
    pub assets: Arc<dyn AssetStore>,
    /// S3 を使うときだけ入る (presigned URL と移行で使う)。
    pub s3: Option<Arc<S3Store>>,
}

impl NativeStores {
    /// Trusted SHA bound to the installed file/DB generation or immutable S3
    /// reference ETag. No canonical body is rehashed to reconcile a receipt.
    pub fn transfer_asset_fingerprint<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<(u64,String)>>>{Box::pin(async move{
        if crate::platform::is_illustration_key(key)&&let Some(reference)=self.primary.remote_reference(key).await?{
            if reference.physical.is_empty(){return Ok(None);}
            if reference.logical!=key.as_ref()||reference.etag.is_empty()||reference.sha256.len()!=64{return Err(NarouError::Platform("remote alias fingerprint proof missing".into()));}
            let remote=self.s3.as_ref().ok_or_else(||NarouError::Platform("canonical remote alias requires configured S3 storage".into()))?;let physical=ObjectKey::try_new(reference.physical)?;
            let metadata=AssetStore::stat(remote.as_ref(),&physical).await?.ok_or_else(||NarouError::NotFound(physical.to_string()))?;
            if metadata.size!=reference.size||metadata.etag.as_deref()!=Some(reference.etag.as_str()){return Err(NarouError::Platform("canonical remote alias generation changed".into()));}return Ok(Some((reference.size,reference.sha256)));
        }
        if crate::platform::is_illustration_key(key)&&self.s3.is_some(){return Ok(None);}self.primary.transfer_fingerprint(key).await
    })}
    pub fn remote_objects(primary:Arc<NativeStore>,remote:Arc<S3Store>)->Arc<dyn ObjectStore>{Arc::new(NativeRemoteStore{primary,remote})}
    pub fn commit_transfer_asset<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,stage:&'a ObjectKey,key:&'a ObjectKey,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if crate::platform::is_illustration_key(key)&&let Some(remote)=&self.s3{
            let(id,reservation_key)=remote_preparation_identity(owner,epoch,stage,key,sha)?;let row=self.load_remote_reservation(&reservation_key).await?.ok_or_else(||NarouError::Platform("remote asset was not prepared".into()))?;
            let prep=row.preparation.as_ref().ok_or_else(||NarouError::Platform("remote preparation state missing".into()))?;let physical=ObjectKey::try_new(row.reference.physical.clone())?;
            if row.owner!=owner||row.epoch!=epoch||row.reference.logical!=key.as_ref()||row.reference.prefix!=prefix.as_ref().trim_end_matches('/')||!row.reference.sha256.eq_ignore_ascii_case(sha)||prep.source!=stage.as_ref()||prep.remote_stage!=id||prep.phase!=NativeRemotePhase::Ready||!prep.cursor.sealed||prep.cursor.confirmed_bytes!=row.reference.size||!prep.cursor.prefix_sha256().is_some_and(|value|value.eq_ignore_ascii_case(sha))||prep.target_url!=remote.location().object_url(&physical){return Err(NarouError::Platform("remote prepared asset identity/checkpoint conflict".into()));}
            run_scoped_blocking(||self.primary.commit_remote_reference(prefix,Some(owner),epoch,key,Some(&row.reference),None))
        }else{self.primary.commit_staged(prefix,owner,epoch,stage,key).await}
    })}
    pub fn remove_transfer_asset<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if crate::platform::is_illustration_key(key)&&self.s3.is_some(){
            run_scoped_blocking(||self.primary.commit_remote_reference(prefix,Some(owner),epoch,key,None,Some(sha)))
        }else{self.primary.remove_committed(prefix,owner,epoch,key,sha).await}
    })}

    /// Reclaim only an explicitly terminal owner, never an active publication.
    pub fn reclaim_remote_owner<'a>(&'a self,owner:&'a str,epoch:u64,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<Option<String>>>{Box::pin(async move{
        let mut request=ObjectListRequest::new(ObjectPrefix::new(".narou-transfer/.remote/")?,std::num::NonZeroUsize::new(limit.clamp(1,1)).expect("nonzero"));if let Some(cursor)=cursor{request=request.after(cursor);}
        let page=self.primary.list_page(&request).await?;let retry=||Some(cursor.unwrap_or("").to_owned());
        for row in page.objects{let Some(reservation)=self.load_remote_reservation(&row.key).await?else{continue;};if reservation.owner!=owner||reservation.epoch!=epoch{continue;}
            let key=ObjectKey::try_new(reservation.reference.logical.clone())?;let prefix=ObjectPrefix::new(reservation.reference.prefix.clone())?;let gate=self.primary.gate_info(&prefix).await?;if gate.state==GateState::Closed{continue;}
            let live=self.primary.remote_reference(&key).await?.is_some_and(|current|current.physical==reservation.reference.physical);
            if let Some(prep)=&reservation.preparation{
                let remote=self.s3.as_ref().ok_or_else(||NarouError::Platform("remote stage GC storage missing".into()))?;if !remote.release_target_stage(&prep.remote_stage,owner,epoch,live||reservation.pool).await?{return Ok(retry());}
                let receipts=self.primary.list_page(&ObjectListRequest::new(ObjectPrefix::new(format!(".narou-transfer/.remote_ops/{}/",prep.remote_stage))?,std::num::NonZeroUsize::new(16).expect("nonzero"))).await?;for receipt in receipts.objects{ObjectStore::delete(self.primary.as_ref(),&receipt.key).await?;}if receipts.next_cursor.is_some(){return Ok(retry());}
            }
            if live{continue;}
            if !reservation.pool&&let Some(remote)=&self.s3{ObjectStore::delete(remote.as_ref(),&ObjectKey::try_new(reservation.reference.physical)?).await?;}ObjectStore::delete(self.primary.as_ref(),&row.key).await?;
        }Ok(page.next_cursor)
    })}

    /// ライブラリルート (` .narou` を持つディレクトリ) から保存先を組む。
    pub fn for_narou_root(narou_root: &Path) -> Result<Self> {
        let primary: Arc<NativeStore> = Arc::new(NativeStore::for_narou_root(narou_root)?);
        let Some(s3) = crate::native::s3::illustration_store()? else {
            let objects: Arc<dyn ObjectStore> = primary.clone();
            let assets: Arc<dyn AssetStore> = primary.clone();
            return Ok(Self {
                primary,
                objects,
                assets,
                s3: None,
            });
        };
        let primary_objects: Arc<dyn ObjectStore> = primary.clone();
        let primary_assets: Arc<dyn AssetStore> = primary.clone();
        let remote=Arc::new(NativeRemoteStore{primary:primary.clone(),remote:s3.clone()});
        let s3_objects: Arc<dyn ObjectStore> = remote.clone();
        let s3_assets: Arc<dyn AssetStore> = remote;
        let split: Arc<SplitStore> = Arc::new(SplitStore::new(
            (primary_objects, primary_assets),
            (s3_objects, s3_assets),
        ));
        let objects: Arc<dyn ObjectStore> = split.clone();
        let assets: Arc<dyn AssetStore> = split;
        Ok(Self {
            primary,
            objects,
            assets,
            s3: Some(s3),
        })
    }

    /// プロセス全体のデータベースルートから保存先を組む
    /// (`NativeStore::for_current_root` と同じ解決)。
    pub fn for_current_root() -> Result<Self> {
        let root = crate::db::with_database(|db| Ok(db.archive_root().to_path_buf()))?;
        let narou_root = root
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        Self::for_narou_root(&narou_root)
    }
}

impl ObjectStore for NativeStore {
    fn delete_closed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{match self{Self::Fs(store)=>store.delete_closed(prefix,owner,epoch,key),Self::Sqlite(store)=>store.delete_closed(prefix,owner,epoch,key)}}
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a crate::platform::WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{
        match self{Self::Fs(store)=>store.update_small_guarded(fence,key,mutate),Self::Sqlite(store)=>store.update_small_guarded(fence,key,mutate)}
    }
    fn access_fence(&self) -> Option<&dyn NovelAccessFence> { Some(self) }
    fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        match self { Self::Fs(store) => store.read_stream_leased(prefix, lease, key, range), Self::Sqlite(store) => store.read_stream_leased(prefix, lease, key, range) }
    }
    /// Fs objects are real files; a snapshot can hardlink instead of copying
    /// (issue #58). SQLite bodies live in the DB, so they keep the copy path.
    fn native_source_path(&self, key: &ObjectKey) -> Option<std::path::PathBuf> {
        match self { Self::Fs(store) => store.resolve_path(key).ok(), Self::Sqlite(_) => None }
    }

    fn is_remote_reference<'a>(&'a self, key:&'a ObjectKey)->PlatformFuture<'a,Result<bool>>{self.has_remote_reference(key)}
    fn write_small_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        match self { Self::Fs(store) => store.write_small_guarded(fence,key,data), Self::Sqlite(store) => store.write_small_guarded(fence,key,data) }
    }
    fn delete_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        match self { Self::Fs(store) => store.delete_guarded(fence,key), Self::Sqlite(store) => store.delete_guarded(fence,key) }
    }

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        match self {
            NativeStore::Fs(store) => ObjectStore::stat(store, key),
            NativeStore::Sqlite(store) => ObjectStore::stat(store, key),
        }
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        match self {
            NativeStore::Fs(store) => store.read_small(key),
            NativeStore::Sqlite(store) => store.read_small(key),
        }
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => store.write_small(key, data),
            NativeStore::Sqlite(store) => store.write_small(key, data),
        }
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        match self {
            NativeStore::Fs(store) => store.update_small(key, mutate),
            NativeStore::Sqlite(store) => store.update_small(key, mutate),
        }
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => ObjectStore::delete(store, key),
            NativeStore::Sqlite(store) => ObjectStore::delete(store, key),
        }
    }

    fn delete_prefix<'a>(
        &'a self,
        prefix: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => store.delete_prefix(prefix),
            NativeStore::Sqlite(store) => store.delete_prefix(prefix),
        }
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        match self {
            NativeStore::Fs(store) => store.list_page(request),
            NativeStore::Sqlite(store) => store.list_page(request),
        }
    }
}

impl AssetStore for NativeStore {
    fn read_stream_range<'a>(&'a self, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        match self { Self::Fs(store) => store.read_stream_range(key,range), Self::Sqlite(store) => store.read_stream_range(key,range) }
    }
    fn write_stream_guarded<'a>(&'a self, fence: &'a crate::platform::WriteFence, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        match self { Self::Fs(store) => store.write_stream_guarded(fence,key,stream), Self::Sqlite(store) => store.write_stream_guarded(fence,key,stream) }
    }
    fn write_stream_once<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream, size: u64, sha: &'a str) -> PlatformFuture<'a, Result<WriteOnce>> {
        match self { Self::Fs(store) => store.write_stream_once(key,stream,size,sha), Self::Sqlite(store) => store.write_stream_once(key,stream,size,sha) }
    }

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        match self {
            NativeStore::Fs(store) => AssetStore::stat(store, key),
            NativeStore::Sqlite(store) => AssetStore::stat(store, key),
        }
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        match self {
            NativeStore::Fs(store) => store.read_stream(key),
            NativeStore::Sqlite(store) => store.read_stream(key),
        }
    }

    fn read_range<'a>(
        &'a self, key: &'a ObjectKey, range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        match self {
            NativeStore::Fs(store) => store.read_range(key, range),
            NativeStore::Sqlite(store) => store.read_range(key, range),
        }
    }

    fn stage_stream<'a>(&'a self, stream: AssetStream) -> PlatformFuture<'a, Result<crate::platform::staging::StagedAsset>> {
        match self {
            NativeStore::Fs(store) => store.stage_stream(stream),
            NativeStore::Sqlite(store) => store.stage_stream(stream),
        }
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => store.write_stream(key, stream),
            NativeStore::Sqlite(store) => store.write_stream(key, stream),
        }
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => AssetStore::delete(store, key),
            NativeStore::Sqlite(store) => AssetStore::delete(store, key),
        }
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => store.copy(source, destination),
            NativeStore::Sqlite(store) => store.copy(source, destination),
        }
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        match self {
            NativeStore::Fs(store) => store.move_or_copy(source, destination),
            NativeStore::Sqlite(store) => store.move_or_copy(source, destination),
        }
    }
}

/// Delete every object stored below `prefix` using the active storage backend.
///
/// `remove --with-file` has to drop the stored objects together with their
/// filesystem mirror: in SQLite mode the `objects` table survives the deleted
/// folder, and a later download then treats the sections as already present
/// and never rewrites the files the converter reads.
pub fn delete_prefix_sync(store: &dyn ObjectStore, prefix: &ObjectKey) -> Result<()> {
    futures::executor::block_on(store.delete_prefix(prefix))?;
    Ok(())
}

macro_rules! native_port {
    ($trait:ident { $(fn $method:ident<$lt:lifetime>($($arg:ident : $ty:ty),*) -> $result:ty;)* } $($extra:item)*) => {
        impl $trait for NativeStore {
            $(fn $method<$lt>(&$lt self, $($arg: $ty),*) -> PlatformFuture<$lt, Result<$result>> {
                match self { Self::Fs(store) => $trait::$method(store,$($arg),*), Self::Sqlite(store) => $trait::$method(store,$($arg),*) }
            })*
            $($extra)*
        }
    };
}
native_port!(TransferStageStore {
    fn stage_open<'a>(id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->StageCursor;
    fn stage_cursor<'a>(id:&'a str)->Option<StageCursor>;
    fn stage_append<'a>(id:&'a str,owner:&'a str,epoch:u64,offset:u64,payload:&'a [u8],last:bool)->StageCursor;
    fn stage_rewind<'a>(id:&'a str,owner:&'a str,epoch:u64,checkpoint:&'a StageCursor)->StageCursor;
    fn stage_read<'a>(id:&'a str,range:std::ops::Range<u64>)->Option<AssetStream>;
    fn stage_touch<'a>(id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->();
    fn stage_seal<'a>(id:&'a str,owner:&'a str,epoch:u64,size:u64,sha:&'a str)->SealedAsset;
    fn stage_abort<'a>(id:&'a str,owner:&'a str,epoch:u64)->();
    fn stage_lease_page<'a>(cursor:Option<&'a str>,limit:usize)->(Vec<StageLease>,Option<String>);
    fn stage_reclaim<'a>(id:&'a str,owner:&'a str,epoch:u64)->bool;
});
native_port!(NovelAccessFence {
    fn pending_removals<'a>(cursor:Option<&'a str>,limit:usize)->(Vec<(ObjectPrefix,String,u64)>,Option<String>);
    fn capture_writer<'a>(prefix:&'a ObjectPrefix)->crate::platform::WriteFence;
    fn rebind_writer<'a>(prefix:&'a ObjectPrefix,expected_epoch:u64)->crate::platform::WriteFence;
    fn admit_reader<'a>(prefix:&'a ObjectPrefix,id:&'a str,ttl:i64)->GateLease;
    fn renew_reader<'a>(prefix:&'a ObjectPrefix,lease:&'a GateLease,ttl:i64)->GateLease;
    fn release_reader<'a>(prefix:&'a ObjectPrefix,lease:&'a GateLease)->();
    fn gate_info<'a>(prefix:&'a ObjectPrefix)->GateInfo;
    fn close_gate<'a>(prefix:&'a ObjectPrefix,owner:&'a str)->u64;
    fn renew_gate<'a>(prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64)->();
    fn open_gate<'a>(prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64)->();
    fn commit_staged<'a>(prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,stage:&'a ObjectKey,canonical:&'a ObjectKey)->();
    fn remove_committed<'a>(prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey,sha:&'a str)->();
}
    fn retire_writer<'a>(&'a self,fence:&'a crate::platform::WriteFence,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{run_scoped_blocking(||self.retire_checked(fence,owner))})}
);

// Transfer storage uses stable lock files: never unlink or steal them by TTL.
use crate::platform::{TransferStageStore, NovelAccessFence, ObjectPrefix, StageCursor,
    SealedAsset, StageLease, GateLease, GateInfo, GateState, WriteOnce};
use crate::platform::{transfer_stage as ts, access_fence as af};
use sha2::Digest;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct FsGate {
    #[serde(default)]
    pub(crate) prefix:Option<String>,
    #[serde(default)]
    pub(crate) origin_url_fold:String,
    #[serde(default)]
    pub(crate) content_revision:u64,
    pub(crate) epoch: u64, pub(crate) owner: Option<String>, pub(crate) updated_at: i64,
    /// Set when SQL gate authority adopted this namespace. All FS-side
    /// admission, mutation, rebind and origin writes must fail closed; only
    /// the migration helper flips it, under the same OS lock as the SQL tx.
    #[serde(default)]
    pub(crate) migrated: bool,
    pub(crate) readers: std::collections::BTreeMap<String, (u64, i64)>,
}
impl FsGate {
    fn prune(&mut self) { let now = ts::now_secs(); self.readers.retain(|_, (_, expiry)| *expiry > now); }
    fn require_writer(&mut self, prefix: &ObjectPrefix, owner: &str, epoch: u64) -> Result<()> {
        self.prune();
        if self.migrated{return Err(af::gate_closed_error(prefix));}
        if self.owner.as_deref() != Some(owner) || self.epoch != epoch { return Err(af::gate_epoch_error(prefix, epoch, self.epoch)); }
        if !self.readers.is_empty() { return Err(af::novel_busy_error(prefix)); }
        Ok(())
    }
}


/// How the live SQLite authority relates to one durable FS gate tuple.
/// Consulted on every FS-side gate admission so a committed FS→SQL
/// handoff is honored even when the file's `migrated` marker was never
/// written (process died between the SQL commit and the marker flush).
pub(crate) enum SqlGateAuthority{
    /// A committed FS→SQL handoff covers this exact tuple: the file's
    /// authority was superseded; its `migrated` marker is the only part
    /// still missing.
    Superseded,
    /// A committed SQL→FS handoff names this exact tuple as the canonical
    /// FS gate; the retired SQL row may already be deleted.
    FsCanonical,
    /// The live SQL row carries authority this tuple cannot explain.
    Conflict,
    /// No consultable SQLite authority — filesystem authority stands alone.
    Absent,
}

/// A `db.sqlite` the caller may consult for gate authority but must never
/// take through `configure`: migrations, legacy import and the startup gate
/// sweep belong to the configured handle only. A missing file stays `None`
/// (the `.narou` directory may predate any SQLite use) and the consult never
/// creates one — opening a not-yet-created database would fabricate an
/// empty `db.sqlite` as a side effect, so `SQLITE_OPEN_CREATE` is never set.
/// A `CannotOpen` open error only yields `None` while the file stays gone
/// (deleted between the existence check and open); on an existing file a
/// `ReadOnly`/`CannotOpen` read-write failure retries read-only, and every
/// other open or schema error propagates so the caller fails closed. The
/// connection is never registered in `OPEN_HANDLES`, preserving the
/// invariant that a configured handle observed a reconciled library.
fn native_db_gate_connection(narou_dir:&Path)->Result<Option<rusqlite::Connection>>{
    let database=narou_dir.join(crate::native::sqlite::state::DB_FILE_NAME);
    match database.try_exists(){Ok(true)=>{},Ok(false)=>return Ok(None),Err(error)=>return Err(error.into())}
    let open=|flags|match rusqlite::Connection::open_with_flags(&database,flags){
        Ok(connection)=>Ok(Some(connection)),
        // Deleted between the existence check and open: still absent.
        Err(rusqlite::Error::SqliteFailure(code,_))if code.code==rusqlite::ErrorCode::CannotOpen&&!database.exists()=>Ok(None),
        Err(error)=>Err(error),
    };
    let mut connection=match open(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE|rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX){
        Ok(connection)=>connection,
        // A surviving file that rejects read-write open (read-only media or
        // rights) is consulted read-only; a failing RO open still fails closed.
        Err(rusqlite::Error::SqliteFailure(code,_))if matches!(code.code,rusqlite::ErrorCode::ReadOnly|rusqlite::ErrorCode::CannotOpen)=>open(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY|rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(crate::native::sqlite::sqlite_error)?,
        Err(error)=>return Err(crate::native::sqlite::sqlite_error(error)),
    };
    let Some(connection)=connection.take()else{return Ok(None);};
    // Same wait budget as `StateDb::configure`: a short checkpoint or VACUUM
    // must not turn this consult into SQLITE_BUSY.
    connection.busy_timeout(std::time::Duration::from_secs(5)).map_err(crate::native::sqlite::sqlite_error)?;
    Ok(Some(connection))
}

/// Classify the durable FS tuple `(epoch, origin)` against the gate
/// authority one consultable connection sees for `prefix`. Every committed
/// adoption writes its row/lease changes and its handoff record in one
/// transaction, so a fresh consult sees either the complete authority state
/// or nothing — ordering of the checks is the provenance order:
/// `Superseded` (a committed FS→SQL handoff covers this exact tuple),
/// `FsCanonical` (a committed SQL→FS handoff names this exact tuple and the
/// retired row is gone), `Conflict` (a live row, a mismatched handoff, or
/// the contradictory row+handoff state) and finally `Absent`.
fn consult_sql_gate_authority(connection:&rusqlite::Connection,prefix:&ObjectPrefix,epoch:u64,origin:&str)->Result<SqlGateAuthority>{
    use rusqlite::OptionalExtension;
    let prefix=prefix.as_ref().trim_end_matches('/');
    if identity::read_sql_gate_handoff(connection,prefix,epoch,origin)?{return Ok(SqlGateAuthority::Superseded);}
    let handoff=identity::read_fs_gate_handoff_raw(connection,prefix)?;
    let row:Option<(i64,String)>=connection.query_row("SELECT epoch,origin_url_fold FROM novel_access_gates WHERE prefix=?1",[prefix],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;
    if let Some((row_epoch,_))=&row&&*row_epoch<0{return Err(NarouError::Platform("negative epoch in durable SQL gate".into()));}
    match (handoff,row){
        (Some((_,fs_epoch,fs_origin)),None)if fs_epoch==epoch&&fs_origin==origin=>Ok(SqlGateAuthority::FsCanonical),
        (Some(_),_)=>Ok(SqlGateAuthority::Conflict),
        (None,Some(_))=>Ok(SqlGateAuthority::Conflict),
        (None,None)=>Ok(SqlGateAuthority::Absent),
    }
}

/// Compare the durable FS tuple `(epoch, origin)` against the live SQLite
/// authority for `prefix`. The established handle is consulted first; when
/// none exists this opens `db.sqlite` directly so a committed cross-process
/// handoff still fences this file — an established-handle-only lookup could
/// never prove absence. The consult only reads committed durable state and
/// never runs `configure`: migrations, legacy import and the startup gate
/// sweep belong to the configured handle alone. Open/schema errors and real
/// `Conflict`/`Superseded` provenance still fail closed; `try_lock`
/// contention on the shared handle is not provenance, so it falls back to a
/// dedicated consult connection that sees the same committed state (a
/// parallel update holding the mutex mid-transaction can no longer
/// fabricate a `Conflict`, issue #50).
pub(crate) fn live_sql_gate_authority(narou_dir:&Path,prefix:&ObjectPrefix,epoch:u64,origin:&str)->Result<SqlGateAuthority>{
    if crate::native::sqlite::state::legacy_yaml_active() || crate::native::sqlite::state::read_mode(narou_dir)!=crate::native::sqlite::state::StorageMode::Sqlite{return Ok(SqlGateAuthority::Absent);}
    if let Some(state)=crate::native::sqlite::state::established_for(narou_dir)
        && let Some(connection)=state.conn_ref().try_lock()
    {
        return consult_sql_gate_authority(&connection,prefix,epoch,origin);
    }
    let Some(connection)=native_db_gate_connection(narou_dir)?else{return Ok(SqlGateAuthority::Absent);};
    consult_sql_gate_authority(&connection,prefix,epoch,origin)
}

impl NativeObjectStore {
    fn live_sql_authority(&self,prefix:&ObjectPrefix,epoch:u64,origin:&str)->Result<SqlGateAuthority>{
        if !self.consult_sql_authority{return Ok(SqlGateAuthority::Absent);}
        let narou_dir=self.root().parent().unwrap_or_else(||self.root()).join(".narou");
        live_sql_gate_authority(&narou_dir,prefix,epoch,origin)
    }

    /// Raw gate mutation used by `release_reader`, which must stay idempotent
    /// even after SQL authority adopted the namespace.
    fn with_gate_raw<T>(&self, prefix: &ObjectPrefix, operation: impl FnOnce(&mut FsGate) -> Result<T>) -> Result<T> {
        self.with_reader_gate_lock(prefix.as_ref().trim_end_matches('/'), || {
            let path = self.gate_path(prefix)?;
            self.ensure_parent_dir(&path)?;
            let mut gate: FsGate = match fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(format!("invalid durable gate: {error}")))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => FsGate::default(),
                Err(error) => return Err(error.into()),
            };
            gate.prefix=Some(prefix.as_ref().trim_end_matches('/').to_owned());
            gate.prune();
            // Drain paths run under any authority, but a proven handoff is
            // the one durable cue that re-marks the file migrated when the
            // post-commit marker write never landed.
            if !gate.migrated&&matches!(self.live_sql_authority(prefix,gate.epoch,&gate.origin_url_fold.clone())?,SqlGateAuthority::Superseded){gate.migrated=true;}
            let value = operation(&mut gate)?;
            write_gate_file(&self.root, &path, &serde_json::to_vec(&gate).map_err(|error| NarouError::Platform(error.to_string()))?)?;
            Ok(value)
        })
    }


    /// Adopt this FS gate into SQL gate authority for `prefix`. Under the
    /// prefix's OS gate lock this opens one immediate transaction on
    /// `connection` (caller holds the metadata boundary but must NOT hold an
    /// open transaction — nesting fails), rejects the adoption if a durable
    /// SQL→FS handoff still names this gate canonical, installs the canonical
    /// SQL gate row at fs_epoch+1 preserving origin/revision, imports every
    /// live reader lease, records the durable handoff and retires SQL→FS
    /// provenance. Only after the COMMIT lands does the FS gate file gain
    /// its `migrated` marker — a failed commit leaves the FS file the sole
    /// authority with zero partial state, and a crash between commit and
    /// marker write is reconciled by the startup sweep via the recorded
    /// handoff. Idempotent: a second adoption passes only when the handoff
    /// covers this exact FS tuple; a competing un-adopted SQL row fails
    /// closed.
    pub(crate) fn adopt_fs_gate_locked(&self,prefix:&ObjectPrefix,connection:&rusqlite::Connection)->Result<()>{
        self.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{
            let path=self.gate_path(prefix)?;let mut gate:FsGate=match fs::read(&path){
                Ok(bytes)=>serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?,
                Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Err(af::gate_closed_error(prefix)),
                Err(error)=>return Err(error.into()),
            };
            gate.prune();
            if gate.migrated{
                // Already adopted: pass only when the recorded handoff still
                // proves this exact FS tuple.
                if identity::read_sql_gate_handoff(connection,prefix.as_ref().trim_end_matches('/'),gate.epoch,&gate.origin_url_fold)?{return Ok(());}
                return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));
            }
            if gate.owner.is_some(){return Err(NarouError::Platform("closed FS gate cannot migrate to SQL authority".into()));}
            let fs_epoch=gate.epoch;let fs_origin=gate.origin_url_fold.clone();let sql_epoch=fs_epoch.checked_add(1).ok_or_else(||NarouError::Platform("FS gate epoch exhausted".into()))?;
            let sql_epoch_i64=i64::try_from(sql_epoch).map_err(|_|NarouError::Platform("SQL gate epoch exceeds SQLite integer range".into()))?;
            let tx=rusqlite::Transaction::new_unchecked(connection,rusqlite::TransactionBehavior::Immediate).map_err(crate::native::sqlite::sqlite_error)?;
            use rusqlite::OptionalExtension;
            // A committed SQL→FS handoff names the canonical FS gate for this
            // prefix: when it is THIS exact gate the re-adoption legally
            // retires that handoff below; when it names a different tuple the
            // file carries no proven authority and fails closed.
            match identity::read_fs_gate_handoff_raw(&tx,prefix.as_ref().trim_end_matches('/'))?{
                Some((_,hand_fs,hand_origin)) if hand_fs==fs_epoch&&hand_origin==fs_origin=>{}
                Some(_)=>{return Err(NarouError::Platform("FS gate conflicts with committed SQL-to-FS handoff".into()));}
                None=>{}
            }
            // Fail closed on a competing un-adopted SQL row instead of
            // silently skipping the upsert below: the only acceptable
            // existing row is the one a proven earlier adoption installed.
            let existing:Option<i64>=tx.query_row("SELECT epoch FROM novel_access_gates WHERE prefix=?1",[prefix.as_ref().trim_end_matches('/')],|row|row.get(0)).optional().map_err(crate::native::sqlite::sqlite_error)?;
            if let Some(existing_epoch)=existing{
                let proven=identity::read_sql_gate_handoff(&tx,prefix.as_ref().trim_end_matches('/'),fs_epoch,&fs_origin)?&&existing_epoch>=0&&existing_epoch as u64==sql_epoch;
                if !proven{return Err(NarouError::Platform("FS gate adoption conflicts with existing SQL authority".into()));}
            }
            tx.execute("INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES(?1,?2,0,NULL,unixepoch(),?3,?4) ON CONFLICT(prefix) DO UPDATE SET epoch=excluded.epoch,content_revision=excluded.content_revision,origin_url_fold=excluded.origin_url_fold WHERE novel_access_gates.epoch<excluded.epoch AND novel_access_gates.closed=0 AND novel_access_gates.origin_url_fold=excluded.origin_url_fold",
                rusqlite::params![prefix.as_ref().trim_end_matches('/'),sql_epoch_i64,i64::try_from(gate.content_revision).map_err(|_|NarouError::Platform("content revision exceeds SQLite integer range".into()))?,fs_origin]).map_err(crate::native::sqlite::sqlite_error)?;
            for (reader_id,(epoch,expiry)) in gate.readers.iter(){
                tx.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES(?1,?2,?3,?4) ON CONFLICT(prefix,reader_id) DO UPDATE SET epoch=excluded.epoch,expires_at=excluded.expires_at",rusqlite::params![prefix.as_ref().trim_end_matches('/'),reader_id,i64::try_from(*epoch).map_err(|_|NarouError::Platform("reader lease epoch exceeds SQLite integer range".into()))?,*expiry]).map_err(crate::native::sqlite::sqlite_error)?;
            }
            identity::record_sql_gate_handoff(&tx,prefix.as_ref().trim_end_matches('/'),fs_epoch,sql_epoch,&fs_origin)?;
            // Re-adoption retires any SQL→FS provenance: the new SQL row
            // epoch exceeds the recorded fs_epoch, so stale FS tokens still
            // lose even if a reverse handoff lingers elsewhere.
            identity::clear_fs_gate_handoff(&tx,prefix.as_ref().trim_end_matches('/'))?;
            tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
            // COMMIT landed: SQL is the canonical authority from here. The
            // migrated marker retires the file; a crash before this write is
            // repaired by the startup sweep via the recorded handoff.
            gate.migrated=true;gate.updated_at=ts::now_secs();
            write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)?;
            Ok(())
        })
    }

    /// Pre-flight the canonical FS gate an SQL→FS adoption is about to
    /// install, inside the caller's transaction so a conflict aborts the
    /// commit. Rules: absent file → install; identical epoch+origin →
    /// idempotent rewrite; migrated gate proven by its FS→SQL handoff (the
    /// exact lineage this round-trip reverses) → overwrite; anything else
    /// fails closed rather than clobbering independent FS authority.
    pub(crate) fn require_adopted_gate_target(&self,path:&Path,prefix:&ObjectPrefix,gate:&FsGate,connection:&rusqlite::Connection)->Result<()>{
        match fs::read(path){
            Ok(bytes)=>{
                let existing:FsGate=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?;
                if existing.epoch==gate.epoch&&existing.origin_url_fold==gate.origin_url_fold&&!existing.migrated{return Ok(());}
                if existing.migrated&&identity::read_sql_gate_handoff(connection,prefix.as_ref().trim_end_matches('/'),existing.epoch,&existing.origin_url_fold)?{return Ok(());}
                Err(NarouError::Platform("FS gate adoption conflicts with existing filesystem authority".into()))
            }
            Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(()),
            Err(error)=>Err(error.into()),
        }
    }

    /// Install/replace this gate as the SQL→FS handoff target, under the
    /// caller-held OS transfer lock, after `require_adopted_gate_target`
    /// passed inside the committed transaction. The identical conflict rules
    /// are re-validated against the committed handoff state.
    pub(crate) fn install_adopted_gate_locked(&self,path:&Path,prefix:&ObjectPrefix,gate:&FsGate,connection:&rusqlite::Connection)->Result<()>{
        self.install_adopted_gate_nosync(path,prefix,gate,connection)?;sync_transfer_parent(path)
    }

    pub(crate) fn install_adopted_gate_nosync(&self,path:&Path,prefix:&ObjectPrefix,gate:&FsGate,connection:&rusqlite::Connection)->Result<()>{
        self.require_adopted_gate_target(path,prefix,gate,connection)?;
        self.ensure_parent_dir(path)?;
        write_gate_file(&self.root,path,&serde_json::to_vec(gate).map_err(|error|NarouError::Platform(error.to_string()))?)
    }

    /// Batched variant of [`Self::install_adopted_gate_nosync`]: the same
    /// validation, but the file is written through `batch` (no per-file
    /// counter work or parent sync). The caller must sync the `.gate/`
    /// directory and run [`GateRevisionBatch::finish`] once the batch is
    /// complete (issue #58: 1200-file cutovers cannot afford 1200 exclusive
    /// lock-and-fsync cycles).
    pub(crate) fn install_adopted_gate_batch(&self,path:&Path,prefix:&ObjectPrefix,gate:&FsGate,connection:&rusqlite::Connection,batch:&GateRevisionBatch)->Result<()>{
        self.require_adopted_gate_target(path,prefix,gate,connection)?;
        self.ensure_parent_dir(path)?;
        batch.write_gate(path,&serde_json::to_vec(gate).map_err(|error|NarouError::Platform(error.to_string()))?)
    }


    /// Durable intent journal for the SQL→FS adoption of `prefix`: a
    /// `.gate.pending/<sha256>` file written inside the intent transaction
    /// and retired only after the canonical gate lands.
    pub(crate) fn gate_pending_path(&self, prefix: &ObjectPrefix) -> Result<PathBuf> {
        self.resolve_path(&ObjectKey::try_new(format!(".narou-transfer/.gate.pending/{}",
            hex::encode(sha2::Sha256::digest(prefix.as_ref().trim_end_matches('/').as_bytes()))))?)
    }

    pub(crate) fn install_pending_gate_locked(&self,prefix:&ObjectPrefix,gate:&FsGate)->Result<()>{
        let path=self.install_pending_gate_nosync(prefix,gate)?;sync_transfer_parent(&path)
    }

    pub(crate) fn install_pending_gate_nosync(&self,prefix:&ObjectPrefix,gate:&FsGate)->Result<PathBuf>{
        let path=self.gate_pending_path(prefix)?;self.ensure_parent_dir(&path)?;
        atomic_write_bytes(&path,&serde_json::to_vec(gate).map_err(|error|NarouError::Platform(error.to_string()))?)?;Ok(path)
    }

    pub(crate) fn read_pending_gate_locked(&self,prefix:&ObjectPrefix)->Result<Option<FsGate>>{
        match fs::read(self.gate_pending_path(prefix)?){
            Ok(bytes)=>Ok(Some(serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid pending gate journal: {error}")))?)),
            Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),
            Err(error)=>Err(error.into()),
        }
    }

    pub(crate) fn remove_pending_gate_locked(&self,prefix:&ObjectPrefix)->Result<()>{
        let path=self.gate_pending_path(prefix)?;
        if self.remove_pending_gate_nosync(prefix)? { sync_transfer_parent(&path)?; }
        Ok(())
    }

    pub(crate) fn remove_pending_gate_nosync(&self,prefix:&ObjectPrefix)->Result<bool>{
        let path=self.gate_pending_path(prefix)?;
        match fs::remove_file(&path){Ok(())=>Ok(true),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(false),Err(error)=>Err(error.into())}
    }

    pub(crate) fn sync_gate_dir(&self)->Result<()>{
        let dir=self.root().join(".objects/.narou-transfer/.gate");
        if dir.is_dir(){crate::compat::fsync_dir(&dir)?;}
        Ok(())
    }

    pub(crate) fn sync_pending_gate_dir(&self)->Result<()>{
        let dir=self.root().join(".objects/.narou-transfer/.gate.pending");
        if dir.is_dir(){crate::compat::fsync_dir(&dir)?;}
        Ok(())
    }

    /// Clear the `migrated` marker of a gate whose FS→SQL commit never
    /// landed, restoring the file as sole authority. Re-verifies the
    /// unproven state inside the lock so a proven gate is never un-marked.
    pub(crate) fn clear_unproven_migrated_gate_locked(&self,prefix:&ObjectPrefix,connection:&rusqlite::Connection)->Result<()>{
        self.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{
            let path=self.gate_path(prefix)?;let mut gate:FsGate=match fs::read(&path){
                Ok(bytes)=>serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?,
                Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(()),
                Err(error)=>return Err(error.into()),
            };
            if !gate.migrated{return Ok(());}
            if identity::read_sql_gate_handoff(connection,prefix.as_ref().trim_end_matches('/'),gate.epoch,&gate.origin_url_fold)?{return Ok(());}
            gate.migrated=false;gate.updated_at=ts::now_secs();
            write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)
        })
    }

    /// Mark a gate migrated whose FS→SQL handoff is already proven — the
    /// crash window between that commit and its marker write. Caller holds
    /// the sweep boundary; re-validates nothing by itself beyond existence.
    pub(crate) fn mark_proven_migrated_gate_locked(&self,prefix:&ObjectPrefix)->Result<()>{
        self.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{
            let path=self.gate_path(prefix)?;let mut gate:FsGate=match fs::read(&path){
                Ok(bytes)=>serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?,
                Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(()),
                Err(error)=>return Err(error.into()),
            };
            if gate.migrated{return Ok(());}
            gate.migrated=true;gate.updated_at=ts::now_secs();
            write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)
        })
    }

    /// Drop this reader's FS gate entry under the caller-held transfer lock
    /// (no re-lock). Used by the SQL store's release path so an imported
    /// lease drains on both sides atomically.
    pub(crate) fn release_reader_locked(&self,prefix:&ObjectPrefix,lease:&GateLease)->Result<()>{
        let path=self.gate_path(prefix)?;let mut gate:FsGate=match fs::read(&path){
            Ok(bytes)=>serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?,
            Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(()),
            Err(error)=>return Err(error.into()),
        };
        gate.prune();
        if gate.readers.get(&lease.reader_id).is_some_and(|(epoch,_)|*epoch==lease.epoch){gate.readers.remove(&lease.reader_id);}else{return Ok(());}
        gate.updated_at=ts::now_secs();
        write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)
    }

    /// Drain this lease's SQL row through the shared process handle, when a
    /// live SQLite backend still knows the token. Best-effort by location:
    /// no handle means the SQL side cannot hold the lease.
    fn release_sql_reader_lease(&self,prefix:&ObjectPrefix,lease:&GateLease)->Result<()>{
        let Some(narou_root)=self.root().parent().map(|parent|parent.to_path_buf())else{return Ok(());};
        let Some(state)=crate::native::sqlite::state::established_for(&narou_root.join(".narou"))else{return Ok(());};
        let epoch=i64::try_from(lease.epoch).map_err(|_|af::gate_epoch_error(prefix,lease.epoch,u64::MAX))?;
        state.conn_ref().lock().execute("DELETE FROM novel_reader_leases WHERE prefix=? AND reader_id=? AND epoch=?",rusqlite::params![prefix.as_ref().trim_end_matches('/'),lease.reader_id,epoch]).map_err(crate::native::sqlite::sqlite_error)?;Ok(())
    }
}
/// Backup-only transform. Closed publications must recover before copying.
pub fn sanitize_gate_snapshot(bytes:&[u8])->Result<Vec<u8>>{
    let mut gate:FsGate=serde_json::from_slice(bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?;
    if gate.owner.is_some(){return Err(NarouError::Platform("novel_recovering: closed gate cannot enter backup".into()));}gate.readers.clear();serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))
}

/// Read one durable FS gate file; `Ok(None)` when absent, parse errors fail
/// closed (never silently treated as no authority).
pub(crate) fn read_durable_gate(path:&Path)->Result<Option<FsGate>>{
    match fs::read(path){
        Ok(bytes)=>Ok(Some(serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?)),
        Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),
        Err(error)=>Err(error.into()),
    }
}

/// Canonical tuple of one durable FS gate file: prefix + parsed FsGate.
/// Collected before SQL rows are emitted so a SQL→FS handoff can suppress
/// the SQL tuple it superseded (sorted by prefix for deterministic merge).
pub(crate) fn durable_fs_gates(directory:&Path)->Result<Vec<(String,FsGate)>>{
    let entries=match fs::read_dir(directory){Ok(entries)=>entries,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(Vec::new()),Err(error)=>return Err(error.into())};
    let mut out=Vec::new();
    // Atomic replacements are unpublished state, even when their JSON is complete.
    for entry in entries{let entry=entry?;let path=entry.path();if path.file_name().is_some_and(|name|name.as_encoded_bytes().starts_with(ATOMIC_OBJECT_TEMP_PREFIX.as_bytes()))||!entry.file_type()?.is_file(){continue;}let gate:FsGate=serde_json::from_slice(&fs::read(&path)?).map_err(|error|NarouError::Platform(format!("invalid namespace gate: {error}")))?;let prefix=gate.prefix.clone().ok_or_else(||NarouError::Platform("protected namespace gate has no durable prefix".into()))?;out.push((prefix,gate));}
    out.sort_by(|left,right|left.0.cmp(&right.0));
    Ok(out)
}

/// Process-wide cache of the durable FS gate scan. The seqlock revision proves
/// whether the cached snapshot is still current, so a scan costs one small
/// file read instead of one open+read+parse per gate file (issue #58).
/// An odd revision means a mutation is in flight or crashed: the scan runs
/// uncached so a stable even value is required before caching again.
type FsGateScanCache = parking_lot::Mutex<std::collections::HashMap<PathBuf,(u64,Arc<Vec<(String,FsGate)>>)>>;
static FS_GATE_SCAN_CACHE: std::sync::OnceLock<FsGateScanCache> = std::sync::OnceLock::new();

fn fs_gate_scan_cache()->&'static FsGateScanCache{
    FS_GATE_SCAN_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Durable FS gates for one archive root, reusing the cached snapshot while the
/// scan revision proves it current.
fn cached_fs_gates(archive_root:&Path,directory:&Path)->Result<std::collections::BTreeMap<String,FsGate>>{
    let revision=read_gate_revision(archive_root)?;
    if revision%2==0{
        let cache=fs_gate_scan_cache().lock();
        if let Some((cached_revision,gates))=cache.get(archive_root)&&*cached_revision==revision{
            return Ok(gates.iter().cloned().collect());
        }

    }
    let gates=Arc::new(durable_fs_gates(directory)?);
    // Re-read after the scan: a stable even value proves every counted
    // mutation is visible in this snapshot, so it is safe to cache.
    let after=read_gate_revision(archive_root)?;
    if after%2==0&&after==revision{
        fs_gate_scan_cache().lock().insert(archive_root.to_path_buf(),(after,gates.clone()));
    }
    Ok(gates.iter().cloned().collect())
}

/// Metadata-only visitor. The caller already owns metadata/SQL authority; this
/// never re-locks a connection or OS gate and never reads novel payloads.
///
/// Access budget (issue #58): the durable FS gate scan is cached behind the
/// gate-revision seqlock (one small file read per scan instead of one
/// open+read+parse per gate file), and the handoff tables are read with one
/// query each instead of two queries per gate row. The predicates below
/// reproduce the per-row readers exactly, including their fail-closed
/// regression errors.
pub(crate) fn visit_namespace_gates(root:&Path,connection:&rusqlite::Connection,mut visit:impl FnMut(&str,bool,Option<&str>,u64,&str)->Result<()>)->Result<()>{
    let archive_root=root.join(crate::downloader::types::ARCHIVE_ROOT_DIR);
    let directory=archive_root.join(".objects/.narou-transfer/.gate");
    let fs_gates:std::collections::BTreeMap<String,FsGate>=if root.as_os_str().is_empty(){std::collections::BTreeMap::new()}else{cached_fs_gates(&archive_root,&directory)?};
    // One existence query for all three tables, then one row query per
    // handoff table that exists.
    let mut existing=std::collections::BTreeSet::new();
    {let mut statement=connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('novel_access_gates','fs_gate_handoffs','native_gate_handoffs')").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let name:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;existing.insert(name);}}
    let fs_handoffs=if existing.contains("fs_gate_handoffs"){load_fs_gate_handoffs(connection)?}else{std::collections::BTreeMap::new()};
    let sql_handoffs=if existing.contains("native_gate_handoffs"){load_sql_gate_handoffs(connection)?}else{std::collections::BTreeMap::new()};
    if existing.contains("novel_access_gates"){let mut statement=connection.prepare("SELECT prefix,closed,owner,epoch,origin_url_fold FROM novel_access_gates WHERE closed<>0 OR origin_url_fold<>'' ORDER BY prefix").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let prefix:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;let closed:bool=row.get(1).map_err(crate::native::sqlite::sqlite_error)?;let owner:Option<String>=row.get(2).map_err(crate::native::sqlite::sqlite_error)?;let epoch=crate::native::sqlite::read_u64(row,3).map_err(crate::native::sqlite::sqlite_error)?;let origin:String=row.get(4).map_err(crate::native::sqlite::sqlite_error)?;
        // A SQL→FS handoff supersedes this row only while a live FS gate
        // carries the recorded fs_epoch and origin: exact tuple proof. The
        // handoff applies to this row only when its sql_epoch still equals
        // the row's epoch; a regressed handoff fails closed.
        if let Some(handoff)=fs_handoffs.get(prefix.as_str())&&handoff.sql_epoch>=0&&handoff.sql_epoch as u64==epoch{
            if handoff.fs_epoch<=handoff.sql_epoch as u64{return Err(NarouError::Platform("FS gate handoff regress below the SQL epoch".into()));}
            if fs_gates.get(&prefix).is_some_and(|gate|gate.epoch==handoff.fs_epoch&&gate.origin_url_fold==handoff.fs_origin){continue;}
        }
        visit(&prefix,closed,owner.as_deref(),epoch,&origin)?;}}
    for (prefix,gate)in fs_gates{
        if gate.owner.is_none()&&gate.origin_url_fold.is_empty()&&!gate.migrated{continue;}
        // FS→SQL handoff: the row must carry this gate's exact fs_epoch and
        // origin, and its sql_epoch must advance beyond it.
        let handed_off=sql_handoffs.get(prefix.as_str()).is_some_and(|handoff|handoff.fs_epoch==gate.epoch&&handoff.sql_epoch>=0&&handoff.sql_epoch as u64>gate.epoch&&handoff.fs_origin==gate.origin_url_fold);
        if gate.migrated&&!handed_off{return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));}
        if handed_off{continue;}
        visit(&prefix,gate.owner.is_some(),gate.owner.as_deref(),gate.epoch,&gate.origin_url_fold)?;}
    Ok(())
}

/// Durable handoff row. `sql_epoch` keeps its signed form because the
/// per-row readers treated a negative sql_epoch as "not handed off" rather
/// than as a hard error.
struct GateHandoffRow{fs_epoch:u64,sql_epoch:i64,fs_origin:String}
/// SQL→FS handoff rows keyed by prefix.
fn load_fs_gate_handoffs(connection:&rusqlite::Connection)->Result<std::collections::BTreeMap<String,GateHandoffRow>>{
    let mut out=std::collections::BTreeMap::new();
    let mut statement=connection.prepare("SELECT prefix,fs_epoch,sql_epoch,fs_origin_fold FROM fs_gate_handoffs").map_err(crate::native::sqlite::sqlite_error)?;
    let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
    while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{
        let prefix:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;
        let fs_epoch:i64=row.get(1).map_err(crate::native::sqlite::sqlite_error)?;
        let sql_epoch:i64=row.get(2).map_err(crate::native::sqlite::sqlite_error)?;
        let origin:String=row.get(3).map_err(crate::native::sqlite::sqlite_error)?;
        if fs_epoch<0{return Err(NarouError::Platform("negative epoch in durable FS gate handoff".into()));}
        out.insert(prefix,GateHandoffRow{fs_epoch:fs_epoch as u64,sql_epoch,fs_origin:origin});
    }
    Ok(out)
}

/// FS→SQL handoff rows keyed by prefix.
fn load_sql_gate_handoffs(connection:&rusqlite::Connection)->Result<std::collections::BTreeMap<String,GateHandoffRow>>{
    let mut out=std::collections::BTreeMap::new();
    let mut statement=connection.prepare("SELECT prefix,fs_epoch,sql_epoch,fs_origin_fold FROM native_gate_handoffs").map_err(crate::native::sqlite::sqlite_error)?;
    let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
    while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{
        let prefix:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;
        let fs_epoch:i64=row.get(1).map_err(crate::native::sqlite::sqlite_error)?;
        let sql_epoch:i64=row.get(2).map_err(crate::native::sqlite::sqlite_error)?;
        let origin:String=row.get(3).map_err(crate::native::sqlite::sqlite_error)?;
        if fs_epoch<0{return Err(NarouError::Platform("negative epoch in durable SQL gate handoff".into()));}
        out.insert(prefix,GateHandoffRow{fs_epoch:fs_epoch as u64,sql_epoch,fs_origin:origin});
    }
    Ok(out)
}

/// Exact borrowed gate state for one prefix under the caller's metadata/SQL
/// boundary. `root` is the mirror archive root (the `小説データ/` directory a
/// `NativeObjectStore::from_root` was built on), so its gate file lives at
/// `<root>/.objects/.narou-transfer/.gate`. Mode/handoff authoritative and
/// includes unbound OPEN rows so first-bind/empty-capture callers can verify
/// the supplied epoch and the initialized gate themselves:
/// `(closed, epoch, origin_url_fold)`; `Ok(None)` when neither backend holds
/// a row/file for this prefix. Provenance order: a live FS gate carrying a
/// proven SQL→FS handoff wins; otherwise the SQL row; otherwise a live
/// un-adopted FS gate. Migrated-without-proof FS gates and mismatched
/// handoff tuples fail closed rather than guess.
#[cfg(test)]
pub(crate) fn read_namespace_gate_locked(root:&Path,connection:&rusqlite::Connection,prefix:&str)->Result<Option<(bool,u64,String)>>{
    use rusqlite::OptionalExtension;
    let prefix=prefix.trim_end_matches('/');
    let path=root.join(".objects").join(".narou-transfer").join(".gate").join(hex::encode(sha2::Sha256::digest(prefix.as_bytes())));
    let fs_gate:Option<FsGate>=if root.as_os_str().is_empty(){None}else{match fs::read(&path){Ok(bytes)=>{
        let gate:FsGate=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid namespace gate: {error}")))?;
        if gate.prefix.as_deref().is_some_and(|stored|stored!=prefix){return Err(NarouError::Platform("filesystem gate prefix mismatch".into()));}
        Some(gate)},
        Err(error)if error.kind()==std::io::ErrorKind::NotFound=>None,Err(error)=>return Err(error.into())}};
    let table:Option<i64>=connection.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='novel_access_gates'",[],|row|row.get(0)).optional().map_err(crate::native::sqlite::sqlite_error)?;
    if table.is_some(){
        let row:Option<(bool,u64,String)>=connection.query_row("SELECT closed,epoch,origin_url_fold FROM novel_access_gates WHERE prefix=?1",[prefix],|row|Ok((row.get(0)?,crate::native::sqlite::read_u64(row,1)?,row.get(2)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;
        if let Some((closed,epoch,origin))=row{
            // A proven SQL→FS handoff covering this exact row epoch returns
            // the live FS gate tuple instead — the row is superseded.
            if let Some((fs_epoch,fs_origin))=identity::read_fs_gate_handoff(connection,prefix,epoch)?&&let Some(gate)=fs_gate.as_ref().filter(|gate|gate.epoch==fs_epoch&&gate.origin_url_fold==fs_origin){
                if gate.migrated{return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));}
                return Ok(Some((gate.owner.is_some(),gate.epoch,gate.origin_url_fold.clone())));
            }
            return Ok(Some((closed,epoch,origin)));
        }
    }
    let Some(gate)=fs_gate else{return Ok(None);};
    if identity::read_sql_gate_handoff(connection,prefix,gate.epoch,&gate.origin_url_fold)?{return Ok(None);}
    if gate.migrated{return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));}
    Ok(Some((gate.owner.is_some(),gate.epoch,gate.origin_url_fold)))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileStamp { identity:String, size:u64, modified:String, changed:String }

fn file_stamp(path:&Path)->Result<FileStamp>{let file=fs::File::open(path)?;file_stamp_opened(&file)}
fn file_stamp_opened(file:&fs::File)->Result<FileStamp>{
    let metadata=file.metadata()?;
    if !metadata.is_file(){return Err(NarouError::Platform("verified object is not a regular file".into()));}
    #[cfg(unix)]
    {use std::os::unix::fs::MetadataExt;Ok(FileStamp{identity:format!("{}:{}",metadata.dev(),metadata.ino()),size:metadata.len(),modified:format!("{}:{}",metadata.mtime(),metadata.mtime_nsec()),changed:format!("{}:{}",metadata.ctime(),metadata.ctime_nsec())})}
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle,GetFileInformationByHandleEx,BY_HANDLE_FILE_INFORMATION,FILE_BASIC_INFO,FileBasicInfo};
        let mut identity:BY_HANDLE_FILE_INFORMATION=unsafe{std::mem::zeroed()};
        let mut basic:FILE_BASIC_INFO=unsafe{std::mem::zeroed()};
        if unsafe{GetFileInformationByHandle(file.as_raw_handle(),&mut identity)}==0||unsafe{GetFileInformationByHandleEx(file.as_raw_handle(),FileBasicInfo,(&mut basic as *mut FILE_BASIC_INFO).cast(),std::mem::size_of::<FILE_BASIC_INFO>() as u32)}==0{return Err(std::io::Error::last_os_error().into());}
        Ok(FileStamp{identity:format!("{}:{}:{}",identity.dwVolumeSerialNumber,identity.nFileIndexHigh,identity.nFileIndexLow),size:metadata.len(),modified:basic.LastWriteTime.to_string(),changed:basic.ChangeTime.to_string()})
    }
    #[cfg(not(any(unix,windows)))]
    {let _=(file,metadata);Err(NarouError::Platform("OS file identity is unavailable".into()))}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct VerifiedFile { stamp:FileStamp, sha256:String, source:Option<String>, pending:bool }

impl NativeObjectStore {
    pub(crate) fn forget_file_proof(&self,key:&ObjectKey)->Result<()>{let path=self.proof_path(key)?;match fs::remove_file(path){Ok(())=>Ok(()),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(()),Err(error)=>Err(error.into())}}
    fn require_open_writer_locked(&self,fence:&crate::platform::WriteFence)->Result<FsGate>{
        let gate:FsGate=serde_json::from_slice(&fs::read(self.gate_path(&fence.prefix)?)?).map_err(|error|NarouError::Platform(error.to_string()))?;
        if gate.migrated{return Err(af::gate_closed_error(&fence.prefix));}
        match self.live_sql_authority(&fence.prefix,gate.epoch,&gate.origin_url_fold)?{SqlGateAuthority::Superseded|SqlGateAuthority::Conflict=>return Err(af::gate_closed_error(&fence.prefix)),_=>{}}
        if gate.owner.is_some()||gate.epoch!=fence.epoch{return Err(af::gate_epoch_error(&fence.prefix,fence.epoch,gate.epoch));}Ok(gate)
    }
    /// Called only while `NativeStore::with_open_writes` holds this OS lock.
    /// Persist invalidation before YAML deletion: a crash can reject stale
    /// jobs conservatively, but can never resurrect deleted canonical bytes.
    pub(crate) fn invalidate_writer_locked(&self,fence:&crate::platform::WriteFence)->Result<()>{
        let mut gate=self.require_open_writer_locked(fence)?;gate.epoch=gate.epoch.checked_add(1).ok_or_else(||NarouError::Platform("gate epoch overflow".into()))?;gate.updated_at=ts::now_secs();
        let path=self.gate_path(&fence.prefix)?;write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)
    }
    pub(crate) fn file_generation(&self,key:&ObjectKey)->Result<Option<String>>{
        match file_stamp(&self.resolve_read_path(key)?){Ok(stamp)=>serde_json::to_string(&stamp).map(Some).map_err(|error|NarouError::Platform(error.to_string())),Err(NarouError::Io(error))if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(error)=>Err(error)}
    }
    fn proof_path(&self,key:&ObjectKey)->Result<PathBuf>{self.resolve_path(&ObjectKey::try_new(format!(".narou-transfer/.proof/{}",hex::encode(sha2::Sha256::digest(key.as_ref().as_bytes()))))?)}
    fn read_file_proof(&self,key:&ObjectKey)->Result<Option<VerifiedFile>>{
        match fs::read(self.proof_path(key)?){Ok(bytes)=>serde_json::from_slice(&bytes).map(Some).map_err(|error|NarouError::Platform(format!("invalid file proof: {error}"))),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(error)=>Err(error.into())}
    }
    fn put_file_proof(&self,key:&ObjectKey,proof:&VerifiedFile)->Result<()>{let path=self.proof_path(key)?;self.ensure_parent_dir(&path)?;atomic_write_bytes(&path,&serde_json::to_vec(proof).map_err(|error|NarouError::Platform(error.to_string()))?)?;sync_transfer_parent(&path)}
    pub(crate) fn save_verified_file(&self,key:&ObjectKey,size:u64,sha:&str)->Result<()>{
        let stamp=file_stamp(&self.resolve_path(key)?)?;if stamp.size!=size{return Err(NarouError::Platform("verified file length changed".into()));}
        self.put_file_proof(key,&VerifiedFile{stamp,sha256:sha.to_ascii_lowercase(),source:None,pending:false})
    }
    pub(crate) fn trusted_file_fingerprint(&self,key:&ObjectKey)->Result<Option<(u64,String)>>{
        let Some(proof)=self.read_file_proof(key)?else{return Ok(None);};
        let stamp=match file_stamp(&self.resolve_path(key)?){Ok(stamp)=>stamp,Err(NarouError::Io(error))if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(error)=>return Err(error)};
        if proof.pending||proof.stamp!=stamp{return Err(NarouError::Platform("recovery fingerprint conflict: OS file identity/change tag differs".into()));}
        Ok(Some((stamp.size,proof.sha256)))
    }
    /// The SHA was checked while staging. Only file identity/change metadata is
    /// inspected here; publication and rollback never re-read the payload.
    pub(crate) fn install_verified_file(&self,stage:&ObjectKey,key:&ObjectKey)->Result<(u64,String)>{
        let from=self.resolve_path(stage)?;let target=self.resolve_path(key)?;
        if !from.exists(){
            let mut proof=self.read_file_proof(key)?.filter(|proof|proof.source.as_deref()==Some(stage.as_ref())).ok_or_else(||NarouError::NotFound(stage.to_string()))?;
            let stamp=file_stamp(&target)?;
            if proof.pending{
                // rename changes the OS change tag, but not file ID, length or
                // last-write time. This is the durable rename-intent recovery.
                if stamp.identity!=proof.stamp.identity||stamp.size!=proof.stamp.size||stamp.modified!=proof.stamp.modified{return Err(NarouError::Platform("recovery rename fingerprint conflict".into()));}
                proof.stamp=stamp;proof.pending=false;self.put_file_proof(key,&proof)?;
            }else if stamp!=proof.stamp{return Err(NarouError::Platform("recovery fingerprint conflict".into()));}
            return Ok((proof.stamp.size,proof.sha256));
        }
        let mut proof=self.read_file_proof(stage)?.ok_or_else(||NarouError::Platform("sealed stage file proof missing".into()))?;
        if proof.pending||file_stamp(&from)?!=proof.stamp{return Err(NarouError::Platform("sealed stage file identity changed".into()));}
        proof.source=Some(stage.to_string());proof.pending=true;self.put_file_proof(key,&proof)?;
        self.ensure_parent_dir(&target)?;fs::rename(&from,&target)?;sync_transfer_parent(&target)?;sync_transfer_parent(&from)?;
        proof.stamp=file_stamp(&target)?;proof.pending=false;self.put_file_proof(key,&proof)?;let _=self.forget_file_proof(stage);Ok((proof.stamp.size,proof.sha256))
    }
    pub(crate) fn remove_verified_file(&self,key:&ObjectKey,sha:&str)->Result<()>{
        let path=self.resolve_path(key)?;if !path.exists(){return Ok(());}
        let (_,current)=self.trusted_file_fingerprint(key)?.ok_or_else(||NarouError::Platform("recovery file proof missing".into()))?;
        if !current.eq_ignore_ascii_case(sha){return Err(NarouError::Platform("recovery fingerprint conflict".into()));}fs::remove_file(&path)?;sync_transfer_parent(&path)
    }
}

struct CutoverLockGuard(fs::File);

impl Drop for CutoverLockGuard {
    fn drop(&mut self) {
        // Explicit unlock releases immediately before handle close / reuse on both Windows and Unix.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl NativeObjectStore {
    fn acquire_cutover_lock(&self, exclusive: bool) -> Result<CutoverLockGuard> {
        let path = self.resolve_path(&ObjectKey::try_new(".narou-transfer/.lock/_cutover")?)?;
        self.ensure_parent_dir(&path)?;
        let file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        let acquired = if exclusive { fs2::FileExt::try_lock_exclusive(&file) } else { fs2::FileExt::try_lock_shared(&file) };
        acquired.map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                NarouError::Platform("novel_busy: OS mutation lock is held".into())
            } else { error.into() }
        })?;
        Ok(CutoverLockGuard(file))
    }
    /// Library-wide exclusive barrier for SQLite→YAML cutover: excludes every
    /// per-prefix writer/reader lock in one FD instead of opening N prefix FDs.
    pub(crate) fn with_cutover_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _cutover = self.acquire_cutover_lock(true)?;
        operation()
    }
    /// The caller's closure (including canonical rename) runs under this OS lock.
    /// A paused process still owns its kernel lock; no wall-clock takeover exists.
    pub fn with_transfer_lock<T>(&self, identity: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.with_namespace_lock(identity, false, operation)
    }
    /// Readers share publication exclusion, then serialize only gate metadata.
    /// A live publisher still fails admission immediately, without takeover.
    pub(crate) fn with_reader_gate_lock<T>(&self, identity: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.with_namespace_lock(identity, true, operation)
    }
    fn with_namespace_lock<T>(&self, identity: &str, reader: bool, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _cutover = self.acquire_cutover_lock(false)?;
        let digest = hex::encode(sha2::Sha256::digest(identity.as_bytes()));
        let path = self.resolve_path(&ObjectKey::try_new(format!(".narou-transfer/.lock/{digest}"))?)?;
        self.ensure_parent_dir(&path)?;
        let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        let acquired = if reader { fs2::FileExt::try_lock_shared(&lock) } else { fs2::FileExt::try_lock_exclusive(&lock) };
        acquired.map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                NarouError::Platform("novel_busy: OS mutation lock is held".into())
            } else { error.into() }
        })?;
        let _lock = CutoverLockGuard(lock);
        let _readers = if reader {
            let path = self.resolve_path(&ObjectKey::try_new(format!(".narou-transfer/.lock/{digest}.readers"))?)?;
            let file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
            fs2::FileExt::lock_exclusive(&file)?;
            Some(CutoverLockGuard(file))
        } else { None };
        operation()
    }

    fn writer_for_key(&self,key:&ObjectKey)->Result<Option<crate::platform::WriteFence>>{
        if !key.as_ref().starts_with("novels/"){return Ok(None);}let mut value=key.as_ref();
        while let Some((parent,_))=value.rsplit_once('/'){
            let prefix=ObjectPrefix::new(parent)?;let path=self.gate_path(&prefix)?;
            match fs::read(path){Ok(bytes)=>{let gate:FsGate=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(error.to_string()))?;if gate.migrated{return Err(af::gate_closed_error(&prefix));}match self.live_sql_authority(&prefix,gate.epoch,&gate.origin_url_fold)?{SqlGateAuthority::Superseded|SqlGateAuthority::Conflict=>return Err(af::gate_closed_error(&prefix)),_=>{}}if gate.owner.is_some(){return Err(af::novel_busy_error(&prefix));}return Ok(Some(crate::platform::WriteFence::new(prefix,gate.epoch)));},
                Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}
            value=parent;
        }Ok(None)
    }
    pub(crate) fn gate_path(&self, prefix: &ObjectPrefix) -> Result<PathBuf> {
        self.resolve_path(&ObjectKey::try_new(format!(".narou-transfer/.gate/{}",
            hex::encode(sha2::Sha256::digest(prefix.as_ref().trim_end_matches('/').as_bytes()))))?)
    }
    fn with_gate<T>(&self, prefix: &ObjectPrefix, operation: impl FnOnce(&mut FsGate) -> Result<T>) -> Result<T> {
        self.with_gate_mode(prefix, false, operation)
    }
    fn with_reader_gate<T>(&self, prefix: &ObjectPrefix, operation: impl FnOnce(&mut FsGate) -> Result<T>) -> Result<T> {
        self.with_gate_mode(prefix, true, operation)
    }
    fn with_gate_mode<T>(&self, prefix: &ObjectPrefix, reader: bool, operation: impl FnOnce(&mut FsGate) -> Result<T>) -> Result<T> {
        self.with_namespace_lock(prefix.as_ref().trim_end_matches('/'), reader, || {
            let path = self.gate_path(prefix)?;
            self.ensure_parent_dir(&path)?;
            let mut gate: FsGate = match fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(format!("invalid durable gate: {error}")))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => FsGate::default(),
                Err(error) => return Err(error.into()),
            };
            gate.prefix=Some(prefix.as_ref().trim_end_matches('/').to_owned());
            gate.prune();
            if gate.migrated{return Err(af::gate_closed_error(prefix));}
            match self.live_sql_authority(prefix,gate.epoch,&gate.origin_url_fold.clone())?{
                SqlGateAuthority::Superseded=>{
                    // Committed FS→SQL handoff; the marker write never landed.
                    // Heal the marker before rejecting so the file stops
                    // presenting live authority even if this op retried.
                    gate.migrated=true;gate.updated_at=ts::now_secs();
                    write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)?;
                    return Err(af::gate_closed_error(prefix));
                }
                SqlGateAuthority::Conflict=>return Err(NarouError::Platform(format!("conflicting live SQL authority for {}",prefix.as_ref()))),
                _=>{}
            }
            let before = scan_relevant(&gate);
            let value = operation(&mut gate)?;
            // The uniform writer compares the scan-relevant projection and
            // invalidates the scan cache only when it changed; a
            // content_revision-only write (ordinary content save) keeps the
            // cache (issue #58).
            write_gate_file_with(&self.root, &path, &serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?, Some(before))?;
            Ok(value)
        })
    }
    /// Pure read through the shared reader namespace lock: validates the gate
    /// and runs `operation` without writing the durable gate back. Bulk readers
    /// (snapshot copies, EPUB builds) open one file per call, so the old
    /// unconditional rewrite paid a full atomic write+fsync per file (issue #58).
    /// `gate.prune()`/`prefix` changes are in-memory only here; the single
    /// read-path gate mutation is the `Superseded` heal, which writes and
    /// returns before this point.
    fn with_reader_gate_read<T>(&self, prefix: &ObjectPrefix, operation: impl FnOnce(&mut FsGate) -> Result<T>) -> Result<T> {
        self.with_namespace_lock(prefix.as_ref().trim_end_matches('/'), true, || {
            let path = self.gate_path(prefix)?;
            self.ensure_parent_dir(&path)?;
            let mut gate: FsGate = match fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(format!("invalid durable gate: {error}")))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => FsGate::default(),
                Err(error) => return Err(error.into()),
            };
            gate.prefix=Some(prefix.as_ref().trim_end_matches('/').to_owned());
            gate.prune();
            if gate.migrated{return Err(af::gate_closed_error(prefix));}
            match self.live_sql_authority(prefix,gate.epoch,&gate.origin_url_fold.clone())?{
                SqlGateAuthority::Superseded=>{
                    gate.migrated=true;gate.updated_at=ts::now_secs();
                    write_gate_file(&self.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)?;
                    return Err(af::gate_closed_error(prefix));
                }
                SqlGateAuthority::Conflict=>return Err(NarouError::Platform(format!("conflicting live SQL authority for {}",prefix.as_ref()))),
                _=>{}
            }
            operation(&mut gate)
        })
    }
    fn with_stage<T>(&self, id: &str, operation: impl FnOnce(Option<ts::StageLedgerRecord>) -> Result<(T, Option<ts::StageLedgerRecord>)>) -> Result<T> {
        let ledger = self.resolve_path(&ts::stage_ledger_key(id)?)?;
        self.ensure_parent_dir(&ledger)?;
        self.with_transfer_lock(&format!("stage/{id}"), || {
            let current = match fs::read(&ledger) {
                Ok(bytes) => Some(ts::decode_ledger(&bytes)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let (result, next) = operation(current)?;
            if let Some(next) = next {
                if next.lease.state==ts::StageLeaseState::Writing{
                    let checkpoint=self.resolve_path(&ts::stage_checkpoint_key(id,next.confirmed_bytes)?)?;self.ensure_parent_dir(&checkpoint)?;
                    atomic_write_bytes(&checkpoint,&serde_json::to_vec(&next.cursor()).map_err(|error|NarouError::Platform(error.to_string()))?)?;sync_transfer_parent(&checkpoint)?;
                }
                atomic_write_bytes(&ledger, &ts::encode_ledger(&next)?)?; sync_transfer_parent(&ledger)?;
            }
            Ok(result)
        })
    }
    pub(crate) fn open_range(&self, key: &ObjectKey, range: std::ops::Range<u64>) -> Result<Option<(fs::File, u64)>> {
        use std::io::{Seek, SeekFrom};
        if range.start > range.end { return Err(NarouError::Platform("invalid asset range".into())); }
        let mut file = match fs::File::open(self.resolve_read_path(key)?) {
            Ok(file) => file, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() { return Ok(None); }
        let end = if range.end == u64::MAX { metadata.len() } else { range.end };
        if range.start > end || end > metadata.len() { return Err(NarouError::Platform("asset range exceeds object".into())); }
        file.seek(SeekFrom::Start(range.start))?;
        Ok(Some((file, end - range.start)))
    }
    /// Validate admission and open the file within the same OS critical section.
    pub fn open_leased_asset<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<(ObjectMetadata,AssetStream)>>> {
        let this=self.clone();let prefix=prefix.clone();let lease=lease.clone();let key=key.clone();Box::pin(async move{
            let opened=run_blocking(move||this.with_reader_gate_read(&prefix,|gate|{
                if !prefix.matches(&key)||gate.owner.is_some()||gate.epoch!=lease.epoch{return Err(af::gate_closed_error(&prefix));}
                match gate.readers.get(&lease.reader_id){
                    Some((epoch,expiry)) if *epoch==lease.epoch&&*expiry>ts::now_secs()=>{},
                    Some((epoch,_)) if *epoch!=lease.epoch=>return Err(af::gate_closed_error(&prefix)),
                    _=>return Err(af::lease_expired_error(&prefix)),
                }
                let Some((file,length))=this.open_range(&key,range)?else{return Ok(None);};Ok(Some((Self::metadata_for_open_file(&key,&file)?,file,length)))
            })).await?;Ok(opened.map(|(metadata,file,length)|(metadata,native_range_stream(file,length))))
        })
    }
    pub fn read_stream_leased<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{Ok(self.open_leased_asset(prefix,lease,key,range).await?.map(|(_,stream)|stream))})}

    fn ref_marker_exists(&self,key:&ObjectKey)->Result<bool>{Ok(self.resolve_path(&remote_reference_key(key)?)?.is_file())}
    /// Local-only open within the same OS critical section: the reference
    /// marker check and the file open share the `with_gate`/`with_transfer_lock`
    /// boundary that `commit_remote_reference` uses, so no stale mirror bytes
    /// can be served for a key that already resolved elsewhere.
    pub fn open_local_leased_asset<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<(ObjectMetadata,AssetStream)>>>{
        let this=self.clone();let prefix=prefix.clone();let lease=lease.clone();let key=key.clone();Box::pin(async move{
            let opened=run_blocking(move||this.with_reader_gate_read(&prefix,|gate|{
                if !prefix.matches(&key)||gate.owner.is_some()||gate.epoch!=lease.epoch{return Err(af::gate_closed_error(&prefix));}
                match gate.readers.get(&lease.reader_id){
                    Some((epoch,expiry)) if *epoch==lease.epoch&&*expiry>ts::now_secs()=>{},
                    Some((epoch,_)) if *epoch!=lease.epoch=>return Err(af::gate_closed_error(&prefix)),
                    _=>return Err(af::lease_expired_error(&prefix)),
                }
                if this.ref_marker_exists(&key)?{return Ok(None);}
                let Some((file,length))=this.open_range(&key,range)?else{return Ok(None);};Ok(Some((Self::metadata_for_open_file(&key,&file)?,file,length)))
            })).await?;Ok(opened.map(|(metadata,file,length)|(metadata,native_range_stream(file,length))))
        })
    }
    /// Local-only open under the prefix's OS transfer lock without an admission.
    /// `Ok(None)` covers missing keys and canonical references alike.
    pub fn local_asset_stream<'a>(&'a self,prefix:&'a ObjectPrefix,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{
        let this=self.clone();let prefix=prefix.clone();let key=key.clone();Box::pin(async move{
            run_blocking(move||this.with_reader_gate_lock(prefix.as_ref().trim_end_matches('/'),||{
                if this.gate_path(&prefix).map(|path|path.is_file())?&&{let bytes=fs::read(this.gate_path(&prefix)?)?;let gate:FsGate=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(format!("invalid durable gate: {error}")))?;gate.migrated}{return Err(af::gate_closed_error(&prefix));}
                if this.ref_marker_exists(&key)?{return Ok(None);}
                Ok(this.open_range(&key,range)?.map(|(file,length)|native_range_stream(file,length)))
            })).await
        })
    }
}

fn sync_transfer_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() { fs::File::open(parent)?.sync_all()?; }
    // Windows provides file flush+atomic rename, not POSIX directory fsync.
    let _ = path;
    Ok(())
}
pub(crate) fn native_range_stream(file: fs::File, remaining: u64) -> AssetStream {
    Box::pin(futures::stream::try_unfold((file, remaining), |(mut file, remaining)| run_blocking(move || {
        use std::io::Read;
        if remaining == 0 { return Ok(None); }
        let mut bytes = vec![0; remaining.min(64 * 1024) as usize];
        file.read_exact(&mut bytes).map_err(|error| NarouError::Platform(format!("asset range truncated: {error}")))?;
        let next = remaining - bytes.len() as u64;
        Ok(Some((bytes, (file, next))))
    })))
}
fn file_sha256(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?; let mut hash = sha2::Sha256::new(); let mut buffer = [0; 64 * 1024];
    loop { let size = file.read(&mut buffer)?; if size == 0 { break; } hash.update(&buffer[..size]); }
    Ok(hex::encode(hash.finalize()))
}

impl TransferStageStore for NativeObjectStore {
    fn stage_open<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64, ttl: i64) -> PlatformFuture<'a, Result<StageCursor>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_stage(id, |record| {
            let record = match record { Some(record) => { ts::require_owner(&record, owner, epoch)?;
                if record.lease.state == ts::StageLeaseState::Aborting { return Err(NarouError::Platform("stage_aborted".into())); } record },
                None => ts::StageLedgerRecord::new(id, owner, epoch, ttl)? };
            Ok((record.cursor(), Some(record)))
        })) })
    }
    fn stage_cursor<'a>(&'a self, id: &'a str) -> PlatformFuture<'a, Result<Option<StageCursor>>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_stage(id, |record| Ok((record.as_ref().map(|record| record.cursor()), None)))) })
    }
    fn stage_append<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64, offset: u64, payload: &'a [u8], final_chunk: bool) -> PlatformFuture<'a, Result<StageCursor>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_stage(id, |record| {
            use std::io::{Seek, SeekFrom};
            let mut record = record.ok_or_else(|| NarouError::Platform("stage not open".into()))?;
            record.append(owner, epoch, offset, payload, final_chunk)?;
            let path = self.resolve_path(&ts::stage_data_key(id)?)?; self.ensure_parent_dir(&path)?;
            let mut file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)?;
            if file.metadata()?.len() < offset { return Err(NarouError::Platform("confirmed stage bytes disappeared".into())); }
            file.set_len(offset)?; file.seek(SeekFrom::Start(offset))?; file.write_all(payload)?; file.sync_all()?; sync_transfer_parent(&path)?;
            Ok((record.cursor(), Some(record)))
        })) })
    }
    fn stage_rewind<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64, checkpoint: &'a StageCursor) -> PlatformFuture<'a, Result<StageCursor>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_stage(id, |record| {
            let mut record = record.ok_or_else(|| NarouError::Platform("stage not open".into()))?;
            let trusted=if checkpoint.confirmed_bytes==0{ts::StageLedgerRecord::new(id,owner,epoch,1)?.cursor()}else{
                let path=self.resolve_path(&ts::stage_checkpoint_key(id,checkpoint.confirmed_bytes)?)?;let stored:StageCursor=serde_json::from_slice(&fs::read(path)?).map_err(|error|NarouError::Platform(error.to_string()))?;
                if stored.prefix_sha256()!=checkpoint.prefix_sha256(){return Err(NarouError::Platform("rewind SHA checkpoint conflict".into()));}stored
            };
            record.rewind(owner, epoch, &trusted)?;
            let path = self.resolve_path(&ts::stage_data_key(id)?)?;
            match fs::OpenOptions::new().write(true).open(path) {
                Ok(file) => { file.set_len(checkpoint.confirmed_bytes)?; file.sync_all()?; },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && checkpoint.confirmed_bytes == 0 => {},
                Err(error) => return Err(error.into()),
            }
            Ok((record.cursor(), Some(record)))
        })) })
    }
    fn stage_read<'a>(&'a self, id: &'a str, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move {
            let opened = run_scoped_blocking(|| self.with_stage(id, |record| {
                let result=if let Some(record)=record.filter(|record|record.lease.state==ts::StageLeaseState::Sealed){let key=ts::stage_data_key(id)?;let Some((file,length))=self.open_range(&key,range)?else{return Err(NarouError::Platform("sealed stage payload missing".into()));};let(size,sha)=self.verified_opened_fingerprint(&key,&file)?;
                    if size!=record.confirmed_bytes||!record.cursor().prefix_sha256().is_some_and(|expected|expected.eq_ignore_ascii_case(&sha)){return Err(NarouError::Platform("sealed stage size/SHA proof conflict".into()));}Some((file,length))
                }else{None};Ok((result,None))
            }))?;
            Ok(opened.map(|(file, length)| native_range_stream(file, length)))
        })
    }
    fn stage_touch<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64, ttl: i64) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_stage(id, |record| {
            let mut record = record.ok_or_else(|| NarouError::Platform("stage not open".into()))?; ts::require_owner(&record, owner, epoch)?;
            if record.lease.state == ts::StageLeaseState::Aborting { return Err(NarouError::Platform("stage_aborted".into())); }
            record.lease.expires_at = ts::now_secs().saturating_add(ttl.max(1)); Ok(((), Some(record)))
        })) })
    }
    fn stage_seal<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64, size: u64, sha: &'a str) -> PlatformFuture<'a, Result<SealedAsset>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_stage(id, |record| {
            let mut record = record.ok_or_else(|| NarouError::Platform("stage not open".into()))?; let sealed = record.verify_seal(owner, epoch, size, sha)?;
            let path = self.resolve_path(&sealed.key)?; let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
            if file.metadata()?.len() < size { return Err(NarouError::Platform("stage bytes disappeared".into())); }
            file.set_len(size)?; file.sync_all()?;self.save_verified_file(&sealed.key,size,sha)?;record.lease.state = ts::StageLeaseState::Sealed;
            Ok((sealed, Some(record)))
        })) })
    }
    fn stage_abort<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            run_scoped_blocking(|| self.with_stage(id, |record| {
                let Some(mut record) = record else { return Ok(((), None)); }; ts::require_owner(&record, owner, epoch)?;
                record.lease.state = ts::StageLeaseState::Aborting; Ok(((), Some(record)))
            }))?;
            run_scoped_blocking(|| self.with_stage(id, |record| {
                let Some(record) = record else { return Ok(((), None)); }; ts::require_owner(&record, owner, epoch)?;
                let path = self.resolve_path(&ts::stage_data_key(id)?)?;
                match fs::remove_file(&path) { Ok(()) => sync_transfer_parent(&path)?, Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}, Err(error) => return Err(error.into()) }
                let prefix=self.resolve_prefix_dir(&format!(".narou-transfer/.checkpoints/{id}"))?;let mut more=false;
                match fs::read_dir(&prefix){Ok(entries)=>{for(index,entry)in entries.enumerate(){if index==32{more=true;break;}let entry=entry?;if !entry.file_type()?.is_file(){return Err(NarouError::Platform("unexpected transfer checkpoint entry".into()));}fs::remove_file(entry.path())?;}},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}
                if more{Ok(((),Some(record)))}else{let ledger=self.resolve_path(&ts::stage_ledger_key(id)?)?;fs::remove_file(&ledger)?;sync_transfer_parent(&ledger)?;Ok(((),None))}
            }))
        })
    }
    fn stage_lease_page<'a>(&'a self, cursor: Option<&'a str>, limit: usize) -> PlatformFuture<'a, Result<(Vec<StageLease>, Option<String>)>> {
        Box::pin(ts::lease_page(self, cursor, limit.min(100)))
    }
    fn stage_reclaim<'a>(&'a self, id: &'a str, owner: &'a str, epoch: u64) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            let matching=run_scoped_blocking(||self.with_stage(id,|record|{let Some(mut record)=record else{return Ok((true,None));};if record.lease.owner!=owner||record.lease.epoch!=epoch||(record.lease.state!=ts::StageLeaseState::Aborting&&record.lease.expires_at>ts::now_secs()){return Ok((false,None));}record.lease.state=ts::StageLeaseState::Aborting;Ok((true,Some(record)))}))?;
            if !matching { return Ok(false); }self.stage_abort(id,owner,epoch).await?;Ok(self.stage_cursor(id).await?.is_none())
        })
    }
}

impl NovelAccessFence for NativeObjectStore {
    fn retire_writer<'a>(&'a self,fence:&'a crate::platform::WriteFence,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{af::parse_removal_owner(owner)?;let next=fence.epoch.checked_add(1).ok_or_else(||NarouError::Platform("gate epoch overflow".into()))?;run_scoped_blocking(||self.with_gate(&fence.prefix,|gate|{
        if gate.owner.as_deref()==Some(owner)&&gate.epoch==next{return Ok(next);}
        if gate.owner.is_some()||gate.epoch!=fence.epoch{return Err(af::gate_epoch_error(&fence.prefix,fence.epoch,gate.epoch));}gate.epoch=next;gate.owner=Some(owner.into());gate.updated_at=ts::now_secs();Ok(next)
    }))})}
    fn pending_removals<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<(Vec<(ObjectPrefix,String,u64)>,Option<String>)>>{Box::pin(async move{
        let mut request=ObjectListRequest::new(ObjectPrefix::new(".narou-transfer/.gate/")?,std::num::NonZeroUsize::new(limit.clamp(1,100)).expect("nonzero"));if let Some(cursor)=cursor{request=request.after(cursor);}let page=self.list_page(&request).await?;let mut out=Vec::new();
        for row in page.objects{let Some(bytes)=self.read_small(&row.key).await?else{continue;};let gate:FsGate=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(error.to_string()))?;if let Some(owner)=gate.owner.filter(|owner|owner.starts_with("remove-v1:")){af::parse_removal_owner(&owner)?;out.push((ObjectPrefix::new(gate.prefix.ok_or_else(||NarouError::Platform("removal gate prefix missing".into()))?)?,owner,gate.epoch));}}
        Ok((out,page.next_cursor))
    })}
    /// Writer admission must leave durable gate state: the read-only
    /// `gate_info` path no longer creates the file, but later
    /// `namespace_state_locked`/origin binds read it directly. Capture keeps
    /// the write-back (creating) reader path, which also performs the
    /// SQL-authority check.
    fn capture_writer<'a>(&'a self, prefix: &'a ObjectPrefix) -> PlatformFuture<'a, Result<crate::platform::WriteFence>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_reader_gate(prefix, |gate| {
            if gate.owner.is_some() { return Err(af::novel_busy_error(prefix)); }
            Ok(crate::platform::WriteFence::new(prefix.clone(), gate.epoch))
        })) })
    }
    fn rebind_writer<'a>(&'a self,prefix:&'a ObjectPrefix,expected_epoch:u64)->PlatformFuture<'a,Result<crate::platform::WriteFence>>{Box::pin(async move{run_scoped_blocking(||self.with_gate(prefix,|gate|{if gate.owner.is_some()||gate.epoch!=expected_epoch{return Err(af::gate_epoch_error(prefix,expected_epoch,gate.epoch));}Ok(crate::platform::WriteFence::new(prefix.clone(),expected_epoch))}))})}
    fn admit_reader<'a>(&'a self, prefix: &'a ObjectPrefix, id: &'a str, ttl: i64) -> PlatformFuture<'a, Result<GateLease>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_reader_gate(prefix, |gate| {
            if gate.owner.is_some() { return Err(af::gate_closed_error(prefix)); }
            let expiry = ts::now_secs().saturating_add(ttl.clamp(1, af::READER_LEASE_SECS));
            gate.readers.insert(id.into(), (gate.epoch, expiry));
            Ok(GateLease { reader_id: id.into(), epoch: gate.epoch, expires_at: expiry })
        })) })
    }
    fn renew_reader<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a GateLease, ttl: i64) -> PlatformFuture<'a, Result<GateLease>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_reader_gate(prefix, |gate| {
            if gate.owner.is_some() || gate.epoch != lease.epoch { return Err(af::gate_closed_error(prefix)); }
            match gate.readers.get(&lease.reader_id) {
                Some((epoch, expiry)) if *epoch == lease.epoch && *expiry > ts::now_secs() => {}
                // A same-id admission at a different epoch is a stale token
                // (gate boundary); a missing/expired row is a dead admission.
                Some((epoch, _)) if *epoch != lease.epoch => return Err(af::gate_closed_error(prefix)),
                _ => return Err(af::lease_expired_error(prefix)),
            }
            let expiry = ts::now_secs().saturating_add(ttl.clamp(1, af::READER_LEASE_SECS));
            gate.readers.insert(lease.reader_id.clone(), (gate.epoch, expiry));
            Ok(GateLease { reader_id: lease.reader_id.clone(), epoch: gate.epoch, expires_at: expiry })
        })) })
    }
    fn release_reader<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a GateLease) -> PlatformFuture<'a, Result<()>> {
        // `with_gate_raw`: release stays idempotent after SQL adoption too —
        // a migrated gate may still hold a pre-cutover reader id to clear.
        // The matching SQL lease row (if a live SQLite handle knows the
        // token) drains in the same OS critical section so a release racing
        // an adoption never leaves the lease live on the other side.
        Box::pin(async move { run_scoped_blocking(|| self.with_gate_raw(prefix, |gate| {
            if gate.readers.get(&lease.reader_id).is_some_and(|(epoch, _)| *epoch == lease.epoch) { gate.readers.remove(&lease.reader_id); }
            self.release_sql_reader_lease(prefix, lease)
        })) })
    }
    fn gate_info<'a>(&'a self, prefix: &'a ObjectPrefix) -> PlatformFuture<'a, Result<GateInfo>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_reader_gate_read(prefix, |gate| Ok(GateInfo { epoch: gate.epoch, content_revision:gate.content_revision,
            state: if gate.owner.is_some() { GateState::Closed } else { GateState::Open }, pending_readers: gate.readers.len() as u32, owner: gate.owner.clone(), updated_at: gate.updated_at }))) })
    }
    fn close_gate<'a>(&'a self, prefix: &'a ObjectPrefix, owner: &'a str) -> PlatformFuture<'a, Result<u64>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(prefix, |gate| {
            if let Some(current) = &gate.owner { if current != owner { return Err(af::gate_owner_error(prefix, current)); } return Ok(gate.epoch); }
            gate.epoch = gate.epoch.checked_add(1).ok_or_else(|| NarouError::Platform("gate epoch overflow".into()))?;
            gate.owner = Some(owner.into()); gate.updated_at = ts::now_secs(); Ok(gate.epoch)
        })) })
    }
    fn renew_gate<'a>(&'a self, prefix: &'a ObjectPrefix, owner: &'a str, epoch: u64) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(prefix, |gate| {
            if gate.owner.as_deref() != Some(owner) || gate.epoch != epoch { return Err(af::gate_epoch_error(prefix, epoch, gate.epoch)); } gate.updated_at = ts::now_secs(); Ok(())
        })) })
    }
    fn open_gate<'a>(&'a self, prefix: &'a ObjectPrefix, owner: &'a str, epoch: u64) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(prefix, |gate| {
            if gate.owner.as_deref() != Some(owner) || gate.epoch != epoch { return Err(af::gate_epoch_error(prefix, epoch, gate.epoch)); }
            if owner.starts_with("remove-v1:")&&af::parse_removal_owner(owner)?.1{gate.origin_url_fold.clear();}gate.owner = None; gate.updated_at = ts::now_secs(); Ok(())
        })) })
    }
    fn commit_staged<'a>(&'a self, prefix: &'a ObjectPrefix, owner: &'a str, epoch: u64, stage: &'a ObjectKey, canonical: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(prefix, |gate| {
            gate.require_writer(prefix, owner, epoch)?;
            if !prefix.matches(canonical) || !crate::platform::is_reserved_key(stage) { return Err(NarouError::Platform("invalid fenced install namespace".into())); }
            self.install_verified_file(stage,canonical)?;
            gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;Ok(())
        })) })
    }
    fn remove_committed<'a>(&'a self, prefix: &'a ObjectPrefix, owner: &'a str, epoch: u64, key: &'a ObjectKey, sha: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { run_scoped_blocking(|| self.with_gate(prefix, |gate| {
            gate.require_writer(prefix, owner, epoch)?; if !prefix.matches(key) { return Err(NarouError::Platform("rollback key outside novel".into())); }
            self.remove_verified_file(key,sha)?;
            gate.content_revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision overflow".into()))?;Ok(())
        })) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_write_reports_replace_failure_without_exposing_root() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let key = ObjectKey::try_new("books/chapter.yaml").unwrap();
        let target = store.resolve_path(&key).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), b"original").unwrap();

        let error = store.write_small_blocking(&key, b"replacement".to_vec()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("replace destination for object \"books/chapter.yaml\""));
        assert!(!message.contains(&*root.path().file_name().unwrap().to_string_lossy()));
        assert_eq!(
            crate::application::jobs::classify_failure(&error),
            crate::application::jobs::JobFailureClass::Retryable,
        );
        let NarouError::ObjectIo { source, .. } = error else { panic!("expected contextual I/O error") };
        assert!(source.raw_os_error().is_some());
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"original");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn object_write_redacts_paths_added_by_tempfile() {
        let root = tempfile::tempdir().unwrap();
        let key = ObjectKey::try_new("missing/chapter.yaml").unwrap();
        let target = root.path().join(key.as_ref());
        let error = atomic_write_bytes_with(&target, b"data", |operation, source| {
            object_io_error(&key, operation, source)
        })
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("create temporary file for object \"missing/chapter.yaml\""));
        assert!(!message.contains(&*root.path().file_name().unwrap().to_string_lossy()));
        let NarouError::ObjectIo { source, .. } = error else { panic!("expected contextual I/O error") };
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        assert!(source.to_string().contains(" at path "));
    }

    #[test]
    fn transfer_checkpoint_survives_reopen_and_rewinds_unreceipted_tail(){
        let root=tempfile::tempdir().unwrap();let store=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        futures::executor::block_on(async{
            store.stage_open("resume","session",7,30).await.unwrap();
            let prefix=store.stage_append("resume","session",7,0,b"abc",false).await.unwrap();
            store.stage_append("resume","session",7,3,b"unreceipted",true).await.unwrap();
            let reopened=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
            let receipt=StageCursor::from_sha_checkpoint(3,prefix.hash_state);
            let cursor=reopened.stage_rewind("resume","session",7,&receipt).await.unwrap();assert_eq!(cursor.confirmed_bytes,3);
            reopened.stage_append("resume","session",7,3,b"def",true).await.unwrap();
            let sha=hex::encode(sha2::Sha256::digest(b"abcdef"));reopened.stage_seal("resume","session",7,6,&sha).await.unwrap();
            let mut stream=reopened.stage_read("resume",2..5).await.unwrap().unwrap();assert_eq!(stream.next().await.unwrap().unwrap(),b"cde");assert!(stream.next().await.is_none());
            assert!(reopened.stage_append("resume","session",7,6,b"g",true).await.is_err());
        });
    }
    #[test]
    fn transfer_close_precedes_drain_and_rejects_stale_writers(){
        let root=tempfile::tempdir().unwrap();let store=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        futures::executor::block_on(async{
            let prefix=ObjectPrefix::new("novels/site/book").unwrap();let key=ObjectKey::try_new("novels/site/book/toc.yaml").unwrap();
            let old=store.capture_writer(&prefix).await.unwrap();store.write_small_guarded(&old,&key,b"old".to_vec()).await.unwrap();
            let reader=store.admit_reader(&prefix,"reader",30).await.unwrap();let revision=store.gate_info(&prefix).await.unwrap().content_revision;
            let epoch=store.close_gate(&prefix,"publication").await.unwrap();assert_eq!(store.gate_info(&prefix).await.unwrap().content_revision,revision);
            assert!(store.admit_reader(&prefix,"late",30).await.is_err());assert!(store.renew_reader(&prefix,&reader,30).await.is_err());
            assert!(store.close_gate(&prefix,"different").await.is_err());assert_eq!(store.close_gate(&prefix,"publication").await.unwrap(),epoch);
            let stage=store.stage_stream(Box::pin(futures::stream::once(async{Ok(b"new".to_vec())}))).await.unwrap();
            assert!(store.commit_staged(&prefix,"publication",epoch,&stage.key,&key).await.is_err());
            store.release_reader(&prefix,&reader).await.unwrap();store.commit_staged(&prefix,"publication",epoch,&stage.key,&key).await.unwrap();store.open_gate(&prefix,"publication",epoch).await.unwrap();
            assert!(store.write_small_guarded(&old,&key,b"late".to_vec()).await.is_err());assert_eq!(store.read_small(&key).await.unwrap().unwrap(),b"new");
        });
    }
    #[test]
    fn renew_reader_classifies_stale_epoch_and_expired_lease(){
        let root=tempfile::tempdir().unwrap();let store=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        futures::executor::block_on(async{
            let prefix=ObjectPrefix::new("novels/site/book").unwrap();
            // close→open bumps the gate epoch: a token minted before it is a
            // gate boundary error (`novel_recovering`), same as SQLite/D1.
            let stale=store.admit_reader(&prefix,"stale",30).await.unwrap();
            let epoch=store.close_gate(&prefix,"transfer").await.unwrap();
            store.open_gate(&prefix,"transfer",epoch).await.unwrap();
            store.admit_reader(&prefix,"other",30).await.unwrap();
            let error=store.renew_reader(&prefix,&stale,30).await.unwrap_err();
            assert!(error.to_string().contains("novel_recovering"),"unexpected: {error}");
            // A missing row at the current epoch is a dead admission
            // (`novel_lease_expired`), never a gate boundary problem.
            let live=store.admit_reader(&prefix,"live",30).await.unwrap();
            store.release_reader(&prefix,&live).await.unwrap();
            let error=store.renew_reader(&prefix,&live,30).await.unwrap_err();
            assert!(error.to_string().contains("novel_lease_expired"),"unexpected: {error}");
        });
    }
    #[test]
    fn transfer_live_os_lock_cannot_be_stolen(){
        let root=tempfile::tempdir().unwrap();let store=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        store.with_transfer_lock("novels/site/book",||{assert!(store.with_transfer_lock("novels/site/book",||Ok(())).is_err());Ok(())}).unwrap();
        store.with_transfer_lock("novels/site/book",||Ok(())).unwrap();
    }
    #[test]
    fn durable_gate_scan_ignores_uncommitted_atomic_files() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let prefix = ObjectPrefix::new("novels/site/retained").unwrap();
        futures::executor::block_on(store.close_gate(&prefix, "publication")).unwrap();
        let path = store.gate_path(&prefix).unwrap();
        let directory = path.parent().unwrap();
        let staged = tempfile::Builder::new().prefix(ATOMIC_OBJECT_TEMP_PREFIX).tempfile_in(directory).unwrap();
        let mut unpublished: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        unpublished["owner"] = serde_json::json!("unpublished");
        let unpublished_bytes = serde_json::to_vec(&unpublished).unwrap();
        for bytes in [b"{".as_slice(), unpublished_bytes.as_slice()] {
            fs::write(staged.path(), bytes).unwrap();
            let gates = durable_fs_gates(directory).unwrap();
            assert_eq!(gates.len(), 1);
            assert_eq!(gates[0].0, prefix.as_ref());
            assert_eq!(gates[0].1.owner.as_deref(), Some("publication"));
            assert_eq!(gates[0].1.epoch, 1);
        }
        atomic_write_bytes(&path, b"{").unwrap();
        assert!(durable_fs_gates(directory).is_err(), "corrupt committed authority must still fail closed");
    }
    #[test]
    fn pending_removals_ignore_uncommitted_atomic_files() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let prefix = ObjectPrefix::new("novels/site/removal").unwrap();
        let owner = af::new_removal_owner(crate::platform::NovelId(1), true).unwrap();
        let epoch = futures::executor::block_on(async {
            let fence = store.capture_writer(&prefix).await?;
            store.retire_writer(&fence, &owner).await
        }).unwrap();
        let path = store.gate_path(&prefix).unwrap();
        let committed = fs::read(&path).unwrap();
        let staged = tempfile::Builder::new().prefix(ATOMIC_OBJECT_TEMP_PREFIX).tempfile_in(path.parent().unwrap()).unwrap();
        for bytes in [b"{".as_slice(), committed.as_slice()] {
            fs::write(staged.path(), bytes).unwrap();
            let (pending, cursor) = futures::executor::block_on(store.pending_removals(None, 100)).unwrap();
            assert!(cursor.is_none());
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].0, prefix);
            assert_eq!(pending[0].1, owner);
            assert_eq!(pending[0].2, epoch);
        }
        atomic_write_bytes(&path, b"{").unwrap();
        assert!(futures::executor::block_on(store.pending_removals(None, 100)).is_err());
    }
    #[test]
    fn windowed_inventory_preserves_atomic_prefix_novel_files() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let key = ObjectKey::try_new(format!("novels/site/user/{ATOMIC_OBJECT_TEMP_PREFIX}note.txt")).unwrap();
        let path = store.resolve_path(&key).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"user").unwrap();
        let private = root.path().join(".objects/metadata");
        fs::create_dir_all(&private).unwrap();
        let staged = tempfile::Builder::new().prefix(ATOMIC_OBJECT_TEMP_PREFIX).tempfile_in(private).unwrap();
        fs::write(staged.path(), b"{").unwrap();
        for prefix in ["novels/site/user", "novels", ""] {
            let request = ObjectListRequest::new(ObjectPrefix::new(prefix).unwrap(), std::num::NonZeroUsize::new(10).unwrap());
            let page = futures::executor::block_on(store.list_page(&request)).unwrap();
            assert!(page.next_cursor.is_none());
            assert_eq!(page.objects.len(), 1);
            assert_eq!(page.objects[0].key, key);
            assert_eq!(page.objects[0].size, 4);
        }
    }
    #[test]
    fn transfer_readers_share_namespace_while_serializing_gate_updates() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let reopened = NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        let first = futures::executor::block_on(store.admit_reader(&prefix, "first", 30)).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        std::thread::scope(|threads| {
            let held_store = &store;
            let held_prefix = &prefix;
            let holder = threads.spawn(move || held_store.with_reader_gate(held_prefix, |gate| {
                assert!(gate.readers.contains_key("first"));
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
                Ok(())
            }));
            entered_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            let digest = hex::encode(sha2::Sha256::digest(prefix.as_ref().as_bytes()));
            let path = store.resolve_path(&ObjectKey::try_new(format!(".narou-transfer/.lock/{digest}")).unwrap()).unwrap();
            let probe = fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
            let shared = fs2::FileExt::try_lock_shared(&probe);
            let publication = store.with_transfer_lock(prefix.as_ref(), || Ok(()));
            let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let reader_store = &reopened;
            let reader_prefix = &prefix;
            let waiter = threads.spawn(move || {
                attempted_tx.send(()).unwrap();
                result_tx.send(futures::executor::block_on(async {
                    let lease = reader_store.admit_reader(reader_prefix, "second", 30).await?;
                    reader_store.renew_reader(reader_prefix, &lease, 30).await
                })).unwrap();
            });
            attempted_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            let pending = result_rx.recv_timeout(std::time::Duration::from_millis(100));
            let waited = matches!(&pending, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
            release_tx.send(()).unwrap();
            let second = pending.unwrap_or_else(|error| {
                assert!(matches!(error, std::sync::mpsc::RecvTimeoutError::Timeout));
                result_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap()
            });
            waiter.join().unwrap();
            let held = holder.join().unwrap();
            if shared.is_ok() { fs2::FileExt::unlock(&probe).unwrap(); }
            held.unwrap();
            shared.unwrap();
            assert!(waited, "a concurrent reader must wait for the gate metadata update");
            assert!(publication.is_err(), "readers must still exclude publication");
            let second = second.unwrap();
            assert_eq!(first.epoch, second.epoch);
            futures::executor::block_on(async {
                assert_eq!(store.gate_info(&prefix).await.unwrap().pending_readers, 2);
                store.release_reader(&prefix, &first).await.unwrap();
                reopened.release_reader(&prefix, &second).await.unwrap();
                assert_eq!(store.gate_info(&prefix).await.unwrap().pending_readers, 0);
                store.close_gate(&prefix, "publication").await.unwrap();
                assert!(reopened.admit_reader(&prefix, "late", 30).await.is_err());
            });
        });
    }
    use crate::platform::ObjectStore;

    #[test]
    fn streamed_files_roundtrip_without_a_tokio_runtime() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        futures::executor::block_on(async {
            let size = crate::platform::staging::ASSET_CHUNK_BYTES;
            let source = futures::stream::iter([Ok(vec![3; size]), Ok(vec![5; 19])]);
            let stage = store.stage_stream(Box::pin(source)).await.unwrap();
            assert_eq!(stage.size, size as u64 + 19);
            let bytes = store.read_range(&stage.key, size as u64 - 3..size as u64 + 8).await.unwrap().unwrap();
            assert_eq!(bytes, [vec![3; 3], vec![5; 8]].concat());
            let mut stream = store.read_stream(&stage.key).await.unwrap().unwrap();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                assert!(chunk.len() <= size);
                bytes.extend_from_slice(&chunk);
            }
            assert_eq!(&bytes[..size], vec![3; size]);
            assert_eq!(&bytes[size..], &[5; 19]);
            AssetStore::delete(&store, &stage.key).await.unwrap();
        });
    }

    #[test]
    fn active_stage_read_holds_cleanup_lease_until_drop() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let stage = store.stage_stream(Box::pin(futures::stream::once(async { Ok(vec![9; 1024]) }))).await.unwrap();
            let stream = store.read_stream(&stage.key).await.unwrap().unwrap();
            let path = store.resolve_path(&stage.key).unwrap();
            let expired = std::time::SystemTime::now() - std::time::Duration::from_secs(
                crate::platform::staging::STAGE_TTL_SECONDS as u64 * 2);
            fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(expired).unwrap();
            collect_expired_stage_files(root.path()).unwrap();
            assert!(path.exists(), "a live reader must retain its stage beyond creation TTL");
            drop(stream);
            collect_expired_stage_files(root.path()).unwrap();
            assert!(!path.exists(), "dropping the reader releases the cleanup lease");
        });
    }

    #[test]
    fn relative_novel_dir_becomes_absolute_for_materialization() {
        let relative = Path::new("小説データ/site/novel");
        let absolute = absolute_path(relative).unwrap();
        assert!(absolute.is_absolute());
        assert_eq!(absolute.file_name().and_then(|name| name.to_str()), Some("novel"));
    }

    #[test]
    fn relative_novel_dir_matches_the_absolute_archive_root() {
        let cwd = std::env::current_dir().unwrap();
        let root = cwd.join("小説データ");
        let novel_dir = absolute_path(Path::new("小説データ/site/novel")).unwrap();
        let key = logical_key_for_native_path(&root, &novel_dir).expect("prefix must resolve");
        assert_eq!(key.as_ref(), "novels/site/novel");
    }

    #[test]
    fn native_store_maps_logical_novel_keys_to_existing_layout() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let key = keys.toc();
        futures::executor::block_on(store.write_small(&key, b"title: test\n".to_vec())).unwrap();
        assert!(
            root.path()
                .join("site")
                .join("12")
                .join("n1234ab")
                .join("toc.yaml")
                .exists()
        );
        assert_eq!(
            futures::executor::block_on(store.read_small(&key))
                .unwrap()
                .unwrap(),
            b"title: test\n"
        );
    }

    #[test]
    fn native_store_rejects_escape_keys() {
        let root = tempfile::tempdir().unwrap();
        let _store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        assert!(ObjectKey::try_new("novels/../escape").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn failed_cache_move_removes_only_its_new_empty_version_directory() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "title", false).unwrap();
        let source = keys.section("1", "locked");
        futures::executor::block_on(store.write_small(&source, b"old body".to_vec())).unwrap();
        let source_path = store.resolve_path(&source).unwrap();
        for (version, existed) in [("new", false), ("legacy-empty", true)] {
            let destination = keys.cached_section(version, "1", "locked");
            let destination_path = store.resolve_path(&destination).unwrap();
            let version_dir = destination_path.parent().unwrap();
            if existed {
                fs::create_dir_all(version_dir).unwrap();
            }
            let locked = fs::OpenOptions::new().read(true).share_mode(0).open(&source_path).unwrap();
            let result = futures::executor::block_on(AssetStore::move_or_copy(&store, &source, &destination));
            drop(locked);
            assert!(result.is_err(), "the real rename and copy must fail on the exclusive source handle");
            assert_eq!(version_dir.exists(), existed, "only the newly owned empty directory is removable");
            assert_eq!(fs::read(&source_path).unwrap(), b"old body");
            assert!(!store.resolve_path(&keys.cached_version_marker(version)).unwrap().exists());
        }
    }

    /// `mirror-files=false` の有効な作品はフォルダが無くても「保存済み」と
    /// 判定できること (download/backup の誤削除防止)。
    #[test]
    fn novel_content_stored_reads_sqlite_objects_without_mirror() {
        use crate::native::sqlite::object_store::SqliteObjectStore;
        use crate::native::sqlite::state;

        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        state::write_mode(&narou_dir, state::StorageMode::Sqlite).unwrap();
        let archive_root = root
            .path()
            .join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let novel_dir = archive_root.join("site").join("title");

        let handle = state::active_for(&narou_dir).expect("sqlite state");
        let mirror = NativeObjectStore::from_root(archive_root.clone()).unwrap();
        let store = SqliteObjectStore::with_mirror_files(handle.conn_ref().clone(), mirror, false);
        let prefix = novel_object_prefix(&archive_root, &novel_dir).unwrap();
        futures::executor::block_on(store.write_small(
            &prefix.join("toc.yaml").unwrap(),
            b"title: test\n".to_vec(),
        ))
        .unwrap();

        assert!(!novel_dir.exists(), "mirror disabled must not create files");
        assert!(novel_content_stored(&archive_root, &novel_dir).unwrap());
        assert!(!novel_content_stored(
            &archive_root,
            &archive_root.join("site").join("missing")
        ).unwrap());
    }

    #[test]
    fn native_store_preserves_all_novel_object_layouts() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let subtitle = "第一話";
        let section = keys.section("1", subtitle);
        let raw = keys.raw_section("1", subtitle);
        let setting = keys.setting();
        let replace = keys.replace();
        let cache = keys.illustration_cache();
        let illustration = keys.illustration("abc.png").unwrap();

        futures::executor::block_on(async {
            for (key, value) in [
                (keys.toc(), b"toc".as_slice()),
                (section.clone(), b"section".as_slice()),
                (raw.clone(), b"raw".as_slice()),
                (setting.clone(), b"setting".as_slice()),
                (replace.clone(), b"replace".as_slice()),
                (cache.clone(), b"cache".as_slice()),
                (illustration.clone(), b"png".as_slice()),
            ] {
                store.write_small(&key, value.to_vec()).await.unwrap();
            }
            let prefix = crate::platform::ObjectPrefix::new(keys.prefix().to_string()).unwrap();
            let page = store
                .list_page(&crate::platform::ObjectListRequest::new(
                    prefix,
                    std::num::NonZeroUsize::new(3).unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(page.objects.len(), 3);
            assert!(page.next_cursor.is_some());
        });

        let novel_dir = root.path().join("site").join("12").join("n1234ab");
        assert!(novel_dir.join("toc.yaml").is_file());
        assert!(novel_dir.join("本文").join("1 第一話.yaml").is_file());
        assert!(novel_dir.join("raw").join("1 第一話.html").is_file());
        assert!(novel_dir.join("setting.ini").is_file());
        assert!(novel_dir.join("replace.txt").is_file());
        assert!(novel_dir.join(".illustration_cache.yaml").is_file());
        assert!(novel_dir.join("挿絵").join("abc.png").is_file());
    }

    #[test]
    fn native_store_lists_non_novel_logical_objects() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let key = ObjectKey::try_new("generated/epub/book.epub").unwrap();
        futures::executor::block_on(store.write_small(&key, b"epub".to_vec())).unwrap();
        let prefix = crate::platform::ObjectPrefix::new("generated").unwrap();
        let page =
            futures::executor::block_on(store.list_page(&crate::platform::ObjectListRequest::new(
                prefix,
                std::num::NonZeroUsize::new(10).unwrap(),
            )))
            .unwrap();
        assert_eq!(page.objects[0].key, key);
        assert_eq!(
            std::fs::read(
                root.path()
                    .join(".objects")
                    .join("generated")
                    .join("epub")
                    .join("book.epub")
            )
            .unwrap(),
            b"epub"
        );
    }

    #[test]
    fn native_store_cursor_skips_deleted_cursor_object() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let first = ObjectKey::try_new("generated/a.bin").unwrap();
        let second = ObjectKey::try_new("generated/b.bin").unwrap();
        let third = ObjectKey::try_new("generated/c.bin").unwrap();
        futures::executor::block_on(async {
            for key in [&first, &second, &third] {
                store.write_small(key, b"data".to_vec()).await.unwrap();
            }
            let prefix = crate::platform::ObjectPrefix::new("generated").unwrap();
            let page = store
                .list_page(&crate::platform::ObjectListRequest::new(
                    prefix.clone(),
                    std::num::NonZeroUsize::new(1).unwrap(),
                ))
                .await
                .unwrap();
            let cursor = page.next_cursor.clone().unwrap();
            assert_eq!(page.objects[0].key, first);
            ObjectStore::delete(&store, &first).await.unwrap();
            let page = store
                .list_page(
                    &crate::platform::ObjectListRequest::new(
                        prefix,
                        std::num::NonZeroUsize::new(1).unwrap(),
                    )
                    .after(cursor),
                )
                .await
                .unwrap();
            assert_eq!(page.objects[0].key, second);
        });
    }

    #[test]
    fn native_store_loads_existing_layout_without_migration() {
        let root = tempfile::tempdir().unwrap();
        let novel_dir = root.path().join("site").join("12").join("n1234ab");
        std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
        std::fs::write(novel_dir.join("本文").join("1 旧題.yaml"), b"legacy").unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let subtitle = crate::downloader::SubtitleInfo {
            index: "1".to_string(),
            href: String::new(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "新題".to_string(),
            file_subtitle: "新題".to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        };
        let loaded = futures::executor::block_on(
            store.read_small(&keys.section(&subtitle.index, &subtitle.file_subtitle)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded, b"legacy");
    }

    #[cfg(windows)]
    #[test]
    fn native_store_lists_through_a_case_aliased_root() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("ArchiveRoot")).unwrap();
        let root = temp.path().join("archiveroot");
        let store = NativeObjectStore::from_root(root.clone()).unwrap();
        let key = ObjectKey::try_new("webnovel/example.yaml").unwrap();
        futures::executor::block_on(async {
            store.write_small(&key, b"name: custom".to_vec()).await.unwrap();
            let request = ObjectListRequest::new(
                crate::platform::ObjectPrefix::new("webnovel").unwrap(),
                std::num::NonZeroUsize::new(10).unwrap(),
            );
            let page = store.list_page(&request).await.unwrap();
            assert_eq!(page.objects.iter().map(|object| &object.key).collect::<Vec<_>>(), [&key]);
            let physical = store.resolve_path(&key).unwrap();
            assert_eq!(logical_key_for_native_path(&root, &physical), Some(key.clone()));
            let configured_path = root.join(".objects/webnovel/example.yaml");
            assert_eq!(logical_key_for_native_path(store.root(), &configured_path), Some(key.clone()));
            assert_eq!(store.read_small(&key).await.unwrap().unwrap(), b"name: custom");
        });
    }

    #[test]
    fn missing_section_parent_is_a_cache_miss() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let section = keys.section("1", "第一話");

        let result = futures::executor::block_on(store.read_small(&section)).unwrap();
        assert!(result.is_none());
    }

    /// Objects whose novel directory carries the historical `‛`-escaped
    /// spelling must stay reachable through the canonical logical key:
    /// writes of *new* sections land inside the legacy directory (never in a
    /// parallel canonical directory), reads find them, and listings emit
    /// keys under the caller's logical prefix.
    #[test]
    fn native_store_reads_writes_and_lists_inside_escaped_novel_dir() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let file_title = "n6247ex 旧題：異世界";
        let keys = crate::platform::NovelObjectKeys::new("site", file_title, false).unwrap();
        let legacy_dir = root
            .path()
            .join("site")
            .join("n6247ex 旧題\u{201b}：異世界");
        std::fs::create_dir_all(legacy_dir.join("本文")).unwrap();
        std::fs::write(legacy_dir.join("toc.yaml"), b"title: legacy\n").unwrap();
        std::fs::write(legacy_dir.join("本文").join("1 第一話.yaml"), b"section one").unwrap();

        futures::executor::block_on(async {
            // Read: the logical key reaches the escaped physical file.
            assert_eq!(
                store.read_small(&keys.toc()).await.unwrap().unwrap(),
                b"title: legacy\n"
            );
            assert_eq!(
                store
                    .read_small(&keys.section("1", "第一話"))
                    .await
                    .unwrap()
                    .unwrap(),
                b"section one"
            );

            // Write: a new section is created inside the legacy directory
            // with its canonical filename; no parallel directory appears.
            store
                .write_small(&keys.section("2", "第二話"), b"section two".to_vec())
                .await
                .unwrap();
        });
        assert_eq!(
            std::fs::read(legacy_dir.join("本文").join("2 第二話.yaml")).unwrap(),
            b"section two"
        );
        assert!(!root.path().join("site").join("n6247ex 旧題：異世界").exists());

        // List: entries are emitted under the caller's logical prefix even
        // though the physical directory name differs.
        let listed = store.list_direct_files(keys.prefix()).unwrap();
        assert!(
            listed.iter().any(|key| key == &keys.toc()),
            "{listed:?} must contain {}",
            keys.toc().as_ref()
        );

        futures::executor::block_on(async {
            let prefix =
                crate::platform::ObjectPrefix::new(keys.prefix().to_string()).unwrap();
            let page = store
                .list_page(&ObjectListRequest::new(
                    prefix,
                    std::num::NonZeroUsize::new(10).unwrap(),
                ))
                .await
                .unwrap();
            let listed: Vec<_> = page.objects.iter().map(|object| object.key.as_ref()).collect();
            assert!(listed.contains(&keys.toc().as_ref()), "{listed:?}");
            assert!(
                listed.contains(&keys.section("2", "第二話").as_ref()),
                "{listed:?}"
            );
            assert!(
                listed.iter().all(|key| key.starts_with("novels/")),
                "{listed:?}"
            );
        });

        // Snapshot callers that build keys from the physical escaped
        // directory name keep working verbatim.
        let physical_prefix = crate::platform::ObjectPrefix::new(
            "novels/site/n6247ex 旧題\u{201b}：異世界",
        )
        .unwrap();
        let page = futures::executor::block_on(store.list_page(&ObjectListRequest::new(
            physical_prefix,
            std::num::NonZeroUsize::new(10).unwrap(),
        )))
        .unwrap();
        assert!(
            page.objects
                .iter()
                .any(|object| object.key.as_ref().ends_with("toc.yaml")),
            "{:?}",
            page.objects
        );
    }

    /// When both spellings exist the canonical directory wins; the escaped
    /// one is left untouched.
    #[test]
    fn native_store_prefers_canonical_dir_over_escaped() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let file_title = "n0001ab 旧題：話";
        let keys = crate::platform::NovelObjectKeys::new("site", file_title, false).unwrap();
        std::fs::create_dir_all(root.path().join("site").join(file_title)).unwrap();
        let legacy_dir = root
            .path()
            .join("site")
            .join("n0001ab 旧題\u{201b}：話");
        std::fs::create_dir_all(&legacy_dir).unwrap();

        futures::executor::block_on(
            store.write_small(&keys.toc(), b"title: canonical\n".to_vec()),
        )
        .unwrap();
        assert!(root
            .path()
            .join("site")
            .join(file_title)
            .join("toc.yaml")
            .is_file());
        assert!(!legacy_dir.join("toc.yaml").exists());
    }

    /// A literal `‛` inside the logical name is itself escaped (`‛‛`), so a
    /// name containing the marker resolves to its own directory rather than
    /// aliasing a marked sibling.
    #[test]
    fn native_store_doubles_literal_marker_character() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys =
            crate::platform::NovelObjectKeys::new("site", "n2 t\u{201b}：x", false).unwrap();
        let legacy_dir = root
            .path()
            .join("site")
            .join("n2 t\u{201b}\u{201b}\u{201b}：x");
        std::fs::create_dir_all(&legacy_dir).unwrap();

        futures::executor::block_on(
            store.write_small(&keys.toc(), b"title: marker\n".to_vec()),
        )
        .unwrap();
        assert!(legacy_dir.join("toc.yaml").is_file());
        assert!(!root.path().join("site").join("n2 t：x").exists());
    }
    #[test]
    fn visit_namespace_gates_suppresses_adopted_side_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let mirror = NativeObjectStore::from_root(root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR)).unwrap();
        let store = crate::native::sqlite::object_store::SqliteObjectStore::new(conn.clone(), mirror.clone());
        let prefix = ObjectPrefix::new("novels/site/visit/").unwrap();
        futures::executor::block_on(async {
            let mut seen = Vec::new();
            mirror.rebind_writer(&prefix, 0).await.unwrap();
            gate_origin::bind_fs_gate_origin_locked(root.path(), &prefix, "https://example.test/").unwrap();
            // Baseline: the open FS gate emits its own tuple.
            visit_namespace_gates(root.path(), &conn.lock(), |p, closed, _, epoch, _| { seen.push((p.to_owned(), closed, epoch)); Ok(()) }).unwrap();
            assert_eq!(seen, vec![("novels/site/visit".to_string(), false, 0u64)]);
            // FS→SQL: the migrated FS gate is suppressed; the SQL row emits.
            mirror.adopt_fs_gate_locked(&prefix, &conn.lock()).unwrap();
            seen.clear();
            visit_namespace_gates(root.path(), &conn.lock(), |p, closed, _, epoch, _| { seen.push((p.to_owned(), closed, epoch)); Ok(()) }).unwrap();
            assert_eq!(seen, vec![("novels/site/visit".to_string(), false, 1u64)]);
            // SQL→FS: the deleted row emits nothing; the live FS gate emits.
            store.adopt_sql_gate_locked(&prefix).unwrap();
            seen.clear();
            visit_namespace_gates(root.path(), &conn.lock(), |p, closed, _, epoch, _| { seen.push((p.to_owned(), closed, epoch)); Ok(()) }).unwrap();
            assert_eq!(seen, vec![("novels/site/visit".to_string(), false, 2u64)]);
        });
    }

    /// issue #58: the scan cache must survive a content-only gate write
    /// (content_revision is not scan-relevant) and must be invalidated by a
    /// scan-relevant change (a closed namespace).
    #[test]
    fn gate_scan_cache_ignores_content_writes_and_follows_authority_changes() {
        let root = tempfile::tempdir().unwrap();
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let archive = root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let mirror = NativeObjectStore::from_root(archive.clone()).unwrap();
        let prefix = ObjectPrefix::new("novels/site/cached/").unwrap();
        futures::executor::block_on(async {
            mirror.rebind_writer(&prefix, 0).await.unwrap();
            gate_origin::bind_fs_gate_origin_locked(root.path(), &prefix, "https://example.test/").unwrap();
            let mut seen = Vec::new();
            visit_namespace_gates(root.path(), &conn.lock(), |p, closed, _, epoch, _| { seen.push((p.to_owned(), closed, epoch)); Ok(()) }).unwrap();
            assert_eq!(seen, vec![("novels/site/cached".to_string(), false, 0u64)]);
            // The first scan populated the cache under this revision.
            let revision = read_gate_revision(&archive).unwrap();
            assert_eq!(revision % 2, 0, "a published revision must be even");

            // A content-only write (content_revision) keeps the revision: the
            // scan cache stays valid, so the scan costs one small file read.
            let keys = crate::platform::NovelObjectKeys::new("site", "cached", false).unwrap();
            mirror.write_small_guarded(&crate::platform::WriteFence::new(prefix.clone(), 0), &keys.toc(), b"title: x\n".to_vec()).await.unwrap();
            assert_eq!(read_gate_revision(&archive).unwrap(), revision, "a content-only gate write must not invalidate the scan");

            // A closed namespace is scan-relevant: the revision advances and
            // the next scan must observe the closed gate.
            mirror.close_gate(&prefix, "remove-v1:1:1").await.unwrap();
            assert!(read_gate_revision(&archive).unwrap() > revision, "closing a namespace must invalidate the scan");
            seen.clear();
            visit_namespace_gates(root.path(), &conn.lock(), |p, closed, _, epoch, _| { seen.push((p.to_owned(), closed, epoch)); Ok(()) }).unwrap();
            assert_eq!(seen, vec![("novels/site/cached".to_string(), true, 1u64)]);
        });
    }

    /// issue #58: an odd (in-flight or crashed) revision must never serve a
    /// cached scan, and a crashed writer must be sealed by the next writer.
    #[test]
    fn gate_scan_cache_refuses_odd_revision_and_seals_a_crashed_writer() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let mirror = NativeObjectStore::from_root(archive.clone()).unwrap();
        let prefix = ObjectPrefix::new("novels/site/odd/").unwrap();
        futures::executor::block_on(async {
            mirror.rebind_writer(&prefix, 0).await.unwrap();
            gate_origin::bind_fs_gate_origin_locked(root.path(), &prefix, "https://example.test/").unwrap();
            let conn = crate::native::sqlite::open_in_memory().unwrap();
            visit_namespace_gates(root.path(), &conn.lock(), |_, _, _, _, _| { Ok(()) }).unwrap();
            // Simulate a crashed writer: the seqlock stays odd.
            write_gate_revision(&archive, 7).unwrap();
            assert_eq!(read_gate_revision(&archive).unwrap(), 7);
            // A write that changes nothing scan-relevant does not touch the
            // counter: the odd value only disables the cache, it never
            // breaks correctness.
            mirror.rebind_writer(&prefix, 0).await.unwrap();
            assert_eq!(read_gate_revision(&archive).unwrap(), 7, "a no-op gate write must not disturb the counter");
            // The next scan-relevant mutation seals the crashed predecessor
            // first and then publishes an even value again.
            mirror.close_gate(&prefix, "remove-v1:1:1").await.unwrap();
            let revision = read_gate_revision(&archive).unwrap();
            assert_eq!(revision % 2, 0, "a writer must seal a crashed predecessor");
            assert!(revision > 7, "the sealed revision must advance past the crashed value");
        });
    }

    #[test]
    fn cutover_lock_excludes_concurrent_transfer_lock() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let busy = store
            .with_transfer_lock("novels/site/book", || store.with_cutover_lock(|| Ok(())))
            .unwrap_err();
        assert!(busy.to_string().contains("novel_busy:"), "{busy}");

        let busy_reverse = store
            .with_cutover_lock(|| store.with_transfer_lock("novels/site/book", || Ok(())))
            .unwrap_err();
        assert!(busy_reverse.to_string().contains("novel_busy:"), "{busy_reverse}");
    }

    // issue #50: an established SQLite handle whose `StateDb.conn` mutex is
    // held by a parallel update (or re-entrantly by the caller) must not
    // turn the FS gate authority consult into a fabricated `Conflict`. The
    // consult falls back to a dedicated read of committed state.
    fn issue50_sqlite_library(root: &Path) -> crate::native::sqlite::state::StateDb {
        let narou_dir = root.join(".narou");
        crate::native::sqlite::state::write_mode(
            &narou_dir,
            crate::native::sqlite::state::StorageMode::Sqlite,
        )
        .unwrap();
        crate::native::sqlite::state::active_for(&narou_dir).expect("sqlite state is active")
    }

    #[test]
    fn fs_gate_admission_survives_contended_state_connection() {
        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        let state = issue50_sqlite_library(root.path());
        let archive = root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let store = NativeObjectStore::from_root(archive).unwrap();
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        // A parallel update's transaction holds the shared SQLite mutex on
        // another thread AND an open `BEGIN IMMEDIATE` write transaction;
        // every FS-side gate admission must still read the committed
        // authority through a dedicated WAL snapshot connection.
        let held_conn = state.conn_ref().clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let held = held_conn.lock();
            held.execute_batch("BEGIN IMMEDIATE; INSERT OR REPLACE INTO app_state(scope, key, value_json, value_yaml) VALUES('meta', 'issue50_probe', '{}', '1');").unwrap();
            locked_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            held.execute_batch("COMMIT;").unwrap();
        });
        locked_rx.recv().unwrap();
        assert!(matches!(
            live_sql_gate_authority(&narou_dir, &prefix, 0, "").unwrap(),
            SqlGateAuthority::Absent
        ));
        let guard = futures::executor::block_on(store.gate_info(&prefix)).unwrap();
        assert_eq!(guard.epoch, 0);
        let lease =
            futures::executor::block_on(store.admit_reader(&prefix, "reader", 30)).unwrap();
        store
            .with_gate(&prefix, |gate| {
                gate.updated_at = ts::now_secs();
                Ok(())
            })
            .unwrap();
        let objects: Arc<dyn ObjectStore> = Arc::new(store.clone());
        let write = crate::native::converter::run_store_io(
            crate::application::novel_access::NovelWriteGuard::capture(
                &objects,
                None,
                &crate::platform::ObjectPrefix::new("novels").unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(write.prefix().as_ref(), "novels");
        done_tx.send(()).unwrap();
        holder.join().unwrap();
        // `release_reader` drains the matching SQL lease row through the shared
        // handle, so it legitimately waits for the other worker's transaction.
        futures::executor::block_on(store.release_reader(&prefix, &lease)).unwrap();
    }

    #[test]
    fn fs_gate_admission_keeps_fail_closed_under_contention() {
        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        let state = issue50_sqlite_library(root.path());
        let archive = root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let store = NativeObjectStore::from_root(archive).unwrap();
        let conflicted = ObjectPrefix::new("novels/site/conflicted").unwrap();
        let superseded = ObjectPrefix::new("novels/site/superseded").unwrap();
        // Plant the durable gate tuple the FS side owns for `superseded`:
        // closing the gate writes epoch 1 / empty origin.
        let gate_epoch =
            futures::executor::block_on(store.close_gate(&superseded, "publication")).unwrap();
        {
            let conn = state.conn_ref().lock();
            conn.execute(
                "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES(?1,1,0,NULL,0,0,'')",
                [conflicted.as_ref()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO native_gate_handoffs(prefix,fs_epoch,sql_epoch,fs_origin_fold,recorded_at) VALUES(?1,?2,?3,'',0)",
                rusqlite::params![superseded.as_ref(), i64::try_from(gate_epoch).unwrap(), i64::try_from(gate_epoch + 1).unwrap()],
            )
            .unwrap();
            assert!(matches!(
                live_sql_gate_authority(&narou_dir, &conflicted, 0, "").unwrap(),
                SqlGateAuthority::Conflict
            ));
            assert!(matches!(
                live_sql_gate_authority(&narou_dir, &superseded, gate_epoch, "").unwrap(),
                SqlGateAuthority::Superseded
            ));
            let busy = futures::executor::block_on(store.admit_reader(&conflicted, "reader", 30))
                .unwrap_err();
            assert!(
                busy.to_string().contains("conflicting live SQL authority"),
                "{busy}"
            );
            let healed = futures::executor::block_on(store.admit_reader(&superseded, "reader", 30))
                .unwrap_err();
            assert!(healed.to_string().contains("novel_recovering"), "{healed}");
            let gate_path = store.gate_path(&superseded).unwrap();
            let gate: FsGate = serde_json::from_slice(&fs::read(gate_path).unwrap()).unwrap();
            assert!(gate.migrated, "the unflushed migrated marker must be repaired");
        }
    }

    #[test]
    fn private_conversion_store_never_consults_library_authority() {
        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let state = issue50_sqlite_library(root.path());
        let store =
            NativeObjectStore::from_private_root(root.path().join(".narou-snapshot-test")).unwrap();
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        let held = state.conn_ref().lock();
        // Live authority rows for BOTH consulted prefixes in the parent
        // library: any consult would see `Conflict` rows, so `Absent` and a
        // successful capture prove the private store never reads this DB.
        held.execute(
            "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES(?1,1,0,NULL,0,0,'')",
            [prefix.as_ref()],
        )
        .unwrap();
        held.execute(
            "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES('novels',1,0,NULL,0,0,'')",
            [],
        )
        .unwrap();
        assert!(matches!(
            store.live_sql_authority(&prefix, 0, "").unwrap(),
            SqlGateAuthority::Absent
        ));
        let objects: Arc<dyn ObjectStore> = Arc::new(store);
        let write = crate::native::converter::run_store_io(
            crate::application::novel_access::NovelWriteGuard::capture(
                &objects,
                None,
                &crate::platform::ObjectPrefix::new("novels").unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(write.prefix().as_ref(), "novels");
        drop(held);
    }

    #[test]
    fn private_conversion_workspace_captures_while_connection_is_held() {
        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let state = issue50_sqlite_library(root.path());
        let private = tempfile::Builder::new()
            .prefix(".narou-snapshot-")
            .tempdir_in(root.path())
            .unwrap();
        let held = state.conn_ref().lock();
        let workspace = crate::native::converter::ConversionWorkspace::begin(
            private.path(),
            0,
            None,
            crate::native::materialization::ConversionInputSelection {
                illustrations: false,
                cover: false,
            },
        )
        .unwrap();
        assert_eq!(workspace.private_write.prefix().as_ref(), "novels");
        crate::native::converter::private_capabilities(private.path()).unwrap();
        drop(held);
    }

    #[test]
    fn gate_consult_never_creates_the_database_file() {
        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        crate::native::sqlite::state::write_mode(
            &narou_dir,
            crate::native::sqlite::state::StorageMode::Sqlite,
        )
        .unwrap();
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        assert!(matches!(
            live_sql_gate_authority(&narou_dir, &prefix, 0, "").unwrap(),
            SqlGateAuthority::Absent
        ));
        assert!(!narou_dir.join(crate::native::sqlite::state::DB_FILE_NAME).exists());
    }

    #[test]
    fn legacy_yaml_env_bypasses_sqlite_gate_authority_consult() {
        let _guard = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let state = issue50_sqlite_library(root.path());
        let narou_dir = root.path().join(".narou");
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        state
            .conn_ref()
            .lock()
            .execute(
                "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES(?1,1,0,NULL,0,0,'')",
                [prefix.as_ref()],
            )
            .unwrap();
        assert!(matches!(
            live_sql_gate_authority(&narou_dir, &prefix, 0, "").unwrap(),
            SqlGateAuthority::Conflict
        ));
        let prev = std::env::var_os("NAROU_RS_LEGACY_YAML");
        unsafe { std::env::set_var("NAROU_RS_LEGACY_YAML", "1"); }
        let outcome = live_sql_gate_authority(&narou_dir, &prefix, 0, "");
        match prev {
            Some(value) => unsafe { std::env::set_var("NAROU_RS_LEGACY_YAML", value); },
            None => unsafe { std::env::remove_var("NAROU_RS_LEGACY_YAML"); },
        }
        assert!(matches!(outcome.unwrap(), SqlGateAuthority::Absent));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_files_rejects_symlink_cycle() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("site/book")).unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("site/book/loop")).unwrap();
        let mut files = Vec::new();
        let err = NativeObjectStore::recursive_files(root.path(), root.path(), &mut files).unwrap_err();
        assert!(err.to_string().contains("cyclic native namespace link"), "{err}");
    }
}

