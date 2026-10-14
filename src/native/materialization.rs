use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};

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
        let absolute_root = if library_root.is_absolute() { library_root.to_path_buf() } else {
            std::env::current_dir()?.join(library_root)
        };
        let absolute_novel = if novel_dir.is_absolute() { novel_dir.to_path_buf() } else {
            std::env::current_dir()?.join(novel_dir)
        };
        let relative = absolute_novel.strip_prefix(&absolute_root)
            .map_err(|_| NarouError::Platform("materialization directory is outside the library".to_string()))?;
        // Canonicalize only the stable library root: the novel directory may
        // not exist until materialization creates it (Windows adds \\?\\ too).
        let key = std::fs::canonicalize(&absolute_root)?.join(relative);
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
}
