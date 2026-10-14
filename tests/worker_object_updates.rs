#![cfg(feature = "native-runtime")]

#[path = "../worker_entry/src/d1_object_store/update.rs"]
mod update;

use narou_rs::platform::object_crc32;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use update::{Header, Chunk, Snapshot};

const KEY: &str = "novels/site/title/setting.ini";
const GUARD: &str = "object_write_guard/novels/site/title/setting.ini";
const TIME: &str = "2026-10-12T00:00:00Z";

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(
        "CREATE TABLE objects(object_key TEXT PRIMARY KEY, size INTEGER NOT NULL,
            updated_at TEXT NOT NULL, content_type TEXT, encoding TEXT NOT NULL,
            crc32 INTEGER NOT NULL, data BLOB);
         CREATE TABLE object_chunks(object_key TEXT NOT NULL, seq INTEGER NOT NULL,
            data BLOB NOT NULL, PRIMARY KEY(object_key, seq));
         CREATE TABLE app_state(scope TEXT NOT NULL, key TEXT NOT NULL,
            value_yaml TEXT, value_json TEXT, PRIMARY KEY(scope, key));"
    ).unwrap();
    connection
}

fn seed(connection: &Connection, bytes: &[u8], chunked: bool) {
    connection.execute("INSERT INTO objects VALUES (?1, ?2, ?3, NULL, 'none', ?4, ?5)",
        params![KEY, bytes.len() as i64, TIME, i64::from(object_crc32(bytes)),
            if chunked { None } else { Some(bytes) }],
    ).unwrap();
    if chunked {
        for (seq, data) in bytes.chunks(512 * 1024).enumerate() {
            connection.execute("INSERT INTO object_chunks VALUES (?1, ?2, ?3)",
                params![KEY, seq as i64, data],
            ).unwrap();
        }
    }
}

fn snapshot(connection: &Connection) -> Snapshot {
    let mut statement = connection.prepare(update::READ_SQL).unwrap();
    let mut rows = statement.query([KEY]).unwrap();
    let mut snapshot = Snapshot::default();
    while let Some(row) = rows.next().unwrap() {
        if snapshot.header.is_none() {
            snapshot.header = Some(Header {
                size: row.get(0).unwrap(), updated_at: row.get(1).unwrap(),
                content_type: row.get(2).unwrap(), encoding: row.get(3).unwrap(),
                crc32: row.get(4).unwrap(), data: row.get(5).unwrap(),
            });
        }
        if let Some(seq) = row.get::<_, Option<i64>>(6).unwrap() {
            snapshot.chunks.push(Chunk { seq, data: row.get(7).unwrap() });
        }
    }
    snapshot
}

fn commit(connection: &mut Connection, snapshot: &Snapshot, candidate: &[u8], chunked: bool)
    -> rusqlite::Result<bool>
{
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(update::RESET_SQL, [GUARD])?;
    let header = snapshot.header.as_ref();
    transaction.execute(update::AUTHORIZE_SQL, params![
        GUARD, KEY, header.is_some(), header.map(|h| h.size),
        header.map(|h| h.updated_at.as_str()), header.and_then(|h| h.content_type.as_deref()),
        header.map(|h| h.encoding.as_str()), header.map(|h| h.crc32),
        header.and_then(|h| h.data.as_deref()),
    ])?;
    transaction.execute(update::CHECK_CHUNK_COUNT_SQL,
        params![GUARD, KEY, snapshot.chunks.len() as i64])?;
    for chunk in &snapshot.chunks {
        transaction.execute(update::CHECK_CHUNK_SQL, params![GUARD, KEY, chunk.seq, chunk.data])?;
    }
    if chunked {
        transaction.execute(update::WRITE_CHUNKED_SQL, params![
            KEY, candidate.len() as i64, TIME, Option::<&str>::None, "none",
            i64::from(object_crc32(candidate)), GUARD,
        ])?;
    } else {
        transaction.execute(update::WRITE_INLINE_SQL, params![
            KEY, candidate.len() as i64, TIME, Option::<&str>::None, "none",
            i64::from(object_crc32(candidate)), candidate, GUARD,
        ])?;
    }
    transaction.execute(update::DELETE_CHUNKS_SQL, params![KEY, GUARD])?;
    if chunked {
        for (seq, data) in candidate.chunks(512 * 1024).enumerate() {
            transaction.execute(update::WRITE_CHUNK_SQL, params![KEY, seq as i64, data, GUARD])?;
        }
    }
    let committed = transaction.query_row(update::FINISH_SQL, [GUARD],
        |row| row.get::<_, i64>(0)).optional()?.is_some();
    transaction.commit()?;
    Ok(committed)
}

fn assert_guard_cleared(connection: &Connection) {
    let guard: Option<String> = connection.query_row(
        "SELECT value_yaml FROM app_state WHERE scope = 'inv' AND key = ?", [GUARD],
        |row| row.get(0),
    ).optional().unwrap();
    assert_eq!(guard, None);
}

#[test]
fn absent_empty_inline_and_chunked_objects_have_distinct_atomic_snapshots() {
    let mut connection = connection();
    let absent = snapshot(&connection);
    assert_eq!(absent.payload(16 * 1024 * 1024).unwrap(), None);
    assert!(commit(&mut connection, &absent, b"", false).unwrap());
    assert_eq!(snapshot(&connection).into_payload(16 * 1024 * 1024).unwrap(), Some(vec![]));
    // An absent snapshot cannot overwrite a subsequently created empty BLOB.
    assert!(!commit(&mut connection, &absent, b"stale", false).unwrap());
    let empty = snapshot(&connection);
    let bytes = vec![0x5a; 512 * 1024 + 1];
    assert!(commit(&mut connection, &empty, &bytes, true).unwrap());
    let chunked = snapshot(&connection);
    assert_eq!(chunked.payload(bytes.len()).unwrap().unwrap().as_ref(), bytes);
    assert!(commit(&mut connection, &chunked, b"inline again", false).unwrap());
    assert_eq!(snapshot(&connection).into_payload(16 * 1024 * 1024).unwrap(), Some(b"inline again".to_vec()));
    assert_guard_cleared(&connection);
}

#[test]
fn same_size_timestamp_and_crc_do_not_allow_a_changed_final_chunk_to_be_overwritten() {
    let mut connection = connection();
    let mut bytes = vec![0u8; 16 * 1024 * 1024];
    let offset = bytes.len() - 8;
    bytes[offset..].copy_from_slice(b"plumless");
    seed(&connection, &bytes, true);
    let stale = snapshot(&connection);
    bytes[offset..].copy_from_slice(b"buckeroo");
    assert_eq!(object_crc32(&bytes), stale.header.as_ref().unwrap().crc32 as u32);
    let last = bytes.chunks(512 * 1024).last().unwrap();
    connection.execute("UPDATE object_chunks SET data = ?1 WHERE object_key = ?2 AND seq = 31",
        params![last, KEY],
    ).unwrap();
    assert!(!commit(&mut connection, &stale, b"must not replace current bytes", true).unwrap());
    assert_eq!(snapshot(&connection).into_payload(bytes.len()).unwrap(), Some(bytes));
    assert_guard_cleared(&connection);
}

#[test]
fn added_removed_or_replaced_chunks_invalidate_the_entire_candidate() {
    for mutation in [
        "INSERT INTO object_chunks VALUES ('novels/site/title/setting.ini', 1, X'6162')",
        "DELETE FROM object_chunks",
        "UPDATE object_chunks SET seq = 2",
        "UPDATE object_chunks SET data = X'6261'",
    ] {
        let mut connection = connection();
        seed(&connection, b"ab", true);
        let stale = snapshot(&connection);
        connection.execute(mutation, []).unwrap();
        let before = snapshot(&connection);
        assert!(!commit(&mut connection, &stale, b"replacement", true).unwrap());
        let after = snapshot(&connection);
        assert_eq!(after.header.as_ref().unwrap().data, before.header.as_ref().unwrap().data);
        assert_eq!(after.header.as_ref().unwrap().size, before.header.as_ref().unwrap().size);
        assert_eq!(after.chunks.iter().map(|c| (c.seq, c.data.as_slice())).collect::<Vec<_>>(),
            before.chunks.iter().map(|c| (c.seq, c.data.as_slice())).collect::<Vec<_>>());
        assert_guard_cleared(&connection);
    }
}

#[test]
fn statement_failure_rolls_back_header_chunks_and_private_guard() {
    let mut connection = connection();
    seed(&connection, b"old body", true);
    let original = snapshot(&connection);
    connection.execute_batch("CREATE TRIGGER deny_chunks BEFORE INSERT ON object_chunks
        BEGIN SELECT RAISE(ABORT, 'denied'); END;").unwrap();
    assert!(commit(&mut connection, &original, b"new body", true).is_err());
    assert_eq!(snapshot(&connection).into_payload(16 * 1024 * 1024).unwrap(), Some(b"old body".to_vec()));
    assert_guard_cleared(&connection);
}

#[test]
fn every_changed_raw_header_column_and_deletion_reject_stale_writes() {
    for mutation in [
        "UPDATE objects SET size = 9",
        "UPDATE objects SET updated_at = '2026-10-12T00:00:01Z'",
        "UPDATE objects SET content_type = ''",
        "UPDATE objects SET encoding = 'brotli'",
        "UPDATE objects SET crc32 = 0",
        "UPDATE objects SET data = X'6275636b65726f6f'",
        "UPDATE objects SET data = NULL",
        "DELETE FROM objects",
    ] {
        let mut connection = connection();
        seed(&connection, b"plumless", false);
        let stale = snapshot(&connection);
        connection.execute(mutation, []).unwrap();
        let before = snapshot(&connection);
        assert!(!commit(&mut connection, &stale, b"replacement", false).unwrap());
        let after = snapshot(&connection);
        let raw = |snapshot: Snapshot| snapshot.header.map(|header| (
            header.size, header.updated_at, header.content_type,
            header.encoding, header.crc32, header.data,
        ));
        assert_eq!(raw(after), raw(before), "{mutation}");
        assert_guard_cleared(&connection);
    }
}

#[test]
fn empty_inline_blob_does_not_equal_null_and_an_old_guard_cannot_authorize_it() {
    let mut connection = connection();
    seed(&connection, b"", false);
    let stale = snapshot(&connection);
    connection.execute("UPDATE objects SET data = NULL", []).unwrap();
    connection.execute("INSERT INTO app_state VALUES ('inv', ?1, '1', '{}')", [GUARD]).unwrap();
    assert!(!commit(&mut connection, &stale, b"replacement", false).unwrap());
    assert_eq!(snapshot(&connection).header.unwrap().data, None);
    assert_guard_cleared(&connection);
}
