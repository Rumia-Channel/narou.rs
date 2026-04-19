pub mod database;
pub mod index_store;
pub mod inventory;
pub mod novel_record;
pub mod paths;
pub mod ruby_time;

pub use database::Database;
pub use novel_record::NovelRecord;
pub use paths::{create_subdirectory_name, existing_novel_dir_for_record, novel_dir_for_record};

use parking_lot::Mutex;

use crate::error::{NarouError, Result};

pub static DATABASE: Mutex<Option<Database>> = parking_lot::const_mutex(None);

pub fn init_database() -> Result<()> {
    let db = Database::new()?;
    *DATABASE.lock() = Some(db);
    Ok(())
}

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
