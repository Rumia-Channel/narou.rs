//! Native ID claims. YAML metadata remains YAML; its private allocator shares
//! the transfer control database so ordinary downloads and transfers cannot
//! reserve the same ID in different processes.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::error::{NarouError, Result};
use crate::platform::{NovelId, NovelIdReservation};

type YamlIds = BTreeMap<i64, serde::de::IgnoredAny>;

/// `(pending (id, generation) rows, next cursor)` for the abandoned-creation sweep.
pub(crate) type PendingLiveness = (Vec<(NovelId, i64)>, Option<i64>);

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS native_transfer_id_owners (
    allocator TEXT NOT NULL,
    owner TEXT NOT NULL,
    novel_id INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (allocator, owner),
    UNIQUE (allocator, novel_id)
);
CREATE TABLE IF NOT EXISTS native_yaml_id_sequence (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    next_id INTEGER NOT NULL,
    next_generation INTEGER NOT NULL
);
INSERT OR IGNORE INTO native_yaml_id_sequence VALUES (1, 0, 0);
CREATE TABLE IF NOT EXISTS native_yaml_id_claims (
    novel_id INTEGER PRIMARY KEY,
    generation INTEGER NOT NULL,
    available INTEGER NOT NULL CHECK (available IN (0, 1)),
    retired INTEGER NOT NULL CHECK (retired IN (0, 1))
);
CREATE INDEX IF NOT EXISTS native_yaml_available_ids
    ON native_yaml_id_claims (available, retired, novel_id);
CREATE TABLE IF NOT EXISTS native_id_liveness (
    allocator TEXT NOT NULL,
    novel_id INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    liveness_key TEXT NOT NULL,
    committed INTEGER NOT NULL CHECK (committed IN (0, 1)),
    PRIMARY KEY (allocator, novel_id)
);
"#;

pub(crate) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA).map_err(crate::native::sqlite::sqlite_error)
}

pub(crate) fn try_liveness_lock(
    root: &std::path::Path, id: NovelId, generation: i64,
) -> Result<Option<std::sync::Arc<std::fs::File>>> {
    if id.0 < 0 || generation < 0 {
        return Err(NarouError::Database("invalid ID reservation identity".into()));
    }
    let directory = root.join(".narou/id_claims");
    std::fs::create_dir_all(&directory)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(directory.join(format!("{}-{generation}.lock", id.0)))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(std::sync::Arc::new(file))),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Acquire liveness before the allocating transaction commits. Old binaries'
/// reservations have no registration here and are never reaped as abandoned.
pub(crate) fn hold_ordinary_id(
    connection: &Connection, root: &std::path::Path, id: NovelId,
    generation: i64, yaml: bool,
) -> Result<NovelIdReservation> {
    let resource = try_liveness_lock(root, id, generation)?.ok_or_else(|| {
        NarouError::Platform("novel_busy: ID reservation is still owned".into())
    })?;
    connection.execute(
        "INSERT INTO native_id_liveness VALUES (?1,?2,?3,?4,0) ON CONFLICT(allocator,novel_id) DO UPDATE SET generation=excluded.generation,liveness_key=excluded.liveness_key,committed=0",
        (if yaml { "yaml" } else { "sqlite" },id.0,generation,format!("{}-{generation}",id.0)),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    Ok(NovelIdReservation::new(id,generation).with_liveness(resource))
}


/// The caller holds the actual metadata authority and has already completed
/// any owner-fenced namespace cleanup. This primitive never drops a claim or
/// infers that canonical data is disposable.
pub(crate) fn release_numeric_conn(
    connection: &Connection, reservation: &NovelIdReservation,
    yaml: bool, registered: bool,
) -> Result<bool> {
    if registered { return Ok(false); }
    let allocator = if yaml { "yaml" } else { "sqlite" };
    let owned: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_transfer_id_owners WHERE allocator=?1 AND novel_id=?2 AND generation=?3)",
        (allocator,reservation.id().0,reservation.generation()), |row| row.get(0),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    if owned {
        return Err(NarouError::Platform("id_reservation_owned: transfer owner is required".into()));
    }
    let changed = if yaml {
        connection.execute(
            "UPDATE native_yaml_id_claims SET available=1 WHERE novel_id=?1 AND generation=?2 AND available=0 AND retired=0",
            (reservation.id().0,reservation.generation()),
        )
    } else {
        connection.execute(crate::db::novel_query::RELEASE_ID_SQL, (reservation.id().0,reservation.generation()))
    }.map_err(crate::native::sqlite::sqlite_error)?;
    connection.execute(
        "DELETE FROM native_id_liveness WHERE allocator=?1 AND novel_id=?2 AND generation=?3 AND committed=0",
        (allocator,reservation.id().0,reservation.generation()),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    Ok(changed != 0)
}

/// `true` is a durable generation receipt for an already-published create.
pub(crate) fn verify_generation(
    connection: &Connection, reservation: &NovelIdReservation, yaml: bool,
) -> Result<bool> {
    connection.execute_batch(SCHEMA).map_err(crate::native::sqlite::sqlite_error)?;
    let claim: Option<(i64,i64)> = if yaml {
        connection.query_row(
            "SELECT generation,retired FROM native_yaml_id_claims WHERE novel_id=?1 AND available=0",
            [reservation.id().0], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional().map_err(crate::native::sqlite::sqlite_error)?
    } else {
        connection.query_row(
            "SELECT generation,0 FROM novel_id_reservations WHERE id=?1 AND available=0",
            [reservation.id().0], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional().map_err(crate::native::sqlite::sqlite_error)?
    };
    if let Some((generation,retired)) = claim
        && generation == reservation.generation()
    {
        return Ok(retired != 0);
    }
    let committed = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_id_liveness WHERE allocator=?1 AND novel_id=?2 AND generation=?3 AND committed=1)",
        (if yaml { "yaml" } else { "sqlite" },reservation.id().0,reservation.generation()),
        |row| row.get(0),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    if committed { return Ok(true); }
    Err(NarouError::Platform("id_reservation_lost: create ownership changed".into()))
}

pub(crate) fn acknowledge_create(
    connection: &Connection, reservation: &NovelIdReservation, yaml: bool,
) -> Result<()> {
    let changed = connection.execute(
        "UPDATE native_id_liveness SET committed=1 WHERE allocator=?1 AND novel_id=?2 AND generation=?3",
        (if yaml { "yaml" } else { "sqlite" },reservation.id().0,reservation.generation()),
    ).map_err(crate::native::sqlite::sqlite_error)?;
    if changed != 1 {
        return Err(NarouError::Platform("id_reservation_lost: native liveness registration missing".into()));
    }
    Ok(())
}

pub(super) fn pending_liveness(
    database: &super::Database, cursor: Option<i64>, limit: usize,
) -> Result<PendingLiveness> {
    let limit = limit.clamp(1, 100);
    let yaml = !database.using_sqlite();
    let connection = crate::native::transfer::control_connection(database.inventory.root_dir())?;
    let connection = connection.lock();
    initialize(&connection)?;
    // Include registered YAML rows whose creator died before its control ACK.
    // The repository promotes those receipts; it must never purge their data.
    let sql = if yaml {
        "SELECT l.novel_id,l.generation FROM native_id_liveness l JOIN native_yaml_id_claims r ON r.novel_id=l.novel_id AND r.generation=l.generation WHERE l.allocator='yaml' AND l.liveness_key<>'' AND l.committed=0 AND r.available=0 AND l.novel_id>?1 ORDER BY l.novel_id LIMIT ?2"
    } else {
        "SELECT l.novel_id,l.generation FROM native_id_liveness l JOIN novel_id_reservations r ON r.id=l.novel_id AND r.generation=l.generation WHERE l.allocator='sqlite' AND l.liveness_key<>'' AND l.committed=0 AND r.available=0 AND l.novel_id>?1 ORDER BY l.novel_id LIMIT ?2"
    };
    let mut statement = connection.prepare(sql).map_err(crate::native::sqlite::sqlite_error)?;
    let mut rows = statement.query((cursor.unwrap_or(-1), limit as i64))
        .map_err(crate::native::sqlite::sqlite_error)?;
    let mut claims = Vec::new();
    let mut count = 0;
    let mut last = None;
    while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
        let id: i64 = row.get(0).map_err(crate::native::sqlite::sqlite_error)?;
        let generation = row.get(1).map_err(crate::native::sqlite::sqlite_error)?;
        last = Some(id);
        count += 1;
        claims.push((NovelId(id),generation));
    }
    Ok((claims, if count == limit { last } else { None }))
}

/// Only the native repository's completed owner-cleanup path calls this while
/// holding the actual OPEN post-cleanup OS proof. Ordinary release cannot
/// discard an active namespace claim by invoking the scalar ledger directly.
pub(super) fn release_after_cleanup(
    database: &super::Database, prefix: &crate::platform::ObjectPrefix,
    completed_epoch: u64, reservation: &NovelIdReservation,
) -> Result<()> {
    with_ledger(database, |connection, ids, yaml| {
        let transaction = rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
            .map_err(crate::native::sqlite::sqlite_error)?;
        if !yaml {
            crate::native::sqlite::object_store::require_transfer_epoch(
                &transaction, prefix, None, completed_epoch, false,
            )?;
        }
        let registered = if let Some(ids) = ids {
            ids.contains_key(&reservation.id().0)
        } else {
            transaction.query_row("SELECT EXISTS(SELECT 1 FROM novels WHERE id=?1)", [reservation.id().0], |row| row.get(0))
                .map_err(crate::native::sqlite::sqlite_error)?
        };
        if !registered {
            let epoch: Option<i64> = transaction.query_row(
                "SELECT epoch FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2 AND prefix=?3",
                (reservation.id().0,reservation.generation(),prefix.as_ref().trim_end_matches('/')),
                |row| row.get(0),
            ).optional().map_err(crate::native::sqlite::sqlite_error)?;
            if let Some(epoch) = epoch
                && u64::try_from(epoch).ok().and_then(|epoch| epoch.checked_add(1)) != Some(completed_epoch)
            {
                return Err(NarouError::Platform("identity_cleanup_lost: completed epoch does not match creation".into()));
            }
            crate::native::transfer::release_ordinary_claim_conn(
                &transaction, reservation.id().0, reservation.generation(),
            )?;
            release_numeric_conn(&transaction, reservation, yaml, false)?;
        }
        transaction.commit().map_err(crate::native::sqlite::sqlite_error)
    })
}

fn overflow() -> NarouError {
    NarouError::Database("小説 ID の予約番号が上限に達しました".into())
}

fn yaml_ids(inventory: &super::Inventory) -> Result<YamlIds> {
    let path = inventory.inventory_path("database", super::InventoryScope::Local);
    match std::fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => Ok(serde_yaml::from_str(&raw)?),
        Ok(_) => Ok(BTreeMap::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

fn with_ledger<T>(
    database: &super::Database,
    action: impl FnOnce(&Connection, Option<&YamlIds>, bool) -> Result<T>,
) -> Result<T> {
    if let Some(repository) = &database.sqlite
        && !database.compat_active()
    {
        return action(&repository.conn_handle().lock(), None, false);
    }
    database.with_library_metadata_snapshot(|inventory, connection| {
        let ids = yaml_ids(inventory)?;
        match connection {
            Some(connection) => action(connection, Some(&ids), false),
            None => {
                let connection = crate::native::transfer::control_connection(inventory.root_dir())?;
                action(&connection.lock(), Some(&ids), true)
            }
        }
    })
}

pub(super) fn allocate(database: &super::Database, owner: Option<&str>) -> Result<NovelIdReservation> {
    let allocate_with = |connection: &Connection, ids: Option<&YamlIds>, yaml: bool| {
        connection.execute_batch(SCHEMA).map_err(crate::native::sqlite::sqlite_error)?;
        let tx = rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
            .map_err(crate::native::sqlite::sqlite_error)?;
        let allocator = if yaml { "yaml" } else { "sqlite" };
        if let Some(owner) = owner {
            let existing: Option<(i64, i64)> = tx.query_row(
                "SELECT novel_id, generation FROM native_transfer_id_owners WHERE allocator=?1 AND owner=?2",
                (allocator, owner), |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(crate::native::sqlite::sqlite_error)?;
            if let Some((id, generation)) = existing {
                return Ok(NovelIdReservation::new(NovelId(id), generation));
            }
        }
        let (id, generation) = if yaml {
            let ids = ids.expect("YAML allocation holds its metadata snapshot");
            let (next, generation): (i64, i64) = tx.query_row(
                "SELECT next_id, next_generation FROM native_yaml_id_sequence WHERE id=1",
                [], |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            let mut available = tx.prepare(
                "SELECT novel_id FROM native_yaml_id_claims WHERE available=1 AND retired=0 ORDER BY novel_id",
            ).map_err(crate::native::sqlite::sqlite_error)?;
            let mut rows = available.query([]).map_err(crate::native::sqlite::sqlite_error)?;
            let mut reusable = None;
            while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
                let candidate: i64 = row.get(0).map_err(crate::native::sqlite::sqlite_error)?;
                if !ids.contains_key(&candidate) { reusable = Some(candidate); break; }
            }
            let high_water = ids.last_key_value().map_or(Ok(0), |(id, _)| id.checked_add(1).ok_or_else(overflow))?;
            let cached_high_water = database.data.keys().max()
                .map_or(Ok(0), |id| id.checked_add(1).ok_or_else(overflow))?;
            let high_water = high_water.max(cached_high_water);
            let id = reusable.unwrap_or(next.max(high_water));
            let next = next.max(id.checked_add(1).ok_or_else(overflow)?);
            let next_generation = generation.checked_add(1).ok_or_else(overflow)?;
            tx.execute(
                "INSERT INTO native_yaml_id_claims (novel_id,generation,available,retired) VALUES (?1,?2,0,0) ON CONFLICT(novel_id) DO UPDATE SET generation=excluded.generation,available=0,retired=0",
                (id, generation),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            tx.execute("UPDATE native_yaml_id_sequence SET next_id=?1,next_generation=?2 WHERE id=1", (next,next_generation))
                .map_err(crate::native::sqlite::sqlite_error)?;
            (id, generation)
        } else {
            if let Some(ids) = ids {
                let high_water = ids.last_key_value().map_or(Ok(0), |(id, _)| id.checked_add(1).ok_or_else(overflow))?;
                tx.execute("UPDATE novel_id_sequence SET next_id=MAX(next_id,?1) WHERE id=1", [high_water])
                    .map_err(crate::native::sqlite::sqlite_error)?;
            }
            loop {
                tx.execute(crate::db::novel_query::ALLOCATE_STEP_SQL, []).map_err(crate::native::sqlite::sqlite_error)?;
                tx.execute(crate::db::novel_query::ALLOCATE_BUMP_SQL, []).map_err(crate::native::sqlite::sqlite_error)?;
                let (id, generation) = tx.query_row(
                    crate::db::novel_query::ALLOCATE_READ_SQL, [], |row| Ok((row.get(0)?,row.get(1)?)),
                ).map_err(crate::native::sqlite::sqlite_error)?;
                if ids.is_some_and(|ids| ids.contains_key(&id)) {
                    tx.execute(crate::db::novel_query::COMMIT_ID_RESERVATION_SQL, [id])
                        .map_err(crate::native::sqlite::sqlite_error)?;
                } else { break (id, generation); }
            }
        };
        if let Some(owner) = owner {
            tx.execute("INSERT INTO native_transfer_id_owners VALUES (?1,?2,?3,?4)", (allocator,owner,id,generation))
                .map_err(crate::native::sqlite::sqlite_error)?;
        }
        let reservation = if owner.is_some() {
            NovelIdReservation::new(NovelId(id), generation)
        } else {
            hold_ordinary_id(&tx, database.inventory.root_dir(), NovelId(id), generation, yaml)?
        };
        tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
        Ok(reservation)
    };
    with_ledger(database, allocate_with)
}

pub(super) fn resume(database: &super::Database, id: NovelId, generation: i64) -> Result<NovelIdReservation> {
    with_ledger(database, |connection, ids, yaml| {
        initialize(connection)?;
        let transaction = rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
            .map_err(crate::native::sqlite::sqlite_error)?;
        let reservation = NovelIdReservation::new(id, generation);
        let committed = verify_generation(&transaction, &reservation, yaml)?;
        let allocator = if yaml { "yaml" } else { "sqlite" };
        let transfer_owned: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_transfer_id_owners WHERE allocator=?1 AND novel_id=?2 AND generation=?3)",
            (allocator,id.0,generation), |row| row.get(0),
        ).map_err(crate::native::sqlite::sqlite_error)?;
        if transfer_owned { return Err(NarouError::Platform("id_reservation_lost: transfer owner proof is required".into())); }
        let registered = if let Some(ids) = ids { ids.contains_key(&id.0) } else {
            transaction.query_row("SELECT EXISTS(SELECT 1 FROM novels WHERE id=?1)", [id.0], |row| row.get(0))
                .map_err(crate::native::sqlite::sqlite_error)?
        };
        if registered && committed {
            transaction.commit().map_err(crate::native::sqlite::sqlite_error)?;
            return Ok(reservation);
        }
        if registered { return Err(NarouError::Platform("id_reservation_lost: publication receipt is unresolved".into())); }
        if committed {
            let schema: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='native_transfer_target_claims')",
                [], |row| row.get(0),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            let active = schema && transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2)",
                (id.0,generation), |row| row.get::<_,bool>(0),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            if !active { return Err(NarouError::Platform("id_reservation_lost: the created record was removed".into())); }
        }
        let liveness: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_id_liveness WHERE allocator=?1 AND novel_id=?2 AND generation=?3 AND liveness_key=?4)",
            (allocator,id.0,generation,format!("{}-{generation}",id.0)), |row| row.get(0),
        ).map_err(crate::native::sqlite::sqlite_error)?;
        if !liveness { return Err(NarouError::Platform("id_reservation_lost: original native liveness proof is missing".into())); }
        let resource = try_liveness_lock(database.inventory.root_dir(), id, generation)?
            .ok_or_else(||NarouError::Platform("novel_busy: ID reservation is still owned".into()))?;
        transaction.commit().map_err(crate::native::sqlite::sqlite_error)?;
        Ok(reservation.with_liveness(resource))
    })
}

pub(super) fn release(database: &super::Database, reservation: &NovelIdReservation) -> Result<()> {
    if database.data.contains_key(&reservation.id().0) { return Ok(()); }
    release_inner(database, None, Some(reservation))
}

pub(super) fn release_owner(database: &super::Database, owner: &str) -> Result<()> {
    release_inner(database, Some(owner), None)
}

fn release_inner(database: &super::Database, owner: Option<&str>, reservation: Option<&NovelIdReservation>) -> Result<()> {
    let release_with = |connection: &Connection, ids: Option<&YamlIds>, yaml: bool| {
        initialize(connection)?;
        let tx = rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
            .map_err(crate::native::sqlite::sqlite_error)?;
        let allocator = if yaml { "yaml" } else { "sqlite" };
        let claim = if let Some(owner) = owner {
            tx.query_row("SELECT novel_id,generation FROM native_transfer_id_owners WHERE allocator=?1 AND owner=?2", (allocator,owner), |row| Ok((row.get(0)?,row.get(1)?)))
                .optional().map_err(crate::native::sqlite::sqlite_error)?
        } else { reservation.map(|claim| (claim.id().0, claim.generation())) };
        let registered = if let Some((id,_)) = claim {
            if database.data.contains_key(&id) { true }
            else if let Some(ids) = ids { ids.contains_key(&id) }
            else { tx.query_row("SELECT EXISTS(SELECT 1 FROM novels WHERE id=?1)", [id], |row| row.get(0))
                .map_err(crate::native::sqlite::sqlite_error)? }
        } else { false };
        let _liveness = if owner.is_none() && !registered && let Some((id,generation)) = claim {
            let schema: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='native_transfer_target_claims')",
                [], |row| row.get(0),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            if schema {
                let active: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2)",
                    (id,generation), |row| row.get(0),
                ).map_err(crate::native::sqlite::sqlite_error)?;
                if active {
                    return Err(NarouError::Platform("identity_cleanup_required: pending creation still owns canonical content".into()));
                }
            }
            let protected: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM native_id_liveness WHERE allocator=?1 AND novel_id=?2 AND generation=?3 AND liveness_key<>'')",
                (allocator,id,generation), |row| row.get(0),
            ).map_err(crate::native::sqlite::sqlite_error)?;
            if protected && !reservation.is_some_and(NovelIdReservation::has_liveness) {
                Some(try_liveness_lock(database.inventory.root_dir(),NovelId(id),generation)?
                    .ok_or_else(||NarouError::Platform("novel_busy: ID reservation is still owned".into()))?)
            } else { None }
        } else { None };
        if let Some((id,generation)) = claim {
            let changed = if registered { false }
                else if owner.is_none() {
                    release_numeric_conn(&tx,reservation.expect("ordinary release carries its token"),yaml,false)?
                } else if yaml {
                    tx.execute("UPDATE native_yaml_id_claims SET available=1 WHERE novel_id=?1 AND generation=?2 AND available=0 AND retired=0", (id,generation))
                        .map_err(crate::native::sqlite::sqlite_error)? != 0
                } else {
                    tx.execute(crate::db::novel_query::RELEASE_ID_SQL, (id,generation))
                        .map_err(crate::native::sqlite::sqlite_error)? != 0
                };
            if changed || (owner.is_some() && !registered) {
                tx.execute("DELETE FROM native_transfer_id_owners WHERE allocator=?1 AND novel_id=?2 AND generation=?3", (allocator,id,generation))
                    .map_err(crate::native::sqlite::sqlite_error)?;
            }
        }
        tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
        Ok(())
    };
    with_ledger(database, release_with)
}

/// Called while the YAML metadata locks are still held. Conservatively retire
/// claims before their record publication is acknowledged: a failed publication
/// may burn an ID, but a later cleanup must never free an already-published ID.
pub(super) fn retire_yaml(database: &super::Database, records: &BTreeMap<i64, super::NovelRecord>) -> Result<()> {
    let connection = crate::native::transfer::control_connection(database.inventory.root_dir())?;
    let mut connection = connection.lock();
    connection.execute_batch(SCHEMA).map_err(crate::native::sqlite::sqlite_error)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(crate::native::sqlite::sqlite_error)?;
    {
        let mut claims = tx.prepare("SELECT novel_id FROM native_yaml_id_claims WHERE retired=0")
            .map_err(crate::native::sqlite::sqlite_error)?;
        let mut rows = claims.query([]).map_err(crate::native::sqlite::sqlite_error)?;
        while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
            let id: i64 = row.get(0).map_err(crate::native::sqlite::sqlite_error)?;
            if records.contains_key(&id) {
                tx.execute("UPDATE native_yaml_id_claims SET retired=1,available=0 WHERE novel_id=?1", [id])
                    .map_err(crate::native::sqlite::sqlite_error)?;
            }
        }
    }
    tx.commit().map_err(crate::native::sqlite::sqlite_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn fixture() -> tempfile::TempDir {
        let parent = std::env::current_dir().unwrap().join("target/test-libraries");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::TempDir::new_in(parent).unwrap();
        std::fs::create_dir(root.path().join(".narou")).unwrap();
        root
    }

    fn record(id: i64) -> super::super::NovelRecord {
        serde_yaml::from_str(&format!(
            "id: {id}\nauthor: author\ntitle: title\nfile_title: book-{id}\ntoc_url: https://example.com/{id}/\nsitename: Example\nlast_update: 2026-04-20 00:00:00.000000000 +09:00\n",
        )).unwrap()
    }

    fn child(root: &std::path::Path, operation: &str, owner: &str) -> (i64, i64) {
        let output_path = root.join("child-result.json");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "db::database::id_reservations::tests::reservation_child"])
            .env("NAROU_RS_TEST_ID_ROOT", root)
            .env("NAROU_RS_TEST_ID_OPERATION", operation)
            .env("NAROU_RS_TEST_ID_OWNER", owner)
            .env("NAROU_RS_LEGACY_YAML", "1")
            .output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap()
    }

    #[test]
    #[ignore = "subprocess helper for cross-process ID reservation regressions"]
    fn reservation_child() {
        let root = std::path::PathBuf::from(std::env::var_os("NAROU_RS_TEST_ID_ROOT").unwrap());
        let mut database = Database::with_root(root.clone()).unwrap();
        let operation = std::env::var("NAROU_RS_TEST_ID_OPERATION").unwrap();
        let result = if operation == "record" {
            database.update_records(|mut records| {
                records.insert(50, record(50));
                Ok((records, ()))
            }).unwrap();
            (50, -1)
        } else {
            let reservation = if operation == "owned" {
                database.allocate_transfer_id(&std::env::var("NAROU_RS_TEST_ID_OWNER").unwrap()).unwrap()
            } else {
                database.allocate_id().unwrap()
            };
            (reservation.id().0, reservation.generation())
        };
        std::fs::write(root.join("child-result.json"), serde_json::to_vec(&result).unwrap()).unwrap();
    }

    #[test]
    fn yaml_transfer_and_ordinary_claims_share_cross_process_ownership() {
        let _global = crate::test_support::global_state_guard();
        let root = fixture();
        let mut first = Database::with_root(root.path().to_path_buf()).unwrap();
        let mut second = Database::with_root(root.path().to_path_buf()).unwrap();
        let owned = first.allocate_transfer_id("session:publication:plan").unwrap();
        let stale = NovelIdReservation::new(owned.id(), owned.generation());
        assert_eq!(
            child(root.path(), "owned", "session:publication:plan"),
            (owned.id().0, owned.generation()),
        );
        let other_process = child(root.path(), "ordinary", "");
        let other_local = second.allocate_id().unwrap();
        assert_ne!(other_process.0, owned.id().0);
        assert_ne!(other_local.id(), owned.id());
        assert_ne!(other_local.id().0, other_process.0);
        first.release_transfer_id("session:publication:plan").unwrap();
        let reused = second.allocate_id().unwrap();
        assert_eq!(reused.id(), owned.id());
        assert_ne!(reused.generation(), owned.generation());
        first.release_id(&stale).unwrap();
        let next = first.allocate_id().unwrap();
        assert_ne!(next.id(), reused.id(), "delayed cleanup must not free another owner's ID");
    }

    #[test]
    fn yaml_allocator_reads_ids_published_after_its_cached_snapshot() {
        let _global = crate::test_support::global_state_guard();
        let root = fixture();
        let mut cached = Database::with_root(root.path().to_path_buf()).unwrap();
        assert_eq!(child(root.path(), "record", "").0, 50);
        let reservation = cached.allocate_id().unwrap();
        assert!(reservation.id().0 > 50);
    }

    #[test]
    fn delayed_transfer_cleanup_does_not_recycle_a_published_then_removed_id() {
        let _global = crate::test_support::global_state_guard();
        let root = fixture();
        let mut database = Database::with_root(root.path().to_path_buf()).unwrap();
        let reserved = database.allocate_transfer_id("session:publication:plan").unwrap();
        let id = reserved.id().0;
        database.update_records(|mut records| {
            records.insert(id, record(id));
            Ok((records, ()))
        }).unwrap();
        database.update_records(|mut records| {
            records.remove(&id);
            Ok((records, ()))
        }).unwrap();
        database.release_transfer_id("session:publication:plan").unwrap();
        let mut reopened = Database::with_root(root.path().to_path_buf()).unwrap();
        assert!(reopened.allocate_id().unwrap().id().0 > id);
    }

    #[test]
    fn native_resume_keeps_exact_generation_and_respects_retained_os_ownership() {
        let _global = crate::test_support::global_state_guard();
        for sqlite in [false, true] {
            let root = fixture();
            std::fs::create_dir_all(root.path().join(".narou")).unwrap();
            if sqlite { std::fs::write(root.path().join(".narou/storage-backend"), "sqlite").unwrap(); }
            let mut database = Database::with_root(root.path().to_path_buf()).unwrap();
            assert_eq!(database.using_sqlite(), sqlite);
            let owned = database.allocate_id().unwrap();
            let (id,generation) = (owned.id(),owned.generation());
            let retained = owned.liveness().unwrap();
            drop(owned);
            assert!(database.resume_id(id,generation).unwrap_err().to_string().contains("novel_busy"));
            drop(retained);
            let resumed = database.resume_id(id,generation).unwrap();
            assert_eq!((resumed.id(),resumed.generation()),(id,generation));
            assert!(resumed.has_liveness());
            let next = database.allocate_id().unwrap();
            assert_eq!(next.generation(),generation+1,"resume must not allocate another generation");
            database.release_id(&resumed).unwrap();
            drop(resumed);
            let replacement = database.allocate_id().unwrap();
            assert_eq!(replacement.id(),id);
            assert!(database.resume_id(id,generation).unwrap_err().to_string().contains("id_reservation_lost"));
            database.release_id(&next).unwrap();
            database.release_id(&replacement).unwrap();
        }
    }
}
