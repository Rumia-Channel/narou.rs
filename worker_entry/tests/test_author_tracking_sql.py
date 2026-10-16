"""Exercise production author-state CAS and indexed URL membership SQL."""
import json
import re
import sqlite3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SAVE = re.search(r'pub\(crate\) const SAVE: &str = r#"(.*?)"#;', (ROOT / 'worker_entry/src/authors.rs').read_text(encoding='utf-8'), re.S).group(1)
KNOWN = re.search(r'pub\(crate\) const KNOWN_TOC_URLS: &str = r#"(.*?)"#;', (ROOT / 'worker_entry/src/d1_repository.rs').read_text(encoding='utf-8'), re.S).group(1)
A = 'https://example.test/users/1'
B = 'https://example.test/users/2'


class AuthorTrackingSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.addCleanup(self.db.close)
        for migration in sorted((ROOT / 'worker_entry/migrations').glob('*.sql')):
            self.db.executescript(migration.read_text(encoding='utf-8'))

    def snapshot(self):
        row = self.db.execute("SELECT value_json FROM app_state WHERE scope='inv' AND key='author'").fetchone()
        return row[0] if row else None

    def save(self, expected, value):
        encoded = json.dumps(value)
        return bool(self.db.execute(SAVE, (encoded, encoded, expected)).fetchall())

    def test_concurrent_first_registrations_cannot_overwrite_each_other(self):
        self.assertTrue(self.save(None, {A: 'site'}))
        self.assertFalse(self.save(None, {B: 'site'}))
        snapshot = self.snapshot()
        self.assertEqual(json.loads(snapshot), {A: 'site'})
        self.assertTrue(self.save(snapshot, {A: 'site', B: 'site'}))
        self.assertEqual(json.loads(self.snapshot()), {A: 'site', B: 'site'})

    def test_stale_removal_cannot_erase_a_concurrent_registration(self):
        self.save(None, {A: 'site'})
        stale = self.snapshot()
        self.save(stale, {A: 'site', B: 'site'})
        self.assertFalse(self.save(stale, {}))
        self.assertTrue(self.save(self.snapshot(), {B: 'site'}))
        self.assertEqual(json.loads(self.snapshot()), {B: 'site'})

    def test_stale_addition_cannot_resurrect_a_removed_author(self):
        self.save(None, {A: 'site'})
        stale = self.snapshot()
        self.assertTrue(self.save(stale, {}))
        self.assertFalse(self.save(stale, {A: 'site', B: 'site'}))
        self.assertTrue(self.save(self.snapshot(), {B: 'site'}))
        self.assertEqual(json.loads(self.snapshot()), {B: 'site'})

    def test_author_changes_preserve_other_inventory_state(self):
        self.db.execute("INSERT INTO app_state(scope,key,value_json) VALUES('inv','freeze','{\"7\":true}')")
        self.db.execute("INSERT INTO app_state(scope,key,value_json) VALUES('local','author','\"unrelated\"')")
        self.save(None, {A: 'site'})
        self.save(self.snapshot(), {})
        self.assertEqual(self.db.execute("SELECT value_json FROM app_state WHERE scope='inv' AND key='freeze'").fetchone()[0], '{"7":true}')
        self.assertEqual(self.db.execute("SELECT value_json FROM app_state WHERE scope='local' AND key='author'").fetchone()[0], '"unrelated"')

    def test_url_membership_preserves_order_duplicates_and_missing_urls(self):
        self.db.execute("INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update) VALUES(1,'','','book','book','book','https://example.test/book/1','https://example.test/book/1','site','site','now')")
        urls = ['https://example.test/book/2', 'https://example.test/book/1', 'https://example.test/book/1', 'https://example.test/book/10']
        self.assertEqual(self.db.execute(KNOWN, (json.dumps(urls),)).fetchall(), [(0, 0), (1, 1), (2, 1), (3, 0)])
        self.assertEqual(self.db.execute(KNOWN, ('[]',)).fetchall(), [])


if __name__ == '__main__':
    unittest.main()
