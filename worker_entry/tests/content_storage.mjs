// Isolated behavioral smoke: actual Worker/Wasm, local D1, S3 HTTP fixture.
// Build first with worker-build --release, then: node tests/content_storage.mjs
import assert from 'node:assert/strict';
import http from 'node:http';
import { createHash } from 'node:crypto';
import { brotliDecompressSync, inflateRawSync } from 'node:zlib';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const windows = process.platform === 'win32';
const npx = windows ? 'npx.cmd' : 'npx';
mkdirSync(join(root, '.wrangler'), { recursive: true });
const work = mkdtempSync(join(root, '.wrangler', 'content-smoke-'));
const config = join(work, 'wrangler.toml');
const persist = join(work, 'state');
const token = 'isolated-content-smoke-token';
const objects = new Map();
let failPut = false;
let sourceGeneration = 1;
let sourceSubtitle = '第一話';
let child;
let logs = '';
let port;
const xml = (s) => String(s).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
const etag = (bytes) => `"${createHash('md5').update(bytes).digest('hex')}"`;
const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url, 'http://127.0.0.1');
    const key = decodeURIComponent(url.pathname);
    if (req.method === 'GET' && key === '/smoke-novel/') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(`<h1>Controlled Story</h1><span class="author">Storage Author</span><a href="/smoke-novel/1/">${sourceSubtitle}</a>`);
      return;
    }
    if (req.method === 'GET' && key === '/smoke-novel/1/') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(`<div class="body"><p>${(sourceGeneration === 1 ? '初回取得の本文です。' : sourceGeneration === 2 ? '更新後の本文です。' : '失敗する更新の本文です。').repeat(100)}</p></div>`);
      return;
    }
    if (req.method === 'GET' && url.searchParams.get('list-type') === '2') {
      const prefix = url.searchParams.get('prefix') ?? '';
      const bucket = key.replace(/\/$/, '');
      const after = url.searchParams.get('continuation-token') ?? '';
      const all = [...objects.keys()].filter((k) => k.startsWith(`${bucket}/${prefix}`) && k.slice(bucket.length + 1) > after).sort();
      const selected = all.slice(0, Number(url.searchParams.get('max-keys') ?? 1000));
      const more = all.length > selected.length;
      const content = selected.map((k) => `<Contents><Key>${xml(k.slice(bucket.length + 1))}</Key><LastModified>2026-10-15T00:00:00Z</LastModified><ETag>${xml(etag(objects.get(k)))}</ETag><Size>${objects.get(k).length}</Size></Contents>`).join('');
      res.setHeader('content-type', 'application/xml');
      res.end(`<ListBucketResult><Name>narou-local</Name><Prefix>${xml(prefix)}</Prefix><KeyCount>${selected.length}</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>${more}</IsTruncated>${content}${more ? `<NextContinuationToken>${xml(selected.at(-1).slice(bucket.length + 1))}</NextContinuationToken>` : ''}</ListBucketResult>`);
      return;
    }
    if (req.method === 'PUT') {
      if (failPut && !req.headers['x-amz-copy-source']) { req.resume(); res.writeHead(503); res.end('<Error><Code>SlowDown</Code></Error>'); return; }
      if (req.headers['x-amz-copy-source']) {
        const source = decodeURIComponent(req.headers['x-amz-copy-source']);
        const bytes = objects.get(source.startsWith('/') ? source : `/${source}`);
        if (!bytes) { res.writeHead(404); res.end('<Error><Code>NoSuchKey</Code></Error>'); return; }
        objects.set(key, Buffer.from(bytes));
        res.setHeader('content-type', 'application/xml');
        res.end(`<CopyObjectResult><LastModified>2026-10-15T00:00:00Z</LastModified><ETag>${xml(etag(bytes))}</ETag></CopyObjectResult>`);
        return;
      }
      const chunks = [];
      let length = 0;
      for await (const chunk of req) { length += chunk.length; assert(length <= 16 * 1024 * 1024); chunks.push(chunk); }
      const bytes = Buffer.concat(chunks);
      objects.set(key, bytes); res.setHeader('etag', etag(bytes)); res.end(); return;
    }
    if (req.method === 'DELETE') { objects.delete(key); res.writeHead(204); res.end(); return; }
    const bytes = objects.get(key);
    if (!bytes) { res.writeHead(404); res.end('<Error><Code>NoSuchKey</Code></Error>'); return; }
    res.setHeader('etag', etag(bytes));
    if (req.headers.range) {
      res.writeHead(206, { 'content-range': `bytes 0-0/${bytes.length}`, 'content-length': 1 });
      res.end(bytes.subarray(0, 1)); return;
    }
    res.setHeader('content-length', bytes.length); res.end(req.method === 'HEAD' ? undefined : bytes);
  } catch (error) { res.writeHead(500); res.end(String(error)); }
});
const sourceServer = http.createServer(server.listeners('request')[0]);

function run(args, options = {}) {
  const result = spawnSync(npx, ['wrangler', ...args], { cwd: root, shell: windows, encoding: 'utf8', ...options });
  if (result.status !== 0) throw new Error(`wrangler failed: ${result.stderr}\n${result.stdout}`);
  return result.stdout;
}
function sql(command) {
  // Always --local, with a fresh dedicated persistence root. Never a real D1.
  const path = join(work, 'query.sql');
  writeFileSync(path, command);
  const out = run(['d1', 'execute', 'DB', '--local', '--config', config, '--persist-to', persist, '--file', path, '--json']);
  return JSON.parse(out.slice(out.indexOf('[\n')));
}
const scalar = (command) => Object.values(sql(command)[0].results[0])[0];
const quote = (s) => `'${String(s).replaceAll("'", "''")}'`;
function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}
function objectInsert(key, value) {
  const bytes = Buffer.from(typeof value === 'string' ? value : JSON.stringify(value));
  return `INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (${quote(key)},${bytes.length},'2026-10-15T00:00:00+00:00','none',${crc32(bytes)},X'${bytes.toString('hex')}');`;
}
async function start() {
  logs = '';
  child = spawn(npx, ['wrangler', 'dev', '--config', config, '--persist-to', persist, '--port', String(port)], { cwd: root, shell: windows, detached: !windows, stdio: ['ignore', 'pipe', 'pipe'] });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', (chunk) => { logs = (logs + chunk).slice(-300000); });
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`worker exited: ${logs}`);
    try { if ((await fetch(`http://127.0.0.1:${port}/health/live`, { headers: { Connection: 'close' } })).ok) return; } catch {}
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`Worker did not start: ${logs}`);
}
async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, 'exit');
  if (windows) spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
  else {
    try { process.kill(-child.pid, 'SIGTERM'); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  await exited;
  child = undefined;
}
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  return fetch(`http://127.0.0.1:${port}${path}`, { method, headers: { Authorization: `Bearer ${token}`, 'Content-Type': typeof body === 'string' ? 'application/yaml' : 'application/json', Connection: 'close' }, body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body) });
}
async function download(target, force, expectedFailure = false) {
  const accepted = await (await api('/api/download', { targets: [target], force })).json();
  assert(accepted.success, JSON.stringify(accepted));
  const deadline = Date.now() + 120000;
  let job;
  do {
    job = await (await api(`/api/jobs/${accepted.results[0].job_id}`)).json();
    if (['succeeded', 'permanent', 'blocked'].includes(job.status) || (expectedFailure && job.status === 'retryable')) break;
    await new Promise((resolve) => setTimeout(resolve, 500));
  } while (Date.now() < deadline);
  assert.equal(job.status, expectedFailure ? 'retryable' : 'succeeded', JSON.stringify(job));
  return job;
}
async function migrationCli(apply = false) {
  const cli = spawn(process.execPath, [join(root, '../scripts/migrate-worker-content.mjs'), ...(apply ? ['--apply'] : [])], {
    cwd: root, env: { ...process.env, BASE_URL: `http://127.0.0.1:${port}`, NAROU_ADMIN_TOKEN: token, NAROU_CONTENT_MIGRATION_LIMIT: '1', CF_ACCESS_CLIENT_ID: '', CF_ACCESS_CLIENT_SECRET: '' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  for (const stream of [cli.stdout, cli.stderr]) stream.on('data', (chunk) => { output += chunk; });
  const [code] = await once(cli, 'exit');
  assert.equal(code, 0, output);
  console.log(output.trim());
}
async function phase(action, expectedStatus = 200) {
  const response = await api('/api/admin/content-migration', { action, limit: 1 });
  const report = await response.json();
  assert.equal(response.status, expectedStatus, JSON.stringify(report));
  console.log(`content migration ${action}: ${report.backend}, done=${report.done}, HTTP ${response.status}`);
  return report;
}
async function complete(action) {
  for (let count = 0; count < 100; count++) { const report = await phase(action); if (report.done) return report; }
  throw new Error(`phase ${action} did not finish`);
}
function epubText(bytes) {
  let end = bytes.length - 22;
  while (end >= 0 && bytes.readUInt32LE(end) !== 0x06054b50) end--;
  assert(end >= 0, 'EPUB must contain a ZIP central directory');
  let cursor = bytes.readUInt32LE(end + 16);
  const texts = [];
  for (let index = 0; index < bytes.readUInt16LE(end + 10); index++) {
    assert.equal(bytes.readUInt32LE(cursor), 0x02014b50);
    const nameLength = bytes.readUInt16LE(cursor + 28);
    const name = bytes.subarray(cursor + 46, cursor + 46 + nameLength).toString();
    const local = bytes.readUInt32LE(cursor + 42);
    const dataStart = local + 30 + bytes.readUInt16LE(local + 26) + bytes.readUInt16LE(local + 28);
    const data = bytes.subarray(dataStart, dataStart + bytes.readUInt32LE(cursor + 20));
    if (name.endsWith('.xhtml')) texts.push((bytes.readUInt16LE(cursor + 10) === 8 ? inflateRawSync(data) : data).toString());
    cursor += 46 + nameLength + bytes.readUInt16LE(cursor + 30) + bytes.readUInt16LE(cursor + 32);
  }
  return texts.join('\n');
}
async function epub() {
  const response = await api('/api/novels/1/download.epub');
  assert.equal(response.status, 200, await response.clone().text());
  const text = epubText(Buffer.from(await response.arrayBuffer()));
  assert(text.includes('初版本文がここにあります'), 'EPUB must contain the saved current body');
  assert(text.includes('第二話の本文です'), 'EPUB must include the second episode');
  assert(!text.includes('変更前の旧版本文'), 'history must not replace the current episode');
}

try {
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const s3Port = server.address().port;
  sourceServer.listen(s3Port, '::1'); await once(sourceServer, 'listening');
  const portProbe = http.createServer(); portProbe.listen(0, '127.0.0.1'); await once(portProbe, 'listening');
  port = portProbe.address().port; await new Promise((resolve) => portProbe.close(resolve));
  let toml = readFileSync(join(root, 'wrangler.toml'), 'utf8').replace(/\[build\][^\[]*/, '');
  toml = toml.replace(/^main = .*$/m, `main = ${JSON.stringify(join(root, 'build/worker/shim.mjs').replaceAll('\\', '/'))}`)
    .replace(/^directory = "public"$/m, `directory = ${JSON.stringify(join(root, 'public').replaceAll('\\', '/'))}`)
    .replace(/^migrations_dir = "migrations"$/m, `migrations_dir = ${JSON.stringify(join(root, 'migrations').replaceAll('\\', '/'))}`)
    .replace('http://127.0.0.1:9000', `http://127.0.0.1:${s3Port}`);
  writeFileSync(config, toml);
  writeFileSync(join(work, '.dev.vars'), `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32, 7).toString('base64')}\nS3_ACCESS_KEY_ID=local-test-key\nS3_SECRET_ACCESS_KEY=local-test-secret\n`);
  run(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
  const prefix = 'novels/Storage Smoke/本文/本文';
  const subtitle = (index, name) => ({ index: String(index), href: `https://example.com/n1234ab/${index}/`, chapter: '', subchapter: '', subtitle: name, file_subtitle: name, subdate: '2026-10-15' });
  const first = subtitle(1, '第一話'); const second = subtitle(2, '第二話');
  const section = (sub, body) => ({ ...sub, element: { data_type: 'html', introduction: '', body: `<p>${body.repeat(100)}</p>`, postscript: '' } });
  const firstKey = `${prefix}/本文/1 第一話.yaml`;
  sql(`INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,ncode,ncode_fold,general_all_no,length,use_subdirectory) VALUES (1,'Author','author','本文保存検証','本文保存検証','本文','https://example.com/n1234ab/','https://example.com/n1234ab/','Storage Smoke','storage smoke','2026-10-15T00:00:00Z','n1234ab','n1234ab',2,1000,1); UPDATE novel_id_sequence SET next_id=2;`
    + objectInsert(`${prefix}/toc.yaml`, { title: '本文保存検証', author: 'Author', toc_url: 'https://example.com/n1234ab/', story: '', subtitles: [first, second], novel_type: 1 })
    + objectInsert(firstKey, section(first, '初版本文がここにあります。'))
    + objectInsert(`${prefix}/本文/2 第二話.yaml`, section(second, '第二話の本文です。'))
    + objectInsert(`${prefix}/本文/cache/old/1 第一話.yaml`, section(first, '変更前の旧版本文。'))
    + objectInsert(`${prefix}/本文/.cache/legacy/1 点付き旧話.yaml`, section(subtitle(1, '点付き旧話'), '点付きキャッシュの旧本文。'))
    + objectInsert(`${prefix}/setting.ini`, '') + objectInsert(`${prefix}/replace.txt`, '')
    + objectInsert(`${prefix}/converter.yaml`, {}) + objectInsert(`${prefix}/.illustration_cache.yaml`, {}));
  await start();
  assert.equal((await fetch(`http://127.0.0.1:${port}/api/admin/content-migration`, { headers: { Connection: 'close' } })).status, 401);
  await migrationCli();
  const plan = await phase('plan'); assert.equal(plan.planned, 4); assert.equal(plan.backend, 'd1');
  await epub();
  failPut = true; await phase('copy', 503); failPut = false;
  assert.equal(scalar('SELECT COUNT(*) FROM content_objects'), 0, 'failed PUT cannot publish a reference');
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 4, 'failed PUT cannot remove legacy content');
  await complete('copy');
  assert.equal(scalar('SELECT COUNT(*) FROM content_objects'), 4);
  const refs = sql('SELECT object_key,blob_key,encoding,sha256 FROM content_objects ORDER BY object_key')[0].results;
  for (const ref of refs) {
    const stored = objects.get(`/narou-local/narou/local/${ref.blob_key}`);
    const bytes = ref.encoding === 'brotli' ? brotliDecompressSync(stored) : stored;
    assert.equal(createHash('sha256').update(bytes).digest('hex'), ref.sha256);
    const expectedBody = ref.object_key.includes('/.cache/') ? '点付きキャッシュの旧本文。' : ref.object_key.includes('/cache/') ? '変更前の旧版本文。' : ref.object_key === firstKey ? '初版本文がここにあります。' : '第二話の本文です。';
    assert.equal(JSON.parse(bytes).element.body, `<p>${expectedBody.repeat(100)}</p>`);
  }
  await phase('cutover', 409);
  const damaged = `/narou-local/narou/local/${refs.find((ref) => ref.object_key === firstKey).blob_key}`;
  const original = objects.get(damaged); objects.set(damaged, Buffer.from('corrupt'));
  let corruptionRejected = false;
  for (let page = 0; page <= refs.length; page++) {
    const response = await api('/api/admin/content-migration', { action: 'verify', limit: 1 });
    if (response.status === 503) { corruptionRejected = true; break; }
    assert.equal(response.status, 200, await response.clone().text());
    const progress = await response.json();
    assert(!progress.done, 'corrupt content cannot finish verification');
  }
  assert(corruptionRejected, 'verification must reject corrupt S3 bytes regardless of listing order');
  objects.set(damaged, original);
  await complete('verify'); await phase('cutover');
  objects.delete(damaged);
  assert(!(await api('/api/novels/1/download.epub')).ok, 'missing S3 must not fall back to retained D1');
  objects.set(damaged, original);
  await epub();
  await migrationCli(true);
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0);
  assert.equal(scalar('SELECT COUNT(*) FROM content_objects'), 4);
  assert.equal(scalar("SELECT COUNT(*) FROM objects WHERE object_key LIKE '%/toc.yaml'"), 1);
  assert.equal(scalar(`SELECT COUNT(*) FROM objects WHERE object_key IN (${['toc.yaml', 'converter.yaml', '.illustration_cache.yaml'].map((name) => quote(`${prefix}/${name}`)).join(',')})`), 3, 'subdirectory-layout root metadata must remain in D1 after purge');
  assert.equal(scalar("SELECT COUNT(*) FROM content_objects WHERE object_key LIKE '%/toc.yaml' OR object_key LIKE '%/converter.yaml' OR object_key LIKE '%/.illustration_cache.yaml'"), 0);
  const mode = await (await api('/api/storage/mode')).json();
  assert.equal(mode.content_backend, 's3'); assert.equal(mode.illustration_backend, 'd1');
  await stop(); await start(); await epub();
  const migratedHistory = await (await api('/api/diff_list?target=1')).text();
  assert(migratedHistory.includes('第一話') && migratedHistory.includes('点付き旧話'), 'both migrated cache layouts must remain readable in diff history');
  const target = `http://localhost:${s3Port}/smoke-novel/`;
  const definition = `name: Controlled Source
domain: localhost
top_url: http://localhost:${s3Port}
url: 'http://localhost:${s3Port}/(?<ncode>smoke-novel)/?'
sitename: Controlled Source
version: 1.0
confirm_over18: no
toc_url: '${target}'
title: '<h1>(?<title>.+?)</h1>'
author: '<span class="author">(?<author>.+?)</span>'
subtitles: '<a href="(?<href>/smoke-novel/(?<index>\\d+)/)">(?<subtitle>.+?)</a>'
body_pattern: '<div class="body">(?<body>.+?)</div>'
min_interval: 0.01
`;
  const savedSite = await (await api('/api/sites/localhost.yaml', definition, 'PUT')).json();
  assert(savedSite.success, JSON.stringify(savedSite));
  let originalEpisode;
  for (const force of [false, true]) {
    sourceGeneration = force ? 2 : 1;
    await download(target, force);
    assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0, 'downloads must not duplicate body blobs into D1');
    const current = sql("SELECT * FROM content_objects WHERE object_key LIKE '%/smoke-novel%/本文/%' AND instr(object_key,'/cache/')=0")[0].results;
    assert.equal(current.length, 1);
    const bytes = objects.get(`/narou-local/narou/local/${current[0].blob_key}`);
    const text = (current[0].encoding === 'brotli' ? brotliDecompressSync(bytes) : bytes).toString();
    assert(text.includes((force ? '更新後の本文です。' : '初回取得の本文です。').repeat(100)));
    if (!force) originalEpisode = current[0];
    else {
      assert.notEqual(current[0].sha256, originalEpisode.sha256);
      const history = sql(`SELECT * FROM content_objects WHERE instr(object_key,'/smoke-novel')>0 AND instr(object_key,'/cache/')>0 AND sha256=${quote(originalEpisode.sha256)}`)[0].results;
      assert.equal(history.length, 1, 'update history must retain the exact previous episode');
      assert.notEqual(history[0].blob_key, originalEpisode.blob_key, 'history must own an independent immutable revision');
      const old = objects.get(`/narou-local/narou/local/${history[0].blob_key}`);
      assert((history[0].encoding === 'brotli' ? brotliDecompressSync(old) : old).toString().includes('初回取得の本文です。'));
    }
  }
  console.log('PASS actual download and changed-body force update retain exact S3 history without D1 body copies');
  const downloadedId = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
  const downloadedEpub = await api(`/api/novels/${downloadedId}/download.epub`);
  assert.equal(downloadedEpub.status, 200, await downloadedEpub.clone().text());
  const downloadedText = epubText(Buffer.from(await downloadedEpub.arrayBuffer()));
  assert(downloadedText.includes('更新後の本文です。'));
  assert(!downloadedText.includes('初回取得の本文です。'), 'EPUB must use current body, not history');
  const diffHistory = await (await api(`/api/diff_list?target=${downloadedId}`)).text();
  assert(diffHistory.includes('第一話'), 'new S3 history must be readable in the existing diff catalog');
  const cleaned = await (await api('/api/diff_clean', { target: downloadedId })).json();
  assert(cleaned.success, JSON.stringify(cleaned));
  assert.equal(scalar("SELECT COUNT(*) FROM content_objects WHERE instr(object_key,'/smoke-novel')>0 AND instr(object_key,'/cache/')>0"), 0);
  const afterClean = await api(`/api/novels/${downloadedId}/download.epub`);
  assert.equal(afterClean.status, 200);
  assert(epubText(Buffer.from(await afterClean.arrayBuffer())).includes('更新後の本文です。'), 'diff cleanup must preserve current body');
  const stable = sql("SELECT * FROM content_objects WHERE object_key LIKE '%/smoke-novel%/本文/%' AND instr(object_key,'/cache/')=0")[0].results[0];
  failPut = true; sourceGeneration = 3;
  await download(target, true, true);
  assert.equal(scalar(`SELECT COUNT(*) FROM content_objects WHERE object_key=${quote(stable.object_key)} AND blob_key=${quote(stable.blob_key)}`), 1, 'a failed replacement PUT must preserve the previous current body');
  const unchanged = await api(`/api/novels/${downloadedId}/download.epub`);
  assert.equal(unchanged.status, 200);
  assert(epubText(Buffer.from(await unchanged.arrayBuffer())).includes('更新後の本文です。'));
  failPut = false; sourceGeneration = 2;
  console.log('PASS failed replacement PUT preserves the current episode and readable EPUB');
  const cancelled = await (await api('/api/queue/cancel', {})).json();
  assert(cancelled.success, JSON.stringify(cancelled));
  sourceSubtitle = '改題した話';
  await download(target, true);
  const renamed = sql("SELECT * FROM content_objects WHERE object_key LIKE '%/smoke-novel%/本文/%' AND instr(object_key,'/cache/')=0")[0].results;
  assert.equal(renamed.length, 1, 'renamed subtitles must not leave duplicate current references');
  assert(renamed[0].object_key.endsWith('/本文/1 改題した話.yaml'));
  assert.equal(scalar(`SELECT COUNT(*) FROM content_objects WHERE object_key=${quote(stable.object_key)}`), 0);
  const renamedEpub = await api(`/api/novels/${downloadedId}/download.epub`);
  assert.equal(renamedEpub.status, 200);
  const renamedText = epubText(Buffer.from(await renamedEpub.arrayBuffer()));
  assert(renamedText.includes('改題した話') && renamedText.includes('更新後の本文です。'));
  assert(!renamedText.includes('失敗する更新の本文です。'));
  console.log('PASS renamed subtitles publish the new body and TOC before retiring the old current key');
  const deleted = await api('/api/novels/1', { with_file: true }, 'DELETE');
  assert.equal(deleted.status, 200, await deleted.clone().text());
  await complete('gc');
  assert.equal(scalar(`SELECT COUNT(*) FROM content_objects WHERE instr(object_key,${quote(prefix)})=1`), 0);
  for (const ref of refs) assert(!objects.has(`/narou-local/narou/local/${ref.blob_key}`), 'deleted episode revisions must be removed from S3');
  console.log('PASS migration, actual compressed bytes, corruption/PUT failure, strict reads, EPUB contents, purge, restart, independent illustration mode, and deletion');
  if (process.env.CONTENT_SMOKE_CONTRACT === '1') {
    const contract = spawn(process.execPath, [join(root, 'tests/contract.mjs')], {
      cwd: root, env: { ...process.env, BASE_URL: `http://127.0.0.1:${port}`, NAROU_ADMIN_TOKEN: token,
        NAROU_AUTH_REQUIRED: 'true', NAROU_REQUIRE_S3: 'false', CONTRACT_QUEUE: '1', CONTRACT_SKIP_ACCESS_PROBE: '1',
        CF_ACCESS_CLIENT_ID: '', CF_ACCESS_CLIENT_SECRET: '' }, stdio: 'inherit',
    });
    const [code] = await once(contract, 'exit');
    assert.equal(code, 0, 'existing Worker HTTP contracts must also pass after S3 cutover');
  }
} catch (error) {
  console.error(error);
  console.error(logs);
  throw error;
} finally {
  await stop();
  await new Promise((resolve) => server.close(resolve));
  await new Promise((resolve) => sourceServer.close(resolve));
  rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
