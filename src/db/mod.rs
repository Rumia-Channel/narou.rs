#[cfg(feature = "native-runtime")]
pub mod database;
#[cfg(feature = "native-runtime")]
pub mod index_store;
#[cfg(feature = "native-runtime")]
pub mod inventory;
pub mod novel_record;
pub mod sort;
#[cfg(feature = "native-runtime")]
pub mod paths;
pub mod ruby_time;

#[cfg(feature = "native-runtime")]
pub use database::{compare_records_by_key, sort_key_valid, sort_keys, Database, SORT_KEYS};
#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
pub use sort::{compare_records_by_key, sort_key_valid, sort_keys, SORT_KEYS};
pub use novel_record::NovelRecord;
#[cfg(feature = "native-runtime")]
pub use paths::{create_subdirectory_name, existing_novel_dir_for_record, novel_dir_for_record};

#[cfg(feature = "native-runtime")]
use parking_lot::Mutex;

#[cfg(feature = "native-runtime")]
use crate::error::{NarouError, Result};

#[cfg(feature = "native-runtime")]
pub static DATABASE: Mutex<Option<Database>> = parking_lot::const_mutex(None);

#[cfg(feature = "native-runtime")]
pub fn init_database() -> Result<()> {
    let db = Database::new()?;
    *DATABASE.lock() = Some(db);
    Ok(())
}

#[cfg(feature = "native-runtime")]
pub fn with_database<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&Database) -> Result<T>,
{
    let guard = DATABASE.lock();
    let db = guard
        .as_ref()
        .ok_or_else(|| NarouError::Database("Database not initialized".to_string()))?;
    f(db)
}

#[cfg(feature = "native-runtime")]
pub fn with_database_mut<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&mut Database) -> Result<T>,
{
    let mut guard = DATABASE.lock();
    let db = guard
        .as_mut()
        .ok_or_else(|| NarouError::Database("Database not initialized".to_string()))?;
    f(db)
}
