use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) fn cwd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) struct CurrentDirGuard {
    _lock: MutexGuard<'static, ()>,
    original_dir: PathBuf,
}

pub(crate) fn set_current_dir_for_test(dir: &Path) -> CurrentDirGuard {
    let lock = cwd_lock().lock().unwrap();
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    CurrentDirGuard {
        _lock: lock,
        original_dir,
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_dir);
    }
}
