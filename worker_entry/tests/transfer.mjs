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
import { deflateSync, inflateSync, inflateRawSync } from 'node:zlib';
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
// 旧形式アーカイブ検証用の別作品 (同じ toc_url だと衝突キーになる)。
const LEGACY_TOC_URL = 'https://example.com/legacy9999zz/';
// 1x1 の透過 PNG (挿絵は別ストアへ振り分けられる経路も通す)。
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  'base64',
);

function solidPng(rgb) {
  const chunk = (name, data) => {
    const type = Buffer.from(name);
    const size = Buffer.alloc(4), crc = Buffer.alloc(4);
    size.writeUInt32BE(data.length);
    crc.writeUInt32BE(crc32(Buffer.concat([type, data])));
    return Buffer.concat([size, type, data, crc]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(96, 0); header.writeUInt32BE(96, 4);
  header[8] = 8; header[9] = 2;
  const row = Buffer.alloc(1 + 96 * 3);
  row.fill(Buffer.from(rgb), 1);
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', header), chunk('IDAT', deflateSync(Buffer.concat(Array(96).fill(row)))),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function firstPngPixel(bytes) {
  assert.deepEqual(bytes.subarray(0, 8), Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  assert.equal(bytes[24], 8);
  assert([2, 6].includes(bytes[25]), 'the solid-color fixture must remain RGB or RGBA');
  const chunks = [];
  for (let offset = 8; offset + 12 <= bytes.length;) {
    const size = bytes.readUInt32BE(offset);
    if (bytes.toString('ascii', offset + 4, offset + 8) === 'IDAT') chunks.push(bytes.subarray(offset + 8, offset + 8 + size));
    offset += size + 12;
  }
  const pixels = inflateSync(Buffer.concat(chunks));
  assert(pixels[0] <= 4);
  // Every PNG row filter predicts zero for the first pixel of the first row.
  return [...pixels.subarray(1, 4)];
}

function tocYaml(story, subtitles) {
  return tocYamlFor(TOC_URL, TITLE, story, subtitles);
}

/// 指定 URL / 作品名の目次 YAML。
function tocYamlFor(tocUrl, title, story, subtitles) {
  const lines = ['---', `toc_url: ${tocUrl}`, `title: ${title}`, `author: ${AUTHOR}`, 'story: |-', `  ${story}`, 'subtitles:'];
  for (const subtitle of subtitles) {
    lines.push(
      `- index: '${subtitle.index}'`,
      `  href: ${tocUrl}${subtitle.index}/`,
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
  return sectionYamlFor(TOC_URL, index, name, body);
}

/// 指定 URL の本文 YAML。
function sectionYamlFor(tocUrl, index, name, body) {
  return [
    '---',
    `index: '${index}'`,
    `href: ${tocUrl}${index}/`,
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
/// `options.legacy` は旧形式 (属性タグが `tags` に埋まっている) の manifest を作る。
function archiveFor(files, options = {}) {
  const manifest = {
    format_version: 1,
    app_version: '0.5.0',
    exported_at: '2026-10-24T00:00:00Z',
    novels: [
      {
        toc_url: options.tocUrl ?? TOC_URL,
        title: options.title ?? TITLE,
        author: AUTHOR,
        sitename: SITENAME,
        domain: DOMAIN,
        tags: options.legacy ? ['テスト', '小説'] : ['テスト'],
        novel_type: 1,
        ncode: options.ncode ?? 'n1234ab',
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
// 虚拟小说（分组）的归档 fixture
// ---------------------------------------------------------------------------

// 分组父作品的标题 / 表紙 / 保存先 ( 通常作品とは別の保存先を持つ )。
const GROUP_TITLE = 'TransferVirtualGroup';
const GROUP_COVER = solidPng([12, 34, 56]);
const GROUP_PREFIX = `novels/${SITENAME}/${GROUP_TITLE}`;

/// 通常の作品記述子。
function ordinaryDescriptor({ tocUrl, title, ncode, story, body, files = [] }) {
  return {
    toc_url: tocUrl,
    title,
    ncode,
    files: [
      { path: 'toc.yaml', data: Buffer.from(tocYamlFor(tocUrl, title, story, [{ index: '1', name: '第一話' }])) },
      { path: '本文/1 第一話.yaml', data: Buffer.from(sectionYamlFor(tocUrl, '1', '第一話', body)) },
      ...files,
    ],
  };
}

/// 归档記述子から manifest と ZIP を作る。
/// `virtual_members` を持つ作品だけが format 2 (分组) として書き出される。
function archiveFrom(descriptors) {
  const manifest = {
    format_version: descriptors.some((descriptor) => descriptor.virtual_members) ? 2 : 1,
    app_version: '0.6.3',
    exported_at: '2026-11-10T00:00:00Z',
    novels: descriptors.map((descriptor) => ({
      toc_url: descriptor.toc_url,
      title: descriptor.title,
      author: descriptor.author ?? AUTHOR,
      sitename: descriptor.sitename ?? SITENAME,
      domain: descriptor.domain ?? DOMAIN,
      tags: descriptor.tags ?? ['テスト'],
      attribute_tag: descriptor.attribute_tag ?? '小説',
      novel_type: descriptor.novel_type ?? 1,
      ...(descriptor.ncode === undefined ? {} : { ncode: descriptor.ncode }),
      ...(descriptor.virtual_members === undefined ? {} : { virtual_members: descriptor.virtual_members }),
      files: descriptor.files.map((file) => ({
        path: file.path,
        size: file.data.length,
        sha256: sha256(file.data),
      })),
    })),
  };
  const entries = [{ name: 'manifest.json', data: JSON.stringify(manifest) }];
  descriptors.forEach((descriptor, index) => {
    for (const file of descriptor.files) {
      entries.push({ name: `novels/${index}/${file.path}`, data: file.data });
    }
  });
  return { zip: buildZip(entries), manifest };
}

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

function preparePhase(name, extraSql = '') {
  const directory = join(work, name);
  mkdirSync(directory, { recursive: true });
  let toml = readFileSync(join(root, 'wrangler.toml'), 'utf8').replace(/\[build\][^\[]*/, '');
  toml = toml
    .replace(/^main = .*$/m, `main = ${JSON.stringify(join(root, 'tests', 'transfer_failure_worker.mjs').replaceAll('\\', '/'))}`)
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
  if (extraSql) {
    const extra = join(directory, 'extra-seed.sql');
    writeFileSync(extra, extraSql);
    runWrangler(['d1', 'execute', 'DB', '--local', '--file', extra], phase);
  }
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
let phaseC;
let phaseD;
let workerA;
let workerB;
let workerC;
let workerD;
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
  // 属性タグは通常タグから独立している (保存は extra_fields / attribute_tag)。
  assert.deepEqual(record.body.tags, ['テスト']);
  assert.equal(record.body.attribute_tag, '小説');
  assert.equal(record.body.general_all_no, 1);
  assert.equal(record.body.use_subdirectory, false);
  console.log('PASS import creates the record and objects; unauthenticated / wrong-type / broken-zip are rejected');

  for (const [fault, ncode] of [['lookup', 'n9981zz'], ['delete', 'n9982zz']]) {
    const failed = await a.json('/api/transfer/import', {
      method: 'POST',
      headers: { 'Content-Type': 'application/zip', 'x-narou-test-cleanup-fault': fault },
      body: archiveFor(fixtureA, { tocUrl: `https://ncode.syosetu.com/${ncode}/`, ncode }),
      signal: AbortSignal.timeout(15000),
    });
    assert.equal(failed.response.status, 200, JSON.stringify(failed.body));
    assert.equal(failed.body.success, false);
    assert.equal(failed.body.created, 0);
    assert.equal(failed.response.headers.get('x-test-cleanup-lookups'), '1', 'failed cleanup must not respawn itself');
    assert.equal(failed.response.headers.get('x-test-cleanup-deletes'), fault === 'delete' ? '1' : '0');
    const remaining = await a.json('/api/novels');
    assert.deepEqual(remaining.body.data.map(record => record.toc_url), [TOC_URL], 'failed publication must not expose a new novel');
  }
  console.log('PASS failed import cleanup is bounded when lookup or deletion fails; existing novels remain usable');

  // 同じ ZIP の再取り込みは skip。
  const skipped = await a.upload(zipA);
  assert.deepEqual(skipped.body, { success: true, created: 0, overwritten: 0, skipped: 1, errors: [] });

  // export: ヘッダと ZIP の中身。
  const exportedA = await a.exportZip(id);
  assert.equal(exportedA.response.headers.get('content-type'), 'application/zip');
  assert.equal(exportedA.response.headers.get('cache-control'), 'no-store');
  assert.match(exportedA.response.headers.get('content-disposition') ?? '', /^attachment; filename="narou-export-\d{14}\.zip"$/);

  const { entries: zipEntries, manifest: exportedManifest } = manifestOf(exportedA.bytes);
  // 書き出しは format 2 ( 旧形式 1 の読み込みは引き続き可能 )。
  assert.equal(exportedManifest.format_version, 2);
  assert.equal(exportedManifest.novels.length, 1);
  const exportedNovel = exportedManifest.novels[0];
  assert.equal(exportedNovel.toc_url, TOC_URL);
  assert.equal(exportedNovel.title, TITLE);
  assert.equal(exportedNovel.author, AUTHOR);
  assert.equal(exportedNovel.sitename, SITENAME);
  assert.equal(exportedNovel.domain, DOMAIN);
  assert.equal(exportedNovel.ncode, 'n1234ab');
  assert.equal(exportedNovel.novel_type, 1);
  // 属性タグは manifest の独立フィールド (tags には入らない)。
  assert.deepEqual(exportedNovel.tags, ['テスト']);
  assert.equal(exportedNovel.attribute_tag, '小説');
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
  // 分组父作品の表紙は保存先に置かれる ( 設定如同 )。
  phaseB = preparePhase('b', `INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES ('${GROUP_PREFIX}/cover.png',${GROUP_COVER.length},'2026-11-10T00:00:00Z','none',${crc32(GROUP_COVER)},X'${GROUP_COVER.toString('hex')}');`);
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

  // A late corrupt entry must not publish the valid replacement entries read
  // before it. Rebuild CRC-correct ZIP records but leave the declared SHA bad.
  const corruptEntries = readZip(zipB);
  const corruptManifest = JSON.parse(corruptEntries.get('manifest.json'));
  corruptManifest.novels[0].files.at(-1).sha256 = '0'.repeat(64);
  corruptEntries.set('manifest.json', Buffer.from(JSON.stringify(corruptManifest)));
  const corruptOverwrite = await b.upload(buildZip([...corruptEntries].map(([name, data]) => ({ name, data }))), '?overwrite=true');
  assert.equal(corruptOverwrite.response.status, 400);
  const afterCorruption = readZip((await b.exportZip(idB)).bytes);
  for (const [path, bytes] of afterEntries) {
    if (path !== 'manifest.json') assert.deepEqual(afterCorruption.get(path), bytes, `${path}: corrupt import must not mutate live data`);
  }
  console.log('PASS late archive corruption leaves every existing file unchanged');

  // overwrite なしの再取り込みは skip (上書きしない)。
  const skippedAgain = await b.upload(zipB);
  assert.deepEqual(skippedAgain.body, { success: true, created: 0, overwritten: 0, skipped: 1, errors: [] });
  console.log('PASS importing an existing novel without ?overwrite=true only skips it');

  // --- 旧形式 (属性タグが tags に埋まっているアーカイブ) ----------------------
  const legacyZip = archiveFor(fixtureA, { legacy: true, tocUrl: LEGACY_TOC_URL, ncode: 'n9999zz' });
  const legacy = await b.upload(legacyZip);
  assert.deepEqual(legacy.body, { success: true, created: 1, overwritten: 0, skipped: 0, errors: [] });
  const legacyList = await b.json('/api/novels');
  const legacyRow = legacyList.body.data.find((row) => row.toc_url === LEGACY_TOC_URL);
  assert(legacyRow, '旧形式のアーカイブが取り込まれていない');
  const legacyRecord = await b.json(`/api/novels/${legacyRow.id}`);
  // 旧形式の属性タグは tags から取り出され、専用の項目に移る。
  assert.deepEqual(legacyRecord.body.tags, ['テスト']);
  assert.equal(legacyRecord.body.attribute_tag, '小説');
  const legacyExport = await b.exportZip(legacyRow.id);
  const { manifest: legacyManifest } = manifestOf(legacyExport.bytes);
  assert.deepEqual(legacyManifest.novels[0].tags, ['テスト']);
  assert.equal(legacyManifest.novels[0].attribute_tag, '小説');
  console.log('PASS a legacy archive with the classification inside tags is migrated into attribute_tag');

  // 158 works must not require retaining every uncompressed body in one Worker.
  const bulkCount = 158;
  const bulkBody = Buffer.from(`body: ${'x'.repeat(511992)}\n`);
  const bulkBodyExpression = "CAST('body: ' || replace(hex(zeroblob(255996)), '00', 'xx') || char(10) AS BLOB)";
  const bulkQueries = [];
  const bulkIds = [];
  for (let index = 0; index < bulkCount; index++) {
    const id = 10000 + index;
    bulkIds.push(id);
    const tocUrl = `https://example.com/bulk/${index}/`;
    const title = `Bulk ${index}`;
    const prefix = `novels/BulkTransfer/bulk-${index}`;
    const toc = Buffer.from(`title: ${title}\nauthor: Fixture\ntoc_url: ${tocUrl}\nsubtitles: []\nnovel_type: 1\n`);
    const quote = (value) => `'${String(value).replaceAll("'", "''")}'`;
    bulkQueries.push(`INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,ncode,ncode_fold,novel_type,use_subdirectory) VALUES (${id},'Fixture','fixture',${quote(title)},${quote(title.toLowerCase())},'bulk-${index}',${quote(tocUrl)},${quote(tocUrl)},'BulkTransfer','bulktransfer','2026-11-02T00:00:00Z','bulk-${index}','bulk-${index}',1,0);`);
    bulkQueries.push(`INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (${quote(`${prefix}/toc.yaml`)},${toc.length},'2026-11-02T00:00:00Z','none',${crc32(toc)},X'${toc.toString('hex')}');`);
    bulkQueries.push(`INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (${quote(`${prefix}/本文/1 話.yaml`)},${bulkBody.length},'2026-11-02T00:00:00Z','none',${crc32(bulkBody)},${bulkBodyExpression});`);
  }
  const bulkSeed = join(phaseA.directory, 'bulk-transfer.sql');
  writeFileSync(bulkSeed, bulkQueries.join('\n'));
  await stopWorker(workerA);
  runWrangler(['d1', 'execute', 'DB', '--local', '--file', bulkSeed], phaseA);
  workerA = await startWorker(phaseA);
  const bulkClient = client(workerA.base);
  async function progressResponse(response) {
    if (response.status !== 200) assert.fail(`HTTP ${response.status}: ${await response.text()}`);
    assert.match(response.headers.get('content-type'), /application\/x-ndjson/);
    const decoder = new TextDecoder();
    let pending = '';
    const frames = [], chunks = [];
    for await (const chunk of response.body) {
      pending += decoder.decode(chunk, { stream: true });
      let newline;
      while ((newline = pending.indexOf('\n')) !== -1) {
        const frame = JSON.parse(pending.slice(0, newline));
        pending = pending.slice(newline + 1);
        assert.notEqual(frame.type, 'error', JSON.stringify(frame));
        if (frame.type === 'data') chunks.push(Buffer.from(frame.data, 'base64'));
        else frames.push(frame);
      }
    }
    assert.equal(pending.trim(), '');
    assert.equal(frames.at(-1)?.type, 'complete', 'a closed stream is not a successful transfer');
    return { frames, bytes: Buffer.concat(chunks) };
  }
  const bulkExport = await progressResponse(await bulkClient.request(`/api/transfer/export?progress=1&ids=${bulkIds.join(',')}`));
  assert.equal(bulkExport.frames.at(-1).bytes, bulkExport.bytes.length);
  assert(bulkExport.frames.some((frame) => frame.type === 'progress' && frame.completed === bulkCount && frame.total === bulkCount));
  const { manifest: bulkManifest, entries: bulkEntries } = manifestOf(bulkExport.bytes);
  assert.equal(bulkManifest.novels.length, bulkCount);
  for (let index = 0; index < bulkCount; index++) {
    assert.equal(bulkManifest.novels[index].toc_url, `https://example.com/bulk/${index}/`);
    assert.deepEqual(bulkEntries.get(`novels/${index}/本文/1 話.yaml`), bulkBody);
    assert.equal(bulkManifest.novels[index].files.find((file) => file.path === '本文/1 話.yaml').sha256, sha256(bulkBody));
  }
  console.log('PASS 158 novels / 77 MiB of bodies stream into one valid ZIP with progress and verified hashes');
  const streamedImport = await progressResponse(await b.request('/api/transfer/import?progress=1', {
    method: 'POST', headers: { 'Content-Type': 'application/zip' }, body: bulkExport.bytes,
  }));
  assert.deepEqual(streamedImport.frames.at(-1).result, { success: true, created: bulkCount, overwritten: 0, skipped: 0, errors: [] });
  assert(streamedImport.frames.some((frame) => frame.type === 'progress' && frame.phase === 'import' && frame.completed === bulkCount));
  const importList = await b.json('/api/novels?limit=1000');
  assert.equal(importList.body.data.filter((novel) => novel.toc_url.startsWith('https://example.com/bulk/')).length, bulkCount);
  const missingStream = await bulkClient.request('/api/transfer/export?progress=1&ids=999999999');
  const missingFrames = (await missingStream.text()).trim().split('\n').map(JSON.parse);
  assert.equal(missingFrames.at(-1).type, 'error');
  assert(!missingFrames.some((frame) => frame.type === 'complete'));
  console.log('PASS streamed import reports all 158 committed works and missing exports fail without a completion marker');

  const colors = [[240, 0, 0], [0, 0, 240]];
  const memberTitles = ['赤い作品', '青い作品'];
  const memberUrls = memberTitles.map((_, index) => `https://example.com/virtual-image-${index}/`);
  for (const [index, color] of colors.entries()) {
    const files = [
      { path: 'toc.yaml', data: Buffer.from(tocYaml('結合に混ぜない個別あらすじ', [{ index: '1', name: '第一話' }])) },
      { path: firstSectionPath, data: Buffer.from(sectionYaml('1', '第一話', `${memberTitles[index]}の本文。［＃挿絵（挿絵/001.png）入る］`)) },
      { path: '挿絵/001.png', data: solidPng(color) },
    ];
    const imported = await b.upload(archiveFor(files, { tocUrl: memberUrls[index], ncode: `virtual-image-${index}`, title: memberTitles[index] }));
    assert.equal(imported.response.status, 200, JSON.stringify(imported.body));
    assert.equal(imported.body.created, 1);
  }
  const withImages = (await b.json('/api/novels?limit=1000')).body.data;
  const members = memberUrls.map(url => withImages.find(record => record.toc_url === url).id);
  const post = body => ({ method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  const virtual = await b.json('/api/virtual_novels', post({ title: '画像結合検証', author: '検証作者', members }));
  assert.equal(virtual.response.status, 200, JSON.stringify(virtual.body));
  const virtualId = virtual.body.id;
  const savedLayout = await b.json(`/api/settings/${virtualId}`, post({ settings: [{ name: 'enable_yokogaki', value: true }] }));
  assert.equal(savedLayout.response.status, 200, JSON.stringify(savedLayout.body));
  assert.equal((await b.request(`/api/virtual_novels/${members[0]}/download.epub`)).status, 404);
  const combined = await b.request(`/api/virtual_novels/${virtualId}/download.epub`);
  assert.equal(combined.status, 200, combined.status === 200 ? '' : await combined.text());
  const book = readZip(Buffer.from(await combined.arrayBuffer()));
  assert(!book.has('item/xhtml/cover.xhtml'), 'the first member illustration must stay in the virtual book body');
  const pixels = [...book].filter(([name]) => name.startsWith('item/image/')).map(([, bytes]) => firstPngPixel(bytes));
  assert.deepEqual(new Set(pixels.map(rgb => rgb.join(','))), new Set(colors.map(rgb => rgb.join(','))), 'both same-named member images must survive');
  const packageText = [...book].find(([name]) => name.endsWith('.opf'))[1].toString();
  assert.match(packageText, /page-progression-direction="ltr"/);
  assert.doesNotMatch(packageText, /\bcover-image\b/, 'a virtual book needs its own explicit cover');
  const xhtml = [...book].filter(([name]) => name.endsWith('.xhtml')).map(([, bytes]) => bytes.toString()).join('\n');
  for (const title of memberTitles) assert(xhtml.includes(`${title}の本文`), xhtml);
  assert(!xhtml.includes('結合に混ぜない個別あらすじ'), 'member books must not be concatenated with their individual front matter');
  const queued = await b.json('/api/convert', post({ targets: [String(virtualId)], device: 'epub' }));
  assert.equal(queued.response.status, 200, JSON.stringify(queued.body));
  const jobId = queued.body.results[0].job_id;
  let outcome;
  const deadline = Date.now() + 60000;
  do {
    outcome = (await b.json(`/api/jobs/${jobId}`)).body;
    if (!['pending', 'running'].includes(outcome.status)) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  } while (Date.now() < deadline);
  assert.equal(outcome.status, 'succeeded', JSON.stringify(outcome));
  console.log('PASS virtual EPUB preserves member pixels, one book structure, target layout, and queued conversion');

  // --- バーチャル小説の構成とメンバーを別ライブラリへ転送 -------------------
  // 本文は通常作品、親は構成・変換設定・明示的な表紙を保持する。
  const groupTitle = GROUP_TITLE;
  const groupCover = GROUP_COVER;
  const vmemberOne = { tocUrl: 'https://example.com/vmember-1/', title: 'VirtualMemberOne', ncode: 'vmember1', body: '第一の作品の本文。' };
  const vmemberTwo = { tocUrl: 'https://example.com/vmember-2/', title: 'VirtualMemberTwo', ncode: 'vmember2', body: '第二の作品の本文。' };
  for (const member of [vmemberOne, vmemberTwo]) {
    const { zip } = archiveFrom([ordinaryDescriptor({
      tocUrl: member.tocUrl, title: member.title, ncode: member.ncode,
      story: `${member.title}のあらすじ`, body: member.body,
    })]);
    const imported = await b.upload(zip);
    assert.equal(imported.response.status, 200, JSON.stringify(imported.body));
    assert.equal(imported.body.created, 1, JSON.stringify(imported.body));
  }
  const virtualList = await b.json('/api/novels?limit=1000');
  const memberIds = [vmemberOne, vmemberTwo].map((member) => {
    const row = virtualList.body.data.find((record) => record.toc_url === member.tocUrl);
    assert(row, `missing member ${member.tocUrl}`);
    return row.id;
  });
  const group = await b.json('/api/virtual_novels', post({
    title: groupTitle, author: 'グループ作者', sitename: SITENAME, members: memberIds,
  }));
  assert.equal(group.response.status, 200, JSON.stringify(group.body));
  const groupId = group.body.id;
  const groupRecord = await b.json(`/api/novels/${groupId}`);
  assert.match(groupRecord.body.toc_url, /^https:\/\/narou\.rs\/virtual\/[0-9a-f]{32}$/, 'a new group uses a portable identity');
  assert.equal(groupRecord.body.ncode, `vs_${groupId}`);
  const groupSettings = await b.json(`/api/settings/${groupId}`, post({ settings: [{ name: 'enable_yokogaki', value: true }] }));
  assert.equal(groupSettings.response.status, 200, JSON.stringify(groupSettings.body));
  console.log('PASS a new virtual group registers with a portable identity and per-novel settings');

  // 親だけを書き出すと、メンバーが自動的に展開される。
  const groupExport = await b.exportZip(groupId);
  const { entries: groupEntries, manifest: groupManifest } = manifestOf(groupExport.bytes);
  assert.equal(groupManifest.format_version, 2);
  assert.equal(groupManifest.novels.length, 3, 'the parent expands into its two members');
  assert.equal(groupManifest.novels[0].virtual_members, undefined);
  assert.equal(groupManifest.novels[1].virtual_members, undefined);
  const parentNovel = groupManifest.novels[2];
  assert.deepEqual(parentNovel.virtual_members, [vmemberOne.tocUrl, vmemberTwo.tocUrl], 'the membership order travels with the parent');
  assert.equal(parentNovel.ncode, undefined, 'a parent carries no library-local ncode');
  assert.equal(parentNovel.toc_url, groupRecord.body.toc_url);
  assert(!parentNovel.tags.some((tag) => tag.startsWith('vs_')), 'the internal tag never travels');
  assert.deepEqual(parentNovel.files.map((file) => file.path), ['cover.png', 'setting.ini']);
  assert.deepEqual(groupEntries.get('novels/2/cover.png'), groupCover);
  assert(groupEntries.get('novels/2/setting.ini').toString('utf8').includes('enable_yokogaki'));
  // 選択されたメンバーも 1 度だけ書かれる。
  const dedupExport = await b.exportZip(`${groupId},${memberIds[0]}`);
  const { manifest: dedupManifest } = manifestOf(dedupExport.bytes);
  assert.equal(dedupManifest.novels.length, 3, 'a member that is also selected is stored once');
  console.log('PASS exporting a selected group writes the members once and the parent descriptor with its order');

  // --- 別ライブラリへ取り込む (衝突 / 順序 / 設定 / 表紙 / EPUB) --------------
  phaseC = preparePhase('c');
  workerC = await startWorker(phaseC);
  const c = client(workerC.base);
  // 同じ作品名のグループが既にあるライブラリを用意する (保存先の衝突)。
  const otherMember = { tocUrl: 'https://example.com/vmember-9/', title: 'VirtualMemberNine', ncode: 'vmember9' };
  const otherImport = await c.upload(archiveFrom([ordinaryDescriptor({
    tocUrl: otherMember.tocUrl, title: otherMember.title, ncode: otherMember.ncode,
    story: 'Ninth story', body: 'VirtualMemberNineの本文。',
  })]).zip);
  assert.equal(otherImport.body.created, 1, JSON.stringify(otherImport.body));
  const otherRow = (await c.json('/api/novels?limit=1000')).body.data.find((row) => row.toc_url === otherMember.tocUrl);
  const sameTitle = await c.json('/api/virtual_novels', post({
    title: groupTitle, author: '既存グループ', sitename: SITENAME, members: [otherRow.id],
  }));
  assert.equal(sameTitle.response.status, 200, JSON.stringify(sameTitle.body));
  const sameTitleRow = (await c.json(`/api/novels/${sameTitle.body.id}`)).body;

  // 取り込み先に既にいるメンバーは skip されても、親はその ID を引き継ぐ。
  const existingMember = await c.upload(archiveFrom([ordinaryDescriptor({
    tocUrl: vmemberOne.tocUrl, title: vmemberOne.title, ncode: vmemberOne.ncode,
    story: `${vmemberOne.title}のあらすじ`, body: vmemberOne.body,
  })]).zip);
  assert.equal(existingMember.body.created, 1, JSON.stringify(existingMember.body));

  const importedGroup = await c.upload(groupExport.bytes);
  assert.deepEqual(importedGroup.body, { success: true, created: 2, overwritten: 0, skipped: 1, errors: [] });
  const cList = (await c.json('/api/novels?limit=1000')).body.data;
  const cMembers = [vmemberOne, vmemberTwo].map((member) => cList.find((row) => row.toc_url === member.tocUrl).id);
  const cParentRow = cList.find((row) => row.toc_url === groupRecord.body.toc_url);
  assert(cParentRow, 'the parent was imported');
  const cParent = (await c.json(`/api/novels/${cParentRow.id}`)).body;
  assert.equal(cParent.ncode, `vs_${cParent.id}`, 'the parent gets a destination-local ncode');
  assert.notEqual(cParent.file_title, sameTitleRow.file_title, 'the imported group must not share the folder of an existing same-title group');
  const cGroups = (await c.json('/api/virtual_novels')).body.novels;
  const cGroup = cGroups.find((row) => row.id === cParent.id);
  assert.deepEqual(cGroup.members, cMembers, 'the membership order survives the reallocated ids');
  assert.deepEqual(cGroups.find((row) => row.id === sameTitle.body.id).members, [otherRow.id], 'the existing group is untouched');
  console.log('PASS a fresh library imports the group after its members and keeps the order under new ids');

  // 取り込み直後の父作品: 設定と表紙がそのまま保存されている。
  const cParentExport = await c.exportZip(cParent.id);
  const { entries: cParentEntries, manifest: cParentManifest } = manifestOf(cParentExport.bytes);
  const cParentNovel = cParentManifest.novels.find((novel) => novel.toc_url === groupRecord.body.toc_url);
  const cParentIndex = cParentManifest.novels.indexOf(cParentNovel);
  assert.deepEqual(cParentNovel.files.map((file) => file.path), ['cover.png', 'setting.ini']);
  assert.deepEqual(cParentEntries.get(`novels/${cParentIndex}/cover.png`), groupCover);
  assert.deepEqual(cParentEntries.get(`novels/${cParentIndex}/setting.ini`), groupEntries.get('novels/2/setting.ini'));
  console.log('PASS the imported group keeps its settings and cover bytes');

  // 結合 EPUB: UI の実経路で両メンバーの本文が登録順に現れる。
  const epub = await c.request(`/novels/${cParent.id}/download`);
  assert.equal(epub.status, 200, epub.status === 200 ? '' : await epub.text());
  const importedBook = readZip(Buffer.from(await epub.arrayBuffer()));
  const importedXhtml = [...importedBook].filter(([name]) => name.endsWith('.xhtml')).map(([, bytes]) => bytes.toString()).join('\n');
  assert(importedXhtml.includes(vmemberOne.body), importedXhtml);
  assert(importedXhtml.includes(vmemberTwo.body), importedXhtml);
  assert(importedXhtml.indexOf(vmemberOne.body) < importedXhtml.indexOf(vmemberTwo.body), 'the members keep their order');
  console.log('PASS the imported group renders one EPUB through the UI route');

  // 同じアーカイブの再取り込みは skip。
  const repeated = await c.upload(groupExport.bytes);
  assert.deepEqual(repeated.body, { success: true, created: 0, overwritten: 0, skipped: 3, errors: [] });
  console.log('PASS importing the same group archive again skips every entry');

  // 上書き: メンバー順が入れ替わっても保存先 ID とフォルダは変わらない。
  const reversedArchive = archiveFrom([
    ordinaryDescriptor({
      tocUrl: vmemberOne.tocUrl, title: vmemberOne.title, ncode: vmemberOne.ncode,
      story: `${vmemberOne.title}のあらすじ`, body: vmemberOne.body,
    }),
    ordinaryDescriptor({
      tocUrl: vmemberTwo.tocUrl, title: vmemberTwo.title, ncode: vmemberTwo.ncode,
      story: `${vmemberTwo.title}のあらすじ`, body: vmemberTwo.body,
    }),
    {
      toc_url: groupRecord.body.toc_url, title: groupTitle, files: [],
      virtual_members: [vmemberTwo.tocUrl, vmemberOne.tocUrl],
    },
  ]);
  const reversedImport = await c.upload(reversedArchive.zip, '?overwrite=true');
  assert.deepEqual(reversedImport.body, { success: true, created: 0, overwritten: 3, skipped: 0, errors: [] });
  const reversedGroup = (await c.json('/api/virtual_novels')).body.novels.find((row) => row.id === cParent.id);
  assert.deepEqual(reversedGroup.members, [cMembers[1], cMembers[0]], 'the overwrite replaces the membership order');
  const reversedParent = (await c.json(`/api/novels/${cParent.id}`)).body;
  assert.equal(reversedParent.file_title, cParent.file_title, 'the folder of the group is preserved');
  assert.equal(reversedParent.toc_url, groupRecord.body.toc_url, 'the portable identity is preserved');
  console.log('PASS overwriting a group replaces the membership order and keeps its identity and folder');

  // --- 壊れたメンバー参照は公開前に拒否される ------------------------------
  const brokenGraphs = [
    ['missing member', ['https://example.com/missing-member/']],
    ['self reference', ['https://narou.rs/virtual/11111111111111111111111111111111']],
  ];
  for (const [label, refs] of brokenGraphs) {
    const broken = archiveFrom([
      ordinaryDescriptor({
        tocUrl: 'https://example.com/broken-member/', title: 'BrokenMember', ncode: 'broken1',
        story: 'broken', body: 'broken',
      }),
      {
        toc_url: 'https://narou.rs/virtual/11111111111111111111111111111111',
        title: 'BrokenGroup', files: [], virtual_members: refs,
      },
    ]);
    const refused = await c.upload(broken.zip);
    assert.equal(refused.response.status, 400, `${label}: ${JSON.stringify(refused.body)}`);
    const afterRefusal = (await c.json('/api/novels?limit=1000')).body.data;
    assert(!afterRefusal.some((row) => row.toc_url === 'https://example.com/broken-member/'), `${label}: nothing may be imported`);
  }
  // 入れ子の親 (親がメンバーを参照) も拒否される。
  const nested = archiveFrom([
    ordinaryDescriptor({
      tocUrl: 'https://example.com/nested-member/', title: 'NestedMember', ncode: 'nested1',
      story: 'nested', body: 'nested',
    }),
    {
      toc_url: 'https://narou.rs/virtual/22222222222222222222222222222222',
      title: 'InnerGroup', files: [], virtual_members: ['https://example.com/nested-member/'],
    },
    {
      toc_url: 'https://narou.rs/virtual/33333333333333333333333333333333',
      title: 'OuterGroup', files: [], virtual_members: ['https://narou.rs/virtual/22222222222222222222222222222222'],
    },
  ]);
  assert.equal((await c.upload(nested.zip)).response.status, 400);
  console.log('PASS malformed member graphs are refused before anything is imported');

  // --- メンバーの保存に失敗したら親も公開されない --------------------------
  const blocked = await c.upload(archiveFrom([
    // ncode / 保存先が既存作品と衝突する。
    ordinaryDescriptor({
      tocUrl: 'https://example.com/collide/', title: 'CollideMember', ncode: 'vmember1',
      story: 'collide', body: 'CollideMemberの本文。',
    }),
    ordinaryDescriptor({
      tocUrl: 'https://example.com/healthy-member/', title: 'HealthyMember', ncode: 'vmember3',
      story: 'healthy', body: 'HealthyMemberの本文。',
    }),
    {
      toc_url: 'https://narou.rs/virtual/44444444444444444444444444444444',
      title: 'BlockedGroup', files: [],
      virtual_members: ['https://example.com/collide/', 'https://example.com/healthy-member/'],
    },
  ]).zip);
  assert.equal(blocked.response.status, 200, JSON.stringify(blocked.body));
  assert.equal(blocked.body.success, false);
  assert.equal(blocked.body.created, 1, 'the healthy member is still imported');
  const blockedList = (await c.json('/api/novels?limit=1000')).body.data;
  assert(!blockedList.some((row) => row.toc_url === 'https://narou.rs/virtual/44444444444444444444444444444444'), 'the parent must not be published');
  assert(!(await c.json('/api/virtual_novels')).body.novels.some((row) => row.title === 'BlockedGroup'));
  console.log('PASS a failed member blocks the publication of its parent');

  // --- 旧 ID のバーチャル小説 ( 移植可能な ID への移行 ) --------------------
  // ライブラリローカルな URL (`/virtual/vs_<ID>`) だけを持つ旧グループ。
  const legacyRows = [
    { id: 901, title: 'LegacyMemberOne', fileTitle: 'legacymember1', tocUrl: 'https://example.com/legacy-1/' },
    { id: 902, title: 'LegacyMemberTwo', fileTitle: 'legacymember2', tocUrl: 'https://example.com/legacy-2/' },
  ];
  const legacySeed = [];
  const sqlQuote = (value) => `'${String(value).replaceAll("'", "''")}'`;
  for (const row of legacyRows) {
    const toc = Buffer.from(`title: ${row.title}\nauthor: Fixture\ntoc_url: ${row.tocUrl}\nsubtitles: []\nnovel_type: 1\n`);
    const prefix = `novels/Legacy Site/${row.fileTitle}`;
    legacySeed.push(`INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,ncode,ncode_fold,novel_type,use_subdirectory) VALUES (${row.id},'Fixture','fixture',${sqlQuote(row.title)},${sqlQuote(row.title.toLowerCase())},${sqlQuote(row.fileTitle)},${sqlQuote(row.tocUrl)},${sqlQuote(row.tocUrl)},'Legacy Site','legacy site','2026-11-10T00:00:00Z',${sqlQuote(row.fileTitle)},${sqlQuote(row.fileTitle)},1,0);`);
    legacySeed.push(`INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (${sqlQuote(`${prefix}/toc.yaml`)},${toc.length},'2026-11-10T00:00:00Z','none',${crc32(toc)},X'${toc.toString('hex')}');`);
  }
  legacySeed.push("INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,ncode,ncode_fold,novel_type,use_subdirectory) VALUES (900,'Fixture','fixture','LegacyGroup','legacygroup','LegacyGroup','https://narou.rs/virtual/vs_900','https://narou.rs/virtual/vs_900','Legacy Site','legacy site','2026-11-10T00:00:00Z','vs_900','vs_900',0,0);");
  legacySeed.push("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','virtual_novel_members','{\"900\":[901,902]}','{\"900\":[901,902]}');");
  phaseD = preparePhase('d', legacySeed.join('\n'));
  workerD = await startWorker(phaseD);
  const d = client(workerD.base);
  const legacyGroupExport = await d.exportZip(900);
  const { manifest: legacyGroupManifest } = manifestOf(legacyGroupExport.bytes);
  assert.equal(legacyGroupManifest.novels.length, 3, 'the legacy group expands into its members');
  const legacyParent = legacyGroupManifest.novels[2];
  assert.match(legacyParent.toc_url, /^https:\/\/narou\.rs\/virtual\/[0-9a-f]{32}$/, 'the first export persists a portable identity');
  assert.deepEqual(legacyParent.virtual_members, ['https://example.com/legacy-1/', 'https://example.com/legacy-2/']);
  const legacyGroupRow = await d.json('/api/novels/900');
  assert.equal(legacyGroupRow.body.toc_url, legacyParent.toc_url, 'the source record keeps the portable identity');
  const legacyExportAgain = await d.exportZip(900);
  const { manifest: legacyAgain } = manifestOf(legacyExportAgain.bytes);
  assert.equal(legacyAgain.novels[2].toc_url, legacyParent.toc_url, 'later exports reuse the same identity');
  console.log('PASS the first export of an older group persists one portable identity and reuses it');

  // Exercise one file and one ZIP above the old 16 MiB small-object ceiling.
  // Client fixture allocations are outside the Worker; production routes must
  // receive, stage, validate, export and re-import these bodies incrementally.
  const large = Buffer.alloc(17 * 1024 * 1024);
  let random = 0x12345678;
  for (let index = 0; index < large.length; index++) {
    random ^= random << 13; random ^= random >>> 17; random ^= random << 5;
    large[index] = random & 255;
  }
  const largeFiles = [
    { path: 'toc.yaml', data: Buffer.from(tocYaml('大きな挿絵', [])) },
    { path: '挿絵/large.bin', data: large },
  ];
  const largeImport = await b.upload(archiveFor(largeFiles, { tocUrl: 'https://example.com/large-transfer/', ncode: 'large-transfer' }));
  assert.equal(largeImport.body.created, 1, JSON.stringify(largeImport.body));
  const largeId = (await b.json('/api/novels?limit=1000')).body.data.find(record => record.toc_url === 'https://example.com/large-transfer/').id;
  const largeExport = await progressResponse(await b.request(`/api/transfer/export?progress=1&ids=${largeId}`));
  assert(largeExport.bytes.length > 16 * 1024 * 1024);
  assert.equal(sha256(readZip(largeExport.bytes).get('novels/0/挿絵/large.bin')), sha256(large));
  const largeRoundtrip = await bulkClient.upload(largeExport.bytes);
  assert.equal(largeRoundtrip.body.created, 1, JSON.stringify(largeRoundtrip.body));
  // Cancelling a body releases the isolate-local heavy permit. A subsequent
  // request must not remain busy merely because the client stopped pulling.
  const cancelledExport = await b.request(`/api/transfer/export?progress=1&ids=${largeId}`);
  const cancelledReader = cancelledExport.body.getReader();
  await cancelledReader.read();
  await cancelledReader.cancel('contract cancellation');
  const resumed = await b.exportZip(largeId);
  assert.equal(sha256(readZip(resumed.bytes).get('novels/0/挿絵/large.bin')), sha256(large));
  console.log('PASS large per-entry transfer and cancelled-stream admission cleanup');

  exitCode = 0;
} catch (error) {
  if (workerA) console.error(workerA.logs().slice(-20000));
  if (workerB) console.error(workerB.logs().slice(-20000));
  if (workerC) console.error(workerC.logs().slice(-20000));
  if (workerD) console.error(workerD.logs().slice(-20000));
  console.error(error);
} finally {
  await stopWorker(workerA);
  await stopWorker(workerB);
  await stopWorker(workerC);
  await stopWorker(workerD);
  rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}

process.exit(exitCode);
