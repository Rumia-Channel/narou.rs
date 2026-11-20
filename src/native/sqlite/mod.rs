//! Native SQLite storage engine (P1 of the SQLite migration plan).
//!
//! Statements come from `crate::db::novel_query` and the record codec from
//! `crate::db::novel_codec` — both shared with the Worker D1 adapter, so the
//! WHERE/sort/status/sequence rules cannot drift; this module supplies the
//! connection handling and the driver bind conversions.
//!
//! Connection policy: WAL journal, `busy_timeout = 5s`, `foreign_keys = ON`.
//! Every trait call runs on a blocking thread (`tokio::task::spawn_blocking`)
//! because the trait is async only for D1 symmetry.

pub mod bulk;
pub mod content;
pub mod export_yaml;
pub mod freeze_repair;
pub mod versions;
pub mod object_store;
mod migrations;
pub mod state;
mod record_map;
mod repository;

pub use repository::{SqliteFreezeStore, SqliteNovelRepository};

use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::error::{NarouError, Result};

pub(crate) const SQLITE_JOURNAL_SIZE_LIMIT_BYTES: i64 = 64 * 1024 * 1024;
pub(crate) const SQLITE_CACHE_SIZE_KIB: i64 = -16384;

/// Open (creating if needed) the database file at `path`, apply pending
/// migrations, and return the guarded connection.
pub fn open(path: &std::path::Path) -> Result<Arc<Mutex<Connection>>> {
    let mut conn = Connection::open(path)
        .map_err(|error| NarouError::Platform(format!("sqlite open: {error}")))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "journal_size_limit", SQLITE_JOURNAL_SIZE_LIMIT_BYTES)
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "cache_size", SQLITE_CACHE_SIZE_KIB)
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(sqlite_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sqlite_error)?;
    migrations::apply(&mut conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

/// Open an in-memory database with the full schema applied. Intended for
/// tests and dual-run comparisons.
pub fn open_in_memory() -> Result<Arc<Mutex<Connection>>> {
    let mut conn = Connection::open_in_memory().map_err(sqlite_error)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(sqlite_error)?;
    migrations::apply(&mut conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

pub(crate) fn sqlite_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("sqlite: {error}"))
}

pub(crate) fn read_u64(row:&rusqlite::Row<'_>,index:usize)->rusqlite::Result<u64>{let value:i64=row.get(index)?;u64::try_from(value).map_err(|error|rusqlite::Error::FromSqlConversionFailure(index,rusqlite::types::Type::Integer,Box::new(error)))}
