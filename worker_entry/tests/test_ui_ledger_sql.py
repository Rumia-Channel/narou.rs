"""Execute the production ledger enqueue SQL against SQLite, without D1.

Models the batch enqueue semantics at the SQL level: the partial unique dedupe
index (`worker_jobs_active_dedupe`), `INSERT OR IGNORE` per-statement change
counts (the signal `enqueue_batch` uses to find ignored inserts), the batched
active-key lookup, and id-collision retries.

Run: python -m unittest discover -s worker_entry/tests -p 'test_ui_*.py'
"""
import json
import re
import sqlite3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "worker_entry/src/ledger.rs").read_text(encoding="utf-8")
SQL = dict(re.findall(r'const (\w+_SQL): &str = r#"(.*?)"#;', SOURCE, re.S))

ACTIVE = ("pending", "retryable", "running")


class LedgerEnqueueSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.row_factory = sqlite3.Row
        self.db.executescript("""
            CREATE TABLE worker_jobs (
                job_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                target TEXT NOT NULL,
                options TEXT NOT NULL,
                status TEXT NOT NULL,
                dedupe_key TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                lease_until TEXT,
                execution_token TEXT,
                checkpoint_json TEXT
            );
            CREATE UNIQUE INDEX worker_jobs_active_dedupe
                ON worker_jobs(dedupe_key)
                WHERE status IN ('pending', 'running', 'retryable');
        """)
        self.clock = 0

    def now(self):
        self.clock += 1
        return f"2026-10-12T00:00:{self.clock:02d}.000000000Z"

    def insert_ignoring(self, job_id, dedupe_key, *, kind="update", target="1", at=None):
        """One statement of the production INSERT OR IGNORE batch.

        Returns the per-statement change count, which is what the Worker reads
        from the D1 result meta to tell an insert from an ignored one.
        """
        created = at or self.now()
        before = self.db.total_changes
        self.db.execute(
            SQL["INSERT_OR_IGNORE_SQL"],
            (job_id, kind, target, "[]", dedupe_key, created, created),
        )
        return self.db.total_changes - before

    def active_by_dedupe(self, *keys):
        rows = self.db.execute(
            SQL["ACTIVE_BY_DEDUPE_KEYS_SQL"], (json.dumps(list(keys)),)
        )
        # The Rust lookup inserts the first row per key (oldest wins);
        # `setdefault` mirrors that instead of overwriting with the last row.
        winners = {}
        for row in rows:
            winners.setdefault(row["dedupe_key"], row["job_id"])
        return winners

    def test_insert_or_ignore_counts_each_statement(self):
        self.assertEqual(self.insert_ignoring("j1", "k1"), 1)
        # Same active dedupe key: the partial unique index ignores the row.
        self.assertEqual(self.insert_ignoring("j2", "k1"), 0)
        self.assertEqual(self.active_by_dedupe("k1"), {"k1": "j1"})

    def test_duplicate_plans_in_one_batch_share_one_row(self):
        # Two plans with the same dedupe key in one request: the first inserts,
        # the second is ignored and must resolve to the first row's id.
        changes = [
            self.insert_ignoring("dup-1", "k2"),
            self.insert_ignoring("dup-2", "k2"),
        ]
        self.assertEqual(changes, [1, 0])
        self.assertEqual(self.active_by_dedupe("k2"), {"k2": "dup-1"})

    def test_terminal_rows_release_the_dedupe_key(self):
        self.assertEqual(self.insert_ignoring("old", "k3"), 1)
        self.db.execute("UPDATE worker_jobs SET status = 'succeeded' WHERE job_id = 'old'")
        # The index only covers active rows, so a later identical request
        # enqueues fresh instead of deduping onto a terminal row.
        self.assertEqual(self.insert_ignoring("new", "k3"), 1)
        self.assertEqual(self.active_by_dedupe("k3"), {"k3": "new"})

    def test_active_lookup_ignores_missing_and_terminal_keys(self):
        self.assertEqual(self.insert_ignoring("pending", "k4"), 1)
        self.assertEqual(self.insert_ignoring("done", "k5"), 1)
        self.db.execute("UPDATE worker_jobs SET status = 'partial' WHERE job_id = 'done'")
        self.assertEqual(
            self.active_by_dedupe("k4", "k5", "absent"),
            {"k4": "pending"},
        )

    def test_id_collision_is_not_confused_with_a_dedupe_race(self):
        # A primary-key collision yields changes == 0 just like a dedupe race;
        # the batch path must re-read the key, find nothing for the new key,
        # and retry that row with a regenerated id.
        self.assertEqual(self.insert_ignoring("same-id", "k6"), 1)
        self.assertEqual(self.insert_ignoring("same-id", "k7"), 0)
        self.assertEqual(self.active_by_dedupe("k7"), {})
        self.assertEqual(self.insert_ignoring("regenerated", "k7"), 1)
        self.assertEqual(self.active_by_dedupe("k7"), {"k7": "regenerated"})

    def test_dedupe_key_json_binding_round_trips_special_characters(self):
        key = 'download:https://例.example/users/7?x="1"\t--force'
        self.assertEqual(self.insert_ignoring("j9", key), 1)
        self.assertEqual(self.active_by_dedupe(key), {key: "j9"})

    def test_legacy_duplicate_active_rows_resolve_to_the_oldest(self):
        # The partial index makes this impossible for rows this ledger wrote;
        # if a legacy row set violates it, the batched lookup must still agree
        # on one identity (oldest created_at, then job_id).
        legacy = sqlite3.connect(":memory:")
        self.addCleanup(legacy.close)
        legacy.row_factory = sqlite3.Row
        legacy.executescript("""
            CREATE TABLE worker_jobs (
                job_id TEXT PRIMARY KEY, kind TEXT NOT NULL, target TEXT NOT NULL,
                options TEXT NOT NULL, status TEXT NOT NULL, dedupe_key TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                lease_until TEXT, execution_token TEXT, checkpoint_json TEXT
            );
        """)
        for job_id, at in (("later", "2026-10-12T00:00:02Z"), ("earlier", "2026-10-12T00:00:01Z")):
            legacy.execute(
                SQL["INSERT_OR_IGNORE_SQL"],
                (job_id, "update", "1", "[]", "k8", at, at),
            )
        rows = legacy.execute(SQL["ACTIVE_BY_DEDUPE_KEYS_SQL"], (json.dumps(["k8"]),))
        winners = {}
        for row in rows:
            winners.setdefault(row["dedupe_key"], row["job_id"])
        self.assertEqual(winners, {"k8": "earlier"})

    def test_frozen_skip_never_touches_a_running_or_terminal_row(self):
        # Freeze race safety: the batch's frozen snapshot settles only rows
        # that have not started. A dedupe hit on the row already executing
        # keeps its status, lease and execution token, so the live execution
        # still owns the row (and re-checks the freeze itself).
        self.assertEqual(self.insert_ignoring("pending", "k9", at="2026-10-12T00:00:00Z"), 1)
        self.assertEqual(self.insert_ignoring("running", "k9-run", at="2026-10-12T00:00:00Z"), 1)
        self.db.execute(
            "UPDATE worker_jobs SET status = 'running', lease_until = '2026-10-12T00:15:00Z',"
            " execution_token = 'e1' WHERE job_id = 'running'"
        )
        self.assertEqual(self.insert_ignoring("done", "k9-done", at="2026-10-12T00:00:00Z"), 1)
        self.db.execute("UPDATE worker_jobs SET status = 'succeeded' WHERE job_id = 'done'")

        def settle(job_id):
            before = self.db.total_changes
            self.db.execute(
                SQL["SETTLE_SKIPPED_SQL"],
                ("partial", "frozen", "2026-10-12T00:01:00Z", job_id),
            )
            return self.db.total_changes - before

        self.assertEqual(settle("pending"), 1)
        row = self.db.execute(
            "SELECT status, last_error, lease_until, execution_token FROM worker_jobs"
            " WHERE job_id = 'pending'"
        ).fetchone()
        self.assertEqual(tuple(row), ("partial", "frozen", None, None))

        self.assertEqual(settle("running"), 0)
        row = self.db.execute(
            "SELECT status, lease_until, execution_token FROM worker_jobs"
            " WHERE job_id = 'running'"
        ).fetchone()
        self.assertEqual(tuple(row), ("running", "2026-10-12T00:15:00Z", "e1"))

        self.assertEqual(settle("done"), 0)
        self.assertEqual(
            self.db.execute("SELECT status FROM worker_jobs WHERE job_id = 'done'").fetchone()[0],
            "succeeded",
        )


if __name__ == "__main__":
    unittest.main()
