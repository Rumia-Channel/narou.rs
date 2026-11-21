// Real Worker/D1 regression for the initial Web UI list on a large library.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runWrangler } from './read_worker_fixture.mjs';


const base = new URL(process.env.BASE_URL);
const config = process.env.NAROU_TEST_CONFIG;
const persist = process.env.NAROU_TEST_PERSIST;
assert(base.protocol === 'http:' && ['127.0.0.1', 'localhost'].includes(base.hostname));
assert(config && persist, 'An isolated D1 fixture is required');
const headers = { Authorization: `Bearer ${process.env.NAROU_ADMIN_TOKEN}` };
const size = 5500;
const first = 990800001;
const last = first + size - 1;
const title = id => `list-snapshot-${id}`;
const prefix = id => `novels/list-snapshot/${title(id)}`;
// Windows では `shell:true` の spawnSync が `--command` の値を cmd.exe /c で
// 空白分割するため、複文・括弧を含む SQL が wrangler へ壊れて届く
// ("--command または --file が必要")。他の fixture と同じく `--file` 経由にする。
const sqlDir = mkdtempSync(join(tmpdir(), 'narou-list-snapshot-'));
let sqlIndex = 0;
const sql = command => {
  const file = join(sqlDir, `fixture-${sqlIndex++}.sql`);
  writeFileSync(file, command);
  return runWrangler([
    'd1', 'execute', 'DB', '--local', '-c', config, '--persist-to', persist, '--file', file, '--json',
  ]);
};

const list = query => fetch(new URL(`/api/list?${query}`, base), { headers, signal: AbortSignal.timeout(35_000) });
async function payload(query) {
  const response = await list(query);
  if (response.status !== 200) assert.fail(`${response.status}: ${await response.text()}`);
  return response.json();
}

try {
  sql(`WITH RECURSIVE fixture(id) AS (VALUES(${first}) UNION ALL SELECT id+1 FROM fixture WHERE id<${last})
    INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update)
    SELECT id,'fixture','fixture','list-snapshot-'||id,'list-snapshot-'||id,'list-snapshot-'||id,
      'https://listing-test.invalid/'||id,'https://listing-test.invalid/'||id,'list-snapshot','list-snapshot',
      '2026-11-01T00:00:00Z' FROM fixture;`);

  const start = performance.now();
  const full = await payload('all=true');
  const ids = full.data.filter(row => row.id >= first && row.id <= last).map(row => row.id);
  assert.deepEqual(ids, Array.from({ length: size }, (_, index) => first + index));
  assert(full.records_total >= size && full.records_filtered === full.data.length);
  const page = await payload('filter=list-snapshot&start=2&length=3&order[0][dir]=desc');
  assert.equal(page.records_total, full.records_total);
  assert.equal(page.records_filtered, size);
  assert.deepEqual(page.data.map(row => row.id), [last - 2, last - 3, last - 4]);

  // The fixtures have NULL ncode. A negated single Any term must still keep
  // the non-matching works, including on the paginated query with later binds.
  const terms = encodeURIComponent(`list-snapshot -${title(first)}`);
  const withoutFirst = await payload(`filter=${terms}&length=2`);
  assert.equal(withoutFirst.records_filtered, size - 1);
  assert.deepEqual(withoutFirst.data.map(row => row.id), [first + 1, first + 2]);
  const alternatives = encodeURIComponent(`${title(first)}|${title(last)}`);
  assert.deepEqual((await payload(`filter=${alternatives}&all=true`)).data.map(row => row.id), [first, last]);

  const gates = JSON.parse(sql("SELECT count(*) AS count FROM novel_access_gates WHERE prefix LIKE 'novels/list-snapshot/%'"));
  assert.equal(gates[0].results[0].count, 0, 'metadata-only reads must not create durable leases or gates');

  const setHidden = async action => {
    const response = await fetch(new URL('/api/hidden', base), {
      method: 'POST',
      headers: { ...headers, 'Content-Type': 'application/json' },
      body: JSON.stringify({ action, novels: [first] }),
      signal: AbortSignal.timeout(10_000),
    });
    assert.equal(response.status, 200);
    assert.equal((await response.json()).success, true);
  };
  await setHidden('add');
  try {
    const hidden = await payload(`filter=${title(first)}&all=true`);
    assert.equal(hidden.records_filtered, 0);
    assert.deepEqual(hidden.data, []);
    const included = await payload(`filter=${title(first)}&all=true&show_hidden=true`);
    assert.equal(included.records_filtered, 1);
    assert.deepEqual(included.data.map(row => row.id), [first]);
  } finally {
    await setHidden('remove');
  }

  // Include an unbound legacy record: gate lookup must use the same derived
  // namespace as ordinary storage, not only newer novel_gate_bindings rows.
  sql(`INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at) VALUES('${prefix(last)}',1,1,'snapshot-fixture',unixepoch());`);
  const blocked = await list(`filter=${title(last)}&all=true`);
  assert.equal(blocked.status, 503);
  assert.equal((await blocked.json()).error.code, 'novel_recovering');
  const unaffected = await payload(`filter=${title(first)}&all=true`);
  assert.deepEqual(unaffected.data.map(row => row.id), [first]);
  sql(`UPDATE novel_access_gates SET closed=0,owner=NULL WHERE prefix='${prefix(last)}';`);
  const recovered = await payload(`filter=${title(last)}&all=true`);
  assert.deepEqual(recovered.data.map(row => row.id), [last]);
  console.log(`Worker list: ${size} visible works, matching counts and sorting, no lease writes, recovery boundary verified (${Math.round(performance.now() - start)} ms including checks)`);
} finally {
  try {
    sql(`DELETE FROM novel_access_gates WHERE prefix LIKE 'novels/list-snapshot/%'; DELETE FROM novels WHERE id BETWEEN ${first} AND ${last};`);
  } finally {
    rmSync(sqlDir, { recursive: true, force: true });
  }
}
