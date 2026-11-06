use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};

/// Drive native store I/O from CLI or a blocking Web handler without nesting
/// Tokio runtimes on the current thread.
pub(crate) fn block_on<F, T>(future: F) -> Result<T>
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

/// Copy complete stored content into a private, automatically removed directory.
/// Archives and maintenance must not restore outputs into a live conversion's
/// directory: their cleanup could otherwise delete a newly generated file.
pub fn snapshot(novel_dir: &Path) -> Result<tempfile::TempDir> {
    block_on(async {
        let inventory = crate::db::inventory::Inventory::with_default_root()?;
        let root = inventory.root_dir().join(crate::downloader::ARCHIVE_ROOT_DIR);
        let source = crate::db::paths::ensure_within_archive_root(novel_dir, &root)?;
        let prefix = crate::native::object_store::logical_key_for_native_path(&root, &source)
            .ok_or_else(|| NarouError::Platform("snapshot directory is outside the archive".into()))?;
        let stores = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?;
        let temporary = tempfile::Builder::new().prefix(".narou-snapshot-")
            .tempdir_in(inventory.root_dir())?;
        for listed_prefix in [prefix.clone(), prefix.join("挿絵")?] {
            let mut request = crate::platform::ObjectListRequest::new(
                crate::platform::ObjectPrefix::new(listed_prefix.as_ref())?,
                std::num::NonZeroUsize::new(512).unwrap(),
            );
            loop {
                let page = stores.objects.list_page(&request).await?;
                for object in page.objects {
                    let relative = object.key.as_ref().strip_prefix(prefix.as_ref())
                        .and_then(|rest| rest.strip_prefix('/'))
                        .ok_or_else(|| NarouError::Platform("snapshot key is outside the novel".into()))?;
                    if relative.starts_with("backup/") || relative.ends_with(".lock") || relative.ends_with(".tmp") {
                        continue;
                    }
                    let destination = temporary.path().join(relative);
                    if destination.is_file() {
                        continue;
                    }
                    let original = crate::db::paths::ensure_within_archive_root(&source.join(relative), &root)?;
                    if original.is_file() {
                        // Preserve the existing mirror's user edits, as the old
                        // filesystem archive collectors did.
                        std::fs::create_dir_all(destination.parent().unwrap())?;
                        std::fs::copy(original, destination)?;
                    } else if !copy_asset(stores.assets.as_ref(), &object.key, &destination).await? {
                        return Err(NarouError::NotFound(object.key.to_string()));
                    }
                }
                match page.next_cursor {
                    Some(cursor) => request = request.after(cursor),
                    None => break,
                }
            }
        }
        // Content-addressed images need not appear in a per-novel S3 LIST.
        match std::fs::read(temporary.path().join(".illustration_cache.yaml")) {
            Ok(bytes) => {
                let index: crate::illustration_store::IllustrationIndex = serde_yaml::from_slice(&bytes)?;
                for filename in index.filenames() {
                    let key = crate::platform::illustration_key(&prefix, &filename)?;
                    let relative = key.as_ref().strip_prefix(prefix.as_ref()).unwrap().trim_start_matches('/');
                    let destination = temporary.path().join(relative);
                    if !destination.is_file() {
                        copy_asset(stores.assets.as_ref(), &key, &destination).await?;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(temporary)
    })
}

/// Copy a single stored asset without imposing the small-object buffering cap.
pub(crate) async fn copy_asset(
    assets: &dyn crate::platform::AssetStore,
    key: &crate::platform::ObjectKey,
    destination: &Path,
) -> Result<bool> {
    let Some(mut stream) = assets.read_stream(key).await? else { return Ok(false); };
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut output = File::create(destination)?;
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        std::io::Write::write_all(&mut output, &chunk?)?;
    }
    Ok(true)
}

static SCOPES: LazyLock<Mutex<HashMap<PathBuf, Weak<MaterializationScope>>>> = LazyLock::new(Default::default);

/// Same-process users share restored files until the last reader finishes.
/// A file lock serializes other processes touching the same novel's temporary
/// files. Existing user files are never registered for deletion.
pub(crate) struct MaterializationScope {
    key: PathBuf,
    owned: Mutex<HashSet<PathBuf>>,
    lock: File,
}

impl MaterializationScope {
    pub(crate) fn acquire(library_root: &Path, novel_dir: &Path) -> Result<Arc<Self>> {
        // Use the same canonical containment rule as the store. A Web request
        // may already hold a canonical path while the configured root retains
        // a different spelling (including Windows case/verbatim aliases).
        let key = crate::db::paths::ensure_within_archive_root(novel_dir, library_root)?;
        if let Some(scope) = SCOPES.lock().get(&key).and_then(Weak::upgrade) {
            return Ok(scope);
        }
        let lock_dir = library_root.join(".narou/materialization-locks");
        std::fs::create_dir_all(&lock_dir)?;
        let name = hex::encode(Sha256::digest(key.to_string_lossy().as_bytes()));
        let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(lock_dir.join(format!("{name}.lock")))?;
        // Never hold the process registry while waiting for another process.
        fs2::FileExt::lock_exclusive(&lock)?;
        let scope = Arc::new(Self { key: key.clone(), owned: Mutex::new(HashSet::new()), lock });
        SCOPES.lock().insert(key, Arc::downgrade(&scope));
        Ok(scope)
    }

    pub(crate) fn register(&self, path: &Path) {
        self.owned.lock().insert(path.to_path_buf());
    }
}

impl Drop for MaterializationScope {
    fn drop(&mut self) {
        for path in self.owned.get_mut().drain() {
            let _ = std::fs::remove_file(path);
        }
        let _ = fs2::FileExt::unlock(&self.lock);
        let mut scopes = SCOPES.lock();
        if scopes.get(&self.key).is_some_and(|scope| std::ptr::eq(scope.as_ptr(), self)) {
            scopes.remove(&self.key);
        }
        // Keep lock files: unlinking a lock can let two processes lock different
        // inodes under the same name and bypass serialization.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_readers_keep_owned_files_until_the_last_scope_drops() {
        let root = tempfile::tempdir().unwrap();
        let novel = root.path().join("novel");
        std::fs::create_dir(&novel).unwrap();
        let original = novel.join("original");
        let restored = novel.join("restored");
        std::fs::write(&original, b"user file").unwrap();
        let first = MaterializationScope::acquire(root.path(), &novel).unwrap();
        std::fs::write(&restored, b"stored file").unwrap();
        first.register(&restored);
        let second = MaterializationScope::acquire(root.path(), &novel).unwrap();
        drop(first);
        assert_eq!(std::fs::read(&restored).unwrap(), b"stored file");
        drop(second);
        assert!(!restored.exists());
        assert_eq!(std::fs::read(&original).unwrap(), b"user file");
    }

    #[cfg(windows)]
    #[test]
    fn canonical_scope_preserves_files_across_root_case_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("MixedCaseLibrary");
        let configured_root = temp.path().join("mixedcaselibrary");
        let novel = root.join("novel");
        std::fs::create_dir_all(&novel).unwrap();
        let restored = novel.join("restored");
        let first = MaterializationScope::acquire(&configured_root, &novel).unwrap();
        std::fs::write(&restored, b"stored file").unwrap();
        first.register(&restored);
        let second = MaterializationScope::acquire(&root, &novel).unwrap();
        drop(first);
        assert_eq!(std::fs::read(&restored).unwrap(), b"stored file");
        drop(second);
        assert!(!restored.exists());
    }
}
