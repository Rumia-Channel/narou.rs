// Real HTTP/D1 races; virtual_race_worker.mjs holds completed membership reads
// until both callers have the same snapshot. No SQL results are fabricated.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';

const base = process.env.BASE_URL || 'http://127.0.0.1:8787';
assert(/^http:\/\/(127\.0\.0\.1|localhost):/.test(base), 'Local Worker required');
const config = process.env.NAROU_TEST_CONFIG;
const persist = process.env.NAROU_TEST_PERSIST;
assert(config && persist, 'An isolated D1 fixture is required');
const authorization = `Bearer ${process.env.NAROU_ADMIN_TOKEN || 'local-contract-token'}`;
function sql(command) {
  const dir = mkdtempSync(join(dirname(config), 'virtual-membership-'));
  try {
    const file = join(dir, 'fixture.sql');
    writeFileSync(file, command);
    const result = spawnSync(process.platform === 'win32' ? 'npx.cmd' : 'npx', [
      '--yes', `wrangler@${process.env.NAROU_WRANGLER_VERSION || '4.151.3'}`,
      'd1', 'execute', 'DB', '--local', '-c', config, '--persist-to', persist, '--file', file, '--json',
    ], { shell: process.platform === 'win32', encoding: 'utf8' });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    return JSON.parse(result.stdout).flatMap(result => result.results || []);
  } finally { rmSync(dir, { recursive: true, force: true }); }
}
async function api(path, body, phase, method, headers = {}) {
  const response = await fetch(base + path, {
    method: method || (body === undefined ? 'GET' : 'POST'),
    headers: { 'content-type': 'application/json', Authorization: authorization,
      ...(phase ? { 'x-narou-test-snapshot-race': phase } : {}), ...headers },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const text = await response.text();
  let data;
  try { data = JSON.parse(text); } catch { data = text; }
  return { status: response.status, data };
}
async function groups() { return (await api('/api/virtual_novels')).data.novels; }
function summary(id) { return sql(`SELECT general_all_no,tags_json,attribute_tag FROM novels WHERE id=${id}`)[0]; }
const members = [990300001, 990300002, 990300003, 990300004, 990300005, 990300006, 990300007, 990300008, 990300009];
const created = [];
try {
  sql(members.map(id => `INSERT INTO novels(id,title,title_fold,author,author_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,attribute_tag,general_all_no,tags_json) VALUES (${id},'Membership ${id}','membership ${id}','fixture','fixture','membership-${id}','https://example.test/membership-${id}','https://example.test/membership-${id}','fixture','fixture','2026-11-06T00:00:00Z','小説',1,json_array('membership-${id}'));`).join('\n'));
  const registrations = await Promise.all(members.slice(0, 2).map((id, index) =>
    api('/api/virtual_novels', { title: `Concurrent virtual ${index}`, members: [id] }, 'membership-create')));
  for (const result of registrations) {
    if (result.data?.id) created.push(result.data.id);
    assert.equal(result.status, 200, JSON.stringify(result));
  }
  let listed = await groups();
  assert.deepEqual(listed.filter(row => created.includes(row.id)).map(row => row.id).sort((a,b) => a-b), [...created].sort((a,b) => a-b),
    'both successful registrations must retain their membership');
  const parent = created[0];
  const appended = await Promise.all(members.slice(2, 4).map(id =>
    api(`/api/virtual_novels/${parent}/members`, { members: [id] }, 'membership-append')));
  for (const result of appended) assert.equal(result.status, 200, JSON.stringify(result));
  listed = await groups();
  assert.deepEqual([...listed.find(row => row.id === parent).members].sort((a,b) => a-b), [members[0], members[2], members[3]]);
  assert.equal(summary(parent).general_all_no, 3, 'record counts and membership must commit together');

  const duplicate = await Promise.all([0, 1].map(() =>
    api(`/api/virtual_novels/${parent}/members`, { members: [members[6]] }, 'membership-duplicate')));
  assert.deepEqual(duplicate.map(result => result.status).sort(), [200, 400],
    'identical concurrent appends must not both report success');
  assert.equal(summary(parent).general_all_no, 4);

  const competing = await Promise.all(created.map(id =>
    api(`/api/virtual_novels/${id}/members`, { members: [members[4]] }, 'membership-owner')));
  assert.deepEqual(competing.map(result => result.status).sort(), [200, 400], 'a member can have only one owner');
  listed = await groups();
  assert.equal(listed.filter(row => row.members.includes(members[4])).length, 1);
  for (const id of created) assert.equal(summary(id).general_all_no, listed.find(row => row.id === id).members.length);

  const ordered = [...listed.find(row => row.id === parent).members].reverse();
  const ordering = await Promise.all([
    api('/api/virtual_novels', { action: 'order', id: parent, members: ordered }, 'membership-order'),
    api(`/api/virtual_novels/${parent}/members`, { members: [members[7]] }, 'membership-order'),
  ]);
  assert([200, 400].includes(ordering[0].status), JSON.stringify(ordering));
  assert.equal(ordering[1].status, 200, JSON.stringify(ordering));
  listed = await groups();
  const actualOrder = listed.find(row => row.id === parent).members;
  assert.equal(actualOrder.at(-1), members[7]);
  assert.deepEqual([...actualOrder].sort((a,b) => a-b), [...ordered, members[7]].sort((a,b) => a-b),
    'a stale ordering request must not erase a concurrent append');
  if (ordering[0].status === 200) assert.deepEqual(actualOrder, [...ordered, members[7]]);
  assert.equal(summary(parent).general_all_no, actualOrder.length);

  const before = listed.find(row => row.id === parent).members;
  const beforeSummary = summary(parent);
  sql(`CREATE TRIGGER fail_virtual_summary BEFORE UPDATE OF general_all_no ON novels WHEN NEW.id=${parent} BEGIN SELECT RAISE(ABORT,'virtual summary fixture'); END;`);
  try {
    const rejected = await api(`/api/virtual_novels/${parent}/members`, { members: [members[5]] });
    assert.notEqual(rejected.status, 200, JSON.stringify(rejected));
    assert.deepEqual((await groups()).find(row => row.id === parent).members, before);
    assert.deepEqual(summary(parent), beforeSummary, 'failed publication must not leave half of the update');
  } finally { sql('DROP TRIGGER fail_virtual_summary'); }

  sql(`CREATE TRIGGER fail_virtual_remove BEFORE DELETE ON novels WHEN OLD.id=${parent} BEGIN SELECT RAISE(ABORT,'virtual removal fixture'); END;`);
  try {
    const rejected = await api(`/api/virtual_novels/${parent}`, undefined, undefined, 'DELETE');
    assert.notEqual(rejected.status, 200, JSON.stringify(rejected));
    assert.deepEqual((await groups()).find(row => row.id === parent).members, before);
    assert.deepEqual(summary(parent), beforeSummary);
  } finally { sql('DROP TRIGGER fail_virtual_remove'); }

  sql(`UPDATE novels SET attribute_tag='漫画' WHERE id IN (${[...before, members[8]].join(',')})`);
  assert.equal((await groups()).find(row => row.id === parent).attribute_tag, '漫画');
  const classificationAppend = api(`/api/virtual_novels/${parent}/members`, { members: [members[8]] },
    undefined, undefined, { 'x-narou-test-snapshot-pause': 'member-classification' });
  try {
    const paused = await fetch(base + '/__test/virtual-pause/member-classification/wait');
    assert.equal(paused.status, 200);
    sql(`UPDATE novels SET attribute_tag='小説' WHERE id=${before[0]}`);
    const selected = await api(`/api/virtual_novels/${parent}/attribute`, { attribute_tag: '小説' });
    assert.equal(selected.status, 200, JSON.stringify(selected));
  } finally {
    await fetch(base + '/__test/virtual-pause/member-classification/release');
  }
  assert.equal((await classificationAppend).status, 200);
  assert.equal(summary(parent).attribute_tag, '小説', 'a stale member snapshot must not overwrite a successful mixed-attribute selection');
  assert.equal((await groups()).find(row => row.id === parent).attribute_tag, '小説');

  const removal = await Promise.all([
    api(`/api/virtual_novels/${parent}`, undefined, 'membership-remove', 'DELETE'),
    api(`/api/virtual_novels/${parent}/members`, { members: [members[5]] }, 'membership-remove'),
  ]);
  assert.equal(removal[0].status, 200, JSON.stringify(removal));
  assert([200, 400].includes(removal[1].status), JSON.stringify(removal));
  assert(!(await groups()).some(row => row.id === parent));
  assert.deepEqual(sql(`SELECT id FROM novels WHERE id=${parent}`), []);
  assert.deepEqual(sql(`SELECT id FROM novels WHERE id IN (${members.join(',')}) ORDER BY id`).map(row => row.id), members,
    'removing a virtual book must preserve its real members');
  console.log('Virtual membership atomicity workerd contracts passed');
} finally {
  sql('DROP TRIGGER IF EXISTS fail_virtual_summary');
  for (const id of created) await api(`/api/virtual_novels/${id}`, undefined, undefined, 'DELETE');
  const ids = [...members, ...created].join(',');
  sql(`DELETE FROM novel_tags WHERE novel_id IN (${ids}); DELETE FROM novels WHERE id IN (${ids});`);
}
