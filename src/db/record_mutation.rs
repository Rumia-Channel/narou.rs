//! Authorization carried into the existing metadata transaction. Native callers
//! hold the original OPEN or CLOSED OS proof before acquiring DATABASE locks.

use std::collections::BTreeMap;
use rusqlite::Connection;

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::native::object_store::{read_fs_gate_origin, rebind_fs_gate_origin_locked};
use crate::platform::{NovelId, NovelIdReservation, NovelObjectKeys, ObjectPrefix, WriteFence};

#[derive(Clone, Copy)]
pub(super) enum RecordMutation<'a> {
    Create {
        fence: &'a WriteFence,
        reservation: &'a NovelIdReservation,
        allow_existing: bool,
    },
    RemoveClosed {
        prefix: &'a ObjectPrefix,
        owner: &'a str,
        epoch: u64,
        id: NovelId,
    },
    /// Rebind the durable gate origin of one virtual novel to its portable
    /// URI inside the same immediate metadata authority as the record CAS.
    /// The caller already holds the original OPEN gate proof (`fence`) for
    /// the whole boundary.
    Rebind {
        fence: &'a WriteFence,
        id: NovelId,
        observed: &'a str,
        candidate: &'a str,
    },
    VirtualFences(&'a [WriteFence]),
}

/// Durable gate origins captured before the record commit decides. The SQL
/// row and the filesystem gate mirror are tracked separately so a torn
/// post-commit write stays repairable on the next rebind.
struct RebindPlan<'a> {
    id: NovelId,
    fence: &'a WriteFence,
    sql_origin: Option<String>,
    fs_origin: Option<String>,
    new_fold: String,
}

/// Borrowed observed/candidate TOC URLs for one virtual rebind proof.
#[derive(Clone, Copy)]
struct RebindUrls<'a> {
    observed: &'a str,
    candidate: &'a str,
}

pub(super) struct PreparedRecords<'a> {
    claims: crate::native::transfer::RecordClaimGuard,
    creation: Option<(&'a WriteFence, &'a NovelIdReservation, bool)>,
    rebind: Option<RebindPlan<'a>>,
}

fn identity_error() -> NarouError {
    NarouError::Platform("identity_conflict: record identity changed".into())
}

pub(super) fn record_in_prefix(record: &NovelRecord, prefix: &ObjectPrefix) -> Result<bool> {
    Ok(NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?
        .prefix().as_ref() == prefix.as_ref().trim_end_matches('/'))
}

fn record_overlaps_prefix(record: &NovelRecord, prefix: &ObjectPrefix) -> Result<bool> {
    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
    Ok(crate::platform::access_fence::namespace_prefixes_overlap(keys.prefix().as_ref(), prefix.as_ref()))
}

pub(super) fn assert_current_identity_conn(
    connection: &Connection, id: NovelId, prefix: &ObjectPrefix, folded_url: &str,
) -> Result<bool> {
    assert_identity_conn(connection, id, prefix, Some(folded_url))
}

pub(super) fn assert_current_namespace_conn(
    connection: &Connection, id: NovelId, prefix: &ObjectPrefix,
) -> Result<bool> {
    assert_identity_conn(connection, id, prefix, None)
}

fn assert_identity_conn(
    connection: &Connection, id: NovelId, prefix: &ObjectPrefix, folded_url: Option<&str>,
) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let normalized = prefix.as_ref().trim_end_matches('/');
    let mut ancestors = Vec::with_capacity(4);
    for (index, _) in normalized.match_indices('/') { ancestors.push(&normalized[..index]); }
    ancestors.push(normalized);
    let ancestors = serde_json::to_string(&ancestors)
        .map_err(|error| NarouError::Platform(error.to_string()))?;
    let descendant_lower = format!("{normalized}/");
    // '/' has exact successor '0': every Unicode child is inside this range.
    let descendant_upper = format!("{normalized}0");
    let conflict: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM novels WHERE ?1 IS NOT NULL AND toc_url_fold=?1 AND id<>?2 UNION ALL SELECT n.id FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE n.id<>?2 AND (b.prefix IN (SELECT value FROM json_each(?3)) OR (b.prefix>=?4 AND b.prefix<?5)))",
        rusqlite::params![folded_url,id.0,ancestors,descendant_lower,descendant_upper],
        |row| row.get(0),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    if conflict { return Err(identity_error()); }
    let own: Option<(String,String,String,bool)> = connection.query_row(
        "SELECT toc_url_fold,sitename,file_title,use_subdirectory FROM novels WHERE id=?1",
        [id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).optional().map_err(crate::native::sqlite::sqlite_error)?;
    if let Some((url,site,title,subdirectory)) = own {
        if folded_url.is_some_and(|expected| url != expected)
            || NovelObjectKeys::new(&site,&title,subdirectory)?.prefix().as_ref() != normalized
        { return Err(identity_error()); }
        Ok(true)
    } else { Ok(false) }
}

pub(super) fn prepare<'a>(
    root: &std::path::Path,
    connection: Option<&Connection>,
    current: &BTreeMap<i64, NovelRecord>,
    marker: Option<&crate::native::transfer::TransferCommitMarker>,
    mutation: Option<RecordMutation<'a>>,
) -> Result<PreparedRecords<'a>> {
    let prepare_with = |connection: &Connection, yaml: bool| {
        if let Some(RecordMutation::Rebind { fence, id, observed, candidate }) = mutation {
            prepare_virtual_origin(root, connection, current, fence, id, RebindUrls { observed, candidate }, yaml)?;
        }
        if let Some(RecordMutation::VirtualFences(fences)) = mutation {
            let by_prefix = fences.iter().map(|fence| (fence.prefix.as_ref().trim_end_matches('/'), fence))
                .collect::<BTreeMap<_, _>>();
            for (&id, record) in current {
                if crate::virtual_novel::is_virtual_record(record)
                    && crate::virtual_novel::is_portable_toc_url(&record.toc_url)
                {
                    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
                    if let Some(fence) = by_prefix.get(keys.prefix().as_ref()) {
                        prepare_virtual_origin(root, connection, current, fence, NovelId(id), RebindUrls { observed: &record.toc_url, candidate: &record.toc_url }, yaml)?;
                    }
                }
            }
        }
        let mut claims = crate::native::transfer::record_claim_guard_conn(root, connection, current, marker)?;
        let mut rebind = None;
        let creation = match mutation {
            Some(RecordMutation::Create { fence, reservation, allow_existing }) => {
                if !yaml {
                    crate::native::sqlite::object_store::require_transfer_epoch(
                        connection, &fence.prefix, None, fence.epoch, false,
                    )?;
                }
                let committed = super::id_reservations::verify_generation(connection, reservation, yaml)?;
                let existing = current.get(&reservation.id().0);
                if !(committed && existing.is_some()) && !reservation.has_liveness() {
                    return Err(NarouError::Platform("id_reservation_lost: pending native creation has no retained liveness proof".into()));
                }
                if let Some(existing) = existing&&(!allow_existing||!record_in_prefix(existing, &fence.prefix)?) {
                    return Err(identity_error());
                }
                let active: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2 AND prefix=?3 AND epoch=?4)",
                    rusqlite::params![reservation.id().0,reservation.generation(),fence.prefix.as_ref().trim_end_matches('/'),fence.epoch as i64],
                    |row| row.get(0),
                ).map_err(crate::native::sqlite::sqlite_error)?;
                if active {
                    claims.authorize_ordinary(reservation.id().0, reservation.generation(), &fence.prefix, fence.epoch)?;
                } else if !(committed && existing.is_some()) {
                    return Err(NarouError::Platform("identity_claim_lost: creation authority is unavailable".into()));
                }
                Some((fence, reservation, active))
            }
            Some(RecordMutation::RemoveClosed { prefix, owner, epoch, id }) => {
                if crate::platform::access_fence::parse_removal_owner(owner)?.0 != id {
                    return Err(identity_error());
                }
                if !yaml {
                    crate::native::sqlite::object_store::require_transfer_epoch(
                        connection, prefix, Some(owner), epoch, true,
                    )?;
                }
                if let Some(existing) = current.get(&id.0)
                    && !record_in_prefix(existing, prefix)?
                {
                    return Err(identity_error());
                }
                None
            }
            Some(RecordMutation::Rebind { fence, id, observed, candidate }) => {
                let record = current.get(&id.0).ok_or_else(|| NarouError::Platform(
                    "virtual identity record disappeared".into()))?;
                // An already-portable row wins the race: its stored URI is
                // authoritative, the caller's random candidate is dropped.
                let effective_candidate = if crate::virtual_novel::is_portable_toc_url(&record.toc_url) {
                    record.toc_url.as_str()
                } else {
                    candidate
                };
                let new_fold = crate::db::novel_codec::fold(effective_candidate);
                let sql_origin = sql_gate_origin(connection, &fence.prefix)?;
                let fs_origin = read_fs_gate_origin(root, &fence.prefix)?;
                let legacy = legacy_virtual_url(id);
                let effective_observed = if [sql_origin.as_deref(), fs_origin.as_deref()]
                    .into_iter().flatten().any(|origin| origin == legacy)
                {
                    legacy
                } else {
                    crate::db::novel_codec::fold(observed)
                };
                claims.authorize_virtual_rebind(current, fence, id, &effective_observed, effective_candidate)?;
                rebind = Some(RebindPlan {
                    id, fence,
                    sql_origin,
                    // SQL authority retires the FS file as immutable handoff
                    // evidence; only a live YAML gate needs an origin update.
                    fs_origin: if yaml { fs_origin } else { None },
                    new_fold,
                });
                None
            }
            Some(RecordMutation::VirtualFences(_)) | None => None,
        };
        Ok(PreparedRecords { claims, creation, rebind })
    };
    if let Some(connection) = connection {
        return prepare_with(connection, false);
    }
    let connection = crate::native::transfer::control_connection(root)?;
    prepare_with(&connection.lock(), true)
}

impl PreparedRecords<'_> {
    pub(super) fn validate(&self, records: &BTreeMap<i64, NovelRecord>) -> Result<()> {
        if let Some((fence, reservation, _)) = self.creation {
            let record = records.get(&reservation.id().0).ok_or_else(identity_error)?;
            if !record_in_prefix(record, &fence.prefix)? { return Err(identity_error()); }
        }
        if let Some(rebind) = &self.rebind {
            let record = records.get(&rebind.id.0).ok_or_else(identity_error)?;
            if !record_in_prefix(record, &rebind.fence.prefix)?
                || crate::db::novel_codec::fold(&record.toc_url) != rebind.new_fold
            { return Err(identity_error()); }
        }
        self.claims.validate(records)
    }

    pub(super) fn finish_sql(&self, connection: &Connection) -> Result<()> {
        self.finish(connection, false)
    }

    fn finish(&self, connection: &Connection, yaml: bool) -> Result<()> {
        if let Some(rebind) = &self.rebind && !yaml {
            // The SQL gate row commits inside this same transaction, so the
            // origin can never diverge from the committed record.
            let prefix = rebind.fence.prefix.as_ref().trim_end_matches('/');
            let expected = rebind.sql_origin.as_deref().unwrap_or_default();
            let changed = connection.execute(
                "UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND origin_url_fold IN ('',?3,?1) AND epoch=?4 AND closed=0 AND owner IS NULL",
                rusqlite::params![rebind.new_fold, prefix, expected, rebind.fence.epoch as i64],
            ).map_err(crate::native::sqlite::sqlite_error)?;
            if changed != 1 {
                return Err(NarouError::Platform(
                    "identity_conflict: retained gate origin changed during virtual rebind".into()));
            }
        }
        if let Some((fence, reservation, active)) = self.creation {
            super::id_reservations::acknowledge_create(connection, reservation, yaml)?;
            if active {
                crate::native::transfer::consume_ordinary_claim_conn(
                    connection, reservation.id().0, reservation.generation(), &fence.prefix, fence.epoch,
                )?;
            }
        }
        Ok(())
    }

    /// Publish the filesystem gate mirror immediately after the durable
    /// record commit while the caller still holds the OS gate proof and the
    /// metadata authority. YAML never publishes a random origin first.
    pub(super) fn finish_fs_origin(&self, root: &std::path::Path) -> Result<()> {
        if let Some(rebind) = &self.rebind && rebind.fs_origin.is_some() {
            #[cfg(test)]
            virtual_origin_fault(false)?;
            publish_fs_origin(root, rebind.fence, rebind.fs_origin.as_deref(), &rebind.new_fold)?;
            #[cfg(test)]
            virtual_origin_fault(true)?;
        }
        Ok(())
    }

    /// YAML files have already completed their existing rollback boundary. A
    /// failed/uncertain control acknowledgement leaves the actual record and
    /// its exact generation proof available for an idempotent retry.
    pub(super) fn finish_yaml(&self, root: &std::path::Path) -> Result<()> {
        self.finish_fs_origin(root)?;
        if self.creation.is_none() { return Ok(()); }
        let connection = crate::native::transfer::control_connection(root)?;
        let mut connection = connection.lock();
        let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(crate::native::sqlite::sqlite_error)?;
        self.finish(&transaction, true)?;
        transaction.commit().map_err(crate::native::sqlite::sqlite_error)
    }
}

#[cfg(test)]
thread_local! {
    static VIRTUAL_ORIGIN_FAULT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn fail_virtual_origin(after_write: bool) {
    VIRTUAL_ORIGIN_FAULT.with(|slot| slot.set(Some(after_write)));
}

#[cfg(test)]
fn virtual_origin_fault(after_write: bool) -> Result<()> {
    let fail = VIRTUAL_ORIGIN_FAULT.with(|slot| {
        if slot.get() == Some(after_write) { slot.set(None); true } else { false }
    });
    if fail { Err(NarouError::Database("injected virtual origin failure".into())) } else { Ok(()) }
}

/// Removal still holds OPEN E0 here. A proven interrupted rebind must be
/// repaired before that live record disappears and only provenance remains.
pub(super) fn repair_virtual_before_removal(
    inventory: &super::Inventory, records: Option<&Connection>, control: &Connection,
    yaml_backend: bool, fence: &WriteFence, id: NovelId,
) -> Result<()> {
    let current = match records {
        Some(connection) => {
            let own = crate::native::sqlite::SqliteNovelRepository::point_snapshot_conn(connection, id)?;
            if own.as_ref().is_none_or(|record| !crate::virtual_novel::is_virtual_record(record)
                || !crate::virtual_novel::is_portable_toc_url(&record.toc_url))
            { return Ok(()); }
            crate::native::sqlite::bulk::load_all_records(connection)?
        }
        None => {
            let path = inventory.inventory_path("database", super::InventoryScope::Local);
            let raw = crate::db::inventory::read_optional_yaml_file(&path)?;
            let mut current: BTreeMap<i64, NovelRecord> = if raw.trim().is_empty() { BTreeMap::new() } else { serde_yaml::from_str(&raw)? };
            for (&id, record) in &mut current { record.id = id; }
            current
        }
    };
    if let Some(record) = current.get(&id.0)
        && crate::virtual_novel::is_virtual_record(record)
        && crate::virtual_novel::is_portable_toc_url(&record.toc_url)
    {
        prepare_virtual_origin(inventory.root_dir(), control, &current, fence, id, RebindUrls { observed: &record.toc_url, candidate: &record.toc_url }, yaml_backend)?;
    }
    Ok(())
}

fn legacy_virtual_url(id: NovelId) -> String {
    format!("https://narou.rs/virtual/vs_{}", id.0)
}

/// Recovery is derived only from a live, exact-ID virtual row. An absent
/// record or arbitrary non-portable retained URL never authorizes a rebind.
fn prepare_virtual_origin(
    root: &std::path::Path, connection: &Connection,
    current: &BTreeMap<i64, NovelRecord>, fence: &WriteFence,
    id: NovelId, RebindUrls { observed, candidate }: RebindUrls<'_>, yaml: bool,
) -> Result<()> {
    let record = current.get(&id.0).ok_or_else(identity_error)?;
    let legacy = legacy_virtual_url(id);
    let current_fold = crate::db::novel_codec::fold(&record.toc_url);
    let portable = crate::virtual_novel::is_portable_toc_url(&record.toc_url);
    if id.0 < 0 || record.id != id.0
        || record.ncode.as_deref() != Some(crate::virtual_novel::ncode(id.0).as_str())
        || !record_in_prefix(record, &fence.prefix)?
        || (!portable && (current_fold != legacy || record.toc_url != observed))
        || (!portable && !crate::virtual_novel::is_portable_toc_url(candidate))
    { return Err(identity_error()); }
    for (&other_id, other) in current {
        if other_id != id.0 && (crate::db::novel_codec::fold(&other.toc_url) == current_fold
            || record_overlaps_prefix(other, &fence.prefix)?)
        { return Err(identity_error()); }
    }
    crate::native::transfer::assert_physical_records(
        &super::Inventory::new(root.to_path_buf()), current, id, &fence.prefix,
    )?;
    if !yaml {
        crate::native::sqlite::object_store::require_transfer_epoch(
            connection, &fence.prefix, None, fence.epoch, false,
        )?;
    }
    let sql_origin = sql_gate_origin(connection, &fence.prefix)?;
    let fs_origin = read_fs_gate_origin(root, &fence.prefix)?;
    for origin in [sql_origin.as_deref(), fs_origin.as_deref()].into_iter().flatten() {
        if !origin.is_empty() && origin != legacy && origin != current_fold {
            return Err(identity_error());
        }
    }
    let prefix = fence.prefix.as_ref().trim_end_matches('/');
    crate::native::object_store::visit_namespace_gates(root, connection, |other, closed, _owner, epoch, origin| {
        let other = other.trim_end_matches('/');
        if other == prefix {
            if closed || epoch != fence.epoch { return Err(identity_error()); }
        } else if (closed || !origin.is_empty())
            && crate::platform::access_fence::namespace_prefixes_overlap(other, prefix)
        { return Err(identity_error()); }
        Ok(())
    })?;
    let claims_exist: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_transfer_target_claims')",
        [], |row| row.get(0),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    if claims_exist {
        let candidate_fold = (!portable).then(|| crate::db::novel_codec::fold(candidate));
        let mut query = connection.prepare(
            "SELECT novel_id,folded_url,prefix FROM native_transfer_target_claims",
        ).map_err(crate::native::sqlite::sqlite_error)?;
        let mut rows = query.query([]).map_err(crate::native::sqlite::sqlite_error)?;
        while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
            let claimed_id: i64 = row.get(0).map_err(crate::native::sqlite::sqlite_error)?;
            let url: String = row.get(1).map_err(crate::native::sqlite::sqlite_error)?;
            let claimed_prefix: String = row.get(2).map_err(crate::native::sqlite::sqlite_error)?;
            if claimed_id == id.0 || url == current_fold
                || candidate_fold.as_deref() == Some(url.as_str())
                || crate::platform::access_fence::namespace_prefixes_overlap(&claimed_prefix, prefix)
            { return Err(identity_error()); }
        }
    }
    // Never prebind the caller's generated candidate. An unbound legacy
    // namespace first retains its actual OLD URI, so a metadata failure or
    // crash leaves either legacy/legacy or portable/legacy, never portable/blank.
    // An already-portable restored LIVE row is ordinary current admission:
    // bind its actual stored URI, not a caller candidate or inferred lineage.
    let actual_origin = if portable { current_fold.as_str() } else { legacy.as_str() };
    if sql_origin.as_deref().is_some_and(|origin| origin.is_empty() || (portable && origin == legacy)) {
        let changed = connection.execute(
            "UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND origin_url_fold IN ('',?3) AND epoch=?4 AND closed=0 AND owner IS NULL",
            rusqlite::params![actual_origin,prefix,legacy,fence.epoch as i64],
        ).map_err(crate::native::sqlite::sqlite_error)?;
        if changed != 1 { return Err(identity_error()); }
    }
    if yaml && let Some(origin) = fs_origin.as_deref()
        && (origin.is_empty() || (portable && origin == legacy))
    {
        rebind_fs_gate_origin_locked(root, &fence.prefix, fence.epoch, origin, actual_origin)?;
    }
    Ok(())
}

/// `novel_access_gates.origin_url_fold` for the exact prefix, when the SQL
/// gate table exists on this connection.
fn sql_gate_origin(connection: &Connection, prefix: &ObjectPrefix) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='novel_access_gates')",
        [], |row| row.get(0),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    if !table { return Ok(None); }
    connection.query_row(
        "SELECT origin_url_fold FROM novel_access_gates WHERE prefix=?1",
        [prefix.as_ref().trim_end_matches('/')], |row| row.get(0),
    ).optional().map_err(crate::native::sqlite::sqlite_error)
}

/// The caller retains the original writer proof and metadata authority. A
/// missing filesystem gate is SQL-only; a present gate's old identity must
/// already have been proved and bound before portable metadata publication.
fn publish_fs_origin(root: &std::path::Path, fence: &WriteFence, expected: Option<&str>, new: &str) -> Result<()> {
    match expected {
        None => Ok(()),
        Some("") => Err(identity_error()),
        Some(expected) => rebind_fs_gate_origin_locked(root, &fence.prefix, fence.epoch, expected, new),
    }
}

pub(super) fn reserve_identity(
    database: &super::Database, fence: &WriteFence,
    reservation: &NovelIdReservation, toc_url: &str, empty_owned: bool,
) -> Result<()> {
    let folded = crate::db::novel_codec::fold(toc_url);
    database.with_library_metadata_snapshot(|inventory, connection| {
        let records: Option<BTreeMap<i64, NovelRecord>> = if database.metadata_is_yaml_in_snapshot(connection) {
            let path = inventory.inventory_path("database", super::InventoryScope::Local);
            let raw = match std::fs::read_to_string(path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            };
            Some(if raw.trim().is_empty() { BTreeMap::new() } else { serde_yaml::from_str(&raw)? })
        } else { None };
        let reserve_with = |connection: &Connection, yaml: bool| {
            let transaction = rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)
                .map_err(crate::native::sqlite::sqlite_error)?;
            if !yaml {
                crate::native::sqlite::object_store::require_transfer_epoch(
                    &transaction, &fence.prefix, None, fence.epoch, false,
                )?;
            }
            let committed = super::id_reservations::verify_generation(&transaction, reservation, yaml)?;
            let existing = if let Some(records) = records.as_ref() {
                for (&id, record) in records {
                    let same_url = if record.toc_url.is_ascii() && folded.is_ascii() {
                        record.toc_url.trim().eq_ignore_ascii_case(&folded)
                    } else { crate::db::novel_codec::fold(&record.toc_url) == folded };
                    if id != reservation.id().0 && (same_url || record_overlaps_prefix(record, &fence.prefix)?) {
                        return Err(identity_error());
                    }
                }
                if let Some(record) = records.get(&reservation.id().0) {
                    if crate::db::novel_codec::fold(&record.toc_url) != folded
                        || !record_in_prefix(record, &fence.prefix)?
                    { return Err(identity_error()); }
                    true
                } else { false }
            } else {
                assert_current_identity_conn(&transaction, reservation.id(), &fence.prefix, &folded)?
            };
            // The physical namespace check shares this same authority before
            // any claim row or canonical write: scalar SQL bindings in SQL
            // mode, fresh YAML files otherwise.
            crate::native::transfer::assert_physical_current_namespace(
                inventory,
                if records.is_some() { None } else { Some(&transaction) },
                reservation.id(),
                &fence.prefix,
            )?;
            if !(committed && existing) && !reservation.has_liveness() {
                return Err(NarouError::Platform("id_reservation_lost: pending native creation has no retained liveness proof".into()));
            }
            if committed && existing {
                return transaction.commit().map_err(crate::native::sqlite::sqlite_error);
            }
            if committed {
                let active: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2 AND epoch=?3 AND prefix=?4)",
                    rusqlite::params![reservation.id().0,reservation.generation(),fence.epoch as i64,fence.prefix.as_ref().trim_end_matches('/')],
                    |row| row.get(0),
                ).map_err(crate::native::sqlite::sqlite_error)?;
                if !active {
                    return Err(NarouError::Platform("id_reservation_lost: the created record was removed".into()));
                }
            }
            if !yaml {
                let origin: String = transaction.query_row(
                    "SELECT origin_url_fold FROM novel_access_gates WHERE prefix=?1",
                    [fence.prefix.as_ref().trim_end_matches('/')], |row| row.get(0),
                ).map_err(crate::native::sqlite::sqlite_error)?;
                if (!origin.is_empty() && origin != folded) || (origin.is_empty() && !empty_owned) {
                    return Err(NarouError::Platform("identity_conflict: unowned or different-URL content occupies the prefix".into()));
                }
                transaction.execute(
                    "UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND epoch=?3 AND closed=0",
                    rusqlite::params![folded,fence.prefix.as_ref().trim_end_matches('/'),fence.epoch as i64],
                ).map_err(crate::native::sqlite::sqlite_error)?;
            }
            crate::native::transfer::reserve_ordinary_claim_conn(inventory.root_dir(), &transaction, fence, reservation, toc_url, empty_owned)?;
            transaction.commit().map_err(crate::native::sqlite::sqlite_error)
        };
        if let Some(connection) = connection {
            reserve_with(connection, false)
        } else {
            let connection = crate::native::transfer::control_connection(inventory.root_dir())?;
            reserve_with(&connection.lock(), true)
        }
    })
}
