//! Regression coverage for the 0007 novels-table rebuild and for keeping the
//! two migration directories (`worker_entry/migrations` for D1 and
//! `src/native/sqlite/migrations` for rusqlite) in lockstep.

#![cfg(feature = "native-runtime")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every SQL file in a directory, keyed by file name.
fn migration_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (name, bytes)
        })
        .collect()
}

/// The schema is ported between D1 and rusqlite by keeping the SQL files
/// identical. Files that exist under the same name in both directories must
/// be byte-for-byte equal; shared migrations that were renumbered on one
/// backend (D1 gained worker_jobs/job_leases between 0004 and 0007, native
/// gained content/versions there) must likewise stay identical under their
/// mapped names, and every backend-only file must be declared below so a new
/// file cannot silently drift or duplicate under a mismatched name.
#[test]
fn migration_directories_stay_in_sync() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let worker = migration_files(&root.join("worker_entry/migrations"));
    let native = migration_files(&root.join("src/native/sqlite/migrations"));

    // Same logical migration under different numeric prefixes:
    // worker name → native name.
    const RENAMED: &[(&str, &str)] = &[
        ("0009_requires_login.sql", "0011_requires_login.sql"),
        ("0010_login_session.sql", "0012_login_session.sql"),
    ];
    const WORKER_ONLY: &[&str] = &["0005_worker_jobs.sql", "0006_job_leases.sql"];
    const NATIVE_ONLY: &[&str] = &[
        "0005_content.sql",
        "0006_versions.sql",
        "0009_section_bodies.sql",
        "0010_drop_body_yaml.sql",
    ];

    let renamed_worker: Vec<&str> = RENAMED.iter().map(|(worker, _)| *worker).collect();
    let renamed_native: Vec<&str> = RENAMED.iter().map(|(_, native)| *native).collect();

    for name in worker.keys() {
        if WORKER_ONLY.contains(&name.as_str()) {
            assert!(
                !native.contains_key(name),
                "worker-only migration {name} also exists on native; update the test"
            );
            continue;
        }
        let native_name = renamed_worker
            .iter()
            .position(|n| *n == name)
            .map(|idx| renamed_native[idx])
            .unwrap_or(name.as_str());
        assert_eq!(
            worker.get(name),
            native.get(native_name),
            "migration {name} differs from src/native/sqlite/migrations/{native_name}"
        );
    }
    for name in native.keys() {
        if NATIVE_ONLY.contains(&name.as_str()) {
            assert!(
                !worker.contains_key(name),
                "native-only migration {name} also exists on worker; update the test"
            );
            continue;
        }
        let worker_name = renamed_native
            .iter()
            .position(|n| *n == name)
            .map(|idx| renamed_worker[idx])
            .unwrap_or(name.as_str());
        assert!(
            worker.contains_key(worker_name),
            "native migration {name} has no worker counterpart {worker_name}"
        );
    }
}

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
    let conn = conn.lock().unwrap();

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

    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 12);
}
