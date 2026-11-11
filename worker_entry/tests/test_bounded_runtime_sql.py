"""Local SQLite regressions for bounded runtime migration; no deployment calls."""
import pathlib
import sqlite3
import unittest

MIGRATION = (pathlib.Path(__file__).resolve().parents[1] / "migrations" / "0019_bounded_runtime.sql").read_text(encoding="utf-8")


class BoundedRuntimeMigration(unittest.TestCase):
    def database(self, payload, yaml="legacy: retained"):
        db = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        db.executescript("""
            CREATE TABLE app_state(scope TEXT, key TEXT, value_yaml TEXT, value_json TEXT,
                                   PRIMARY KEY(scope,key));
            CREATE TABLE worker_jobs(job_id TEXT PRIMARY KEY, attempts INTEGER NOT NULL DEFAULT 0);
        """)
        db.execute("INSERT INTO app_state VALUES('inv','section_hash_cache',?,?)", (yaml, payload))
        db.execute("INSERT INTO worker_jobs(job_id) VALUES('waiting')")
        db.executescript(MIGRATION)
        return db

    def test_valid_legacy_json_becomes_independent_novel_rows(self):
        db = self.database('{"42":{"0001":"aaa"},"99":{"0002":"bbb"}}')
        self.assertEqual(db.execute("SELECT novel_id,digests_json FROM novel_section_hashes ORDER BY novel_id").fetchall(),
                         [("42", '{"0001":"aaa"}'), ("99", '{"0002":"bbb"}')])
        self.assertEqual(db.execute("SELECT count(*) FROM app_state").fetchone()[0], 0)
        db.execute("UPDATE novel_section_hashes SET digests_json=? WHERE novel_id='42'", ('{"0001":"new"}',))
        self.assertEqual(db.execute("SELECT digests_json FROM novel_section_hashes WHERE novel_id='99'").fetchone()[0], '{"0002":"bbb"}')
        self.assertEqual(db.execute("SELECT attempts,recovery_attempts FROM worker_jobs").fetchone(), (0, 0))

    def test_unconvertible_legacy_payload_is_retained_without_json_errors(self):
        for payload in (None, "", "not-json", "[]", '"text"', '{"42":{"a":"b"},"bad":17}'):
            with self.subTest(payload=payload):
                db = self.database(payload)
                self.assertEqual(db.execute("SELECT value_yaml,value_json FROM app_state").fetchone(), ("legacy: retained", payload))


if __name__ == "__main__":
    unittest.main()
