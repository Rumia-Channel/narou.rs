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
    (7, include_str!("migrations/0007_toc_url_not_unique.sql")),
    (8, include_str!("migrations/0008_objects.sql")),
    (9, include_str!("migrations/0009_section_bodies.sql")),
    (10, include_str!("migrations/0010_drop_body_yaml.sql")),
    (11, include_str!("migrations/0011_requires_login.sql")),
    (12, include_str!("migrations/0012_login_session.sql")),
];

pub(crate) fn apply(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| NarouError::Platform(format!("sqlite user_version: {error}")))?;
    for &(version, sql) in MIGRATIONS {
        if version <= current {
            continue;
        }
        // Before 0010 drops the legacy body_yaml columns, move existing
        // bodies into the content-addressed section_bodies table. Runs here
        // (not after 0009) so a crash between 0009 and 0010 still migrates:
        // the function is idempotent and skips rows already carrying
        // body_hash.
        if version == 10 {
            super::object_store::migrate_section_bodies(conn)?;
        }
        // 0007 rebuilds `novels` (DROP + RENAME). With foreign_keys ON the
        // DROP cascades and erases the children that exist only in the
        // native schema (novel_outputs, novel_sections, novel_versions and
        // their grandchildren) — the SQL file itself evacuates only the
        // children shared with D1. `PRAGMA foreign_keys` is a no-op inside
        // a transaction, so it is toggled here, outside the BEGIN.
        if version == 7 {
            conn.pragma_update(None, "foreign_keys", "OFF")
                .map_err(|error| {
                    NarouError::Platform(format!("sqlite foreign_keys off: {error}"))
                })?;
        }
        let result = apply_one(conn, version, sql);
        if version == 7 {
            // Always re-enable, even when the migration failed and rolled
            // back, so the connection returns to its documented policy.
            let restore = conn
                .pragma_update(None, "foreign_keys", "ON")
                .map_err(|error| NarouError::Platform(format!("sqlite foreign_keys on: {error}")));
            result.and(restore)?;
        } else {
            result?;
        }
    }
    Ok(())
}

fn apply_one(conn: &mut Connection, version: i64, sql: &str) -> Result<()> {
    let tx = conn.transaction().map_err(|error| {
        NarouError::Platform(format!("sqlite begin migration {version}: {error}"))
    })?;
    tx.execute_batch(sql)
        .map_err(|error| NarouError::Platform(format!("sqlite migration {version}: {error}")))?;
    tx.pragma_update(None, "user_version", version)
        .map_err(|error| {
            NarouError::Platform(format!("sqlite set user_version {version}: {error}"))
        })?;
    tx.commit().map_err(|error| {
        NarouError::Platform(format!("sqlite commit migration {version}: {error}"))
    })?;
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
        assert_eq!(version, 12);
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

    /// Seed a database as it would look after applying migrations 0001–0006,
    /// including novels rows with populated child tables.
    fn v6_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for &(version, sql) in MIGRATIONS {
            if version > 6 {
                break;
            }
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 6).unwrap();
        // Three novels exercising nullable columns, Japanese text, embedded
        // whitespace, and distinct values in the five columns whose physical
        // order differs between the old table and the 0007 rebuild
        // (status_sort / convert_failure / extra_fields_json /
        // extra_fields_bytes / extra_fields_yaml).
        conn.execute_batch(
            "INSERT INTO novels (
                 id, author, author_fold, title, title_fold, file_title,
                 toc_url, toc_url_fold, sitename, sitename_fold,
                 last_update, new_arrivals_date, general_firstup,
                 novelupdated_at, general_lastup, last_mail_date,
                 tags_json, tags_fold, tags_sort, ncode, ncode_fold,
                 domain, domain_fold, general_all_no, length,
                 last_check_date,
                 status_sort, convert_failure, extra_fields_json,
                 extra_fields_yaml, extra_fields_bytes
             ) VALUES
             (1, '山田 太郎', '山田 太郎', 'テスト小説', 'テスト小説', 'テスト小説',
              'https://example.com/n/1/', 'https://example.com/n/1/', '小説家になろう', '小説家になろう',
              '2024-01-02T03:04:05Z', '2024-01-01T00:00:00Z', '2023-12-01T00:00:00Z',
              '2024-01-02T03:04:05Z', '2024-01-02T03:04:05Z', '2024-01-03T00:00:00Z',
              '[\"end\",\"404\"]', 'end 404', 'end 404', 'N1234AB', 'n1234ab',
              'example.com', 'example.com', 120, 345678,
              '2024-01-04T00:00:00Z',
              '完結, 削除', 1, '{\"k\":\"v\"}', 'k: v', 0),
             (2, '空白 あり 著者', '空白 あり 著者', '白い 小説', '白い 小説', '白い小説',
              'https://example.com/n/2/', 'https://example.com/n/2/', 'カクヨム', 'カクヨム',
              '2024-02-01T00:00:00Z', NULL, NULL, NULL, NULL, NULL,
              '[]', '', '', NULL, NULL, NULL, NULL, NULL, NULL, NULL,
              '', 0, '{}', '{}', 2),
             (3, 'c', 'c', 'd', 'd', 'd',
              'https://example.com/n/3/', 'https://example.com/n/3/', 'e', 'e',
              '2024-03-01T00:00:00Z', NULL, NULL, NULL, NULL, NULL,
              '[\"a\"]', 'a', 'a', 'X9', 'x9', 'example.org', 'example.org', 7, 42, NULL,
              '中断', 0, '{\"n\":3}', 'n: 3', 1);
             INSERT INTO novel_tags (novel_id, position, tag, tag_fold) VALUES
                 (1, 0, 'end', 'end'),
                 (1, 1, '404', '404'),
                 (3, 0, 'a', 'a');
             INSERT INTO frozen_novels (novel_id) VALUES (1);
             INSERT INTO novel_outputs (novel_id, kind, payload, updated_at) VALUES
                 (1, 'converted_text', X'0102', '2024-01-05T00:00:00Z');
             INSERT INTO novel_sections (novel_id, idx, subtitle, body_yaml) VALUES
                 (1, '０ プロローグ', '始まり', 'body: 本文テキスト'),
                 (3, '1', NULL, 'body: x');
             INSERT INTO novel_versions (id, novel_id, parent_id, origin, note, created_at) VALUES
                 (1, 1, NULL, 'import', NULL, '2024-01-05T00:00:00Z');
             INSERT INTO novel_version_sections (version_id, idx, subtitle, body_yaml) VALUES
                 (1, '０ プロローグ', '始まり', 'body: 本文テキスト');
             INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES
                 (1, NULL, '--- a\n+++ b');",
        )
        .unwrap();
        conn
    }

    fn child_row_counts(conn: &Connection) -> Vec<i64> {
        [
            "novel_tags",
            "frozen_novels",
            "novel_outputs",
            "novel_sections",
            "novel_versions",
            "novel_version_sections",
            "novel_version_diffs",
        ]
        .iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .collect()
    }

    /// Regression: applying migrations past 0006 on a populated database must
    /// preserve every novels column (0007 previously copied `SELECT *` into a
    /// table whose tail column order differs, so STRICT type checks failed or
    /// values would have been silently swapped) and must keep all child rows
    /// (the DROP TABLE cascade had wiped novel_tags & friends).
    #[test]
    fn upgrade_from_v6_preserves_novels_and_children() {
        let mut conn = v6_fixture();
        let before_children = child_row_counts(&conn);
        assert_eq!(before_children, vec![3, 1, 1, 2, 1, 1, 1]);

        apply(&mut conn).unwrap();

        // Every child table survives the novels rebuild.
        assert_eq!(child_row_counts(&conn), before_children);

        // The five tail columns land under their correct names. Cast
        // everything to text so a residual column-order mixup (e.g. a JSON
        // string sitting in an INTEGER column) still fails the assertion
        // instead of passing through an implicit conversion.
        fn column(conn: &Connection, column: &str) -> Vec<Option<String>> {
            conn.prepare(&format!(
                "SELECT CAST({column} AS TEXT) FROM novels ORDER BY id"
            ))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
        }
        assert_eq!(
            column(&conn, "status_sort"),
            vec![
                Some("完結, 削除".to_string()),
                Some(String::new()),
                Some("中断".to_string())
            ]
        );
        assert_eq!(
            column(&conn, "convert_failure"),
            vec![
                Some("1".to_string()),
                Some("0".to_string()),
                Some("0".to_string())
            ]
        );
        assert_eq!(
            column(&conn, "extra_fields_json"),
            vec![
                Some("{\"k\":\"v\"}".to_string()),
                Some("{}".to_string()),
                Some("{\"n\":3}".to_string())
            ]
        );
        assert_eq!(
            column(&conn, "extra_fields_yaml"),
            vec![
                Some("k: v".to_string()),
                Some("{}".to_string()),
                Some("n: 3".to_string())
            ]
        );
        assert_eq!(
            column(&conn, "extra_fields_bytes"),
            vec![
                Some("0".to_string()),
                Some("2".to_string()),
                Some("1".to_string())
            ]
        );
        assert_eq!(
            column(&conn, "last_check_date"),
            vec![Some("2024-01-04T00:00:00Z".to_string()), None, None]
        );
        assert_eq!(
            column(&conn, "new_arrivals_date"),
            vec![Some("2024-01-01T00:00:00Z".to_string()), None, None]
        );

        // Child rows still point at their novels after the rebuild.
        let tags: Vec<(i64, String)> = conn
            .prepare("SELECT novel_id, tag FROM novel_tags ORDER BY novel_id, position")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(
            tags,
            vec![
                (1, "end".to_string()),
                (1, "404".to_string()),
                (3, "a".to_string())
            ]
        );

        // 0007 must recreate the index 0003 established, not the pre-0003
        // (suspend, "end", id) definition it replaced.
        let index_columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_info('novels_status_sort_idx') ORDER BY seqno")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(index_columns, vec!["status_sort", "id"]);

        // toc_url is no longer UNIQUE (the reason for the rebuild).
        let unique_indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_index_list('novels') i
                 WHERE i.[unique] = 1 AND EXISTS (
                     SELECT 1 FROM pragma_index_info(i.name) WHERE name = 'toc_url')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unique_indexes, 0);

        // The runner's foreign_keys=OFF around 0007 must be restored after
        // the rebuild, not left disabled on the shared connection.
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
    }
}
