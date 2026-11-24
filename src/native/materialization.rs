use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};

/// Drive native store I/O from CLI or a blocking Web handler without nesting
/// Tokio runtimes on the current thread.
pub fn block_on<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            std::thread::Builder::new().spawn_scoped(scope, move || block_on(future))
                .map_err(|error| NarouError::Platform(error.to_string()))?
                .join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    } else {
        tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|error| NarouError::Platform(error.to_string()))?
            .block_on(future)
    }
}

/// Capture at the start of a native logical operation, before any fetch or
/// private rendering. Helpers downstream receive this token; they do not renew it.
pub fn capture_write_for_record(record: &crate::db::NovelRecord) -> Result<crate::application::novel_access::NovelWriteGuard> {
    let stores = crate::native::object_store::NativeStores::for_current_root()?;
    let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
    let write = block_on(crate::application::novel_access::NovelWriteGuard::capture_for(&stores.objects, Some(&stores.assets), &keys))?;
    stores.primary.with_open_write(write.fence(), || {
        let current = crate::native::novel_repository::NativeNovelRepository::new().get_current_sync(record.id.into())?
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", record.id)))?;
        if &current != record { return Err(NarouError::Platform("plan_stale: novel metadata changed before operation admission".into())); }
        crate::native::transfer::assert_source_namespace(&current)
    })?;
    Ok(write)
}

pub fn capture_write_for_id(id: i64) -> Result<crate::application::novel_access::NovelWriteGuard> {
    let record = crate::native::novel_repository::NativeNovelRepository::new().get_current_sync(id.into())?
        .ok_or_else(|| NarouError::NotFound(format!("ID: {id}")))?;
    capture_write_for_record(&record)
}

pub fn directory_for_origin(write: &crate::application::novel_access::NovelWriteGuard) -> Result<PathBuf> {
    let primary = crate::native::object_store::NativeStore::for_current_root()?;
    primary.with_open_write(write.fence(), || primary.novel_directory(write.prefix()))
}

/// Compose canonical local inputs with remote images behind one primary gate.
pub fn read_store(root: &Path) -> Result<Arc<dyn crate::platform::ObjectStore>> {
    let stores = crate::native::object_store::NativeStores::for_narou_root(root)?;
    Ok(read_store_with(&stores))
}

/// Rebind the read facade onto already-resolved stores without opening a
/// second connection set. Canonical keys (toc, body, settings, outputs) stay
/// on the caller's primary; only illustration keys route to the caller's
/// alias-aware remote pair, never to bare S3.
pub(crate) fn read_store_with(stores: &crate::native::object_store::NativeStores) -> Arc<dyn crate::platform::ObjectStore> {
    let primary_objects: Arc<dyn crate::platform::ObjectStore> = stores.primary.clone();
    if stores.s3.is_none() {
        return primary_objects;
    }
    let primary_assets: Arc<dyn crate::platform::AssetStore> = stores.primary.clone();
    Arc::new(crate::platform::split_store::SplitStore::new(
        (primary_objects, primary_assets),
        (stores.objects.clone(), stores.assets.clone()),
    ))
}

/// Copy canonical inputs with the caller's original operation namespace.
pub fn snapshot_with_origin(novel_dir: &Path, write: &crate::application::novel_access::NovelWriteGuard) -> Result<tempfile::TempDir> {
    snapshot_with_origin_excluding(novel_dir, write, None)
}

/// [`snapshot_with_origin`] that skips every canonical object whose
/// novel-relative logical path sits inside `excluded_relative`. The caller
/// owns exclusion (e.g. a managed output destination); source user files and
/// other generated outputs are still copied.
pub fn snapshot_with_origin_excluding(novel_dir: &Path, write: &crate::application::novel_access::NovelWriteGuard, excluded_relative: Option<&Path>) -> Result<tempfile::TempDir> {
    block_on(async {
        let inventory = crate::db::inventory::Inventory::with_default_root()?;
        validate_origin_directory(novel_dir, write)?;
        let objects = read_store(inventory.root_dir())?;
        let mut lease = crate::application::novel_access::NovelReadLease::admit(
            &objects, write.prefix(), "native-snapshot",
        ).await?;
        let result = async {
            if write.epoch() != lease.epoch() {
                return Err(NarouError::Platform("novel_busy: snapshot origin changed".into()));
            }
            snapshot_leased_excluding(&objects, &mut lease, inventory.root_dir(), excluded_relative).await
        }.await;
        let released = lease.release().await;
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    })
}

/// Validate current ownership from the original admitted prefix. This
/// resolves metadata only; it never captures or substitutes an epoch.
async fn validate_leased_source(lease: &mut crate::application::novel_access::NovelReadLease) -> Result<()> {
    lease.validate().await?;
    let prefix = crate::platform::ObjectKey::try_new(lease.prefix().as_ref().trim_end_matches('/'))?;
    match crate::native::novel_repository::NativeNovelRepository::new().find_by_storage_prefix_current_sync(&prefix)? {
        Some(record) => crate::native::transfer::assert_source_namespace(&record)?,
        None => crate::native::transfer::assert_retained_source_namespace(lease.prefix())?,
    }
    lease.validate().await
}

/// Materialize only the novel-root canonical files `select` accepts, keeping
/// every other object (body sections, illustrations, nested state) on the
/// store. The workspace keeps each copied file's store modification time so
/// freshness consumers compare the object's timestamp, not the copy's.
/// Returned names are the logical file names that were actually copied.
pub(crate) async fn selected_files_snapshot_leased(
    objects: &Arc<dyn crate::platform::ObjectStore>,
    lease: &mut crate::application::novel_access::NovelReadLease,
    library_root: &Path,
    select: impl Fn(&str) -> bool + Send,
) -> Result<(tempfile::TempDir, std::collections::BTreeSet<String>)> {
    validate_leased_source(lease).await?;
    let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("snapshot reader fence unavailable".into()))?;
    let before = fence.gate_info(lease.prefix()).await?;
    let temporary = tempfile::Builder::new().prefix(".narou-snapshot-").tempdir_in(library_root)?;
    let prefix = lease.prefix().as_ref().to_owned();
    let mut copied = std::collections::BTreeSet::new();
    let mut request = crate::platform::ObjectListRequest::new(
        lease.prefix().clone(), std::num::NonZeroUsize::new(100).expect("nonzero snapshot page"),
    );
    loop {
        lease.validate().await?;
        let page = objects.list_page(&request).await?;
        for object in page.objects {
            let Some(name) = object.key.as_ref().strip_prefix(&prefix)
                .and_then(|rest| rest.strip_prefix('/'))
            else {
                return Err(NarouError::Platform("snapshot key is outside the novel".into()));
            };
            if crate::platform::is_reserved_key(&object.key) || name.contains('/') || !select(name) {
                continue;
            }
            let destination = temporary.path().join(name);
            if !copy_asset(objects, lease, &object.key, &destination, object.last_modified.map(Into::into)).await? {
                return Err(NarouError::NotFound(object.key.to_string()));
            }
            copied.insert(name.to_owned());
        }
        match page.next_cursor { Some(cursor) => request = request.after(cursor), None => break }
    }
    lease.validate().await?;
    let after = fence.gate_info(lease.prefix()).await?;
    if before.epoch != after.epoch || before.content_revision != after.content_revision {
        return Err(NarouError::Platform("novel_busy: snapshot source changed during capture".into()));
    }
    Ok((temporary, copied))
}

fn validate_origin_directory(directory: &Path, write: &crate::application::novel_access::NovelWriteGuard) -> Result<()> {
    let archive = crate::db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;
    let source = crate::db::paths::ensure_within_archive_root(directory, &archive)?;
    let expected = crate::db::paths::ensure_within_archive_root(&directory_for_origin(write)?, &archive)?;
    if source != expected { return Err(NarouError::Platform("plan_stale: native directory changed its canonical operation namespace".into())); }
    Ok(())
}

fn key_for_destination(write: &crate::application::novel_access::NovelWriteGuard, destination: &Path) -> Result<crate::platform::ObjectKey> {
    let archive = crate::db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;
    let path = crate::db::paths::ensure_within_archive_root(destination, &archive)?;
    let directory = crate::db::paths::ensure_within_archive_root(&directory_for_origin(write)?, &archive)?;
    let relative = path.strip_prefix(&directory).map_err(|_| NarouError::Platform("generated output is outside its original novel directory".into()))?;
    let relative = relative.to_str().ok_or_else(|| NarouError::Platform("generated output path is not UTF-8".into()))?.replace('\\', "/");
    relative.split('/').try_fold(
        crate::platform::ObjectKey::try_new(write.prefix().as_ref())?,
        |key, component| key.join(component),
    )
}

/// Reuse the caller's admission for a coherent private multi-file input set.
pub(crate) async fn snapshot_leased(
    objects: &Arc<dyn crate::platform::ObjectStore>,
    lease: &mut crate::application::novel_access::NovelReadLease,
    library_root: &Path,
) -> Result<tempfile::TempDir> {
    snapshot_leased_excluding(objects, lease, library_root, None).await
}

/// [`snapshot_leased`] with a caller-owned exclusion on novel-relative
/// logical paths. An excluded destination subtree is never read: listing
/// entries under it are skipped before any body I/O, and illustration-index
/// members are skipped when the exclusion covers their `挿絵/…` path.
pub(crate) async fn snapshot_leased_excluding(
    objects: &Arc<dyn crate::platform::ObjectStore>,
    lease: &mut crate::application::novel_access::NovelReadLease,
    library_root: &Path,
    excluded_relative: Option<&Path>,
) -> Result<tempfile::TempDir> {
    validate_leased_source(lease).await?;
    let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("snapshot reader fence unavailable".into()))?;
    let before = fence.gate_info(lease.prefix()).await?;
    let temporary = tempfile::Builder::new().prefix(".narou-snapshot-").tempdir_in(library_root)?;
    let prefix = lease.prefix().as_ref().to_owned();
    let excluded = |relative: &Path| excluded_relative.is_some_and(|base| relative.starts_with(base));
    // The illustration pair can have aliases not present in the primary LIST.
    for listing in [lease.prefix().clone(), crate::platform::ObjectPrefix::new(format!("{prefix}/挿絵"))?] {
        let mut request = crate::platform::ObjectListRequest::new(
            listing, std::num::NonZeroUsize::new(100).expect("nonzero snapshot page"),
        );
        loop {
            lease.validate().await?;
            let page = objects.list_page(&request).await?;
            for object in page.objects {
                let relative = object.key.as_ref().strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .ok_or_else(|| NarouError::Platform("snapshot key is outside the novel".into()))?;
                if crate::platform::is_reserved_key(&object.key) { continue; }
                let relative = Path::new(relative);
                if excluded(relative) { continue; }
                let destination = temporary.path().join(relative);
                if destination.is_file() { continue; }
                if !copy_asset(objects, lease, &object.key, &destination, None).await? {
                    return Err(NarouError::NotFound(object.key.to_string()));
                }
            }
            match page.next_cursor { Some(cursor) => request = request.after(cursor), None => break }
        }
    }
    match std::fs::read(temporary.path().join(".illustration_cache.yaml")) {
        Ok(bytes) => {
            let index: crate::illustration_store::IllustrationIndex = serde_yaml::from_slice(&bytes)?;
            let prefix_key = crate::platform::ObjectKey::try_new(prefix)?;
            for filename in index.filenames() {
                let relative = Path::new("挿絵").join(&filename);
                if excluded(&relative) { continue; }
                let key = crate::platform::illustration_key(&prefix_key, &filename)?;
                let destination = temporary.path().join(&relative);
                if !destination.is_file() && !copy_asset(objects, lease, &key, &destination, None).await? {
                    return Err(NarouError::NotFound(key.to_string()));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    lease.validate().await?;
    let after = fence.gate_info(lease.prefix()).await?;
    if before.epoch != after.epoch || before.content_revision != after.content_revision {
        return Err(NarouError::Platform("novel_busy: snapshot source changed during capture".into()));
    }
    Ok(temporary)
}

#[derive(Clone, Copy)]
pub struct ConversionInputSelection {
    pub illustrations: bool,
    pub cover: bool,
}

pub(crate) struct ConversionInputs {
    directory: tempfile::TempDir,
    revision: u64,
}

impl ConversionInputs {
    pub(crate) async fn begin(objects: &Arc<dyn crate::platform::ObjectStore>, lease: &mut crate::application::novel_access::NovelReadLease, root: &Path) -> Result<Self> {
        validate_leased_source(lease).await?;
        lease.validate().await?;
        let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("conversion reader fence unavailable".into()))?;
        let revision = fence.gate_info(lease.prefix()).await?.content_revision;
        let directory = tempfile::Builder::new().prefix(".narou-snapshot-").tempdir_in(root)?;
        let prefix = crate::platform::ObjectKey::try_new(lease.prefix().as_ref())?;
        for name in ["toc.yaml", "setting.ini", "replace.txt", "converter.yaml", ".illustration_cache.yaml"] {
            copy_asset(objects, lease, &prefix.join(name)?, &directory.path().join(name), None).await?;
        }
        lease.validate().await?;
        Ok(Self { directory, revision })
    }

    pub(crate) fn path(&self) -> &Path { self.directory.path() }

    pub(crate) async fn populate(&self, objects: &Arc<dyn crate::platform::ObjectStore>, lease: &mut crate::application::novel_access::NovelReadLease, body: bool, sections: Option<&[crate::downloader::SubtitleInfo]>, selection: ConversionInputSelection) -> Result<()> {
        let prefix = crate::platform::ObjectKey::try_new(lease.prefix().as_ref())?;
        let selected = sections.map(|sections| sections.iter().map(|section| section.index.as_str()).collect::<std::collections::BTreeSet<_>>());
        let body_prefix = (body && sections.is_none_or(|sections| !sections.is_empty())).then(|| prefix.join("本文")).transpose()?;
        let image_prefix = selection.illustrations.then(|| prefix.join("挿絵")).transpose()?;
        for (is_body, listing) in [(true, body_prefix), (false, image_prefix)] {
            let Some(listing) = listing else { continue; };
            let mut request = crate::platform::ObjectListRequest::new(crate::platform::ObjectPrefix::from(listing), std::num::NonZeroUsize::new(100).expect("nonzero conversion page"));
            loop {
                lease.validate().await?;
                let page = objects.list_page(&request).await?;
                for object in page.objects {
                    let relative = object.key.as_ref().strip_prefix(prefix.as_ref()).and_then(|tail| tail.strip_prefix('/'))
                        .ok_or_else(|| NarouError::Platform("conversion input escaped its admitted novel".into()))?;
                    if crate::platform::is_reserved_key(&object.key) { continue; }
                    if is_body && let Some(selected) = &selected {
                        let filename = relative.rsplit('/').next().unwrap_or("");
                        let index = filename.split_once(' ').map(|(index, _)| index).unwrap_or(filename);
                        if !selected.contains(index) { continue; }
                    }
                    let destination = self.path().join(relative);
                    if !copy_asset(objects, lease, &object.key, &destination, None).await? { return Err(NarouError::NotFound(object.key.to_string())); }
                }
                match page.next_cursor { Some(cursor) => request = request.after(cursor), None => break }
            }
        }
        if selection.cover {
            for name in ["cover.jpg", "cover.png", "cover.jpeg"] {
                copy_asset(objects, lease, &prefix.join(name)?, &self.path().join(name), None).await?;
            }
        }
        if selection.illustrations {
            match std::fs::read(self.path().join(".illustration_cache.yaml")) {
                Ok(bytes) => {
                    let index: crate::illustration_store::IllustrationIndex = serde_yaml::from_slice(&bytes)?;
                    for filename in index.filenames() {
                        let destination = self.path().join("挿絵").join(&filename);
                        let key = crate::platform::illustration_key(&prefix, &filename)?;
                        if !destination.is_file() && !copy_asset(objects, lease, &key, &destination, None).await? { return Err(NarouError::NotFound(key.to_string())); }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        lease.validate().await?;
        let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("conversion reader fence unavailable".into()))?;
        if fence.gate_info(lease.prefix()).await?.content_revision != self.revision { return Err(NarouError::Platform("novel_busy: conversion inputs changed during capture".into())); }
        Ok(())
    }

    pub(crate) fn into_directory(self) -> tempfile::TempDir { self.directory }
}

/// Load settings from admitted private metadata before selecting body/image I/O.
pub fn conversion_snapshot_with_origin<T: Send>(directory: &Path, write: &crate::application::novel_access::NovelWriteGuard, sections: Option<&[crate::downloader::SubtitleInfo]>, configure: impl FnOnce(&Path) -> Result<(T, ConversionInputSelection)> + Send) -> Result<(tempfile::TempDir, T)> {
    let context = crate::application::messages::MessageContext::current();
    block_on(async {
        let inventory = crate::db::inventory::Inventory::with_default_root()?;
        validate_origin_directory(directory, write)?;
        let objects = read_store(inventory.root_dir())?;
        let mut lease = crate::application::novel_access::NovelReadLease::admit(&objects, write.prefix(), "native-conversion-inputs").await?;
        let result = async {
            if lease.epoch() != write.epoch() { return Err(NarouError::Platform("novel_busy: conversion source epoch changed".into())); }
            let inputs = ConversionInputs::begin(&objects, &mut lease, inventory.root_dir()).await?;
            let (configured, selection) = match context { Some(context) => context.run(|| configure(inputs.path())), None => configure(inputs.path()) }?;
            inputs.populate(&objects, &mut lease, true, sections, selection).await?;
            Ok((inputs.into_directory(), configured))
        }.await;
        let released = lease.release().await;
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    })
}

/// Publish a private generated file under its operation's original epoch.
pub fn publish_file(
    write: &crate::application::novel_access::NovelWriteGuard,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    crate::application::novel_access::require_private_path(source)?;
    let key = key_for_destination(write, destination)?;
    let file = File::open(source)?;
    let stream: crate::platform::AssetStream = Box::pin(futures::stream::try_unfold(file, |mut file| async move {
        use std::io::Read;
        let mut bytes = vec![0; 64 * 1024];
        let count = file.read(&mut bytes)?;
        if count == 0 { return Ok(None); }
        bytes.truncate(count);
        Ok(Some((bytes, file)))
    }));
    block_on(write.write_stream(&key, stream))?;
    if !crate::native::sqlite::state::mirror_files_enabled() {
        let staging = tempfile::Builder::new().prefix(".narou-snapshot-").tempdir_in(inventory.root_dir())?;
        let mut stage = tempfile::NamedTempFile::new_in(staging.path())?;
        std::io::copy(&mut File::open(source)?, stage.as_file_mut())?;
        stage.as_file().sync_all()?;
        let primary = crate::native::object_store::NativeStore::for_narou_root(inventory.root_dir())?;
        primary.with_open_write(write.fence(), || {
            let parent = destination.parent().ok_or_else(|| NarouError::Platform("output has no parent".into()))?;
            std::fs::create_dir_all(parent)?;
            stage.persist(destination).map_err(|error| NarouError::Io(error.error))?;
            Ok(())
        })?;
    }
    Ok(())
}

pub fn delete_file(write: &crate::application::novel_access::NovelWriteGuard, destination: &Path) -> Result<()> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let key = key_for_destination(write, destination)?;
    block_on(write.delete(&key))?;
    if !crate::native::sqlite::state::mirror_files_enabled() {
        let primary = crate::native::object_store::NativeStore::for_narou_root(inventory.root_dir())?;
        primary.with_open_write(write.fence(), || {
            match std::fs::remove_file(destination) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })?;
    }
    Ok(())
}

/// Legacy filesystem maintenance operates on a private input set and publishes
/// only its actual changes, all with the token captured before the snapshot.
pub struct NativeMutationSnapshot {
    directory: PathBuf,
    temporary: tempfile::TempDir,
    write: crate::application::novel_access::NovelWriteGuard,
    originals: std::collections::BTreeMap<PathBuf, [u8; 32]>,
}

impl NativeMutationSnapshot {
    pub fn begin_with_origin(directory: &Path, write: crate::application::novel_access::NovelWriteGuard) -> Result<Self> {
        let temporary = snapshot_with_origin(directory, &write)?;
        let originals = snapshot_fingerprints(temporary.path())?;
        Ok(Self { directory: directory.to_path_buf(), temporary, write, originals })
    }

    /// Resolve a native path by current record ownership, never by a guessed
    /// key-depth split. Private paths need no admission; canonical paths must
    /// belong to exactly one current novel before any source file is read.
    pub fn origin_for_path(path: &Path) -> Result<Option<(crate::application::novel_access::NovelWriteGuard, PathBuf)>> {
        if crate::application::novel_access::is_private_path(path)? {
            return Ok(None);
        }
        let archive = crate::db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;
        let path = crate::db::paths::ensure_within_archive_root(path, &archive)?;
        let primary = crate::native::object_store::NativeStore::for_current_root()?;
        let records = crate::native::novel_repository::NativeNovelRepository::new().query_current_sync(
            &crate::platform::NovelQuery::page(crate::platform::NovelFilter::all(),
                crate::platform::NovelSort::by(crate::platform::NovelSortKey::Id), 0, usize::MAX),
        )?;
        let mut owner = None;
        for record in records {
            let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
            let prefix = crate::platform::ObjectPrefix::from(keys.prefix().clone());
            let directory = crate::db::paths::ensure_within_archive_root(&primary.novel_directory(&prefix)?, &archive)?;
            if let Ok(relative) = path.strip_prefix(&directory) {
                if owner.is_some() {
                    return Err(NarouError::Platform("native source path has overlapping current novel owners".into()));
                }
                owner = Some((record, relative.to_path_buf()));
            }
        }
        let (record, relative) = owner.ok_or_else(|| NarouError::Platform("canonical source path has no current novel owner".into()))?;
        let write = capture_write_for_record(&record)?;
        Ok(Some((write, relative)))
    }

    pub fn path(&self) -> &Path { self.temporary.path() }
    pub fn canonical_directory(&self) -> &Path { &self.directory }


    pub fn origin(&self) -> &crate::application::novel_access::NovelWriteGuard { &self.write }

    pub fn publish_changes(&self) -> Result<()> {
        let updated = snapshot_fingerprints(self.path())?;
        for (relative, fingerprint) in &updated {
            if self.originals.get(relative) != Some(fingerprint) {
                publish_file(&self.write, &self.path().join(relative), &self.directory.join(relative))?;
            }
        }
        for relative in self.originals.keys() {
            if !updated.contains_key(relative) { self.delete_relative(relative)?; }
        }
        let mut removed_directories = std::collections::BTreeSet::new();
        for relative in self.originals.keys() {
            let mut parent = relative.parent();
            while let Some(directory) = parent.filter(|path| !path.as_os_str().is_empty()) {
                if !self.path().join(directory).is_dir() { removed_directories.insert(directory.to_path_buf()); }
                parent = directory.parent();
            }
        }
        if !removed_directories.is_empty() {
            let primary = crate::native::object_store::NativeStore::for_current_root()?;
            primary.with_open_write(self.write.fence(), || {
                for relative in removed_directories.iter().rev() {
                    match std::fs::remove_dir(self.directory.join(relative)) {
                        Ok(()) => {},
                        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty) => {},
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn delete_relative(&self, relative: &Path) -> Result<()> {
        delete_file(&self.write, &self.directory.join(relative))
    }
}

fn snapshot_fingerprints(root: &Path) -> Result<std::collections::BTreeMap<PathBuf, [u8; 32]>> {
    fn visit(root: &Path, directory: &Path, entries: &mut std::collections::BTreeMap<PathBuf, [u8; 32]>) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() { return Err(NarouError::Platform("private snapshot contains a symlink".into())); }
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?;
            if relative.starts_with(".objects/.narou-transfer") || relative.starts_with(".objects/.narou-staging") { continue; }
            if kind.is_dir() { visit(root, &path, entries)?; }
            else if kind.is_file() {
                let mut hash = Sha256::new();
                use std::io::Read;
                let mut input = File::open(&path)?;
                let mut buffer = [0; 64 * 1024];
                loop {
                    let count = input.read(&mut buffer)?;
                    if count == 0 { break; }
                    hash.update(&buffer[..count]);
                }
                let relative = path.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?;
                entries.insert(relative.to_path_buf(), hash.finalize().into());
            }
        }
        Ok(())
    }
    let mut entries = std::collections::BTreeMap::new();
    visit(root, root, &mut entries)?;
    Ok(entries)
}

/// Copy a leased logical file into a private destination using bounded chunks.
pub(crate) async fn copy_asset(
    objects: &Arc<dyn crate::platform::ObjectStore>,
    lease: &mut crate::application::novel_access::NovelReadLease,
    key: &crate::platform::ObjectKey,
    destination: &Path,
    restore_modified: Option<std::time::SystemTime>,
) -> Result<bool> {
    // Fs-backed keys already have a real file: hardlink it so a bulk snapshot
    // (convert copies every canonical file) never streams bytes per file
    // (issue #58). Canonical writes are atomic temp+rename, so the linked
    // inode keeps the generation the lease admitted. Remote/S3 keys return
    // `None` and keep the streamed path below.
    if let Some(source) = objects.native_source_path(key) {
        if let Some(parent) = destination.parent() { std::fs::create_dir_all(parent)?; }
        if let Err(error) = std::fs::hard_link(&source, destination) {
            // A writer may rename/replace the source mid-snapshot: the
            // hardlink misses the inode and fails. Fail loudly like the
            // streamed path's missing-object branch; the caller's
            // before/after epoch check fences mixed generations.
            if error.kind() == std::io::ErrorKind::NotFound { return Ok(false); }
            return Err(error.into());
        }
        // The link already shares the inode (and mtime): stamping
        // restore_modified here would write through to the canonical file.
        return Ok(true);
    }
    let Some(mut stream) = objects.read_stream_leased(lease.prefix(), lease.lease(), key, 0..u64::MAX).await? else { return Ok(false); };
    if let Some(parent) = destination.parent() { std::fs::create_dir_all(parent)?; }
    let mut output = File::create(destination)?;
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        lease.refresh().await?;
        std::io::Write::write_all(&mut output, &chunk?)?;
    }
    if let Some(modified) = restore_modified {
        output.set_modified(modified)?;
    }
    Ok(true)
}

#[cfg(test)]
pub(crate) fn create_test_record(mut record: crate::db::NovelRecord) -> Result<(crate::db::NovelRecord, crate::application::novel_access::NovelWriteGuard)> {
    use crate::platform::NovelRepository;
    let repository = crate::native::novel_repository::NativeNovelRepository::new();
    let stores = crate::native::object_store::NativeStores::for_current_root()?;
    block_on(async {
        let reservation = repository.allocate_id().await?;
        record.id = reservation.id().0;
        let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
        let mut write = crate::application::novel_access::NovelWriteGuard::capture_for(&stores.objects, Some(&stores.assets), &keys).await?;
        write.retain_reservation(&reservation)?;
        repository.reserve_create(write.fence(), &reservation, &record.toc_url).await?;
        repository.apply_create_guarded(write.fence(), &reservation, record.clone()).await?;
        repository.release_id(reservation).await?;
        let current = repository.get_current_sync(record.id.into())?
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", record.id)))?;
        Ok((current, write))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::NovelAccessFence;

    #[test]
    fn stale_private_maintenance_cannot_replace_or_delete_new_generation() {
        struct DatabaseRestore(Option<crate::db::Database>);
        impl Drop for DatabaseRestore { fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); } }
        let root = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        let _legacy = crate::test_support::legacy_yaml_guard();
        std::fs::create_dir(root.path().join(".narou")).unwrap();
        crate::db::init_database().unwrap();
        let record = serde_yaml::from_str("id: 1\ntitle: book\nauthor: author\nfile_title: book\nsitename: site\ntoc_url: https://example.test/book\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        let (_, write) = create_test_record(record).unwrap();
        let directory = directory_for_origin(&write).unwrap();
        let key = crate::platform::ObjectKey::try_new(write.prefix().as_ref()).unwrap();
        block_on(async {
            write.write_small(&key.join("setting.ini")?, b"old setting".to_vec()).await?;
            write.write_small(&key.join("挿絵")?.join("old.png")?, b"old image".to_vec()).await
        }).unwrap();
        let stores = crate::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let snapshot = NativeMutationSnapshot::begin_with_origin(&directory, write).unwrap();
        std::fs::write(snapshot.path().join("setting.ini"), b"stale setting").unwrap();
        std::fs::remove_file(snapshot.path().join("挿絵/old.png")).unwrap();
        let prefix = snapshot.origin().prefix().clone();
        block_on(async {
            let epoch = stores.primary.close_gate(&prefix, "test-publisher").await?;
            stores.primary.open_gate(&prefix, "test-publisher", epoch).await?;
            let fresh = crate::application::novel_access::NovelWriteGuard::capture(&stores.objects, Some(&stores.assets), &prefix).await?;
            let key = crate::platform::ObjectKey::try_new(prefix.as_ref())?;
            fresh.write_small(&key.join("setting.ini")?, b"new setting".to_vec()).await?;
            fresh.write_small(&key.join("挿絵")?.join("old.png")?, b"new image".to_vec()).await
        }).unwrap();
        assert!(snapshot.publish_changes().is_err());
        assert_eq!(std::fs::read(directory.join("setting.ini")).unwrap(), b"new setting");
        assert_eq!(std::fs::read(directory.join("挿絵/old.png")).unwrap(), b"new image");
    }

    #[test]
    fn copy_asset_hardlinks_fs_backed_sources_without_streaming() {
        // Issue #58: bulk snapshots copy every canonical file; a byte copy
        // per file was the regression. Fs objects must land as hardlinks
        // (same inode), never streamed copies.
        struct DatabaseRestore(Option<crate::db::Database>);
        impl Drop for DatabaseRestore { fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); } }
        let root = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        let _legacy = crate::test_support::legacy_yaml_guard();
        std::fs::create_dir(root.path().join(".narou")).unwrap();
        crate::db::init_database().unwrap();
        let record = serde_yaml::from_str("id: 1\ntitle: book\nauthor: author\nfile_title: book\nsitename: site\ntoc_url: https://example.test/book\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        let (_, write) = create_test_record(record).unwrap();
        let key = crate::platform::ObjectKey::try_new(write.prefix().as_ref()).unwrap();
        block_on(async {
            write.write_small(&key.join("toc.yaml")?, b"toc body".to_vec()).await
        }).unwrap();
        let objects = read_store(root.path()).unwrap();
        let prefix = crate::platform::ObjectPrefix::new(write.prefix().as_ref()).unwrap();
        let destination = root.path().join("snapshot/toc.yaml");
        block_on(async {
            let mut lease = crate::application::novel_access::NovelReadLease::admit(&objects, &prefix, "snapshot-test").await?;
            assert!(copy_asset(&objects, &mut lease, &key.join("toc.yaml")?, &destination, None).await?);
            lease.release().await
        }).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"toc body");
        let source = crate::native::object_store::NativeStore::for_narou_root(root.path())
            .unwrap().novel_directory(&prefix).unwrap().join("toc.yaml");
        // Same-inode proof without unstable APIs: a write through the
        // destination must appear when the canonical path is read back (a
        // byte copy would diverge).
        std::fs::write(&destination, b"linked write").unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), b"linked write",
            "snapshot destination must hardlink the canonical file, not copy bytes");
        std::fs::write(&destination, b"toc body").unwrap();
    }
}
