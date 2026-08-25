//! `PRAGMA user_version`-driven schema migrations.
//!
//! The SQL files are ported from `worker_entry/migrations/` (D1) so the two
//! backends keep identical table definitions. Each step runs inside one
//! transaction and is idempotent (`IF NOT EXISTS` / additive `ALTER TABLE`).

use rusqlite::Connection;

use crate::error::{NarouError, Result};

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/0001_core.sql")),
    (2, include_str!("migrations/0002_search.sql")),
    (3, include_str!("migrations/0003_status_sort.sql")),
    (4, include_str!("migrations/0004_extra_fields_yaml.sql")),
    (5, include_str!("migrations/0005_content.sql")),
    (6, include_str!("migrations/0006_versions.sql")),
];

pub(crate) fn apply(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| NarouError::Platform(format!("sqlite user_version: {error}")))?;
    for &(version, sql) in MIGRATIONS {
        if version <= current {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|error| NarouError::Platform(format!("sqlite begin migration {version}: {error}")))?;
        tx.execute_batch(sql)
            .map_err(|error| NarouError::Platform(format!("sqlite migration {version}: {error}")))?;
        tx.pragma_update(None, "user_version", version)
            .map_err(|error| NarouError::Platform(format!("sqlite set user_version {version}: {error}")))?;
        tx.commit()
            .map_err(|error| NarouError::Platform(format!("sqlite commit migration {version}: {error}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_reaches_latest_user_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 6);
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn).unwrap();
        apply(&mut conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('novels','novel_tags','frozen_novels','app_state','novel_id_sequence')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 5);
    }
}
