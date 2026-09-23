use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

thread_local! {
    /// How many guards this thread already holds. The lock below is not
    /// reentrant, so nested helpers share the outermost guard.
    static GUARD_DEPTH: Cell<usize> = const { Cell::new(0) };
}

fn state_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Serialize tests that touch process-global library state: the working
/// directory, the process-wide database binding (`db::init_database`), and the
/// `NAROU_RS_LEGACY_YAML` escape hatch. Any two of those interleave into
/// another test's library, so all three share one lock.
pub(crate) struct GlobalStateGuard {
    _lock: Option<MutexGuard<'static, ()>>,
}

impl Drop for GlobalStateGuard {
    fn drop(&mut self) {
        GUARD_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Reentrant within a thread: helpers like [`set_current_dir_for_test`] and
/// [`legacy_yaml_guard`] call this themselves, and a test may hold both.
pub(crate) fn global_state_guard() -> GlobalStateGuard {
    let depth = GUARD_DEPTH.with(|depth| {
        let current = depth.get();
        depth.set(current + 1);
        current
    });
    if depth > 0 {
        return GlobalStateGuard { _lock: None };
    }
    let lock = state_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    GlobalStateGuard { _lock: Some(lock) }
}

pub(crate) struct CurrentDirGuard {
    _guard: GlobalStateGuard,
    original_dir: PathBuf,
}

pub(crate) fn set_current_dir_for_test(dir: &Path) -> CurrentDirGuard {
    let guard = global_state_guard();
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    CurrentDirGuard {
        _guard: guard,
        original_dir,
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_dir);
    }
}

/// Serialize tests that force the legacy YAML storage backend and toggle the
/// process-global `NAROU_RS_LEGACY_YAML` escape hatch. Holds the shared state
/// guard so a SQLite-mode test cannot observe the flag mid-run.
pub(crate) fn legacy_yaml_guard() -> LegacyYamlGuard {
    let guard = global_state_guard();
    // SAFETY: single-threaded with respect to other guard holders.
    unsafe { std::env::set_var("NAROU_RS_LEGACY_YAML", "1") };
    LegacyYamlGuard { _guard: guard }
}

pub(crate) struct LegacyYamlGuard {
    _guard: GlobalStateGuard,
}

impl Drop for LegacyYamlGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("NAROU_RS_LEGACY_YAML") };
    }
}
