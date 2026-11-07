"""Exercise production D1 SQL semantics and deterministic SQLite work bounds."""
import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[2]
OBJECT_SQL = re.search(r'LIST_PAGE_SQL: &str = r#"(.*?)"#;',
    (ROOT / 'src/db/object_query.rs').read_text(encoding='utf-8'), re.S).group(1)
CLASSIFY_SQL = re.search(r'async fn reconcile_attributes.*?prepare\(r#"(.*?)"#\)',
    (ROOT / 'worker_entry/src/virtual_novels.rs').read_text(encoding='utf-8'), re.S).group(1)


class SqlAccessPathTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.addCleanup(self.db.close)
        for migration in sorted((ROOT / 'worker_entry/migrations').glob('*.sql')):
            self.db.executescript(migration.read_text(encoding='utf-8'))
        self.db.execute('PRAGMA foreign_keys=ON')

    def bounded(self, sql, params, max_steps):
        ticks = 0
        def progress():
            nonlocal ticks
            ticks += 100
            return int(ticks > max_steps)
        self.db.set_progress_handler(progress, 100)
        try:
            return self.db.execute(sql, params).fetchall()
        finally:
            self.db.set_progress_handler(None, 0)

    def novel(self, novel_id, attribute):
        self.db.execute("""INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,
            toc_url,toc_url_fold,sitename,sitename_fold,last_update,attribute_tag)
            VALUES (?,'a','a','t','t','t',?,?,'s','s','now',?)""",
            (novel_id, f'url/{novel_id}', f'url/{novel_id}', attribute))

    def test_tag_rows_cannot_outlive_their_novel_in_the_migrated_schema(self):
        self.novel(1, '小説')
        self.db.execute("INSERT INTO novel_tags(novel_id,position,tag,tag_fold) VALUES(1,0,'tag','tag')")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("INSERT INTO novel_tags(novel_id,position,tag,tag_fold) VALUES(99,0,'orphan','orphan')")
        self.db.execute('DELETE FROM novels WHERE id=1')
        self.assertEqual(self.db.execute('SELECT tag FROM novel_tags').fetchall(), [])

    def test_object_pages_seek_and_keep_exact_unicode_and_sibling_boundaries(self):
        self.db.executemany("INSERT INTO objects(object_key,size,updated_at) VALUES (?,1,'now')",
            ((f'novels/book/{i:06d}',) for i in range(10000)))
        self.db.executemany("INSERT INTO objects(object_key,size,updated_at) VALUES (?,1,'now')",
            [(key,) for key in ('novels/book', 'novels/bookish/1', 'novels/book/本文', '\U0010fffftail')])
        def page(prefix, cursor, cap):
            lower = prefix + '/' if prefix else ''
            return [row[0] for row in self.bounded(OBJECT_SQL,
                (prefix, lower, lower + '\U0010ffff', cursor, cap), 2000)]
        self.assertEqual(page('novels/book', '', 2), ['novels/book', 'novels/book/000000'])
        self.assertEqual(page('novels/book', 'novels/book/009998', 3),
            ['novels/book/009999', 'novels/book/本文'])
        self.db.execute("DELETE FROM objects WHERE object_key='novels/book/009998'")
        self.assertEqual(page('', 'novels/book/009998', 3),
            ['novels/book/009999', 'novels/book/本文', 'novels/bookish/1'])
        self.assertEqual(page('', 'z', 1), ['\U0010fffftail'])

    def test_virtual_classification_does_not_recompute_large_member_sets(self):
        for i in range(1, 2001):
            self.novel(i, '漫画' if i % 2 else '小説')
        groups = {str(3000+i): list(range(1, 2001)) for i in range(20)}
        for key in groups:
            self.novel(int(key), '漫画')
        self.novel(4000, '小説')
        self.novel(4001, '漫画')
        self.novel(4002, '漫画')
        groups.update({'4000': [1, 3], '4001': [2, 4], '4002': [999999]})
        self.db.execute("INSERT INTO app_state(scope,key,value_json) VALUES ('inv','virtual_novel_members',?)",
            (json.dumps(groups),))
        self.bounded(CLASSIFY_SQL, (), 450000)
        actual = dict(self.db.execute('SELECT id,attribute_tag FROM novels WHERE id>=3000'))
        self.assertEqual(actual, {**{3000+i: '漫画' for i in range(20)},
            4000: '漫画', 4001: '小説', 4002: '漫画'})

    def test_virtual_classification_ignores_unrelated_manga_and_reflects_member_changes(self):
        self.novel(1, None)
        self.novel(2, '小説')
        self.novel(3, '漫画')
        for i in range(10, 10010):
            self.novel(i, '漫画')
        groups = {'20001': [1, 2], '20002': [3], '20003': [1, 3],
                  '20004': [99999], '20005': []}
        for key in groups:
            self.novel(int(key), '小説' if key in ('20002', '20003') else '漫画')
        self.db.execute("INSERT INTO app_state(scope,key,value_json) VALUES ('inv','virtual_novel_members',?)",
                        (json.dumps(groups),))
        self.bounded(CLASSIFY_SQL, (), 10000)
        self.assertEqual(dict(self.db.execute('SELECT id,attribute_tag FROM novels WHERE id>=20001')),
                         {20001: '小説', 20002: '漫画', 20003: '小説', 20004: '漫画', 20005: '漫画'})
        # No cached classification may hide changes made by another invocation.
        self.db.execute("UPDATE novels SET attribute_tag='漫画' WHERE id=1")
        self.novel(99999, '小説')
        self.bounded(CLASSIFY_SQL, (), 10000)
        self.assertEqual(dict(self.db.execute('SELECT id,attribute_tag FROM novels WHERE id BETWEEN 20001 AND 20005')),
                         {20001: '小説', 20002: '漫画', 20003: '漫画', 20004: '小説', 20005: '漫画'})

    def test_admission_aliases_follow_identity_changes_and_rollback(self):
        self.novel(1, '小説')
        self.novel(2, '漫画')
        self.db.execute("UPDATE novels SET ncode_fold='shared',toc_url_fold='https://example.test/a/shared///' WHERE id=1")
        self.db.execute("UPDATE novels SET ncode_fold='shared',toc_url_fold='https://example.test/b/shared/' WHERE id=2")
        def lookup(target):
            return self.db.execute("SELECT MIN(novel_id) FROM novel_target_aliases WHERE target=?", (target,)).fetchone()[0]
        self.assertEqual(lookup('shared'), 1)
        self.assertEqual(lookup('a/shared'), 1)
        self.assertEqual(lookup('https://example.test/a/shared///'), 1)
        self.assertIsNone(lookup('https://example.test/a/shared'))
        self.db.execute("UPDATE novels SET ncode_fold='new',toc_url_fold='https://example.test/new/' WHERE id=1")
        self.assertEqual(lookup('shared'), 2)
        self.assertIsNone(lookup('a/shared'))
        self.assertEqual(lookup('new'), 1)
        self.db.execute("SAVEPOINT removed")
        self.db.execute("DELETE FROM novels WHERE id=2")
        self.assertIsNone(lookup('shared'))
        self.db.execute("ROLLBACK TO removed")
        self.db.execute("RELEASE removed")
        self.assertEqual(lookup('shared'), 2)
        self.db.execute("UPDATE novels SET id=4 WHERE id=1")
        self.assertEqual(lookup('new'), 4)
        changes = self.db.total_changes
        self.db.execute("UPDATE novels SET title='renamed',toc_url_fold=toc_url_fold WHERE id=4")
        self.assertEqual(self.db.total_changes - changes, 1)
        self.db.execute("DELETE FROM novels WHERE id=4")
        self.assertIsNone(lookup('new'))

    def test_admission_alias_backfill_keeps_collisions_and_literal_url_suffixes(self):
        self.novel(1, '小説')
        self.novel(2, '小説')
        self.db.execute("UPDATE novels SET ncode_fold='https://example.test/a/b/',toc_url_fold='plain' WHERE id=1")
        self.db.execute("UPDATE novels SET ncode_fold='',toc_url_fold='https://example.test/a/b/' WHERE id=2")
        self.db.executescript("""
            DROP TRIGGER novel_target_aliases_insert;
            DROP TRIGGER novel_target_aliases_update;
            DROP TRIGGER novel_target_aliases_delete;
            DROP TABLE novel_target_aliases;
        """)
        self.db.executescript((ROOT / 'worker_entry/migrations/0018_novel_target_aliases.sql').read_text(encoding='utf-8'))
        self.assertEqual(self.db.execute("SELECT target,novel_id FROM novel_target_aliases ORDER BY target,novel_id").fetchall(),
                         [('', 2), ('/example.test/a/b', 2), ('a/b', 2), ('b', 2), ('example.test/a/b', 2),
                          ('https://example.test/a/b/', 1), ('https://example.test/a/b/', 2), ('plain', 1)])


if __name__ == '__main__':
    unittest.main()
