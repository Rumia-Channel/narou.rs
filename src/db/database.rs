use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::index_store::IndexStore;
use super::inventory::{Inventory, InventoryScope};
use super::novel_record::NovelRecord;

use crate::error::Result;

const ARCHIVE_ROOT_DIR: &str = "小説データ";
const DATABASE_NAME: &str = "database";

pub struct Database {
    data: HashMap<i64, NovelRecord>,
    index: IndexStore,
    inventory: Inventory,
    archive_root: PathBuf,
    next_id: i64,
    /// SQLite backend (P2). `None` when `NAROU_RS_LEGACY_YAML` is set or the
    /// database directory cannot host `db.sqlite`; then the legacy YAML path
    /// below runs unchanged.
    #[cfg(feature = "native-runtime")]
    sqlite: Option<crate::native::sqlite::SqliteNovelRepository>,
}

impl Database {
    pub fn new() -> Result<Self> {
        let inventory = Inventory::with_default_root()?;
        Self::with_inventory(inventory)
    }

    fn with_inventory(inventory: Inventory) -> Result<Self> {
        let archive_root = inventory.root_dir().join(ARCHIVE_ROOT_DIR);
        std::fs::create_dir_all(&archive_root)?;

        #[cfg(feature = "native-runtime")]
        let sqlite = {
            let narou_dir = inventory.root_dir().join(".narou");
            match crate::native::sqlite::state::active_for(&narou_dir) {
                Some(state) => {
                    let repo =
                        crate::native::sqlite::SqliteNovelRepository::new(state.conn_ref().clone());
                    let yaml_path =
                        inventory.inventory_path("database", InventoryScope::Local);
                    let index_path =
                        inventory.inventory_path("database_index", InventoryScope::Local);
                    let file_content = std::fs::read_to_string(&yaml_path).ok();
                    let stored_sha = state
                        .get_raw_db("meta", "database_yaml_sha")
                        .ok()
                        .flatten();
                    let file_sha = file_content
                        .as_ref()
                        .map(|content| sha256_hex(content.as_bytes()));
                    if state.compat_now() {
                        // Compat mode: database.yaml is authoritative.
                        match file_content {
                            Some(content) => {
                                if file_sha.as_deref() != stored_sha.as_deref() {
                                    if !content.trim().is_empty() {
                                        let parsed: BTreeMap<i64, NovelRecord> =
                                            serde_yaml::from_str(&content)?;
                                        let conn = repo.conn_handle();
                                        crate::native::sqlite::bulk::replace_all_records(
                                            &mut conn.lock().expect("sqlite mutex poisoned"),
                                            &parsed,
                                        )?;
                                    }
                                    let _ = state.set_raw_db(
                                        "meta",
                                        "database_yaml_sha",
                                        &file_sha.clone().unwrap_or_default(),
                                    );
                                }
                            }
                            None => {
                                // File deleted externally: recreate it from
                                // the database so narou.rb still sees the
                                // library.
                                mirror_records_yaml(&state, &repo, &yaml_path)?;
                            }
                        }
                    } else {
                        // Non-compat: import the file when it carries data the
                        // DB has not mirrored yet, then retire it.
                        let should_import = match (&file_content, &stored_sha) {
                            (Some(content), Some(sha)) => {
                                file_sha.as_deref() != Some(sha.as_str())
                                    && !content.trim().is_empty()
                            }
                            (Some(content), None) => {
                                let conn = repo.conn_handle();
                                crate::native::sqlite::bulk::load_all_records(
                                    &conn.lock().expect("sqlite mutex poisoned"),
                                )?
                                .is_empty()
                                    && !content.trim().is_empty()
                            }
                            (None, _) => false,
                        };
                        if should_import {
                            let parsed: BTreeMap<i64, NovelRecord> =
                                serde_yaml::from_str(file_content.as_deref().unwrap_or(""))?;
                            let conn = repo.conn_handle();
                            crate::native::sqlite::bulk::replace_all_records(
                                &mut conn.lock().expect("sqlite mutex poisoned"),
                                &parsed,
                            )?;
                        }
                        if yaml_path.exists() {
                            rename_imported_file(&yaml_path);
                        }
                        if index_path.exists() {
                            rename_imported_file(&index_path);
                        }
                    }
                    Some(repo)
                }
                None => None,
            }
        };
        let mut db = Self {
            data: HashMap::new(),
            index: IndexStore::load(&inventory)?,
            inventory,
            archive_root,
            next_id: 0,
            #[cfg(feature = "native-runtime")]
            sqlite,
        };
        db.refresh()?;
        #[cfg(feature = "native-runtime")]
        if db.compat_active() {
            let index_path = db
                .inventory
                .inventory_path("database_index", InventoryScope::Local);
            if !index_path.exists() {
                db.index.mark_dirty();
            }
        }
        Ok(db)
    }

    /// Test/CLI helper: construct against an explicit narou root instead of
    /// walking up from the current directory.
    pub fn with_root(root: PathBuf) -> Result<Self> {
        Self::with_inventory(Inventory::new(root))
    }

    /// True when the SQLite backend is active AND `narou-compat` is on:
    /// `.narou/*.yaml` files stay authoritative and every write is mirrored.
    #[cfg(feature = "native-runtime")]
    fn compat_active(&self) -> bool {
        self.state_handle()
            .map(|state| state.compat_now())
            .unwrap_or(false)
    }

    /// Serialized `database_index.yaml` payload for `db export-yaml`.
    pub fn index_yaml(&self) -> Result<String> {
        self.index.to_yaml()
    }

    #[cfg(feature = "native-runtime")]
    fn state_handle(&self) -> Option<crate::native::sqlite::state::StateDb> {
        crate::native::sqlite::state::active_for(
            &self.inventory.root_dir().join(".narou"),
        )
    }

    /// `PRAGMA integrity_check` result when the SQLite backend is active.
    pub fn sqlite_integrity(&self) -> Result<Option<String>> {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let conn = repo.conn_handle();
            let guard = conn.lock().expect("sqlite mutex poisoned");
            let status: String = guard
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(|error| crate::native::sqlite::sqlite_error(error))?;
            return Ok(Some(status));
        }
        #[allow(unreachable_code)]
        Ok(None)
    }

    /// Verify every `objects`/`section_bodies` payload against its stored
    /// CRC-32 after decompression. Returns the count of corrupted rows, or
    /// `None` in legacy YAML mode / when the tables do not exist yet.
    pub fn sqlite_payload_check(&self) -> Result<Option<usize>> {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let conn = repo.conn_handle();
            let guard = conn.lock().expect("sqlite mutex poisoned");
            return crate::native::sqlite::object_store::verify_payload_crc32(&guard)
                .map(Some);
        }
        #[allow(unreachable_code)]
        Ok(None)
    }

    fn using_sqlite(&self) -> bool {
        #[cfg(feature = "native-runtime")]
        {
            self.sqlite.is_some()
        }
        #[cfg(not(feature = "native-runtime"))]
        {
            false
        }
    }

    pub fn refresh(&mut self) -> Result<()> {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let conn = repo.conn_handle();
            let loaded = crate::native::sqlite::bulk::load_all_records(
                &conn.lock().expect("sqlite mutex poisoned"),
            )?;
            self.data = loaded.into_iter().collect();
            if let Some(max_id) = self.data.keys().max().copied() {
                self.next_id = self.next_id.max(max_id.saturating_add(1));
            }
            return Ok(());
        }
        let raw = self
            .inventory
            .load_raw(DATABASE_NAME, InventoryScope::Local)?;
        if !raw.is_empty() {
            let loaded: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&raw)?;
            self.data = loaded
                .into_iter()
                .map(|(id, mut record)| {
                    record.id = id;
                    (id, record)
                })
                .collect();
        } else {
            self.data.clear();
        }
        if let Some(max_id) = self.data.keys().max().copied() {
            self.next_id = self.next_id.max(max_id.saturating_add(1));
        }
        if !self.using_sqlite() {
            self.index.reconcile(&self.data);
        }
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let normalized: BTreeMap<i64, NovelRecord> = self
                .data
                .iter()
                .map(|(&id, record)| {
                    let mut normalized = record.clone();
                    normalized.id = id;
                    (id, normalized)
                })
                .collect();
            let conn = repo.conn_handle();
            crate::native::sqlite::bulk::replace_all_records(
                &mut conn.lock().expect("sqlite mutex poisoned"),
                &normalized,
            )?;
            if self.compat_active() {
                let yaml_path = self
                    .inventory
                    .inventory_path(DATABASE_NAME, InventoryScope::Local);
                let content = serde_yaml::to_string(&normalized)?;
                crate::db::inventory::atomic_write(&yaml_path, &content)?;
                if let Some(state) = self.state_handle() {
                    let _ = state.set_raw_db(
                        "meta",
                        "database_yaml_sha",
                        &sha256_hex(content.as_bytes()),
                    );
                }
                self.index.flush(&self.inventory)?;
            }
            return Ok(());
        }
        let content = serde_yaml::to_string(
            &self
                .data
                .iter()
                .map(|(&id, record)| {
                    let mut normalized = record.clone();
                    normalized.id = id;
                    (id, normalized)
                })
                .collect::<BTreeMap<_, _>>(),
        )?;
        self.inventory
            .save_raw(DATABASE_NAME, InventoryScope::Local, &content)?;
        if !self.using_sqlite() {
            self.index.flush(&self.inventory)?;
        }
        Ok(())
    }

    pub fn update_records<T, F>(&mut self, update: F) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>) -> Result<(BTreeMap<i64, NovelRecord>, T)>,
    {
        #[cfg(feature = "native-runtime")]
        if self.sqlite.is_some() {
            let current: BTreeMap<i64, NovelRecord> = self
                .data
                .iter()
                .map(|(&id, record)| {
                    let mut normalized = record.clone();
                    normalized.id = id;
                    (id, normalized)
                })
                .collect();
            let (mut updated, result) = update(current)?;
            for (id, record) in updated.iter_mut() {
                record.id = *id;
            }
            if let Some(repo) = &self.sqlite {
                let conn = repo.conn_handle();
                crate::native::sqlite::bulk::replace_all_records(
                    &mut conn.lock().expect("sqlite mutex poisoned"),
                    &updated,
                )?;
            }
            if self.compat_active() {
                let yaml_path = self
                    .inventory
                    .inventory_path(DATABASE_NAME, InventoryScope::Local);
                let content = serde_yaml::to_string(&updated)?;
                crate::db::inventory::atomic_write(&yaml_path, &content)?;
                if let Some(state) = self.state_handle() {
                    let _ = state.set_raw_db(
                        "meta",
                        "database_yaml_sha",
                        &sha256_hex(content.as_bytes()),
                    );
                }
            }
            self.data = updated.into_iter().collect();
            if self.compat_active() {
                self.index.flush(&self.inventory)?;
            }
            return Ok(result);
        }
        let result = self.inventory.update_yaml(
            DATABASE_NAME,
            InventoryScope::Local,
            |mut current: BTreeMap<i64, NovelRecord>| {
                for (id, record) in current.iter_mut() {
                    record.id = *id;
                }
                let (mut updated, result) = update(current)?;
                for (id, record) in updated.iter_mut() {
                    record.id = *id;
                }
                Ok((updated, result))
            },
        )?;
        self.refresh()?;
        if !self.using_sqlite() {
            self.index.flush(&self.inventory)?;
        }
        Ok(result)
    }

    pub fn get(&self, id: i64) -> Option<&NovelRecord> {
        self.data.get(&id)
    }

    pub fn insert(&mut self, record: NovelRecord) {
        let id = record.id;
        let toc_url = Some(record.toc_url.clone());
        let title = Some(record.title.clone());
        self.next_id = self.next_id.max(id.saturating_add(1));
        self.data.insert(id, record);
        self.index.upsert(id, toc_url.as_deref(), title.as_deref());
    }

    pub fn remove(&mut self, id: i64) -> Option<NovelRecord> {
        let record = self.data.remove(&id)?;
        self.index.delete(id);
        Some(record)
    }

    pub fn ids(&self) -> Vec<i64> {
        self.data.keys().copied().collect()
    }

    pub fn all_records(&self) -> &HashMap<i64, NovelRecord> {
        &self.data
    }

    pub fn all_records_mut(&mut self) -> &mut HashMap<i64, NovelRecord> {
        &mut self.data
    }

    pub fn novel_exists(&self, id: i64) -> bool {
        self.data.contains_key(&id)
    }

    pub fn get_by_toc_url(&self, toc_url: &str) -> Option<&NovelRecord> {
        self.index
            .lookup_by_toc_url(toc_url)
            .and_then(|id| self.data.get(&id))
    }

    pub fn get_by_title(&self, title: &str) -> Option<&NovelRecord> {
        self.index
            .lookup_by_title(title)
            .and_then(|id| self.data.get(&id))
    }

    pub fn find_by_title(&self, title: &str) -> Option<&NovelRecord> {
        if let Some(id) = self.index.lookup_by_title(title) {
            return self.data.get(&id);
        }
        let lower = title.to_lowercase();
        for record in self.data.values() {
            if record.title.to_lowercase() == lower {
                return Some(record);
            }
        }
        None
    }

    /// Legacy read-only candidate; repository callers must use `allocate_id`.
    pub fn create_new_id(&self) -> i64 {
        self.next_id
    }

    /// Reserve the next novel ID while the shared database lock is held.
    pub fn allocate_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    pub fn sort_by(&self, key: &str, reverse: bool) -> Vec<&NovelRecord> {
        let mut records: Vec<&NovelRecord> = self.data.values().collect();
        records.sort_by(|a, b| {
            let ordering = compare_records_by_key(a, b, key).then_with(|| a.id.cmp(&b.id));
            if reverse {
                ordering.reverse()
            } else {
                ordering
            }
        });
        records
    }

    pub fn tag_index(&self) -> HashMap<String, Vec<i64>> {
        let mut index: HashMap<String, Vec<i64>> = HashMap::new();
        for (id, record) in &self.data {
            for tag in &record.tags {
                index.entry(tag.clone()).or_default().push(*id);
            }
        }
        index
    }

    pub fn archive_root(&self) -> &Path {
        &self.archive_root
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }
}

/// 任意の 2 レコードを指定キーで比較する。`db::sort_by` と
/// `web::sort_state::sort_record_ordering` の双方から呼ばれる統一実装。
/// BUG-9 で導入された `compare_optional` 経由の型付き + None 安定順を採用する。
pub fn compare_records_by_key(
    a: &NovelRecord,
    b: &NovelRecord,
    key: &str,
) -> std::cmp::Ordering {
    match key {
        "id" => a.id.cmp(&b.id),
        "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        "author" => a.author.to_lowercase().cmp(&b.author.to_lowercase()),
        "last_update" => a.last_update.cmp(&b.last_update),
        "general_lastup" => compare_optional(a.general_lastup, b.general_lastup),
        "sitename" => a.sitename.cmp(&b.sitename),
        "novel_type" => a.novel_type.cmp(&b.novel_type),
        "length" => compare_optional(a.length, b.length),
        "last_check_date" => compare_optional(a.last_check_date, b.last_check_date),
        "tags" => record_tags_key(a).cmp(&record_tags_key(b)),
        "general_all_no" => compare_optional(a.general_all_no, b.general_all_no),
        "status" => record_status_key(a).cmp(&record_status_key(b)),
        "toc_url" => a.toc_url.cmp(&b.toc_url),
        // `new_arrivals_date` は `commands/update.rs` の挙動 (`Option<DateTime>` を
        // 直接 `.cmp()` する形) と整合させるため `compare_optional` ではなく
        // `Option::cmp` を使う。None < Some(...) の標準順序。
        "new_arrivals_date" => a.new_arrivals_date.cmp(&b.new_arrivals_date),
        _ => a.id.cmp(&b.id),
    }
}

/// `db::sort_by` と `web::sort_state::sort_record_ordering` が受理するソートキーの
/// 単一の真実源。CLI (`narou list --sort-by`, `narou update --sort-by`) と Web UI の
/// バリデーションはこの一覧を共有する。`const` として公開しているのは
/// `web::sort_state::SORT_COLUMN_KEYS` から `pub use` 再エクスポートするため。
///
/// **インデックスは `web::sort_state::SORT_COLUMN_LABELS` の並びと完全一致させること**。
/// 既存の `web::jobs::tests::sort_records_for_web_update_matches_general_lastup_descending`
/// 等は `column: 2` (= `general_lastup`) に依存しているため、Web UI のカラム順を
/// 維持する。
pub const SORT_KEYS: &[&str] = &[
    "id",                // 0
    "last_update",       // 1
    "general_lastup",    // 2
    "last_check_date",   // 3
    "title",             // 4
    "author",            // 5
    "sitename",          // 6
    "novel_type",        // 7
    "tags",              // 8
    "general_all_no",    // 9
    "length",            // 10
    "status",            // 11
    "toc_url",           // 12
    "new_arrivals_date", // 13
];

/// `SORT_KEYS` の関数版。`sort_keys().contains(&key)` のようなメソッドチェインが
/// 必要になる CLI / テスト側の便宜のために `SORT_KEYS` と並べて公開する。
pub fn sort_keys() -> &'static [&'static str] {
    SORT_KEYS
}

/// 未知のキーを `sort_by` に渡した場合のデフォルト。CLI/Web 双方の検証失敗時に
/// 同じ挙動を提供するために `sort_keys` と並べて公開する。
pub fn sort_key_valid(key: &str) -> bool {
    SORT_KEYS.iter().any(|candidate| *candidate == key)
}

pub(crate) fn compare_optional<T: Ord>(a: Option<T>, b: Option<T>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub(crate) fn record_tags_key(record: &NovelRecord) -> String {
    record
        .tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>()
        .join("\u{0}")
}

pub(crate) fn record_status_key(record: &NovelRecord) -> String {
    let mut status = Vec::new();
    if record.tags.iter().any(|tag| tag == "end") || record.end {
        status.push("完結");
    }
    if record.tags.iter().any(|tag| tag == "404") {
        status.push("削除");
    }
    if record.suspend {
        status.push("中断");
    }
    status.join(", ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::db::inventory::{Inventory, InventoryScope};
    use crate::db::novel_record::NovelRecord;
    use chrono::{TimeZone, Utc};

    #[test]
    fn database_parity_create_new_id_starts_at_zero() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::with_inventory(Inventory::new(temp.path().to_path_buf())).unwrap();
        assert_eq!(db.create_new_id(), 0);
    }

    #[test]
    fn database_parity_save_preserves_unknown_fields_and_zero_id() {
    let _legacy = crate::test_support::legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        std::fs::write(
            narou_dir.join("database.yaml"),
            r#"0:
  id: 999
  author: author
  title: title
  file_title: file title
  toc_url: https://example.com/0/
  sitename: Example
  novel_type: 1
  end: false
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  custom_flag: true
  nested:
    answer: 42
"#,
        )
        .unwrap();

        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();
        assert_eq!(db.get(0).unwrap().id, 0);
        assert_eq!(
            db.get(0)
                .unwrap()
                .extra_fields
                .get("custom_flag")
                .and_then(serde_yaml::Value::as_bool),
            Some(true)
        );

        db.save().unwrap();

        let saved = db
            .inventory()
            .load_raw("database", InventoryScope::Local)
            .unwrap();
        assert!(saved.contains("0:\n"));
        assert!(saved.contains("id: 0"));
        assert!(saved.contains("custom_flag: true"));
        assert!(saved.contains("answer: 42"));
        assert_eq!(db.create_new_id(), 1);
    }

    #[test]
    fn update_records_merges_against_current_database_yaml() {
    let _legacy = crate::test_support::legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        db.insert(sample_record(1, Some(10), &["old"]));
        db.save().unwrap();
        std::fs::write(
            narou_dir.join("database.yaml"),
            std::fs::read_to_string(narou_dir.join("database.yaml"))
                .unwrap()
                .replace("- old", "- edited"),
        )
        .unwrap();

        db.update_records(|mut records| {
            let record = records.get_mut(&1).unwrap();
            record.length = Some(20);
            Ok((records, ()))
        })
        .unwrap();

        let record = db.get(1).unwrap();
        assert_eq!(record.tags, vec!["edited"]);
        assert_eq!(record.length, Some(20));
    }

    #[test]
    fn sort_by_uses_typed_ordering_and_stable_none_order() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        db.insert(sample_record(10, Some(2), &[]));
        db.insert(sample_record(9, Some(10), &[]));
        db.insert(sample_record(1, None, &[]));

        let by_id = db.sort_by("id", false).iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(by_id, vec![1, 9, 10]);
        let by_length = db
            .sort_by("length", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(by_length, vec![10, 9, 1]);
    }

    #[test]
    fn sort_keys_lists_every_supported_key() {
        let keys = super::sort_keys();
        // 既存の8キー + 新規6キーがすべて含まれていること。
        for required in [
            "id",
            "title",
            "author",
            "last_update",
            "general_lastup",
            "sitename",
            "novel_type",
            "length",
            "last_check_date",
            "tags",
            "general_all_no",
            "status",
            "toc_url",
            "new_arrivals_date",
        ] {
            assert!(
                keys.contains(&required),
                "sort_keys must accept {required}, got {keys:?}"
            );
        }
        // sort_key_valid は sort_keys と完全一致する。
        for key in keys {
            assert!(super::sort_key_valid(key), "sort_key_valid({key})");
        }
        assert!(!super::sort_key_valid("not_a_key"));
    }

    #[test]
    fn sort_by_last_check_date_uses_compare_optional() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        // 値あり2件と None 1件を混在させ、(Some,None)→Less の順序を検証する。
        let mut with_ts_old = sample_record(10, Some(2), &[]);
        with_ts_old.last_check_date = Some(Utc.timestamp_opt(1_700_000_100, 0).unwrap());
        let mut with_ts_new = sample_record(9, Some(10), &[]);
        with_ts_new.last_check_date = Some(Utc.timestamp_opt(1_700_000_300, 0).unwrap());
        let without_ts = sample_record(1, None, &[]);
        db.insert(with_ts_old);
        db.insert(with_ts_new);
        db.insert(without_ts);

        // Some(古い) < Some(新しい) < None の順で並ぶ。
        let asc = db
            .sort_by("last_check_date", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(asc, vec![10, 9, 1]);

        // 降順では None が先頭に出る。
        let desc = db
            .sort_by("last_check_date", true)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(desc, vec![1, 9, 10]);
    }

    #[test]
    fn sort_by_general_all_no_handles_missing_values() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        let mut a = sample_record(1, Some(5), &[]);
        a.general_all_no = Some(7);
        let mut b = sample_record(2, Some(3), &[]);
        b.general_all_no = None;
        db.insert(a);
        db.insert(b);

        let by_general_all_no = db
            .sort_by("general_all_no", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(by_general_all_no, vec![1, 2]);
    }

    #[test]
    fn sort_by_tags_and_status_use_dedicated_keys() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        let mut alpha = sample_record(1, None, &["zeta"]);
        alpha.toc_url = "https://example.com/z".to_string();
        let mut end = sample_record(2, None, &["alpha"]);
        end.end = true;
        end.toc_url = "https://example.com/a".to_string();
        db.insert(alpha);
        db.insert(end);

        // tags: "alpha" < "zeta"
        let by_tags = db
            .sort_by("tags", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(by_tags, vec![2, 1]);
        // status: "" (no flags) < "完結"
        let by_status = db
            .sort_by("status", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(by_status, vec![1, 2]);
        // toc_url: "https://example.com/a" < "https://example.com/z"
        let by_url = db
            .sort_by("toc_url", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(by_url, vec![2, 1]);
    }

    #[test]
    fn sort_by_new_arrivals_date_matches_option_cmp_semantics() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        let mut a = sample_record(1, None, &[]);
        a.new_arrivals_date = None;
        let mut b = sample_record(2, None, &[]);
        b.new_arrivals_date = Some(Utc.timestamp_opt(1_700_000_100, 0).unwrap());
        let mut c = sample_record(3, None, &[]);
        c.new_arrivals_date = Some(Utc.timestamp_opt(1_700_000_300, 0).unwrap());
        db.insert(a);
        db.insert(b);
        db.insert(c);

        // update.rs:553 と整合: Option::cmp の自然順序 (None < Some)。
        let asc = db
            .sort_by("new_arrivals_date", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(asc, vec![1, 2, 3]);
        let desc = db
            .sort_by("new_arrivals_date", true)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(desc, vec![3, 2, 1]);
    }

    #[test]
    fn sort_by_unknown_key_falls_back_to_id() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        db.insert(sample_record(2, Some(5), &[]));
        db.insert(sample_record(1, Some(3), &[]));

        let order = db
            .sort_by("not_a_key", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(order, vec![1, 2]);
    }

    fn sample_record(id: i64, length: Option<i64>, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: format!("title {id}"),
            file_title: format!("title {id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "Example".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }
}

#[cfg(feature = "native-runtime")]
fn rename_imported_file(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => format!("{name}.imported-{stamp}"),
        None => return,
    };
    let _ = std::fs::rename(path, path.with_file_name(name));
}


#[cfg(feature = "native-runtime")]
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(data);
    hex::encode(digest)
}

/// Write `database.yaml` from the current SQLite records and remember its
/// SHA-256 in `app_state` so the next open can detect external edits.
#[cfg(feature = "native-runtime")]
fn mirror_records_yaml(
    state: &crate::native::sqlite::state::StateDb,
    repo: &crate::native::sqlite::SqliteNovelRepository,
    yaml_path: &std::path::Path,
) -> Result<()> {
    let conn = repo.conn_handle();
    let records = crate::native::sqlite::bulk::load_all_records(
        &conn.lock().expect("sqlite mutex poisoned"),
    )?;
    let content = serde_yaml::to_string(&records)?;
    if let Some(parent) = yaml_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::db::inventory::atomic_write(yaml_path, &content)?;
    let _ = state.set_raw_db("meta", "database_yaml_sha", &sha256_hex(content.as_bytes()));
    Ok(())
}