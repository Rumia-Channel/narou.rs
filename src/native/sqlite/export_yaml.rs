//! Rollback bundle: write the legacy YAML files back from the database.
//!
//! `narou db export-yaml` and the storage switch in the Web UI both use this.
//! An in-place export also flips `.narou/storage-backend` back to `yaml`, so
//! the next run (and narou.rb) sees a complete legacy library.
//!
//! # What is written
//!
//! In SQLite mode the database — not `小説データ/` — is authoritative, and
//! `sqlite.mirror-files=false` keeps no files there at all, so a rollback has
//! to render the stored objects back into the narou.rb layout before the
//! marker flips:
//!
//! - `.narou/database.yaml` and `database_index.yaml` from the live records,
//! - the managed states (`freeze.yaml`, `alias.yaml`, …) verbatim from
//!   `app_state`, plus `global_setting.yaml` in its own directory (it stays a
//!   plain file in both modes),
//! - every stored object: `novels/…` under `小説データ/…`, user site
//!   definitions (`webnovel/<name>.yaml`) into the library `webnovel/` folder
//!   (where YAML mode reads them from — the object mirror under
//!   `小説データ/.objects/webnovel/` is never read by narou.rb), and any other
//!   namespace into `小説データ/.objects/…` like the filesystem store does,
//! - illustrations that the per-novel index references but that only exist in
//!   S3 (`s3.asset-backend=s3`; the dedup pool is not listable as a logical
//!   key). They are read back through the configured illustration backend so
//!   the rolled-back library still resolves `挿絵/<file>`.
//!
//! Files that exist on disk without a stored row (pre-import files, or writes
//! that bypassed the store) are part of the storage fallback and are exported
//! as well; nothing outside those paths is touched, so unrelated user files in
//! the library survive unchanged.
//!
//! 本文 and friends are rendered from the object store, which is the
//! authoritative working set (`novel_sections` only mirrors it). A library
//! that is already in YAML mode has nothing to render — the export then writes
//! only `database.yaml` and `database_index.yaml`.
//!
//! Without `--in-place` the same content lands in one bundle directory
//! (default `narou-yaml-export/`): the `.narou/` states at its top level (the
//! historical `--out` layout), the content under `<out>/小説データ/…`, the
//! definitions under `<out>/webnovel/` and the global setting under
//! `<out>/.narousetting/`. The storage marker is never touched in that mode.
//!
//! # Failure policy
//!
//! The marker flips only after every file has been written. A read, decode
//! (CRC-32) or write error aborts the export with the marker left as it was:
//! a corrupt database, an unreadable object or an unreachable S3 illustration
//! must not be reported as a completed rollback.
//! In-place management files are atomically published with their SHA-256
//! provenance recorded first. SQLite startup retires unchanged export
//! projections without reimporting them, preserving writes made after a
//! failed rollback; genuinely edited files remain legacy import sources.

use std::collections::BTreeSet;
use std::future::Future;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;

use crate::db::state_inventory;
use crate::error::{NarouError, Result};
use crate::native::object_store::NativeObjectStore;
use crate::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListRequest, ObjectPrefix, ObjectStore,
};

use super::state::StorageMode;

/// Directory a non-in-place export defaults to (under the library root).
const DEFAULT_EXPORT_DIR: &str = "narou-yaml-export";

/// Illustration index every novel directory may carry; its entries name the
/// illustration files the section bodies refer to.
const ILLUSTRATION_CACHE_FILE: &str = ".illustration_cache.yaml";

/// Listing page size for the object walk (one page per round trip).
const OBJECT_PAGE: usize = 512;

/// Write the YAML/TXT bundle and return the directory holding it.
///
/// `in_place` writes to the real locations and switches the storage marker
/// back to YAML; otherwise everything lands under `out` (or
/// `<root>/narou-yaml-export`) so nothing is overwritten.
pub fn export_yaml(out: Option<PathBuf>, in_place: bool) -> Result<PathBuf> {
    run_export(export_yaml_async(out, in_place))
}

/// Async entry point. The Web switch handler already runs on a runtime and
/// awaits this directly; [`export_yaml`] drives it for synchronous callers.
pub async fn export_yaml_async(out: Option<PathBuf>, in_place: bool) -> Result<PathBuf> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let root = inventory.root_dir().to_path_buf();
    let narou_dir = root.join(".narou");
    // Validate the authoritative backend before writing any rollback artifact.
    let sqlite_state = match crate::native::sqlite::state::read_mode(&narou_dir) {
        StorageMode::Sqlite => Some(crate::native::sqlite::state::active_for(&narou_dir).ok_or_else(|| {
            NarouError::Database(format!(
                "storage-backend は sqlite ですが {} を開けませんでした (YAML への書き出しを中止しました)",
                narou_dir.join("db.sqlite").display()
            ))
        })?),
        StorageMode::Yaml => None,
    };
    let out_dir = if in_place {
        narou_dir.clone()
    } else {
        out.unwrap_or_else(|| root.join(DEFAULT_EXPORT_DIR))
    };
    std::fs::create_dir_all(&out_dir)?;
    let layout = ExportLayout::new(&root, out_dir.clone(), in_place)?;
    let export_state = sqlite_state.as_ref().filter(|_| {
        in_place || comparable(&out_dir) == comparable(&narou_dir)
    });

    // Take DATABASE before the authority lock, matching record mutations.
    // Keep synchronous metadata reads and publication together: a running Web
    // process may retain a cache older than another CLI writer's commit.
    let global_payload = crate::db::with_database(|db| {
        let publish = || -> Result<Option<String>> {
            let mut records = if let Some(state) = &sqlite_state {
                let source = narou_dir.join("database.yaml");
                let file_raw = if state.compat_now() {
                    match crate::db::inventory::ensure_yaml_size_limit(&source) {
                        Ok(()) => Some(std::fs::read_to_string(&source)?),
                        Err(NarouError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                };
                if let Some(raw) = file_raw {
                    if raw.trim().is_empty() { std::collections::BTreeMap::new() }
                    else { serde_yaml::from_str(&raw)? }
                } else {
                    super::bulk::load_all_records(&state.conn_ref().lock())?
                }
            } else {
                db.all_records().iter().map(|(&id, record)| (id, record.clone())).collect()
            };
            for (&id, record) in &mut records { record.id = id; }
            write_state_file(export_state, &out_dir, "database.yaml", &serde_yaml::to_string(&records)?)?;

            // Derive the index from that same authoritative map, never from
            // the singleton's independently stale cached index.
            let mut index = crate::db::index_store::IndexStore::new();
            index.reconcile(&records.into_iter().collect());
            write_state_file(export_state, &out_dir, "database_index.yaml", &index.to_yaml()?)?;
            if let Some(state) = &sqlite_state {
                for (key, file) in state_inventory::import_export_states() {
                    if let Some(payload) = state.get_raw_unlocked("inv", key)? {
                        write_state_file(export_state, &out_dir, file, &payload)?;
                    }
                }
                state.get_raw_db("global", "global_setting")
            } else {
                Ok(None)
            }
        };
        if let Some(state) = &sqlite_state { state.with_compat_lock(publish) }
        else { publish() }
    })?;
    // Global settings are shared, file-only state; do not hold the library
    // authority lock while taking their independent file lock.
    if let Some(payload) = global_payload {
        let global_dir = layout.global_setting_dir();
        std::fs::create_dir_all(&global_dir)?;
        crate::db::inventory::atomic_write(&global_dir.join("global_setting.yaml"), &payload)?;
    }
    if let Some(state) = &sqlite_state {
        // No synchronous authority guard is held across asynchronous object IO.
        export_objects(state, &layout).await?;
    }

    if in_place {
        // Switch the storage marker back so the next launch runs in legacy
        // YAML mode and narou.rb sees the full library.
        crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Yaml)?;
    }
    Ok(out_dir)
}

fn write_state_file(
    state: Option<&super::state::StateDb>,
    directory: &Path,
    file: &str,
    content: &str,
) -> Result<()> {
    let path = directory.join(file);
    crate::db::inventory::with_exclusive_file_lock(&path, || {
        if let Some(state) = state {
            state.remember_export_file(file, &path, content)?;
        }
        crate::db::inventory::atomic_write_locked(&path, content)?;
        if let Some(state) = state {
            state.finish_export_file(file, content)?;
        }
        Ok(())
    })
}

/// Where the exported halves go: the real library (`in_place`) or one bundle
/// directory.
struct ExportLayout {
    /// The library's real `小説データ/`: reads (and the filesystem fallback)
    /// come from here even when the export itself writes into a bundle.
    library_archive: PathBuf,
    /// Where user site definitions are written.
    definitions_dir: PathBuf,
    /// `~/.narousetting` (in place) or the bundle's `.narousetting`.
    global_setting_dir: PathBuf,
    /// Filesystem store rooted at the export's archive root; `resolve_path`
    /// maps `novels/…` onto the legacy layout and every other namespace under
    /// `.objects/`.
    write_root: NativeObjectStore,
}

impl ExportLayout {
    fn new(root: &Path, out_dir: PathBuf, in_place: bool) -> Result<Self> {
        let library_archive = root.join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let archive_root = if in_place {
            library_archive.clone()
        } else {
            // A bundle inside `小説データ/` would be re-imported as library
            // content on the next start; refuse instead of corrupting both.
            if comparable(&out_dir).starts_with(comparable(&library_archive)) {
                return Err(NarouError::Database(format!(
                    "出力先が {} の中にあります: {}",
                    crate::downloader::types::ARCHIVE_ROOT_DIR,
                    out_dir.display()
                )));
            }
            out_dir.join(crate::downloader::types::ARCHIVE_ROOT_DIR)
        };
        Ok(Self {
            library_archive,
            definitions_dir: if in_place {
                root.join("webnovel")
            } else {
                out_dir.join("webnovel")
            },
            global_setting_dir: if in_place {
                crate::db::inventory::global_setting_dir(root)
            } else {
                out_dir.join(".narousetting")
            },
            write_root: NativeObjectStore::from_root(archive_root)?,
        })
    }

    fn global_setting_dir(&self) -> PathBuf {
        self.global_setting_dir.clone()
    }

    /// File a logical key is written back to.
    fn target_path(&self, key: &ObjectKey) -> Result<PathBuf> {
        // `webnovel/<name>.yaml` is the one namespace YAML mode does not read
        // from the archive tree: site definitions live next to the library.
        if let Some(name) = key.as_ref().strip_prefix("webnovel/")
            && !name.contains('/')
            && name.ends_with(".yaml")
        {
            return Ok(self.definitions_dir.join(name));
        }
        self.write_root.resolve_path(key)
    }
}

/// Absolute, symlink-resolved form for the "is this inside the archive?" guard.
/// Both paths usually exist; a missing one falls back to the absolute path so
/// the comparison still works.
fn comparable(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Render every stored object and the illustrations the per-novel indexes
/// reference back into the export layout.
async fn export_objects(
    state: &crate::native::sqlite::state::StateDb,
    layout: &ExportLayout,
) -> Result<usize> {
    let library_mirror = NativeObjectStore::from_root(layout.library_archive.clone())?;
    let store =
        super::object_store::SqliteObjectStore::new(state.conn_ref().clone(), library_mirror);
    let (written, indexed, seen) = write_stored_objects(&store, layout).await?;
    let backend = illustration_backend()?;
    let indexed_written =
        write_indexed_illustrations(&store, layout, &seen, indexed, backend.as_deref()).await?;
    Ok(written + indexed_written)
}

/// The configured illustration backend, or `None` when illustrations live in
/// the local store.
///
/// An S3 illustration backend that cannot be built must not silently produce a
/// bundle without the illustrations it holds (fail closed), but a local-only
/// library never touches S3 at all.
fn illustration_backend() -> Result<Option<Arc<dyn AssetStore>>> {
    if !crate::native::s3::illustrations_in_s3() {
        return Ok(None);
    }
    let Some(store) = crate::native::s3::illustration_store()? else {
        return Ok(None);
    };
    let store: Arc<dyn AssetStore> = store;
    Ok(Some(store))
}

/// Write every listed object to its legacy path and collect the per-novel
/// illustration indexes (used for the S3-only files).
#[allow(clippy::type_complexity)]
async fn write_stored_objects(
    store: &super::object_store::SqliteObjectStore,
    layout: &ExportLayout,
) -> Result<(usize, Vec<(ObjectKey, Vec<String>)>, BTreeSet<String>)> {
    let mut written = 0usize;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut indexes: Vec<(ObjectKey, Vec<String>)> = Vec::new();

    let mut cursor: Option<String> = None;
    loop {
        let mut request = ObjectListRequest::new(
            ObjectPrefix::new("")?,
            NonZeroUsize::new(OBJECT_PAGE).expect("page size is non-zero"),
        );
        if let Some(current) = &cursor {
            request = request.after(current.clone());
        }
        let page = store.list_page(&request).await?;
        for metadata in &page.objects {
            if !seen.insert(metadata.key.as_ref().to_string()) {
                continue;
            }
            let key = &metadata.key;
            if let Some(prefix) = index_prefix(key) {
                let Some(payload) = store.read_small(key).await? else {
                    return Err(NarouError::Database(format!(
                        "挿絵索引を読み出せません: {key}"
                    )));
                };
                let index: crate::illustration_store::IllustrationIndex =
                    serde_yaml::from_slice(&payload).map_err(|error| {
                        NarouError::Database(format!("挿絵索引 {key} を読めません: {error}"))
                    })?;
                indexes.push((
                    ObjectKey::try_new(prefix.to_string())?,
                    index.filenames().into_iter().collect(),
                ));
            }
            if !write_asset(store, key, &layout.target_path(key)?).await? {
                return Err(NarouError::Database(format!(
                    "書き出すべきオブジェクトを読み出せません: {key}"
                )));
            }
            written += 1;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok((written, indexes, seen)),
        }
    }
}

/// Illustrations the index names but the listing does not carry.
///
/// With `s3.asset-backend=s3` the dedup pool keeps the bytes under a physical
/// name the logical listing hides, so the index is the only reference. A name
/// with no file in the local store is read from the illustration backend; a
/// name with no file anywhere is skipped (the index is metadata and does not
/// guarantee the blob), while a failed read aborts the export.
async fn write_indexed_illustrations(
    store: &super::object_store::SqliteObjectStore,
    layout: &ExportLayout,
    seen: &BTreeSet<String>,
    indexes: Vec<(ObjectKey, Vec<String>)>,
    backend: Option<&dyn AssetStore>,
) -> Result<usize> {
    let mut written = 0usize;
    for (prefix, names) in indexes {
        for name in names {
            let key = crate::platform::illustration_key(&prefix, &name)?;
            if seen.contains(key.as_ref()) {
                continue;
            }
            let path = layout.target_path(&key)?;
            if write_asset(store, &key, &path).await? {
                written += 1;
                continue;
            }
            let Some(backend) = backend else { continue };
            if write_asset(backend, &key, &path).await? {
                written += 1;
            }
        }
    }
    Ok(written)
}

/// `<novel prefix>` of a `.illustration_cache.yaml` object key.
fn index_prefix(key: &ObjectKey) -> Option<&str> {
    let prefix = key.as_ref().strip_suffix(ILLUSTRATION_CACHE_FILE)?;
    let prefix = prefix.strip_suffix('/')?;
    (!prefix.is_empty()).then_some(prefix)
}

/// Stream one stored object into `path`, replacing any previous file
/// atomically. Returns `false` when the store does not know the key.
async fn write_asset(store: &dyn AssetStore, key: &ObjectKey, path: &Path) -> Result<bool> {
    let Some(stream) = store
        .read_stream(key)
        .await
        .map_err(|error| export_error(key, error))?
    else {
        return Ok(false);
    };
    write_stream_to_path(stream, path)
        .await
        .map_err(|error| export_error(key, error))?;
    Ok(true)
}

/// Name the object an export failure came from: the CLI and the Web switch
/// both surface this message, and "some payload is corrupt" is not actionable.
fn export_error(key: &ObjectKey, error: NarouError) -> NarouError {
    match error {
        NarouError::Database(message) => NarouError::Database(format!("{key}: {message}")),
        other => NarouError::Database(format!("{key}: {other}")),
    }
}

/// Drain one asset stream into a temporary file next to `path` and move it
/// into place, so an interrupted write never leaves a half-written library
/// file behind (the chunks are never held in memory as a whole).
async fn write_stream_to_path(mut stream: AssetStream, path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        NarouError::Database(format!("書き出し先に親ディレクトリがありません: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".narou-export-")
        .tempfile_in(parent)?;
    while let Some(chunk) = stream.next().await {
        temporary.write_all(&chunk?)?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Drive the async export from synchronous callers. A caller that already runs
/// on a runtime (the Web handler) gets a dedicated thread, so the blocking
/// object walk never stalls the runtime's worker threads.
fn run_export<T>(future: impl Future<Output = Result<T>> + Send) -> Result<T>
where
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("yaml-export".to_string())
                .spawn_scoped(scope, move || run_export(future))
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| NarouError::Platform(error.to_string()))?;
    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::MemoryObjectStore;
    use std::sync::Arc;

    #[test]
    fn interrupted_second_publication_preserves_later_acknowledged_state() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join(".narou");
        let state = super::super::state::configure(&managed).unwrap();
        state.set_raw("inv", "notepad", "before").unwrap();
        write_state_file(Some(&state), &managed, "notepad.txt", "before").unwrap();
        state.set_raw("inv", "notepad", "after").unwrap();

        // A second attempt commits its provenance, then stops before replacing
        // the first projection (process interruption or failed atomic publish).
        state.remember_export_file("notepad.txt", &managed.join("notepad.txt"), "second").unwrap();
        let reopened = super::super::state::configure(&managed).unwrap();
        assert_eq!(reopened.get_raw("inv", "notepad").unwrap().as_deref(), Some("after"));
    }

    fn temp_layout(in_place: bool, dir: &Path) -> ExportLayout {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        ExportLayout::new(&root, dir.join("bundle"), in_place).unwrap()
    }

    fn memory_store(layout: &ExportLayout) -> super::super::object_store::SqliteObjectStore {
        let conn = {
            let mut conn = rusqlite::Connection::open_in_memory().unwrap();
            crate::native::sqlite::migrations::apply(&mut conn).unwrap();
            conn
        };
        let mirror = NativeObjectStore::from_root(layout.library_archive.clone()).unwrap();
        super::super::object_store::SqliteObjectStore::with_mirror_files(
            Arc::new(parking_lot::Mutex::new(conn)),
            mirror,
            false,
        )
    }

    /// Every stored namespace reaches its legacy path: content under the
    /// archive root, definitions next to the library, other namespaces under
    /// `.objects/`.
    #[test]
    fn objects_render_into_the_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = temp_layout(true, dir.path());
        let archive = layout.write_root.root();
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("novels/site/title/toc.yaml").unwrap())
                .unwrap(),
            archive.join("site").join("title").join("toc.yaml")
        );
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("webnovel/example.com.yaml").unwrap())
                .unwrap(),
            dir.path().join("library").join("webnovel").join("example.com.yaml")
        );
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("generated/epub/book.epub").unwrap())
                .unwrap(),
            archive.join(".objects").join("generated").join("epub").join("book.epub")
        );
        // A nested `webnovel/…` key is not a definition file: it stays in the
        // archive tree instead of leaking subdirectories into `webnovel/`.
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("webnovel/挿絵/0001.png").unwrap())
                .unwrap(),
            archive.join(".objects").join("webnovel").join("挿絵").join("0001.png")
        );
    }

    /// A bundle inside `小説データ/` would be imported as content on the next
    /// start, so the export refuses it.
    #[test]
    fn bundle_inside_the_archive_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let inside = root.join(crate::downloader::types::ARCHIVE_ROOT_DIR).join("export");
        std::fs::create_dir_all(&inside).unwrap();
        let error = ExportLayout::new(&root, inside, false).err().expect("unsafe export is refused");
        assert!(error.to_string().contains("出力先"), "{error}");
    }

    /// The object walk writes database rows and filesystem-only files into the
    /// legacy layout and reports the index contents for the S3 pass.
    #[test]
    fn stored_objects_are_rendered_to_the_legacy_layout() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let archive = layout.write_root.root().to_path_buf();
            let store = memory_store(&layout);
            let keys = crate::platform::NovelObjectKeys::new("site", "title", false).unwrap();

            ObjectStore::write_small(&store, &keys.toc(), b"toc\n".to_vec())
                .await
                .unwrap();
            ObjectStore::write_small(&store, &keys.setting(), b"ini\n".to_vec())
                .await
                .unwrap();
            ObjectStore::write_small(
                &store,
                &keys.illustration_cache(),
                b"sources:\n  https://example.com/0002.png: 0002.png\n".to_vec(),
            )
            .await
            .unwrap();
            // A file written outside the store (pre-import / legacy writer).
            let legacy = archive.join("site/title/挿絵/0001.png");
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, b"legacy").unwrap();

            let (written, indexes, seen) = write_stored_objects(&store, &layout).await.unwrap();
            assert_eq!(written, 4);
            assert_eq!(
                std::fs::read(archive.join("site/title/toc.yaml")).unwrap(),
                b"toc\n"
            );
            assert_eq!(
                std::fs::read(archive.join("site/title/setting.ini")).unwrap(),
                b"ini\n"
            );
            assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy");
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].0.as_ref(), "novels/site/title");
            assert_eq!(indexes[0].1, vec!["0002.png".to_string()]);
            assert!(seen.contains("novels/site/title/挿絵/0001.png"));
        });
    }

    /// Illustrations named by the index but absent from the local store are
    /// materialized from the illustration backend; names with no file anywhere
    /// are skipped instead of failing the rollback.
    #[test]
    fn indexed_illustrations_are_materialized_from_the_backend() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let archive = layout.write_root.root().to_path_buf();
            let store = memory_store(&layout);
            let prefix = ObjectKey::try_new("novels/site/title").unwrap();
            let pooled = crate::platform::illustration_key(&prefix, "0002.png").unwrap();
            let backend = MemoryObjectStore::new();
            crate::platform::AssetStore::write_stream(
                &backend,
                &pooled,
                Box::pin(futures::stream::iter(vec![Ok(b"image".to_vec())])),
            )
            .await
            .unwrap();

            let indexes = vec![(
                prefix.clone(),
                vec!["0002.png".to_string(), "upstream.png".to_string()],
            )];
            let backend_ref: &dyn AssetStore = &backend;
            let written = write_indexed_illustrations(
                &store,
                &layout,
                &BTreeSet::new(),
                indexes,
                Some(backend_ref),
            )
            .await
            .unwrap();
            assert_eq!(written, 1);
            assert_eq!(
                std::fs::read(archive.join("site/title/挿絵/0002.png")).unwrap(),
                b"image"
            );
            assert!(!archive.join("site/title/挿絵/upstream.png").exists());
        });
    }

    /// Defines the files the marker switch must not lose: an object whose
    /// payload cannot be read fails the export instead of producing a partial
    /// bundle.
    #[test]
    fn unreadable_objects_abort_the_export() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let store = memory_store(&layout);
            let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &key, b"toc\n".to_vec())
                .await
                .unwrap();
            // Corrupt the stored CRC-32 so the payload fails verification.
            {
                let conn = store.conn_handle();
                let guard = conn.lock();
                guard
                    .execute(
                        "UPDATE objects SET crc32 = 0 WHERE object_key = ?",
                        rusqlite::params![key.as_ref()],
                    )
                    .unwrap();
            }
            let error = write_stored_objects(&store, &layout).await.unwrap_err();
            assert!(error.to_string().contains("toc.yaml"), "{error}");
        });
    }
}
