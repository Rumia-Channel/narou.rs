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
    let lock = cwd_lock().lock().unwrap_or_else(|e| e.into_inner());
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

/// Serialize tests that force the legacy YAML storage backend and toggle the
/// process-global `NAROU_RS_LEGACY_YAML` escape hatch.
pub(crate) fn legacy_yaml_guard() -> LegacyYamlGuard {
    let lock = LEGACY_LOCK.get_or_init(|| Mutex::new(()));
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: single-threaded with respect to other legacy-guard holders.
    unsafe { std::env::set_var("NAROU_RS_LEGACY_YAML", "1") };
    LegacyYamlGuard { _guard: guard }
}

pub(crate) struct LegacyYamlGuard {
    _guard: MutexGuard<'static, ()>,
}

impl Drop for LegacyYamlGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("NAROU_RS_LEGACY_YAML") };
    }
}

static LEGACY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
