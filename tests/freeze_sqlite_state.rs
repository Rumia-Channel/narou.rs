//! Freeze state must live in the SQLite store (issue #35).
//!
//! Builds before the fix wrote `.narou/freeze.yaml` directly even in SQLite
//! mode, and the next launch imported that fragment over the stored payload:
//! previously frozen novels lost their state, `frozen_novels` stayed empty and
//! a new `freeze.yaml.imported-<unix_ts>` appeared on every run.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use narou_rs::db::Database;
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::native::sqlite::state::{self, StorageMode};

const GOLDEN_DATABASE: &str = "tests/golden/library-a/.narou/database.yaml";

/// Temp library holding the golden records and the SQLite storage marker, with
/// no freeze state yet — each test writes the state it needs.
fn prepare_library() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let narou_dir = temp.path().join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_DATABASE);
    std::fs::copy(golden, narou_dir.join("database.yaml")).unwrap();
    state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
    temp
}

fn with_connection<T>(root: &Path, f: impl FnOnce(&rusqlite::Connection) -> T) -> T {
    let conn = narou_rs::native::sqlite::open(&root.join(".narou").join("db.sqlite")).unwrap();
    let guard = conn.lock().unwrap();
    f(&guard)
}

fn frozen_rows(root: &Path) -> Vec<i64> {
    with_connection(root, |conn| {
        let mut statement = conn
            .prepare("SELECT novel_id FROM frozen_novels ORDER BY novel_id")
            .unwrap();
        let rows = statement.query_map([], |row| row.get::<_, i64>(0)).unwrap();
        rows.map(|row| row.unwrap()).collect()
    })
}

/// The shared status search expression reports 凍結 from `frozen_novels` alone,
/// so this is what `narou list <status filter>` / the Web UI search rely on.
fn status_search_frozen(root: &Path, id: i64) -> bool {
    let expression = include_str!("../src/native/sqlite/sql/status_search_expression.sql");
    with_connection(root, |conn| {
        conn.query_row(
            &format!(
                "SELECT instr({expression}, '凍結') > 0 FROM novels n WHERE n.id = ?1"
            ),
            [id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            != 0
    })
}

fn stored_freeze_payload(root: &Path) -> Option<String> {
    with_connection(root, |conn| {
        conn.query_row(
            "SELECT value_yaml FROM app_state WHERE scope = 'inv' AND key = 'freeze'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
    })
}

fn stored_freeze_ids(root: &Path) -> BTreeSet<i64> {
    let frozen: HashMap<i64, serde_yaml::Value> = Inventory::new(root.to_path_buf())
        .load("freeze", InventoryScope::Local)
        .unwrap();
    frozen.into_keys().collect()
}

/// `freeze.yaml.imported-*` names left behind by the legacy import.
fn fragments(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join(".narou"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("freeze.yaml.imported-"))
        .collect();
    names.sort();
    names
}

fn freeze(root: &Path, id: i64, on: bool) {
    let inventory = Inventory::new(root.to_path_buf());
    inventory
        .update_yaml::<(), HashMap<i64, serde_yaml::Value>, _>(
            "freeze",
            InventoryScope::Local,
            |mut frozen| {
                if on {
                    frozen.insert(id, serde_yaml::Value::Bool(true));
                } else {
                    frozen.remove(&id);
                }
                Ok((frozen, ()))
            },
        )
        .unwrap();
}

/// The CLI entry point for freeze, which the reporter exercised: it must not
/// create a `freeze.yaml` fragment, and the state must survive a restart.
#[test]
fn cli_freeze_keeps_other_novels_frozen_across_restarts() {
    let temp = prepare_library();
    let root = temp.path();
    std::fs::write(root.join(".narou").join("freeze.yaml"), "1: true\n").unwrap();

    run_cli(root, &["list"]);
    assert_eq!(stored_freeze_ids(root), BTreeSet::from([1]));
    assert_eq!(fragments(root).len(), 1, "the legacy file is imported once");

    // The reporter's step: freeze a second novel. Pre-fix builds wrote a
    // fragment holding only that id, which the next launch imported.
    run_cli(root, &["freeze", "2"]);
    assert!(
        !root.join(".narou").join("freeze.yaml").exists(),
        "SQLite mode must not write freeze.yaml"
    );

    run_cli(root, &["list"]);

    assert_eq!(stored_freeze_ids(root), BTreeSet::from([1, 2]));
    assert_eq!(frozen_rows(root), vec![1, 2]);
    assert!(status_search_frozen(root, 2));
    assert_eq!(
        fragments(root).len(),
        1,
        "no new .imported- fragment per freeze"
    );

    // Repeating the sequence stays stable: `freeze 2` toggles the state off.
    run_cli(root, &["freeze", "2"]);
    run_cli(root, &["list"]);
    assert_eq!(stored_freeze_ids(root), BTreeSet::from([1]));
    assert_eq!(frozen_rows(root), vec![1]);
    assert_eq!(fragments(root).len(), 1);
}

#[test]
fn freeze_writes_reach_the_store_without_touching_legacy_files() {
    let temp = prepare_library();
    let root = temp.path().to_path_buf();
    std::fs::write(root.join(".narou").join("freeze.yaml"), "1: true\n").unwrap();

    let _db = Database::with_root(root.clone()).unwrap();
    assert_eq!(stored_freeze_ids(&root), BTreeSet::from([1]));
    assert_eq!(
        frozen_rows(&root),
        vec![1],
        "the imported payload must be projected onto frozen_novels"
    );
    assert!(status_search_frozen(&root, 1));
    assert_eq!(fragments(&root).len(), 1);

    freeze(&root, 2, true);
    freeze(&root, 1, false);

    assert_eq!(stored_freeze_ids(&root), BTreeSet::from([2]));
    assert_eq!(
        frozen_rows(&root),
        vec![2],
        "unfreezing must remove the projected row too"
    );
    assert!(!status_search_frozen(&root, 1));
    assert!(
        !root.join(".narou").join("freeze.yaml").exists(),
        "SQLite mode must not write freeze.yaml"
    );
    assert_eq!(fragments(&root).len(), 1, "no new .imported- fragment");
}

/// Upgrade path: a fragment written by a pre-fix build must be imported as a
/// union with the stored payload, never as a replacement.
#[test]
fn a_stray_fragment_is_imported_as_a_union() {
    let temp = prepare_library();
    let root = temp.path().to_path_buf();
    let narou_dir = root.join(".narou");

    with_connection(&root, |conn| {
        conn.execute_batch(
            "INSERT INTO app_state (scope, key, value_json, value_yaml)
             VALUES ('inv', 'freeze', '{}', '1: true')",
        )
        .unwrap();
    });
    std::fs::write(narou_dir.join("freeze.yaml"), "2: true\n").unwrap();

    let _db = Database::with_root(root.clone()).unwrap();

    assert_eq!(stored_freeze_ids(&root), BTreeSet::from([1, 2]));
    assert_eq!(frozen_rows(&root), vec![1, 2]);
    assert_eq!(fragments(&root).len(), 1);
    assert!(!narou_dir.join("freeze.yaml").exists());
}

/// Libraries already affected by the bug keep the payload but an empty
/// `frozen_novels` table; opening the database rebuilds the projection.
#[test]
fn frozen_novels_is_rebuilt_from_the_stored_payload() {
    let temp = prepare_library();
    let root = temp.path().to_path_buf();

    with_connection(&root, |conn| {
        conn.execute_batch(
            "INSERT INTO app_state (scope, key, value_json, value_yaml)
             VALUES ('inv', 'freeze', '{}', '1: true\n2: true')",
        )
        .unwrap();
    });

    let _db = Database::with_root(root.clone()).unwrap();

    assert_eq!(frozen_rows(&root), vec![1, 2]);
    assert!(status_search_frozen(&root, 1));
    assert!(status_search_frozen(&root, 2));
    assert_eq!(stored_freeze_payload(&root).unwrap().trim(), "1: true\n2: true");
}

/// `narou db repair-freeze` recovers the ids that only survive in the
/// fragments the old builds left behind.
#[test]
fn cli_repair_freeze_restores_ids_lost_to_fragments() {
    let temp = prepare_library();
    let root = temp.path();
    let narou_dir = root.join(".narou");

    with_connection(root, |conn| {
        conn.execute_batch(
            "INSERT INTO app_state (scope, key, value_json, value_yaml)
             VALUES ('inv', 'freeze', '{}', '2: true')",
        )
        .unwrap();
    });
    std::fs::write(narou_dir.join("freeze.yaml.imported-100"), "1: true\n").unwrap();
    std::fs::write(narou_dir.join("freeze.yaml.imported-200"), "2: true\n").unwrap();

    let dry_run = run_cli(root, &["db", "repair-freeze", "--dry-run"]);
    assert!(dry_run.contains("復旧予定: 1 件"), "{dry_run}");
    assert!(dry_run.contains("Alpha物語"), "{dry_run}");
    assert_eq!(
        stored_freeze_ids(root),
        BTreeSet::from([2]),
        "--dry-run must not change the stored state"
    );

    let repaired = run_cli(root, &["db", "repair-freeze"]);
    assert!(repaired.contains("1 件の凍結状態を復旧しました"), "{repaired}");
    assert_eq!(stored_freeze_ids(root), BTreeSet::from([1, 2]));
    assert_eq!(frozen_rows(root), vec![1, 2]);
    assert!(status_search_frozen(root, 1));

    // Idempotent: nothing left to do, and the fragments are kept as-is.
    let again = run_cli(root, &["db", "repair-freeze"]);
    assert!(again.contains("修復対象なし"), "{again}");
    assert_eq!(fragments(root).len(), 2);
}

/// YAML mode keeps narou.rb compatibility: the freeze command must still
/// maintain `.narou/freeze.yaml` and never create a database.
#[test]
fn yaml_mode_freeze_still_writes_freeze_yaml() {
    let temp = prepare_library();
    let root = temp.path();
    std::fs::remove_file(root.join(".narou").join("storage-backend")).unwrap();

    run_cli(root, &["freeze", "2"]);
    let freeze_yaml = std::fs::read_to_string(root.join(".narou").join("freeze.yaml")).unwrap();
    assert!(freeze_yaml.contains("2: true"), "{freeze_yaml}");
    assert!(!root.join(".narou").join("db.sqlite").exists());

    run_cli(root, &["freeze", "1"]);
    let freeze_yaml = std::fs::read_to_string(root.join(".narou").join("freeze.yaml")).unwrap();
    assert!(
        freeze_yaml.contains("1: true") && freeze_yaml.contains("2: true"),
        "{freeze_yaml}"
    );

    // Toggling the same id off rewrites the file without it.
    run_cli(root, &["freeze", "2"]);
    let freeze_yaml = std::fs::read_to_string(root.join(".narou").join("freeze.yaml")).unwrap();
    assert!(
        freeze_yaml.contains("1: true") && !freeze_yaml.contains("2: true"),
        "{freeze_yaml}"
    );
    assert!(fragments(root).is_empty());
}

fn run_cli(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("narou_rs must run");
    assert!(
        output.status.success(),
        "narou {} failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
