// Worker の可搬アーカイブ API (`/api/transfer/export` / `import`) の契約テスト。
//
// 先に Worker をビルドしておく (このファイルはビルドしない):
//
//   worker-build            # または node tests/run.mjs (run.mjs が全部やる)
//   node tests/transfer.mjs
//
// 独立した D1 (miniflare SQLite) を 2 つ用意し、1 つ目へ取り込んだ小説を
// 書き出し、その ZIP を 2 つ目の空 DB へ取り込む往復を検証する。DB は
// `.wrangler/transfer-smoke-*` に作り、終了時に削除する。
//
// 検証すること:
//   * import (create): ZIP → D1 レコード + オブジェクト (toc.yaml / 本文 /
//     setting.ini / replace.txt / converter.yaml / 挿絵)
//   * export: manifest.json のメタデータ・ファイル一覧・SHA-256 と実体
//   * 別 DB への import と再 export (同じ相対パス・同じ内容)
//   * overwrite: レコード ID と保存先を維持し、アーカイブに無いファイルを
//     消さない / 空のメタデータだけ埋める
//   * 認証 (401) / Content-Type (415) / 存在しない ID (404) / 壊れた ZIP (400)
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inflateRawSync } from 'node:zlib';
import process from 'node:process';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
if (!existsSync(join(root, 'build', 'worker', 'shim.mjs'))) {
  console.error('build/worker/shim.mjs is missing; run `worker-build` (or node tests/run.mjs) first');
  process.exit(1);
}
mkdirSync(join(root, '.wrangler'), { recursive: true });
const work = mkdtempSync(join(root, '.wrangler', 'transfer-smoke-'));
const token = 'isolated-transfer-smoke-token';
const windows = process.platform === 'win32';
const npx = windows ? 'npx.cmd' : 'npx';
// Same pinned Wrangler as `tests/run.mjs` (and the deploy CI).
const WRANGLER_VERSION = process.env.NAROU_WRANGLER_VERSION || '4.151.3';
if (!/^\d+\.\d+\.\d+$/.test(WRANGLER_VERSION)) {
  throw new Error(`NAROU_WRANGLER_VERSION must be exact x.y.z: ${WRANGLER_VERSION}`);
}
const WRANGLER = `wrangler@${WRANGLER_VERSION}`;
const devVars = `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32, 7).toString('base64')}\nS3_ACCESS_KEY_ID=local-test-key\nS3_SECRET_ACCESS_KEY=local-test-secret\n`;

// ---------------------------------------------------------------------------
// ZIP (ストアのみで作り、読み出しはストア/Deflate の両対応)
// ---------------------------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let index = 0; index < 256; index += 1) {
    let value = index;
    for (let bit = 0; bit < 8; bit += 1) {
      value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
    }
    table[index] = value >>> 0;
  }
  return table;
})();

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function sha256(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

function buildZip(entries) {
  const locals = [];
  const central = [];
  let offset = 0;
  for (const entry of entries) {
    const name = Buffer.from(entry.name, 'utf8');
    const data = Buffer.isBuffer(entry.data) ? entry.data : Buffer.from(entry.data, 'utf8');
    const crc = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6); // UTF-8 names
    local.writeUInt16LE(0, 8); // stored
    local.writeUInt16LE(0, 10);
    local.writeUInt16LE(0x21, 12); // 1980-01-01
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    local.writeUInt16LE(0, 28);
    const header = Buffer.concat([local, name, data]);

    const directory = Buffer.alloc(46);
    directory.writeUInt32LE(0x02014b50, 0);
    directory.writeUInt16LE(20, 4);
    directory.writeUInt16LE(20, 6);
    directory.writeUInt16LE(0x0800, 8);
    directory.writeUInt16LE(0, 10);
    directory.writeUInt16LE(0, 12);
    directory.writeUInt16LE(0x21, 14);
    directory.writeUInt32LE(crc, 16);
    directory.writeUInt32LE(data.length, 20);
    directory.writeUInt32LE(data.length, 24);
    directory.writeUInt16LE(name.length, 28);
    directory.writeUInt16LE(0, 30);
    directory.writeUInt16LE(0, 32);
    directory.writeUInt16LE(0, 34);
    directory.writeUInt16LE(0, 36);
    directory.writeUInt32LE(0, 38);
    directory.writeUInt32LE(offset, 42);
    central.push(Buffer.concat([directory, name]));
    locals.push(header);
    offset += header.length;
  }
  const centralBytes = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralBytes.length, 12);
  end.writeUInt32LE(offset, 16);
  end.writeUInt16LE(0, 20);
  return Buffer.concat([...locals, centralBytes, end]);
}

function readZip(buffer) {
  const end = buffer.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  assert(end >= 0, 'zip end-of-central-directory not found');
  const count = buffer.readUInt16LE(end + 10);
  let offset = buffer.readUInt32LE(end + 16);
  const entries = new Map();
  for (let index = 0; index < count; index += 1) {
    assert.equal(buffer.readUInt32LE(offset), 0x02014b50, 'central directory signature');
    const method = buffer.readUInt16LE(offset + 10);
    const crc = buffer.readUInt32LE(offset + 16);
    const compressedSize = buffer.readUInt32LE(offset + 20);
    const size = buffer.readUInt32LE(offset + 24);
    const nameLength = buffer.readUInt16LE(offset + 28);
    const extraLength = buffer.readUInt16LE(offset + 30);
    const commentLength = buffer.readUInt16LE(offset + 32);
    const localOffset = buffer.readUInt32LE(offset + 42);
    const name = buffer.toString('utf8', offset + 46, offset + 46 + nameLength);
    const dataStart =
      localOffset +
      30 +
      buffer.readUInt16LE(localOffset + 26) +
      buffer.readUInt16LE(localOffset + 28);
    const raw = buffer.subarray(dataStart, dataStart + compressedSize);
    const data = method === 0 ? Buffer.from(raw) : inflateRawSync(raw);
    assert.equal(data.length, size, `${name}: uncompressed size`);
    assert.equal(crc32(data), crc, `${name}: crc`);
    entries.set(name, data);
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return entries;
}

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

const TOC_URL = 'https://example.com/n1234ab/';
const TITLE = '転生したらテストだった';
const AUTHOR = '作者名';
const SITENAME = 'Example Site';
const DOMAIN = 'example.com';
// 1x1 の透過 PNG (挿絵は別ストアへ振り分けられる経路も通す)。
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  'base64',
);

function tocYaml(story, subtitles) {
  const lines = ['---', `toc_url: ${TOC_URL}`, `title: ${TITLE}`, `author: ${AUTHOR}`, 'story: |-', `  ${story}`, 'subtitles:'];
  for (const subtitle of subtitles) {
    lines.push(
      `- index: '${subtitle.index}'`,
      `  href: ${TOC_URL}${subtitle.index}/`,
      "  chapter: ''",
      "  subchapter: ''",
      `  subtitle: ${subtitle.name}`,
      `  file_subtitle: ${subtitle.name}`,
      "  subdate: '2026-01-02 03:04:05'",
    );
  }
  lines.push('novel_type: 1', '');
  return lines.join('\n');
}

function sectionYaml(index, name, body) {
  return [
    '---',
    `index: '${index}'`,
    `href: ${TOC_URL}${index}/`,
    "chapter: ''",
    "subchapter: ''",
    `subtitle: ${name}`,
    `file_subtitle: ${name}`,
    "subdate: '2026-01-02 03:04:05'",
    'element:',
    '  data_type: text',
    "  introduction: ''",
    "  postscript: ''",
    '  body: |-',
    `    ${body}`,
    '',
  ].join('\n');
}

/// `files` は小説ディレクトリからの相対パス。
function archiveFor(files) {
  const manifest = {
    format_version: 1,
    app_version: '0.5.0',
    exported_at: '2026-10-24T00:00:00Z',
    novels: [
      {
        toc_url: TOC_URL,
        title: TITLE,
        author: AUTHOR,
        sitename: SITENAME,
        domain: DOMAIN,
        tags: ['テスト'],
        novel_type: 1,
        ncode: 'n1234ab',
        files: files.map((file) => ({
          path: file.path,
          size: file.data.length,
          sha256: sha256(file.data),
        })),
      },
    ],
  };
  return buildZip([
    { name: 'manifest.json', data: JSON.stringify(manifest) },
    ...files.map((file) => ({ name: `novels/0/${file.path}`, data: file.data })),
  ]);
}

const firstSectionPath = '本文/1 第一話.yaml';
const secondSectionPath = '本文/2 第二話.yaml';

const fixtureA = [
  { path: 'toc.yaml', data: Buffer.from(tocYaml('テスト用のあらすじ', [{ index: '1', name: '第一話' }])) },
  { path: firstSectionPath, data: Buffer.from(sectionYaml('1', '第一話', '最初の本文です。')) },
  { path: 'setting.ini', data: Buffer.from('[global]\nenable_yokogaki=false\n') },
  { path: 'replace.txt', data: Buffer.from('元の置換設定\n') },
  { path: 'converter.yaml', data: Buffer.from('{}\n') },
  { path: '挿絵/001.png', data: PNG },
];
const zipA = archiveFor(fixtureA);

// overwrite 用: 本文と目次を差し替え、replace.txt と挿絵を**含めない**
// (アーカイブに無いファイルは消さないことの確認)。
const fixtureB = [
  {
    path: 'toc.yaml',
    data: Buffer.from(
      tocYaml('更新後のあらすじ', [
        { index: '1', name: '第一話' },
        { index: '2', name: '第二話' },
      ]),
    ),
  },
  { path: firstSectionPath, data: Buffer.from(sectionYaml('1', '第一話', '更新後の本文です。')) },
  { path: secondSectionPath, data: Buffer.from(sectionYaml('2', '第二話', '追加された本文です。')) },
];
const zipB = archiveFor(fixtureB);

// ---------------------------------------------------------------------------
// Worker の起動 / 停止
// ---------------------------------------------------------------------------

function runWrangler(args, phase) {
  const result = spawnSync(npx, [WRANGLER, ...args, '--config', phase.config, '--persist-to', phase.persist], {
    cwd: root,
    shell: windows,
    encoding: 'utf8',
    env: { ...process.env, CI: 'true', WRANGLER_SEND_METRICS: 'false' },
  });
  if (result.status !== 0) {
    throw new Error(`wrangler ${args.join(' ')} failed: ${result.stderr}\n${result.stdout}`);
  }
  return result.stdout;
}

function preparePhase(name) {
  const directory = join(work, name);
  mkdirSync(directory, { recursive: true });
  let toml = readFileSync(join(root, 'wrangler.toml'), 'utf8').replace(/\[build\][^\[]*/, '');
  toml = toml
    .replace(/^main = .*$/m, `main = ${JSON.stringify(join(root, 'build', 'worker', 'shim.mjs').replaceAll('\\', '/'))}`)
    .replace(/^directory = "public"$/m, `directory = ${JSON.stringify(join(root, 'public').replaceAll('\\', '/'))}`)
    .replace(/^migrations_dir = "migrations"$/m, `migrations_dir = ${JSON.stringify(join(root, 'migrations').replaceAll('\\', '/'))}`);
  const config = join(directory, 'wrangler.toml');
  writeFileSync(config, toml);
  writeFileSync(join(directory, '.dev.vars'), devVars);
  const phase = { config, persist: join(directory, 'state'), directory };
  runWrangler(['d1', 'migrations', 'apply', 'DB', '--local'], phase);
  // The contract test intentionally runs legacy D1 (no MinIO): keep bodies in
  // D1 instead of the S3 default.
  const seed = join(directory, 'content-backend.sql');
  writeFileSync(
    seed,
    `INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','"d1"','d1') ON CONFLICT(scope,key) DO NOTHING;`,
  );
  runWrangler(['d1', 'execute', 'DB', '--local', '--file', seed], phase);
  return phase;
}

async function freePort() {
  const probe = await import('node:net').then((net) => net.createServer());
  probe.listen(0, '127.0.0.1');
  await once(probe, 'listening');
  const port = probe.address().port;
  await new Promise((resolve) => probe.close(resolve));
  return port;
}

async function startWorker(phase) {
  const port = await freePort();
  const child = spawn(
    npx,
    [WRANGLER, 'dev', '--config', phase.config, '--persist-to', phase.persist, '--port', String(port), '--test-scheduled'],
    {
      cwd: root,
      shell: windows,
      detached: !windows,
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, CI: 'true', WRAPPER_SEND_METRICS: 'false', WRANGLER_SEND_METRICS: 'false' },
    },
  );
  let logs = '';
  for (const stream of [child.stdout, child.stderr]) {
    stream.on('data', (chunk) => {
      logs = (logs + chunk).slice(-100000);
    });
  }
  const base = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`worker exited: ${logs}`);
    try {
      if ((await fetch(`${base}/health/live`, { headers: { Connection: 'close' } })).ok) {
        return { base, child, logs: () => logs };
      }
    } catch {
      // not up yet
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`worker did not become ready: ${logs}`);
}

async function stopWorker(worker) {
  if (!worker || worker.child.exitCode !== null) return;
  const exited = once(worker.child, 'exit');
  if (windows) {
    spawnSync('taskkill', ['/pid', String(worker.child.pid), '/T', '/F'], { stdio: 'ignore' });
  } else {
    try {
      process.kill(-worker.child.pid, 'SIGTERM');
    } catch (error) {
      if (error.code !== 'ESRCH') throw error;
    }
  }
  await exited;
}

// ---------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------

function client(base) {
  const auth = { Authorization: `Bearer ${token}`, Connection: 'close' };
  return {
    async request(path, init = {}) {
      return fetch(`${base}${path}`, { ...init, headers: { ...auth, ...(init.headers ?? {}) } });
    },
    async json(path, init) {
      const response = await this.request(path, init);
      const body = await response.json();
      return { response, body };
    },
    async anonymous(path, init = {}) {
      return fetch(`${base}${path}`, { ...init, headers: { Connection: 'close', ...(init.headers ?? {}) } });
    },
    async upload(zip, query = '') {
      return this.json(`/api/transfer/import${query}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/zip' },
        body: zip,
      });
    },
    async exportZip(ids) {
      const response = await this.request(`/api/transfer/export?ids=${ids}`);
      assert.equal(response.status, 200, await response.clone().text());
      return { response, bytes: Buffer.from(await response.arrayBuffer()) };
    },
  };
}

function manifestOf(bytes) {
  const entries = readZip(bytes);
  const manifest = JSON.parse(entries.get('manifest.json').toString('utf8'));
  return { entries, manifest };
}

function filesFrom(manifest, index = 0) {
  return manifest.novels[index].files.map((file) => ({
    path: file.path,
    size: file.size,
    sha256: file.sha256,
  }));
}

// ---------------------------------------------------------------------------
// scenario
// ---------------------------------------------------------------------------

let phaseA;
let phaseB;
let workerA;
let workerB;
let exitCode = 1;
try {
  // --- DB A: import → export ------------------------------------------------
  phaseA = preparePhase('a');
  workerA = await startWorker(phaseA);
  console.log(`worker A ready on ${workerA.base}`);
  const a = client(workerA.base);

  // 未認証は 401 (export / import とも)。
  assert.equal((await a.anonymous('/api/transfer/export?ids=1')).status, 401);
  assert.equal(
    (await a.anonymous('/api/transfer/import', { method: 'POST', headers: { 'Content-Type': 'application/zip' }, body: zipA }))
      .status,
    401,
  );

  // Content-Type が ZIP でなければ 415。
  const wrongType = await a.json('/api/transfer/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ hello: 'world' }),
  });
  assert.equal(wrongType.response.status, 415, JSON.stringify(wrongType.body));
  assert.equal(wrongType.body.success, false);
  assert.equal(wrongType.body.errors.length, 1);

  // 壊れた ZIP は 400 (リクエスト単位の失敗)。
  const broken = await a.json('/api/transfer/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/zip' },
    body: Buffer.from('this is not a zip'),
  });
  assert.equal(broken.response.status, 400, JSON.stringify(broken.body));
  assert.equal(broken.body.success, false);
  assert.equal(broken.body.created, 0);

  // 取り込み (create)。
  const created = await a.upload(zipA);
  assert.equal(created.response.status, 200, JSON.stringify(created.body));
  assert.deepEqual(created.body, { success: true, created: 1, overwritten: 0, skipped: 0, errors: [] });

  const list = await a.json('/api/novels');
  assert.equal(list.body.records_total, 1, JSON.stringify(list.body));
  const id = list.body.data[0].id;
  const record = await a.json(`/api/novels/${id}`);
  assert.equal(record.body.title, TITLE);
  assert.equal(record.body.author, AUTHOR);
  assert.equal(record.body.sitename, SITENAME);
  assert.equal(record.body.toc_url, TOC_URL);
  assert.equal(record.body.file_title, 'n1234ab');
  assert.equal(record.body.ncode, 'n1234ab');
  assert.equal(record.body.domain, DOMAIN);
  assert.equal(record.body.novel_type, 1);
  assert.deepEqual(record.body.tags, ['テスト']);
  assert.equal(record.body.general_all_no, 1);
  assert.equal(record.body.use_subdirectory, false);
  console.log('PASS import creates the record and objects; unauthenticated / wrong-type / broken-zip are rejected');

  // 同じ ZIP の再取り込みは skip。
  const skipped = await a.upload(zipA);
  assert.deepEqual(skipped.body, { success: true, created: 0, overwritten: 0, skipped: 1, errors: [] });

  // export: ヘッダと ZIP の中身。
  const exportedA = await a.exportZip(id);
  assert.equal(exportedA.response.headers.get('content-type'), 'application/zip');
  assert.equal(exportedA.response.headers.get('cache-control'), 'no-store');
  assert.match(exportedA.response.headers.get('content-disposition') ?? '', /^attachment; filename="narou-export-\d{14}\.zip"$/);

  const { entries: zipEntries, manifest: exportedManifest } = manifestOf(exportedA.bytes);
  assert.equal(exportedManifest.format_version, 1);
  assert.equal(exportedManifest.novels.length, 1);
  const exportedNovel = exportedManifest.novels[0];
  assert.equal(exportedNovel.toc_url, TOC_URL);
  assert.equal(exportedNovel.title, TITLE);
  assert.equal(exportedNovel.author, AUTHOR);
  assert.equal(exportedNovel.sitename, SITENAME);
  assert.equal(exportedNovel.domain, DOMAIN);
  assert.equal(exportedNovel.ncode, 'n1234ab');
  assert.equal(exportedNovel.novel_type, 1);
  assert.deepEqual(exportedNovel.tags, ['テスト']);
  assert.equal(exportedNovel.subtitle, 'テスト用のあらすじ');
  assert.match(exportedNovel.last_update, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);
  assert.equal(exportedNovel.general_lastup, undefined);

  // ファイルは相対パスの昇順で並び、すべて manifest に載る。
  const paths = exportedNovel.files.map((file) => file.path);
  assert.deepEqual(paths, [...paths].sort());
  assert.deepEqual(paths, [
    'converter.yaml',
    'replace.txt',
    'setting.ini',
    'toc.yaml',
    '挿絵/001.png',
    firstSectionPath,
  ]);
  for (const file of exportedNovel.files) {
    const data = zipEntries.get(`novels/0/${file.path}`);
    assert(data, `${file.path} が ZIP にありません`);
    assert.equal(file.size, data.length, `${file.path}: size`);
    assert.equal(file.sha256, sha256(data), `${file.path}: sha256`);
  }
  // 実体が一致する。
  for (const file of fixtureA) {
    assert.deepEqual(zipEntries.get(`novels/0/${file.path}`), file.data, `${file.path}: body`);
  }
  console.log('PASS export writes a manifest with records metadata, digests and the stored objects');

  // 存在しない ID / ids なし。
  const missing = await a.json('/api/transfer/export?ids=99999');
  assert.equal(missing.response.status, 404, JSON.stringify(missing.body));
  assert.equal(missing.body.success, false);
  assert.equal(typeof missing.body.message, 'string');
  const noIds = await a.json('/api/transfer/export');
  assert.equal(noIds.response.status, 400);
  assert.equal(noIds.body.success, false);
  console.log('PASS export rejects missing ids (404 JSON) and an omitted ids parameter (400)');

  await stopWorker(workerA);
  workerA = undefined;

  // --- DB B: 別 DB へ取り込み、同じ ZIP が再現できる -----------------------
  phaseB = preparePhase('b');
  workerB = await startWorker(phaseB);
  console.log(`worker B ready on ${workerB.base}`);
  const b = client(workerB.base);

  const imported = await b.upload(exportedA.bytes);
  assert.deepEqual(imported.body, { success: true, created: 1, overwritten: 0, skipped: 0, errors: [] });
  const listB = await b.json('/api/novels');
  assert.equal(listB.body.records_total, 1);
  const idB = listB.body.data[0].id;
  const recordB = await b.json(`/api/novels/${idB}`);
  assert.equal(recordB.body.title, TITLE);
  assert.equal(recordB.body.ncode, 'n1234ab');
  assert.equal(recordB.body.domain, DOMAIN);

  const reExported = await b.exportZip(idB);
  const { manifest: reExportedManifest } = manifestOf(reExported.bytes);
  assert.deepEqual(filesFrom(reExportedManifest), filesFrom(exportedManifest));
  // 取り込みも書き出しも決定的: 同じ小説は同じバイト列になる
  // (exported_at と last_update だけが時刻)。
  assert.equal(reExported.bytes.length > 0, true);
  for (const file of fixtureA) {
    assert.deepEqual(
      readZip(reExported.bytes).get(`novels/0/${file.path}`),
      file.data,
      `${file.path}: 往復後の実体`,
    );
  }
  console.log('PASS a fresh database imports the exported ZIP and re-exports identical files');

  // --- overwrite -----------------------------------------------------------
  const overwritten = await b.upload(zipB, '?overwrite=true');
  assert.deepEqual(overwritten.body, { success: true, created: 0, overwritten: 1, skipped: 0, errors: [] });

  const recordAfter = await b.json(`/api/novels/${idB}`);
  assert.equal(recordAfter.body.id, idB, '上書きでレコード ID は変わらない');
  assert.equal(recordAfter.body.file_title, 'n1234ab', '上書きで保存先は変わらない');
  assert.equal(recordAfter.body.title, TITLE);
  assert.equal(recordAfter.body.author, AUTHOR);

  const afterOverwrite = await b.exportZip(idB);
  const { entries: afterEntries, manifest: afterManifest } = manifestOf(afterOverwrite.bytes);
  const overwrittenNovel = afterManifest.novels[0];
  assert.equal(overwrittenNovel.subtitle, '更新後のあらすじ');
  const afterPaths = overwrittenNovel.files.map((file) => file.path);
  assert(afterPaths.includes(secondSectionPath), '追加された話が保存されている');
  // アーカイブに無いファイルは消さない。
  assert(afterPaths.includes('replace.txt'), 'アーカイブに無い replace.txt が消えている');
  assert(afterPaths.includes('挿絵/001.png'), 'アーカイブに無い挿絵が消えている');
  // 置換された実体。
  assert.equal(
    afterEntries.get(`novels/0/${firstSectionPath}`).toString('utf8'),
    fixtureB.find((file) => file.path === firstSectionPath).data.toString('utf8'),
  );
  assert.deepEqual(afterEntries.get(`novels/0/${secondSectionPath}`), fixtureB.find((file) => file.path === secondSectionPath).data);
  assert.deepEqual(afterEntries.get('novels/0/replace.txt'), Buffer.from('元の置換設定\n'));
  assert.deepEqual(afterEntries.get('novels/0/挿絵/001.png'), PNG);
  console.log('PASS overwrite replaces archived files, keeps the id/folder and leaves absent files alone');

  // overwrite なしの再取り込みは skip (上書きしない)。
  const skippedAgain = await b.upload(zipB);
  assert.deepEqual(skippedAgain.body, { success: true, created: 0, overwritten: 0, skipped: 1, errors: [] });
  console.log('PASS importing an existing novel without ?overwrite=true only skips it');

  exitCode = 0;
} catch (error) {
  if (workerA) console.error(workerA.logs().slice(-20000));
  if (workerB) console.error(workerB.logs().slice(-20000));
  console.error(error);
} finally {
  await stopWorker(workerA);
  await stopWorker(workerB);
  rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}

process.exit(exitCode);
