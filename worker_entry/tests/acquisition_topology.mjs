// 取得 (acquisition) レーン構成の共通 fixture。
//
// 本番と同じ形 (acquisition lane contract):
//   - writer 1 本: Web/API/cron、変換キュー、旧取得キュー (drain-only 入口)。
//   - 取得専用 Worker 4 本: それぞれ自分の取得キューだけを消費する。
//     DB / オブジェクトストア / writer の Durable Object 名前空間は共有。
//   - どの script も同じ Wasm 成果物を使う。
//
// ここでは本番の判断を一切代用しない。キューも D1 台帳も admission も
// settlement も実 Worker の中だけで完結する。fixture が用意するのは
// 隔離した設定・隔離したストレージ・delivery の観測 sink だけ。
import http from 'node:http';
import { once } from 'node:events';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { freePort, runWrangler, startDev, writeDevVars } from './read_worker_fixture.mjs';

export const LANE_COUNT = 4;
export const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const COMPATIBILITY_DATE = '2026-08-10';

/** lane 0..3 の取得キュー名 (`narou-jobs-acquire-<lane+1>-<target>`)。 */
export function acquisitionQueueName(target, lane) {
  return `narou-jobs-acquire-${lane + 1}-${target}`;
}

/**
 * writer + 取得レーン 4 本の設定を描画する。
 *
 * - lane には NAROU_JOBS (旧入口) を結線しない。旧入口は writer だけが drain する。
 * - lane には cron / assets / 独自の DO 名前空間を作らない。
 * - 4 本の producer 束縛 (ACQUISITION_QUEUE_0..3) は writer と lane の双方に
 *   必須とする。割り当ての送信先は常に 4 本のどれかになる。
 */
export function renderAcquisitionTopology({
  work,
  target = 'local',
  main,
  assetsDirectory = join(root, 'public').replaceAll('\\', '/'),
  migrationsDirectory = join(root, 'migrations').replaceAll('\\', '/'),
  s3Endpoint = 'http://127.0.0.1:9000',
  bucket = 'narou-local',
  prefix = 'narou/local',
  controlOrigin = '',
  databaseName = `narou-acq-${target}`,
  vars = {},
}) {
  const writerName = `narou-acq-${target}-writer`;
  const jobsQueue = `narou-jobs-${target}`;
  const convertQueue = `narou-jobs-convert-${target}`;

  const shared = (extraVars) => [
    `main = ${JSON.stringify(main)}`,
    `compatibility_date = ${JSON.stringify(COMPATIBILITY_DATE)}`,
    'compatibility_flags = ["enable_request_signal"]',
    'workers_dev = true',
    '',
    '[limits]',
    'cpu_ms = 300000',
    '',
    '[vars]',
    ...Object.entries({
      NAROU_AUTH_REQUIRED: 'true',
      CONVERT_QUEUE_NAME: convertQueue,
      S3_ENDPOINT: s3Endpoint,
      S3_REGION: 'us-east-1',
      S3_BUCKET: bucket,
      S3_PREFIX: prefix,
      ...(controlOrigin ? { NAROU_ACQUISITION_FIXTURE_ORIGIN: controlOrigin } : {}),
      ...vars,
      ...extraVars,
    }).map(([key, value]) => `${key} = ${JSON.stringify(value)}`),
    '',
    '[[d1_databases]]',
    'binding = "DB"',
    `database_name = ${JSON.stringify(databaseName)}`,
    `database_id = ${JSON.stringify(databaseName)}`,
    `migrations_dir = ${JSON.stringify(migrationsDirectory)}`,
    '',
  ].join('\n');

  const producers = (entries) => entries.flatMap(([binding, queue]) => [
    '[[queues.producers]]',
    `binding = ${JSON.stringify(binding)}`,
    `queue = ${JSON.stringify(queue)}`,
    '',
  ]);

  const consumer = (queue) => [
    '[[queues.consumers]]',
    `queue = ${JSON.stringify(queue)}`,
    'max_batch_size = 1',
    'max_retries = 3',
    `dead_letter_queue = ${JSON.stringify(`${queue}-dlq`)}`,
    'max_concurrency = 1',
    '',
  ];

  const acquisitionProducers = Array.from(
    { length: LANE_COUNT },
    (_, lane) => [`ACQUISITION_QUEUE_${lane}`, acquisitionQueueName(target, lane)],
  );

  const writer = [
    `name = ${JSON.stringify(writerName)}`,
    shared({ NAROU_WORKER_ROLE: 'writer' }),
    '[assets]',
    `directory = ${JSON.stringify(assetsDirectory)}`,
    'binding = "ASSETS"',
    'html_handling = "auto-trailing-slash"',
    'not_found_handling = "none"',
    '',
    ...producers([['NAROU_JOBS', jobsQueue], ['CONVERT_QUEUE', convertQueue], ...acquisitionProducers]),
    ...consumer(jobsQueue),
    ...consumer(convertQueue),
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
    '[triggers]',
    'crons = ["* * * * *"]',
    '',
  ].join('\n');

  const laneNames = [];
  const laneConfigs = [];
  for (let lane = 0; lane < LANE_COUNT; lane += 1) {
    const name = `narou-acq-${target}-acquire-${lane + 1}`;
    laneNames.push(name);
    laneConfigs.push([
      `name = ${JSON.stringify(name)}`,
      shared({ NAROU_WORKER_ROLE: 'acquisition', NAROU_ACQUISITION_LANE: String(lane) }),
      ...producers([['CONVERT_QUEUE', convertQueue], ...acquisitionProducers]),
      ...consumer(acquisitionQueueName(target, lane)),
      '[[services]]',
      'binding = "NAROU_WRITER"',
      `service = ${JSON.stringify(writerName)}`,
      '',
      '[[durable_objects.bindings]]',
      'name = "RATE_LIMITER"',
      'class_name = "SiteRateLimiter"',
      `script_name = ${JSON.stringify(writerName)}`,
      '',
      '[[durable_objects.bindings]]',
      'name = "PUSH_HUB"',
      'class_name = "PushHub"',
      `script_name = ${JSON.stringify(writerName)}`,
      '',
    ].join('\n'));
  }

  const writerPath = join(work, 'writer.toml');
  const lanePaths = laneConfigs.map((_, index) => join(work, `acquire-${index + 1}.toml`));
  writeFileSync(writerPath, writer);
  lanePaths.forEach((path, index) => writeFileSync(path, laneConfigs[index]));
  return {
    writerPath,
    lanePaths,
    // writer を先頭に置く: 開発サーバの入口スクリプトになる。
    configs: [writerPath, ...lanePaths],
    writerName,
    laneNames,
    jobsQueue,
    convertQueue,
  };
}

/** 隔離した作業ディレクトリ (.wrangler/acquisition-*) を作る。 */
export function createAcquisitionWorkspace() {
  mkdirSync(join(root, '.wrangler'), { recursive: true });
  const work = mkdtempSync(join(root, '.wrangler', 'acquisition-'));
  return {
    work,
    persist: join(work, 'state'),
    devVars: join(work, '.dev.vars'),
    cleanup() {
      rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 });
    },
  };
}

/**
 * 取得レーン構成を起動する。
 *
 * 5 scripts (writer + 4 lanes) share one workerd and the same local D1.
 * Queue consumers remain independent per script, with one heavy operation each.
 */
export async function startAcquisitionTopology({ configs, port, persist, extraArgs = [] }) {
  return startDev({ config: configs, port, persist, extraArgs });
}

/** ローカル D1 にマイグレーションを適用する。 */
export function applyMigrations({ config, persist }) {
  runWrangler(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
}

/** 隔離した .dev.vars を書く (開発用の .dev.vars には触らない)。 */
export function writeTopologyDevVars(path, values) {
  writeDevVars(path, values);
}

/**
 * delivery 観測 sink。
 *
 * 取得 Worker は delivery を受けた script 名・lane 番号・envelope をここへ
 * 送るだけ。判定も ack も一切しない。
 */
export function createObservationSink() {
  const deliveries = [];
  const sendGates = new Map();
  const server = http.createServer(async (req, res) => {
    try {
      const path = new URL(req.url, 'http://127.0.0.1').pathname;
      if (req.method !== 'POST' || !['/__fixture/observation', '/__fixture/send'].includes(path)) {
        res.writeHead(404);
        res.end();
        return;
      }
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const payload = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      if (path === '/__fixture/send') {
        const gate = sendGates.get(payload.binding === 'CONVERT_QUEUE' ? 'convert' : payload.body.lane);
        if (gate) {
          await new Promise(resolve => {
            const entry = { body: payload.body, release: resolve };
            gate.push(entry);
            res.once('close', resolve);
          });
        }
      } else {
        deliveries.push({ ...payload, at: Date.now() });
      }
      if (!res.destroyed) {
        res.writeHead(204);
        res.end();
      }
    } catch (error) {
      if (!res.headersSent) res.writeHead(500);
      res.end(String(error));
    }
  });
  return {
    deliveries,
    async listen() {
      server.listen(0, '127.0.0.1');
      await once(server, 'listening');
      const port = server.address().port;
      return { port, origin: `http://127.0.0.1:${port}` };
    },
    holdConvertSend() {
      if (sendGates.has('convert')) throw new Error('conversion send barrier already armed');
      sendGates.set('convert', []);
    },
    heldConvertSends() {
      return sendGates.get('convert') ?? [];
    },
    releaseConvertSend() {
      const held = sendGates.get('convert') ?? [];
      sendGates.delete('convert');
      for (const entry of held) entry.release();
    },
    holdLaneSend(lane) {
      if (sendGates.has(lane)) throw new Error(`lane ${lane} send barrier already armed`);
      sendGates.set(lane, []);
    },
    heldSends(lane) {
      return sendGates.get(lane) ?? [];
    },
    releaseLaneSend(lane) {
      const held = sendGates.get(lane) ?? [];
      sendGates.delete(lane);
      for (const entry of held) entry.release();
    },
    async close() {
      for (const lane of sendGates.keys()) this.releaseLaneSend(lane);
      server.closeAllConnections();
      if (server.listening) await new Promise(resolve => server.close(resolve));
    },
  };
}

/**
 * サイト系列ごとの実 HTTP ソース。
 *
 * 1 系列 = 1 ポート。系列ごとに独立した rate-limit bucket と独立した
 * 受付記録を持つ。応答は gate で保持できる (production は実際に upstream を
 * 待つ状態になる。合成の失敗や即時応答は返さない)。
 */
export async function startFamilySource({ family }) {
  const requests = [];
  let gate = null;

  const handler = async (req, res) => {
    try {
      const path = new URL(req.url, 'http://127.0.0.1').pathname;
      requests.push({ method: req.method, path, at: Date.now() });
      if (gate && req.method === 'GET') {
        const current = gate;
        await new Promise(resolve => {
          const entry = { path, respond: resolve };
          current.holds.push(entry);
          res.once('close', () => {
            const index = current.holds.indexOf(entry);
            if (index >= 0) current.holds.splice(index, 1);
            resolve();
          });
        });
        if (res.destroyed) return;
      }
      const episode = new RegExp(`^/${family}/[^/]+/(\\d+)/$`).exec(path);
      if (episode) {
        res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
        res.end(`<div class="body"><p>${family} 第${episode[1]}話の本文です。</p></div>`);
        return;
      }
      if (new RegExp(`^/${family}/[^/]+/$`).test(path)) {
        res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
        const novel = path.split('/')[2];
        res.end(
          `<h1>${family} ${novel} Story</h1><span class="author">Source Author</span>` +
          `<a href="/${family}/${novel}/1/">第1話</a>`,
        );
        return;
      }
      res.writeHead(404, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end('<h1>Not Found</h1>');
    } catch (error) {
      if (!res.headersSent) res.writeHead(500);
      res.end(String(error));
    }
  };

  const server = http.createServer(handler);
  const serverV6 = http.createServer(handler);
  let port;
  try {
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    port = server.address().port;
    serverV6.listen(port, '::1');
    await once(serverV6, 'listening');
  } catch (error) {
    server.closeAllConnections();
    serverV6.closeAllConnections();
    await Promise.all([server, serverV6].filter(entry => entry.listening)
      .map(entry => new Promise(resolve => entry.close(resolve))));
    throw error;
  }

  return {
    family,
    port,
    origin: `http://localhost:${port}`,
    requests,
    /** 以後の GET 応答を保留する (保留中は応答しない)。 */
    hold() {
      gate = { holds: [] };
    },
    /** 保留中の応答を解放する。gate 自体も外す。 */
    release() {
      const holds = gate ? gate.holds.splice(0) : [];
      gate = null;
      for (const hold of holds) hold.respond();
    },
    /** まだ応答していない保留要求の数。 */
    pending() {
      return gate ? gate.holds.length : 0;
    },
    /** この系列への要求数。 */
    count() {
      return requests.length;
    },
    paths() {
      return requests.map(request => request.path);
    },
    async close() {
      if (gate) {
        for (const hold of gate.holds.splice(0)) hold.respond();
        gate = null;
      }
      server.closeAllConnections();
      serverV6.closeAllConnections();
      await Promise.all([
        new Promise(resolve => server.close(resolve)),
        new Promise(resolve => serverV6.close(resolve)),
      ]);
    },
  };
}

/** 系列 1 件分のサイト定義 (YAML)。 */
export function familySiteDefinition({ family, novel, port }) {
  const origin = `http://localhost:${port}`;
  return [
    `name: ${family} ${novel}`,
    'domain: localhost',
    // 同じ系列の小説は別 YAML でも同じ site_key を持つ。
    `site_family: ${family}`,
    `top_url: ${origin}`,
    `url: '${origin}/${family}/(?<ncode>${family}-${novel})/?'`,
    `sitename: ${family}`,
    'version: 1.0',
    'confirm_over18: no',
    `toc_url: '\\k<top_url>/${family}/\\k<ncode>/'`,
    `title: '<h1>(?<title>.+?)</h1>'`,
    `author: '<span class="author">(?<author>.+?)</span>'`,
    `subtitles: '<a href="(?<href>/${family}/${family}-${novel}/(?<index>\\d+)/)">(?<subtitle>.+?)</a>'`,
    `body_pattern: '<div class="body">(?<body>.+?)</div>'`,
    'min_interval: 0.01',
    '',
  ].join('\n');
}

/** 未使用ポートを 1 つ取る。 */
export async function unusedPort() {
  return freePort();
}

/** 隔離した設定の main (観測 wrapper) の絶対パス。 */
export function observerMain() {
  return join(root, 'tests', 'acquisition_worker.mjs').replaceAll('\\', '/');
}

/**
 * 設定から queue consumer を取り除く。
 *
 * 「起動直後に crash した envelope を再配送させない」検証用。writer の旧
 * 入口と変換キュー、4 本の lane consumer の全部を外す。
 */
export function stripQueueConsumers(configPath) {
  const toml = readFileSync(configPath, 'utf8');
  writeFileSync(configPath, toml.replace(/\[\[queues\.consumers\]\][\s\S]*?(?=\n\[|$)/g, ''));
}

/** 隔離した D1 に初期設定を書く (SQL ファイル経由)。 */
export function seedSql({ config, persist, sql }) {
  const file = join(dirname(config), 'seed.sql');
  writeFileSync(file, sql);
  runWrangler(['d1', 'execute', 'DB', '--local', '--config', config, '--persist-to', persist, '--file', file]);
}

/** オフライン SQL (workerd が同じ SQLite に触っていない時だけ)。 */
export function offlineSql({ config, persist, sql }) {
  const file = join(dirname(config), 'query.sql');
  writeFileSync(file, sql);
  const output = runWrangler([
    'd1', 'execute', 'DB', '--local', '--config', config, '--persist-to', persist, '--file', file, '--json',
  ]);
  const start = output.indexOf('[\n');
  if (start < 0) throw new Error(`unexpected d1 output: ${output}`);
  return JSON.parse(output.slice(start))[0].results;
}
