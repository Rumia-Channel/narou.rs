//! Native SQLite storage engine (P1 of the SQLite migration plan).
//!
//! SQL semantics mirror the Worker D1 adapter exactly (same WHERE building,
//! sort expressions, status materialization, sequence allocation). The novel
//! record codec — column lists, bind values, upsert/select SQL — lives in
//! `crate::db::novel_codec`, shared with the Worker so they cannot drift.
//!
//! Connection policy: WAL journal, `busy_timeout = 5s`, `foreign_keys = ON`.
//! Every trait call runs on a blocking thread (`tokio::task::spawn_blocking`)
//! because the trait is async only for D1 symmetry.

pub mod bulk;
pub mod content;
pub mod versions;
pub mod object_store;
mod migrations;
pub mod state;
mod query;
mod record_map;
mod repository;

pub use repository::{SqliteFreezeStore, SqliteNovelRepository};

use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::error::{NarouError, Result};

/// Open (creating if needed) the database file at `path`, apply pending
/// migrations, and return the guarded connection.
pub fn open(path: &std::path::Path) -> Result<Arc<Mutex<Connection>>> {
    let mut conn = Connection::open(path)
        .map_err(|error| NarouError::Platform(format!("sqlite open: {error}")))?;
    conn.pragma_update(None, "journal_mode", "WAL")
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
