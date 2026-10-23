//! Authoritative native record mutations and SQLite/YAML re-entry.
#![cfg(feature = "native-runtime")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use narou_rs::db::{Database, NovelRecord};
use narou_rs::native::sqlite::state::{self, StorageMode};

fn library() -> tempfile::TempDir {
    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/sqlite-record-consistency");
    std::fs::create_dir_all(&scratch).unwrap();
    let temp = tempfile::Builder::new().prefix("library-").tempdir_in(scratch).unwrap();
    std::fs::create_dir_all(temp.path().join(".narousetting")).unwrap();
    state::write_mode(&temp.path().join(".narou"), StorageMode::Sqlite).unwrap();
    temp
}

fn record(id: i64) -> NovelRecord {
    serde_json::from_value(serde_json::json!({
        "id": id, "author": "author", "title": format!("title-{id}"),
        "file_title": format!("title-{id}"), "toc_url": format!("https://example.com/{id}/"),
        "sitename": "site", "last_update": "2026-01-01T00:00:00Z"
    })).unwrap()
}

fn seed(root: &Path) -> Database {
    let mut db = Database::with_root(root.to_path_buf()).unwrap();
    db.update_records(|mut records| {
        records.insert(1, record(1));
        records.insert(2, record(2));
        Ok((records, ()))
    }).unwrap();
    db
}

#[test]
fn stale_mutation_preserves_current_fields_new_rows_children_and_deletes() {
    let temp = library();
    let mut first = seed(temp.path());
    let mut stale = Database::with_root(temp.path().to_path_buf()).unwrap();
    first.update_records(|mut records| {
        records.get_mut(&1).unwrap().tags.push("current".into());
        records.get_mut(&2).unwrap().author = "current author".into();
        records.get_mut(&2).unwrap().extra_fields.insert("future".into(), serde_yaml::from_str("nested: [1, 2]").unwrap());
        records.insert(3, record(3));
        Ok((records, ()))
    }).unwrap();
    let state = state::active_for(&temp.path().join(".narou")).unwrap();
    {
        let conn = state.conn_ref().lock();
        let hash = narou_rs::native::sqlite::content::store_body(&conn, "body: intact").unwrap();
        conn.execute("INSERT INTO novel_sections (novel_id, idx, subtitle, body_hash) VALUES (3, '1', 'chapter', ?)", [&hash]).unwrap();
        conn.execute_batch("INSERT INTO novel_versions (id, novel_id, parent_id, origin, note, created_at) VALUES (1, 3, NULL, 'import', NULL, '2026-01-01T00:00:00Z');").unwrap();
        conn.execute("INSERT INTO novel_version_sections (version_id, idx, subtitle, body_hash) VALUES (1, '1', 'chapter', ?)", [&hash]).unwrap();
    }
    stale.update_records(|mut records| {
        assert_eq!(records[&1].tags, vec!["current"], "closure must see current same-row fields");
        records.get_mut(&1).unwrap().length = Some(42);
        records.remove(&2);
        Ok((records, ()))
    }).unwrap();
    let reopened = Database::with_root(temp.path().to_path_buf()).unwrap();
    assert_eq!(reopened.get(1).unwrap().tags, vec!["current"]);
    assert_eq!(reopened.get(1).unwrap().length, Some(42));
    assert!(reopened.get(2).is_none(), "intentional deletion remains effective");
    assert!(reopened.get(3).is_some(), "stale cache cannot delete newly published novel");
    let count: i64 = state.conn_ref().lock().query_row("SELECT count(*) FROM novel_sections WHERE novel_id = 3", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);
    let count: i64 = state.conn_ref().lock().query_row("SELECT count(*) FROM novel_version_sections WHERE version_id = 1", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);
    let sections = narou_rs::native::sqlite::content::load_sections(&state.conn_ref().lock(), 3).unwrap().unwrap();
    assert_eq!(sections["1"].1, "body: intact");
}

#[test]
fn stale_mutation_preserves_other_rows_and_unknown_fields() {
    let temp = library();
    let mut first = seed(temp.path());
    let mut stale = Database::with_root(temp.path().to_path_buf()).unwrap();
    first.update_records(|mut records| {
        records.get_mut(&1).unwrap().extra_fields.insert("future".into(), serde_yaml::from_str("nested: [1, 2]").unwrap());
        records.get_mut(&1).unwrap().tags.push("current".into());
        Ok((records, ()))
    }).unwrap();
    stale.update_records(|mut records| {
        records.get_mut(&2).unwrap().length = Some(42);
        Ok((records, ()))
    }).unwrap();
    assert_eq!(stale.get(1).unwrap().tags, vec!["current"]);
    assert_eq!(stale.get(1).unwrap().extra_fields["future"], serde_yaml::from_str::<serde_yaml::Value>("nested: [1, 2]").unwrap());
}

#[test]
fn bulk_mutation_does_not_rewind_reserved_sequence_and_refreshes_local_allocator() {
    let temp = library();
    let mut db = seed(temp.path());
    let state = state::active_for(&temp.path().join(".narou")).unwrap();
    state.conn_ref().lock().execute_batch("UPDATE novel_id_sequence SET next_id = 100 WHERE id = 1;
        INSERT INTO novel_id_reservations (id, generation, available) VALUES (99, 1, 0);").unwrap();
    db.update_records(|mut records| { records.insert(50, record(50)); Ok((records, ())) }).unwrap();
    let next: i64 = state.conn_ref().lock().query_row("SELECT next_id FROM novel_id_sequence WHERE id = 1", [], |row| row.get(0)).unwrap();
    assert_eq!(next, 100, "bulk writes must not reclaim reserved sequence space");
    let reservation: (i64, i64) = state.conn_ref().lock().query_row("SELECT generation, available FROM novel_id_reservations WHERE id = 99", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(reservation, (1, 0), "unpublished ID ownership must remain intact");
    assert!(db.allocate_id().unwrap().id().0 > 50, "updated cache must reconcile its local allocator");
}

#[test]
fn mutation_holds_sqlite_write_transaction_before_invoking_closure_and_rolls_back_errors() {
    let temp = library();
    let mut db = seed(temp.path());
    let outsider = rusqlite::Connection::open(temp.path().join(".narou/db.sqlite")).unwrap();
    outsider.busy_timeout(std::time::Duration::ZERO).unwrap();
    let error = db.update_records(|mut records| -> narou_rs::error::Result<(BTreeMap<i64, NovelRecord>, ())> {
        assert!(outsider.execute("UPDATE novels SET author = 'racing' WHERE id = 1", []).is_err(),
            "a separate connection must not write between authoritative read and mutation");
        records.remove(&1);
        Err(narou_rs::error::NarouError::Database("reject mutation".into()))
    }).unwrap_err();
    assert!(error.to_string().contains("reject mutation"));
    let reopened = Database::with_root(temp.path().to_path_buf()).unwrap();
    assert_eq!(reopened.get(1).unwrap().author, "author");
}

#[test]
fn save_applies_only_local_field_and_record_deltas() {
    let temp = library();
    let mut stale = seed(temp.path());
    stale.update_records(|mut records| {
        records.get_mut(&1).unwrap().extra_fields.insert("remove_me".into(), serde_yaml::Value::Bool(true));
        Ok((records, ()))
    }).unwrap();
    let mut writer = Database::with_root(temp.path().to_path_buf()).unwrap();
    writer.update_records(|mut records| {
        records.get_mut(&1).unwrap().tags.push("concurrent".into());
        records.get_mut(&1).unwrap().extra_fields.insert("new_key".into(), serde_yaml::Value::Bool(true));
        records.insert(3, record(3));
        Ok((records, ()))
    }).unwrap();
    let local = stale.all_records_mut().get_mut(&1).unwrap();
    local.length = Some(42);
    local.extra_fields.remove("remove_me");
    stale.remove(2);
    stale.insert(record(4));
    stale.save().unwrap();
    assert_eq!(stale.get(1).unwrap().tags, vec!["concurrent"]);
    assert_eq!(stale.get(1).unwrap().length, Some(42));
    assert_eq!(stale.get(1).unwrap().extra_fields["new_key"], serde_yaml::Value::Bool(true));
    assert!(!stale.get(1).unwrap().extra_fields.contains_key("remove_me"));
    assert!(stale.get(2).is_none());
    assert!(stale.get(3).is_some());
    assert!(stale.get(4).is_some());
    writer.update_records(|mut records| {
        records.get_mut(&3).unwrap().author = "latest".into();
        records.remove(&1);
        Ok((records, ()))
    }).unwrap();
    stale.all_records_mut().get_mut(&1).unwrap().length = Some(99);
    stale.save().unwrap();
    assert!(stale.get(1).is_none(), "stale saves do not resurrect deleted rows");
    assert_eq!(stale.get(3).unwrap().author, "latest");
    stale.save().unwrap();
    assert_eq!(Database::with_root(temp.path().to_path_buf()).unwrap().get(3).unwrap().author, "latest");
}

#[test]
fn compat_mutation_reads_external_yaml_edits_and_mirrors_current_records() {
    let temp = library();
    let mut db = seed(temp.path());
    let state = state::active_for(&temp.path().join(".narou")).unwrap();
    state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
    db.update_records(|records| Ok((records, ()))).unwrap();
    let path = temp.path().join(".narou/database.yaml");
    let mut records: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    records.get_mut(&1).unwrap().tags.push("external".into());
    std::fs::write(&path, serde_yaml::to_string(&records).unwrap()).unwrap();
    db.all_records_mut().get_mut(&1).unwrap().length = Some(42);
    db.save().unwrap();
    let records: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(records[&1].tags, vec!["external"]);
    assert_eq!(records[&1].length, Some(42));
    assert_eq!(db.get(1).unwrap().tags, vec!["external"]);
    assert!(temp.path().join(".narou/database_index.yaml").is_file());
}

#[test]
fn failed_compat_save_preserves_sqlite_and_pending_local_delta() {
    let temp = library();
    let mut db = seed(temp.path());
    let state = state::active_for(&temp.path().join(".narou")).unwrap();
    state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
    let path = temp.path().join(".narou/database.yaml");
    // Enabling compat may seed the mirror. Replace only this verified scratch
    // fixture path with a directory to force the next mutation to fail.
    assert_eq!(path.parent().unwrap(), temp.path().join(".narou"));
    if path.is_file() { std::fs::remove_file(&path).unwrap(); }
    std::fs::create_dir(&path).unwrap();
    db.all_records_mut().get_mut(&1).unwrap().length = Some(42);
    db.save().expect_err("invalid mirror path must not acknowledge SQL changes");
    let length: Option<i64> = state.conn_ref().lock().query_row("SELECT length FROM novels WHERE id = 1", [], |row| row.get(0)).unwrap();
    assert_eq!(length, None);
    // The verified scratch target is this test's empty directory, not user data.
    assert_eq!(path.parent().unwrap(), temp.path().join(".narou"));
    std::fs::remove_dir(&path).unwrap();
    db.save().unwrap();
    assert_eq!(db.get(1).unwrap().length, Some(42));
}

#[test]
fn sqlite_allocations_share_durable_ownership_and_generation_fenced_release() {
    use narou_rs::platform::NovelIdReservation;
    let temp = library();
    let mut first = seed(temp.path());
    let mut second = Database::with_root(temp.path().to_path_buf()).unwrap();
    let owned = first.allocate_id().unwrap();
    let stale = NovelIdReservation::new(owned.id(), owned.generation());
    let other = second.allocate_id().unwrap();
    assert_ne!(owned.id(), other.id());
    let reusable = owned.id();
    first.release_id(owned);
    let retry = second.allocate_id().unwrap();
    assert_eq!(retry.id(), reusable);
    assert_ne!(retry.generation(), stale.generation());
    first.release_id(stale);
    let fresh = first.allocate_id().unwrap();
    assert_ne!(fresh.id(), retry.id(), "stale release must not free a new owner's reservation");
    second.insert(record(retry.id().0));
    second.save().unwrap();
    second.release_id(retry);
    let later = first.allocate_id().unwrap();
    assert_ne!(later.id(), reusable, "committed IDs must not reenter the pool");
    first.release_id(fresh);
    first.release_id(later);
    second.release_id(other);
}

#[test]
fn save_rejects_conflicting_new_id_without_overwriting_acknowledged_record() {
    let temp = library();
    let mut first = seed(temp.path());
    let mut stale = Database::with_root(temp.path().to_path_buf()).unwrap();
    first.insert(record(3));
    first.save().unwrap();
    let mut conflicting = record(3);
    conflicting.title = "conflict".into();
    stale.insert(conflicting);
    stale.remove(1);
    stale.save().expect_err("a stale new-ID collision must not overwrite a published row");
    let reopened = Database::with_root(temp.path().to_path_buf()).unwrap();
    assert_eq!(reopened.get(3).unwrap().title, "title-3");
    assert!(reopened.get(1).is_some(), "the whole conflicting save must roll back");
}

// Invoked by the parent test as an actual fresh process, never through Cargo.
#[test]
#[ignore]
fn reservation_process_child() {
    let root = PathBuf::from(std::env::var_os("NAROU_RECORD_TEST_ROOT").expect("child-only helper"));
    let output = std::env::var_os("NAROU_RECORD_TEST_OUTPUT").unwrap();
    let mut db = Database::with_root(root.clone()).unwrap();
    let reservation = db.allocate_id().unwrap();
    std::fs::write(root.join(output), reservation.id().0.to_string()).unwrap();
    // Deliberately exit without publication/release: another process must still
    // respect this durable owner, not restart from the published maximum.
}

#[test]
fn fresh_processes_do_not_reallocate_unpublished_sqlite_ids() {
    let temp = library();
    drop(seed(temp.path()));
    for output in ["first-id", "second-id"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "reservation_process_child", "--ignored", "--nocapture"])
            .env("NAROU_RECORD_TEST_ROOT", temp.path())
            .env("NAROU_RECORD_TEST_OUTPUT", output)
            .env_remove("NAROU_RS_LEGACY_YAML").output().unwrap();
        assert!(result.status.success(), "{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    }
    let first = std::fs::read_to_string(temp.path().join("first-id")).unwrap();
    let second = std::fs::read_to_string(temp.path().join("second-id")).unwrap();
    assert_ne!(first, second, "fresh processes must not reuse unpublished IDs");
}

#[test]
fn edited_export_takes_precedence_over_an_older_matching_compat_sha() {
    for remain_compat in [false, true] {
        let temp = library();
        let mut db = seed(temp.path());
        let state = state::active_for(&temp.path().join(".narou")).unwrap();
        state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
        db.save().unwrap();
        let old_yaml = std::fs::read_to_string(temp.path().join(".narou/database.yaml")).unwrap();
        if remain_compat {
            // Compat YAML is authoritative: publish the newer snapshot through
            // the normal mutation seam, not a DB-only write behind its mirror.
            db.update_records(|mut records| {
                records.get_mut(&1).unwrap().title = "newer export".into();
                Ok((records, ()))
            }).unwrap();
        } else {
            state.set_raw("inv", "local_setting", "narou-compat: false\n").unwrap();
            db.update_records(|mut records| {
                records.get_mut(&1).unwrap().title = "newer export".into();
                Ok((records, ()))
            }).unwrap();
        }
        cli(temp.path(), &["db", "export-yaml", "--in-place"]);
        let exported: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&std::fs::read_to_string(temp.path().join(".narou/database.yaml")).unwrap()).unwrap();
        assert_eq!(exported[&1].title, "newer export");
        std::fs::write(temp.path().join(".narou/database.yaml"), old_yaml).unwrap();
        state::write_mode(&temp.path().join(".narou"), StorageMode::Sqlite).unwrap();
        cli(temp.path(), &["list"]);
        let reopened = Database::with_root(temp.path().to_path_buf()).unwrap();
        assert_eq!(reopened.get(1).unwrap().title, "title-1", "edited export must beat old mirror SHA with narou-compat={remain_compat}");
    }
}

#[test]
fn ordinary_compat_mirror_ownership_does_not_reimport_after_external_disable() {
    let temp = library();
    let mut db = seed(temp.path());
    let state = state::active_for(&temp.path().join(".narou")).unwrap();
    state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
    db.update_records(|mut records| {
        records.get_mut(&1).unwrap().tags.push("new mirror".into());
        Ok((records, ()))
    }).unwrap();
    // External settings edits are supported authority changes, not an API
    // cutover that would itself refresh all projection fingerprints.
    std::fs::write(temp.path().join(".narou/local_setting.yaml"), "narou-compat: false\n").unwrap();
    db.update_records(|mut records| {
        records.get_mut(&1).unwrap().length = Some(42);
        Ok((records, ()))
    }).unwrap();
    let reopened = Database::with_root(temp.path().to_path_buf()).unwrap();
    assert_eq!(reopened.get(1).unwrap().length, Some(42), "unchanged owned compat mirror must not replace later SQLite writes");
    assert_eq!(reopened.get(1).unwrap().tags, vec!["new mirror"]);
}

#[test]
fn compat_constructor_waits_for_authoritative_yaml_lock_before_reading() {
    use fs2::FileExt;
    use std::sync::mpsc;
    let temp = library();
    let mut db = seed(temp.path());
    let state = state::active_for(&temp.path().join(".narou")).unwrap();
    state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
    db.save().unwrap();
    let path = temp.path().join(".narou/database.yaml");
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(temp.path().join(".narou/database.yaml.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let root = temp.path().to_path_buf();
    let thread = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let reopened = Database::with_root(root).unwrap();
        done_tx.send(reopened.get(1).unwrap().tags.clone()).unwrap();
    });
    started_rx.recv().unwrap();
    let early = done_rx.recv_timeout(std::time::Duration::from_millis(200));
    let mut records: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    records.get_mut(&1).unwrap().tags.push("written while opening".into());
    std::fs::write(&path, serde_yaml::to_string(&records).unwrap()).unwrap();
    FileExt::unlock(&lock).unwrap();
    assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)), "constructor must not read authoritative YAML before obtaining its lock");
    assert_eq!(done_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), vec!["written while opening"]);
    thread.join().unwrap();
}

fn cli(root: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_narou_rs")).args(args).current_dir(root)
        .env("HOME", root).env("USERPROFILE", root)
        .env_remove("NAROU_RS_LEGACY_YAML").env_remove("NAROU_RS_ASSET_BACKEND")
        .output().unwrap();
    assert!(output.status.success(), "{args:?}: {}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn fresh_process_reentry_imports_edited_exported_yaml_including_deletions() {
    let temp = library();
    drop(seed(temp.path()));
    cli(temp.path(), &["db", "export-yaml", "--in-place"]);
    cli(temp.path(), &["tag", "-a", "yaml-change", "1"]);
    let path = temp.path().join(".narou/database.yaml");
    let mut records: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(records[&1].tags.contains(&"yaml-change".into()));
    records.remove(&2);
    records.get_mut(&1).unwrap().extra_fields.insert("future".into(), serde_yaml::Value::Bool(true));
    std::fs::write(&path, serde_yaml::to_string(&records).unwrap()).unwrap();
    state::write_mode(&temp.path().join(".narou"), StorageMode::Sqlite).unwrap();
    cli(temp.path(), &["list"]);
    let reopened = Database::with_root(temp.path().to_path_buf()).unwrap();
    assert!(reopened.get(1).unwrap().tags.contains(&"yaml-change".into()));
    assert_eq!(reopened.get(1).unwrap().extra_fields["future"], serde_yaml::Value::Bool(true));
    assert!(reopened.get(2).is_none());
}
