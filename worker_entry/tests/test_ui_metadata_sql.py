"""Execute the production UI SQL against SQLite, without network or credentials.

This verifies query semantics and bounded result sizes, not D1 latency or Rust
compilation. Run: python -m unittest discover -s worker_entry/tests -p 'test_ui_*.py'
"""
from collections import Counter
import itertools
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "worker_entry/src/webui/queue.rs").read_text(encoding="utf-8")
SQL = dict(re.findall(r'const (\w+_SQL): &str = r#"(.*?)"#;', SOURCE, re.S))
ACTIVE = {"pending", "retryable", "running"}
SECONDARY = {"convert", "send", "backup", "mail"}
STATUSES = (*sorted(ACTIVE), "succeeded", "partial", "blocked", "permanent")
KINDS = ("download", "update", "auto_update", "convert", "send", "backup", "mail", "future-kind")


class UiMetadataSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.row_factory = sqlite3.Row
        self.db.executescript("""
            CREATE TABLE novels (id INTEGER PRIMARY KEY, title TEXT, extra_fields_json TEXT);
            CREATE TABLE novel_tags (
                novel_id INTEGER NOT NULL, tag TEXT NOT NULL, position INTEGER NOT NULL,
                UNIQUE(novel_id, tag)
            );
            CREATE TABLE worker_jobs (
                job_id TEXT PRIMARY KEY, kind TEXT NOT NULL, target TEXT NOT NULL,
                options TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE INDEX jobs_status ON worker_jobs(status);
        """)

    def insert_job(self, job_id, kind, status, *, target="1", options="[]", at="2026-10-03T00:00:00Z"):
        self.db.execute("INSERT INTO worker_jobs VALUES (?, ?, ?, ?, ?, ?)",
                        (job_id, kind, target, options, status, at))

    def query(self, name):
        self.assertIn(name, SQL, "test must use the production SQL, not a copied query")
        return [dict(row) for row in self.db.execute(SQL[name])]

    def test_empty_library_and_queue(self):
        for name in SQL:
            self.assertEqual(self.query(name), [])

    def test_every_status_and_lane_matches_row_based_counts(self):
        expected = Counter()
        lane_sizes = Counter()
        for index, (kind, status) in enumerate(itertools.product(KINDS, STATUSES)):
            repetitions = 1 + index % 3
            for repetition in range(repetitions):
                self.insert_job(f"job-{index}-{repetition}", kind, status)
            lane = int(kind in SECONDARY)
            expected[(status, lane)] += repetitions
            if status in ACTIVE:
                lane_sizes[lane] += repetitions
        actual = {(r["status"], r["lane"]): r["n"] for r in self.query("QUEUE_COUNTS_SQL")}
        self.assertEqual(actual, dict(expected))
        self.assertEqual({r["lane"]: r["n"] for r in self.query("LANE_COUNTS_SQL")}, dict(lane_sizes))

    def test_terminal_jobs_do_not_change_lane_sizes(self):
        self.insert_job("pending", "update", "retryable")
        for index, (kind, status) in enumerate(itertools.product(KINDS, ("succeeded", "partial", "blocked", "permanent"))):
            self.insert_job(f"terminal-{index}", kind, status)
        self.assertEqual(self.query("LANE_COUNTS_SQL"), [{"lane": 0, "n": 1}])

    def test_running_label_query_is_bounded_and_fifo(self):
        self.insert_job("b", "update", "running", target="2")
        self.insert_job("a", "convert", "running", target="3")
        self.insert_job("pending", "download", "pending", at="2020-01-01T00:00:00Z")
        rows = self.query("FIRST_RUNNING_SQL")
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["job_id"], "a")
        self.assertEqual(rows[0]["target"], "3")
        counts = self.query("QUEUE_COUNTS_SQL")
        self.assertEqual(sum(r["n"] for r in counts if r["status"] == "running"), 2)

    def test_single_running_job_keeps_fields_for_existing_label_formatter(self):
        self.insert_job("one", "update", "running", target="42", options='["--force"]')
        row, = self.query("FIRST_RUNNING_SQL")
        self.assertEqual((row["kind"], row["target"], row["options"]), ("update", "42", '["--force"]'))

    def test_tag_frequency_unicode_case_empty_tag_and_orphan(self):
        values = {1: ["冒険", "SF", "sf", "", '<script>'], 2: ["冒険", "SF"], 3: ["冒険"]}
        counts = Counter()
        for novel_id, tags in values.items():
            self.db.execute("INSERT INTO novels VALUES (?, ?, ?)", (novel_id, "not transferred", "x" * 20000))
            for position, tag in enumerate(tags):
                self.db.execute("INSERT INTO novel_tags VALUES (?, ?, ?)", (novel_id, tag, position))
                counts[tag] += 1
        # A broken imported orphan must not become a tag from a nonexistent novel.
        self.db.execute("INSERT INTO novel_tags VALUES (999, 'orphan', 0)")
        rows = self.query("TAG_NAMES_SQL")
        self.assertEqual([r["tag"] for r in rows], sorted(counts, key=lambda t: (-counts[t], t)))
        self.assertTrue(all(set(row) == {"tag"} for row in rows))

    def test_large_pending_queue_returns_counts_not_payloads(self):
        count = 10000
        self.db.executemany("INSERT INTO worker_jobs VALUES (?, 'update', '1', ?, 'pending', '2026-10-03T00:00:00Z')",
                            ((f"job-{i}", '["' + "x" * 512 + '"]') for i in range(count)))
        self.assertEqual(self.query("QUEUE_COUNTS_SQL"), [{"status": "pending", "lane": 0, "n": count}])
        self.assertEqual(self.query("LANE_COUNTS_SQL"), [{"lane": 0, "n": count}])
        self.assertEqual(self.query("FIRST_RUNNING_SQL"), [])

    def test_status_result_bound_for_mixed_large_queue(self):
        for i in range(1000):
            self.insert_job(str(i), KINDS[i % len(KINDS)], STATUSES[i % len(STATUSES)])
        self.assertLessEqual(len(self.query("QUEUE_COUNTS_SQL")), 14)
        self.assertLessEqual(len(self.query("FIRST_RUNNING_SQL")), 1)
        self.assertLessEqual(len(self.query("LANE_COUNTS_SQL")), 2)


if __name__ == "__main__":
    unittest.main()
