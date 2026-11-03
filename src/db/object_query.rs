//! Indexed object metadata pages shared by Native SQLite and Worker D1.

/// Exact prefix key plus descendants, strictly after the last returned key.
/// Separate ranges avoid the optional-prefix OR that forces a full index scan.
/// The caller reads limit + 1 rows to determine whether another page exists.
/// Binds: exact prefix, descendant lower bound, upper bound, cursor, row cap.
pub const LIST_PAGE_SQL: &str = r#"
SELECT object_key, size, content_type, updated_at FROM objects
WHERE object_key = ?1 AND object_key > ?4
UNION ALL
SELECT object_key, size, content_type, updated_at FROM objects
WHERE ?1 != '' AND object_key >= max(?2, ?4) AND object_key < ?3 AND object_key > ?4
UNION ALL
SELECT object_key, size, content_type, updated_at FROM objects
WHERE ?1 = '' AND object_key > ?4
ORDER BY object_key LIMIT ?5
"#;

#[cfg(all(test, feature = "native-runtime"))]
mod tests {
    use super::*;
    use crate::platform::prefix_upper_bound;
    use rusqlite::{Connection, StatementStatus, params};

    #[test]
    fn pages_seek_past_history_and_preserve_prefix_boundaries() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE objects(object_key TEXT PRIMARY KEY, size INTEGER, content_type TEXT, updated_at TEXT);
             WITH RECURSIVE ids(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<30000)
             INSERT INTO objects SELECT printf('other/%06d',i),1,NULL,'now' FROM ids;
             WITH RECURSIVE ids(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<1000)
             INSERT INTO objects SELECT printf('novels/book/%06d',i),1,NULL,'now' FROM ids;
             INSERT INTO objects VALUES ('novels/book',1,NULL,'now'),('novels/bookish/1',1,NULL,'now'),
                 ('novels/book/本文',1,NULL,'now'),('\u{10ffff}tail',1,NULL,'now');",
        ).unwrap();
        let page = |prefix: &str, cursor: &str, cap: i64| {
            let lower = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
            let mut statement = conn.prepare(LIST_PAGE_SQL).unwrap();
            let rows = statement.query_map(params![prefix, lower, prefix_upper_bound(&lower), cursor, cap],
                |row| row.get::<_, String>(0)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap();
            assert!(statement.get_status(StatementStatus::VmStep) < 2000);
            rows
        };
        assert_eq!(page("novels/book", "", 2), vec!["novels/book", "novels/book/000001"]);
        assert_eq!(page("novels/book", "novels/book/000998", 4),
            vec!["novels/book/000999", "novels/book/001000", "novels/book/本文"]);
        assert_eq!(page("", "other/029998", 3), vec!["other/029999", "other/030000", "\u{10ffff}tail"]);
        conn.execute("DELETE FROM objects WHERE object_key='novels/book/000998'", []).unwrap();
        assert_eq!(page("novels/book", "novels/book/000998", 1), vec!["novels/book/000999"]);
    }
}
