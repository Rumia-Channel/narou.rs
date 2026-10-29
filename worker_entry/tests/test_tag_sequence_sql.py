"""Execute the production tag-sequence SQL against SQLite, without network.

This verifies the CAS upsert semantics of `app_state(scope='inv',
key='tag_sequence')` and the library tag query, not D1 latency or Rust
compilation.

Run: python -m unittest discover -s worker_entry/tests -p 'test_tag_sequence*.py'
"""
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "worker_entry/src/tag_sequence.rs").read_text(encoding="utf-8")
SQL = dict(re.findall(r'const (\w+_SQL): &str = r#"(.*?)"#;', SOURCE, re.S))
SQL.update(re.findall(r'const (\w+_SQL): &str =\s*"(.*?)";', SOURCE, re.S))


class TagSequenceSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.row_factory = sqlite3.Row
        self.db.executescript("""
            CREATE TABLE novels (id INTEGER PRIMARY KEY, title TEXT);
            CREATE TABLE novel_tags (
                novel_id INTEGER NOT NULL, tag TEXT NOT NULL, position INTEGER NOT NULL,
                UNIQUE(novel_id, tag)
            );
            CREATE TABLE app_state (
                scope TEXT NOT NULL, key TEXT NOT NULL, value_json TEXT, value_yaml TEXT,
                PRIMARY KEY(scope, key)
            );
        """)

    def save(self, value_json, expected):
        """Run the production CAS upsert. `expected` is the snapshot the writer
        read (None when the row did not exist yet)."""
        self.assertIn("SAVE_SQL", SQL, "test must use the production SQL")
        rows = self.db.execute(
            SQL["SAVE_SQL"], (value_json, value_json, expected)
        ).fetchall()
        return [dict(row) for row in rows]

    def stored(self):
        row = self.db.execute(
            "SELECT value_json, value_yaml FROM app_state WHERE scope='inv' AND key='tag_sequence'"
        ).fetchone()
        return dict(row) if row else None

    def test_first_save_inserts_the_row(self):
        rows = self.save('{"order":["a"]}', None)
        self.assertEqual([row["key"] for row in rows], ["tag_sequence"])
        stored = self.stored()
        self.assertEqual(stored["value_json"], '{"order":["a"]}')
        # YAML も同じ内容を保存する (YAML 管理への書き戻し互換)。
        self.assertIn("order", stored["value_yaml"])

    def test_stale_snapshot_does_not_overwrite(self):
        self.save('{"order":["a"]}', None)
        # 別 writers が読んだ古いスナップショットからの更新は失敗する。
        rows = self.save('{"order":["b"]}', '{"order":["old"]}')
        self.assertEqual(rows, [])
        self.assertEqual(self.stored()["value_json"], '{"order":["a"]}')

    def test_matching_snapshot_updates_the_row(self):
        self.save('{"order":["a"]}', None)
        rows = self.save('{"order":["a","b"]}', '{"order":["a"]}')
        self.assertEqual([row["key"] for row in rows], ["tag_sequence"])
        self.assertEqual(self.stored()["value_json"], '{"order":["a","b"]}')

    def test_library_tags_report_the_minimal_novel_id(self):
        self.db.execute("INSERT INTO novels VALUES (1, 'one')")
        self.db.execute("INSERT INTO novels VALUES (2, 'two')")
        self.db.execute("INSERT INTO novel_tags VALUES (2, 'shared', 0)")
        self.db.execute("INSERT INTO novel_tags VALUES (1, 'shared', 0)")
        self.db.execute("INSERT INTO novel_tags VALUES (1, 'own', 0)")
        # 削除済み小説のタグは数えない。
        self.db.execute("INSERT INTO novel_tags VALUES (9, 'gone', 0)")
        rows = [
            dict(row)
            for row in self.db.execute(SQL["LIBRARY_TAGS_SQL"])
        ]
        self.assertEqual(
            sorted((row["tag"], row["novel_id"]) for row in rows),
            [("own", 1), ("shared", 1)],
        )

    def test_select_reads_the_stored_value(self):
        self.assertEqual(self.db.execute(SQL["SELECT_SQL"]).fetchall(), [])
        self.save('{"order":["a"]}', None)
        rows = [dict(row) for row in self.db.execute(SQL["SELECT_SQL"])]
        self.assertEqual([row["value_json"] for row in rows], ['{"order":["a"]}'])
        self.assertIn("order", rows[0]["value_yaml"])

    def test_a_native_written_row_keeps_the_yaml_payload(self):
        # native は value_json に `{}` を、value_yaml に YAML を書く。
        self.db.execute(
            "INSERT INTO app_state(scope,key,value_json,value_yaml) "
            "VALUES ('inv','tag_sequence','{}','order:\n- a\n- b\n')"
        )
        rows = [dict(row) for row in self.db.execute(SQL["SELECT_SQL"])]
        self.assertEqual(rows[0]["value_json"], "{}")
        self.assertIn("order", rows[0]["value_yaml"])


if __name__ == "__main__":
    unittest.main()
