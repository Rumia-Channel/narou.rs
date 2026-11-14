// Real Worker/D1 regression: mixed current/legacy freezes and nonselected rows.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';

const base = process.env.BASE_URL;
const config = process.env.NAROU_TEST_CONFIG;
const persist = process.env.NAROU_TEST_PERSIST;
assert(/^http:\/\/(127\.0\.0\.1|localhost):/.test(base), 'Local Worker required');
assert(config && persist, 'An isolated D1 fixture is required');
const exec = promisify(execFile);
const quote = value => `'${String(value).replaceAll("'", "''")}'`;
async function sql(command) {
  const dir = mkdtempSync(join(dirname(config), 'freeze-'));
  try {
    const file = join(dir, 'query.sql');
    writeFileSync(file, command);
    const args = ['--yes', `wrangler@${process.env.NAROU_WRANGLER_VERSION || '4.151.3'}`, 'd1', 'execute', 'DB', '--local',
      '-c', config, '--persist-to', persist, '--file', file, '--json'];
    const { stdout } = await exec(process.platform === 'win32' ? 'npx.cmd' : 'npx', args, { shell: process.platform === 'win32' });
    return JSON.parse(stdout.slice(stdout.indexOf('[\n')))[0].results;
  } finally { rmSync(dir, { recursive: true, force: true }); }
}
async function api(path, body) {
  const response = await fetch(base + path, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { Authorization: `Bearer ${process.env.NAROU_ADMIN_TOKEN}`, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  assert.equal(response.status, 200, `${path}: ${text}`);
  const result = JSON.parse(text);
  assert.notEqual(result.success, false, text);
  return result;
}
const first = 990700000;
const ids = [first, first + 1, first + 2, first + 3];
async function expectStates(expected) {
  const response = await api('/api/list?all=true');
  for (const [index, id] of ids.entries()) {
    const row = response.data.find(row => row.id === id);
    assert(row, `missing ${id}`);
    assert.equal(row.frozen, expected[index], JSON.stringify(row));
    assert.equal(row.tags.filter(tag => tag === 'frozen').length, expected[index] ? 1 : 0, JSON.stringify(row));
    assert(row.tags.includes('freeze-fixture'), 'unrelated tags must survive');
  }
  const rows = await sql(`SELECT novel_id FROM frozen_novels WHERE novel_id BETWEEN ${first} AND ${first + 3} ORDER BY novel_id`);
  assert.deepEqual(rows.map(row => row.novel_id), ids.filter((_, index) => expected[index]));
}
try {
  await sql(ids.map((id, index) => {
    const tags = index === 0 ? ['freeze-fixture', 'frozen'] : ['freeze-fixture'];
    const title = `Freeze Fixture ${id}`;
    const url = `https://fixture.invalid/freeze/${id}`;
    return `INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,tags_json,tags_fold) VALUES(${id},'fixture','fixture',${quote(title)},${quote(title.toLowerCase())},${quote(title)},${quote(url)},${quote(url)},'fixture','fixture','2026-11-01T00:00:00Z',${quote(JSON.stringify(tags))},${quote(tags.join('\n'))});` +
      tags.map((tag, position) => `INSERT INTO novel_tags(novel_id,position,tag,tag_fold) VALUES(${id},${position},${quote(tag)},${quote(tag)});`).join('');
  }).join('\n'));
  // The first row has only the old frozen tag, not a freeze-store entry.
  await api('/api/novels/unfreeze', { ids: [first, first + 1] });
  await expectStates([false, false, false, false]);
  await api(`/api/novels/${first + 3}/freeze`, {});
  for (let repeat = 0; repeat < 2; ++repeat) {
    await api('/api/novels/freeze', { ids: [first + 2, first, first + 1, first] });
    await expectStates([true, true, true, true]);
  }
  assert((await api('/api/tag_list?format=json')).tags.includes('frozen'));
  await api('/api/freeze', { ids: [first, first + 2] });
  await expectStates([false, true, false, true]);
  await api('/api/novels/unfreeze', { ids: [first, first + 1, first + 2] });
  await expectStates([false, false, false, true]);
  await api(`/api/novels/${first + 3}/unfreeze`, {});
  await expectStates([false, false, false, false]);
  console.log('Worker freeze: legacy cleanup, batch on/off, mixed toggle, tags and nonselected rows passed');
} finally {
  await sql(`DELETE FROM novels WHERE id BETWEEN ${first} AND ${first + 3}`);
}
