//! Indexed object metadata pages shared by Native SQLite and Worker D1.

/// Exact prefix key plus descendants, strictly after the last returned key.
/// Separate ranges avoid the optional-prefix OR that forces a full index scan.
/// The caller reads limit + 1 rows to determine whether another page exists.
/// Binds: exact prefix, descendant lower bound, upper bound, cursor, row cap.
pub const LIST_PAGE_SQL: &str = r#"
SELECT object_key, size, content_type, updated_at FROM objects
WHERE object_key = ?1 AND object_key > ?4
UNION ALL
SELECT object_key, size, content_type, updated_at FROM objects
WHERE ?1 != '' AND object_key >= max(?2, ?4) AND object_key < ?3 AND object_key > ?4
UNION ALL
SELECT object_key, size, content_type, updated_at FROM objects
WHERE ?1 = '' AND object_key > ?4
ORDER BY object_key LIMIT ?5
"#;

/// Native schema v16; the Worker migration carries the same storage tables.
pub const TRANSFER_STORAGE_SCHEMA: &str = r#"
ALTER TABLE objects ADD COLUMN generation TEXT NOT NULL DEFAULT '';
UPDATE objects SET generation=lower(hex(randomblob(16)));
CREATE TRIGGER transfer_object_insert_generation AFTER INSERT ON objects BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_object_update_generation AFTER UPDATE OF size,encoding,crc32,data ON objects BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_chunk_insert_generation AFTER INSERT ON object_chunks BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_chunk_update_generation AFTER UPDATE OF data ON object_chunks BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_chunk_delete_generation AFTER DELETE ON object_chunks BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=OLD.object_key; END;
CREATE TABLE transfer_stage_ledger(stage_id TEXT PRIMARY KEY,record_json TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 0,mutation_id TEXT NOT NULL DEFAULT '') WITHOUT ROWID;
CREATE TABLE novel_gate_bindings(novel_id INTEGER PRIMARY KEY,prefix TEXT NOT NULL);
CREATE INDEX novel_gate_binding_prefix ON novel_gate_bindings(prefix);
CREATE TABLE novel_access_gates(prefix TEXT PRIMARY KEY,epoch INTEGER NOT NULL DEFAULT 0,closed INTEGER NOT NULL DEFAULT 0,owner TEXT,updated_at INTEGER NOT NULL,content_revision INTEGER NOT NULL DEFAULT 0,origin_url_fold TEXT NOT NULL DEFAULT '') WITHOUT ROWID;
CREATE TABLE novel_reader_leases(prefix TEXT NOT NULL,reader_id TEXT NOT NULL,epoch INTEGER NOT NULL,expires_at INTEGER NOT NULL,PRIMARY KEY(prefix,reader_id)) WITHOUT ROWID;
CREATE INDEX novel_reader_leases_expiry ON novel_reader_leases(prefix,expires_at);
CREATE TABLE transfer_storage_guards(token TEXT PRIMARY KEY,prefix TEXT NOT NULL,owner TEXT NOT NULL,epoch INTEGER NOT NULL,closed INTEGER NOT NULL) WITHOUT ROWID;
CREATE TRIGGER transfer_storage_guard_authorize BEFORE INSERT ON transfer_storage_guards WHEN NOT EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.prefix=NEW.prefix AND g.epoch=NEW.epoch AND g.closed=NEW.closed AND (NEW.closed=0 OR (g.owner=NEW.owner AND NOT EXISTS(SELECT 1 FROM novel_reader_leases r WHERE r.prefix=g.prefix AND r.expires_at>unixepoch())))) BEGIN SELECT RAISE(ABORT,'novel_epoch_conflict'); END;
CREATE TABLE transfer_object_refs(object_key TEXT PRIMARY KEY,blob_key TEXT NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,backend TEXT NOT NULL DEFAULT 'd1',updated_at TEXT NOT NULL) WITHOUT ROWID;
CREATE INDEX transfer_object_refs_blob ON transfer_object_refs(blob_key);
ALTER TABLE transfer_object_refs ADD COLUMN blob_generation TEXT NOT NULL DEFAULT '';
CREATE TABLE transfer_verified_objects(blob_key TEXT PRIMARY KEY,size INTEGER NOT NULL,sha256 TEXT NOT NULL,generation TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE native_gate_handoffs(prefix TEXT PRIMARY KEY,fs_epoch INTEGER NOT NULL,sql_epoch INTEGER NOT NULL,fs_origin_fold TEXT NOT NULL DEFAULT '',recorded_at INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE fs_gate_handoffs(prefix TEXT PRIMARY KEY,sql_epoch INTEGER NOT NULL,fs_epoch INTEGER NOT NULL,fs_origin_fold TEXT NOT NULL DEFAULT '',recorded_at INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE VIEW native_logical_objects AS
 SELECT o.object_key,o.size,o.content_type,o.updated_at FROM objects o WHERE NOT EXISTS(SELECT 1 FROM transfer_object_refs r WHERE r.object_key=o.object_key)
 UNION ALL SELECT r.object_key,r.size,o.content_type,r.updated_at FROM transfer_object_refs r JOIN objects o ON o.object_key=r.blob_key WHERE r.backend='d1';
CREATE TRIGGER transfer_native_clear_ref_insert AFTER INSERT ON objects BEGIN DELETE FROM transfer_object_refs WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_native_clear_ref_update AFTER UPDATE OF size,encoding,crc32,data ON objects BEGIN DELETE FROM transfer_object_refs WHERE object_key=NEW.object_key; DELETE FROM transfer_verified_objects WHERE blob_key=NEW.object_key; END;
CREATE TRIGGER transfer_native_clear_verified_delete AFTER DELETE ON objects BEGIN DELETE FROM transfer_verified_objects WHERE blob_key=OLD.object_key; END;
CREATE TRIGGER transfer_native_ref_insert AFTER INSERT ON transfer_object_refs BEGIN
 UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/';
 UPDATE novel_revisions SET content_revision=content_revision+1 WHERE novel_id IN (SELECT novel_id FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/');
END;
CREATE TRIGGER transfer_native_ref_update AFTER UPDATE ON transfer_object_refs BEGIN
 UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/';
 UPDATE novel_revisions SET content_revision=content_revision+1 WHERE novel_id IN (SELECT novel_id FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/');
END;
CREATE TRIGGER transfer_native_ref_delete AFTER DELETE ON transfer_object_refs BEGIN
 UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/';
 UPDATE novel_revisions SET content_revision=content_revision+1 WHERE novel_id IN (SELECT novel_id FROM novel_gate_bindings WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/');
END;
CREATE TABLE novel_revisions(novel_id INTEGER PRIMARY KEY,record_revision INTEGER NOT NULL DEFAULT 0,content_revision INTEGER NOT NULL DEFAULT 0);
CREATE TRIGGER transfer_record_insert AFTER INSERT ON novels BEGIN INSERT INTO novel_revisions(novel_id,record_revision) VALUES(NEW.id,1) ON CONFLICT(novel_id) DO UPDATE SET record_revision=record_revision+1; END;
CREATE TRIGGER transfer_record_update AFTER UPDATE ON novels BEGIN INSERT INTO novel_revisions(novel_id,record_revision) VALUES(NEW.id,1) ON CONFLICT(novel_id) DO UPDATE SET record_revision=record_revision+1; END;
CREATE TRIGGER transfer_record_delete AFTER DELETE ON novels BEGIN
 INSERT INTO novel_revisions(novel_id,record_revision) VALUES(OLD.id,1) ON CONFLICT(novel_id) DO UPDATE SET record_revision=record_revision+1;
 UPDATE novel_access_gates SET epoch=CASE WHEN epoch<9223372036854775807 THEN epoch+1 ELSE abs(-9223372036854775808) END,updated_at=unixepoch() WHERE closed=0 AND prefix IN (SELECT prefix FROM novel_gate_bindings WHERE novel_id=OLD.id);
END;
CREATE TRIGGER transfer_native_content_insert AFTER INSERT ON objects BEGIN
 UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/';
 UPDATE novel_revisions SET content_revision=content_revision+1 WHERE novel_id IN (SELECT novel_id FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/');
END;
CREATE TRIGGER transfer_native_content_update AFTER UPDATE OF size,encoding,crc32,data ON objects BEGIN
 UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/';
 UPDATE novel_revisions SET content_revision=content_revision+1 WHERE novel_id IN (SELECT novel_id FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/');
END;
CREATE TRIGGER transfer_native_content_delete AFTER DELETE ON objects BEGIN
 UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/';
 UPDATE novel_revisions SET content_revision=content_revision+1 WHERE novel_id IN (SELECT novel_id FROM novel_gate_bindings WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/');
END;
"#;

#[cfg(all(test, feature = "native-runtime"))]
mod tests {
    use super::*;
    use crate::platform::prefix_upper_bound;
    use rusqlite::{Connection, StatementStatus, params};

    #[test]
    fn pages_seek_past_history_and_preserve_prefix_boundaries() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE objects(object_key TEXT PRIMARY KEY, size INTEGER, content_type TEXT, updated_at TEXT);
             WITH RECURSIVE ids(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<30000)
             INSERT INTO objects SELECT printf('other/%06d',i),1,NULL,'now' FROM ids;
             WITH RECURSIVE ids(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<1000)
             INSERT INTO objects SELECT printf('novels/book/%06d',i),1,NULL,'now' FROM ids;
             INSERT INTO objects VALUES ('novels/book',1,NULL,'now'),('novels/bookish/1',1,NULL,'now'),
                 ('novels/book/本文',1,NULL,'now'),('\u{10ffff}tail',1,NULL,'now');",
        ).unwrap();
        let page = |prefix: &str, cursor: &str, cap: i64| {
            let lower = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
            let mut statement = conn.prepare(LIST_PAGE_SQL).unwrap();
            let rows = statement.query_map(params![prefix, lower, prefix_upper_bound(&lower), cursor, cap],
                |row| row.get::<_, String>(0)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap();
            assert!(statement.get_status(StatementStatus::VmStep) < 2000);
            rows
        };
        assert_eq!(page("novels/book", "", 2), vec!["novels/book", "novels/book/000001"]);
        assert_eq!(page("novels/book", "novels/book/000998", 4),
            vec!["novels/book/000999", "novels/book/001000", "novels/book/本文"]);
        assert_eq!(page("", "other/029998", 3), vec!["other/029999", "other/030000", "\u{10ffff}tail"]);
        conn.execute("DELETE FROM objects WHERE object_key='novels/book/000998'", []).unwrap();
        assert_eq!(page("novels/book", "novels/book/000998", 1), vec!["novels/book/000999"]);
    }
}
