// 読み取り配信 Worker (reader) の smoke 用の共通 fixture。
//
//   - S3 互換 HTTP fixture (実 GET/PUT/DELETE/LIST/multipart を受理し、要求を全部記録)
//   - EPUB (ZIP) の実バイト列を解析する補助
//   - reader / writer の wrangler 設定の描画と dev プロセスの起動・停止
//
// 隔离: 作業ディレクトリは .wrangler/read-worker-* に作り、開発用の .dev.vars と
// .wrangler/state には触らない。
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { inflateRawSync } from 'node:zlib';
import http from 'node:http';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const root = join(dirname(fileURLToPath(import.meta.url)), '..');
export const windows = process.platform === 'win32';
export const npx = windows ? 'npx.cmd' : 'npx';
const wranglerVersion = process.env.NAROU_WRANGLER_VERSION || '4.151.3';
if (!/^\d+\.\d+\.\d+$/.test(wranglerVersion)) throw new Error('Wrangler version must be exact');
export const wranglerPackage = `wrangler@${wranglerVersion}`;
const environment = { ...process.env, WRANGLER_SEND_METRICS: 'false' };
for (const name of Object.keys(environment)) {
  if (/^(CLOUDFLARE_|CF_API_|NAROU_|S3_|AWS_)/.test(name)) delete environment[name];
}

// 配信 Worker が書き込んでよい唯一の S3 接頭辞 (private staging と予約台帳)。
export const STAGING_PREFIX = '.narou-staging/';
export const JOURNAL_PREFIX = '.narou-stage-journal/';

export function runWrangler(args, { cwd = root, env = environment } = {}) {
  const result = spawnSync(npx, ['--yes', wranglerPackage, ...args], {
    cwd,
    shell: windows,
    encoding: 'utf8',
    env,
  });
  if (result.status !== 0) {
    throw new Error(
      `wrangler ${args.join(' ')} failed (${result.status}):\n${result.stderr}\n${result.stdout}`,
    );
  }
  return result.stdout;
}

/** 未使用の TCP ポートを 1 つ確保する (0 番で listen して番号を返す)。 */
export async function freePort() {
  const probe = http.createServer();
  probe.listen(0, '127.0.0.1');
  await once(probe, 'listening');
  const { port } = probe.address();
  await new Promise((resolve) => probe.close(resolve));
  return port;
}

const xmlEscape = (value) =>
  String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');

const etagOf = (bytes) => `"${createHash('md5').update(bytes).digest('hex')}"`;

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

export { crc32 };

/**
 * S3 互換 fixture。
 *
 * - `objects` は保存済みオブジェクト (完全パス -> bytes)。
 * - `requests` は全 S3 要求 (method / path / key)。
 * - `reports` は Worker 側 probe (reader の D1 書き込み検知) の報告。
 * - `holdNextSourceRead()` は次の source 読み出しを 1 回だけ保留する。
 *   writer の重い処理を保持したい時に使う (source fixture でブロックし、実 OOM は起こさない)。
 */
export async function createS3Fixture({ bucket, prefix }) {
  const objects = new Map();
  const requests = [];
  const reports = [];
  const held = [];
  const uploads = new Map();
  let armed = false;
  let uploadSequence = 0;

  const base = `/${bucket}/${prefix}/`;
  const pathOf = (key) => `${base}${key}`;

  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://127.0.0.1');
      const path = decodeURIComponent(url.pathname);
      if (path.startsWith('/_probe/')) {
        const chunks = [];
        for await (const chunk of req) chunks.push(chunk);
        reports.push({
          path,
          body: Buffer.concat(chunks).toString('utf8'),
          at: Date.now(),
        });
        res.writeHead(204);
        res.end();
        return;
      }
      const key = path.startsWith(`/${bucket}/`) ? path.slice(bucket.length + 2) : path;
      const logicalKey = key.startsWith(`${prefix}/`) ? key.slice(prefix.length + 1) : key;
      requests.push({ method: req.method, path, key, logicalKey, at: Date.now() });

      // 保留された source 読み出し (1 回だけ)。
      if (
        armed &&
        req.method === 'GET' &&
        !url.searchParams.get('list-type') &&
        !isPrivateKey(logicalKey)
      ) {
        armed = false;
        await new Promise((resolve) => held.push({ key, resolve }));
      }

      if (req.method === 'GET' && url.searchParams.get('list-type') === '2') {
        const listPrefix = url.searchParams.get('prefix') ?? '';
        const after = url.searchParams.get('continuation-token') ?? '';
        const maxKeys = Number(url.searchParams.get('max-keys') ?? 1000);
        // バケット内のキー (S3 接頭辞込み) で比較する。
        const all = [...objects.keys()]
          .filter((path2) => path2.startsWith(`/${bucket}/`))
          .map((path2) => path2.slice(bucket.length + 2))
          .filter((key2) => key2.startsWith(listPrefix) && key2 > after)
          .sort();
        const selected = all.slice(0, maxKeys);
        const more = all.length > selected.length;
        const contents = selected
          .map((key2) => {
            const bytes = objects.get(`/${bucket}/${key2}`);
            return (
              `<Contents><Key>${xmlEscape(encodeURIComponent(key2))}</Key>` +
              `<LastModified>2026-10-15T00:00:00Z</LastModified>` +
              `<ETag>${xmlEscape(etagOf(bytes))}</ETag><Size>${bytes.length}</Size></Contents>`
            );
          })
          .join('');
        res.writeHead(200, { 'content-type': 'application/xml' });
        res.end(
          `<ListBucketResult><Name>${xmlEscape(bucket)}</Name>` +
            `<Prefix>${xmlEscape(listPrefix)}</Prefix><KeyCount>${selected.length}</KeyCount>` +
            `<MaxKeys>${maxKeys}</MaxKeys><IsTruncated>${more}</IsTruncated>${contents}` +
            `${more ? `<NextContinuationToken>${xmlEscape(encodeURIComponent(selected.at(-1)))}</NextContinuationToken>` : ''}` +
            `</ListBucketResult>`,
        );
        return;
      }

      if (req.method === 'PUT' && url.searchParams.has('partNumber')) {
        const uploadId = url.searchParams.get('uploadId');
        const upload = uploads.get(uploadId);
        if (!upload) {
          res.writeHead(404);
          res.end('<Error><Code>NoSuchUpload</Code></Error>');
          return;
        }
        const bytes = await readBody(req);
        upload.parts.set(Number(url.searchParams.get('partNumber')), bytes);
        res.writeHead(200, { etag: etagOf(bytes) });
        res.end();
        return;
      }

      if (req.method === 'POST' && url.searchParams.has('uploadId')) {
        const upload = uploads.get(url.searchParams.get('uploadId'));
        if (!upload) {
          res.writeHead(404);
          res.end('<Error><Code>NoSuchUpload</Code></Error>');
          return;
        }
        const parts = [...upload.parts.entries()].sort((a, b) => a[0] - b[0]);
        const bytes = Buffer.concat(parts.map(([, value]) => value));
        objects.set(upload.path, bytes);
        uploads.delete(upload.id);
        res.writeHead(200, { 'content-type': 'application/xml' });
        res.end(
          `<CompleteMultipartUploadResult><Location>${xmlEscape(upload.path)}</Location>` +
            `<ETag>${xmlEscape(etagOf(bytes))}</ETag></CompleteMultipartUploadResult>`,
        );
        return;
      }

      if (req.method === 'POST' && url.searchParams.has('uploads')) {
        uploadSequence += 1;
        const id = `upload-${uploadSequence}`;
        uploads.set(id, { id, path, parts: new Map() });
        res.writeHead(200, { 'content-type': 'application/xml' });
        res.end(
          `<InitiateMultipartUploadResult><Bucket>${xmlEscape(bucket)}</Bucket>` +
            `<Key>${xmlEscape(encodeURIComponent(key))}</Key><UploadId>${id}</UploadId>` +
            `</InitiateMultipartUploadResult>`,
        );
        return;
      }

      if (req.method === 'PUT') {
        const copySource = req.headers['x-amz-copy-source'];
        if (copySource) {
          const source = decodeURIComponent(String(copySource));
          const bytes = objects.get(source.startsWith('/') ? source : `/${source}`);
          if (!bytes) {
            res.writeHead(404);
            res.end('<Error><Code>NoSuchKey</Code></Error>');
            return;
          }
          const conflict = conditionalConflict(req, objects.get(path));
          if (conflict) {
            res.writeHead(conflict);
            res.end('<Error><Code>PreconditionFailed</Code></Error>');
            return;
          }
          objects.set(path, Buffer.from(bytes));
          res.writeHead(200, { 'content-type': 'application/xml' });
          res.end(
            `<CopyObjectResult><LastModified>2026-10-15T00:00:00Z</LastModified>` +
              `<ETag>${xmlEscape(etagOf(bytes))}</ETag></CopyObjectResult>`,
          );
          return;
        }
        const bytes = await readBody(req);
        const conflict = conditionalConflict(req, objects.get(path));
        if (conflict) {
          res.writeHead(conflict);
          res.end('<Error><Code>PreconditionFailed</Code></Error>');
          return;
        }
        objects.set(path, bytes);
        res.writeHead(200, { etag: etagOf(bytes) });
        res.end();
        return;
      }

      if (req.method === 'DELETE') {
        if (url.searchParams.has('uploadId')) {
          uploads.delete(url.searchParams.get('uploadId'));
          res.writeHead(204);
          res.end();
          return;
        }
        objects.delete(path);
        res.writeHead(204);
        res.end();
        return;
      }

      if (req.method === 'GET' || req.method === 'HEAD') {
        const bytes = objects.get(path);
        if (!bytes) {
          res.writeHead(404);
          res.end('<Error><Code>NoSuchKey</Code></Error>');
          return;
        }
        const headers = {
          'content-type': 'application/octet-stream',
          'last-modified': 'Wed, 15 Oct 2026 00:00:00 GMT',
          etag: etagOf(bytes),
        };
        const range = parseRange(req.headers.range, bytes.length);
        if (range) {
          const slice = bytes.subarray(range.start, range.end + 1);
          res.writeHead(206, {
            ...headers,
            'content-length': slice.length,
            'content-range': `bytes ${range.start}-${range.end}/${bytes.length}`,
          });
          res.end(req.method === 'HEAD' ? undefined : slice);
          return;
        }
        res.writeHead(200, { ...headers, 'content-length': bytes.length });
        res.end(req.method === 'HEAD' ? undefined : bytes);
        return;
      }

      res.writeHead(404);
      res.end('<Error><Code>NoSuchKey</Code></Error>');
    } catch (error) {
      res.writeHead(500);
      res.end(String(error));
    }
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;

  return {
    bucket,
    prefix,
    port,
    endpoint: `http://127.0.0.1:${port}`,
    objects,
    requests,
    reports,
    /** 次の source 読み出し (GET, 非 staging) を 1 回だけ保留する。 */
    holdNextSourceRead() {
      armed = true;
    },
    heldCount() {
      return held.length;
    },
    /** 保留した読み出しを解放し、元の内容を返す。 */
    releaseHolds() {
      for (const entry of held.splice(0)) entry.resolve();
    },
    /** staging 接頭辞のオブジェクトだけを残した一覧 (残留 staging の検出用)。 */
    stagedKeys() {
      return [...objects.keys()]
        .map((path) => path.slice(base.length))
        .filter((key) => isPrivateKey(key));
    },
    /** source (非 private) のオブジェクト一覧。 */
    sourceKeys() {
      return [...objects.keys()]
        .map((path) => path.slice(base.length))
        .filter((key) => !isPrivateKey(key));
    },
    async close() {
      this.releaseHolds();
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

function isPrivateKey(key) {
  return key.startsWith(STAGING_PREFIX) || key.startsWith(JOURNAL_PREFIX);
}

function conditionalConflict(req, existing) {
  const ifNoneMatch = req.headers['if-none-match'];
  if (ifNoneMatch === '*' && existing) return 412;
  const ifMatch = req.headers['if-match'];
  if (ifMatch && existing && etagOf(existing) !== ifMatch) return 412;
  return 0;
}

async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  return Buffer.concat(chunks);
}

function parseRange(header, size) {
  if (!header) return null;
  const match = /^bytes=(\d+)-(\d*)$/.exec(String(header));
  if (!match) return null;
  const start = Number(match[1]);
  const end = match[2] === '' ? size - 1 : Math.min(Number(match[2]), size - 1);
  if (start > end || start >= size) return null;
  return { start, end };
}

/**
 * EPUB (ZIP) の実バイト列を解析して entry 名 -> 内容を返す。
 * ZIP の central directory を読むので「成功した」だけでは済ませない。
 */
export function epubEntries(bytes) {
  let end = bytes.length - 22;
  while (end >= 0 && bytes.readUInt32LE(end) !== 0x06054b50) end -= 1;
  if (end < 0) throw new Error('EPUB must contain a ZIP central directory');
  const count = bytes.readUInt16LE(end + 10);
  let cursor = bytes.readUInt32LE(end + 16);
  const entries = new Map();
  for (let index = 0; index < count; index += 1) {
    if (bytes.readUInt32LE(cursor) !== 0x02014b50) {
      throw new Error('EPUB central directory is corrupt');
    }
    const method = bytes.readUInt16LE(cursor + 10);
    const compressedSize = bytes.readUInt32LE(cursor + 20);
    const nameLength = bytes.readUInt16LE(cursor + 28);
    const extraLength = bytes.readUInt16LE(cursor + 30);
    const commentLength = bytes.readUInt16LE(cursor + 32);
    const local = bytes.readUInt32LE(cursor + 42);
    const name = bytes.subarray(cursor + 46, cursor + 46 + nameLength).toString('utf8');
    const localNameLength = bytes.readUInt16LE(local + 26);
    const localExtraLength = bytes.readUInt16LE(local + 28);
    const dataStart = local + 30 + localNameLength + localExtraLength;
    const data = bytes.subarray(dataStart, dataStart + compressedSize);
    entries.set(name, method === 8 ? inflateRawSync(data) : Buffer.from(data));
    cursor += 46 + nameLength + extraLength + commentLength;
  }
  return entries;
}

/** EPUB 内の本文テキスト (XHTML) を連結して返す。 */
export function epubText(bytes) {
  const entries = epubEntries(bytes);
  const texts = [];
  for (const [name, content] of entries) {
    if (name.endsWith('.xhtml') || name.endsWith('.html')) texts.push(content.toString('utf8'));
  }
  return texts.join('\n');
}

/** EPUB が well-formed な ZIP として書けているかを検査する。 */
export function assertEpubShape(bytes) {
  const entries = epubEntries(bytes);
  const names = [...entries.keys()];
  if (!names.includes('mimetype')) throw new Error('EPUB must contain a mimetype entry');
  const mimetype = entries.get('mimetype').toString('utf8');
  if (mimetype !== 'application/epub+zip') {
    throw new Error(`EPUB mimetype must be application/epub+zip, got ${mimetype}`);
  }
  if (!names.some((name) => name.endsWith('.xhtml'))) {
    throw new Error('EPUB must contain at least one XHTML document');
  }
  if (!names.some((name) => name.endsWith('.opf'))) {
    throw new Error('EPUB must contain an OPF package document');
  }
  return entries;
}

/**
 * reader / writer の wrangler 設定を描画する。
 *
 * reader は 読み取り専用: queue producer/consumer・cron・RATE_LIMITER を持ち、
 * 共有 DB と S3、既存 writer への service binding だけを持つ。
 */
export function renderConfigs({
  work,
  writerName,
  readerName,
  s3Endpoint,
  bucket,
  prefix,
  main,
  assetsDirectory,
  migrationsDirectory,
  readerVars = {},
}) {
  const shared = (role, extraVars) =>
    [
      `main = ${JSON.stringify(main)}`,
      'compatibility_date = "2026-08-10"',
      'workers_dev = true',
      '',
      '[limits]',
      'cpu_ms = 300000',
      '',
      '[vars]',
      'NAROU_AUTH_REQUIRED = "true"',
      'NAROU_REQUIRE_S3 = "true"',
      `NAROU_WORKER_ROLE = ${JSON.stringify(role)}`,
      `S3_ENDPOINT = ${JSON.stringify(s3Endpoint)}`,
      'S3_REGION = "us-east-1"',
      `S3_BUCKET = ${JSON.stringify(bucket)}`,
      `S3_PREFIX = ${JSON.stringify(prefix)}`,
      'CONVERT_QUEUE_NAME = "narou-read-smoke-convert"',
      ...Object.entries(extraVars).map(([key, value]) => `${key} = ${JSON.stringify(value)}`),
      '',
      '[[d1_databases]]',
      'binding = "DB"',
      `database_name = ${JSON.stringify(bucket)}`,
      `database_id = ${JSON.stringify(bucket)}`,
      `migrations_dir = ${JSON.stringify(migrationsDirectory)}`,
    ].join('\n');

  const writer = [
    `name = ${JSON.stringify(writerName)}`,
    shared('writer', {}),
    '',
    // 静的アセットは writer だけ (reader は配信だけを持つ)。
    '[assets]',
    `directory = ${JSON.stringify(assetsDirectory)}`,
    'binding = "ASSETS"',
    'html_handling = "auto-trailing-slash"',
    'not_found_handling = "none"',
    '',
    '[[queues.producers]]',
    'binding = "NAROU_JOBS"',
    'queue = "narou-read-smoke-jobs"',
    '',
    '[[queues.producers]]',
    'binding = "CONVERT_QUEUE"',
    'queue = "narou-read-smoke-convert"',
    '',
    '[[queues.consumers]]',
    'queue = "narou-read-smoke-jobs"',
    'max_batch_size = 1',
    'max_retries = 3',
    '',
    '[[durable_objects.bindings]]',
    'name = "RATE_LIMITER"',
    'class_name = "SiteRateLimiter"',
    '',
    '[[durable_objects.bindings]]',
    'name = "PUSH_HUB"',
    'class_name = "PushHub"',
    '',
    '[[migrations]]',
    'tag = "v1"',
    'new_sqlite_classes = ["SiteRateLimiter"]',
    '',
    '[[migrations]]',
    'tag = "v2"',
    'new_sqlite_classes = ["PushHub"]',
    '',
  ].join('\n');

  const reader = [
    `name = ${JSON.stringify(readerName)}`,
    shared('reader', readerVars),
    '',
    '[[services]]',
    'binding = "NAROU_WRITER"',
    `service = ${JSON.stringify(writerName)}`,
    '',
    // 本番と同じく、reader は writer の PushHub 名前空間へ束縛する
    // (新しい名前空間を作らない)。script_name で既存 DO を指す。
    '[[durable_objects.bindings]]',
    'name = "PUSH_HUB"',
    'class_name = "PushHub"',
    `script_name = ${JSON.stringify(writerName)}`,
    '',
  ].join('\n');

  const writerPath = join(work, 'writer.toml');
  const readerPath = join(work, 'reader.toml');
  writeFileSync(writerPath, writer);
  writeFileSync(readerPath, reader);
  return { writerPath, readerPath, writer, reader };
}

/** 隔離した作業ディレクトリを作る (.wrangler/read-worker-*)。 */
export function createWorkspace() {
  mkdirSync(join(root, '.wrangler'), { recursive: true });
  const work = mkdtempSync(join(root, '.wrangler', 'read-worker-'));
  return {
    work,
    persist: join(work, 'state'),
    devVars: join(work, '.dev.vars'),
    cleanup() {
      // Windows では停止直後のファイルロックで失敗することがある。
      rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 });
    },
  };
}

/** wrangler dev を起動して readiness を待つ。 */
export async function startDev({ config, port, persist, cwd = root, env = environment }) {
  let logs = '';
  const configs = Array.isArray(config) ? config : [config];
  const child = spawn(npx, [
    '--yes', wranglerPackage, 'dev',
    ...configs.flatMap(path => ['--config', path]),
    '--persist-to', persist, '--port', String(port), '--ip', '127.0.0.1',
    '--show-interactive-dev-session', 'false',
  ], { cwd, shell: windows, detached: !windows, stdio: ['ignore', 'pipe', 'pipe'], env });
  for (const stream of [child.stdout, child.stderr]) {
    stream.on('data', chunk => { logs = (logs + chunk.toString()).slice(-200000); });
  }
  const stop = async () => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, 'exit');
    if (windows) {
      spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
    } else {
      try { process.kill(-child.pid, 'SIGTERM'); }
      catch (error) { if (error.code !== 'ESRCH') throw error; }
    }
    await exited;
  };
  const base = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 180000;
  try {
    for (;;) {
      if (child.exitCode !== null) throw new Error(`wrangler dev exited early:\n${logs}`);
      try {
        const response = await fetch(`${base}/health/live`, { signal: AbortSignal.timeout(1000) });
        if (response.status === 200) break;
      } catch {
        // The listener is not ready yet.
      }
      if (Date.now() > deadline) throw new Error(`wrangler dev did not become ready:\n${logs}`);
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    return { child, base, port, stop, get logs() { return logs; } };
  } catch (error) {
    await stop();
    throw error;
  }
}

/** 隔離した .dev.vars を書く (開発用の .dev.vars には触らない)。 */
export function writeDevVars(path, values) {
  const body = Object.entries(values)
    .map(([key, value]) => `${key}=${value}`)
    .join('\n');
  writeFileSync(path, `${body}\n`);
}

/** ビルド済み Worker 成果物の存在を確認する。 */
export function requireBuild() {
  const shim = join(root, 'build', 'worker', 'shim.mjs');
  const wasm = join(root, 'build', 'index_bg.wasm');
  for (const path of [shim, wasm]) {
    try {
      readFileSync(path);
    } catch {
      throw new Error(
        `missing build output ${path}. Run "worker-build --release" (worker_entry) before this smoke.`,
      );
    }
  }
  return { shim, wasm };
}
