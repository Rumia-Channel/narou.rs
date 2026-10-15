"""Execute production content publication/migration SQL against SQLite.

Run: python -m unittest discover -s worker_entry/tests -p 'test_content_storage_sql.py'
"""
import json
import re
import sqlite3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SQL = dict(re.findall(r'pub const (\w+): &str = r#"(.*?)"#;', (ROOT / 'worker_entry/src/content_store/sql.rs').read_text(encoding='utf-8'), re.S))
MIGRATION_SQL = dict(re.findall(r'pub\(crate\) const (\w+): &str = r#"(.*?)"#;', (ROOT / 'worker_entry/src/content_migration.rs').read_text(encoding='utf-8'), re.S))
BODY = 'novels/site/本文という題名/本文/1 第一話.yaml'
HISTORY = 'novels/site/本文という題名/本文/cache/version/1 第一話.yaml'


class ContentStorageSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.addCleanup(self.db.close)
        self.db.row_factory = sqlite3.Row
        for migration in sorted((ROOT / 'worker_entry/migrations').glob('*.sql')):
            self.db.executescript(migration.read_text(encoding='utf-8'))

    def mode(self, value):
        self.db.execute("INSERT INTO app_state(scope,key,value_json) VALUES ('inv','content_backend',?) ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json", (json.dumps(value),))

    def legacy(self, key=BODY):
        self.db.execute("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?,3,'old','none',0,?)", (key, b'old'))

    def publish(self, blob, *, expected='*', key=BODY, verified=1, legacy_time='', expires_at='9999-01-01T00:00:00+00:00'):
        self.db.execute('INSERT INTO content_garbage(blob_key,not_before) VALUES (?,?)', (blob, expires_at))
        values = (key, blob, 3, 3, 'none', 0, 'a' * 64, 'now', 'application/yaml', verified, expected, legacy_time)
        self.db.execute(SQL['QUEUE_PREVIOUS'], values)
        self.db.execute(SQL['PUBLISH'], values)
        self.db.execute('DELETE FROM content_garbage WHERE blob_key=? AND EXISTS(SELECT 1 FROM content_objects WHERE blob_key=?)', (blob, blob))
    def test_transition_guards_late_legacy_writes_but_not_metadata(self):
        self.legacy()
        self.mode('migrating')
        for key in (BODY, HISTORY, HISTORY.replace('/cache/', '/.cache/')):
            with self.assertRaises(sqlite3.IntegrityError):
                self.legacy(key)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute('UPDATE objects SET data=? WHERE object_key=?', (b'new', BODY))
        for key in ('novels/site/本文/toc.yaml', 'novels/site/book/本文/cache/v/.version', 'novels/本文/book/setting.ini'):
            self.legacy(key)
        self.assertEqual(bytes(self.db.execute('SELECT data FROM objects WHERE object_key=?', (BODY,)).fetchone()[0]), b'old')

    def test_subdirectory_title_body_preserves_root_metadata_during_migration(self):
        prefix = 'novels/site/本文/本文'
        metadata = {f'{prefix}/{name}' for name in ('toc.yaml', 'converter.yaml', '.illustration_cache.yaml')}
        episodes = {f'{prefix}/本文/1 toc.yaml', f'{prefix}/本文/cache/version/1 toc.yaml', f'{prefix}/本文/.cache/version/1 toc.yaml'}
        for key in metadata | episodes:
            self.legacy(key)
        self.assertEqual({r[0] for r in self.db.execute('SELECT object_key FROM legacy_content_objects')}, episodes)
        self.mode('migrating')
        for key in metadata:
            self.db.execute('UPDATE objects SET data=? WHERE object_key=?', (b'new', key))
        for index, key in enumerate(sorted(episodes)):
            self.publish(f'content/subdir-{index}.bin', key=key)
        self.assertEqual(len(self.db.execute(SQL['CUTOVER']).fetchall()), 1)
        for key in episodes:
            self.db.execute('INSERT INTO content_deletions VALUES (?)', (key,))
            self.db.execute('DELETE FROM objects WHERE object_key=?', (key,))
            self.db.execute('DELETE FROM content_deletions WHERE object_key=?', (key,))
        self.assertEqual({r[0] for r in self.db.execute('SELECT object_key FROM objects')}, metadata)
        listed = self.db.execute(SQL['LIST'], (prefix, prefix+'\U0010ffff', '', 's3', 20)).fetchall()
        self.assertEqual({r['object_key'] for r in listed}, metadata | episodes)
        for key in metadata:
            self.db.execute('DELETE FROM objects WHERE object_key=?', (key,))
            self.legacy(key)
        for key in episodes:
            with self.assertRaises(sqlite3.IntegrityError):
                self.legacy(key)

    def test_migration_absent_cas_does_not_replace_new_download(self):
        self.legacy()
        self.mode('migrating')
        self.publish('content/new.bin')
        self.publish('content/stale-copy.bin', expected='', legacy_time='old', verified=0)
        self.assertEqual(self.db.execute('SELECT blob_key FROM content_objects').fetchone()[0], 'content/new.bin')
        self.assertEqual({r[0] for r in self.db.execute('SELECT blob_key FROM content_garbage')}, {'content/stale-copy.bin'})

    def test_stale_writer_cannot_retire_live_revision(self):
        self.mode('migrating')
        self.publish('content/v1.bin')
        self.publish('content/v2.bin', expected='content/v1.bin')
        self.publish('content/v3.bin', expected='content/v1.bin')
        self.assertEqual(self.db.execute('SELECT blob_key FROM content_objects').fetchone()[0], 'content/v2.bin')
        self.assertEqual({r[0] for r in self.db.execute('SELECT blob_key FROM content_garbage')}, {'content/v1.bin', 'content/v3.bin'})

    def test_cutover_requires_coverage_and_verified_payload(self):
        self.legacy()
        self.mode('migrating')
        self.assertEqual(self.db.execute(SQL['CUTOVER']).fetchall(), [])
        self.publish('content/copied.bin', expected='', verified=0, legacy_time='old')
        self.assertEqual(self.db.execute(SQL['CUTOVER']).fetchall(), [])
        self.db.execute('UPDATE content_objects SET verified=1')
        self.assertEqual(len(self.db.execute(SQL['CUTOVER']).fetchall()), 1)
        self.assertEqual(self.db.execute("SELECT value_json FROM app_state WHERE key='content_backend'").fetchone()[0], '"s3"')
        self.assertEqual(bytes(self.db.execute('SELECT data FROM objects').fetchone()[0]), b'old')

    def test_cutover_cannot_leave_legacy_dot_cache_history_in_d1(self):
        key = HISTORY.replace('/cache/', '/.cache/')
        self.legacy(key)
        self.mode('migrating')
        self.assertEqual(self.db.execute(SQL['CUTOVER']).fetchall(), [])
        self.publish('content/dot-history.bin', key=key)
        self.assertEqual(len(self.db.execute(SQL['CUTOVER']).fetchall()), 1)

    def test_failed_publication_transaction_preserves_reference_and_garbage(self):
        self.mode('migrating')
        self.publish('content/old.bin')
        self.db.commit()
        try:
            with self.db:
                self.publish('content/bad.bin')
                self.db.execute("INSERT INTO content_objects SELECT * FROM content_objects")
        except sqlite3.IntegrityError:
            pass
        self.assertEqual(self.db.execute('SELECT blob_key FROM content_objects').fetchone()[0], 'content/old.bin')
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM content_garbage').fetchone()[0], 0)

    def test_expired_upload_cannot_publish_after_gc_may_have_removed_it(self):
        self.mode('migrating')
        self.publish('content/live.bin')
        self.publish('content/expired.bin', expires_at='2000-01-01T00:00:00+00:00')
        self.assertEqual(self.db.execute('SELECT blob_key FROM content_objects').fetchone()[0], 'content/live.bin')
        self.assertEqual({r[0] for r in self.db.execute('SELECT blob_key FROM content_garbage')}, {'content/expired.bin'})

    def test_old_deleter_cannot_remove_body_after_migration_started(self):
        self.legacy()
        self.mode('migrating')
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute('DELETE FROM objects WHERE object_key=?', (BODY,))
        with self.db:
            self.db.execute('INSERT INTO content_deletions VALUES (?)', (BODY,))
            self.db.execute('DELETE FROM objects WHERE object_key=?', (BODY,))
            self.db.execute('DELETE FROM content_deletions WHERE object_key=?', (BODY,))
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM objects').fetchone()[0], 0)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM content_deletions').fetchone()[0], 0)

    def test_listing_merges_refs_and_legacy_without_adjacent_prefix_leaks(self):
        self.legacy()
        self.legacy('novels/site/本文という題名-extra/本文/2 Other.yaml')
        self.legacy('novels/site/本文という題名/toc.yaml')
        self.mode('migrating')
        self.publish('content/body.bin')
        prefix = 'novels/site/本文という題名'
        first = self.db.execute(SQL['LIST'], (prefix, prefix+'\U0010ffff', '', 'migrating', 1)).fetchall()
        self.assertEqual([r['object_key'] for r in first], [prefix+'/toc.yaml'])
        second = self.db.execute(SQL['LIST'], (prefix, prefix+'\U0010ffff', first[0]['object_key'], 'migrating', 10)).fetchall()
        self.assertEqual([r['object_key'] for r in second], [BODY])

    def test_expired_lease_owner_cannot_save_completion(self):
        first = self.db.execute(MIGRATION_SQL['ACQUIRE_LEASE'], ('first', '20', '10')).fetchall()
        self.assertEqual(first[0]['token'], 'first')
        self.assertEqual(self.db.execute(MIGRATION_SQL['ACQUIRE_LEASE'], ('second', '30', '15')).fetchall(), [])
        self.assertEqual(self.db.execute(MIGRATION_SQL['ACQUIRE_LEASE'], ('second', '40', '21')).fetchone()['token'], 'second')
        self.assertEqual(self.db.execute(MIGRATION_SQL['SAVE_STATE'], ('{"verify_done":true}', 'first', '22')).fetchall(), [])
        self.assertEqual(self.db.execute(MIGRATION_SQL['SAVE_STATE'], ('{"verify_done":false}', 'second', '22')).fetchone()['key'], 'migrate_content')
        self.assertFalse(json.loads(self.db.execute("SELECT value_json FROM app_state WHERE key='migrate_content'").fetchone()[0])['verify_done'])

    def test_inventory_reports_legacy_and_referenced_content_separately(self):
        self.legacy()
        self.legacy(HISTORY)
        self.mode('migrating')
        self.publish('content/body.bin')
        counts = self.db.execute(MIGRATION_SQL['INVENTORY_SQL']).fetchone()
        self.assertEqual(dict(counts), {
            'remaining_legacy': 2, 'legacy_bytes': 6,
            'references': 1, 'pending_garbage': 0,
        })


if __name__ == '__main__':
    unittest.main()
