-- Client-first transfer (protocol v2): durable per-session/file/chunk/
-- operation/create/publication/journal-blob/plan/catalog rows implementing
-- narou_rs::application::transfer::TransferJournal on D1. No receipt or plan
-- is folded into app_state; every row is addressed by the session that owns
-- it. All transfer reads/writes route through the primary handle.
--
-- Opaque byte payloads (spec/receipt/result/journal values, hash
-- checkpoints) travel as base64 TEXT so D1 never has to materialize a BLOB
-- into the control plane.

-- A deliberate duplicate insert aborts the whole D1 batch after a failed CAS.
CREATE TABLE transfer_batch_guard (id INTEGER PRIMARY KEY CHECK(id=1)) STRICT;
INSERT INTO transfer_batch_guard VALUES(1);

CREATE TABLE IF NOT EXISTS transfer_sessions (
    session_id TEXT PRIMARY KEY,
    library_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    secret_hash TEXT NOT NULL,
    direction TEXT NOT NULL CHECK(direction IN ('import','export')),
    state TEXT NOT NULL,
    resume_phase TEXT,
    revision INTEGER NOT NULL,
    conflict_policy TEXT NOT NULL,
    profile TEXT NOT NULL,
    extra_roles_json TEXT NOT NULL DEFAULT '[]',
    policy_version INTEGER NOT NULL DEFAULT 0,
    plan_revision TEXT,
    gc_cursor TEXT,
    catalog_revision INTEGER NOT NULL DEFAULT 0,
    chunk_bytes INTEGER NOT NULL,
    confirmed_bytes INTEGER NOT NULL DEFAULT 0,
    published_novels INTEGER NOT NULL DEFAULT 0,
    next_action TEXT NOT NULL,
    operation_id TEXT,
    last_operation_id TEXT,
    last_operation_result TEXT,
    error TEXT,
    ext_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS transfer_sessions_scope_idx
    ON transfer_sessions(library_id, principal, updated_at);
CREATE INDEX IF NOT EXISTS transfer_sessions_expiry_idx
    ON transfer_sessions(expires_at);

-- request_id -> create receipt (7-day terminal idempotency).
CREATE TABLE IF NOT EXISTS transfer_requests (
    request_id TEXT NOT NULL,
    library_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    session_id TEXT,
    receipt TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (request_id, library_id, principal)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS transfer_requests_created_idx
    ON transfer_requests(created_at);

CREATE TABLE transfer_execution_leases(session_id TEXT PRIMARY KEY,owner TEXT NOT NULL,operation_id TEXT NOT NULL,expires_at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE transfer_backend_steps(session_id TEXT NOT NULL,operation_id TEXT NOT NULL,kind TEXT NOT NULL,input_json TEXT NOT NULL,result_json TEXT,PRIMARY KEY(session_id,operation_id,kind)) WITHOUT ROWID;
CREATE TRIGGER transfer_backend_result_immutable BEFORE UPDATE OF result_json ON transfer_backend_steps WHEN OLD.result_json IS NOT NULL AND OLD.result_json IS NOT NEW.result_json BEGIN SELECT RAISE(ABORT,'transfer_step_receipt_conflict'); END;

CREATE TABLE IF NOT EXISTS transfer_operations (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    if_match_revision INTEGER,
    result TEXT,
    completed INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    PRIMARY KEY (session_id, operation_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS transfer_files (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    file_id INTEGER NOT NULL,
    novel_ordinal INTEGER NOT NULL,
    logical_path TEXT NOT NULL,
    role TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    state TEXT NOT NULL,
    confirmed_bytes INTEGER NOT NULL DEFAULT 0,
    next_chunk_index INTEGER NOT NULL DEFAULT 0,
    confirmed_prefix_sha256 TEXT,
    hash_checkpoint TEXT,
    stage TEXT,
    pin TEXT,
    finalize_operation TEXT,
    PRIMARY KEY (session_id, file_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS transfer_files_state_idx
    ON transfer_files(session_id, state);
CREATE INDEX IF NOT EXISTS transfer_files_stage_idx
    ON transfer_files(stage) WHERE stage IS NOT NULL;

CREATE TABLE IF NOT EXISTS transfer_chunks (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    file_id INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    payload_size INTEGER NOT NULL,
    declared_sha256 TEXT,
    stored_sha256 TEXT NOT NULL,
    prefix_sha256 TEXT NOT NULL,
    hash_checkpoint TEXT,
    offset INTEGER NOT NULL,
    is_final INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, file_id, chunk_index)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS transfer_publications (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    publication_id INTEGER NOT NULL,
    state TEXT NOT NULL,
    spec TEXT NOT NULL,
    phase_cursor TEXT,
    assigned_id INTEGER,
    receipt TEXT,
    error TEXT,
    PRIMARY KEY (session_id, publication_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS transfer_publications_pending_idx
    ON transfer_publications(session_id)
    WHERE state NOT IN ('committed','rolled_back','skipped');

-- Publication journal blobs (undo/new metadata, committed marks, receipts;
-- <=256KiB each). Pre-images live in backend stage objects, never here.
CREATE TABLE IF NOT EXISTS transfer_journal_entries (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    publication_id INTEGER NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (session_id, publication_id, key)
) WITHOUT ROWID;

-- Planned novel rows (import plan page), idempotent on novel_ordinal.
CREATE TABLE IF NOT EXISTS transfer_plan_entries (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    novel_ordinal INTEGER NOT NULL,
    plan_revision TEXT NOT NULL,
    toc_url TEXT NOT NULL,
    title TEXT NOT NULL,
    kind TEXT NOT NULL,
    action TEXT NOT NULL,
    reason TEXT,
    existing_id INTEGER,
    record_revision TEXT,
    content_revision TEXT,
    membership_digest TEXT,
    file_count INTEGER NOT NULL DEFAULT 0,
    total_bytes INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, novel_ordinal)
) WITHOUT ROWID;

-- Export catalog rows (per source file, pre-snapshot), idempotent on
-- (novel_ordinal, logical_path). Selection flags flip after `select`.
CREATE TABLE IF NOT EXISTS transfer_catalog_entries (
    session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id) ON DELETE CASCADE,
    novel_ordinal INTEGER NOT NULL,
    logical_path TEXT NOT NULL,
    catalog_revision INTEGER NOT NULL,
    existing_id INTEGER NOT NULL,
    toc_url TEXT NOT NULL,
    title TEXT NOT NULL,
    kind TEXT NOT NULL,
    role TEXT NOT NULL,
    size INTEGER NOT NULL,
    selectable INTEGER NOT NULL,
    selected INTEGER NOT NULL,
    required INTEGER NOT NULL,
    exclusion_reason TEXT,
    policy_version INTEGER NOT NULL,
    runtime_json TEXT NOT NULL,
    complete_json TEXT NOT NULL,
    PRIMARY KEY (session_id, novel_ordinal, logical_path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS transfer_catalog_selected_idx
    ON transfer_catalog_entries(session_id, selected, novel_ordinal, logical_path);
CREATE INDEX transfer_catalog_selection_order ON transfer_catalog_entries(session_id, (CAST(novel_ordinal AS TEXT)||char(0)||logical_path) COLLATE BINARY);

-- Deferred garbage collection cursor: objects that could not be removed
-- inline are retried here, bounded per sweep. Rows are only queued after
-- the owning session went terminal or its owner lease is dead.
CREATE TABLE IF NOT EXISTS transfer_gc (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    target TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('stage','snapshot','pin','preimage','key')),
    attempts INTEGER NOT NULL DEFAULT 0,
    not_before TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS transfer_gc_due_idx
    ON transfer_gc(not_before);
CREATE UNIQUE INDEX transfer_gc_target_idx ON transfer_gc(session_id,target,kind);

-- Durable owner registration precedes external stage writes.
CREATE TABLE transfer_stage_owners(stage_id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES transfer_sessions(session_id),file_id INTEGER,expected_size INTEGER NOT NULL,epoch INTEGER NOT NULL,backend TEXT NOT NULL,active INTEGER NOT NULL DEFAULT 1,sealed_key TEXT,sha256 TEXT,purpose TEXT NOT NULL DEFAULT 'file',initialized INTEGER NOT NULL DEFAULT 0 CHECK(initialized IN (0,1))) WITHOUT ROWID;
CREATE UNIQUE INDEX transfer_stage_active_file ON transfer_stage_owners(session_id,file_id) WHERE active=1 AND file_id IS NOT NULL;
CREATE TABLE transfer_stage_parts(stage_id TEXT NOT NULL REFERENCES transfer_stage_owners(stage_id) ON DELETE CASCADE,offset INTEGER NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,PRIMARY KEY(stage_id,offset)) WITHOUT ROWID;
CREATE TABLE transfer_pins(pin TEXT PRIMARY KEY,session_id TEXT NOT NULL,stage TEXT NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,backend TEXT NOT NULL) WITHOUT ROWID;
CREATE INDEX transfer_pins_stage ON transfer_pins(stage);
CREATE TABLE transfer_targets(session_id TEXT NOT NULL,publication_id INTEGER NOT NULL,assigned_id INTEGER,reservation_generation INTEGER,prefix TEXT,epoch INTEGER NOT NULL DEFAULT 0,record_json TEXT,summary_json TEXT,committed INTEGER NOT NULL DEFAULT 0,rolled_back INTEGER NOT NULL DEFAULT 0,receipt TEXT,PRIMARY KEY(session_id,publication_id)) WITHOUT ROWID;
CREATE UNIQUE INDEX transfer_targets_prefix ON transfer_targets(prefix) WHERE committed=0 AND rolled_back=0;
-- One namespace authority covers transfer publications and ordinary jobs.
-- V2 has no job_id; ordinary claims must retain their authoritative checkpoint.
CREATE TABLE novel_namespace_claims(prefix TEXT PRIMARY KEY,novel_id INTEGER NOT NULL UNIQUE,reservation_generation INTEGER,toc_url_fold TEXT NOT NULL UNIQUE,owner TEXT NOT NULL,epoch INTEGER NOT NULL CHECK(epoch>=0),job_id TEXT,empty_owned INTEGER NOT NULL DEFAULT 0 CHECK(empty_owned IN (0,1))) WITHOUT ROWID;
CREATE INDEX novel_namespace_claims_job ON novel_namespace_claims(job_id) WHERE job_id IS NOT NULL;
CREATE TABLE novel_creation_receipts(novel_id INTEGER NOT NULL,reservation_generation INTEGER NOT NULL,prefix TEXT NOT NULL,toc_url_fold TEXT NOT NULL,epoch INTEGER NOT NULL,job_id TEXT NOT NULL,PRIMARY KEY(novel_id,reservation_generation)) WITHOUT ROWID;
CREATE INDEX novel_creation_receipts_job ON novel_creation_receipts(job_id);
CREATE TRIGGER novel_creation_receipt_immutable BEFORE UPDATE ON novel_creation_receipts WHEN NEW.novel_id IS NOT OLD.novel_id OR NEW.reservation_generation IS NOT OLD.reservation_generation OR NEW.prefix IS NOT OLD.prefix OR NEW.toc_url_fold IS NOT OLD.toc_url_fold OR NEW.epoch IS NOT OLD.epoch OR NEW.job_id IS NOT OLD.job_id BEGIN SELECT RAISE(ABORT,'creation_receipt_conflict'); END;
-- Numeric reservation authority exists before a TOC reveals its namespace.
ALTER TABLE novel_id_reservations ADD COLUMN job_id TEXT;
CREATE INDEX novel_id_reservations_job ON novel_id_reservations(job_id) WHERE job_id IS NOT NULL;
CREATE INDEX worker_jobs_creation_reservation ON worker_jobs(CAST(json_extract(checkpoint_json,'$.creation_reservation[0]') AS INTEGER),CAST(json_extract(checkpoint_json,'$.creation_reservation[1]') AS INTEGER)) WHERE json_valid(checkpoint_json);
CREATE TABLE transfer_publication_files(session_id TEXT NOT NULL,publication_id INTEGER NOT NULL,file_index INTEGER NOT NULL,object_key TEXT NOT NULL,blob_key TEXT NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,backend TEXT NOT NULL,PRIMARY KEY(session_id,publication_id,file_index),UNIQUE(session_id,publication_id,object_key)) WITHOUT ROWID;
CREATE TABLE transfer_snapshot_state(session_id TEXT PRIMARY KEY,admission_cursor TEXT,admitted INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE transfer_catalog_indexes(session_id TEXT NOT NULL,novel_ordinal INTEGER NOT NULL,stage_id TEXT NOT NULL REFERENCES transfer_stage_owners(stage_id) ON DELETE CASCADE,PRIMARY KEY(session_id,novel_ordinal)) WITHOUT ROWID;
CREATE TABLE transfer_snapshot_gates(session_id TEXT NOT NULL,novel_ordinal INTEGER NOT NULL,prefix TEXT NOT NULL,owner TEXT NOT NULL,epoch INTEGER NOT NULL,PRIMARY KEY(session_id,novel_ordinal)) WITHOUT ROWID;
CREATE TABLE transfer_export_selection_state(session_id TEXT PRIMARY KEY,request_cursor INTEGER NOT NULL DEFAULT 0,member_cursor INTEGER NOT NULL DEFAULT 0,normal_count INTEGER NOT NULL DEFAULT 0,parent_after INTEGER NOT NULL DEFAULT -1,phase INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE transfer_export_sources(session_id TEXT NOT NULL,novel_id INTEGER NOT NULL,kind INTEGER NOT NULL,source_order INTEGER NOT NULL,novel_ordinal INTEGER,record_revision TEXT NOT NULL,content_revision TEXT NOT NULL,membership_digest TEXT,member_count INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(session_id,novel_id),UNIQUE(session_id,novel_ordinal)) WITHOUT ROWID;
CREATE INDEX transfer_export_source_order ON transfer_export_sources(session_id,kind,source_order);
CREATE TABLE transfer_export_members(session_id TEXT NOT NULL,parent_id INTEGER NOT NULL,position INTEGER NOT NULL,member_id INTEGER NOT NULL,toc_url TEXT NOT NULL,record_revision TEXT NOT NULL,PRIMARY KEY(session_id,parent_id,position),UNIQUE(session_id,parent_id,member_id)) WITHOUT ROWID;
CREATE TABLE transfer_inventory_metadata(session_id TEXT NOT NULL,novel_ordinal INTEGER NOT NULL,entry_json TEXT NOT NULL,layout_json TEXT NOT NULL,cursor_json TEXT NOT NULL,metadata_ready INTEGER NOT NULL DEFAULT 0,metadata_bytes INTEGER NOT NULL DEFAULT 0,serialized_bytes INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(session_id,novel_ordinal)) WITHOUT ROWID;
CREATE TABLE transfer_inventory_fragments(session_id TEXT NOT NULL,novel_ordinal INTEGER NOT NULL,field INTEGER NOT NULL,part INTEGER NOT NULL,byte_offset INTEGER NOT NULL,data TEXT NOT NULL,is_final INTEGER NOT NULL,total_bytes INTEGER NOT NULL,PRIMARY KEY(session_id,novel_ordinal,field,part)) WITHOUT ROWID;
CREATE TABLE transfer_source_materializations(session_id TEXT NOT NULL,novel_ordinal INTEGER NOT NULL,logical_path TEXT NOT NULL,stage_id TEXT NOT NULL REFERENCES transfer_stage_owners(stage_id) ON DELETE CASCADE,source_fingerprint TEXT NOT NULL,PRIMARY KEY(session_id,novel_ordinal,logical_path)) WITHOUT ROWID;

-- Storage primitives: all generations/guards are checked in the actual D1 batch.
-- The empty generation identifies a pre-transfer row. Range reads pin it by
-- key just like any other generation, and the first actual write changes it.
-- Do not backfill: a mass UPDATE rewrites legacy body blobs and would violate
-- the write guard on libraries that are migrating or already on S3.
ALTER TABLE objects ADD COLUMN generation TEXT NOT NULL DEFAULT '';
CREATE TRIGGER transfer_object_insert_generation AFTER INSERT ON objects BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_object_update_generation AFTER UPDATE OF size,encoding,crc32,data ON objects BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_chunk_insert_generation AFTER INSERT ON object_chunks BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_chunk_update_generation AFTER UPDATE OF data ON object_chunks BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_chunk_delete_generation AFTER DELETE ON object_chunks BEGIN UPDATE objects SET generation=lower(hex(randomblob(16))) WHERE object_key=OLD.object_key; END;
CREATE TABLE transfer_stage_ledger(stage_id TEXT PRIMARY KEY,record_json TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 0,mutation_id TEXT NOT NULL DEFAULT '') WITHOUT ROWID;
CREATE TABLE novel_access_gates(prefix TEXT PRIMARY KEY,epoch INTEGER NOT NULL DEFAULT 0,closed INTEGER NOT NULL DEFAULT 0,owner TEXT,updated_at INTEGER NOT NULL,content_revision INTEGER NOT NULL DEFAULT 0,origin_url_fold TEXT NOT NULL DEFAULT '') WITHOUT ROWID;
CREATE TRIGGER transfer_publication_close_owner BEFORE UPDATE OF closed,owner ON novel_access_gates WHEN NEW.closed=1 AND NEW.owner GLOB 'sess_*:publish:*' AND NOT EXISTS(SELECT 1 FROM transfer_targets t JOIN transfer_sessions s USING(session_id) WHERE NEW.owner=t.session_id||':publish:'||t.publication_id||':'||s.plan_revision AND t.prefix=NEW.prefix AND t.committed=0 AND t.rolled_back=0 AND s.state IN ('publishing','paused','cancelling')) BEGIN SELECT RAISE(ABORT,'transfer_owner_invalidated'); END;
CREATE TRIGGER transfer_snapshot_close_owner BEFORE UPDATE OF closed,owner ON novel_access_gates WHEN NEW.closed=1 AND NEW.owner GLOB 'sess_*:snapshot:*' AND NOT EXISTS(SELECT 1 FROM transfer_snapshot_gates t JOIN transfer_sessions s USING(session_id) WHERE NEW.owner=t.owner AND t.prefix=NEW.prefix AND s.state IN ('preparing','paused')) BEGIN SELECT RAISE(ABORT,'transfer_owner_invalidated'); END;
CREATE TABLE novel_reader_leases(prefix TEXT NOT NULL,reader_id TEXT NOT NULL,epoch INTEGER NOT NULL,expires_at INTEGER NOT NULL,PRIMARY KEY(prefix,reader_id)) WITHOUT ROWID;
CREATE INDEX novel_reader_leases_expiry ON novel_reader_leases(prefix,expires_at);
CREATE TABLE transfer_storage_guards(token TEXT PRIMARY KEY,prefix TEXT NOT NULL,owner TEXT NOT NULL,epoch INTEGER NOT NULL,closed INTEGER NOT NULL) WITHOUT ROWID;
CREATE TRIGGER transfer_storage_guard_authorize BEFORE INSERT ON transfer_storage_guards WHEN NOT EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.prefix=NEW.prefix AND g.epoch=NEW.epoch AND g.closed=NEW.closed AND (NEW.closed=0 OR (g.owner=NEW.owner AND NOT EXISTS(SELECT 1 FROM novel_reader_leases r WHERE r.prefix=g.prefix AND r.expires_at>unixepoch())))) BEGIN SELECT RAISE(ABORT,'novel_epoch_conflict'); END;
CREATE TABLE transfer_object_refs(object_key TEXT PRIMARY KEY,blob_key TEXT NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,backend TEXT NOT NULL DEFAULT 'd1',updated_at TEXT NOT NULL,blob_generation TEXT NOT NULL DEFAULT '') WITHOUT ROWID;
CREATE TABLE transfer_verified_objects(blob_key TEXT PRIMARY KEY,size INTEGER NOT NULL,sha256 TEXT NOT NULL,generation TEXT NOT NULL) WITHOUT ROWID;
CREATE TRIGGER transfer_verified_cleanup AFTER DELETE ON objects BEGIN DELETE FROM transfer_verified_objects WHERE blob_key=OLD.object_key; END;
CREATE INDEX transfer_object_refs_blob ON transfer_object_refs(blob_key);
CREATE TABLE native_gate_handoffs(prefix TEXT PRIMARY KEY,fs_epoch INTEGER NOT NULL,sql_epoch INTEGER NOT NULL,fs_origin_fold TEXT NOT NULL DEFAULT '',recorded_at INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE fs_gate_handoffs(prefix TEXT PRIMARY KEY,sql_epoch INTEGER NOT NULL,fs_epoch INTEGER NOT NULL,fs_origin_fold TEXT NOT NULL DEFAULT '',recorded_at INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE legacy_object_retirements(object_key TEXT PRIMARY KEY,legacy_key TEXT NOT NULL,pool_key TEXT NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,etag TEXT,backend TEXT NOT NULL,prefix TEXT NOT NULL,epoch INTEGER NOT NULL,updated_at TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE novel_gate_bindings(novel_id INTEGER PRIMARY KEY,prefix TEXT NOT NULL);
CREATE INDEX novel_gate_bindings_prefix ON novel_gate_bindings(prefix);
CREATE TRIGGER transfer_novel_update_fence BEFORE UPDATE ON novels WHEN EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE b.novel_id=OLD.id AND g.closed=1 AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_novel_delete_fence BEFORE DELETE ON novels WHEN EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE b.novel_id=OLD.id AND g.closed=1 AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_novel_insert_fence BEFORE INSERT ON novels WHEN EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE b.novel_id=NEW.id AND g.closed=1 AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_membership_update_fence BEFORE UPDATE OF value_json ON app_state WHEN NEW.scope='inv' AND NEW.key='virtual_novel_members' AND EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE g.closed=1 AND json_extract(OLD.value_json,'$."'||b.novel_id||'"') IS NOT json_extract(NEW.value_json,'$."'||b.novel_id||'"') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.owner=g.owner AND s.epoch=g.epoch)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_membership_insert_fence BEFORE INSERT ON app_state WHEN NEW.scope='inv' AND NEW.key='virtual_novel_members' AND EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE g.closed=1 AND json_extract(NEW.value_json,'$."'||b.novel_id||'"') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.owner=g.owner AND s.epoch=g.epoch)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_membership_delete_fence BEFORE DELETE ON app_state WHEN OLD.scope='inv' AND OLD.key='virtual_novel_members' AND EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE g.closed=1 AND json_extract(OLD.value_json,'$."'||b.novel_id||'"') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.owner=g.owner AND s.epoch=g.epoch)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TABLE novel_revisions(novel_id INTEGER PRIMARY KEY,record_revision INTEGER NOT NULL DEFAULT 0,content_revision INTEGER NOT NULL DEFAULT 0);
CREATE TRIGGER transfer_record_insert AFTER INSERT ON novels BEGIN INSERT INTO novel_revisions(novel_id,record_revision) VALUES(NEW.id,1) ON CONFLICT(novel_id) DO UPDATE SET record_revision=record_revision+1; END;
CREATE TRIGGER transfer_record_update AFTER UPDATE ON novels BEGIN INSERT INTO novel_revisions(novel_id,record_revision) VALUES(NEW.id,1) ON CONFLICT(novel_id) DO UPDATE SET record_revision=record_revision+1; END;
CREATE TRIGGER transfer_record_delete AFTER DELETE ON novels BEGIN INSERT INTO novel_revisions(novel_id,record_revision) VALUES(OLD.id,1) ON CONFLICT(novel_id) DO UPDATE SET record_revision=record_revision+1; UPDATE novel_access_gates SET epoch=CASE WHEN epoch<9223372036854775807 THEN epoch+1 ELSE abs(-9223372036854775808) END,updated_at=unixepoch() WHERE closed=0 AND prefix IN (SELECT prefix FROM novel_gate_bindings WHERE novel_id=OLD.id); END;
CREATE TRIGGER transfer_object_insert_fence BEFORE INSERT ON objects WHEN EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.closed=1 AND (NEW.object_key=g.prefix OR substr(NEW.object_key,1,length(g.prefix)+1)=g.prefix||'/') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_object_update_fence BEFORE UPDATE OF size,encoding,crc32,data ON objects WHEN EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.closed=1 AND (NEW.object_key=g.prefix OR substr(NEW.object_key,1,length(g.prefix)+1)=g.prefix||'/') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_object_delete_fence BEFORE DELETE ON objects WHEN EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.closed=1 AND (OLD.object_key=g.prefix OR substr(OLD.object_key,1,length(g.prefix)+1)=g.prefix||'/') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_ref_insert_fence BEFORE INSERT ON transfer_object_refs WHEN EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.closed=1 AND (NEW.object_key=g.prefix OR substr(NEW.object_key,1,length(g.prefix)+1)=g.prefix||'/') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_ref_update_fence BEFORE UPDATE ON transfer_object_refs WHEN EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.closed=1 AND (NEW.object_key=g.prefix OR substr(NEW.object_key,1,length(g.prefix)+1)=g.prefix||'/') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_ref_delete_fence BEFORE DELETE ON transfer_object_refs WHEN EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.closed=1 AND (OLD.object_key=g.prefix OR substr(OLD.object_key,1,length(g.prefix)+1)=g.prefix||'/') AND NOT EXISTS(SELECT 1 FROM transfer_storage_guards s WHERE s.prefix=g.prefix AND s.epoch=g.epoch AND s.owner=g.owner)) BEGIN SELECT RAISE(ABORT,'novel_recovering'); END;
CREATE TRIGGER transfer_object_clear_ref AFTER UPDATE OF size,encoding,crc32,data ON objects BEGIN DELETE FROM transfer_object_refs WHERE object_key=NEW.object_key; UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/'; END;
CREATE TRIGGER transfer_object_insert_clear_ref AFTER INSERT ON objects BEGIN DELETE FROM transfer_object_refs WHERE object_key=NEW.object_key; END;
CREATE TRIGGER transfer_ref_delete_garbage AFTER DELETE ON transfer_object_refs WHEN OLD.backend='s3' AND OLD.blob_key<>'' AND OLD.blob_key NOT GLOB 'illustrations/*' BEGIN INSERT INTO content_garbage(blob_key,not_before) VALUES(OLD.blob_key,strftime('%Y-%m-%dT%H:%M:%fZ','now','+24 hours')) ON CONFLICT(blob_key) DO UPDATE SET not_before=MAX(not_before,excluded.not_before); END;
CREATE TRIGGER transfer_ref_replace_garbage AFTER UPDATE OF blob_key,backend ON transfer_object_refs WHEN OLD.backend='s3' AND OLD.blob_key<>'' AND OLD.blob_key NOT GLOB 'illustrations/*' AND (OLD.blob_key<>NEW.blob_key OR OLD.backend<>NEW.backend) BEGIN INSERT INTO content_garbage(blob_key,not_before) VALUES(OLD.blob_key,strftime('%Y-%m-%dT%H:%M:%fZ','now','+24 hours')) ON CONFLICT(blob_key) DO UPDATE SET not_before=MAX(not_before,excluded.not_before); END;
CREATE TRIGGER transfer_ref_revision AFTER INSERT ON transfer_object_refs BEGIN UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/'; END;
CREATE TRIGGER transfer_ref_update_revision AFTER UPDATE ON transfer_object_refs BEGIN UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/'; END;

-- Content generations exist independently of whether a gate has ever closed.
CREATE TRIGGER transfer_object_insert_content_revision AFTER INSERT ON objects BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_object_update_content_revision AFTER UPDATE OF size,encoding,crc32,data ON objects BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_object_delete_content_revision AFTER DELETE ON objects BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_reference_insert_content_revision AFTER INSERT ON transfer_object_refs BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_reference_update_content_revision AFTER UPDATE ON transfer_object_refs BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_reference_delete_content_revision AFTER DELETE ON transfer_object_refs BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_content_insert_revision AFTER INSERT ON content_objects BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_content_update_revision AFTER UPDATE ON content_objects BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE NEW.object_key=prefix OR substr(NEW.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;
CREATE TRIGGER transfer_content_delete_revision AFTER DELETE ON content_objects BEGIN INSERT INTO novel_revisions(novel_id,content_revision) SELECT novel_id,1 FROM novel_gate_bindings WHERE OLD.object_key=prefix OR substr(OLD.object_key,1,length(prefix)+1)=prefix||'/' ON CONFLICT(novel_id) DO UPDATE SET content_revision=content_revision+1; END;

