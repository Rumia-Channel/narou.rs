use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::index_store::IndexStore;
use super::inventory::{Inventory, InventoryScope};
use super::novel_record::NovelRecord;
use super::sort::compare_records_by_key;

use crate::error::Result;

const ARCHIVE_ROOT_DIR: &str = "小説データ";

#[path = "id_reservations.rs"]
pub(crate) mod id_reservations;
#[path = "record_mutation.rs"]
mod record_mutation;
const DATABASE_NAME: &str = "database";

pub struct Database {
    data: HashMap<i64, NovelRecord>,
    /// Last acknowledged persisted snapshot; save() publishes only local edits.
    #[cfg(feature = "native-runtime")]
    persisted: HashMap<i64, NovelRecord>,
    index: IndexStore,
    inventory: Inventory,
    archive_root: PathBuf,
    /// SQLite backend (P2). `None` when `NAROU_RS_LEGACY_YAML` is set or the
    /// database directory cannot host `db.sqlite`; then every mutation runs
    /// through the same locked `database.yaml` read-modify-write boundary.
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
        crate::native::transfer::recover_pending_metadata_before_open(inventory.root_dir())?;

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


    /// Mode lookup while the snapshot already owns the primary mutex.
    pub(crate) fn metadata_is_yaml_in_snapshot(&self, primary: Option<&rusqlite::Connection>) -> bool {
        primary.is_none_or(|connection| self.state_handle()
            .is_none_or(|state| state.compat_now_with_connection(connection)))
    }

    /// Serialized `database_index.yaml` payload for `db export-yaml`.
    pub fn index_yaml(&self) -> Result<String> {
        self.index.to_yaml()
    }

    /// Capture authoritative inventory metadata under the publication lock order.
    /// A SQLite callback may create a consistent `VACUUM INTO` image. It must not
    /// call adapters that reacquire the primary connection or copy novel bodies.
    pub(crate) fn with_library_metadata_snapshot<T, F>(&self, capture: F) -> Result<T>
    where
        F: FnOnce(&Inventory, Option<&rusqlite::Connection>) -> Result<T>,
    {
        let sequence_path = self.inventory.inventory_path("tag_sequence", InventoryScope::Local);
        let members_path = self.inventory.inventory_path("virtual_novel_members", InventoryScope::Local);
        let database_path = self.inventory.inventory_path(DATABASE_NAME, InventoryScope::Local);
        let operation = || crate::db::inventory::with_exclusive_file_lock(&sequence_path, || {
            crate::db::inventory::with_exclusive_file_lock(&members_path, || {
                crate::db::inventory::with_exclusive_file_lock(&database_path, || {
                    crate::native::transfer::recover_pending_metadata_locked(self.inventory.root_dir())?;
                    if let Some(repo) = &self.sqlite {
                        let connection = repo.conn_handle();
                        let guard = connection.lock();
                        capture(&self.inventory, Some(&guard))
                    } else {
                        capture(&self.inventory, None)
                    }
                })
            })
        });
        match self.state_handle() {
            Some(state) => state.with_compat_lock(operation),
            None => operation(),
        }
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
        self.persisted = self.data.clone();
        // SQLite モードは上で return するため、ここは YAML モードのみ。
        self.index.reconcile(&self.data);
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        let baseline = self.persisted.clone();
        let edited = self.data.clone();
        self.update_records(move |mut current| {
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
        })
    }

    pub fn update_records<T, F>(&mut self, update: F) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>) -> Result<(BTreeMap<i64, NovelRecord>, T)>,
    {
        self.update_records_inner(false, None, None, |records, _| {
            let (records, result) = update(records)?;
            Ok((records, crate::virtual_novel::VirtualNovelMembers::default(), result))
        })
    }

    /// The caller holds these original OPEN proofs before DATABASE and
    /// metadata authority. Repair only their live portable virtual origins;
    /// never acquire or reconstruct a writer for unrelated metadata rows.
    pub(crate) fn update_records_with_fences<T, F>(
        &mut self, fences: &[crate::platform::WriteFence], update: F,
    ) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>) -> Result<(BTreeMap<i64, NovelRecord>, T)>,
    {
        self.update_records_inner(false, None, Some(record_mutation::RecordMutation::VirtualFences(fences)), |records, _| {
            let (records, result) = update(records)?;
            Ok((records, crate::virtual_novel::VirtualNovelMembers::default(), result))
        })
    }

    /// Publish virtual membership, parent metadata and tag registration together.
    #[cfg(test)]
    pub(crate) fn update_virtual_records<T, F>(&mut self, update: F) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers)
            -> Result<(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers, T)>,
    {
        self.update_records_inner(true, None, None, update)
    }

    /// Original OPEN proofs remain held through virtual metadata mutation and
    /// deterministic recovery of a previously committed portable origin.
    pub(crate) fn update_virtual_records_with_fences<T, F>(
        &mut self, fences: &[crate::platform::WriteFence], update: F,
    ) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers)
            -> Result<(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers, T)>,
    {
        self.update_records_inner(true, None, Some(record_mutation::RecordMutation::VirtualFences(fences)), update)
    }

    /// Publish transfer metadata and its durable receipt in one commit boundary.
    /// The caller holds the publication gate and owns durable YAML preimages.
    pub(crate) fn update_transfer_records<T, F>(
        &mut self,
        marker: &crate::native::transfer::TransferCommitMarker,
        update: F,
    ) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers)
            -> Result<(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers, T)>,
    {
        self.update_records_inner(true, Some(marker), None, update)
    }

    pub(crate) fn reserve_create_identity(
        &mut self, fence: &crate::platform::WriteFence,
        reservation: &crate::platform::NovelIdReservation, toc_url: &str, empty_owned: bool,
    ) -> Result<()> {
        record_mutation::reserve_identity(self, fence, reservation, toc_url, empty_owned)
    }

    pub(crate) fn assert_current_identity_conn(
        connection: &rusqlite::Connection, id: crate::platform::NovelId,
        prefix: &crate::platform::ObjectPrefix, folded_url: &str,
    ) -> Result<bool> {
        record_mutation::assert_current_identity_conn(connection, id, prefix, folded_url)
    }

    pub(crate) fn assert_current_namespace_conn(
        connection: &rusqlite::Connection, id: crate::platform::NovelId,
        prefix: &crate::platform::ObjectPrefix,
    ) -> Result<bool> {
        record_mutation::assert_current_namespace_conn(connection, id, prefix)
    }

    /// Caller holds the original OS gate proof. Keep the metadata authority
    /// until the supplied synchronous gate-close commit is durably recorded.
    pub(crate) fn with_removal_identity<T>(
        &self, fence: &crate::platform::WriteFence, id: crate::platform::NovelId,
        commit: impl FnOnce(Option<&rusqlite::Connection>) -> Result<T>,
    ) -> Result<T> {
        self.with_library_metadata_snapshot(|inventory, primary| {
            crate::native::transfer::with_control_metadata_snapshot(inventory.root_dir(), primary, |control| {
                let transaction = rusqlite::Transaction::new_unchecked(control, rusqlite::TransactionBehavior::Immediate)
                    .map_err(crate::native::sqlite::sqlite_error)?;
                let records = if self.metadata_is_yaml_in_snapshot(primary) { None } else { Some(&*transaction) };
                record_mutation::repair_virtual_before_removal(
                    inventory, records, &transaction, primary.is_none(), fence, id,
                )?;
                crate::native::transfer::assert_removal_identity(inventory, records, &transaction, fence, id)?;
                let result = commit(if primary.is_some() { Some(&transaction) } else { None })?;
                transaction.commit().map_err(crate::native::sqlite::sqlite_error)?;
                Ok(result)
            })
        })
    }

    pub(crate) fn apply_create_record(
        &mut self, fence: &crate::platform::WriteFence,
        reservation: &crate::platform::NovelIdReservation, record: NovelRecord,
    ) -> Result<()> {
        if record.id != reservation.id().0 || !record_mutation::record_in_prefix(&record, &fence.prefix)? {
            return Err(crate::error::NarouError::Platform("identity_conflict: create record does not match its reservation".into()));
        }
        self.update_records_inner(false, None, Some(record_mutation::RecordMutation::Create {
            fence, reservation, allow_existing: true,
        }), move |mut records, members| {
            if let Some(existing) = records.get(&record.id) {
                if crate::db::novel_codec::fold(&existing.toc_url) != crate::db::novel_codec::fold(&record.toc_url)
                    || !record_mutation::record_in_prefix(existing, &fence.prefix)?
                {
                    return Err(crate::error::NarouError::Platform("identity_conflict: published create identity changed".into()));
                }
                // A lost acknowledgement must not overwrite subsequent edits.
            } else {
                records.insert(record.id, record);
            }
            Ok((records, members, ()))
        })
    }

    pub(crate) fn update_virtual_records_for_create<T, F>(
        &mut self, fence: &crate::platform::WriteFence,
        reservation: &crate::platform::NovelIdReservation, update: F,
    ) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers)
            -> Result<(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers, T)>,
    {
        self.update_records_inner(true, None, Some(record_mutation::RecordMutation::Create {
            fence, reservation, allow_existing: false,
        }), update)
    }

    pub(crate) fn remove_closed_record(
        &mut self, prefix: &crate::platform::ObjectPrefix, owner: &str,
        epoch: u64, id: crate::platform::NovelId,
    ) -> Result<()> {
        self.update_records_inner(true, None, Some(record_mutation::RecordMutation::RemoveClosed {
            prefix, owner, epoch, id,
        }), |mut records, mut members| {
            records.remove(&id.0);
            members.remove(id.0);
            Ok((records, members, ()))
        })
    }

    /// Migrate one virtual novel's library-local `toc_url` to its portable
    /// URI and rebind every retained gate origin in the same immediate
    /// metadata authority. The caller must already hold the original OPEN
    /// gate proof (`fence`) for this exact record. `observed` is the URL the
    /// caller read; `candidate` is the generated portable URI. The actual
    /// current URL is returned so concurrent migrations converge on the
    /// first portable winner instead of rewinding it.
    pub(crate) fn rebind_virtual_identity(
        &mut self,
        fence: &crate::platform::WriteFence,
        id: crate::platform::NovelId,
        observed: &str,
        candidate: &str,
    ) -> Result<String> {
        self.update_records_inner(false, None, Some(record_mutation::RecordMutation::Rebind {
            fence, id, observed, candidate,
        }), move |mut records, members| {
            let entry = records.get_mut(&id.0).ok_or_else(|| crate::error::NarouError::NotFound(
                format!("ID: {} が見つかりません", id.0)))?;
            if !crate::virtual_novel::is_virtual_record(entry) {
                return Err(crate::error::NarouError::InvalidTarget(
                    format!("ID: {} はバーチャル小説ではありません", id.0)));
            }
            if !record_mutation::record_in_prefix(entry, &fence.prefix)? {
                return Err(crate::error::NarouError::Platform(
                    "identity_conflict: virtual identity record moved to another namespace".into()));
            }
            // A portable row is immutable and wins the race; only the
            // observed library-local URL is replaced.
            if !crate::virtual_novel::is_portable_toc_url(&entry.toc_url) && entry.toc_url == observed {
                if !crate::virtual_novel::is_portable_toc_url(candidate) {
                    return Err(crate::error::NarouError::Platform(
                        "identity_conflict: virtual rebind candidate is not portable".into()));
                }
                entry.toc_url = candidate.to_owned();
            }
            let current = entry.toc_url.clone();
            Ok((records, members, current))
        })
    }

    fn update_records_inner<T, F>(
        &mut self,
        update_members: bool,
        transfer_marker: Option<&crate::native::transfer::TransferCommitMarker>,
        mutation: Option<record_mutation::RecordMutation<'_>>,
        update: F,
    ) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers)
            -> Result<(BTreeMap<i64, NovelRecord>, crate::virtual_novel::VirtualNovelMembers, T)>,
    {
        let members_path = update_members.then(|| self.inventory.inventory_path("virtual_novel_members", InventoryScope::Local));
        let parse_members = |raw: Option<&str>| -> Result<crate::virtual_novel::VirtualNovelMembers> {
            let raw = raw.unwrap_or_default();
            if raw.trim().is_empty() { Ok(Default::default()) } else { Ok(serde_yaml::from_str(raw)?) }
        };
        #[cfg(feature = "native-runtime")]
        if let Some(repo) = &self.sqlite {
            let state = self.state_handle().ok_or_else(|| crate::error::NarouError::Database(
                "SQLite state is unavailable during record mutation".into(),
            ))?;
            let yaml_path = self.inventory.inventory_path(DATABASE_NAME, InventoryScope::Local);
            // Lock order: compat -> tag sequence -> membership -> database.
            let sequence_path = self.inventory.inventory_path("tag_sequence", InventoryScope::Local);
            // 書き込み前のライブラリ (バックフィルの材料) を作る。
            let before_library: std::cell::RefCell<
                Option<crate::application::tag_sequence::LibraryTags>,
            > = std::cell::RefCell::new(None);
            let finished_claims = std::cell::RefCell::new(None);
            let (updated, result, compat) = state.with_compat_lock(|| {
                let compat = state.compat_now();
                let operation = || {
                    let published = LockedFileUpdates::run(&self.inventory, transfer_marker, compat, |files| {
                    let conn = repo.conn_handle();
                    let mut guard = conn.lock();
                    // Acquire the database write lock BEFORE reading. The process
                    // mutex alone does not exclude separate processes/connections.
                    let tx = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .map_err(crate::native::sqlite::sqlite_error)?;
                    let mut current = crate::native::sqlite::bulk::load_all_records(&tx)?;
                    if compat && let Some(raw) = files.original(&yaml_path)? {
                        current = if raw.trim().is_empty() { BTreeMap::new() }
                            else { serde_yaml::from_str(raw)? };
                    }
                    for (&id, record) in &mut current { record.id = id; }
                    *before_library.borrow_mut() = Some(crate::application::tag_sequence::library_tags(
                        current.iter().map(|(id, record)| (*id, record.tags.as_slice())),
                        |tag| !crate::virtual_novel::is_internal_tag(tag),
                    ));
                    let member_yaml = if update_members {
                        state.read_state_row_conn(&tx, "inv", "virtual_novel_members")?
                    } else { None };
                    let members = if let Some(path) = members_path.as_deref() {
                        let file = if compat { files.original(path)? } else { None };
                        parse_members(file.or(member_yaml.as_deref()))?
                    } else { Default::default() };
                    let record_claims = record_mutation::prepare(
                        self.inventory.root_dir(), Some(&tx), &current, transfer_marker, mutation,
                    )?;
                    let (mut updated, members, result) = update(current, members)?;
                    for (&id, record) in &mut updated { record.id = id; }
                    record_claims.validate(&updated)?;
                    if let Some(marker) = transfer_marker {
                        crate::native::sqlite::content::invalidate_transfer_projection(&tx, marker.novel_id.0)?;
                    }
                    crate::native::sqlite::bulk::replace_records_conn(&tx, &updated)?;
                    record_claims.finish_sql(&tx)?;
                    // 連番表を作品と同じトランザクションで確定する。別の
                    // トランザクションにすると「作品を保存した順」と「連番を
                    // 振った順」がずれる。
                    let written = tags_of_records(&updated);
                    // A present compatibility file is authoritative, including
                    // an explicit empty file. A missing mirror retains SQL state.
                    let sequence_file = if compat { files.original(&sequence_path)? } else { None };
                    let current_sequence = match sequence_file {
                        Some(raw) => std::borrow::Cow::Borrowed(raw),
                        None => std::borrow::Cow::Owned(state.read_state_row_conn(&tx, "inv", "tag_sequence")?.unwrap_or_default()),
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
                            files.write(&sequence_path, &content)?;
                            state.finish_file_row_conn(&tx, "tag_sequence.yaml", &content)?;
                        }
                    }
                    if let Some(path) = members_path.as_deref() {
                        let content = crate::db::inventory::serialize_yaml_content(&members)?;
                        let changed = member_yaml.as_deref() != Some(content.as_str());
                        if changed {
                            state.write_state_row_conn(&tx, "inv", "virtual_novel_members", &content)?;
                        }
                        if compat {
                            let published = files.write(path, &content)?;
                            if published || changed {
                                state.finish_file_row_conn(&tx, "virtual_novel_members.yaml", &content)?;
                            }
                        }
                    }
                    if compat {
                        let content = serde_yaml::to_string(&updated)?;
                        // The file lock is already held. Failed publication rolls
                        // back the SQL mutation and does not acknowledge a new SHA.
                        files.write(&yaml_path, &content)?;
                        remember_records_mirror(&tx, &content)?;
                    }
                    files.publish_transfer()?;
                    let commit_key = transfer_marker.map(|marker| {
                        format!("commit:{}:{}", marker.session_id, marker.publication_id)
                    });
                    if let Some(marker) = transfer_marker {
                        let inserted = tx.execute(
                            "INSERT INTO transfer_backend_marks(key,value) VALUES (?1,?2)
                             ON CONFLICT(key) DO NOTHING",
                            rusqlite::params![commit_key.as_deref(), marker.receipt.as_slice()],
                        ).map_err(crate::native::sqlite::sqlite_error)?;
                        if inserted == 0 {
                            let same_receipt = tx.query_row(
                                "SELECT value FROM transfer_backend_marks WHERE key=?1",
                                [commit_key.as_deref()],
                                |row| Ok(matches!(
                                    row.get_ref(0)?,
                                    rusqlite::types::ValueRef::Blob(bytes) if bytes == marker.receipt.as_slice()
                                )),
                            ).map_err(crate::native::sqlite::sqlite_error)?;
                            if !same_receipt {
                                return Err(crate::error::NarouError::Database(
                                    "transfer publication receipt conflict".into(),
                                ));
                            }
                        }
                    }
                    let committed = tx.commit().map_err(crate::native::sqlite::sqlite_error);
                    if let Some(marker) = transfer_marker {
                        // A commit error can be an uncertain outcome. Never undo
                        // compatibility files after the durable receipt exists.
                        files.committed = committed.is_ok() || guard.query_row(
                            "SELECT value FROM transfer_backend_marks WHERE key=?1",
                            [commit_key.as_deref()],
                            |row| Ok(matches!(
                                row.get_ref(0)?,
                                rusqlite::types::ValueRef::Blob(bytes) if bytes == marker.receipt.as_slice()
                            )),
                        ).unwrap_or_else(|error| !matches!(error, rusqlite::Error::QueryReturnedNoRows));
                    }
                    committed?;
                    *finished_claims.borrow_mut() = Some(record_claims);
                    Ok((updated, result, compat))
                    })?;
                    if let Some(claims) = finished_claims.borrow().as_ref() {
                        // Postcommit, outside rollback but inside every held
                        // inventory lock and the original OS gate proof.
                        claims.finish_fs_origin(self.inventory.root_dir())?;
                    }
                    Ok(published)
                };
                if compat {
                    crate::db::inventory::with_exclusive_file_lock(&sequence_path, || {
                        let publish = || crate::db::inventory::with_exclusive_file_lock(&yaml_path, operation);
                        if let Some(path) = members_path.as_deref() {
                            crate::db::inventory::with_exclusive_file_lock(path, publish)
                        } else { publish() }
                    })
                } else {
                    operation()
                }
            })?;
            self.data = updated.into_iter().collect();
            self.persisted = self.data.clone();
            self.index.reconcile(&self.data);
            if compat {
                self.index.flush(&self.inventory)?;
            }
            return Ok(result);
        }
        let before_library: std::cell::RefCell<
            Option<crate::application::tag_sequence::LibraryTags>,
        > = std::cell::RefCell::new(None);
        let sequence_path = self.inventory.inventory_path("tag_sequence", InventoryScope::Local);
        let yaml_path = self.inventory.inventory_path(DATABASE_NAME, InventoryScope::Local);
        // 連番表のロックを先に取り、作品の書き込みと同じ直列化境界の中で
        // 連番表も確定する (別に更新すると順序が作品とずれる)。
        let result = crate::db::inventory::with_exclusive_file_lock(&sequence_path, || {
            let operation = || crate::db::inventory::with_exclusive_file_lock(&yaml_path, || {
                let (result, record_claims) = LockedFileUpdates::run(&self.inventory, transfer_marker, true, |files| {
                    let raw = files.original(&yaml_path)?.unwrap_or_default();
                    let mut current: BTreeMap<i64, NovelRecord> =
                        if raw.is_empty() { BTreeMap::new() } else { serde_yaml::from_str(raw)? };
                    for (id, record) in current.iter_mut() {
                        record.id = *id;
                    }
                    *before_library.borrow_mut() =
                        Some(crate::application::tag_sequence::library_tags(
                            current.iter().map(|(id, record)| (*id, record.tags.as_slice())),
                            |tag| !crate::virtual_novel::is_internal_tag(tag),
                        ));
                    let members = if let Some(path) = members_path.as_deref() {
                        parse_members(files.original(path)?)?
                    } else { Default::default() };
                    let record_claims = record_mutation::prepare(
                        self.inventory.root_dir(), None, &current, transfer_marker, mutation,
                    )?;
                    let (mut updated, members, result) = update(current, members)?;
                    for (id, record) in updated.iter_mut() {
                        record.id = *id;
                    }
                    record_claims.validate(&updated)?;
                    let written = tags_of_records(&updated);
                    let (content, changed) = crate::native::tag_sequence::advance_content(
                        files.original(&sequence_path)?.unwrap_or_default(),
                        before_library.borrow().as_ref(), &written,
                    )?;
                    if changed { files.write(&sequence_path, &content)?; }
                    if let Some(path) = members_path.as_deref() {
                        files.write(path, &crate::db::inventory::serialize_yaml_content(&members)?)?;
                    }
                    crate::native::tag_sequence::notify_record_save_hook()?;
                    files.write(&yaml_path, &crate::db::inventory::serialize_yaml_content(&updated)?)?;
                    id_reservations::retire_yaml(self, &updated)?;
                    files.publish_transfer()?;
                    if let Some(marker) = transfer_marker {
                        // The durable commit mark IS the immutable publication
                        // receipt; committed()/metadata recovery byte-compare it.
                        let committed = crate::db::inventory::atomic_write_locked(
                            &marker.yaml_commit_path,
                            std::str::from_utf8(&marker.receipt).map_err(|error| {
                                crate::error::NarouError::Platform(format!(
                                    "transfer receipt is not UTF-8 JSON: {error}"
                                ))
                            })?,
                        );
                        files.committed = committed.is_ok() || match std::fs::read(&marker.yaml_commit_path) {
                            Ok(bytes) => bytes == marker.receipt,
                            // Leave an uncertain outcome to durable journal
                            // recovery; do not silently roll back a committed mark.
                            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                        };
                        committed?;
                    }
                    Ok((result, record_claims))
                })?;
                record_claims.finish_yaml(self.inventory.root_dir())?;
                Ok(result)
            });
            if let Some(path) = members_path.as_deref() {
                crate::db::inventory::with_exclusive_file_lock(path, operation)
            } else { operation() }
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

    /// Reserve an ID durably across processes in either storage mode.
    pub fn allocate_id(&mut self) -> Result<crate::platform::NovelIdReservation> {
        id_reservations::allocate(self, None)
    }

    /// Reacquire the same durable generation; never allocate a replacement.
    pub fn resume_id(
        &mut self, id: crate::platform::NovelId, generation: i64,
    ) -> Result<crate::platform::NovelIdReservation> {
        id_reservations::resume(self, id, generation)
    }

    /// Retry the same publication owner without leaking a different ID.
    pub(crate) fn allocate_transfer_id(
        &mut self, owner: &str,
    ) -> Result<crate::platform::NovelIdReservation> {
        id_reservations::allocate(self, Some(owner))
    }

    pub(crate) fn release_transfer_id(&mut self, owner: &str) -> Result<()> {
        id_reservations::release_owner(self, owner)
    }

    pub(crate) fn ordinary_id_liveness_page(
        &self, cursor: Option<i64>, limit: usize,
    ) -> Result<id_reservations::PendingLiveness> {
        id_reservations::pending_liveness(self, cursor, limit)
    }

    pub(crate) fn release_creation_after_cleanup(
        &mut self, prefix: &crate::platform::ObjectPrefix, completed_epoch: u64,
        reservation: &crate::platform::NovelIdReservation,
    ) -> Result<()> {
        id_reservations::release_after_cleanup(self, prefix, completed_epoch, reservation)
    }

    pub fn release_id(&mut self, reservation: &crate::platform::NovelIdReservation) -> Result<()> {
        id_reservations::release(self, reservation)
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

/// Restore already-published YAML projections when a later write/commit fails.
/// Callers retain every file lock through publication and rollback. This is not
/// a crash-atomic multi-file filesystem transaction.
#[derive(Default)]
struct LockedFileUpdates<'a> {
    files: Vec<FileUpdate<'a>>,
    committed: bool,
    transfer_marker: Option<&'a crate::native::transfer::TransferCommitMarker>,
}

struct FileUpdate<'a> {
    path: &'a Path,
    original: Option<String>,
    written: bool,
    staged: bool,
}

impl<'a> LockedFileUpdates<'a> {
    fn run<T>(
        inventory: &Inventory,
        transfer_marker: Option<&'a crate::native::transfer::TransferCommitMarker>,
        recover_pending: bool,
        operation: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if recover_pending {
            crate::native::transfer::recover_pending_metadata_locked(inventory.root_dir())?;
        }
        let mut files = Self { transfer_marker, ..Self::default() };
        let result = operation(&mut files);
        files.finish(inventory, result)
    }

    fn entry(&mut self, path: &'a Path) -> Result<&mut FileUpdate<'a>> {
        let index = match self.files.iter().position(|entry| entry.path == path) {
            Some(index) => index,
            None => {
                let original = match crate::db::inventory::ensure_yaml_size_limit(path) {
                    Err(crate::error::NarouError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                    result => {
                        result?;
                        Some(std::fs::read_to_string(path)?)
                    }
                };
                self.files.push(FileUpdate { path, original, written: false, staged: false });
                self.files.len() - 1
            }
        };
        Ok(&mut self.files[index])
    }

    fn original(&mut self, path: &'a Path) -> Result<Option<&str>> {
        Ok(self.entry(path)?.original.as_deref())
    }

    fn write(&mut self, path: &'a Path, content: &str) -> Result<bool> {
        let marker = self.transfer_marker;
        let entry = self.entry(path)?;
        if entry.original.as_deref() == Some(content) { return Ok(false); }
        if let Some(marker) = marker {
            crate::native::transfer::stage_metadata(marker, path, entry.original.as_deref(), content)?;
            entry.staged = true;
            return Ok(true);
        }
        // Rename may succeed before directory synchronization returns an error.
        entry.written = true;
        crate::db::inventory::atomic_write_locked(path, content)?;
        Ok(true)
    }

    fn publish_transfer(&mut self) -> Result<()> {
        if let Some(marker) = self.transfer_marker {
            // All postimages and undo descriptors exist before the first
            // authoritative metadata file is replaced.
            for entry in self.files.iter_mut().filter(|entry| entry.staged) {
                crate::native::transfer::install_metadata(marker, entry.path)?;
                entry.written = true;
            }
        }
        Ok(())
    }

    fn finish<T>(self, inventory: &Inventory, result: Result<T>) -> Result<T> {
        let mut failures = Vec::new();
        if result.is_err() && !self.committed && self.files.iter().any(|entry| entry.staged)
            && let Some(marker) = self.transfer_marker
        {
            let restored = match marker.yaml_commit_path.parent() {
                Some(directory) => crate::native::transfer::recover_metadata_locked(
                    inventory.root_dir(), directory, false,
                ),
                None => Err(crate::error::NarouError::Database(
                    "transfer metadata journal has no publication directory".into(),
                )),
            };
            if let Err(error) = restored { failures.push(error.to_string()); }
        }
        for entry in self.files.into_iter().rev().filter(|entry| entry.written || entry.staged) {
            if let Some(name) = entry.path.file_stem().and_then(|name| name.to_str()) {
                inventory.unload(name);
            }
            if result.is_err() && !self.committed && self.transfer_marker.is_none() {
                let restored = match entry.original {
                    Some(content) => crate::db::inventory::atomic_write_locked(entry.path, &content),
                    None => match std::fs::remove_file(entry.path) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(error.into()),
                    },
                };
                if let Err(error) = restored { failures.push(format!("{}: {error}", entry.path.display())); }
            }
        }
        match result {
            Err(error) if !failures.is_empty() => Err(crate::error::NarouError::Database(
                format!("{error}; YAML rollback failed: {}", failures.join("; ")),
            )),
            result => result,
        }
    }
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

    fn seed_virtual_records(db: &mut Database) {
        db.update_virtual_records(|mut records, mut members| {
            records.insert(1, sample_record(1, None, &["member"]));
            records.insert(2, sample_record(2, None, &["member"]));
            let mut parent = sample_record(3, None, &["initial", "vs_3"]);
            parent.ncode = Some("vs_3".into());
            parent.general_all_no = Some(1);
            records.insert(3, parent);
            members.insert(3, vec![1]).unwrap();
            Ok((records, members, ()))
        }).unwrap();
    }

    #[test]
    fn a_failed_virtual_record_save_restores_membership_and_tag_order() {
        let _global = crate::test_support::global_state_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".narou")).unwrap();
        let mut db = Database::with_root(root.clone()).unwrap();
        seed_virtual_records(&mut db);
        let before = ["database.yaml", "virtual_novel_members.yaml", "tag_sequence.yaml"].map(|name| {
            let path = root.join(".narou").join(name);
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        });
        let writer_root = root.clone();
        std::thread::spawn(move || {
            let mut db = Database::with_root(writer_root).unwrap();
            crate::native::tag_sequence::set_record_save_hook(|| {
                Err(crate::error::NarouError::Database("injected publication failure".into()))
            });
            db.update_virtual_records(|mut records, mut members| {
                members.append(3, vec![2]).unwrap();
                let parent = records.get_mut(&3).unwrap();
                parent.general_all_no = Some(2);
                parent.tags.push("unpublished".into());
                Ok((records, members, ()))
            }).expect_err("the injected record publication must fail");
        }).join().unwrap();
        for (path, bytes) in before {
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{}", path.display());
        }
        let reopened = Database::with_root(root).unwrap();
        assert_eq!(reopened.get(3).unwrap().general_all_no, Some(1));
    }

    #[test]
    fn a_failed_virtual_commit_restores_compatibility_files() {
        let _global = crate::test_support::global_state_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        crate::native::sqlite::state::write_mode(&root.join(".narou"), crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
        let mut db = Database::with_root(root.clone()).unwrap();
        let state = db.state_handle().unwrap();
        state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
        seed_virtual_records(&mut db);
        let before = ["database.yaml", "virtual_novel_members.yaml", "tag_sequence.yaml"].map(|name| {
            let path = root.join(".narou").join(name);
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        });
        state.conn_ref().lock().execute_batch(
            "CREATE TABLE deferred_virtual_failure(novel_id INTEGER REFERENCES novels(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_virtual_commit AFTER UPDATE OF general_all_no ON novels WHEN NEW.id=3
             BEGIN INSERT INTO deferred_virtual_failure(novel_id) VALUES(999); END;"
        ).unwrap();
        db.update_virtual_records(|mut records, mut members| {
            members.append(3, vec![2]).unwrap();
            let parent = records.get_mut(&3).unwrap();
            parent.general_all_no = Some(2);
            parent.tags.push("unpublished".into());
            Ok((records, members, ()))
        }).expect_err("the deferred foreign key must reject commit");
        for (path, bytes) in before {
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{}", path.display());
        }
        let (raw, count): (String, i64) = state.conn_ref().lock().query_row(
            "SELECT a.value_yaml,n.general_all_no FROM app_state a,novels n WHERE a.scope='inv' AND a.key='virtual_novel_members' AND n.id=3",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        let members: crate::virtual_novel::VirtualNovelMembers = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(members.get(3), Some([1].as_slice()));
        assert_eq!(count, 1);
    }

    #[test]
    fn virtual_membership_uses_sqlite_when_its_compatibility_file_is_missing() {
        let _global = crate::test_support::global_state_guard();
        for explicit_empty in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().to_path_buf();
            crate::native::sqlite::state::write_mode(&root.join(".narou"), crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
            let mut db = Database::with_root(root.clone()).unwrap();
            let state = db.state_handle().unwrap();
            state.set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
            seed_virtual_records(&mut db);
            let path = root.join(".narou/virtual_novel_members.yaml");
            if explicit_empty { std::fs::write(&path, "").unwrap(); }
            else { std::fs::remove_file(&path).unwrap(); }
            db.update_virtual_records(|mut records, mut members| {
                let mut parent = sample_record(4, None, &["vs_4"]);
                parent.ncode = Some("vs_4".into());
                records.insert(4, parent);
                members.insert(4, vec![2]).unwrap();
                Ok((records, members, ()))
            }).unwrap();
            let members: crate::virtual_novel::VirtualNovelMembers =
                serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(members.get(3), (!explicit_empty).then_some([1].as_slice()));
            assert_eq!(members.get(4), Some([2].as_slice()));
            assert!(db.get(3).is_some() && db.get(4).is_some());
        }
    }

    #[test]
    fn virtual_publication_sync_failures_restore_every_published_file() {
        let _global = crate::test_support::global_state_guard();
        for sqlite_compat in [false, true] {
            for failed_name in ["database.yaml", "virtual_novel_members.yaml", "tag_sequence.yaml"] {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path().to_path_buf();
                std::fs::create_dir_all(root.join(".narou")).unwrap();
                if sqlite_compat {
                    crate::native::sqlite::state::write_mode(&root.join(".narou"), crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
                }
                let mut db = Database::with_root(root.clone()).unwrap();
                if sqlite_compat {
                    db.state_handle().unwrap().set_raw("inv", "local_setting", "narou-compat: true\n").unwrap();
                }
                seed_virtual_records(&mut db);
                let before = ["database.yaml", "virtual_novel_members.yaml", "tag_sequence.yaml"].map(|name| {
                    let path = root.join(".narou").join(name);
                    let bytes = std::fs::read(&path).unwrap();
                    (path, bytes)
                });
                let writer_root = root.clone();
                std::thread::spawn(move || {
                    let mut db = Database::with_root(writer_root.clone()).unwrap();
                    crate::db::inventory::fail_after_publication(writer_root.join(".narou").join(failed_name));
                    let error = db.update_virtual_records(|mut records, mut members| {
                        members.append(3, vec![2]).unwrap();
                        let parent = records.get_mut(&3).unwrap();
                        parent.general_all_no = Some(2);
                        parent.tags.push("unpublished".into());
                        Ok((records, members, ()))
                    }).unwrap_err();
                    assert!(error.to_string().contains("injected post-publication"));
                }).join().unwrap();
                for (path, bytes) in before {
                    assert_eq!(std::fs::read(&path).unwrap(), bytes, "sqlite={sqlite_compat}: {}", path.display());
                }
                assert_eq!(Database::with_root(root).unwrap().get(3).unwrap().general_all_no, Some(1));
            }
        }
    }

    #[test]
    fn record_save_sync_failures_restore_records_and_tag_order() {
        let _global = crate::test_support::global_state_guard();
        for failed_name in ["database.yaml", "tag_sequence.yaml"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().to_path_buf();
            std::fs::create_dir_all(root.join(".narou")).unwrap();
            let mut db = Database::with_root(root.clone()).unwrap();
            seed_virtual_records(&mut db);
            let before = ["database.yaml", "tag_sequence.yaml"].map(|name| {
                let path = root.join(".narou").join(name);
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            });
            let writer_root = root.clone();
            std::thread::spawn(move || {
                let mut db = Database::with_root(writer_root.clone()).unwrap();
                let record = db.data.get_mut(&1).unwrap();
                record.title = "unpublished title".into();
                record.tags.push("unpublished tag".into());
                crate::db::inventory::fail_after_publication(writer_root.join(".narou").join(failed_name));
                assert!(db.save().unwrap_err().to_string().contains("injected post-publication"));
            }).join().unwrap();
            for (path, bytes) in before {
                assert_eq!(std::fs::read(&path).unwrap(), bytes, "{}", path.display());
            }
            assert_eq!(Database::with_root(root).unwrap().get(1).unwrap().title, "title 1");
        }
    }

    fn virtual_identity_fixture(root: &std::path::Path, mode: u8, id: i64) -> (
        Database, crate::native::object_store::NativeStore, crate::platform::WriteFence, String,
    ) {
        use crate::platform::NovelAccessFence;
        use crate::native::sqlite::state::{self, StorageMode};
        state::write_mode(&root.join(".narou"), if mode == 0 { StorageMode::Yaml } else { StorageMode::Sqlite }).unwrap();
        if mode == 2 {
            std::fs::write(root.join(".narou/local_setting.yaml"), "narou-compat: true\n").unwrap();
        }
        let mut database = Database::with_root(root.to_path_buf()).unwrap();
        let legacy = format!("https://narou.rs/virtual/vs_{id}");
        let mut record = sample_record(id, None, &["unchanged"]);
        record.ncode = Some(crate::virtual_novel::ncode(id));
        record.toc_url = legacy.clone();
        record.is_narou = false;
        let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).unwrap();
        database.update_records(|mut records| { records.insert(id, record); Ok((records, ())) }).unwrap();
        let store = crate::native::object_store::NativeStore::for_narou_root(root).unwrap();
        let prefix = crate::platform::ObjectPrefix::from(keys.prefix().clone());
        let fence = futures::executor::block_on(store.capture_writer(&prefix)).unwrap();
        store.with_open_write(&fence, || database.with_library_metadata_snapshot(|inventory, primary| {
            crate::native::transfer::with_control_metadata_snapshot(inventory.root_dir(), primary, |connection| {
                crate::native::object_store::bind_namespace_origin(root, connection, &prefix, &legacy)
            })
        })).unwrap();
        (database, store, fence, legacy)
    }

    fn identity_origin(store: &crate::native::object_store::NativeStore, fence: &crate::platform::WriteFence) -> String {
        store.with_gate_lock(&fence.prefix, || store.namespace_state_locked(&fence.prefix).map(|(_, origin)| origin)).unwrap()
    }

    #[test]
    fn virtual_rebind_converges_and_rejects_stale_ordinary_writers_in_every_mode() {
        let _global = crate::test_support::global_state_guard();
        for mode in [0, 1, 2] {
            for id in [0, 7] {
                let root = tempfile::tempdir().unwrap();
                let (mut database, store, fence, legacy) = virtual_identity_fixture(root.path(), mode, id);
                let first = crate::virtual_novel::new_toc_url().unwrap();
                let losing = crate::virtual_novel::new_toc_url().unwrap();
                let rebind = |db: &mut Database, candidate: &str| store.with_open_write(&fence, || {
                    db.rebind_virtual_identity(&fence, crate::platform::NovelId(id), &legacy, candidate)
                });
                assert_eq!(rebind(&mut database, &first).unwrap(), first);
                let mut restarted = Database::with_root(root.path().to_path_buf()).unwrap();
                assert_eq!(rebind(&mut restarted, &losing).unwrap(), first);
                assert_eq!(identity_origin(&store, &fence), first);
                for stale in [&legacy, &losing] {
                    let error = store.with_open_write(&fence, || restarted.update_records(|mut records| {
                        records.get_mut(&id).unwrap().toc_url = stale.to_owned();
                        Ok((records, ()))
                    })).unwrap_err();
                    assert!(error.to_string().contains("immutable"), "{error}");
                }
                assert_eq!(Database::with_root(root.path().to_path_buf()).unwrap().get(id).unwrap().toc_url, first);
            }
        }
    }

    #[test]
    fn virtual_rebind_metadata_and_sql_origin_faults_leave_legacy_identity() {
        let _global = crate::test_support::global_state_guard();
        for mode in [0, 1, 2] {
            let root = tempfile::tempdir().unwrap();
            let (mut database, store, fence, legacy) = virtual_identity_fixture(root.path(), mode, 0);
            let candidate = crate::virtual_novel::new_toc_url().unwrap();
            if mode == 0 {
                crate::db::inventory::fail_after_publication(root.path().join(".narou/database.yaml"));
            } else {
                let connection = database.sqlite.as_ref().unwrap().conn_handle();
                connection.lock().execute_batch(
                    "CREATE TRIGGER fail_virtual_origin BEFORE UPDATE OF origin_url_fold ON novel_access_gates BEGIN SELECT RAISE(ABORT,'injected virtual origin SQL failure'); END;"
                ).unwrap();
            }
            assert!(store.with_open_write(&fence, || database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)).is_err());
            assert_eq!(identity_origin(&store, &fence), legacy);
            assert_eq!(Database::with_root(root.path().to_path_buf()).unwrap().get(0).unwrap().toc_url, legacy);
            if mode != 0 {
                database.sqlite.as_ref().unwrap().conn_handle().lock().execute_batch("DROP TRIGGER fail_virtual_origin").unwrap();
            }
            assert_eq!(store.with_open_write(&fence, || database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)).unwrap(), candidate);
        }
    }

    #[test]
    fn virtual_rebind_postcommit_faults_repair_before_keep_files_removal() {
        use crate::platform::NovelAccessFence;
        let _global = crate::test_support::global_state_guard();
        for (mode, after_write) in [(0, Some(false)), (0, Some(true)), (1, None), (2, None)] {
            let root = tempfile::tempdir().unwrap();
            let (mut database, store, fence, legacy) = virtual_identity_fixture(root.path(), mode, 0);
            let candidate = crate::virtual_novel::new_toc_url().unwrap();
            if let Some(after_write) = after_write {
                super::record_mutation::fail_virtual_origin(after_write);
                let error = store.with_open_write(&fence, || database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)).unwrap_err();
                assert!(error.to_string().contains("injected virtual origin"), "{error}");
            } else {
                // SQL commits the live origin with metadata; the retired FS
                // handoff is immutable, so it has no postcommit file write.
                assert_eq!(store.with_open_write(&fence, || database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)).unwrap(), candidate);
            }
            let mut restarted = Database::with_root(root.path().to_path_buf()).unwrap();
            assert_eq!(restarted.get(0).unwrap().toc_url, candidate);
            assert_eq!(identity_origin(&store, &fence), if after_write == Some(false) { legacy } else { candidate.clone() });
            let owner = crate::platform::access_fence::new_removal_owner(crate::platform::NovelId(0), false).unwrap();
            let epoch = store.with_gate_lock(&fence.prefix, || restarted.with_removal_identity(&fence, crate::platform::NovelId(0), |connection| {
                store.persist_retirement_locked(&fence, &owner, connection)
            })).unwrap();
            store.with_closed_write(&fence.prefix, &owner, epoch, || {
                restarted.remove_closed_record(&fence.prefix, &owner, epoch, crate::platform::NovelId(0))
            }).unwrap();
            futures::executor::block_on(store.open_gate(&fence.prefix, &owner, epoch)).unwrap();
            assert_eq!(identity_origin(&store, &fence), candidate, "keep-files deletion retains the committed portable provenance");
            assert!(store.with_open_write(&fence, || restarted.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &candidate, &candidate)).is_err(), "the original E0 cannot be reused after removal");
            let current = futures::executor::block_on(store.capture_writer(&fence.prefix)).unwrap();
            assert!(store.with_open_write(&current, || restarted.rebind_virtual_identity(&current, crate::platform::NovelId(0), &candidate, &candidate)).is_err(), "record absence never repairs or adopts a retained origin");
        }
    }

    #[test]
    fn virtual_rebind_pre_metadata_failure_does_not_bind_the_candidate() {
        let _global = crate::test_support::global_state_guard();
        let root = tempfile::tempdir().unwrap();
        let (mut database, store, fence, legacy) = virtual_identity_fixture(root.path(), 0, 0);
        let candidate = crate::virtual_novel::new_toc_url().unwrap();
        let writer_store = store.clone();
        let writer_fence = fence.clone();
        let writer_legacy = legacy.clone();
        let writer_candidate = candidate.clone();
        let error = std::thread::spawn(move || {
            crate::native::tag_sequence::set_record_save_hook(|| {
                Err(crate::error::NarouError::Database("injected before virtual metadata".into()))
            });
            writer_store.with_open_write(&writer_fence, || {
                database.rebind_virtual_identity(&writer_fence, crate::platform::NovelId(0), &writer_legacy, &writer_candidate)
            })
        }).join().unwrap().unwrap_err();
        assert!(error.to_string().contains("injected before virtual metadata"), "{error}");
        assert_eq!(identity_origin(&store, &fence), legacy);
        let mut restarted = Database::with_root(root.path().to_path_buf()).unwrap();
        assert_eq!(restarted.get(0).unwrap().toc_url, legacy);
        assert_eq!(store.with_open_write(&fence, || {
            restarted.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)
        }).unwrap(), candidate);
    }

    #[test]
    fn concurrent_virtual_rebinds_keep_the_first_committed_identity() {
        let _global = crate::test_support::global_state_guard();
        for mode in [0, 1, 2] {
            let root = tempfile::tempdir().unwrap();
            let (_database, store, fence, legacy) = virtual_identity_fixture(root.path(), mode, 0);
            let candidates = [crate::virtual_novel::new_toc_url().unwrap(), crate::virtual_novel::new_toc_url().unwrap()];
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let writers = candidates.iter().map(|candidate| {
                let root = root.path().to_path_buf();
                let fence = fence.clone();
                let observed = legacy.clone();
                let candidate = candidate.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut database = Database::with_root(root.clone()).unwrap();
                    let store = crate::native::object_store::NativeStore::for_narou_root(&root).unwrap();
                    barrier.wait();
                    store.with_open_write(&fence, || {
                        database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &observed, &candidate)
                    })
                })
            }).collect::<Vec<_>>();
            let results = writers.into_iter().map(|writer| writer.join().unwrap()).collect::<Vec<_>>();
            assert!(results.iter().any(Result::is_ok));
            let mut restarted = Database::with_root(root.path().to_path_buf()).unwrap();
            let winner = restarted.get(0).unwrap().toc_url.clone();
            assert!(candidates.contains(&winner));
            for result in results {
                match result {
                    Ok(uri) => assert_eq!(uri, winner),
                    Err(error) => assert!(error.to_string().contains("novel_busy"), "{error}"),
                }
            }
            for candidate in &candidates {
                assert_eq!(store.with_open_write(&fence, || {
                    restarted.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, candidate)
                }).unwrap(), winner, "a contender retries the SAME original epoch, never a recaptured one");
            }
            assert_eq!(identity_origin(&store, &fence), winner);
        }
    }

    #[test]
    fn virtual_rebind_first_binds_legacy_origin_before_portable_cutover() {
        use sha2::Digest;
        let _global = crate::test_support::global_state_guard();
        for mode in [0, 1, 2] {
            let root = tempfile::tempdir().unwrap();
            let (mut database, store, fence, legacy) = virtual_identity_fixture(root.path(), mode, 0);
            if mode == 0 {
                let path = root.path().join("小説データ/.objects/.narou-transfer/.gate")
                    .join(hex::encode(sha2::Sha256::digest(fence.prefix.as_ref().as_bytes())));
                let mut gate: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                gate["origin_url_fold"] = serde_json::Value::String(String::new());
                std::fs::write(path, serde_json::to_vec(&gate).unwrap()).unwrap();
            } else {
                database.sqlite.as_ref().unwrap().conn_handle().lock().execute(
                    "UPDATE novel_access_gates SET origin_url_fold='' WHERE prefix=?1", [fence.prefix.as_ref()],
                ).unwrap();
            }
            let candidate = crate::virtual_novel::new_toc_url().unwrap();
            if mode == 0 {
                crate::db::inventory::fail_after_publication(root.path().join(".narou/database.yaml"));
                assert!(store.with_open_write(&fence, || {
                    database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)
                }).is_err());
                assert_eq!(Database::with_root(root.path().to_path_buf()).unwrap().get(0).unwrap().toc_url, legacy);
                assert_eq!(identity_origin(&store, &fence), legacy, "a failed first cutover retains only the actual OLD identity, never the candidate");
            }
            let current = store.with_open_write(&fence, || {
                database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)
            }).unwrap();
            assert_eq!(current, candidate);
            assert_eq!(Database::with_root(root.path().to_path_buf()).unwrap().get(0).unwrap().toc_url, candidate);
            assert_eq!(identity_origin(&store, &fence), candidate);
        }
    }

    #[test]
    fn virtual_rebind_rejects_wrong_kind_id_and_unproven_legacy_origin() {
        use sha2::Digest;
        let _global = crate::test_support::global_state_guard();
        for invalid in ["kind", "id", "origin"] {
            let root = tempfile::tempdir().unwrap();
            let (mut database, store, fence, legacy) = virtual_identity_fixture(root.path(), 0, 0);
            if invalid == "origin" {
                let path = root.path().join("小説データ/.objects/.narou-transfer/.gate")
                    .join(hex::encode(sha2::Sha256::digest(fence.prefix.as_ref().as_bytes())));
                let mut gate: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                gate["origin_url_fold"] = serde_json::Value::String("https://example.test/foreign/".into());
                std::fs::write(path, serde_json::to_vec(&gate).unwrap()).unwrap();
            } else {
                database.update_records(|mut records| {
                    records.get_mut(&0).unwrap().ncode = if invalid == "kind" { None } else { Some("vs_8".into()) };
                    Ok((records, ()))
                }).unwrap();
            }
            let candidate = crate::virtual_novel::new_toc_url().unwrap();
            assert!(store.with_open_write(&fence, || database.rebind_virtual_identity(&fence, crate::platform::NovelId(0), &legacy, &candidate)).is_err(), "{invalid}");
            assert_eq!(Database::with_root(root.path().to_path_buf()).unwrap().get(0).unwrap().toc_url, legacy);
        }
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