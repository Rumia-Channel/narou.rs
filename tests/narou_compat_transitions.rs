#![cfg(feature = "native-runtime")]

use std::sync::Arc;
use narou_rs::application::{SettingsService, authors::AuthorStore};
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::native::{application::NativeSettingsStore, author_store::NativeAuthorStore};
use narou_rs::native::sqlite::state::{self, StorageMode};

fn library() -> tempfile::TempDir {
    let target = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/compat-transition-tests");
    std::fs::create_dir_all(&target).unwrap();
    let temp = tempfile::Builder::new().prefix("library-").tempdir_in(target).unwrap();
    std::fs::create_dir_all(temp.path().join(".narousetting")).unwrap();
    state::write_mode(&temp.path().join(".narou"), StorageMode::Sqlite).unwrap();
    temp
}

#[tokio::test]
async fn enable_preserves_existing_authors_and_seeds_mirrors() {
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let authors = NativeAuthorStore::new(inventory.clone());
    let original = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/1");
    let added = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/2");
    authors.add(&original).await.unwrap();
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory)));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    assert!(temp.path().join(".narou/author.yaml").exists());
    assert!(temp.path().join(".narou/local_setting.yaml").exists());
    assert_eq!(authors.load().await.unwrap(), vec![original.clone()]);
    authors.add(&added).await.unwrap();
    assert_eq!(authors.load().await.unwrap(), vec![original, added]);
}

#[tokio::test]
async fn enable_allows_next_settings_cas_and_rejects_stale_snapshot() {
    use narou_rs::application::settings::SettingsStore;
    use narou_rs::setting_core::SettingScope;
    let temp = library();
    let store = Arc::new(NativeSettingsStore::new(Arc::new(Inventory::new(temp.path().to_path_buf()))));
    let settings = SettingsService::new(store.clone());
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let old = store.load(SettingScope::Local).await.unwrap();
    settings.set("convert.no-open", &serde_json::json!(true)).await.unwrap();
    assert!(!store.compare_save(SettingScope::Local, &old, &old).await.unwrap());
    let current = state::active_for(&temp.path().join(".narou")).unwrap();
    let raw: String = current.conn().lock().query_row(
        "SELECT value_yaml FROM app_state WHERE scope = 'meta' AND key = 'export_file_sha:local_setting.yaml'",
        [], |row| row.get(0),
    ).unwrap();
    let ownership: [Option<String>; 2] = serde_yaml::from_str(&raw).unwrap();
    assert!(ownership[0].is_some());
    assert!(ownership[1].is_none(), "successful ordinary writes must retire prior ownership");
}

#[tokio::test]
async fn disable_then_restart_preserves_changes_and_removals() {
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let authors = NativeAuthorStore::new(inventory.clone());
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory.clone())));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let original = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/1");
    let added = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/2");
    authors.add(&original).await.unwrap();
    inventory.save_raw("login_cookie", InventoryScope::Local, "example.com: old-cookie\n").unwrap();
    settings.set("narou-compat", &serde_json::json!(false)).await.unwrap();
    authors.add(&added).await.unwrap();
    authors.remove(&original.url).await.unwrap();
    inventory.save_raw("login_cookie", InventoryScope::Local, "example.com: new-cookie\n").unwrap();
    settings.set("convert.no-open", &serde_json::json!(true)).await.unwrap();
    // configure opens a fresh SQLite connection and runs real startup reconciliation.
    let reopened = state::configure(&temp.path().join(".narou")).unwrap();
    assert!(!reopened.compat_now());
    assert_eq!(authors.load().await.unwrap(), vec![added]);
    assert_eq!(reopened.get_raw("inv", "login_cookie").unwrap().unwrap(), "example.com: new-cookie\n");
    assert!(reopened.get_raw("inv", "local_setting").unwrap().unwrap().contains("convert.no-open"));
}

#[tokio::test]
async fn missing_compat_mirror_uses_same_preimage_for_read_and_update() {
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let authors = NativeAuthorStore::new(inventory.clone());
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory)));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let first = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/1");
    let second = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/2");
    authors.add(&first).await.unwrap();
    std::fs::remove_file(temp.path().join(".narou/author.yaml")).unwrap();
    assert_eq!(authors.load().await.unwrap(), vec![first.clone()]);
    authors.add(&second).await.unwrap();
    assert_eq!(authors.load().await.unwrap(), vec![first, second]);
}

#[tokio::test]
async fn enable_before_database_initialization_preserves_unimported_legacy_records() {
    let temp = library();
    let record: narou_rs::db::NovelRecord = serde_json::from_value(serde_json::json!({
        "id": 1, "title": "legacy novel", "author": "fixture", "file_title": "legacy novel",
        "toc_url": "https://example.com/legacy/", "sitename": "fixture", "last_update": "2026-01-01T00:00:00Z"
    })).unwrap();
    let records = std::collections::BTreeMap::from([(1, record)]);
    std::fs::write(temp.path().join(".narou/database.yaml"), serde_yaml::to_string(&records).unwrap()).unwrap();
    std::fs::write(temp.path().join(".narou/freeze.yaml"), "1: true\n").unwrap();
    // The settings CLI constructs Inventory before any Database initialization.
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory)));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let current = state::active_for(&temp.path().join(".narou")).unwrap();
    let rows: i64 = current.conn().lock().query_row("SELECT count(*) FROM novels", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, 1, "enable must import existing YAML before seeding its mirror");
    let frozen: i64 = current.conn().lock().query_row("SELECT novel_id FROM frozen_novels", [], |row| row.get(0)).unwrap();
    assert_eq!(frozen, 1);
    let reopened = narou_rs::db::Database::with_root(temp.path().to_path_buf()).unwrap();
    assert_eq!(reopened.get(1).unwrap().title, "legacy novel");
}

#[tokio::test]
async fn repeated_cutovers_seed_current_records_and_preserve_relational_children() {
    fn add(db: &mut narou_rs::db::Database, id: i64) {
        db.update_records(|mut records| {
            records.insert(id, serde_json::from_value(serde_json::json!({
                "id": id, "title": format!("novel-{id}"), "author": "fixture",
                "file_title": format!("novel-{id}"), "toc_url": format!("https://example.com/{id}/"),
                "sitename": "fixture", "last_update": "2026-01-01T00:00:00Z"
            })).unwrap());
            Ok((records, ()))
        }).unwrap();
    }
    let temp = library();
    let mut db = narou_rs::db::Database::with_root(temp.path().to_path_buf()).unwrap();
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(Arc::new(
        Inventory::new(temp.path().to_path_buf()),
    ))));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    add(&mut db, 1);
    let database_path = temp.path().join(".narou/database.yaml");
    let mut external_records: std::collections::BTreeMap<i64, narou_rs::db::NovelRecord> =
        serde_yaml::from_str(&std::fs::read_to_string(&database_path).unwrap()).unwrap();
    external_records.get_mut(&1).unwrap().tags.push("external-compat-edit".into());
    std::fs::write(&database_path, serde_yaml::to_string(&external_records).unwrap()).unwrap();
    settings.set("narou-compat", &serde_json::json!(false)).await.unwrap();
    add(&mut db, 2);
    let current = state::active_for(&temp.path().join(".narou")).unwrap();
    current.conn().lock().execute_batch(
        "INSERT INTO novel_outputs (novel_id, kind, payload, updated_at) VALUES (2, 'text', X'41', '2026-01-01');
         INSERT INTO novel_sections (novel_id, idx, subtitle, body_hash) VALUES (2, '1', 'fixture', NULL);
         INSERT INTO novel_versions (id, novel_id, parent_id, origin, note, created_at) VALUES (200, 2, NULL, 'manual', NULL, '2026-01-01');
         INSERT INTO novel_version_sections (version_id, idx, subtitle, body_hash) VALUES (200, '1', 'fixture', NULL);
         INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES (200, NULL, 'fixture-diff');"
    ).unwrap();
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let yaml: std::collections::BTreeMap<i64, narou_rs::db::NovelRecord> = serde_yaml::from_str(
        &std::fs::read_to_string(temp.path().join(".narou/database.yaml")).unwrap(),
    ).unwrap();
    assert_eq!(yaml.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
    add(&mut db, 3);
    state::configure(&temp.path().join(".narou")).unwrap();
    let reopened = narou_rs::db::Database::with_root(temp.path().to_path_buf()).unwrap();
    for id in [1, 2, 3] { assert!(reopened.get(id).is_some(), "lost novel {id}"); }
    assert!(reopened.get(1).unwrap().tags.contains(&"external-compat-edit".to_string()));
    assert!(reopened.get_by_toc_url("https://example.com/2/").is_some());
    let conn = current.conn();
    let guard = conn.lock();
    for table in ["novel_outputs", "novel_sections", "novel_versions", "novel_version_sections", "novel_version_diffs"] {
        let count: i64 = guard.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1, "cutover removed {table} child");
    }
    let invalid_rows: i64 = guard.query_row(
        "SELECT count(*) FROM app_state WHERE scope = 'inv' AND key IN ('database', 'database_index')",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(invalid_rows, 0, "records/index must not become inventory app_state");
}

#[tokio::test]
async fn concurrent_author_additions_and_disable_preserve_all_acknowledged_values() {
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let first = NativeAuthorStore::new(inventory.clone());
    let second = NativeAuthorStore::new(Arc::new(Inventory::new(temp.path().to_path_buf())));
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory)));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let one = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/1");
    let two = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/2");
    let disabled = serde_json::json!(false);
    let (added_one, added_two, disabled) = tokio::join!(
        first.add(&one), second.add(&two), settings.set("narou-compat", &disabled),
    );
    assert!(added_one.unwrap());
    assert!(added_two.unwrap());
    disabled.unwrap();
    state::configure(&temp.path().join(".narou")).unwrap();
    assert_eq!(first.load().await.unwrap(), vec![one, two]);
}

#[tokio::test]
async fn failed_enable_leaves_old_authority_and_no_stale_import() {
    use narou_rs::setting_core::SettingScope;
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory.clone())));
    inventory.save_raw("alias", InventoryScope::Local, "old: 1\n").unwrap();
    inventory.save_raw("author", InventoryScope::Local, "https://example.com/author/1: example.com\n").unwrap();
    let blocked = temp.path().join(".narou/author.yaml");
    std::fs::create_dir(&blocked).unwrap();
    assert!(settings.set("narou-compat", &serde_json::json!(true)).await.is_err());
    assert_ne!(narou_rs::db::settings::load_with_inventory(&inventory, SettingScope::Local)
        .unwrap().get("narou-compat"), Some(&serde_yaml::Value::Bool(true)));
    assert!(!temp.path().join(".narou/local_setting.yaml").exists());
    std::fs::remove_dir(&blocked).unwrap();
    inventory.save_raw("alias", InventoryScope::Local, "new: 2\n").unwrap();
    let reopened = state::configure(&temp.path().join(".narou")).unwrap();
    assert!(!reopened.compat_now());
    assert_eq!(reopened.get_raw("inv", "alias").unwrap().unwrap(), "new: 2\n");
}

#[tokio::test]
async fn interrupted_disable_keeps_durably_reconciled_author_and_freeze_edits() {
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    let authors = NativeAuthorStore::new(inventory.clone());
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory.clone())));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    let original = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/1");
    let external = narou_rs::author::AuthorRecord::new("example.com", "https://example.com/author/2");
    authors.add(&original).await.unwrap();
    inventory.save_raw("freeze", InventoryScope::Local, "1: true\n").unwrap();
    std::fs::write(temp.path().join(".narou/author.yaml"),
        "https://example.com/author/1: example.com\nhttps://example.com/author/2: example.com\n").unwrap();
    std::fs::write(temp.path().join(".narou/freeze.yaml"), "2: true\n").unwrap();
    let current = state::active_for(&temp.path().join(".narou")).unwrap();
    // Fail only the publication transaction, after durable reconciliation.
    current.conn().lock().execute_batch(
        "CREATE TRIGGER reject_flag BEFORE INSERT ON app_state
         WHEN NEW.scope = 'inv' AND NEW.key = 'local_setting'
         BEGIN SELECT RAISE(ABORT, 'injected flag publication failure'); END;"
    ).unwrap();
    assert!(settings.set("narou-compat", &serde_json::json!(false)).await.is_err());
    assert!(current.compat_now());
    current.conn().lock().execute_batch("DROP TRIGGER reject_flag;").unwrap();
    let stored_freeze: String = current.conn().lock().query_row(
        "SELECT value_yaml FROM app_state WHERE scope = 'inv' AND key = 'freeze'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(stored_freeze, "2: true\n", "old file authority must already be durable");
    // Model interruption after publishing false but before publication commit.
    std::fs::write(temp.path().join(".narou/local_setting.yaml"), "narou-compat: false\n").unwrap();
    let reopened = state::configure(&temp.path().join(".narou")).unwrap();
    assert!(!reopened.compat_now());
    assert_eq!(authors.load().await.unwrap(), vec![original, external]);
    assert_eq!(reopened.get_raw("inv", "freeze").unwrap().unwrap(), "2: true\n");
}

#[cfg(windows)]
#[tokio::test]
#[allow(clippy::permissions_set_readonly_false)] // Windows-only fixture: restore writability.
async fn failed_flag_publication_restores_seeded_files_and_keeps_durable_provenance() {
    struct WritableOnDrop(std::path::PathBuf);
    impl Drop for WritableOnDrop {
        fn drop(&mut self) {
            if let Ok(mut permissions) = std::fs::metadata(&self.0).map(|m| m.permissions()) {
                permissions.set_readonly(false);
                let _ = std::fs::set_permissions(&self.0, permissions);
            }
        }
    }
    let temp = library();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    inventory.save_raw("alias", InventoryScope::Local, "old: 1\n").unwrap();
    let flag_path = temp.path().join(".narou/local_setting.yaml");
    std::fs::write(&flag_path, "narou-compat: false\n").unwrap();
    let writable = WritableOnDrop(flag_path.clone());
    let mut permissions = std::fs::metadata(&flag_path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&flag_path, permissions).unwrap();
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory.clone())));
    assert!(settings.set("narou-compat", &serde_json::json!(true)).await.is_err());
    drop(writable);
    let current = state::active_for(&temp.path().join(".narou")).unwrap();
    assert!(!current.compat_now());
    assert!(!temp.path().join(".narou/alias.yaml").exists(), "earlier seed must be rolled back");
    let provenance: String = current.conn().lock().query_row(
        "SELECT value_yaml FROM app_state WHERE scope = 'meta' AND key = 'export_file_sha:alias.yaml'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(provenance.contains("- "), "ownership must survive failed publication");
    // Model an interrupted cleanup: an already-owned seed remains on disk.
    std::fs::write(temp.path().join(".narou/alias.yaml"), "old: 1\n").unwrap();
    inventory.save_raw("alias", InventoryScope::Local, "new: 2\n").unwrap();
    let reopened = state::configure(&temp.path().join(".narou")).unwrap();
    assert_eq!(reopened.get_raw("inv", "alias").unwrap().unwrap(), "new: 2\n");
}

#[tokio::test]
async fn transitions_keep_global_file_only_and_reconcile_external_freeze_edits() {
    use narou_rs::setting_core::SettingScope;
    let temp = library();
    let mut db = narou_rs::db::Database::with_root(temp.path().to_path_buf()).unwrap();
    db.update_records(|mut records| {
        for id in [1, 2] {
            let record = serde_json::from_value(serde_json::json!({
                "id": id, "title": format!("novel-{id}"), "author": "fixture",
                "file_title": format!("novel-{id}"), "toc_url": format!("https://example.com/{id}/"),
                "sitename": "fixture", "last_update": "2026-01-01T00:00:00Z"
            })).unwrap();
            records.insert(id, record);
        }
        Ok((records, ()))
    }).unwrap();
    let inventory = Arc::new(Inventory::new(temp.path().to_path_buf()));
    narou_rs::db::settings::update_with_inventory(&inventory, SettingScope::Global, |map| {
        map.insert("fixture".into(), serde_yaml::Value::Bool(true));
        Ok(())
    }).unwrap();
    inventory.save_raw("freeze", InventoryScope::Local, "1: true\n").unwrap();
    let settings = SettingsService::new(Arc::new(NativeSettingsStore::new(inventory.clone())));
    settings.set("narou-compat", &serde_json::json!(true)).await.unwrap();
    assert_eq!(std::fs::read_to_string(temp.path().join(".narou/freeze.yaml")).unwrap(), "1: true\n");
    // Compat files are authoritative, including removals, at cutover.
    std::fs::write(temp.path().join(".narou/freeze.yaml"), "2: true\n").unwrap();
    settings.set("narou-compat", &serde_json::json!(false)).await.unwrap();
    let current = state::active_for(&temp.path().join(".narou")).unwrap();
    assert_eq!(current.get_raw("inv", "freeze").unwrap().unwrap(), "2: true\n");
    let frozen_id: i64 = current.conn().lock().query_row(
        "SELECT novel_id FROM frozen_novels", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(frozen_id, 2);
    inventory.save_raw("freeze", InventoryScope::Local, "{}\n").unwrap();
    let reopened = state::configure(&temp.path().join(".narou")).unwrap();
    assert_eq!(reopened.get_raw("inv", "freeze").unwrap().unwrap(), "{}\n");
    let conn = reopened.conn();
    let guard = conn.lock();
    let count: i64 = guard.query_row("SELECT count(*) FROM frozen_novels", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0, "stale compat freeze must not resurrect removed freezes");
    let global_rows: i64 = guard.query_row("SELECT count(*) FROM app_state WHERE scope = 'global'", [], |row| row.get(0)).unwrap();
    assert_eq!(global_rows, 0);
    assert!(temp.path().join(".narousetting/global_setting.yaml").exists());
}
