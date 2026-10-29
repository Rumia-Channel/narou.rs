use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::index_store::IndexStore;
use super::inventory::{Inventory, InventoryScope};
use super::novel_record::NovelRecord;
use super::sort::compare_records_by_key;

use crate::error::Result;

const ARCHIVE_ROOT_DIR: &str = "小説データ";
const DATABASE_NAME: &str = "database";

pub struct Database {
    data: HashMap<i64, NovelRecord>,
    /// Last acknowledged SQLite snapshot; save() publishes only local edits.
    #[cfg(feature = "native-runtime")]
    persisted: HashMap<i64, NovelRecord>,
    index: IndexStore,
    inventory: Inventory,
    archive_root: PathBuf,
    id_allocator: crate::platform::id_allocator::IdAllocator,
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
                    let yaml_path = inventory.inventory_path("database", InventoryScope::Local);
                    state.with_compat_lock(|| crate::db::inventory::with_exclusive_file_lock(&yaml_path, || {
                    let repo =
                        crate::native::sqlite::SqliteNovelRepository::new(state.conn_ref().clone());
                    let index_path =
                        inventory.inventory_path("database_index", InventoryScope::Local);
                    let file_content = match std::fs::read_to_string(&yaml_path) {
                        Ok(content) => Some(content),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    let stored_sha = state.get_raw_db("meta", "database_yaml_sha")?;
                    let file_sha = file_content
                        .as_ref()
                        .map(|content| sha256_hex(content.as_bytes()));
                    if state.compat_now() {
                        // Compat mode: database.yaml is authoritative.
                        match file_content {
                            Some(content) => {
                                let edited_projection = state.get_raw_db(
                                    "meta", "export_file_sha:database.yaml",
                                )?.is_some() && !state.is_export_file("database.yaml", &content)?;
                                let conn = repo.conn_handle();
                                let mut guard = conn.lock();
                                let tx = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                                    .map_err(crate::native::sqlite::sqlite_error)?;
                                if (file_sha.as_deref() != stored_sha.as_deref() || edited_projection)
                                    && !content.trim().is_empty()
                                {
                                    let parsed: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&content)?;
                                    crate::native::sqlite::bulk::replace_records_conn(&tx, &parsed)?;
                                }
                                remember_records_mirror(&tx, &content)?;
                                tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
                            }
                            None => {
                                // File deleted externally: recreate it from
                                // the database so narou.rb still sees the
                                // library.
                                mirror_records_yaml(&repo, &yaml_path)?;
                            }
                        }
                    } else {
                        // Non-compat: import the file when it carries data the
                        // DB has not mirrored yet, then retire it.
                        let exported = state.get_raw_db(
                            "meta", "export_file_sha:database.yaml",
                        )?.is_some();
                        let should_import = match (&file_content, &stored_sha) {
                            // Export provenance takes precedence over an older
                            // compat mirror SHA, including an intentional revert
                            // whose bytes happen to match that old mirror.
                            (Some(content), _) if exported => !content.trim().is_empty(),
                            (Some(content), Some(sha)) => {
                                file_sha.as_deref() != Some(sha.as_str())
                                    && !content.trim().is_empty()
                            }
                            (Some(content), None) => {
                                let conn = repo.conn_handle();
                                crate::native::sqlite::bulk::load_all_records(
                                    &conn.lock(),
                                )?.is_empty() && !content.trim().is_empty()
                            }
                            (None, _) => false,
                        };
                        if should_import
                            && !state.is_export_file("database.yaml", file_content.as_deref().unwrap_or(""))?
                        {
                            let parsed: BTreeMap<i64, NovelRecord> =
                                serde_yaml::from_str(file_content.as_deref().unwrap_or(""))?;
                            let conn = repo.conn_handle();
                            crate::native::sqlite::bulk::replace_all_records(
                                &mut conn.lock(),
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
                    // Novel rows exist now, so the freeze projection (skipped or
                    // partial while `novels` was still empty) can be completed.
                    state.resync_frozen_novels()?;
                    Ok(Some(repo))
                    }))?
                }
                None => None,
            }
        };
        let mut db = Self {
            data: HashMap::new(),
            #[cfg(feature = "native-runtime")]
            persisted: HashMap::new(),
            index: IndexStore::load(&inventory)?,
            inventory,
            archive_root,
            id_allocator: crate::platform::id_allocator::IdAllocator::default(),
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
            let guard = conn.lock();
            let status: String = guard
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(crate::native::sqlite::sqlite_error)?;
            return Ok(Some(status));
        }
        #[allow(unreachable_code)]
        Ok(None)
    }

    /// Verify every `objects`/`section_bodies` payload against its stored
    /// CRC-32 after decompression. Returns one `key: reason` line per
    /// corrupted row, or `None` in legacy YAML mode / when the tables do not
    /// exist yet.
    pub fn sqlite_payload_check(&self) -> Result<Option<Vec<String>>> {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let conn = repo.conn_handle();
            let guard = conn.lock();
            let issues = crate::native::sqlite::object_store::verify_payload_crc32(&guard)?;
            let lines = issues
                .into_iter()
                .map(|issue| format!("{}: {}", issue.key, issue.reason))
                .collect();
            return Ok(Some(lines));
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
                &conn.lock(),
            )?;
            self.data = loaded.into_iter().collect();
            self.persisted = self.data.clone();
            self.id_allocator.reconcile(
                self.data.keys().max().copied(), |id| self.data.contains_key(&id),
            );
            // 索引 (`by_toc_url` / `by_title`) はリレーショナル側の全件から作り直す。
            // `get_by_toc_url` / `find_by_title` は索引しか見ないため、SQLite モードで
            // 退避済みの `database_index.yaml` を引き継ぐと、既に登録済みの URL を
            // 再度ダウンロードしたときに重複レコードができる。
            self.index.reconcile(&self.data);
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
        self.id_allocator.reconcile(
            self.data.keys().max().copied(), |id| self.data.contains_key(&id),
        );
        // SQLite モードは上で return するため、ここは YAML モードのみ。
        self.index.reconcile(&self.data);
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        #[cfg(feature = "native-runtime")]
        if self.sqlite.is_some() {
            let baseline = self.persisted.clone();
            let edited = self.data.clone();
            return self.update_records(move |mut current| {
                for id in baseline.keys() {
                    if !edited.contains_key(id) {
                        current.remove(id);
                    }
                }
                for (&id, record) in &edited {
                    let mut record = record.clone();
                    record.id = id;
                    let Some(before) = baseline.get(&id) else {
                        if current.contains_key(&id) {
                            return Err(crate::error::NarouError::Database(format!(
                                "Novel ID {id} was registered by another writer; local insert was not saved",
                            )));
                        }
                        current.insert(id, record);
                        continue;
                    };
                    let before = serde_yaml::to_value(before)?;
                    let after = serde_yaml::to_value(&record)?;
                    if before == after {
                        continue;
                    }
                    // A stale field edit must not resurrect a deleted novel.
                    let Some(authoritative) = current.get(&id) else { continue; };
                    let mut merged = serde_yaml::to_value(authoritative)?;
                    let before = before.as_mapping().expect("record serializes as a map");
                    let after = after.as_mapping().expect("record serializes as a map");
                    let fields = merged.as_mapping_mut().expect("record serializes as a map");
                    // Flattened unknown fields participate in the same field-level
                    // delta, so concurrent keys are not overwritten by stale saves.
                    for key in before.keys().chain(after.keys()) {
                        if before.get(key) != after.get(key) {
                            match after.get(key) {
                                Some(value) => { fields.insert(key.clone(), value.clone()); }
                                None => { fields.remove(key); }
                            }
                        }
                    }
                    current.insert(id, serde_yaml::from_value(merged)?);
                }
                Ok((current, ()))
            });
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
        let written = tags_of_data(&self.data);
        let using_sqlite = self.using_sqlite();
        let sequence_path = self.inventory.inventory_path("tag_sequence", InventoryScope::Local);
        let yaml_path = self.inventory.inventory_path(DATABASE_NAME, InventoryScope::Local);
        // 作品と連番表を同じ直列化境界の中で確定する ✓
        crate::db::inventory::with_exclusive_file_lock(&sequence_path, || {
            crate::db::inventory::with_exclusive_file_lock(&yaml_path, || {
                // 事前状態は両方のロックを取ったあとに読む (ロックを取った前の
                // 状態を使うと、並行した書き込みと食い違う ✗)。
                let before_library = persisted_library_tags(&self.inventory)?;
                // 連番表を先に確定する。失敗したら作品を書かない (作品だけ
                // 確定した状態を作らない ✓)。
                let previous = write_tag_sequence_file(
                    &self.inventory,
                    &sequence_path,
                    before_library.as_ref(),
                    &written,
                )?;
                // 作品データの保存に失敗を注入する (テスト用 ✓)。
                let saved = (|| -> Result<()> {
                    crate::native::tag_sequence::notify_record_save_hook()?;
                    self.inventory
                        .save_raw(DATABASE_NAME, InventoryScope::Local, &content)?;
                    if !using_sqlite {
                        self.index.flush(&self.inventory)?;
                    }
                    Ok(())
                })();
                if let Err(error) = saved {
                    // 作品データが確定しなかったなら、連番表も元に戻す (タグ
                    // だけ登録された状態を作らない ✓)。
                    restore_tag_sequence_file(&self.inventory, &sequence_path, &previous)?;
                    return Err(error);
                }
                Ok(())
            })
        })
    }

    pub fn update_records<T, F>(&mut self, update: F) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>) -> Result<(BTreeMap<i64, NovelRecord>, T)>,
    {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let state = self.state_handle().ok_or_else(|| crate::error::NarouError::Database(
                "SQLite state is unavailable during record mutation".into(),
            ))?;
            let yaml_path = self.inventory.inventory_path(DATABASE_NAME, InventoryScope::Local);
            // 連番表のファイルロックを先に取り、作品との直列化順序を揃える
            // (他の管理状態のロックは連番表 → database.yaml の順)。
            let sequence_path = self.inventory.inventory_path("tag_sequence", InventoryScope::Local);
            // 書き込み前のライブラリ (バックフィルの材料) を作る。
            let before_library: std::cell::RefCell<
                Option<crate::application::tag_sequence::LibraryTags>,
            > = std::cell::RefCell::new(None);
            let (updated, result, compat) = state.with_compat_lock(|| {
                let compat = state.compat_now();
                let operation = || {
                    let conn = repo.conn_handle();
                    let mut guard = conn.lock();
                    // Acquire the database write lock BEFORE reading. The process
                    // mutex alone does not exclude separate processes/connections.
                    let tx = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .map_err(crate::native::sqlite::sqlite_error)?;
                    let mut current = crate::native::sqlite::bulk::load_all_records(&tx)?;
                    if compat {
                        match std::fs::read_to_string(&yaml_path) {
                            Ok(raw) => {
                                current = if raw.trim().is_empty() { BTreeMap::new() }
                                    else { serde_yaml::from_str(&raw)? };
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    for (&id, record) in &mut current { record.id = id; }
                    *before_library.borrow_mut() = Some(crate::application::tag_sequence::library_tags(
                        current.iter().map(|(id, record)| (*id, record.tags.as_slice())),
                        |tag| !crate::virtual_novel::is_internal_tag(tag),
                    ));
                    let (mut updated, result) = update(current)?;
                    for (&id, record) in &mut updated { record.id = id; }
                    crate::native::sqlite::bulk::replace_records_conn(&tx, &updated)?;
                    // 連番表を作品と同じトランザクションで確定する。別の
                    // トランザクションにすると「作品を保存した順」と「連番を
                    // 振った順」がずれる。
                    let written = tags_of_records(&updated);
                    // 互換モードではファイルが正なので、ファイルから読む。
                    let current_sequence = if compat {
                        crate::db::inventory::read_optional_yaml_file(&sequence_path)?
                    } else {
                        state
                            .read_state_row_conn(&tx, "inv", "tag_sequence")?
                            .unwrap_or_default()
                    };
                    let (content, changed) = crate::native::tag_sequence::advance_content(
                        &current_sequence,
                        before_library.borrow().as_ref(),
                        &written,
                    )?;
                    if changed {
                        state.write_state_row_conn(&tx, "inv", "tag_sequence", &content)?;
                        if compat {
                            // 互換モードではファイルが正。鏡と SHA を同じ
                            // トランザクションで更新する。
                            crate::db::inventory::atomic_write_locked(&sequence_path, &content)?;
                            state.finish_file_row_conn(&tx, "tag_sequence.yaml", &content)?;
                        }
                    }
                    if compat {
                        let content = serde_yaml::to_string(&updated)?;
                        // The file lock is already held. Failed publication rolls
                        // back the SQL mutation and does not acknowledge a new SHA.
                        crate::db::inventory::atomic_write_locked(&yaml_path, &content)?;
                        remember_records_mirror(&tx, &content)?;
                    }
                    tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
                    Ok((updated, result, compat))
                };
                if compat {
                    crate::db::inventory::with_exclusive_file_lock(&sequence_path, || {
                        crate::db::inventory::with_exclusive_file_lock(&yaml_path, operation)
                    })
                } else {
                    operation()
                }
            })?;
            self.data = updated.into_iter().collect();
            self.persisted = self.data.clone();
            self.id_allocator.reconcile(
                self.data.keys().max().copied(), |id| self.data.contains_key(&id),
            );
            self.index.reconcile(&self.data);
            if compat {
                self.index.flush(&self.inventory)?;
            }
            return Ok(result);
        }
        let before_library: std::cell::RefCell<
            Option<crate::application::tag_sequence::LibraryTags>,
        > = std::cell::RefCell::new(None);
        let previous_sequence: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let sequence_path = self.inventory.inventory_path("tag_sequence", InventoryScope::Local);
        // 連番表のロックを先に取り、作品の書き込みと同じ直列化境界の中で
        // 連番表も確定する (別に更新すると順序が作品とずれる)。
        let result = crate::db::inventory::with_exclusive_file_lock(&sequence_path, || {
            let outcome = self.inventory.update_yaml(
                DATABASE_NAME,
                InventoryScope::Local,
                |mut current: BTreeMap<i64, NovelRecord>| {
                    for (id, record) in current.iter_mut() {
                        record.id = *id;
                    }
                    *before_library.borrow_mut() =
                        Some(crate::application::tag_sequence::library_tags(
                            current.iter().map(|(id, record)| (*id, record.tags.as_slice())),
                            |tag| !crate::virtual_novel::is_internal_tag(tag),
                        ));
                    let (mut updated, result) = update(current)?;
                    for (id, record) in updated.iter_mut() {
                        record.id = *id;
                    }
                    // 連番表を作品と同じロックの中で確定する ✓
                    let written = tags_of_records(&updated);
                    let previous = write_tag_sequence_file(
                        &self.inventory,
                        &sequence_path,
                        before_library.borrow().as_ref(),
                        &written,
                    )?;
                    *previous_sequence.borrow_mut() = Some(previous);
                    // 作品データの保存に失敗を注入する (テスト用 ✓)。
                    crate::native::tag_sequence::notify_record_save_hook()?;
                    Ok((updated, result))
                },
            );
            if outcome.is_err()
                && let Some(previous) = previous_sequence.borrow().as_ref()
            {
                // 作品データが確定しなかったなら、連番表も元に戻す (タグだけ
                // 登録された状態を作らない ✓)。
                restore_tag_sequence_file(&self.inventory, &sequence_path, previous)?;
            }
            outcome
        })?;
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
        self.id_allocator.commit(id);
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
        self.data.values().find(|&record| record.title.to_lowercase() == lower).map(|v| v as _)
    }

    /// Reserve an ID until record publication. SQLite ownership is durable
    /// across processes; YAML retains its existing process-local allocator.
    pub fn allocate_id(&mut self) -> Result<crate::platform::NovelIdReservation> {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let conn = repo.conn_handle();
            let mut guard = conn.lock();
            let tx = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(crate::native::sqlite::sqlite_error)?;
            tx.execute(crate::db::novel_query::ALLOCATE_STEP_SQL, [])
                .map_err(crate::native::sqlite::sqlite_error)?;
            tx.execute(crate::db::novel_query::ALLOCATE_BUMP_SQL, [])
                .map_err(crate::native::sqlite::sqlite_error)?;
            let (id, generation) = tx.query_row(
                crate::db::novel_query::ALLOCATE_READ_SQL, [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
            return Ok(crate::platform::NovelIdReservation::new(
                crate::platform::NovelId(id), generation,
            ));
        }
        self.id_allocator.allocate()
    }

    pub fn release_id(&mut self, reservation: crate::platform::NovelIdReservation) {
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let conn = repo.conn_handle();
            // The public release seam is infallible. A failed release leaves
            // ownership reserved (safe leakage), never available to two writers.
            let _ = conn.lock().execute(
                crate::db::novel_query::RELEASE_ID_SQL,
                [reservation.id().0, reservation.generation()],
            );
            return;
        }
        let committed = self.data.contains_key(&reservation.id().0);
        self.id_allocator.release(reservation, committed);
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

/// 保存済みのライブラリのタグを読む。
///
/// バックフィル (既存タグの登録順) の材料にする。読めなければ `None`。
fn persisted_library_tags(
    inventory: &Inventory,
) -> Result<Option<crate::application::tag_sequence::LibraryTags>> {
    let raw = inventory.load_raw(DATABASE_NAME, InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let records: BTreeMap<i64, NovelRecord> = match serde_yaml::from_str(&raw) {
        Ok(records) => records,
        Err(_) => return Ok(None),
    };
    Ok(Some(crate::application::tag_sequence::library_tags(
        records
            .iter()
            .map(|(id, record)| (*id, record.tags.as_slice())),
        |tag| !crate::virtual_novel::is_internal_tag(tag),
    )))
}

/// 小説に付いたタグ (小説 ID 順 → 小説内の位置順)。
///
/// タグの登録順を進めるとき、新しいタグを判定する材料になる。
fn tags_of_records(records: &BTreeMap<i64, NovelRecord>) -> Vec<&str> {
    records
        .values()
        .flat_map(|record| record.tags.iter().map(String::as_str))
        .filter(|tag| !crate::virtual_novel::is_internal_tag(tag))
        .collect()
}

/// 連番表のファイルを更新する。
///
/// 呼び出し側が `path` の排他ロックを保持している前提で、記録の保存と同じ
/// 直列化境界の中で連番表を確定する。`before` は書き込み前のライブラリのタグ
/// (連番表が未初期化のときのバックフィルに使う)、`written` は書き込み後の
/// 小説に付いたタグ。
///
/// 返り値は直前の内容 (作品データの保存が失敗したときに元へ戻すため ✓)。
fn write_tag_sequence_file(
    inventory: &Inventory,
    path: &Path,
    before: Option<&crate::application::tag_sequence::LibraryTags>,
    written: &[&str],
) -> Result<String> {
    let current = crate::db::inventory::read_optional_yaml_file(path)?;
    let (content, changed) =
        crate::native::tag_sequence::advance_content(&current, before, written)?;
    if changed {
        crate::db::inventory::atomic_write_locked(path, &content)?;
        // 在庫のキャッシュはファイルの更新時刻で検証するが、直接書いた
        // 場合は読み直さないので明示的に捨てる。
        inventory.unload("tag_sequence");
    }
    Ok(current)
}

/// 連番表のファイルを直前の内容に戻す。
///
/// 作品データの保存に失敗して作品だけが確定しなかったとき、連番表に残った
/// タグを消して順序のずれを防ぐ (呼び出し側がロックを保持している前提 ✓)。
fn restore_tag_sequence_file(
    inventory: &Inventory,
    path: &Path,
    previous: &str,
) -> Result<()> {
    if previous.trim().is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    } else {
        crate::db::inventory::atomic_write_locked(path, previous)?;
    }
    inventory.unload("tag_sequence");
    Ok(())
}

/// 小説に付いたタグ (小説 ID 順 → 小説内の位置順)。
///
/// `data` は ID 順を持たないため、ソートしてから並べる (登録順の決定を
/// 安定させるため)。
fn tags_of_data(data: &HashMap<i64, NovelRecord>) -> Vec<&str> {
    let mut ids: Vec<i64> = data.keys().copied().collect();
    ids.sort_unstable();
    ids.iter()
        .filter_map(|id| data.get(id))
        .flat_map(|record| record.tags.iter().map(String::as_str))
        .filter(|tag| !crate::virtual_novel::is_internal_tag(tag))
        .collect()
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
    repo: &crate::native::sqlite::SqliteNovelRepository,
    yaml_path: &std::path::Path,
) -> Result<()> {
    let conn = repo.conn_handle();
    let mut guard = conn.lock();
    let tx = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(crate::native::sqlite::sqlite_error)?;
    let records = crate::native::sqlite::bulk::load_all_records(&tx)?;
    let content = serde_yaml::to_string(&records)?;
    if let Some(parent) = yaml_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Caller holds the root compat lock and database.yaml file lock, so the
    // authoritative read above cannot race an acknowledged compat mutation.
    crate::db::inventory::atomic_write_locked(yaml_path, &content)?;
    remember_records_mirror(&tx, &content)?;
    tx.commit().map_err(crate::native::sqlite::sqlite_error)
}

/// A successful ordinary mirror supersedes any interrupted export/cutover
/// projection. Keep only its actual bytes owned, not historical mirror hashes.
#[cfg(feature = "native-runtime")]
fn remember_records_mirror(tx: &rusqlite::Transaction<'_>, content: &str) -> Result<()> {
    let hash = sha256_hex(content.as_bytes());
    let provenance = serde_yaml::to_string(&[Some(hash.clone()), None])?;
    for (key, value) in [("database_yaml_sha", hash), ("export_file_sha:database.yaml", provenance)] {
        tx.execute(
            "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES ('meta', ?, '{}', ?)
             ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml",
            rusqlite::params![key, value],
        ).map_err(crate::native::sqlite::sqlite_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::db::inventory::{Inventory, InventoryScope};
    use crate::db::novel_record::NovelRecord;
    use chrono::{TimeZone, Utc};


    /// SQLite モードでも、開き直した後に toc_url / title で引けること。
    /// 索引をリレーショナル側のデータから作り直さないと、既に登録済みの URL を
    /// もう一度ダウンロードしたときに重複レコードができる。
    #[test]
    #[cfg(feature = "native-runtime")]
    fn sqlite_mode_looks_up_novels_by_toc_url_and_title_after_reopen() {
        use crate::native::sqlite::state::{self, StorageMode};

        let _global = crate::test_support::global_state_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        state::write_mode(&root.join(".narou"), StorageMode::Sqlite).unwrap();

        let toc_url = "https://kakuyomu.jp/works/1177354055617350769";
        let mut db = Database::with_root(root.clone()).unwrap();
        db.update_records(|mut records| {
            records.insert(1, lookup_test_record(1, "テスト作品", toc_url));
            Ok((records, ()))
        })
        .unwrap();
        drop(db);

        // 新しいプロセスで開き直したのと同じ状態。
        let reopened = Database::with_root(root).unwrap();
        assert_eq!(
            reopened.get_by_toc_url(toc_url).map(|record| record.id),
            Some(1),
            "toc_url lookup must survive a reopen in SQLite mode"
        );
        assert_eq!(
            reopened.find_by_title("テスト作品").map(|record| record.id),
            Some(1),
            "title lookup must survive a reopen in SQLite mode"
        );
    }

    #[cfg(feature = "native-runtime")]
    fn lookup_test_record(id: i64, title: &str, toc_url: &str) -> NovelRecord {
        NovelRecord {
            id,
            author: "作者".to_string(),
            title: title.to_string(),
            file_title: title.to_string(),
            toc_url: toc_url.to_string(),
            sitename: "カクヨム".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn database_parity_save_preserves_unknown_fields_and_zero_id() {
    let _global = crate::test_support::global_state_guard();
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
    }

    #[test]
    fn update_records_merges_against_current_database_yaml() {
    let _global = crate::test_support::global_state_guard();
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
        let _global = crate::test_support::global_state_guard();
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
    fn sort_by_last_check_date_uses_compare_optional() {
        let _global = crate::test_support::global_state_guard();
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
        let _global = crate::test_support::global_state_guard();
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
        let _global = crate::test_support::global_state_guard();
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
        let _global = crate::test_support::global_state_guard();
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
        let _global = crate::test_support::global_state_guard();
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

    /// 2 つの書き込みの割り込み (順序の入れ替わり) を再現しても、連番表は
    /// 作品を保存した順を保つ。
    ///
    /// 連番表の更新が作品の保存と同じ直列化境界 (ロック) の中で行われていないと、
    /// 1 つ目の書き込みが作品だけ保存して止まっている間に 2 つ目が連番表を進め、
    /// 登録順が作品と逆順になる。
    #[test]
    fn concurrent_writers_keep_the_registration_order() {
        let _global = crate::test_support::global_state_guard();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let root = temp.path().to_path_buf();

        let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let first = {
            let root = root.clone();
            std::thread::spawn(move || {
                crate::native::tag_sequence::set_advance_hook(move || {
                    let _ = paused_tx.send(());
                    let _ = resume_rx.recv();
                });
                let mut db = Database::with_root(root).unwrap();
                db.update_records(|mut records| {
                    records.insert(1, sample_record(1, None, &["first"]));
                    Ok((records, ()))
                })
                .unwrap();
            })
        };
        // 1 つ目の書き込みは連番表の更新で止まる (ロックを保持したまま ✓)。
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let second = {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut db = Database::with_root(root).unwrap();
                db.update_records(|mut records| {
                    records.insert(2, sample_record(2, None, &["second"]));
                    Ok((records, ()))
                })
                .unwrap();
                let _ = done_tx.send(());
            })
        };
        // 2 つ目の書き込みは直列化境界に入れない (作品だけの保存はしない ✓)。
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "作品だけの保存が先に完了してはいけない"
        );
        let _ = resume_tx.send(());
        first.join().unwrap();
        second.join().unwrap();

        let inventory = Inventory::new(root);
        let raw = inventory
            .load_raw("tag_sequence", InventoryScope::Local)
            .unwrap();
        let sequence: crate::application::tag_sequence::TagSequence =
            serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            sequence.order(),
            ["first".to_string(), "second".to_string()],
            "登録順は作品を保存した順を保つ"
        );
    }

    /// `Database::save()` の直接経路でも、別の書き込みが割り込めない
    /// (連番表の更新と作品データが同じ境界の中なので ✓)。
    #[test]
    fn concurrent_saves_keep_the_registration_order() {
        let _global = crate::test_support::global_state_guard();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let root = temp.path().to_path_buf();

        let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let first = {
            let root = root.clone();
            std::thread::spawn(move || {
                crate::native::tag_sequence::set_advance_hook(move || {
                    let _ = paused_tx.send(());
                    let _ = resume_rx.recv();
                });
                let mut db = Database::with_root(root).unwrap();
                db.insert(sample_record(1, None, &["first"]));
                db.save().unwrap();
            })
        };
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let second = {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut db = Database::with_root(root).unwrap();
                db.insert(sample_record(2, None, &["second"]));
                db.save().unwrap();
                let _ = done_tx.send(());
            })
        };
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "作品だけの保存が先に完了してはいけない"
        );
        let _ = resume_tx.send(());
        first.join().unwrap();
        second.join().unwrap();

        let inventory = Inventory::new(root);
        let raw = inventory
            .load_raw("tag_sequence", InventoryScope::Local)
            .unwrap();
        let sequence: crate::application::tag_sequence::TagSequence =
            serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            sequence.order(),
            ["first".to_string(), "second".to_string()],
            "登録順は作品を保存した順を保つ"
        );
    }

    /// 作品データの保存に失敗したら、連番表も元に戻さなければならない。
    ///
    /// 連番表を先に確定する実装では、作品データの保存が失敗したときに
    /// 連番表だけ進んだ状態になる。そのままだと「実際に保存された順」と
    /// 表示順がずれる (X の保存失敗 → Y の保存成功 → X の保存成功で
    /// Y→X なのに X→Y と表示される) ので、連番表を巻き戻す。
    #[test]
    fn a_failed_record_save_restores_the_registration_order() {
        let _global = crate::test_support::global_state_guard();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let root = temp.path().to_path_buf();

        // 1 回目の書き込みは作品データの保存で失敗させる。
        {
            let root = root.clone();
            let writer = std::thread::spawn(move || {
                crate::native::tag_sequence::set_record_save_hook(|| {
                    Err(crate::error::NarouError::Database("injected".to_string()))
                });
                let mut db = Database::with_root(root).unwrap();
                db.update_records(|mut records| {
                    records.insert(1, sample_record(1, None, &["x"]));
                    Ok((records, ()))
                })
            });
            let error = writer.join().unwrap().expect_err("注入した失敗で止まる");
            assert!(
                error.to_string().contains("injected"),
                "注入した失敗が返る: {error}"
            );
        }

        // 失敗した分のタグは連番表に残らない。
        let inventory = Inventory::new(root.clone());
        let raw = inventory
            .load_raw("tag_sequence", InventoryScope::Local)
            .unwrap();
        assert!(
            !raw.contains("x"),
            "作品データが保存されていないタグは連番表に残さない: {raw}"
        );

        // 以後の保存は通常どおり並ぶ (失敗した分が後から登録される ✓)。
        let mut db = Database::with_root(root.clone()).unwrap();
        db.update_records(|mut records| {
            records.insert(2, sample_record(2, None, &["y"]));
            Ok((records, ()))
        })
        .unwrap();
        let mut db = Database::with_root(root.clone()).unwrap();
        db.update_records(|mut records| {
            records.insert(1, sample_record(1, None, &["x"]));
            Ok((records, ()))
        })
        .unwrap();

        let inventory = Inventory::new(root);
        let raw = inventory
            .load_raw("tag_sequence", InventoryScope::Local)
            .unwrap();
        let sequence: crate::application::tag_sequence::TagSequence =
            serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            sequence.order(),
            ["y".to_string(), "x".to_string()],
            "保存できた順に並ぶ"
        );
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
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }
}