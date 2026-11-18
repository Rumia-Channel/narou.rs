// Isolated behavioral smoke: actual Worker/Wasm, local D1, S3 HTTP fixture.
// Build first with worker-build --release, then: node tests/content_storage.mjs
import assert from 'node:assert/strict';
import http from 'node:http';
import { createHash } from 'node:crypto';
import { brotliDecompressSync, inflateRawSync } from 'node:zlib';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';
import { createS3MultipartFixture } from './s3_multipart_fixture.mjs';
import { animationFixture, verifyPng, isAnimation } from './animation_fixture.mjs';
import { transferClient } from './transfer.mjs';
import { renderAcquisitionTopology } from './acquisition_topology.mjs';
import { localWorkerEnvironment, wranglerPackage } from './read_worker_fixture.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const windows = process.platform === 'win32';
const npx = windows ? 'npx.cmd' : 'npx';
mkdirSync(join(root, '.wrangler'), { recursive: true });
const work = mkdtempSync(join(root, '.wrangler', 'content-smoke-'));
let config = join(work, 'wrangler.toml');
let topology;
let persist = join(work, 'state');
const token = 'isolated-content-smoke-token';
const objects = new Map();
let failPut = false;
const multipart = createS3MultipartFixture({ objects, failPut: () => failPut });
let sourceGeneration = 1;
let sourceSubtitle = '第一話';
let sourceImageCount = 0;
let sourceAnimation = null;
let sourceStaticImage = null;
let sourceEpisodeCount = 1;
let sourcePause = null;
let sourceBodyDelay = 0;
let sourceTimings = null;
const imageRequests = new Map();
const sourceRequests = new Map();
const imageContinuations = [];
let yieldBarrier = null;
// Read-only witness barrier: released only once the armed S3 gate really
// holds an upstream request. The test caller waits on it instead of sleeping.
let upstreamHeld = null;
// The held state is observed before the caller may cancel, so the test can
// check the admission while the upstream is genuinely held.
let heldRelease = null;
let deliveryWitness = null;
let canonicalWriteBarrier = null;
// The armed canonical write is shared by every script of the topology (the
// acquisition runs in a lane isolate, the arm arrives at the writer), so the
// fixture sink keeps it instead of a script-local module variable.
let canonicalArmed = null;
const longEpisodeCount = 1685;
const longEpisodeRequests = new Map();
const longInvocationStarts = [];
const longEpisodeBody = (index) => `保存された${String.fromCodePoint(0x6000 + index)}の本文です。`;
const imagePng = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO3Z5A0AAAAASUVORK5CYII=', 'base64');
let child;
let logs = '';
let port;
let s3Port = 0;
const memorySamples = [];
const memoryObservers = new Set();
const xml = (s) => String(s).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
const etag = (bytes) => `"${createHash('md5').update(bytes).digest('hex')}"`;
const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url, 'http://127.0.0.1');
    const key = decodeURIComponent(url.pathname);
    if (key === '/_fixture/memory') {
      if (req.method === 'POST') {
        const chunks = [];
        for await (const chunk of req) chunks.push(chunk);
        const sample = JSON.parse(Buffer.concat(chunks).toString());
        memorySamples.push(sample);
        if (sample.role === 'acquisition') {
          for (const release of memoryObservers) release();
          memoryObservers.clear();
        }
        res.writeHead(204); res.end(); return;
      }
      if (!memorySamples.some(sample => sample.role === 'acquisition')) {
        await new Promise((resolve, reject) => {
          const timer = setTimeout(() => {
            memoryObservers.delete(release);
            reject(new Error('acquisition memory observation missing'));
          }, 60000);
          const release = () => { clearTimeout(timer); resolve(); };
          memoryObservers.add(release);
        });
      }
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({
        wasmBytes: Math.max(0, ...memorySamples.map(sample => sample.wasmBytes)),
        highWater: Math.max(0, ...memorySamples.map(sample => sample.highWater)),
        instances: Math.max(0, ...memorySamples.map(sample => sample.instances)),
        samples: memorySamples.length,
      }));
      return;
    }
    if (req.method === 'GET' && key === '/_fixture/upstream-held') {
      // Read-only witness. The answer is released only after the armed gate
      // really holds an upstream request, so the caller never guesses when the
      // receiver is pending, and only after the test has observed the held
      // state. One answer per arming.
      const gate = upstreamHeld;
      if (!gate || gate.observed) { res.writeHead(409); res.end('no armed upstream gate'); return; }
      gate.observed = true;
      // The producer callbacks must retain their shared handles until resolved.
      await gate.promise;
      const release = heldRelease;
      if (release) await release.promise;
      if (upstreamHeld === gate) upstreamHeld = null;
      res.end('held');
      return;
    }
    if (req.method === 'POST' && key === '/_fixture/release-held') {
      // The test lets the caller cancel now that the held state is observed.
      heldRelease?.resolve();
      heldRelease = null;
      res.end('released');
      return;
    }
    if (req.method === 'POST' && key === '/_fixture/yield') {
      const gate = yieldBarrier;
      if (gate) {
        gate.entered.resolve();
        await gate.release.promise;
      }
      res.end('released'); return;
    }
    if (req.method === 'POST' && key === '/_fixture/delivery') {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const delivery = JSON.parse(Buffer.concat(chunks).toString());
      // A canonical-only witness ignores the deliveries that did not publish
      // canonical bytes; the expected count and the captured envelopes are
      // shared by both contracts.
      if (deliveryWitness && (!deliveryWitness.canonicalOnly || delivery.canonical)) {
        deliveryWitness.ids.push(...delivery.ids);
        if (deliveryWitness.messages) deliveryWitness.messages.push(...(delivery.messages ?? []));
        deliveryWitness.returned?.resolve();
        if (deliveryWitness.ids.length >= (deliveryWitness.expected ?? 2)) deliveryWitness.completed.resolve();
      }
      res.end('recorded'); return;
    }
    if (key === '/_fixture/canonical-arm') {
      if (req.method === 'POST') {
        const chunks = [];
        for await (const chunk of req) chunks.push(chunk);
        canonicalArmed = JSON.parse(Buffer.concat(chunks).toString());
        res.end('armed'); return;
      }
      if (req.method === 'DELETE') {
        canonicalArmed = null;
        res.end('cleared'); return;
      }
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify(canonicalArmed));
      return;
    }
    if (req.method === 'POST' && key === '/_fixture/canonical-boundary') {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      // The armed boundary fires exactly once.
      canonicalArmed = null;
      const gate = canonicalWriteBarrier;
      if (gate) {
        gate.entered.resolve(JSON.parse(Buffer.concat(chunks).toString()));
        await gate.release.promise;
      }
      res.end('released'); return;
    }
    if (req.method === 'POST' && key === '/_fixture/canonical-result') {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      if (canonicalWriteBarrier) canonicalWriteBarrier.result.resolve(JSON.parse(Buffer.concat(chunks).toString()));
      res.end('recorded'); return;
    }
    if (sourceTimings && key.startsWith('/smoke-novel/')) {
      const timing = { path: key, started: performance.now(), finished: null };
      sourceTimings.push(timing);
      res.once('finish', () => { timing.finished = performance.now(); });
    }
    if (req.method === 'GET' && key === '/smoke-long/') {
      const page = Number(url.searchParams.get('p') ?? 1);
      const pageCount = Math.ceil(longEpisodeCount / 100);
      assert(page >= 1 && page <= pageCount);
      if (page === 1) longInvocationStarts.push(longEpisodeRequests.size);
      const links = Array.from({ length: Math.min(100, longEpisodeCount - (page - 1) * 100) },
        (_, offset) => (page - 1) * 100 + offset + 1)
        .map((index) => `<a href="/smoke-long/${index}/">第${index}話</a>`).join('');
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(`<h1>Long Story</h1><span class="author">Storage Author</span><pages>${pageCount}</pages>${links}${page < pageCount ? `<next>/smoke-long/?p=${page + 1}</next>` : ''}`);
      return;
    }
    const longEpisode = /^\/smoke-long\/(\d+)\/$/.exec(key);
    if (req.method === 'GET' && longEpisode) {
      const index = Number(longEpisode[1]);
      longEpisodeRequests.set(index, (longEpisodeRequests.get(index) ?? 0) + 1);
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(`<div class="body"><p>${longEpisodeBody(index)}</p></div>`);
      return;
    }
    if (req.method === 'GET' && /^\/smoke-(novel|single|series)\/$/.test(key)) {
      sourceRequests.set(key, (sourceRequests.get(key) ?? 0) + 1);
      if (sourcePause) await sourcePause(key);
      if (sourceImageCount && imageRequests.size > 0) {
        const checkpoint = scalar("SELECT checkpoint_json FROM worker_jobs WHERE kind='download' AND status='running'");
        imageContinuations.push({ fetched: imageRequests.size, checkpoint: JSON.parse(checkpoint) });
      }
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      const links = Array.from({ length: sourceEpisodeCount }, (_, i) => `<a href="${key}${i + 1}/">${sourceEpisodeCount === 1 ? sourceSubtitle : `第${i + 1}話`}</a>`).join('');
      res.end(`<h1>Controlled Story</h1><span class="author">Storage Author</span>${links}`);
      return;
    }
    if (req.method === 'GET' && /^\/smoke-(novel|single|series)\/\d+\/$/.test(key)) {
      sourceRequests.set(key, (sourceRequests.get(key) ?? 0) + 1);
      if (sourcePause) await sourcePause(key);
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      if (sourceBodyDelay) {
        res.flushHeaders();
        await new Promise((resolve) => setTimeout(resolve, sourceBodyDelay));
      }
      const images = Array.from({ length: sourceImageCount }, (_, i) => `<img src="http://localhost:${server.address().port}/smoke-image/${i}.png" />`).join('');
      const animation = sourceAnimation ? `<img src="http://localhost:${server.address().port}/smoke-animation/frames.zip?ugoira=${sourceAnimation.delays.join(',')}" />` : '';
      const largeImage = sourceStaticImage ? `<img src="http://localhost:${server.address().port}/smoke-large.png" />` : '';
      res.end(`<div class="body"><p>${(sourceGeneration === 1 ? '初回取得の本文です。' : sourceGeneration === 2 ? '更新後の本文です。' : '失敗する更新の本文です。').repeat(100)}</p>${images}${animation}${largeImage}</div>`);
      return;
    }
    if (req.method === 'GET' && /^\/smoke-image\/\d+\.png$/.test(key)) {
      imageRequests.set(key, (imageRequests.get(key) ?? 0) + 1);
      if (sourcePause) await sourcePause(key);
      res.writeHead(200, { 'Content-Type': 'image/png' });
      // Distinct stored bytes, with an unchanged valid one-pixel PNG.
      res.end(Buffer.concat([imagePng, Buffer.from(key)]));
      return;
    }
    if (req.method === 'GET' && key === '/smoke-animation/frames.zip' && sourceAnimation) {
      res.writeHead(200, { 'content-type': 'application/zip', 'content-length': sourceAnimation.bytes.length });
      res.end(sourceAnimation.bytes); return;
    }
    if (req.method === 'GET' && key === '/smoke-large.png' && sourceStaticImage) {
      res.writeHead(200, { 'content-type': 'image/png', 'content-length': sourceStaticImage.bytes.length });
      res.end(sourceStaticImage.bytes); return;
    }
    if (await multipart.handle(req, res, url, key)) return;
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
        if (!multipart.checkConditions(req, res, key)) return;
        objects.set(key, Buffer.from(bytes));
        multipart.etags.delete(key);
        res.setHeader('content-type', 'application/xml');
        res.end(`<CopyObjectResult><LastModified>2026-10-15T00:00:00Z</LastModified><ETag>${xml(etag(bytes))}</ETag></CopyObjectResult>`);
        return;
      }
      const chunks = [];
      let length = 0;
      for await (const chunk of req) { length += chunk.length; assert(length <= 16 * 1024 * 1024); chunks.push(chunk); }
      const bytes = Buffer.concat(chunks);
      if (!multipart.checkConditions(req, res, key)) return;
      multipart.etags.delete(key);
      objects.set(key, bytes); res.setHeader('etag', etag(bytes)); res.end(); return;
    }
    if (req.method === 'DELETE') {
      if (!multipart.checkConditions(req, res, key)) return;
      objects.delete(key); multipart.etags.delete(key); res.writeHead(204); res.end(); return;
    }
    res.writeHead(404); res.end('<Error><Code>NoSuchKey</Code></Error>');
  } catch (error) { res.writeHead(500); res.end(String(error)); }
});
const sourceServer = http.createServer(server.listeners('request')[0]);

function run(args, options = {}) {
  const result = spawnSync(npx, ['--yes', wranglerPackage, ...args], { cwd: root, shell: windows, encoding: 'utf8', env: localWorkerEnvironment, ...options });
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
/**
 * writer + 取得レーン 4 本の設定を描画する。
 *
 * 取得は writer では実行しない。writer は入口 (Web/API/cron/変換/旧入口)、
 * 取得専用 4 本が自分の取得キューだけを実行する。
 */
function renderTopology({ required = false, entrypoint, invalidEndpoint = false } = {}) {
  topology = renderAcquisitionTopology({
    work,
    target: 'local',
    main: (entrypoint ?? join(root, 'build/worker/shim.mjs')).replaceAll('\\', '/'),
    s3Endpoint: invalidEndpoint ? 'not-a-url' : `http://127.0.0.1:${s3Port}`,
    vars: { NAROU_REQUIRE_S3: String(required) },
  });
  config = topology.writerPath;
  // The test caller dispatches to the actual writer through a real service
  // binding (the workerd service-dispatch cancellation model), so the writer
  // binds to itself. The binding is generated here, next to the topology it
  // belongs to; nothing in the shared topology helper has to change.
  const writerConfig = readFileSync(topology.writerPath, 'utf8');
  if (!writerConfig.includes('binding = "SERVER"')) {
    writeFileSync(topology.writerPath, `${writerConfig}\n[[services]]\nbinding = "SERVER"\nservice = ${JSON.stringify(topology.writerName)}\n`);
  }
}

async function start() {
  logs = '';
  child = spawn(npx, ['--yes', wranglerPackage, 'dev', '--test-scheduled', ...topology.configs.flatMap((path) => ['--config', path]), '--persist-to', persist, '--port', String(port)], { cwd: root, shell: windows, detached: !windows, stdio: ['ignore', 'pipe', 'pipe'], env: localWorkerEnvironment });
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
  return waitForAcquisition(accepted.results[0].job_id, expectedFailure);
}
async function waitForJob(jobId, expectedFailure = false, timeoutMs = 120000) {
  const deadline = Date.now() + timeoutMs;
  let job;
  do {
    job = await (await api(`/api/jobs/${jobId}`)).json();
    if (['succeeded', 'permanent', 'blocked'].includes(job.status) || (expectedFailure && job.status === 'retryable')) break;
    await new Promise((resolve) => setTimeout(resolve, 500));
  } while (Date.now() < deadline);
  assert.equal(job.status, expectedFailure ? 'retryable' : 'succeeded', JSON.stringify(job));
  return job;
}
async function waitForAcquisition(jobId, expectedFailure = false, timeoutMs = 120000) {
  const job = await waitForJob(jobId, expectedFailure, timeoutMs);
  if (!expectedFailure) {
    // Acquisition success only promises that its follow-up Convert was enqueued.
    // Drain that lane before changing the source, injecting faults, or restarting.
    // Do not open a second workerd against the live SQLite file while Convert
    // is writing it. The HTTP snapshot observes the same runtime without a
    // competing local D1 session; keep the terminal ledger assertion below.
    const conversionDeadline = Date.now() + timeoutMs;
    let queue;
    do {
      const response = await api('/api/queue/status');
      assert.equal(response.status, 200, await response.clone().text());
      queue = await response.json();
      if (queue.lane_sizes[1] === 0) break;
      await new Promise((resolve) => setTimeout(resolve, 300));
    } while (Date.now() < conversionDeadline);
    assert.equal(queue.lane_sizes[1], 0, JSON.stringify(queue));
    const conversions = sql("SELECT job_id,status FROM worker_jobs WHERE kind='convert'")[0].results;
    assert(conversions.every((row) => row.status === 'succeeded'), JSON.stringify(conversions));
  }
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
function epubText(bytes, visitEntry) {
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
    if (name.endsWith('.xhtml') || visitEntry) {
      const decoded = bytes.readUInt16LE(cursor + 10) === 8 ? inflateRawSync(data) : data;
      if (name.endsWith('.xhtml')) texts.push(decoded.toString());
      if (visitEntry) visitEntry(name, decoded);
    }
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
  s3Port = server.address().port;
  sourceServer.listen(s3Port, '::1'); await once(sourceServer, 'listening');
  const portProbe = http.createServer(); portProbe.listen(0, '127.0.0.1'); await once(portProbe, 'listening');
  port = portProbe.address().port; await new Promise((resolve) => portProbe.close(resolve));
  // 取得は writer 1 本 + 取得専用 4 本で実行する (writer は実行しない)。
  renderTopology({});
  writeFileSync(join(work, '.dev.vars'), `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32, 7).toString('base64')}\nS3_ACCESS_KEY_ID=local-test-key\nS3_SECRET_ACCESS_KEY=local-test-secret\n`);
  run(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
  const prefix = 'novels/Storage Smoke/本文/本文';
  const subtitle = (index, name) => ({ index: String(index), href: `https://example.com/n1234ab/${index}/`, chapter: '', subchapter: '', subtitle: name, file_subtitle: name, subdate: '2026-10-15' });
  const first = subtitle(1, '第一話'); const second = subtitle(2, '第二話');
  // narou.rb archives can have download_time only in the TOC. Keep the
  // body objects unstamped and require the existing EPUB checks to succeed.
  const legacySubtitles = [first, second].map((sub) => ({ ...sub, download_time: '2021-01-24 20:20:55.807706000 +09:00' }));
  const section = (sub, body) => ({ ...sub, element: { data_type: 'html', introduction: '', body: `<p>${body.repeat(100)}</p>`, postscript: '' } });
  const firstKey = `${prefix}/本文/1 第一話.yaml`;
  const legacyFixtureSql = `INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,ncode,ncode_fold,general_all_no,length,use_subdirectory) VALUES (1,'Author','author','本文保存検証','本文保存検証','本文','https://example.com/n1234ab/','https://example.com/n1234ab/','Storage Smoke','storage smoke','2026-10-15T00:00:00Z','n1234ab','n1234ab',2,1000,1); UPDATE novel_id_sequence SET next_id=2;`
    + objectInsert(`${prefix}/toc.yaml`, { title: '本文保存検証', author: 'Author', toc_url: 'https://example.com/n1234ab/', story: '', subtitles: legacySubtitles, novel_type: 1 })
    + objectInsert(firstKey, section(first, '初版本文がここにあります。'))
    + objectInsert(`${prefix}/本文/2 第二話.yaml`, section(second, '第二話の本文です。'))
    + objectInsert(`${prefix}/本文/cache/old/1 第一話.yaml`, section(first, '変更前の旧版本文。'))
    + objectInsert(`${prefix}/本文/.cache/legacy/1 点付き旧話.yaml`, section(subtitle(1, '点付き旧話'), '点付きキャッシュの旧本文。'))
    + objectInsert(`${prefix}/setting.ini`, '') + objectInsert(`${prefix}/replace.txt`, '')
    + objectInsert(`${prefix}/converter.yaml`, {}) + objectInsert(`${prefix}/.illustration_cache.yaml`, {});
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','\"d1\"','d1');" + legacyFixtureSql);
  await start();
  assert.equal((await fetch(`http://127.0.0.1:${port}/api/admin/content-migration`, { headers: { Connection: 'close' } })).status, 401);
  await migrationCli();
  const plan = await phase('plan'); assert.equal(plan.planned, 4); assert.equal(plan.backend, 'd1');
  await epub();
  const conversion = await (await api('/api/convert', { targets: ['1'] })).json();
  assert.equal(conversion.success, true, JSON.stringify(conversion));
  await waitForJob(conversion.results[0].job_id);
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
  const multiTargetDefinition = definition
    .replace('(?<ncode>smoke-novel)', '(?<ncode>smoke-(?:novel|single|series))')
    .replace('(?<href>/smoke-novel/', '(?<href>/smoke-(?:novel|single|series)/')
    .replace(`toc_url: '${target}'`, "toc_url: '\\k<top_url>/\\k<ncode>/'");
  const savedSite = await (await api('/api/sites/localhost.yaml', definition, 'PUT')).json();
  assert(savedSite.success, JSON.stringify(savedSite));
  let originalEpisode;
  for (const force of [false, true]) {
    sourceGeneration = force ? 2 : 1;
    await download(target, force);
    assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0, 'downloads must not duplicate body blobs into D1');
    const current = sql("SELECT * FROM content_objects WHERE object_key LIKE '%/smoke-novel%/本文/%' AND instr(object_key,'/cache/')=0")[0].results;
    assert.equal(current.length, 1, 'ordinary downloads must publish exactly one current content reference');
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
  // EPUB preparation now stages text/XHTML instead of retaining a whole book.
  // A continuing S3 write outage must fail closed, without discarding the old
  // episode; once writes recover the exact previous content remains readable.
  const unavailableEpub = await api(`/api/novels/${downloadedId}/download.epub`);
  assert.equal(unavailableEpub.status, 500, await unavailableEpub.text());
  failPut = false;
  const unchanged = await api(`/api/novels/${downloadedId}/download.epub`);
  assert.equal(unchanged.status, 200, unchanged.status === 200 ? '' : await unchanged.text());
  assert(epubText(Buffer.from(await unchanged.arrayBuffer())).includes('更新後の本文です。'));
  sourceGeneration = 2;
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

  async function defaultLibrary(name, { required = false, marker, legacy = false, invalidEndpoint = false, entrypoint } = {}) {
    await stop();
    memorySamples.length = 0;
    persist = join(work, `default-${name}`);
    renderTopology({ required, entrypoint, invalidEndpoint });
    run(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
    if (marker !== undefined) sql(`INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend',${quote(JSON.stringify(marker))},${quote(marker)});`);
    if (legacy) sql(legacyFixtureSql);
    failPut = false; sourceGeneration = 1; sourceSubtitle = '第一話';
    sourceEpisodeCount = 1; sourceImageCount = 0; sourcePause = null;
    sourceBodyDelay = 0; sourceTimings = null;
    await start();
  }
  async function installDefaultSource(siteDefinition = definition) {
    const result = await (await api('/api/sites/localhost.yaml', siteDefinition, 'PUT')).json();
    assert(result.success, JSON.stringify(result));
  }
  async function verifyDefaultDownload() {
    await installDefaultSource();
    await download(target, false);
    const ref = sql("SELECT object_key,blob_key,encoding,sha256 FROM content_objects WHERE instr(object_key,'/smoke-novel')>0 AND instr(object_key,'/cache/')=0")[0].results[0];
    const stored = objects.get(`/narou-local/narou/local/${ref.blob_key}`);
    const bytes = ref.encoding === 'brotli' ? brotliDecompressSync(stored) : stored;
    assert.equal(createHash('sha256').update(bytes).digest('hex'), ref.sha256);
    assert(bytes.toString('utf8').includes('初回取得の本文です。'.repeat(100)), 'new default must retain the actual episode in S3');
    const id = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
    const response = await api(`/api/novels/${id}/download.epub`);
    assert.equal(response.status, 200, await response.clone().text());
    assert(epubText(Buffer.from(await response.arrayBuffer())).includes('初回取得の本文です。'));
    return id;
  }

  await defaultLibrary('completion-pacing');
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0.4','0.4'),('local','download.wait-steps','0','0');");
  sourceEpisodeCount = 3; sourceBodyDelay = 650; sourceTimings = [];
  await installDefaultSource();
  await download(target, false);
  const pacedEpisodes = sourceTimings.filter((timing) => /^\/smoke-novel\/\d+\/$/.test(timing.path));
  assert.deepEqual(pacedEpisodes.map((timing) => timing.path), ['/smoke-novel/1/', '/smoke-novel/2/', '/smoke-novel/3/']);
  for (let index = 1; index < pacedEpisodes.length; index++) {
    const previous = pacedEpisodes[index - 1], current = pacedEpisodes[index];
    assert.notEqual(previous.finished, null);
    const gap = current.started - previous.finished;
    assert(gap >= 395, `Worker completion-to-next-start gap was ${gap} ms: ${previous.path} -> ${current.path}`);
    console.log(`PASS Worker response completion -> next request: ${gap.toFixed(1)} ms (configured 400 ms, body 650 ms)`);
  }

  await defaultLibrary('fresh');
  const freshPlan = await phase('plan');
  assert.equal(freshPlan.remaining_legacy, 0);
  assert.equal(scalar("SELECT COUNT(*) FROM app_state WHERE scope='inv' AND key='content_backend'"), 0, 'plan must not initialize a backend');
  const freshId = await verifyDefaultDownload();
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0);
  assert.equal((await (await api('/api/storage/mode')).json()).content_backend, 's3');
  await stop(); await start();
  const restarted = await api(`/api/novels/${freshId}/download.epub`);
  assert.equal(restarted.status, 200, await restarted.clone().text());
  assert(epubText(Buffer.from(await restarted.arrayBuffer())).includes('初回取得の本文です。'));
  console.log('PASS fresh uninitialized libraries store episodes in S3 and remain readable after restart');

  await defaultLibrary('legacy', { legacy: true });
  const legacyPlan = await phase('plan');
  assert.equal(legacyPlan.backend, 'd1');
  assert.equal(scalar("SELECT COUNT(*) FROM app_state WHERE scope='inv' AND key='content_backend'"), 0, 'legacy plan must be read-only');
  await epub();
  assert.equal((await (await api('/api/storage/mode')).json()).content_backend, 'migrating');
  await verifyDefaultDownload();
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 4, 'automatic initialization must not delete uncopied old bodies/history');
  await epub();
  console.log('PASS uninitialized legacy libraries retain old EPUB/history while all new bodies go to S3');

  await complete('copy');
  const partialVerification = await phase('verify');
  assert.equal(partialVerification.verified, 1);
  assert.equal(partialVerification.verify_done, false);
  await migrationCli(true);
  const resumedMigration = await (await api('/api/admin/content-migration')).json();
  assert.equal(resumedMigration.verified, scalar('SELECT COUNT(*) FROM content_objects'), 'resuming the CLI must continue verification without recounting completed objects');
  assert.equal(resumedMigration.planned, legacyPlan.planned, 'automatic migrating initialization must retain the legacy inventory count');
  assert.equal(resumedMigration.planned_bytes, legacyPlan.planned_bytes);
  assert.equal(resumedMigration.remaining_legacy, 0);
  await epub();
  console.log('PASS maintenance CLI resumes partial verification without rewinding completed work');

  await defaultLibrary('required', { required: true, marker: 'd1' });
  await verifyDefaultDownload();
  const requiredMode = await (await api('/api/storage/mode')).json();
  assert.equal(requiredMode.content_backend, 's3');
  assert.equal(requiredMode.illustration_backend, 's3');
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0);
  console.log('PASS required-S3 deployments promote an explicit d1 marker instead of continuing D1 writes');

  await defaultLibrary('invalid-config', { invalidEndpoint: true });
  const unavailable = await api('/api/storage/mode');
  assert.equal(unavailable.status, 503);
  assert.equal((await unavailable.json()).error.code, 'storage_configuration_unavailable');
  const sort = await api('/api/sort_state', { column: 1, dir: 'desc' });
  assert.equal(sort.status, 200);
  assert.equal((await sort.json()).success, true);
  const sorted = await api('/api/sort_state');
  assert.equal(sorted.status, 200);
  const { column, dir } = await sorted.json();
  assert.deepEqual({ column, dir }, { column: 1, dir: 'desc' });
  assert.equal(scalar("SELECT COUNT(*) FROM app_state WHERE scope='inv' AND key='content_backend'"), 0, 'invalid S3 configuration must not mutate the marker');
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0);
  console.log('PASS invalid S3 configuration fails closed without initializing a misleading backend');

  await defaultLibrary('put-failure');
  await installDefaultSource();
  failPut = true;
  await download(target, false, true);
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0, 'failed S3 PUT must not store the episode in D1');
  assert.equal(scalar('SELECT COUNT(*) FROM content_objects'), 0, 'failed S3 PUT cannot publish a reference');
  assert.equal(JSON.parse(scalar("SELECT value_json FROM app_state WHERE scope='inv' AND key='content_backend'")), 's3');
  failPut = false;
  const cleared = await (await api('/api/queue/cancel', {})).json();
  assert(cleared.success, JSON.stringify(cleared));
  await verifyDefaultDownload();
  assert.equal(scalar('SELECT COUNT(*) FROM legacy_content_objects'), 0);
  console.log('PASS failed initial PUT never falls back to D1 and the same download recovers on S3');

  // One episode exceeds the real per-invocation subrequest budget. The Queue
  // must continue its images, not redownload the saved prefix on every attempt.
  await defaultLibrary('image-resume', { required: true });
  await installDefaultSource();
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
  sourceImageCount = 500;
  imageContinuations.length = 0;
  await download(target, false);
  assert.equal(imageRequests.size, sourceImageCount);
  assert([...imageRequests.values()].every((count) => count === 1), 'continuations must not fetch any saved image twice');
  const imageJob = sql("SELECT checkpoint_json,attempts,status FROM worker_jobs WHERE kind='download'")[0].results[0];
  assert.equal(imageJob.status, 'succeeded');
  assert.equal(imageJob.attempts, 0, 'budget continuations must not consume error retries');
  assert(imageContinuations.some(({ fetched, checkpoint }) => fetched > 0 && fetched < sourceImageCount && checkpoint?.next_section_index === 0),
    'the real queue must resume the same episode after persisting a mid-image-loop checkpoint');
  const creationCheckpoints = imageContinuations.filter(({ fetched }) => fetched > 0 && fetched < sourceImageCount).map(({ checkpoint }) => checkpoint);
  assert(creationCheckpoints.length > 0, 'a genuine new-download budget continuation must be witnessed');
  const reservedCreation = creationCheckpoints[0].creation_reservation;
  assert(Array.isArray(reservedCreation) && reservedCreation.length === 2 && reservedCreation.every(Number.isSafeInteger), 'the original reserved numeric ID and generation must be durable before continuation');
  const creationOrigin = creationCheckpoints[0].write_origin;
  assert(creationOrigin && typeof creationOrigin.prefix === 'string' && Number.isSafeInteger(creationOrigin.epoch), 'the original namespace epoch must be durable with the reservation');
  for (const checkpoint of creationCheckpoints) {
    assert.deepEqual(checkpoint.creation_reservation, reservedCreation, 'a new download must retain the SAME ID and generation across real yields, never release/reallocate');
    assert.deepEqual(checkpoint.write_origin, creationOrigin, 'the reservation and original prefix epoch must travel together');
  }
  const imageIndex = sql("SELECT encoding,hex(data) payload FROM objects WHERE object_key LIKE '%/smoke-novel/.illustration_cache.yaml'")[0].results[0];
  assert(imageIndex, 'image source mapping must survive the continuation');
  const encodedIndex = Buffer.from(imageIndex.payload, 'hex');
  const indexText = (imageIndex.encoding === 'brotli' ? brotliDecompressSync(encodedIndex) : encodedIndex).toString();
  for (let i = 0; i < sourceImageCount; i++) {
    assert(indexText.includes(`/smoke-image/${i}.png`), `image ${i} must be recorded in the persisted index`);
    const image = Buffer.concat([imagePng, Buffer.from(`/smoke-image/${i}.png`)]);
    const digest = createHash('sha256').update(image).digest();
    const imageKey = `/narou-local/narou/local/illustrations/${digest.toString('base64url')}.png`;
    if (!objects.has(imageKey)) {
      const exactKeys = [...objects].filter(([, bytes]) => bytes.equals(image)).map(([key]) => key).slice(0, 3);
      const poolKeys = [...objects.keys()].filter((key) => key.includes('/illustrations/')).slice(0, 3);
      assert.fail(`Missing shared image ${imageKey}; exact payload keys=${JSON.stringify(exactKeys)}; pool sample=${JSON.stringify(poolKeys)}; index head=${indexText.slice(0, 600)}`);
    }
    assert.deepEqual(objects.get(imageKey), image);
  }
  const illustratedId = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
  assert.equal(illustratedId, reservedCreation[0], 'the completed real record must adopt its original reserved ID');
  const illustratedEpub = await api(`/api/novels/${illustratedId}/download.epub`);
  assert.equal(illustratedEpub.status, 200, await illustratedEpub.clone().text());
  const illustratedText = epubText(Buffer.from(await illustratedEpub.arrayBuffer()));
  assert.equal((illustratedText.match(/<img\b/g) ?? []).length, sourceImageCount, 'EPUB must retain every image after the continuation');
  assert(!illustratedText.includes('/smoke-image/'), 'EPUB must use its embedded images, not upstream URLs');
  console.log('PASS one 500-image episode yields on the real Worker budget, resumes without refetching, and produces an illustrated EPUB');

  {
    const memoryWrapper = join(work, 'memory-wrapper.mjs');
    const modulePath = JSON.stringify(relative(work, join(root, 'build/worker/shim.mjs')).replaceAll('\\', '/'));
    writeFileSync(memoryWrapper, `
const OriginalInstance = WebAssembly.Instance;
let memory, highWater = 0, instances = 0;
const sample = () => {
  highWater = Math.max(highWater, memory?.buffer.byteLength ?? 0);
  return { wasmBytes: memory?.buffer.byteLength ?? 0, highWater, instances };
};
WebAssembly.Instance = class extends OriginalInstance {
  constructor(...args) {
    super(...args);
    sample(); memory = this.exports.memory; instances++;
  }
};
const Production = await import(${modulePath});
export const PushHub = Production.PushHub;
export const SiteRateLimiter = Production.SiteRateLimiter;
export default class extends Production.default {
  async reportMemory() {
    const response = await fetch('http://127.0.0.1:${s3Port}/_fixture/memory', {
      method: 'POST', body: JSON.stringify({ ...sample(), role: this.env.NAROU_WORKER_ROLE }),
    });
    if (!response.ok) throw new Error('memory observation failed');
    await response.text();
  }
  async fetch(request, ...rest) {
    const path = new URL(request.url).pathname;
    if (path === '/_fixture/memory') {
      await this.reportMemory();
      return fetch('http://127.0.0.1:${s3Port}/_fixture/memory');
    }
    if (path === '/_fixture/dispatch-abort') return this.dispatchAbort(request);
    if (path === '/_fixture/dispatch-body') return this.dispatchBody(request);
    return super.fetch(request, ...rest);
  }
  async queue(batch, ...rest) {
    await super.queue(batch, ...rest);
    await this.reportMemory();
  }
  // Test-only caller. It owns the AbortController and cancels a real
  // service-binding dispatch to the actual writer. The receiver sees its own
  // request.signal fire and must abort the pending upstream read; nothing
  // here fabricates a signal for the application.
  async dispatchAbort(request) {
    if (!this.env.SERVER) return new Response('caller binding missing', 501);
    const target = new URL(request.url).searchParams.get('target');
    const controller = new AbortController();
    const dispatch = this.env.SERVER.fetch(target, {
      method: 'GET',
      headers: { Authorization: 'Bearer ' + this.env.NAROU_ADMIN_TOKEN },
      signal: controller.signal,
    }).then(response => ({ status: response.status }), error => ({ name: error.name }));
    try {
      // Read-only witness: the S3 fixture answers only once the actual
      // upstream request is held, so the cancel never races the receiver.
      await (await fetch('http://127.0.0.1:${s3Port}/_fixture/upstream-held')).text();
      controller.abort();
      return Response.json(await dispatch);
    } finally {
      controller.abort();
    }
  }
  async dispatchBody(request) {
    if (!this.env.SERVER) return new Response('caller binding missing', 501);
    const target = new URL(request.url).searchParams.get('target');
    const controller = new AbortController();
    const dispatch = this.env.SERVER.fetch(target, {
      method: 'GET',
      headers: { Authorization: 'Bearer ' + this.env.NAROU_ADMIN_TOKEN },
      signal: controller.signal,
    });
    const held = fetch('http://127.0.0.1:${s3Port}/_fixture/upstream-held').then(response => response.text());
    try {
      const response = await dispatch;
      const reader = response.body.getReader();
      let bytes = 0;
      while (bytes < 4 * 1024 * 1024) {
        const { value, done } = await reader.read();
        if (done) break;
        bytes += value.length;
      }
      await held;
      controller.abort();
      let name = null;
      try { await reader.read(); } catch (error) { name = error.name; }
      await reader.cancel().catch(() => {});
      return Response.json({ status: response.status, read: bytes, name });
    } finally {
      controller.abort();
    }
  }
}
`);
    await defaultLibrary('animation-stream', { required: true, entrypoint: memoryWrapper });
    await installDefaultSource(multiTargetDefinition);
    sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
    sourceImageCount = 0;
    const animation = animationFixture();
    assert(animation.bytes.length < 16 * 1024 * 1024, 'source fixture stays within the existing upstream ZIP limit');
    sourceAnimation = animation;
    const completedBefore = multipart.completed.length;
    await download(target, false, false);
    const animationId = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
    const saved = [...objects].filter(([key, bytes]) => key.includes('/illustrations/') && isAnimation(bytes));
    assert.equal(saved.length, 1, 'one source animation must have one published pooled asset');
    const [animationKey, animationBytes] = saved[0];
    assert(animationBytes.length > 16 * 1024 * 1024, 'fixture must exercise the formerly unsupported large-asset path');
    verifyPng(animationBytes, animation);
    const digest = createHash('sha256').update(animationBytes).digest('base64url');
    assert(animationKey.endsWith(`/illustrations/${digest}.png`), 'pool name must hash the exact complete animation');
    assert.equal(multipart.uploads.size, 0, 'completed downloads must not leave unfinished multipart uploads');
    for (const upload of multipart.completed.slice(completedBefore)) {
      // Temporary namespaces must never retain a completed object after
      // publication. Canonical immutable targets (the illustration pool) are
      // legal conditional-complete destinations and stay put.
      const temporary = upload.key.includes('/.narou-staging/') || upload.key.includes('/.narou-transfer/');
      assert(!temporary || !objects.has(upload.key), `temporary uploaded object ${upload.key} must be removed after publication`);
    }

    const exportedAnimation = await transferClient(`http://127.0.0.1:${port}`, token).exportData([animationId]);
    const animationHash = createHash('sha256').update(animationBytes).digest('hex');
    const imagePath = `挿絵/${animationHash}.png`;
    const archivedAnimation = exportedAnimation.files.get(`novels/0/${imagePath}`);
    assert(archivedAnimation, 'a pool-only illustration must be included even when novel-prefix LIST is empty');
    assert.deepEqual(archivedAnimation, animationBytes, 'export retains every original animation byte');
    assert.deepEqual(exportedAnimation.manifest.novels[0].files.find(file => file.path === imagePath), {
      path: imagePath, size: animationBytes.length, sha256: animationHash,
    });
    console.log('PASS v2 export resolves pool-only animation with exact bytes, size and SHA-256');

    const response = await api(`/api/novels/${animationId}/download.epub`);
    assert.equal(response.status, 200, response.status === 200 ? undefined : await response.text());
    const readerPrefix = scalar(`SELECT prefix FROM novel_gate_bindings WHERE novel_id=${animationId}`);
    const liveReaders = () => scalar(`SELECT COUNT(*) FROM novel_reader_leases WHERE prefix=${quote(readerPrefix)} AND expires_at>unixepoch()`);
    assert(liveReaders() > 0, 'the real EPUB response must own its durable novel lease before body consumption');
    const busy = await api(`/api/novels/${animationId}/download.epub`);
    assert.equal(busy.status, 503, 'the in-flight EPUB must retain its isolate memory reservation');
    assert(busy.headers.get('retry-after'), 'capacity refusal must tell the client when to retry');
    const busyBody = await busy.json();
    assert.equal(busyBody.error, 'memory_capacity', 'the refusal must name the isolate memory reservation it holds');
    const reader = response.body.getReader();
    const chunks = [];
    for (let count = 0;; count++) {
      const { value, done } = await reader.read();
      if (done) break;
      chunks.push(Buffer.from(value));
      if (count % 8 === 0) await new Promise((resolve) => setTimeout(resolve, 1));
    }
    const imageReferences = new Set();
    const animatedImages = new Set();
    const text = epubText(Buffer.concat(chunks), (name, bytes) => {
      assert(!name.endsWith('/cover.xhtml'), 'a body illustration must not become an unspecified cover');
      if (name.endsWith('.opf')) assert.doesNotMatch(bytes.toString(), /\bcover-image\b/);
      if (name.endsWith('.xhtml')) {
        for (const [, source] of bytes.toString().matchAll(/<(?:img|image)\b[^>]*\b(?:src|(?:xlink:)?href)="([^"]+)"/g)) {
          imageReferences.add(new URL(source, `https://epub.invalid/${name}`).pathname.slice(1));
        }
      }
      if (isAnimation(bytes)) {
        verifyPng(bytes, animation);
        animatedImages.add(name);
      }
    });
    assert(text.includes('初回取得の本文です。'));
    assert.equal(imageReferences.size, 1, JSON.stringify({ references: [...imageReferences], animations: [...animatedImages] }));
    for (const image of imageReferences) assert(animatedImages.has(image), 'the displayed image must retain every animation frame');
    assert.equal(liveReaders(), 0, 'EOF must release the actual D1 reader lease inline');
    // Service-dispatch cancellation. The test caller dispatches to the actual
    // writer through a real service binding and cancels that dispatch; the
    // receiver's own request.signal must fire and abort its pending upstream
    // read. A local TCP disconnect cannot deliver that signal through the dev
    // bridge, so this proves the service-dispatch path only.
    const leaseRows = () => sql(`SELECT reader_id,epoch,expires_at FROM novel_reader_leases WHERE prefix=${quote(readerPrefix)} ORDER BY reader_id`)[0].results;
    const headersPaused = Promise.withResolvers();
    const headersClosed = Promise.withResolvers();
    const upstreamTornDown = Promise.withResolvers();
    let headerResponse;
    const headerWitness = { held: null, socket: { ended: false, closed: false }, error: null };
    multipart.beforeReadHeaders = async (req, res, key, bytes, read) => {
      // The witness must be the upstream read this EPUB request owns: the
      // published pool object. The same animation bytes also travel as a
      // staged copy and as a producer's re-read, and holding one of those
      // would say nothing about the in-flight request.
      if (key !== animationKey) return false;
      if (!isAnimation(bytes)) return false;
      const range = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range ?? '');
      const start = range ? Number(range[1]) : 0;
      const end = range?.[2] ? Math.min(Number(range[2]), bytes.length - 1) : bytes.length - 1;
      if (end - start + 1 <= 8 * 1024 * 1024) return false;
      assert.equal(headerResponse, undefined, 'only one upstream read of the published object may await headers');
      assert.equal(res.headersSent, false);
      headerResponse = res;
      read.held = true;
      headerWitness.held = {
        key, method: req.method, url: req.url, range: req.headers.range ?? null,
        ifMatch: req.headers['if-match'] ?? null, span: end - start + 1, at: Date.now(),
      };
      res.once('close', () => headersClosed.resolve());
      // An aborted upstream surfaces as a socket error; record it instead of
      // letting it escape as an unhandled event.
      res.once('error', (error) => { headerWitness.error = error.code ?? error.message; });
      // The response object can be closed without the connection being torn
      // down. Only the socket proves that the peer really aborted.
      req.socket.once('end', () => { headerWitness.socket.ended = true; });
      req.socket.once('close', () => { headerWitness.socket.closed = true; upstreamTornDown.resolve(); });
      // Read-only witness: the caller may cancel only once the upstream
      // request is really pending.
      headersPaused.resolve();
      upstreamHeld?.resolve();
      await headersClosed.promise;
      return true;
    };
    try {
      const dispatchTarget = `http://${topology.writerName}/api/novels/${animationId}/download.epub`;
      const knownReaders = new Set(leaseRows().map((row) => row.reader_id));
      upstreamHeld = Promise.withResolvers();
      heldRelease = Promise.withResolvers();
      const caller = api(`/_fixture/dispatch-abort?target=${encodeURIComponent(dispatchTarget)}`)
        .then((result) => result.json())
        .catch((error) => ({ name: error.name }));
      let deadline;
      try {
        await Promise.race([
          headersPaused.promise,
          new Promise((_, reject) => {
            deadline = setTimeout(() => reject(new Error(`the dispatched EPUB did not reach the upstream header gate (reads=${JSON.stringify(multipart.reads.slice(-6))})`)), 5000);
          }),
        ]);
      } finally { clearTimeout(deadline); }
      // The receiver admitted its durable reader before it touched the
      // upstream, and it is still live while the upstream is held.
      const admitted = leaseRows().find((row) => !knownReaders.has(row.reader_id));
      assert(admitted, 'the dispatched EPUB must admit its durable novel reader');
      assert(Number(admitted.expires_at) > Math.floor(Date.now() / 1000), 'the dispatched EPUB must hold a live novel admission');
      assert(liveReaders() > 0, 'the held upstream must retain the original novel admission');
      // Only now may the caller cancel its dispatch.
      await fetch(`http://127.0.0.1:${s3Port}/_fixture/release-held`, { method: 'POST' });
      let outcome;
      try {
        outcome = await Promise.race([
          caller,
          new Promise((_, reject) => {
            deadline = setTimeout(() => reject(new Error(`the service-dispatch caller did not settle (reads=${JSON.stringify(multipart.reads.slice(-6))})`)), 5000);
          }),
        ]);
      } finally { clearTimeout(deadline); }
      assert.equal(outcome.name, 'AbortError', 'the cancelled service dispatch must reject with AbortError');
      // The held read is the only full delivery of the published object: no
      // replay, producer or second request shares it while it is open.
      const poolReads = multipart.reads.filter((read) => read.key === animationKey && read.span > 8 * 1024 * 1024);
      const heldRead = poolReads.find((read) => read.held);
      assert(heldRead, `the armed gate must hold the published object read: ${JSON.stringify(poolReads)}`);
      assert.equal(poolReads.filter((read) => read.held).length, 1, `only one upstream read of the published object may be held: ${JSON.stringify(poolReads)}`);
      const overlapping = poolReads.filter((read) => read !== heldRead && read.at < (heldRead.closedAt ?? Infinity) && heldRead.at < (read.closedAt ?? Infinity));
      assert.equal(overlapping.length, 0, `no other full read of the published object may overlap the held one: ${JSON.stringify(poolReads)}`);
      try {
        // The Node response close stays the witness it always was; the socket
        // teardown is the stricter physical fact that must come with it.
        await Promise.race([Promise.all([headersClosed.promise, upstreamTornDown.promise]), new Promise((_, reject) => {
          deadline = setTimeout(() => reject(new Error(`EPUB cancellation did not abort the upstream header wait (readers=${liveReaders()}, tcpEnded=${headerWitness.socket.ended}, destroyed=${headerResponse.destroyed}, held=${JSON.stringify(headerWitness.held)}, socket=${JSON.stringify(headerWitness.socket)}, error=${headerWitness.error}, reads=${JSON.stringify(multipart.reads.slice(-4))})`)), 5000);
        })]);
      } finally { clearTimeout(deadline); }
      assert.equal(headerWitness.socket.closed, true, 'the upstream connection must be torn down, not merely abandoned');
      assert.equal(headerResponse.headersSent, false, 'cancellation must not invent upstream headers');
      assert.equal(headerResponse.writableEnded, false, 'cancellation must not finish the upstream response');
      // Reader contract: the lease has a TTL fallback. An inline release is
      // valid when the invocation could still observe the disconnect signal;
      // otherwise the lease drains by its own expiry. The test never deletes,
      // backdates or rewrites a row, and EOF still releases inline.
      const current = leaseRows().find((row) => row.reader_id === admitted.reader_id);
      if (current) {
        assert.equal(current.epoch, admitted.epoch, 'the cancelled reader lease must keep its own generation');
        assert(Number(current.expires_at) >= Number(admitted.expires_at), 'the cancelled reader lease expiry must not be backdated or rewritten');
      }
      const expiry = Number(current?.expires_at ?? admitted.expires_at);
      const drainBy = expiry * 1000 + 5000;
      while (liveReaders() !== 0) {
        assert(Date.now() < drainBy, `the cancelled reader lease must drain by its own expiry (${expiry}): the renewal must stop`);
        await new Promise((resolve) => setTimeout(resolve, 2000));
      }
    } finally {
      multipart.beforeReadHeaders = null;
      upstreamHeld?.resolve();
      heldRelease?.resolve();
      upstreamHeld = null;
      heldRelease = null;
      headerResponse?.destroy();
    }
    console.log('PASS service-dispatch cancellation before upstream headers aborts the real fetch; the reader lease is released inline or drains by its own TTL');

    const upstreamClosed = Promise.withResolvers();
    const upstreamPaused = Promise.withResolvers();
    const stalledTornDown = Promise.withResolvers();
    let stalledResponse;
    const cancellationReads = { total: 0, animation: 0, bodies: 0, first: [], last: null, held: null, socket: { ended: false, closed: false }, error: null };
    multipart.beforeReadBody = async (req, res, key, bytes, range, read) => {
      cancellationReads.total++;
      if (isAnimation(bytes)) cancellationReads.animation++;
      const observed = { key, size: bytes.length, range };
      if (cancellationReads.first.length < 4) cancellationReads.first.push(observed);
      cancellationReads.last = observed;
      // Pass-through images may stream from their immutable pool object or a
      // stage. Pause the image body, not one storage implementation's path.
      if (!isAnimation(bytes)) return false;
      // Header probes use small ranges. The body may use one full-image
      // Range response while still consuming it in bounded stream chunks.
      if (range && range.end - range.start + 1 <= 8 * 1024 * 1024) return false;
      // Assembling the EPUB copies the animation once; the response body is
      // the next full delivery of it. Count deliveries, not storage paths:
      // the shared pool object, a private stage and a direct full-range read
      // all carry the same bytes, and the copy must not stall the assembly.
      cancellationReads.bodies++;
      if (cancellationReads.bodies < 2) return false;
      assert.equal(stalledResponse, undefined, 'only the EPUB image stream may reach the cancellation gate');
      stalledResponse = res;
      read.held = true;
      // Name the delivery that is held: the pool object, the private stage
      // or a direct full-range read all carry the same animation bytes.
      cancellationReads.held = { key, url: req.url, range: req.headers.range ?? null, ifMatch: req.headers['if-match'] ?? null, at: Date.now() };
      res.once('close', () => upstreamClosed.resolve());
      res.once('error', (error) => { cancellationReads.error = error.code ?? error.message; });
      // Only the socket proves that the peer aborted the upstream request.
      req.socket.once('end', () => { cancellationReads.socket.ended = true; });
      req.socket.once('close', () => { cancellationReads.socket.closed = true; stalledTornDown.resolve(); });
      // Never send EOF: releasing admission by silently finishing the book
      // would not prove cancellation. The upstream request must be aborted.
      const start = range?.start ?? 0;
      const length = 8 * 1024 * 1024;
      res.write(bytes.subarray(start, start + length));
      upstreamPaused.resolve();
      // Read-only witness: the caller cancels only once the upstream body is
      // really held.
      upstreamHeld?.resolve();
      await upstreamClosed.promise;
      return true;
    };
    try {
      const dispatchTarget = `http://${topology.writerName}/api/novels/${animationId}/download.epub`;
      upstreamHeld = Promise.withResolvers();
      heldRelease = Promise.withResolvers();
      // The caller reads the real EPUB body and then cancels its own dispatch.
      const caller = api(`/_fixture/dispatch-body?target=${encodeURIComponent(dispatchTarget)}`)
        .then((result) => result.json())
        .catch((error) => ({ name: error.name }));
      let deadline;
      try {
        await Promise.race([
          upstreamPaused.promise,
          new Promise((_, reject) => {
            deadline = setTimeout(() => reject(new Error(`the EPUB image stream did not reach the cancellation gate (reads=${JSON.stringify(multipart.reads.slice(-6))})`)), 5000);
          }),
        ]);
      } finally { clearTimeout(deadline); }
      assert(stalledResponse, 'the upstream image must be paused before EOF');
      assert(liveReaders() > 0, 'the paused HTTP body must retain novel admission as well as memory');
      // Only now may the caller cancel its dispatch.
      await fetch(`http://127.0.0.1:${s3Port}/_fixture/release-held`, { method: 'POST' });
      let outcome;
      try {
        outcome = await Promise.race([
          caller,
          new Promise((_, reject) => {
            deadline = setTimeout(() => reject(new Error(`the service-dispatch caller did not settle (reads=${JSON.stringify(multipart.reads.slice(-6))})`)), 5000);
          }),
        ]);
      } finally { clearTimeout(deadline); }
      assert.equal(outcome.status, 200, 'the dispatched EPUB must stream the real body (the cancelled dispatch released the memory reservation)');
      assert(outcome.read >= 4 * 1024 * 1024, `the caller must read the real EPUB body before it cancels: ${JSON.stringify(outcome)}`);
      assert.equal(outcome.name, 'AbortError', 'the cancelled dispatch body must reject with AbortError');
      try {
        // The Node response close stays the witness it always was; the socket
        // teardown is the stricter physical fact that must come with it.
        await Promise.race([Promise.all([upstreamClosed.promise, stalledTornDown.promise]), new Promise((_, reject) => {
          deadline = setTimeout(() => reject(new Error(`EPUB cancellation did not abort the upstream body (readers=${liveReaders()}, held=${JSON.stringify(cancellationReads.held)}, socket=${JSON.stringify(cancellationReads.socket)}, error=${cancellationReads.error}, reads=${JSON.stringify(cancellationReads)})`)), 5000);
        })]);
      } finally { clearTimeout(deadline); }
      assert.equal(cancellationReads.socket.closed, true, 'the upstream connection must be torn down, not merely abandoned');
      assert.equal(stalledResponse.writableEnded, false, 'cancellation must not drain the remaining image');
    } finally {
      multipart.beforeReadBody = null;
      upstreamHeld?.resolve();
      heldRelease?.resolve();
      upstreamHeld = null;
      heldRelease = null;
      stalledResponse?.destroy();
    }
    const afterCancel = await api(`/api/novels/${animationId}/download.epub`);
    assert.equal(afterCancel.status, 200, 'client cancellation must release the memory reservation');
    // The dev bridge ignores a Node body cancel, so the fresh EPUB is consumed
    // to EOF instead: that proves the recovery and leaves nothing running.
    const afterImages = new Set();
    const afterText = epubText(Buffer.from(await afterCancel.arrayBuffer()), (name, bytes) => {
      if (isAnimation(bytes)) {
        verifyPng(bytes, animation);
        afterImages.add(name);
      }
    });
    assert(afterText.includes('初回取得の本文です。'), 'the EPUB after a cancelled dispatch must still carry the body');
    assert.equal(afterImages.size, 1, `the EPUB after a cancelled dispatch must still embed the animation: ${JSON.stringify([...afterImages])}`);
    sourceAnimation = null;
    const memory = await (await api('/_fixture/memory')).json();
    assert.equal(memory.instances, 1, 'the successful animation path must not recover from a Wasm trap');
    assert(memory.highWater < 96 * 1024 * 1024, `Wasm high-water must leave space for the JS heap: ${JSON.stringify(memory)}`);
    console.log(`Worker animation Wasm memory high-water: ${memory.highWater} bytes (JS heap excluded)`);
    console.log('PASS large64-frame animation streams through multipart storage and EPUB with alpha/timing intact; service-dispatch cancel aborts the upstream');

    const color = [13, 37, 193, 127];
    const expectedPixels = createHash('sha256');
    const expectedRow = Buffer.alloc(658 * 4, Buffer.from(color));
    for (let y = 0; y < 658; y++) expectedPixels.update(expectedRow);
    const expectedCover = { animated: false, width: 658, height: 658, frameCount: 1, digests: [expectedPixels.digest('hex')] };
    for (const [size, indexedNoise, slug] of [[4096, true, 'smoke-single'], [6000, false, 'smoke-series']]) {
      sourceStaticImage = animationFixture({ frameCount: 1, width: size, height: size, solidColor: color, archive: false, indexedNoise });
      assert(sourceStaticImage.bytes.length < 16 * 1024 * 1024, 'large PNG stays within the former encoded-size limit');
      await download(new URL(`/${slug}/`, target).href, false, false);
      const largeId = scalar(`SELECT id FROM novels WHERE ncode='${slug}'`);
      // Import the explicit cover through the production streaming boundary;
      // exercise chunked storage without embedding large blobs in SQL.
      const transfer = transferClient(`http://127.0.0.1:${port}`, token);
      const coverExport = await transfer.exportData([largeId]);
      const coverNovel = coverExport.manifest.novels[0];
      const coverFiles = coverNovel.files.filter(file => file.path !== 'cover.png').map(file => ({
        path: file.path, bytes: coverExport.files.get(`novels/0/${file.path}`),
      }));
      coverFiles.push({ path: 'cover.png', bytes: sourceStaticImage.bytes });
      const coverImport = await transfer.importData([{ ...coverNovel, files: coverFiles }], { policy: 'overwrite' });
      assert.equal(coverImport.status.state, 'completed');
      assert.equal(coverImport.plan[0].action, 'overwrite');
      const largeEpub = await api(`/api/novels/${largeId}/download.epub`);
      assert.equal(largeEpub.status, 200, largeEpub.status === 200 ? undefined : await largeEpub.text());
      const coverPaths = new Set();
      const bodyPaths = new Set();
      const entries = new Map();
      epubText(Buffer.from(await largeEpub.arrayBuffer()), (name, bytes) => {
        if (name.endsWith('.xhtml') && !name.endsWith('/cover.xhtml')) {
          for (const [, source] of bytes.toString().matchAll(/<(?:img|image)\b[^>]*\b(?:src|(?:xlink:)?href)="([^"]+)"/g)) {
            bodyPaths.add(new URL(source, `https://epub.invalid/${name}`).pathname.slice(1));
          }
        }
        if (name.endsWith('.png')) entries.set(name, bytes);
        if (name.endsWith('/cover.xhtml')) {
          for (const [, source] of bytes.toString().matchAll(/(?:src|xlink:href)="([^"]+)"/g)) {
            coverPaths.add(new URL(source, `https://epub.invalid/${name}`).pathname.slice(1));
          }
        }
      });
      assert.equal(coverPaths.size, 1, 'the explicit cover.png must produce a real cover');
      assert.equal(bodyPaths.size, 1, 'the first body illustration must remain in the body');
      for (const path of coverPaths) {
        assert(entries.has(path), `missing cover image ${path}`);
        verifyPng(entries.get(path), expectedCover);
        assert(!bodyPaths.has(path), 'the explicit cover must not replace the body image');
      }
      sourceStaticImage = null;
      const largeMemory = await (await api('/_fixture/memory')).json();
      assert.equal(largeMemory.instances, 1, 'large-image resizing must not trap and restart Wasm');
      assert(largeMemory.highWater < 112 * 1024 * 1024, `PNG resize must leave room for the JS heap: ${JSON.stringify(largeMemory)}`);
      console.log(`PASS ${size}² PNG becomes a pixel/alpha-exact658² cover; Wasm high-water=${largeMemory.highWater} bytes (JS heap excluded)`);
    }

    const transferId = scalar("SELECT id FROM novels WHERE ncode='smoke-series'");
    const transfer = transferClient(`http://127.0.0.1:${port}`, token);
    const exported = await transfer.exportData([transferId]);
    const importedNovels = exported.manifest.novels.map((novel, index) => ({
      ...novel, files: novel.files.map(file => ({ path: file.path, bytes: exported.files.get(`novels/${index}/${file.path}`) })),
    }));
    const imported = await transfer.importData(importedNovels);
    assert.equal(imported.status.state, 'completed');
    assert(imported.plan.every(row => row.action === 'skip'));
    assert(!transfer.requests.some(request => request.type === 'application/zip'));
    assert(transfer.requests.filter(request => request.type === 'application/octet-stream').every(request => request.bytes <= 8 * 1024 * 1024));
    const transferMemory = await (await api('/_fixture/memory')).json();
    assert.equal(transferMemory.instances, 1, 'bounded v2 transfer must not trap Wasm');
    assert(transferMemory.highWater < 112 * 1024 * 1024, `bounded v2 transfer leaves room for the JS heap: ${JSON.stringify(transferMemory)}`);
    console.log(`PASS chunked v2 transfer/Skip stays bounded; Wasm high-water=${transferMemory.highWater}`);
  }

  // Hold the actual Durable Object notification after the ledger has yielded,
  // without adding hooks to Rust production code. The queue wrapper witnesses
  // completed deliveries, so absence of resurrection is not a sleep assertion.
  const wrapper = join(work, 'continuation-wrapper.mjs');
  const production = JSON.stringify(relative(work, join(root, 'build/worker/shim.mjs')).replaceAll('\\', '/'));
  writeFileSync(wrapper, `
import Entry, { PushHub as ProductionPushHub } from ${production};
export * from ${production};
// The armed canonical write is shared by every script of the topology: the
// acquisition runs in a lane isolate while the arm arrives at the writer, so
// the target lives in the fixture sink and not in this module scope.
async function armedCanonicalTarget() {
  const response = await fetch('http://127.0.0.1:${s3Port}/_fixture/canonical-arm');
  if (!response.ok) return null;
  const value = await response.json();
  return value && value.key ? value : null;
}
function canonicalDatabase(database, onCanonicalBoundary) {
  const metadata = new WeakMap();
  const statement = (raw, sql, values = []) => {
    const proxy = new Proxy(raw, { get(target, name) {
      if (name === 'constructor') return target.constructor;
      if (name === 'bind') return (...bound) => statement(target.bind(...bound), sql, bound);
      const value = Reflect.get(target, name, target);
      return typeof value === 'function' ? value.bind(target) : value;
    } });
    metadata.set(proxy, { raw, sql, values });
    return proxy;
  };
  const wrap = raw => new Proxy(raw, { get(target, name) {
    if (name === 'constructor') return target.constructor;
    if (name === 'prepare') return sql => statement(target.prepare(sql), sql);
    if (name === 'batch') return async statements => {
      const entries = statements.map(value => metadata.get(value)).filter(Boolean);
      const canonical = entries.filter(entry =>
        /(?:INSERT(?:\\s+OR\\s+[A-Z]+)?\\s+INTO|UPDATE|DELETE\\s+FROM)\\s+(?:objects|content_objects|transfer_object_refs)\\b/i.test(entry.sql));
      const selected = canonical.length > 0 ? await armedCanonicalTarget() : null;
      const held = selected && !selected.consumed && canonical.some(entry =>
        entry.values.some(value => value === selected.key));
      if (held) {
        onCanonicalBoundary();
        const guards = entries.filter(entry => entry.sql.includes('transfer_storage_guards'));
        const ownerBound = guards.some(entry => entry.sql.includes(selected.token)
          || entry.values.some(value => typeof value === 'string' && value.includes(selected.token)));
        const response = await fetch('http://127.0.0.1:${s3Port}/_fixture/canonical-boundary', {
          method: 'POST', body: JSON.stringify({ key: selected.key, ownerBound }),
        });
        if (!response.ok) throw new Error('canonical boundary fixture failed');
      }
      try {
        const results = await target.batch(statements.map(value => metadata.get(value)?.raw || value));
        if (held) {
          const failure = results.find(result => result.success === false || result.error);
          await fetch('http://127.0.0.1:${s3Port}/_fixture/canonical-result', {
            method: 'POST', body: JSON.stringify({ ok: !failure, error: failure?.error }),
          });
        }
        return results;
      } catch (error) {
        if (held) await fetch('http://127.0.0.1:${s3Port}/_fixture/canonical-result', {
          method: 'POST', body: JSON.stringify({ ok: false, error: String(error) }),
        });
        throw error;
      }
    };
    const value = Reflect.get(target, name, target);
    if (name === 'withSession' && typeof value === 'function') return (...args) => wrap(value.apply(target, args));
    return typeof value === 'function' ? value.bind(target) : value;
  } });
  return wrap(database);
}
const hasPartial = value => value && typeof value === 'object'
  && (value.type === 'queue_partial' || Object.values(value).some(hasPartial));
export class PushHub extends ProductionPushHub {
  async fetch(request, ...rest) {
    if (request.method === 'POST' && new URL(request.url).pathname === '/broadcast'
        && hasPartial(await request.clone().json())) {
      await fetch('http://127.0.0.1:${s3Port}/_fixture/yield', { method: 'POST' });
    }
    return super.fetch(request, ...rest);
  }
}
export default class extends Entry {
  async fetch(request, ...rest) {
    const path = new URL(request.url).pathname;
    if (path === '/__test/canonical-write/arm') {
      // The armed write is shared by every script of the topology: the
      // acquisition runs in a lane isolate while the arm arrives at the
      // writer, so the fixture sink keeps the target, not this module scope.
      const target = { ...await request.json(), consumed: false };
      const armed = await fetch('http://127.0.0.1:${s3Port}/_fixture/canonical-arm', {
        method: 'POST', body: JSON.stringify(target),
      });
      if (!armed.ok) throw new Error('canonical arm fixture failed');
      return Response.json({ success: true });
    }
    if (path === '/__test/canonical-write/clear') {
      const cleared = await fetch('http://127.0.0.1:${s3Port}/_fixture/canonical-arm', { method: 'DELETE' });
      if (!cleared.ok) throw new Error('canonical arm fixture failed');
      return Response.json({ success: true });
    }
    if (request.method === 'POST' && path === '/_fixture/replay') {
      if (request.headers.get('Authorization') !== 'Bearer ${token}') return new Response('Unauthorized', { status: 401 });
      const envelope = await request.json();
      await this.env['ACQUISITION_QUEUE_' + envelope.lane].send(envelope);
      return Response.json({ success: true });
    }
    return super.fetch(request, ...rest);
  }
  async queue(batch) {
    let canonical = false;
    try {
      // The proxied DB must reach the production entrypoint, so the batch runs
      // on a fresh instance that owns the proxied binding.
      await new Entry(this.ctx, { ...this.env, DB: canonicalDatabase(this.env.DB, () => { canonical = true; }) }).queue(batch);
    } finally {
      await fetch('http://127.0.0.1:${s3Port}/_fixture/delivery', {
        method: 'POST', body: JSON.stringify({
          ids: batch.messages.map(message => message.body.job_id),
          messages: batch.messages.map(message => message.body),
          canonical,
        }),
      });
    }
  }
}
`);
  await defaultLibrary('clear-after-yield', { required: true, entrypoint: wrapper });
  await installDefaultSource();
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
  sourceImageCount = 500;
  sourceRequests.clear(); imageRequests.clear();
  const gate = { entered: Promise.withResolvers(), release: Promise.withResolvers() };
  const witness = { ids: [], messages: [], returned: Promise.withResolvers(), completed: Promise.withResolvers() };
  yieldBarrier = gate; deliveryWitness = witness;
  const tick = async () => {
    const response = await api('/__scheduled?cron=*+*+*+*+*');
    assert.equal(response.status, 200, await response.text());
  };
  const bounded = (promise, label) => Promise.race([
    promise, new Promise((_, reject) => setTimeout(() => reject(new Error(`${label} timed out`)), 180000).unref()),
  ]);
  const settleCreation = async (jobId) => {
    const deadline = Date.now() + 180000;
    while (Date.now() < deadline) {
      if (scalar(`SELECT checkpoint_json IS NULL FROM worker_jobs WHERE job_id=${quote(jobId)}`)) return;
      await tick();
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert.equal(scalar(`SELECT checkpoint_json IS NULL FROM worker_jobs WHERE job_id=${quote(jobId)}`), 1,
      'actual scheduled removal and terminal release must acknowledge full cleanup before clearing its proof');
  };
  try {
    const accepted = await (await api('/api/download', { targets: [target] })).json();
    assert(accepted.success, JSON.stringify(accepted));
    const jobId = accepted.results[0].job_id;
    await bounded(gate.entered.promise, 'post-yield notification');
    const pending = await (await api(`/api/jobs/${jobId}`)).json();
    assert.equal(pending.status, 'pending');
    assert(scalar(`SELECT checkpoint_json IS NOT NULL FROM worker_jobs WHERE job_id=${quote(jobId)}`));
    const cancelledCheckpoint = JSON.parse(scalar(`SELECT checkpoint_json FROM worker_jobs WHERE job_id=${quote(jobId)}`));
    assert(Array.isArray(cancelledCheckpoint.creation_reservation), 'the cancelled new-download continuation must own an exact numeric reservation');
    assert(cancelledCheckpoint.write_origin, 'namespace claim and original epoch must remain recoverable at cancellation');
    const cleared = await (await api('/api/queue/clear', {})).json();
    assert(cleared.success, JSON.stringify(cleared));
    const cancelled = await (await api(`/api/jobs/${jobId}`)).json();
    assert.equal(cancelled.status, 'permanent');
    const received = [...sourceRequests.entries()];
    const images = [...imageRequests.entries()];
    gate.release.resolve();
    await bounded(witness.returned.promise, 'original acquisition delivery completion');
    // A cancelled v3 job is no longer automatically redispatched. Replay its
    // captured real assignment to prove a late delivery cannot resurrect it.
    const original = witness.messages.find(message => message.job_id === jobId);
    assert.equal(original.version, 3);
    assert((await (await api('/_fixture/replay', original)).json()).success);
    await bounded(witness.completed.promise, 'continuation delivery completion');
    assert(witness.ids.every((id) => id === jobId), JSON.stringify(witness.ids));
    assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='download'"), 1,
      'clearing a yielded acquisition must not create a new job identity');
    assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='convert'"), 0);
    const after = await (await api(`/api/jobs/${jobId}`)).json();
    assert.equal(after.status, cancelled.status);
    assert.equal(after.last_error, cancelled.last_error);
    assert.equal(after.attempts, cancelled.attempts);
    assert.deepEqual([...sourceRequests.entries()], received, 'canceled continuation cannot fetch another episode or TOC');
    assert.deepEqual([...imageRequests.entries()], images, 'canceled continuation cannot fetch another image');
    await settleCreation(jobId);
    assert.equal(scalar(`SELECT checkpoint_json IS NULL FROM worker_jobs WHERE job_id=${quote(jobId)}`), 1,
      'only complete fenced cleanup may clear the durable reservation proof');
    assert.equal(scalar("SELECT COUNT(*) FROM content_objects WHERE instr(object_key,'/cache/')=0"), 0,
      'terminal creation cleanup must not retain an unpublished episode');
    console.log('PASS clearing after budget yield preserves the tombstone and original identity through full fenced cleanup');

    // The same namespace must now support a REAL fresh acquisition. Pause its
    // first body response to inspect the new claim before publication, rather
    // than inferring release from an empty checkpoint column.
    sourceImageCount = 0;
    sourceGeneration = 1;
    const retryEntered = Promise.withResolvers();
    const retryRelease = Promise.withResolvers();
    sourcePause = async (path) => {
      if (path !== '/smoke-novel/1/') return;
      sourcePause = null;
      retryEntered.resolve();
      await retryRelease.promise;
    };
    const recreated = download(target, false);
    try {
      await bounded(retryEntered.promise, 'fresh creation after cancellation');
      const checkpoint = JSON.parse(scalar("SELECT checkpoint_json FROM worker_jobs WHERE kind='download' AND status='running'"));
      assert.equal(checkpoint.creation_reservation[0], cancelledCheckpoint.creation_reservation[0], 'the released unpublished numeric ID must be reusable');
      assert.notEqual(checkpoint.creation_reservation[1], cancelledCheckpoint.creation_reservation[1], 'reusing that ID must create a new generation');
      assert.equal(checkpoint.write_origin.prefix, cancelledCheckpoint.write_origin.prefix);
      assert(checkpoint.write_origin.epoch > cancelledCheckpoint.write_origin.epoch, 'release must invalidate the old producer epoch before admitting the new claim');
    } finally {
      retryRelease.resolve();
      sourcePause = null;
    }
    await recreated;
    const recreatedId = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
    assert.equal(recreatedId, cancelledCheckpoint.creation_reservation[0]);
    const recreatedEpub = await api(`/api/novels/${recreatedId}/download.epub`);
    assert.equal(recreatedEpub.status, 200, await recreatedEpub.clone().text());
    assert(epubText(Buffer.from(await recreatedEpub.arrayBuffer())).includes('初回取得の本文です。'.repeat(100)), 'the new claim must publish and serve the actual section bytes');
  } finally {
    gate.release.resolve();
    yieldBarrier = null; deliveryWitness = null;
  }

  // Reclaiming a SAME-job invocation changes its token, not its novel epoch.
  // Pause the real canonical batch after its last active check: the actual
  // D1 guard must reject the old token even while the row remains running.
  {
    await defaultLibrary('same-epoch-execution-token', { required: true, entrypoint: wrapper });
    await installDefaultSource();
    sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
    sourceImageCount = 0;
    await download(target, false);
    sourceGeneration = 2;
    await download(target, true); // Positive control: an existing-record owner really commits.
    const id = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
    const client = transferClient(`http://127.0.0.1:${port}`, token);
    const before = await client.exportData([id]);
    const bodyPath = before.manifest.novels[0].files.find(file => file.path.startsWith('本文/') && file.path.endsWith('.yaml') && !file.path.includes('/cache/') && !file.path.includes('/.cache/')).path;
    const originalBody = before.files.get(`novels/0/${bodyPath}`);
    assert(originalBody.includes(Buffer.from('更新後の本文です。')), 'positive control must publish real changed section bytes');
    sourceGeneration = 3;
    const sourceGate = { entered: Promise.withResolvers(), release: Promise.withResolvers() };
    const writeGate = { entered: Promise.withResolvers(), release: Promise.withResolvers(), result: Promise.withResolvers() };
    const completed = { ids: [], expected: 1, canonicalOnly: true, completed: Promise.withResolvers() };
    canonicalWriteBarrier = writeGate;
    deliveryWitness = completed;
    sourcePause = async path => {
      if (path !== '/smoke-novel/1/') return;
      sourcePause = null;
      sourceGate.entered.resolve();
      await sourceGate.release.promise;
    };
    let jobId;
    let successor;
    try {
      const accepted = await (await api('/api/download', { targets: [target], force: true })).json();
      assert(accepted.success, JSON.stringify(accepted));
      jobId = accepted.results[0].job_id;
      await bounded(sourceGate.entered.promise, 'live source HTTP before canonical write');
      const running = sql(`SELECT execution_token,checkpoint_json FROM worker_jobs WHERE job_id=${quote(jobId)} AND status='running'`)[0].results[0];
      const checkpoint = JSON.parse(running.checkpoint_json);
      assert.equal(checkpoint.creation_reservation, null, 'existing-record writes require execution ownership too');
      assert(!running.checkpoint_json.includes(running.execution_token), 'the execution token must not be persisted in the portable checkpoint');
      const prefix = checkpoint.write_origin.prefix;
      const epoch = scalar(`SELECT epoch FROM novel_access_gates WHERE prefix=${quote(prefix)}`);
      const armed = await (await api('/__test/canonical-write/arm', { key: `${prefix.replace(/\/$/, '')}/${bodyPath}`, token: running.execution_token })).json();
      assert(armed.success);
      sourceGate.release.resolve();
      const boundary = await bounded(writeGate.entered.promise, 'actual canonical transaction boundary');
      assert.equal(boundary.ownerBound, true, 'the actual prepared canonical guard must retain this invocation token');
      successor = globalThis.crypto.randomUUID();
      const now = new Date().toISOString();
      const until = new Date(Date.now() + 600000).toISOString();
      sql(`UPDATE worker_jobs SET execution_token=${quote(successor)},lease_until=${quote(until)},updated_at=${quote(now)},recovery_attempts=recovery_attempts+1 WHERE job_id=${quote(jobId)} AND status='running' AND execution_token=${quote(running.execution_token)}`);
      const claimed = sql(`SELECT status,execution_token FROM worker_jobs WHERE job_id=${quote(jobId)}`)[0].results[0];
      assert.equal(claimed.status, 'running');
      assert.equal(claimed.execution_token, successor);
      assert.equal(scalar(`SELECT epoch FROM novel_access_gates WHERE prefix=${quote(prefix)}`), epoch, 'the race must not rely on epoch revocation');
      writeGate.release.resolve();
      const result = await bounded(writeGate.result.promise, 'actual stale canonical transaction result');
      assert.equal(result.ok, false, `the real D1 canonical batch accepted an old execution token: ${JSON.stringify(result)}`);
      await bounded(completed.completed.promise, 'stale physical invocation completion');
      const live = sql(`SELECT status,execution_token FROM worker_jobs WHERE job_id=${quote(jobId)}`)[0].results[0];
      assert.equal(live.status, 'running', 'old settlement must not terminate the successor');
      assert.equal(live.execution_token, successor);
      assert.equal(scalar(`SELECT epoch FROM novel_access_gates WHERE prefix=${quote(prefix)}`), epoch);
      // Explicit fixture teardown of the durable successor claim; no second
      // physical isolate is fabricated or assumed dead by this regression.
      sql(`UPDATE worker_jobs SET status='permanent',last_error='cancelled by user via Web UI',execution_token=NULL,lease_until=NULL,checkpoint_json=NULL WHERE job_id=${quote(jobId)} AND status='running' AND execution_token=${quote(successor)}`);
      const after = await client.exportData([id]);
      assert.deepEqual(after.files.get(`novels/0/${bodyPath}`), originalBody, 'actual stale canonical rejection must preserve the complete published section bytes');
    } finally {
      sourcePause = null;
      sourceGate.release.resolve();
      writeGate.release.resolve();
      await api('/__test/canonical-write/clear', {});
      canonicalWriteBarrier = null;
      deliveryWitness = null;
      if (jobId && successor) sql(`UPDATE worker_jobs SET status='permanent',last_error='cancelled by user via Web UI',execution_token=NULL,lease_until=NULL,checkpoint_json=NULL WHERE job_id=${quote(jobId)} AND execution_token=${quote(successor)}`);
    }
    console.log('PASS real same-epoch canonical transaction rejects a superseded SAME-job execution token and preserves actual section bytes');
  }

  // Pause a REAL continuation before it enters Rust/MemoryPermit admission,
  // publish replacement bytes through transfer v2, then let that same durable
  // job run. A JSON checkpoint assertion alone cannot prove stale-write safety.
  {
    const staleWrapper = join(work, 'stale-origin-wrapper.mjs');
    writeFileSync(staleWrapper, `
import Entry from ${production};
export * from ${production};
export default class extends Entry {
  async queue(batch, ...rest) {
    for (const message of batch.messages) {
      const row = await this.env.DB.prepare("SELECT checkpoint_json FROM worker_jobs WHERE job_id=? AND status='pending'")
        .bind(message.body.job_id).first();
      if (row?.checkpoint_json) {
        await fetch('http://127.0.0.1:${s3Port}/_fixture/yield', { method: 'POST' });
      }
    }
    return super.queue(batch, ...rest);
  }
}
`);
    await defaultLibrary('stale-origin-after-transfer', { required: true, entrypoint: staleWrapper });
    await installDefaultSource();
    sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
    await download(target, false);
    const id = scalar("SELECT id FROM novels WHERE ncode='smoke-novel'");
    const client = transferClient(`http://127.0.0.1:${port}`, token);
    const baseline = await client.exportData([id]);
    const novel = baseline.manifest.novels[0];
    const bodyPath = novel.files.find(file => file.path.startsWith('本文/') && file.path.endsWith('.yaml') && !file.path.includes('/cache/') && !file.path.includes('/.cache/')).path;
    const publishedMarker = '転送で公開した後続世代の本文です。';
    const replacementFiles = novel.files.map(file => {
      let bytes = baseline.files.get(`novels/0/${file.path}`);
      if (file.path === bodyPath) {
        assert(bytes.includes(Buffer.from('初回取得の本文です。')), 'replace actual serialized section body bytes');
        bytes = Buffer.from(bytes.toString().replaceAll('初回取得の本文です。', publishedMarker));
      }
      return { path: file.path, bytes };
    });
    sourceGeneration = 2;
    sourceImageCount = 500;
    sourceRequests.clear(); imageRequests.clear();
    const gate = { entered: Promise.withResolvers(), release: Promise.withResolvers() };
    yieldBarrier = gate;
    try {
      const accepted = await (await api('/api/download', { targets: [target], force: true })).json();
      assert(accepted.success, JSON.stringify(accepted));
      const jobId = accepted.results[0].job_id;
      await bounded(gate.entered.promise, 'stale-origin continuation admission');
      const checkpoint = JSON.parse(scalar(`SELECT checkpoint_json FROM worker_jobs WHERE job_id=${quote(jobId)}`));
      assert(checkpoint.write_origin, 'the real completed-prefix checkpoint must contain its original fence');
      const requestsBefore = [...sourceRequests.entries()];
      const imagesBefore = [...imageRequests.entries()];
      const imported = await client.importData([{ ...novel, files: replacementFiles }], { policy: 'overwrite' });
      assert.equal(imported.status.state, 'completed');
      assert.equal(imported.plan[0].action, 'overwrite');
      const afterPublication = await client.exportData([id]);
      assert(afterPublication.files.get(`novels/0/${bodyPath}`).includes(Buffer.from(publishedMarker)));

      gate.release.resolve();
      const rejected = await waitForAcquisition(jobId, true);
      assert.match(rejected.last_error ?? '', /novel|epoch|origin/i);
      const afterContinuation = await client.exportData([id]);
      assert.deepEqual(afterContinuation.files.get(`novels/0/${bodyPath}`), replacementFiles.find(file => file.path === bodyPath).bytes,
        'the stale continuation must not replace the later published section bytes');
      assert.deepEqual([...sourceRequests.entries()], requestsBefore, 'origin rejection occurs before another upstream TOC/episode fetch');
      assert.deepEqual([...imageRequests.entries()], imagesBefore, 'origin rejection must not continue the old illustration loop');
      assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='convert'"), 1, 'only the initial successful download may enqueue conversion');
      const cleared = await (await api('/api/queue/cancel', {})).json();
      assert(cleared.success, JSON.stringify(cleared));
    } finally {
      gate.release.resolve();
      yieldBarrier = null;
    }
    console.log('PASS real persisted continuation rejects its old writer origin after v2 publishes replacement bytes');
  }

  for (const scenario of [
    { name: 'cancel-before-toc', episodes: 3, pauseToc: true },
    { name: 'cancel-between-episodes', episodes: 3 },
    { name: 'cancel-final-episode', episodes: 1 },
    { name: 'cancel-between-images', episodes: 1, images: 3 },
    { name: 'cancel-expired-live-invocation', episodes: 1, images: 3, revokeExpired: true },
    { name: 'clear-running-and-waiting', episodes: 3, clearAll: true },
  ]) {
    await defaultLibrary(scenario.name, { required: true, ...(scenario.revokeExpired ? { entrypoint: wrapper } : {}) });
    await installDefaultSource();
    sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
    const objectSnapshotSql = "SELECT object_key,size,encoding,crc32,hex(data) AS payload FROM objects ORDER BY object_key";
    const unrelatedObjects = sql(objectSnapshotSql)[0].results;
    sourceEpisodeCount = scenario.episodes;
    sourceImageCount = scenario.images || 0;
    sourceRequests.clear(); imageRequests.clear();
    const entered = Promise.withResolvers();
    const release = Promise.withResolvers();
    const pausePath = scenario.pauseToc ? '/smoke-novel/' : scenario.images ? '/smoke-image/0.png' : '/smoke-novel/1/';
    const revocationWitness = scenario.revokeExpired ? { ids: [], expected: 1, completed: Promise.withResolvers() } : null;
    if (revocationWitness) deliveryWitness = revocationWitness;
    sourcePause = async (path) => {
      if (path !== pausePath) return;
      sourcePause = null;
      entered.resolve();
      await release.promise;
    };
    try {
      const accepted = await (await api('/api/download', { targets: [target] })).json();
      assert(accepted.success, JSON.stringify(accepted));
      const jobId = accepted.results[0].job_id;
      await Promise.race([entered.promise, new Promise((_, reject) => setTimeout(() => reject(new Error(`source gate timed out: ${scenario.name}`)), 30000).unref())]);
      const earlyRow = sql(`SELECT checkpoint_json,status,execution_token FROM worker_jobs WHERE job_id=${quote(jobId)}`)[0].results[0];
      if (scenario.pauseToc) {
        // The first /smoke-novel/ request is the unowned URL-resolution probe:
        // canonical admission only allocates the durable reservation after that
        // probe resolves the target, so nothing may be claimed or published
        // while the probe is still outstanding.
        assert.equal(earlyRow.status, 'running');
        assert(earlyRow.execution_token, 'the live execution must own its claim before any admission');
        assert.equal(earlyRow.checkpoint_json, null, 'a pre-allocation resolution probe must not be answered with a durable claim already attached');
        assert.equal(scalar('SELECT COUNT(*) FROM novels'), 0);
        assert.equal(scalar('SELECT COUNT(*) FROM content_objects'), 0, 'a pre-allocation probe cannot publish canonical body bytes');
      } else {
        // Every other pause point is an owned upstream fetch: the exact
        // numeric reservation and writer origin must already be durable or a
        // lost execution would leak an unclaimed ID or stale producer epoch.
        assert.equal(earlyRow.status, 'running');
        assert(earlyRow.execution_token, 'the live execution must own its claim');
        const early = JSON.parse(earlyRow.checkpoint_json);
        assert(Array.isArray(early.creation_reservation) && early.creation_reservation.length === 2 && early.creation_reservation.every(Number.isSafeInteger), 'the allocator must durably associate its exact ID/generation with the live job BEFORE the first owned upstream fetch');
        assert(early.write_origin && typeof early.write_origin.prefix === 'string' && Number.isSafeInteger(early.write_origin.epoch), 'the original namespace epoch must be durable before the first owned upstream fetch');
        assert.equal(early.phase, 'planned');
        assert.equal(early.next_section_index, null);
        assert.equal(scalar(`SELECT COUNT(*) FROM novel_id_reservations WHERE job_id=${quote(jobId)} AND available=0 AND id=${early.creation_reservation[0]} AND generation=${early.creation_reservation[1]}`), 1, 'the exact reserved ID/generation must be owned by this live job');
      }
      if (scenario.clearAll) {
        const waiting = await (await api('/api/download', { targets: [target], force: true })).json();
        assert(waiting.success, JSON.stringify(waiting));
        assert.notEqual(waiting.results[0].job_id, jobId);
        assert(scalar("SELECT COUNT(*) FROM worker_jobs WHERE status IN ('pending','retryable')") > 0);
      }
      const cancellation = await (await api(
        scenario.clearAll ? '/api/queue/cancel' : '/api/cancel_running_task',
        scenario.clearAll ? {} : { task_id: jobId },
      )).json();
      assert(scenario.clearAll ? cancellation.success : cancellation.status === 'ok', JSON.stringify(cancellation));
      if (scenario.revokeExpired) {
        const held = sql(`SELECT status,execution_token,checkpoint_json FROM worker_jobs WHERE job_id=${quote(jobId)}`)[0].results[0];
        assert.equal(held.status, 'permanent');
        assert(held.execution_token, 'logical cancellation must not pretend the held physical fetch has stopped');
        assert.equal(scalar("SELECT COUNT(*) FROM content_objects WHERE instr(object_key,'/cache/')=0"), 1,
          'ordinary cancellation retains the body committed before the held image; explicit revocation may drain it');
        const checkpoint = JSON.parse(held.checkpoint_json);
        const origin = checkpoint.write_origin;
        assert(origin, 'the exact original epoch must remain durable until atomic revocation');
        sql(`UPDATE worker_jobs SET lease_until='2000-01-01T00:00:00.000000000Z' WHERE job_id=${quote(jobId)} AND execution_token=${quote(held.execution_token)}`);
        // The source response is STILL awaiting release.promise. Expiry is a
        // revocation condition, never evidence that its physical owner died.
        const revoked = await (await api('/api/queue/clear', {})).json();
        assert(revoked.success, JSON.stringify(revoked));
        const settled = sql(`SELECT execution_token,checkpoint_json FROM worker_jobs WHERE job_id=${quote(jobId)}`)[0].results[0];
        assert.equal(settled.execution_token, null);
        if (settled.checkpoint_json !== null) {
          const pending = JSON.parse(settled.checkpoint_json);
          assert.deepEqual(pending.creation_reservation, checkpoint.creation_reservation, 'bounded cleanup retains the original numeric generation');
          assert.deepEqual(pending.write_origin, origin, 'bounded cleanup must not reauthenticate a revoked producer');
        }
        const prefix = origin.prefix.replace(/\/$/, '');
        const gate = sql(`SELECT epoch,closed,owner,origin_url_fold FROM novel_access_gates WHERE prefix=${quote(prefix)}`)[0].results[0];
        assert.equal(gate.epoch, origin.epoch + 1, 'the fenced producer is superseded by exactly one epoch while its real HTTP fetch is still alive');
        if (gate.closed === 1) {
          assert(settled.checkpoint_json, 'an unfinished drain must retain durable reservation proof');
          assert(gate.owner, 'a closed namespace must retain its removal owner');
          const [id, generation] = checkpoint.creation_reservation;
          assert.equal(scalar(`SELECT COUNT(*) FROM novel_id_reservations WHERE id=${id} AND generation=${generation} AND available=0 AND job_id=${quote(jobId)}`), 1,
            'the numeric identity cannot be released before its closed namespace is drained');
        } else {
          assert.equal(gate.closed, 0);
          assert.equal(gate.owner, null, 'only a completed namespace drain may reopen admission');
          assert.equal(gate.origin_url_fold ?? '', '', 'reopened empty namespaces must clear their old origin');
          assert.equal(scalar("SELECT COUNT(*) FROM content_objects"), 0, 'reopened admission requires all unpublished canonical bytes to be drained');
          assert.deepEqual(sql(objectSnapshotSql)[0].results, unrelatedObjects, 'a completed drain preserves unrelated site definitions');
        }
        if (settled.checkpoint_json === null) {
          assert.equal(gate.closed, 0, 'checkpoint release cannot precede reopening the drained namespace');
          assert.equal(scalar("SELECT COUNT(*) FROM novel_namespace_claims"), 0);
          assert.equal(scalar("SELECT COUNT(*) FROM novel_id_reservations WHERE available=0"), 0);
        }
        assert.equal(imageRequests.get('/smoke-image/0.png'), 1);
      }
      // Only bytes committed before the cancellation tombstone are retained:
      // the held response lands after the tombstone, so a still-paused section
      // body was never durably accepted, while a body committed before the
      // pause survives until fenced terminal cleanup drains the namespace.
      // Explicit revocation can make bounded cleanup progress while the
      // source is still held; ordinary cancellation must retain prior commits.
      if (!scenario.revokeExpired) {
        assert.equal(scalar("SELECT COUNT(*) FROM content_objects WHERE instr(object_key,'/cache/')=0"),
          scenario.images ? 1 : 0,
          'pre-cancellation commits are retained through cancellation; bytes still in flight are never durably accepted');
      }
      release.resolve();
      if (revocationWitness) await bounded(revocationWitness.completed.promise, 'revoked physical delivery settlement');
      if (revocationWitness) assert.deepEqual([...imageRequests.keys()], ['/smoke-image/0.png'], 'revoking a live expired invocation must prevent any later image fetch');
      const deadline = Date.now() + 60000;
      let job;
      do {
        job = sql(`SELECT status,last_error,execution_token FROM worker_jobs WHERE job_id=${quote(jobId)}`)[0].results[0];
        if (job.execution_token === null) break;
        await new Promise((resolve) => setTimeout(resolve, 100));
      } while (Date.now() < deadline);
      assert.equal(job.execution_token, null, JSON.stringify(job));
      assert.equal(job.status, 'permanent');
      assert.equal(job.last_error, 'cancelled by user via Web UI');
      assert.equal(sourceRequests.get('/smoke-novel/1/') || 0, scenario.pauseToc ? 0 : 1);
      assert.equal(sourceRequests.get('/smoke-novel/2/') || 0, 0, 'cancellation must stop subsequent episodes');
      assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='convert'"), 0, 'a canceled acquisition must not enqueue a new conversion');
      assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE status IN ('pending','running','retryable')"), 0, 'bulk clearing must not wake the former waiting queue');
      const queueStatus = await (await api('/api/queue/status')).json();
      assert.equal(queueStatus.failed, 0, 'intentional interruption and cleared waiting work must not appear as failures');
      assert.equal(queueStatus.cancelled, 1, 'the stopped running execution must be counted as canceled');
      if (scenario.clearAll) {
        sql("INSERT INTO worker_jobs(job_id,kind,target,options,status,dedupe_key,attempts,last_error,created_at,updated_at) VALUES ('ordinary-failure','download','unavailable','[]','permanent','ordinary-failure',1,'HTTP 503','2026-11-01T00:00:00Z','2026-11-01T00:00:00Z');");
        const withFailure = await (await api('/api/queue/status')).json();
        assert.equal(withFailure.failed, 1, 'unrelated failures must remain visible alongside canceled executions');
        assert.equal(withFailure.cancelled, 1);
      }
      if (scenario.images && !scenario.revokeExpired) {
        assert.deepEqual([...imageRequests.keys()], ['/smoke-image/0.png'], 'cancellation must stop subsequent images');
        assert.equal(scalar("SELECT COUNT(*) FROM objects WHERE object_key LIKE '%/.illustration_cache.yaml'"), 0,
          'an image still in flight at cancellation is never durably committed, so no saved mapping can exist');
      }
      await settleCreation(jobId);
      // Full fenced cleanup leaves nothing behind for an unpublished creation:
      // no owned numeric reservation, namespace claim, publication receipt,
      // durable checkpoint, record, or stored object survives settlement.
      assert.equal(scalar("SELECT COUNT(*) FROM novel_id_reservations WHERE available=0"), 0,
        'settlement must release the owned numeric ID');
      assert.equal(scalar("SELECT COUNT(*) FROM novel_namespace_claims"), 0,
        'settlement must release the namespace claim');
      assert.equal(scalar("SELECT COUNT(*) FROM novel_creation_receipts"), 0,
        'settlement must release the publication receipt');
      assert.equal(scalar("SELECT COUNT(*) FROM novels"), 0,
        'an unpublished creation must not leave a record');
      assert.equal(scalar("SELECT COUNT(*) FROM content_objects"), 0,
        'settlement drains every unpublished canonical object');
      assert.deepEqual(sql(objectSnapshotSql)[0].results, unrelatedObjects,
        'settlement removes creation-owned objects but preserves the saved site definition and other unrelated data');
      console.log(`PASS ${scenario.name}: durable cancellation, no later fetches or follow-up conversion, full fenced reservation cleanup`);
    } finally {
      sourcePause = null;
      if (revocationWitness) deliveryWitness = null;
      release.resolve();
    }
  }

  sourceImageCount = 0;
  await defaultLibrary('long-resume', { required: true });
  const longTarget = target.replace('/smoke-novel/', '/smoke-long/');
  const longDefinition = definition.replaceAll('smoke-novel', 'smoke-long')
    + "\ntoc_page_max_pattern: '<pages>(\\d+)</pages>'\nnext_toc: '<next>(?<next_path>[^<]+)</next>'\nnext_url: '\\k<top_url>\\k<next_path>'\n";
  assert((await (await api('/api/sites/localhost.yaml', longDefinition, 'PUT')).json()).success);
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
  const longAccepted = await (await api('/api/download', { targets: [longTarget] })).json();
  assert(longAccepted.success, JSON.stringify(longAccepted));
  await waitForAcquisition(longAccepted.results[0].job_id, false, 600000);
  assert.equal(longEpisodeRequests.size, longEpisodeCount);
  assert([...longEpisodeRequests.values()].every((count) => count === 1), 'completed episodes must never be refetched across continuations');
  assert(longInvocationStarts.some((saved) => saved > 850), 'a fresh invocation must progress beyond the prefix that exceeds the real request budget');
  const longJob = sql("SELECT attempts,status FROM worker_jobs WHERE kind='download'")[0].results[0];
  assert.equal(longJob.status, 'succeeded');
  assert.equal(longJob.attempts, 0);
  assert.equal(scalar("SELECT general_all_no FROM novels WHERE ncode='smoke-long'"), longEpisodeCount);
  assert.equal(scalar("SELECT COUNT(*) FROM content_objects WHERE object_key LIKE '%/smoke-long/本文/%' AND instr(object_key,'/cache/')=0"), longEpisodeCount);
  assert.equal(scalar("SELECT COUNT(*) FROM objects WHERE object_key LIKE '%/.download_resume'"), 0, 'completion retires transient resume metadata');
  const longId = scalar("SELECT id FROM novels WHERE ncode='smoke-long'");
  const longEpub = await api(`/api/novels/${longId}/download.epub`);
  assert.equal(longEpub.status, 200, await longEpub.clone().text());
  const longText = epubText(Buffer.from(await longEpub.arrayBuffer()));
  for (const index of [1, 771, 850, 1685]) assert(longText.includes(longEpisodeBody(index)), `EPUB must include episode ${index}`);
  console.log('PASS 1685 episodes / 17 TOC pages cross the real Worker budget, resume without rereading the complete prefix, and produce the complete EPUB');

  await defaultLibrary('automatic-update-rules');
  const automaticDefinition = multiTargetDefinition
    + "\nupdate_rules:\n  - match: {ncode: '^smoke-single$'}\n    automatic: skip\n";
  assert((await (await api('/api/sites/localhost.yaml', automaticDefinition, 'PUT')).json()).success);
  const singleTarget = target.replace('/smoke-novel/', '/smoke-single/');
  const seriesTarget = target.replace('/smoke-novel/', '/smoke-series/');
  await download(singleTarget, false); // Initial acquisition bypasses automatic policy.
  const singleId = scalar("SELECT id FROM novels WHERE ncode='smoke-single'");
  sql(Array.from({ length: 99 }, (_, i) =>
    `INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,ncode,ncode_fold,domain)
     VALUES (${1000 + i},'Writer','writer','A single','a single','single',${quote(`${singleTarget}?copy=${i}`)},${quote(`${singleTarget}?copy=${i}`)},'Controlled Source','controlled source','2026-10-31T00:00:00Z','smoke-single','smoke-single','localhost');`).join('')
    + 'UPDATE novel_id_sequence SET next_id=2000;');
  await download(seriesTarget, false);
  const seriesId = scalar("SELECT id FROM novels WHERE ncode='smoke-series'");
  assert.equal(seriesId, 2000);
  const settingsSaved = await (await api('/api/global_setting', { settings: {
    'update.auto-schedule.enable': true, 'update.auto-schedule': '0000',
    'update.auto-schedule.timezone': 'UTC', 'update.disabled-author-tracking': true,
  } })).json();
  assert(settingsSaved.success, JSON.stringify(settingsSaved));
  const checkpoint = () => JSON.parse(scalar("SELECT value_json FROM app_state WHERE scope='scheduler' AND key='auto_update'"));
  const expirePlannerLease = () => sql("UPDATE app_state SET value_yaml=json_set(value_json, '$.planner_lease_until', '2000-01-01T00:00:00Z'), value_json=json_set(value_json, '$.planner_lease_until', '2000-01-01T00:00:00Z') WHERE scope='scheduler' AND key='auto_update';");
  const singleRequests = sourceRequests.get('/smoke-single/');
  const seriesRequests = sourceRequests.get('/smoke-series/');
  await tick();
  assert.equal(checkpoint().cursor, 1098, 'a completely excluded full page must advance its original ID cursor');
  assert.equal(checkpoint().state, 'running');
  assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='update'"), 0);
  assert.equal(sourceRequests.get('/smoke-single/'), singleRequests);
  expirePlannerLease();
  await tick();
  assert.equal(checkpoint().state, 'done');
  const scheduled = sql("SELECT job_id,target FROM worker_jobs WHERE kind='update'")[0].results;
  assert.deepEqual(scheduled.map((job) => job.target), [String(seriesId)]);
  await waitForAcquisition(scheduled[0].job_id);
  assert.equal(sourceRequests.get('/smoke-single/'), singleRequests);
  assert(sourceRequests.get('/smoke-series/') > seriesRequests);

  // The sorted-offset scan must also consume all-excluded pages and finish.
  const skipAll = automaticDefinition.replace("'^smoke-single$'", "'.*'");
  assert((await (await api('/api/sites/localhost.yaml', skipAll, 'PUT')).json()).success);
  assert((await (await api('/api/global_setting', { settings: { 'update.sort-by': 'title' } })).json()).success);
  sql("DELETE FROM app_state WHERE scope='scheduler' AND key='auto_update';");
  const jobCount = scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='update'");
  await tick();
  assert.equal(checkpoint().cursor, 100);
  assert.equal(checkpoint().state, 'running');
  expirePlannerLease();
  await tick();
  assert.equal(checkpoint().state, 'done');
  assert.equal(scalar("SELECT COUNT(*) FROM worker_jobs WHERE kind='update'"), jobCount);

  assert((await (await api('/api/sites/localhost.yaml', automaticDefinition, 'PUT')).json()).success);
  sql('DELETE FROM novels WHERE id BETWEEN 1000 AND 1098;');
  for (const body of [{ targets: [String(singleId)], force: true }, { update_all: true, force: true }]) {
    const before = sourceRequests.get('/smoke-single/');
    const accepted = await (await api('/api/update', body)).json();
    assert(accepted.success, JSON.stringify(accepted));
    assert.equal(accepted.count, body.update_all ? 2 : 1);
    for (const jobId of accepted.job_ids) {
      await waitForAcquisition(jobId);
    }
    assert(sourceRequests.get('/smoke-single/') > before, 'explicit individual and bulk updates must still fetch excluded singles');
  }
  console.log('PASS site rules skip automatic singles, advance excluded ID/offset pages, hot-reload, and preserve initial/manual acquisition');
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
