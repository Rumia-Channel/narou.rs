"""Run actual ordered Worker migrations and transfer journal queries on SQLite."""
import re
import sqlite3
import unittest
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
JOURNAL = (ROOT / "worker_entry/src/transfer/journal.rs").read_text(encoding="utf-8")
PROGRESS = (ROOT / "worker_entry/src/transfer/progress.rs").read_text(encoding="utf-8")
STEPS = (ROOT / "worker_entry/src/transfer/steps.rs").read_text(encoding="utf-8")
PROTOCOL = (ROOT / "worker_entry/src/transfer/protocol.rs").read_text(encoding="utf-8")


def production_query(method):
    body = JOURNAL.split(f"fn {method}", 1)[1].split("\n    fn ", 1)[0]
    return re.search(r'\.prepare\("([^"]+)"\)', body).group(1)


def publication_statement(prefix):
    return re.search(r'statement\(&backend\.db,"(' + re.escape(prefix) + r'[^"]*)"', PROGRESS).group(1)


def namespace_parameters(values):
    canonical = values[0].rstrip("/")
    ancestors = [canonical[:offset] for offset, char in enumerate(canonical) if char == "/"] + [canonical]
    return (*values, json.dumps(ancestors, ensure_ascii=False), canonical + "/", canonical + "0")


class TransferV2UpgradeTests(unittest.TestCase):
    def test_upgrade_preserves_legacy_bodies_during_s3_cutover(self):
        migrations = sorted((ROOT / "worker_entry/migrations").glob("*.sql"))
        inline = "novels/Site/book/本文/0001.yaml"
        history = "novels/Site/book/本文/cache/old/0001.yaml"
        toc = "novels/Site/book/本文/toc.yaml"
        for backend in ("d1", "migrating", "s3"):
            with self.subTest(backend=backend):
                db = sqlite3.connect(":memory:")
                self.addCleanup(db.close)
                db.execute("PRAGMA foreign_keys=ON")
                for migration in migrations:
                    if migration.name == "0020_transfer_v2.sql":
                        break
                    db.executescript(migration.read_text(encoding="utf-8"))
                objects = [(inline, b"current"), (history, None), (toc, b"metadata")]
                db.executemany(
                    "INSERT INTO objects(object_key,data,size,updated_at) VALUES(?,?,?,'old')",
                    [(key, data, len(data) if data is not None else len(b"history")) for key, data in objects],
                )
                db.execute("INSERT INTO object_chunks VALUES(?,0,?)", (history, b"history"))
                db.execute("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES('inv','content_backend',?,?)",
                           (json.dumps(backend), backend))
                db.commit()

                db.executescript(migration.read_text(encoding="utf-8"))
                self.assertEqual(db.execute("SELECT object_key,data FROM objects ORDER BY object_key").fetchall(),
                                 sorted(objects))
                self.assertEqual(db.execute("SELECT data FROM object_chunks WHERE object_key=?", (history,)).fetchone(),
                                 (b"history",))
                if backend != "d1":
                    with self.assertRaisesRegex(sqlite3.IntegrityError, "legacy body write rejected"):
                        db.execute("UPDATE objects SET data=? WHERE object_key=?", (b"changed", inline))
                    with self.assertRaisesRegex(sqlite3.IntegrityError, "legacy body deletion requires"):
                        db.execute("DELETE FROM objects WHERE object_key=?", (history,))
                    db.execute("INSERT INTO content_deletions VALUES(?)", (history,))
                    db.execute("DELETE FROM objects WHERE object_key=?", (history,))
                    self.assertIsNone(db.execute("SELECT data FROM object_chunks WHERE object_key=?", (history,)).fetchone())
                else:
                    db.execute("UPDATE object_chunks SET data=? WHERE object_key=?", (b"changed", history))
                    generation, = db.execute("SELECT generation FROM objects WHERE object_key=?", (history,)).fetchone()
                    self.assertRegex(generation, r"^[0-9a-f]{32}$")
                old_generation, = db.execute("SELECT generation FROM objects WHERE object_key=?", (toc,)).fetchone()
                db.execute("UPDATE objects SET data=? WHERE object_key=?", (b"new toc", toc))
                new_generation, = db.execute("SELECT generation FROM objects WHERE object_key=?", (toc,)).fetchone()
                self.assertRegex(new_generation, r"^[0-9a-f]{32}$")
                self.assertNotEqual(new_generation, old_generation)


class TransferV2SqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA foreign_keys=ON")
        self.addCleanup(self.db.close)
        for migration in sorted((ROOT / "worker_entry/migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))

    def session(self, state="cataloguing", plan="plan-1"):
        values = {
            "session_id": "sess_sql", "library_id": "library", "principal": "admin",
            "secret_hash": "a" * 64, "direction": "export", "state": state,
            "revision": 1, "conflict_policy": "skip", "profile": "runtime",
            "extra_roles_json": "[]", "plan_revision": plan, "chunk_bytes": 8323072,
            "next_action": "advance", "ext_json": "{}", "created_at": "2000",
            "updated_at": "2000", "expires_at": "2099",
        }
        columns = list(values)
        self.db.execute(f"INSERT INTO transfer_sessions({','.join(columns)}) VALUES({','.join('?' for _ in columns)})", tuple(values.values()))
        return "sess_sql"

    def novel(self, novel_id=1):
        self.db.execute("""INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update)
            VALUES(?, 'Author','author','Title','title','book',?,?,'Site','site','2000')""", (novel_id, f"https://example.test/{novel_id}/", f"https://example.test/{novel_id}/"))

    def test_remove_invalidates_original_writer_even_while_gate_stays_open(self):
        self.novel()
        self.db.execute("INSERT INTO novel_gate_bindings VALUES(1,'novels/Site/book')")
        self.db.execute("INSERT INTO novel_access_gates(prefix,epoch,closed,updated_at) VALUES('novels/Site/book',7,0,0)")
        self.db.execute("DELETE FROM novels WHERE id=1")
        self.assertEqual(self.db.execute("SELECT epoch,closed FROM novel_access_gates").fetchone()[:], (8, 0))
        with self.assertRaisesRegex(sqlite3.IntegrityError, "novel_epoch_conflict"):
            self.db.execute("INSERT INTO transfer_storage_guards VALUES('old','novels/Site/book','',7,0)")
        self.db.execute("INSERT INTO transfer_storage_guards VALUES('fresh','novels/Site/book','',8,0)")

    def test_publication_owner_binds_the_frozen_plan_revision(self):
        session = self.session("publishing")
        self.db.execute("INSERT INTO transfer_targets(session_id,publication_id,prefix) VALUES(?,0,'novels/Site/book')", (session,))
        self.db.execute("INSERT INTO novel_access_gates(prefix,epoch,closed,updated_at) VALUES('novels/Site/book',1,0,0)")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "transfer_owner_invalidated"):
            self.db.execute("UPDATE novel_access_gates SET closed=1,owner='sess_sql:publish:0:older-plan'")
        self.db.execute("UPDATE novel_access_gates SET closed=1,owner='sess_sql:publish:0:plan-1'")
        self.db.execute("UPDATE transfer_sessions SET plan_revision='plan-2'")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "transfer_owner_invalidated"):
            self.db.execute("UPDATE novel_access_gates SET owner='sess_sql:publish:0:plan-1'")

    def test_private_cursor_and_replay_receipt_cannot_commit_different_units(self):
        session = self.session()
        self.db.execute("INSERT INTO transfer_export_selection_state(session_id,normal_count) VALUES(?,1)", (session,))
        self.db.execute("INSERT INTO transfer_backend_steps VALUES(?,'advance-1','freeze_selection','{}','false')", (session,))
        self.db.commit()
        with self.assertRaisesRegex(sqlite3.IntegrityError, "transfer_step_receipt_conflict"):
            with self.db:
                self.db.execute("UPDATE transfer_export_selection_state SET normal_count=2")
                self.db.execute("UPDATE transfer_backend_steps SET result_json='true'")
        self.assertEqual(self.db.execute("SELECT normal_count FROM transfer_export_selection_state").fetchone()[0], 1)
        self.assertEqual(self.db.execute("SELECT result_json FROM transfer_backend_steps").fetchone()[0], "false")

    def test_finalizing_query_includes_reserved_manifest_and_no_uploadable_files(self):
        session = self.session()
        for file_id, state in [(0, "pending"), (1, "finalizing"), (2, "verified"), (4294967295, "finalizing")]:
            self.db.execute("INSERT INTO transfer_files(session_id,file_id,novel_ordinal,logical_path,role,size,sha256,state,finalize_operation) VALUES(?,?,0,?,'metadata',0,?,?,?)", (session, file_id, str(file_id), "a" * 64, state, "finalize-" + str(file_id) if state == "finalizing" else None))
        rows = self.db.execute(production_query("finalizing_files"), (session, 100)).fetchall()
        self.assertEqual([row["file_id"] for row in rows], [1, 4294967295])
        self.assertTrue(all(row["finalize_operation"] for row in rows))

    def test_selection_pages_hash_every_row_in_decimal_ordinal_utf8_order(self):
        session = self.session()
        values = [(2, "\U0001f338", 0), (10, "z", 1), (2, "\ue000", 1), (1, "a", 0)]
        for ordinal, path, selected in values:
            self.db.execute("""INSERT INTO transfer_catalog_entries(session_id,novel_ordinal,logical_path,catalog_revision,existing_id,toc_url,title,kind,role,size,selectable,selected,required,policy_version,runtime_json,complete_json)
                VALUES(?,?,?,1,1,'https://example.test/','Title','normal','unknown',1,1,?,0,1,'{}','{}')""", (session, ordinal, path, selected))
        query = production_query("selection_catalog_entries")
        first = self.db.execute(query, (session, "", 3)).fetchall()
        cursor = str(first[1]["novel_ordinal"]) + "\0" + first[1]["logical_path"]
        second = self.db.execute(query, (session, cursor, 3)).fetchall()
        rows = first[:2] + second
        expected = sorted((str(ordinal) + "\0" + path).encode("utf-8") for ordinal, path, _ in values)
        self.assertEqual([(str(row["novel_ordinal"]) + "\0" + row["logical_path"]).encode("utf-8") for row in rows], expected)
        self.assertEqual(sum(row["selected"] == 0 for row in rows), 2)
        plan = " ".join(str(row[3]) for row in self.db.execute("EXPLAIN QUERY PLAN " + query, (session, "", 3)))
        self.assertIn("transfer_catalog_selection_order", plan)

    def test_verified_raw_stage_generation_proof_is_not_reused_after_mutation(self):
        blob = ".narou-transfer/data/sql-proof"
        self.db.execute("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES(?,3,'old','none',0,?)", (blob, b"old"))
        generation = self.db.execute("SELECT generation FROM objects WHERE object_key=?", (blob,)).fetchone()[0]
        self.db.execute("INSERT INTO transfer_verified_objects VALUES(?,3,?,?)", (blob, "a" * 64, generation))
        query = "SELECT v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=? AND v.size=3 AND v.sha256=?"
        self.assertEqual(self.db.execute(query, (blob, "a" * 64)).fetchone()[0], generation)
        self.db.execute("UPDATE objects SET data=? WHERE object_key=?", (b"new", blob))
        self.assertIsNone(self.db.execute(query, (blob, "a" * 64)).fetchone())
        self.db.execute("DELETE FROM objects WHERE object_key=?", (blob,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transfer_verified_objects").fetchone()[0], 0)

    def test_namespace_claims_cross_fence_ordinary_and_transfer_creation(self):
        session = self.session("publishing")
        self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at) VALUES('site/book',0)")
        self.db.executemany("INSERT INTO novel_id_reservations(id,generation,available) VALUES(?,?,0)", [(10, 1), (20, 2)])
        self.db.execute("INSERT INTO transfer_targets(session_id,publication_id,assigned_id,reservation_generation) VALUES(?,0,10,1)", (session,))
        self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch) VALUES('site/book',20,2,'https://ordinary/','create:20:2',7)")
        self.db.commit()
        values = ("site/book", 10, 1, "https://transfer/", "sess_sql:publish:0:plan-1")
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c")
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(acquire, values)
            self.db.execute(guard, namespace_parameters(values))
        self.assertEqual(self.db.execute("SELECT owner FROM novel_namespace_claims").fetchone()[0], "create:20:2")
        self.assertIsNone(self.db.execute("SELECT prefix FROM transfer_targets").fetchone()[0])
        self.db.execute("DELETE FROM novel_namespace_claims WHERE owner='create:20:2'")
        self.db.commit()
        with self.db:
            self.db.execute(acquire, values)
            self.db.execute(guard, namespace_parameters(values))
            self.db.execute(publication_statement("UPDATE transfer_targets SET prefix=?,record_json=?"), ("site/book", "{}", session, 0, 10, 1))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch) VALUES('site/book',20,2,'https://ordinary/','create:20:2',7)")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch) VALUES('site/other',20,2,'https://transfer/','create:20:2',7)")

    def test_namespace_claim_target_and_generation_adoption_are_one_transaction(self):
        session = self.session("publishing")
        self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at) VALUES('site/book',0)")
        self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,1,0)")
        self.db.execute("INSERT INTO transfer_targets(session_id,publication_id,assigned_id,reservation_generation) VALUES(?,0,10,1)", (session,))
        self.db.commit()
        values = ("site/book", 10, 1, "https://transfer/", "sess_sql:publish:0:plan-1")
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(publication_statement("INSERT OR IGNORE INTO novel_namespace_claims("), values)
            self.db.execute(publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c"), namespace_parameters(values))
            self.db.execute(publication_statement("UPDATE transfer_targets SET prefix=?,record_json=?"), ("site/book", "{}", session, 0, 10, 999))
            self.db.execute("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 0)
        self.assertIsNone(self.db.execute("SELECT record_json FROM transfer_targets").fetchone()[0])

    def test_namespace_release_requires_exact_terminal_publication_owner(self):
        session = self.session("publishing")
        owner = "sess_sql:publish:0:plan-1"
        self.db.execute("INSERT INTO transfer_targets(session_id,publication_id,assigned_id,reservation_generation,prefix) VALUES(?,0,10,1,'site/book')", (session,))
        self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch) VALUES('site/book',10,1,'https://transfer/',?,7)", (owner,))
        release = publication_statement("DELETE FROM novel_namespace_claims WHERE owner=")
        values = (owner, 10, 1, "site/book", session, 0)
        self.db.execute(release, values)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 1)
        self.db.execute("UPDATE transfer_targets SET rolled_back=1")
        self.db.execute(release, ("create:10:1", *values[1:]))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 1)
        self.db.execute(release, values)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 0)
        self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch) VALUES('site/book',10,2,'https://transfer/','create:10:2',9)")
        self.db.execute(release, values)
        self.assertEqual(self.db.execute("SELECT owner FROM novel_namespace_claims").fetchone()[0], "create:10:2")


    def test_job_creation_receipt_survives_consumption_and_rejects_rebinding(self):
        self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,1,0)")
        self.db.execute("INSERT INTO novel_creation_receipts(novel_id,reservation_generation,prefix,toc_url_fold,epoch,job_id) VALUES(10,1,'site/book','https://example/',7,'job_one')")
        self.db.execute("DELETE FROM novel_id_reservations WHERE id=10 AND generation=1")
        self.db.execute("UPDATE novel_creation_receipts SET epoch=7 WHERE novel_id=10 AND reservation_generation=1")
        self.assertEqual(self.db.execute("SELECT job_id FROM novel_creation_receipts").fetchone()[0], "job_one")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "creation_receipt_conflict"):
            self.db.execute("UPDATE novel_creation_receipts SET job_id='job_two' WHERE novel_id=10 AND reservation_generation=1")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "creation_receipt_conflict"):
            self.db.execute("UPDATE novel_creation_receipts SET epoch=9 WHERE novel_id=10 AND reservation_generation=1")
        self.assertEqual(self.db.execute("SELECT prefix,epoch,job_id FROM novel_creation_receipts").fetchone()[:], ("site/book", 7, "job_one"))


    def test_pre_toc_job_numeric_claim_is_indexed_and_cannot_be_adopted(self):
        session = self.session("publishing")
        checkpoint = {"job_id": "job_one", "creation_reservation": [10, 1], "write_origin": None}
        self.db.execute("INSERT INTO worker_jobs(job_id,kind,target,status,dedupe_key,created_at,updated_at,execution_token,lease_until,checkpoint_json) VALUES('job_one','download','planned-url','running','planned','2000','2000','lease-one','2099',?)", (json.dumps(checkpoint),))
        self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,1,0)")
        self.db.execute("INSERT INTO transfer_targets(session_id,publication_id,assigned_id,reservation_generation) VALUES(?,0,10,1)", (session,))
        self.db.commit()
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE EXISTS(SELECT 1 FROM worker_jobs j")
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(guard, (10, 1))
            self.db.execute(acquire, ("site/book", 10, 1, "https://transfer/", "sess_sql:publish:0:plan-1"))
        self.assertEqual(self.db.execute("SELECT available FROM novel_id_reservations").fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 0)
        plan = " ".join(str(row[3]) for row in self.db.execute("EXPLAIN QUERY PLAN " + guard, (10, 1)))
        self.assertIn("worker_jobs_creation_reservation", plan)
        # A retained CP is still authoritative after lease expiry/terminal;
        # only exact release and a NEW generation permit later V2 ownership.
        self.db.execute("UPDATE worker_jobs SET status='permanent',execution_token=NULL,lease_until='2000'")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(guard, (10, 1))
        self.db.execute("UPDATE novel_id_reservations SET generation=2 WHERE id=10")
        self.db.execute(guard, (10, 2))
        self.db.execute("UPDATE novel_id_reservations SET job_id='job_one' WHERE id=10")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(guard, (10, 2))
        self.db.execute("UPDATE novel_id_reservations SET job_id=NULL WHERE id=10")
        self.db.execute(guard, (10, 2))


    def test_incomplete_initializer_reclaim_cascades_private_source_pointers(self):
        session = self.session()
        self.db.execute("INSERT INTO transfer_stage_owners(stage_id,session_id,expected_size,epoch,backend,purpose) VALUES('raw-init',?,16777216,1,'d1','legacy_source')", (session,))
        self.db.execute("INSERT INTO transfer_catalog_indexes(session_id,novel_ordinal,stage_id) VALUES(?,0,'raw-init')", (session,))
        self.db.execute("INSERT INTO transfer_source_materializations(session_id,novel_ordinal,logical_path,stage_id,source_fingerprint) VALUES(?,0,'legacy.bin','raw-init','frozen')", (session,))
        self.db.execute("DELETE FROM transfer_stage_owners WHERE stage_id='raw-init'")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transfer_catalog_indexes").fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transfer_source_materializations").fetchone()[0], 0)


    def test_empty_namespace_origin_is_published_only_with_canonical_transaction(self):
        session = self.session("publishing")
        self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,1,0)")
        self.db.execute("INSERT INTO novel_access_gates(prefix,epoch,content_revision,updated_at) VALUES('site/book',7,3,0)")
        self.db.commit()
        admission = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_access_gates g")
        self.db.execute("UPDATE novel_access_gates SET content_revision=4 WHERE prefix='site/book'")
        self.db.commit()
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(admission, ("site/book", 7, 3, ""))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 0)
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c")
        values = ("site/book", 10, 1, "https://transfer/", "sess_sql:publish:0:plan-1")
        with self.db:
            self.db.execute(admission, ("site/book", 7, 4, ""))
            self.db.execute(acquire, values)
            self.db.execute(guard, namespace_parameters(values))
        self.assertEqual(self.db.execute("SELECT empty_owned FROM novel_namespace_claims").fetchone()[0], 1)
        self.assertEqual(self.db.execute("SELECT origin_url_fold FROM novel_access_gates").fetchone()[0], "")
        publish = publication_statement("UPDATE novel_access_gates SET origin_url_fold=? WHERE prefix=?")
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(publish, ("https://transfer/", "site/book"))
            self.db.execute("INSERT INTO transfer_batch_guard(id) VALUES(1)")
        self.assertEqual(self.db.execute("SELECT origin_url_fold FROM novel_access_gates").fetchone()[0], "")
        with self.db:
            self.db.execute(publish, ("https://transfer/", "site/book"))
            self.db.execute(acquire, values)
        self.assertEqual(self.db.execute("SELECT empty_owned FROM novel_namespace_claims").fetchone()[0], 1, "replay must retain the FIRST empty ownership proof")

    def test_retained_namespace_only_accepts_its_original_url(self):
        self.session("publishing")
        self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,1,0)")
        self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at,origin_url_fold) VALUES('site/book',0,'https://original/')")
        self.db.execute("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES('site/book/replace.txt',3,'old','none',0,?)", (b"old",))
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c")
        same = ("site/book", 10, 1, "https://original/", "sess_sql:publish:0:plan-1")
        self.db.execute(acquire, same)
        self.db.execute(guard, namespace_parameters(same))
        self.assertEqual(self.db.execute("SELECT empty_owned FROM novel_namespace_claims").fetchone()[0], 0)
        self.db.execute("DELETE FROM novel_namespace_claims")
        self.db.commit()
        foreign = ("site/book", 10, 1, "https://different/", "sess_sql:publish:0:plan-1")
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(acquire, foreign)
            self.db.execute(guard, namespace_parameters(foreign))
        self.assertEqual(self.db.execute("SELECT origin_url_fold FROM novel_access_gates").fetchone()[0], "https://original/")
        self.assertEqual(self.db.execute("SELECT data FROM objects WHERE object_key='site/book/replace.txt'").fetchone()[0], b"old")


    def test_reused_unpublished_binding_moves_only_from_captured_epoch_and_generation(self):
        self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,2,0)")
        self.db.execute("INSERT INTO novel_access_gates(prefix,epoch,updated_at,origin_url_fold) VALUES('site/old',7,0,'https://old/')")
        self.db.execute("INSERT INTO novel_access_gates(prefix,epoch,updated_at) VALUES('site/new',0,0)")
        self.db.execute("INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES(10,'site/old')")
        self.db.execute("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES('site/old/replace.txt',3,'old','none',0,?)", (b"old",))
        move = publication_statement("UPDATE novel_gate_bindings SET prefix=?1 WHERE novel_id=?2")
        self.db.execute("UPDATE novel_access_gates SET epoch=8 WHERE prefix='site/old'")
        self.assertEqual(self.db.execute(move, ("site/new", 10, "site/old", 7, 2)).rowcount, 0)
        self.assertEqual(self.db.execute(move, ("site/new", 10, "site/old", 8, 1)).rowcount, 0)
        self.assertEqual(self.db.execute(move, ("site/new", 10, "site/old", 8, 2)).rowcount, 1)
        self.assertEqual(self.db.execute(move, ("site/late", 10, "site/old", 8, 2)).rowcount, 0)
        self.novel(10)
        self.assertEqual(self.db.execute(move, ("site/late", 10, "site/new", 0, 2)).rowcount, 0)
        self.assertEqual(self.db.execute("SELECT prefix FROM novel_gate_bindings WHERE novel_id=10").fetchone()[0], "site/new")
        self.assertEqual(self.db.execute("SELECT origin_url_fold FROM novel_access_gates WHERE prefix='site/old'").fetchone()[0], "https://old/")
        self.assertEqual(self.db.execute("SELECT data FROM objects WHERE object_key='site/old/replace.txt'").fetchone()[0], b"old")

    def test_overwrite_cannot_claim_unknown_bytes_without_canonical_url_binding(self):
        self.session("publishing")
        self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at) VALUES('site/book',0)")
        self.db.commit()
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c")
        values = ("site/book", 10, None, "https://example.test/10/", "sess_sql:publish:0:plan-1")
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(acquire, values)
            self.db.execute(guard, namespace_parameters(values))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 0)
        self.novel(10)
        self.db.execute("INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES(10,'site/book')")
        with self.db:
            self.db.execute(publication_statement("UPDATE novel_access_gates SET origin_url_fold=?4"), values[:4])
            self.db.execute(acquire, values)
            self.db.execute(guard, namespace_parameters(values))
        self.assertEqual(self.db.execute("SELECT empty_owned FROM novel_namespace_claims").fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT origin_url_fold FROM novel_access_gates").fetchone()[0], "https://example.test/10/")


    def test_component_overlap_blocks_ancestors_and_descendants_but_not_neighbors(self):
        self.session("publishing")
        self.db.commit()
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c")
        collision = publication_statement("SELECT 1 collision FROM novel_namespace_claims WHERE")
        cases = [("shelf/book", "shelf", True), ("shelf", "shelf/book", True), ("shelf", "shelf/\U0010ffffchild", True), ("shelf/book", "shelf/book2", False), ("shelf/a%b", "shelf/a_b", False), ('shelf/引用"/book', 'shelf/引用"', True), ("shelf/日本語", "shelf/日本語x", False)]
        for target, foreign, blocked in cases:
            with self.subTest(target=target, foreign=foreign):
                self.db.execute("SAVEPOINT overlap_case")
                self.db.executemany("INSERT INTO novel_id_reservations(id,generation,available) VALUES(?,?,0)", [(10, 1), (20, 2)])
                self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at) VALUES(?,0)", (target,))
                self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch) VALUES(?,20,2,'https://other/','create:20:2',0)", (foreign,))
                values = (target, 10, 1, "https://target/", "sess_sql:publish:0:plan-1")
                parameters = namespace_parameters(values)
                self.assertEqual(self.db.execute(collision, parameters).fetchone() is not None, blocked)
                if blocked:
                    self.db.execute("SAVEPOINT candidate")
                    with self.assertRaises(sqlite3.IntegrityError):
                        self.db.execute(acquire, values)
                        self.db.execute(guard, parameters)
                    self.db.execute("ROLLBACK TO candidate")
                    self.db.execute("RELEASE candidate")
                    self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims WHERE novel_id=10").fetchone()[0], 0)
                else:
                    self.db.execute(acquire, values)
                    self.db.execute(guard, parameters)
                    self.db.execute(acquire, values)
                    self.db.execute(guard, parameters)
                    self.assertIsNone(self.db.execute(collision, parameters).fetchone(), "own exact publication replay must not conflict")
                plan = " ".join(str(row[3]) for row in self.db.execute("EXPLAIN QUERY PLAN " + collision, parameters))
                self.assertIn("MULTI-INDEX OR", plan)
                self.db.execute("ROLLBACK TO overlap_case")
                self.db.execute("RELEASE overlap_case")

    def test_current_and_retained_parent_namespaces_are_not_blindly_replaced(self):
        self.session("publishing")
        self.db.commit()
        acquire = publication_statement("INSERT OR IGNORE INTO novel_namespace_claims(")
        guard = publication_statement("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c")
        for authority in ("live_record", "retained_origin", "closed_gate"):
            with self.subTest(authority=authority):
                self.db.execute("SAVEPOINT parent_case")
                self.db.execute("INSERT INTO novel_id_reservations(id,generation,available) VALUES(10,1,0)")
                self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at) VALUES('shelf/book',0)")
                self.db.execute("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES('shelf/keep.txt',3,'old','none',0,?)", (b"old",))
                if authority == "live_record":
                    self.novel(20)
                    self.db.execute("INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES(20,'shelf')")
                elif authority == "retained_origin":
                    self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at,origin_url_fold) VALUES('shelf',0,'https://target/')")
                else:
                    self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at,closed,owner) VALUES('shelf',0,1,'maintenance')")
                values = ("shelf/book", 10, 1, "https://target/", "sess_sql:publish:0:plan-1")
                self.db.execute("SAVEPOINT candidate")
                with self.assertRaises(sqlite3.IntegrityError):
                    self.db.execute(acquire, values)
                    self.db.execute(guard, namespace_parameters(values))
                self.db.execute("ROLLBACK TO candidate")
                self.db.execute("RELEASE candidate")
                self.assertEqual(self.db.execute("SELECT data FROM objects WHERE object_key='shelf/keep.txt'").fetchone()[0], b"old")
                self.assertEqual(self.db.execute("SELECT COUNT(*) FROM novel_namespace_claims").fetchone()[0], 0)
                self.db.execute("ROLLBACK TO parent_case")
                self.db.execute("RELEASE parent_case")

    def test_namespace_bootstrap_marker_and_initializer_only_receipt_commit_together(self):
        session = self.session("publishing")
        self.db.execute("INSERT INTO transfer_execution_leases VALUES(?,'owner-one','index-one',unixepoch()+60)", (session,))
        self.db.execute("INSERT INTO transfer_backend_steps VALUES(?,'index-one','namespace_index','\"owner-one\"',NULL)", (session,))
        self.db.commit()
        marker = publication_statement("INSERT OR IGNORE INTO transfer_journal_entries(session_id,publication_id,key,value)")
        finish = re.search(r'statement\(&self\.db,"(UPDATE transfer_backend_steps[^"]*)"', STEPS).group(1)
        with self.assertRaises(sqlite3.IntegrityError), self.db:
            self.db.execute(marker, (session,))
            self.db.execute(finish, ("false", session, "namespace_index", session, "owner-one"))
            self.db.execute("INSERT INTO transfer_batch_guard(id) VALUES(1)")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transfer_journal_entries").fetchone()[0], 0)
        self.assertIsNone(self.db.execute("SELECT result_json FROM transfer_backend_steps").fetchone()[0])
        with self.db:
            self.db.execute(marker, (session,))
            self.db.execute(finish, ("false", session, "namespace_index", session, "owner-one"))
        self.assertEqual(self.db.execute("SELECT value FROM transfer_journal_entries").fetchone()[0], "AQ==")
        self.assertEqual(self.db.execute("SELECT result_json FROM transfer_backend_steps").fetchone()[0], "false")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "transfer_step_receipt_conflict"):
            self.db.execute(finish, ("true", session, "namespace_index", session, "owner-one"))
        self.db.execute("UPDATE transfer_execution_leases SET owner='owner-two',operation_id='index-two'")
        self.db.execute("INSERT INTO transfer_backend_steps VALUES(?,'index-two','namespace_index','\"owner-two\"',NULL)", (session,))
        self.db.execute(finish, ("true", session, "namespace_index", session, "owner-two"))
        self.db.execute(finish, ("true", session, "namespace_index", session, "owner-one"))
        self.assertEqual(self.db.execute("SELECT result_json FROM transfer_backend_steps WHERE operation_id='index-one'").fetchone()[0], "false")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transfer_targets").fetchone()[0], 0)

    def test_source_namespaces_reject_legacy_hierarchy_before_payload_work(self):
        overlap = re.search(r'statement\(&self\.db,"(SELECT 1 overlap[^"]*)"', PROTOCOL).group(1)
        cases = [
            ("novels/Site/12", "novels/Site/12/n1234", True),
            ("novels/Site/12/n1234", "novels/Site/12", True),
            ("novels/Site/12", "novels/Site/12x", False),
            ("novels/Site/12x", "novels/Site/12", False),
            ("novels/Site/12", "novels/Site/12/\U0010ffffsuffix", True),
            ('novels/Site/%_"/book', 'novels/Site/%_"', True),
        ]
        for authority in ("current_binding", "retained_origin", "active_claim", "closed_gate"):
            for source, foreign, blocked in cases:
                with self.subTest(authority=authority, source=source, foreign=foreign):
                    self.db.execute("SAVEPOINT source_case")
                    self.novel(10)
                    self.db.execute("INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES(10,?)", (source,))
                    self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at,origin_url_fold) VALUES(?,0,'https://source/')", (source,))
                    if authority == "current_binding":
                        self.novel(20)
                        self.db.execute("INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES(20,?)", (foreign,))
                    elif authority == "retained_origin":
                        self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at,origin_url_fold) VALUES(?,0,'https://retained/')", (foreign,))
                    elif authority == "closed_gate":
                        self.db.execute("INSERT INTO novel_access_gates(prefix,updated_at,closed,owner) VALUES(?,0,1,'maintenance')", (foreign,))
                    else:
                        self.db.execute("INSERT INTO novel_namespace_claims(prefix,novel_id,toc_url_fold,owner,epoch) VALUES(?,20,'https://claimed/','create:20:1',0)", (foreign,))
                    ancestors, lower, upper = namespace_parameters((source, 10, None, "", ""))[5:]
                    parameters = (source, 10, ancestors, lower, upper)
                    self.assertEqual(self.db.execute(overlap, parameters).fetchone() is not None, blocked)
                    plan = " ".join(str(row[3]) for row in self.db.execute("EXPLAIN QUERY PLAN " + overlap, parameters))
                    self.assertIn("novel_gate_bindings_prefix", plan)
                    self.assertIn("MULTI-INDEX OR", plan)
                    self.assertEqual(self.db.execute("SELECT COUNT(*) FROM objects").fetchone()[0], 0)
                    self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transfer_stage_owners").fetchone()[0], 0)
                    self.db.execute("ROLLBACK TO source_case")
                    self.db.execute("RELEASE source_case")


if __name__ == "__main__":
    unittest.main()
