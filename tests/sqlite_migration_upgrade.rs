//! Regression coverage for upgrading existing SQLite libraries without data loss.

#![cfg(feature = "native-runtime")]

use std::path::{Path, PathBuf};

use narou_rs::platform::ObjectStore;

/// Build a database exactly as migrations 0001–0006 leave it (the `SELECT *`
/// column-order bug only bites once ALTER-added columns exist), seed novels
/// plus every child table, then run the real open() upgrade path.
#[test]
fn open_upgrades_v6_library_without_data_loss() {
    let migrations_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/native/sqlite/migrations");
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("db.sqlite");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    for name in [
        "0001_core.sql",
        "0002_search.sql",
        "0003_status_sort.sql",
        "0004_extra_fields_yaml.sql",
        "0005_content.sql",
        "0006_versions.sql",
    ] {
        conn.execute_batch(&std::fs::read_to_string(migrations_dir.join(name)).unwrap())
            .unwrap();
    }
    conn.pragma_update(None, "user_version", 6).unwrap();
    conn.execute_batch(
        "INSERT INTO novels (
             id, author, author_fold, title, title_fold, file_title,
             toc_url, toc_url_fold, sitename, sitename_fold, last_update,
             status_sort, convert_failure, extra_fields_json,
             extra_fields_yaml, extra_fields_bytes
         ) VALUES
         (1, '山田 太郎', '山田 太郎', 'テスト 小説', 'テスト 小説', 'テスト小説',
          'https://example.com/n/1/', 'https://example.com/n/1/',
          '小説家になろう', '小説家になろう', '2024-01-02T03:04:05Z',
          '完結, 削除', 1, '{\"k\":\"v\"}', 'k: v', 0),
         (2, 'b', 'b', 'c', 'c', 'c',
          'https://example.com/n/2/', 'https://example.com/n/2/',
          'd', 'd', '2024-02-01T00:00:00Z', '', 0, '{}', '{}', 2);
         INSERT INTO novel_tags (novel_id, position, tag, tag_fold)
             VALUES (1, 0, 'end', 'end');
         INSERT INTO frozen_novels (novel_id) VALUES (1);
         INSERT INTO novel_outputs (novel_id, kind, payload, updated_at)
             VALUES (1, 'converted_text', X'0102', '2024-01-05T00:00:00Z');
         INSERT INTO novel_sections (novel_id, idx, subtitle, body_yaml)
             VALUES (1, '０ プロローグ', '始まり', 'body: 本文テキスト');
         INSERT INTO novel_versions (id, novel_id, parent_id, origin, note, created_at)
             VALUES (1, 1, NULL, 'import', NULL, '2024-01-05T00:00:00Z');
         INSERT INTO novel_version_sections (version_id, idx, subtitle, body_yaml)
             VALUES (1, '０ プロローグ', '始まり', 'body: 本文テキスト');
         INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff)
             VALUES (1, NULL, '--- a\n+++ b');",
    )
    .unwrap();
    drop(conn);

    // The upgrade path every existing library takes on next launch.
    let conn = narou_rs::native::sqlite::open(&db_path).unwrap();
    let conn = conn.lock();

    let novels: Vec<(i64, String, i64, String, String, i64)> = conn
        .prepare(
            "SELECT id, status_sort, convert_failure, extra_fields_json,
                    extra_fields_yaml, extra_fields_bytes
             FROM novels ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(
        novels,
        vec![
            (
                1,
                "完結, 削除".to_string(),
                1,
                "{\"k\":\"v\"}".to_string(),
                "k: v".to_string(),
                0
            ),
            (2, "".to_string(), 0, "{}".to_string(), "{}".to_string(), 2),
        ]
    );

    // The rebuild's whole point: duplicate toc_url rows are legal afterwards.
    conn.execute(
        "INSERT INTO novels (
             author, author_fold, title, title_fold, file_title,
             toc_url, toc_url_fold, sitename, sitename_fold, last_update
         ) VALUES ('dup', 'dup', 'dup', 'dup', 'dup',
                   'https://example.com/n/1/', 'https://example.com/n/1/',
                   'd', 'd', '2024-03-01T00:00:00Z')",
        [],
    )
    .unwrap();
    for (table, expected) in [
        ("novel_tags", 1),
        ("frozen_novels", 1),
        ("novel_outputs", 1),
        ("novel_sections", 1),
        ("novel_versions", 1),
        ("novel_version_sections", 1),
        ("novel_version_diffs", 1),
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, expected, "{table} lost rows during the 0007 rebuild");
    }

    let index_columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_index_info('novels_status_sort_idx') ORDER BY seqno")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(index_columns, vec!["status_sort", "id"]);

}

/// The 0009 rebuild (`DROP TABLE objects`) justified itself with "the
/// filesystem mirror always holds the same bytes". That premise is false for
/// the supported `sqlite.mirror-files=false` configuration (SORAHOST) and for
/// any key whose mirror file disappeared: objects that only exist in the
/// database must survive the upgrade.
#[test]
fn open_upgrades_v8_objects_without_losing_db_only_rows() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("db.sqlite");
    let archive_root = temp.path().join("小説データ");
    std::fs::create_dir_all(&archive_root).unwrap();

    let conn = v8_objects_database(&db_path);
    // A body that exists only in the database (the mirror is disabled).
    let store = narou_rs::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
        std::sync::Arc::new(parking_lot::Mutex::new(conn)),
        narou_rs::native::object_store::NativeObjectStore::from_root(archive_root.clone()).unwrap(),
        false,
    );
    let key =
        narou_rs::platform::ObjectKey::try_new("novels/site/fixture/本文/1 第一話.yaml").unwrap();
    futures::executor::block_on(store.write_small(&key, b"body: db only".to_vec())).unwrap();
    drop(store);
    let mirror_files: usize = walkdir_yaml_count(&archive_root);
    assert_eq!(mirror_files, 0, "前提: 実ファイルのミラーは存在しない");

    // The upgrade path every library takes on next launch.
    let conn = narou_rs::native::sqlite::open(&db_path).unwrap();
    let store = narou_rs::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
        conn,
        narou_rs::native::object_store::NativeObjectStore::from_root(archive_root).unwrap(),
        false,
    );
    assert_eq!(futures::executor::block_on(store.read_small(&key)).unwrap(),
        Some(b"body: db only".to_vec()));
}

fn v8_objects_database(path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/native/sqlite/migrations");
    for name in [
        "0001_core.sql", "0002_search.sql", "0003_status_sort.sql", "0004_extra_fields_yaml.sql",
        "0005_content.sql", "0006_versions.sql", "0007_toc_url_not_unique.sql", "0008_objects.sql",
    ] {
        conn.execute_batch(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap();
    }
    conn.pragma_update(None, "user_version", 8).unwrap();
    conn
}

fn replace_with_legacy_objects(conn: &rusqlite::Connection, encoding_column: bool) {
    conn.execute_batch(
        "DROP TABLE object_chunks; DROP TABLE objects;
         CREATE TABLE objects (
             object_key TEXT PRIMARY KEY, size INTEGER NOT NULL, updated_at TEXT NOT NULL,
             content_type TEXT, data TEXT
         ) WITHOUT ROWID;
         CREATE TABLE object_chunks (
             object_key TEXT NOT NULL REFERENCES objects(object_key) ON DELETE CASCADE,
             seq INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY (object_key, seq)
         ) WITHOUT ROWID;",
    ).unwrap();
    if encoding_column {
        conn.execute_batch("ALTER TABLE objects ADD COLUMN encoding TEXT NOT NULL DEFAULT 'none'").unwrap();
    }
}

#[test]
fn open_upgrades_legacy_inline_and_chunked_payloads_without_mirrors() {
    use base64::Engine;
    for deflate in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db.sqlite");
        let conn = v8_objects_database(&path);
        replace_with_legacy_objects(&conn, deflate);
        let mut state = 0x12345678u32;
        let payload: Vec<u8> = (0..600_000).map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        }).collect();
        for (key, data) in [("novels/site/title/toc.yaml", "body: 本文".as_bytes()),
            ("novels/site/title/挿絵/image.png", payload.as_slice())] {
            let stored = if deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else { data.to_vec() };
            let encoded = base64::engine::general_purpose::STANDARD.encode(stored);
            let inline = data.len() < 512 * 1024;
            conn.execute(
                "INSERT INTO objects (object_key, size, updated_at, data) VALUES (?, ?, '2026-01-01T00:00:00Z', ?)",
                rusqlite::params![key, data.len() as i64, inline.then_some(encoded.as_str())],
            ).unwrap();
            if deflate {
                conn.execute("UPDATE objects SET encoding = 'deflate' WHERE object_key = ?", [key]).unwrap();
            }
            if !inline {
                for (seq, chunk) in encoded.as_bytes().chunks(512 * 1024).enumerate() {
                    conn.execute("INSERT INTO object_chunks VALUES (?, ?, ?)",
                        rusqlite::params![key, seq as i64, std::str::from_utf8(chunk).unwrap()]).unwrap();
                }
            }
        }
        drop(conn);
        let upgraded = narou_rs::native::sqlite::open(&path).unwrap();
        let store = narou_rs::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
            upgraded,
            narou_rs::native::object_store::NativeObjectStore::from_root(temp.path().join("archive")).unwrap(),
            false,
        );
        for (key, expected) in [("novels/site/title/toc.yaml", "body: 本文".as_bytes()),
            ("novels/site/title/挿絵/image.png", payload.as_slice())] {
            let key = narou_rs::platform::ObjectKey::try_new(key).unwrap();
            assert_eq!(futures::executor::block_on(store.read_small(&key)).unwrap().as_deref(),
                Some(expected), "legacy deflate={deflate}, key={key}");
        }
    }
}

#[test]
fn corrupt_legacy_payload_rolls_back_the_whole_object_migration() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    let conn = v8_objects_database(&path);
    replace_with_legacy_objects(&conn, false);
    conn.execute_batch(
        "INSERT INTO objects (object_key, size, updated_at, data) VALUES
         ('novels/a/toc.yaml', 2, '2026-01-01T00:00:00Z', 'b2s='),
         ('novels/b/toc.yaml', 3, '2026-01-01T00:00:00Z', 'invalid!');",
    ).unwrap();
    drop(conn);
    assert!(narou_rs::native::sqlite::open(&path).is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), 8);
    assert_eq!(conn.query_row("SELECT data FROM objects WHERE object_key = 'novels/a/toc.yaml'",
        [], |row| row.get::<_, String>(0)).unwrap(), "b2s=");
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM pragma_table_info('objects') WHERE name = 'crc32'",
        [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

fn walkdir_yaml_count(root: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                count += 1;
            }
        }
    }
    count
}
