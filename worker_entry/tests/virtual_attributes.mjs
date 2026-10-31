// Local workerd contract: actual D1 rows and HTTP handlers, no external sites.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
const base = process.env.BASE_URL || 'http://127.0.0.1:8787';
if (!/^http:\/\/(127\.0\.0\.1|localhost):/.test(base)) throw new Error('Local Worker required');
const config = process.env.VIRTUAL_TEST_CONFIG || 'wrangler.test.toml';
function sql(command) {
  const dir = mkdtempSync('.wrangler/virtual-attributes-');
  try {
    const file = join(dir, 'fixture.sql');
    writeFileSync(file,command);
    const r = spawnSync(process.platform === 'win32' ? 'npx.cmd' : 'npx', ['--yes', 'wrangler@4.151.3', 'd1', 'execute', 'DB', '--local', '-c', config, '--file', file], { shell: process.platform === 'win32', encoding: 'utf8' });
    assert.equal(r.status, 0, r.stdout + r.stderr);
  } finally { rmSync(dir,{recursive:true,force:true}); }
}
async function api(path, body) {
  const r = await fetch(base + path, { method: body === undefined ? 'GET' : 'POST', headers: { 'content-type': 'application/json', Authorization: `Bearer ${process.env.NAROU_ADMIN_TOKEN || 'local-contract-token'}` }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
  return { status: r.status, data: await r.json() };
}
const ids = [990000001,990000002,990000003];
const created = [];
try {
  for (const [i,id] of ids.entries()) sql(`INSERT INTO novels(id,title,title_fold,author,author_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,attribute_tag,general_all_no) VALUES (${id},'Virtual attribute ${id}','virtual attribute ${id}','fixture','fixture','virtual-attribute-${id}','https://example.test/virtual-attribute-${id}','https://example.test/virtual-attribute-${id}','fixture','fixture','2026-10-30T00:00:00Z','${i === 0 ? '小説' : '漫画'}',1)`);
  for (const [members, expected, mixed] of [[[ids[0]],'小説',false],[[ids[1],ids[2]],'漫画',false],[[ids[0],ids[1]],'小説',true]]) {
    const result = await api('/api/virtual_novels/classify',{members});
    assert.equal(result.status,200); assert.equal(result.data.attribute_tag,expected); assert.equal(result.data.mixed,mixed);
  }
  for (const members of [[],[0],[ids[1],ids[1]]]) assert.equal((await api('/api/virtual_novels/classify',{members})).status,400);
  let r = await api('/api/virtual_novels',{title:'Mixed fixture',members:[ids[0],ids[1]],attribute_tag:'漫画'});
  assert.equal(r.status,200,JSON.stringify(r)); created.push(r.data.id);
  const mixedId = r.data.id;
  r = await api(`/api/virtual_novels/${mixedId}/attribute`,{attribute_tag:'小説'}); assert.equal(r.status,200);
  assert.equal(r.data.attribute_tag,'小説');
  r = await api(`/api/virtual_novels/${mixedId}/attribute`,{attribute_tag:'invalid'}); assert.equal(r.status,400);
  const row = (await api('/api/virtual_novels')).data.novels.find(n=>n.id===mixedId);
  assert.equal(row.attribute_tag,'小説'); assert.equal(row.mixed,true); assert.ok(!row.tags.includes('小説'));
  r = await api('/api/virtual_novels',{title:'Manga fixture',members:[ids[2]]});
  assert.equal(r.status,200);created.push(r.data.id);
  assert.equal((await api(`/api/virtual_novels/${r.data.id}/attribute`,{attribute_tag:'小説'})).status,400);
  let manga = (await api('/api/virtual_novels')).data.novels.find(n=>n.id===r.data.id);
  assert.equal(manga.attribute_tag,'漫画'); assert.equal(manga.mixed,false);
  // A legacy misclassified virtual row is repaired before normal indexed reads.
  sql(`UPDATE novels SET attribute_tag='小説' WHERE id=${r.data.id}`);
  await api('/api/list?all=true');
  manga = (await api('/api/virtual_novels')).data.novels.find(n=>n.id===r.data.id);
  assert.equal(manga.attribute_tag,'漫画');
  // Preserve the manual choice when the member set remains mixed.
  await api(`/api/virtual_novels/${mixedId}/attribute`,{attribute_tag:'漫画'});
  await api('/api/list?all=true');
  assert.equal((await api('/api/virtual_novels')).data.novels.find(n=>n.id===mixedId).attribute_tag,'漫画');
  await fetch(base + `/api/virtual_novels/${r.data.id}`,{method:'DELETE',headers:{Authorization:`Bearer ${process.env.NAROU_ADMIN_TOKEN || 'local-contract-token'}`}});
  assert.equal((await api(`/api/virtual_novels/${mixedId}/members`,{members:[ids[2]]})).status,200);
  const appended = (await api('/api/virtual_novels')).data.novels.find(n=>n.id===mixedId);
  assert.equal(appended.attribute_tag,'漫画');
  assert.deepEqual(appended.members,[ids[0],ids[1],ids[2]]);
  assert.equal(appended.title,'Mixed fixture');
  console.log('Virtual attribute workerd contracts passed');
} finally {
  for (const id of created) await fetch(base + `/api/virtual_novels/${id}`,{method:'DELETE',headers:{Authorization:`Bearer ${process.env.NAROU_ADMIN_TOKEN || 'local-contract-token'}`}});
  sql(`DELETE FROM novel_tags WHERE novel_id IN (${ids.join(',')}); DELETE FROM novels WHERE id IN (${ids.join(',')})`);
}
