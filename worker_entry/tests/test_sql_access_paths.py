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


if __name__ == '__main__':
    unittest.main()
