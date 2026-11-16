use crate::test_support::{GlobalStateGuard, global_state_guard};

pub(crate) struct DatabaseGuard {
    _guard: GlobalStateGuard,
    previous: Option<crate::db::Database>,
}

/// Restore the shared database before a test's temporary library is dropped.
pub(crate) fn isolate_database_for_test() -> DatabaseGuard {
    let guard = global_state_guard();
    let previous = crate::db::DATABASE.lock().take();
    DatabaseGuard { _guard: guard, previous }
}

impl Drop for DatabaseGuard {
    fn drop(&mut self) {
        *crate::db::DATABASE.lock() = self.previous.take();
    }
}
