// Real HTTP/D1 selection and queue execution. Only selection probes pause
// delivery; queued rows remain real and are cancelled before delivery resumes.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';

const base = process.env.BASE_URL || 'http://127.0.0.1:8787';
assert(/^http:\/\/(127\.0\.0\.1|localhost):/.test(base), 'Local Worker required');
const config = process.env.NAROU_TEST_CONFIG;
const persist = process.env.NAROU_TEST_PERSIST;
assert(config && persist, 'An isolated D1 fixture is required');
const exec = promisify(execFile);
const quote = value => value === null ? 'NULL' : `'${String(value).replaceAll("'", "''")}'`;
async function sql(command) {
  const dir = mkdtempSync(join(dirname(config), 'virtual-updates-'));
  try {
    const file = join(dir, 'fixture.sql');
    writeFileSync(file, command);
    const result = await exec(process.platform === 'win32' ? 'npx.cmd' : 'npx', [
      '--yes', `wrangler@${process.env.NAROU_WRANGLER_VERSION || '4.151.3'}`,
      'd1', 'execute', 'DB', '--local', '-c', config, '--persist-to', persist, '--file', file, '--json',
    ], { shell: process.platform === 'win32', encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
    return JSON.parse(result.stdout).flatMap(result => result.results || []);
  } finally { rmSync(dir, { recursive: true, force: true }); }
}
async function api(path, body) {
  const response = await fetch(base + path, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'content-type': 'application/json', Authorization: `Bearer ${process.env.NAROU_ADMIN_TOKEN || 'local-contract-token'}` },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const text = await response.text();
  assert.equal(response.status, 200, `${path}: ${text}`);
  return JSON.parse(text);
}
async function terminalJob(id) {
  const deadline = Date.now() + 60000;
  let result;
  do {
    result = await api(`/api/jobs/${id}`);
    if (['succeeded', 'partial', 'permanent', 'blocked'].includes(result.status)) return result;
    await new Promise(resolve => setTimeout(resolve, 100));
  } while (Date.now() < deadline);
  assert.fail(`job did not settle: ${JSON.stringify(result)}`);
}
const first = 990500000;
const real = first + 100;
const virtualIds = Array.from({ length: 100 }, (_, index) => first + index);
const stateWhere = "(scope='local' AND key IN ('update.sort-by','update.disabled-author-tracking')) OR (scope='inv' AND key='alias') OR (scope='scheduler' AND key='auto_update')";
const savedState = await sql(`SELECT scope,key,value_yaml,value_json FROM app_state WHERE ${stateWhere}`);
const putState = (scope, key, value) => {
  const encoded = quote(JSON.stringify(value));
  return `INSERT INTO app_state(scope,key,value_yaml,value_json) VALUES(${quote(scope)},${quote(key)},${encoded},${encoded}) ON CONFLICT(scope,key) DO UPDATE SET value_yaml=excluded.value_yaml,value_json=excluded.value_json;`;
};
async function noVirtualJobs() {
  const rows = await sql(`SELECT target FROM worker_jobs WHERE kind='update' AND status='pending' AND CAST(target AS INTEGER) BETWEEN ${first} AND ${real - 1}`);
  assert.deepEqual(rows, [], 'virtual parents must not enter the update queue');
}
async function clearQueue() { assert((await api('/api/queue/clear', {})).success); }
async function tick() {
  const response = await fetch(base + '/__scheduled?cron=*%20*%20*%20*%20*');
  assert.equal(response.status, 200, await response.text());
}
let previousRetrySetting;
try {
  assert((await api('/__test/jobs/pause', {})).success);
  await sql([...virtualIds, real].map(id => {
    const virtual = id !== real;
    const title = `!!! virtual-update-fixture ${String(id - first).padStart(3, '0')}`;
    const url = `https://fixture.invalid/virtual-update-${id}`;
    const tags = ['virtual-update-fixture', `vs_${first}`];
    const ncode = virtual ? `vs_${id}` : `n${id}aa`;
    return `INSERT INTO novels(id,ncode,ncode_fold,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,domain,last_update,tags_json,tags_fold) VALUES(${id},${quote(ncode)},${quote(ncode)},'fixture','fixture',${quote(title)},${quote(title)},${quote(title)},${quote(url)},${quote(url)},'fixture','fixture','fixture.invalid','2026-11-01T00:00:00Z',${quote(JSON.stringify(tags))},${quote(tags.join('\n'))});`;
  }).join('\n') + [...virtualIds, real].map(id =>
    `INSERT INTO novel_tags(novel_id,position,tag,tag_fold) VALUES(${id},0,'virtual-update-fixture','virtual-update-fixture'),(${id},1,'vs_${first}','vs_${first}');`).join('\n')
    + putState('local', 'update.disabled-author-tracking', true)
    + putState('inv', 'alias', { 'virtual-update-alias': String(first) }));

  for (const targets of [[String(first)], [`vs_${first + 1}`], ['virtual-update-alias']]) {
    const result = await api('/api/update', { targets, force: true });
    assert.equal(result.success, true, JSON.stringify(result));
    assert.equal(result.count, 0, 'virtual-only selection must not become update-all');
    assert.deepEqual(result.job_ids, []);
  }
  assert.equal((await api('/api/update', { targets: [`vs_${real + 1}`] })).success, false,
    'an unknown virtual-looking target must still report not found');
  const selected = await api('/api/update', { targets: [String(first), String(real), String(real)] });
  assert.equal(selected.count, 1, 'keep the real work even when it carries a vs_ tag');
  await noVirtualJobs();
  await clearQueue();
  assert.equal((await api('/api/update', { targets: [`vs_${first}`] })).count, 1,
    'a matching member tag still expands before ncode lookup');
  await noVirtualJobs();
  await clearQueue();

  const all = await api('/api/update', {});
  const expected = (await sql("SELECT COUNT(*) AS count FROM novels WHERE ncode IS NULL OR NOT (ncode GLOB 'vs_[0-9]*' AND substr(ncode,4) NOT GLOB '*[^0-9]*')"))[0].count;
  assert.equal(all.count, expected);
  await noVirtualJobs();
  await clearQueue();
  assert.equal((await api('/api/update_by_tag', { tags: ['virtual-update-fixture'], exclusion_tags: [] })).count, 1);
  await noVirtualJobs();
  await clearQueue();
  const generic = await api('/api/jobs', { kind: 'Update', targets: [], options: [] });
  assert.equal(generic.ids.length, expected, 'targetless generic Update must include the real library');
  await noVirtualJobs();
  await clearQueue();
  console.log('PASS all, selected, aliased, tagged and generic update selection excludes virtual parents');

  for (const sort of ['id', 'title']) {
    const cursor = sort === 'id' ? first - 1 : (await sql(`SELECT COUNT(*) AS count FROM novels WHERE title_fold < '!!! virtual-update-fixture 000'`))[0].count;
    const checkpoint = { generation: 999, state: 'running', cursor, scan_sort_key: sort === 'id' ? null : sort,
      started_at: '2026-11-01T00:00:00Z', last_run: null, planner_token: null, planner_lease_until: null };
    await sql(putState('local', 'update.sort-by', sort) + putState('scheduler', 'auto_update', checkpoint));
    await tick();
    const after = JSON.parse((await sql("SELECT value_json FROM app_state WHERE scope='scheduler' AND key='auto_update'"))[0].value_json);
    assert.equal(after.state, 'running', `${sort}: a full excluded page is not the end`);
    assert.equal(after.cursor, sort === 'id' ? real - 1 : cursor + 100);
    await noVirtualJobs();
    after.planner_lease_until = null;
    await sql(putState('scheduler', 'auto_update', after));
    await tick();
    const next = await sql(`SELECT target FROM worker_jobs WHERE kind='update' AND status='pending' AND target=${quote(real)}`);
    assert.equal(next.length, 1, `${sort}: the later real work must be scheduled`);
    await noVirtualJobs();
    await clearQueue();
  }
  console.log('PASS keyset and sorted-offset scans advance across 100 excluded virtual records');

  assert((await api('/__test/jobs/resume', {})).success);
  const before = await sql(`SELECT ncode,toc_url,title,last_update,last_check_date FROM novels WHERE id=${first}`);
  for (const target of [String(first), `https://fixture.invalid/virtual-update-${first}`]) {
    const job = await api('/api/jobs', { kind: 'Update', targets: [target], options: ['--force'] });
    assert.equal(job.ids.length, 1);
    const result = await terminalJob(job.ids[0]);
    assert.equal(result.status, 'succeeded', JSON.stringify(result));
    assert.equal(result.last_error, null);
  }
  assert.deepEqual(await sql(`SELECT ncode,toc_url,title,last_update,last_check_date FROM novels WHERE id=${first}`), before);
  console.log('PASS direct/old-style virtual update jobs finish without a site lookup or record mutation');

  previousRetrySetting = (await api('/api/global_setting')).settings
    .find(setting => setting.name === 'queue.max-retries')?.value ?? null;
  assert((await api('/api/global_setting', { settings: { 'queue.max-retries': 0 } })).success);
  assert((await api('/__test/jobs/pause', {})).success);
  const corrupt = await api('/api/jobs', { kind: 'Update', targets: [String(real)], options: ['--force'] });
  assert.equal(corrupt.ids.length, 1);
  await sql(`UPDATE novels SET extra_fields_yaml='[]',extra_fields_bytes=2 WHERE id=${real};
    UPDATE worker_jobs SET updated_at='2000-01-01T00:00:00Z' WHERE job_id=${quote(corrupt.ids[0])};`);
  assert((await api('/__test/jobs/resume', {})).success);
  await tick(); // Redeliver the real pending row after deliberately held delivery.
  const failed = await terminalJob(corrupt.ids[0]);
  assert.equal(failed.status, 'permanent', JSON.stringify(failed));
  assert.equal(failed.attempts, 1, 'a decode failure must consume the configured attempt');
  assert.equal((await sql(`SELECT extra_fields_yaml FROM novels WHERE id=${real}`))[0].extra_fields_yaml, '[]');
  console.log('PASS invalid stored metadata reaches bounded retry settlement without being replaced');
} finally {
  await clearQueue();
  await sql(`DELETE FROM novels WHERE id BETWEEN ${first} AND ${real}; DELETE FROM app_state WHERE ${stateWhere};`
    + savedState.map(row => `INSERT INTO app_state(scope,key,value_yaml,value_json) VALUES(${quote(row.scope)},${quote(row.key)},${quote(row.value_yaml)},${quote(row.value_json)});`).join('\n'));
  if (previousRetrySetting !== undefined) {
    assert((await api('/api/global_setting', { settings: { 'queue.max-retries': previousRetrySetting } })).success);
  }
  await api('/__test/jobs/resume', {});
}
