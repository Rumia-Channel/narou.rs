// 読み取り配信 Worker (reader) と更新 Worker (writer) の分離 smoke。
//
//   node worker_entry/tests/read_worker.mjs            # ビルド済み成果物を使う
//   node worker_entry/tests/read_worker.mjs --keep     # 失敗後も成果物を残す
//
// 前提: `worker-build --release` (worker_entry) でビルド済みであること。
//
// 構成:
//   - 実際の Worker/Wasm (build/worker/shim.mjs) を 2 つ起動する。
//     reader は NAROU_WORKER_ROLE=reader と NAROU_WRITER service binding を持ち、
//     writer は NAROU_WORKER_ROLE=writer。1 つの workerd 内の独立した
//     2 Worker が、同じ D1 インスタンスと S3 fixture を共有する。
//   - S3 は HTTP fixture (実 GET/PUT/DELETE/LIST/multipart) が実際に Serve する。
//   - reader の D1 は probe wrapper が書き込みを拒否する (lazy 初期化も検出する)。
//
// 隔離: 設定・資格情報・D1 は .wrangler/read-worker-* に作る。
// 開発用の .dev.vars と .wrangler/state には触らない。
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import process from 'node:process';
import {
  assertEpubShape,
  createS3Fixture,
  createWorkspace,
  crc32,
  epubText,
  freePort,
  renderConfigs,
  requireBuild,
  root,
  runWrangler,
  startDev,
  writeDevVars,
} from './read_worker_fixture.mjs';

const keep = process.argv.includes('--keep');
const adminToken = 'read-worker-smoke-token';
const loginKey = Buffer.alloc(32, 7).toString('base64');
const allowedOrigin = 'https://books.example.test';
const bucket = 'narou-read-smoke';
const prefix = 'narou/smoke';
const writerName = 'narou-read-smoke-writer';
const readerName = 'narou-read-smoke-reader';


const steps = [];
async function check(name, fn, { fatal = false } = {}) {
  try {
    await fn();
    steps.push({ name, ok: true });
    console.log(`PASS ${name}`);
  } catch (error) {
    steps.push({ name, ok: false, error });
    console.error(`FAIL ${name}\n${error?.stack ?? error}`);
    if (fatal) throw error;
  }
}

// --- 投入するライブラリ ---------------------------------------------------

const NOVEL = {
  id: 1,
  sitename: 'Read Smoke',
  fileTitle: 'ReadNovel',
  title: 'Reader Smoke Novel',
  author: 'Smoke Author',
  ncode: 'n1111aa',
  tocUrl: 'https://example.test/n1111aa/',
  sections: [
    {
      index: '1',
      subtitle: '第一話',
      body: '読み取り配信用の第一話の本文です。',
      marker: '読み取り配信用の第一話の本文',
    },
    {
      index: '2',
      subtitle: '第二話',
      body: '読み取り配信用の第二話の本文です。',
      marker: '読み取り配信用の第二話の本文',
    },
  ],
};
const MEMBER = {
  id: 3,
  sitename: 'Read Smoke',
  fileTitle: 'ReadMember',
  title: 'Reader Member Novel',
  author: 'Smoke Author',
  ncode: 'n2222bb',
  tocUrl: 'https://example.test/n2222bb/',
  sections: [
    {
      index: '1',
      subtitle: '第三話',
      body: '読み取り配信用のメンバー第三話の本文です。',
      marker: '読み取り配信用のメンバー第三話の本文',
    },
  ],
};
const VIRTUAL = {
  id: 2,
  sitename: 'Read Smoke',
  fileTitle: 'ReadVirtual',
  title: 'Reader Virtual Novel',
  author: 'Smoke Author',
  ncode: 'vs_2',
  tocUrl: 'https://narou.rs/virtual/vs_2',
  sections: [],
  members: [NOVEL.id, MEMBER.id],
};

const novelPrefix = (novel) => `novels/${novel.sitename}/${novel.fileTitle}`;
const subtitleOf = (novel, section) => ({
  index: section.index,
  href: `${novel.tocUrl}${section.index}/`,
  chapter: '',
  subchapter: '',
  subtitle: section.subtitle,
  file_subtitle: section.subtitle,
  subdate: '2026-10-15',
});
const tocOf = (novel) => ({
  title: novel.title,
  author: novel.author,
  toc_url: novel.tocUrl,
  story: '',
  subtitles: novel.sections.map((section) => subtitleOf(novel, section)),
  novel_type: 1,
});
const sectionOf = (novel, section) => ({
  ...subtitleOf(novel, section),
  element: {
    data_type: 'html',
    introduction: '',
    // 挿絵参照を 1 個入れる。EPUB 変換が「挿絵を解決できない」警告を
    // PushHub へ配信する経路を通すため (reader → writer の DO 配信を観測する)。
    body: `<p>${section.body}</p><img src="挿絵/illust.png">`,
    postscript: '',
  },
});

/** 小説 1 件分の保存オブジェクト (論理 key -> 値)。 */
function novelObjects(novel) {
  const base = novelPrefix(novel);
  const objects = new Map();
  objects.set(`${base}/toc.yaml`, tocOf(novel));
  objects.set(`${base}/setting.ini`, 'enable_illust=true\n');
  objects.set(`${base}/replace.txt`, '');
  objects.set(`${base}/converter.yaml`, {});
  objects.set(`${base}/.illustration_cache.yaml`, {});
  for (const section of novel.sections) {
    objects.set(`${base}/本文/${section.index} ${section.subtitle}.yaml`, sectionOf(novel, section));
  }
  return objects;
}

const quote = (value) => `'${String(value).replaceAll("'", "''")}'`;

function novelInsert(novel) {
  return (
    `INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,` +
    `sitename,sitename_fold,last_update,ncode,ncode_fold,general_all_no,length,use_subdirectory) ` +
    `VALUES (${novel.id},${quote(novel.author)},${quote(novel.author.toLowerCase())},` +
    `${quote(novel.title)},${quote(novel.title.toLowerCase())},${quote(novel.fileTitle)},` +
    `${quote(novel.tocUrl)},${quote(novel.tocUrl)},${quote(novel.sitename)},${quote(novel.sitename.toLowerCase())},` +
    `'2026-10-15T00:00:00Z',${quote(novel.ncode)},${quote(novel.ncode)},${novel.sections.length},1200,0);`
  );
}

// --- 実行 -----------------------------------------------------------------

let workspace;
let fixture;
let writer;
let reader;
let exitCode = 1;

try {
  requireBuild();
  workspace = createWorkspace();
  fixture = await createS3Fixture({ bucket, prefix });

  const port = await freePort();
  const { writerPath, readerPath, reader: readerConfig } = renderConfigs({
    work: workspace.work,
    writerName,
    readerName,
    s3Endpoint: fixture.endpoint,
    bucket,
    prefix,
    main: join(root, 'tests', 'read_worker_writer.mjs').replaceAll('\\', '/'),
    assetsDirectory: join(root, 'public').replaceAll('\\', '/'),
    migrationsDirectory: join(root, 'migrations').replaceAll('\\', '/'),
    readerVars: {
      NAROU_READ_PROBE_REPORT: `${fixture.endpoint}/_probe/d1-write`,
    },
  });
  // reader は probe 経由 (D1 書き込みを拒否する wrapper) で起動する。
  const readerProbe = join(root, 'tests', 'read_worker_probe.mjs').replaceAll('\\', '/');
  writeFileSync(
    readerPath,
    readerConfig.replace(/^main = .*$/m, `main = ${JSON.stringify(readerProbe)}`),
  );

  writeDevVars(workspace.devVars, {
    NAROU_ADMIN_TOKEN: adminToken,
    NAROU_RS_LOGIN_KEY: loginKey,
    S3_ACCESS_KEY_ID: 'read-worker-key',
    S3_SECRET_ACCESS_KEY: 'read-worker-secret',
  });

  // --- 1. D1 の準備と投入 ------------------------------------------------
  await check(
    'migrations apply to an isolated local D1',
    () => {
      runWrangler(
        [
          'd1',
          'migrations',
          'apply',
          'DB',
          '--local',
          '--config',
          writerPath,
          '--persist-to',
          workspace.persist,
        ],
        { cwd: root },
      );
    },
    { fatal: true },
  );

  const seeded = [NOVEL, MEMBER, VIRTUAL];
  const allObjects = new Map();
  for (const novel of seeded) {
    for (const [key, value] of novelObjects(novel)) allObjects.set(key, value);
  }

  // --- 2. 投入 (legacy D1 と S3 参照の両方) -------------------------------
  await check(
    'seed novels, TOC, sections and a virtual novel',
    () => {
      // 先に content_backend を d1 にして、legacy 本文の投入 trigger を通す。
      const statements = [
        `INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','"d1"','d1');`,
        `INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','favorite_tags','{"tags":["favorites"]}','{"tags":["favorites"]}');`,
        `INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('global','external-api.allowed-origins','{}',${quote(allowedOrigin)});`,
        `INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','virtual_novel_members','{"2":[1,3]}','{"2":[1,3]}');`,
        novelInsert(NOVEL),
        novelInsert(MEMBER),
        novelInsert(VIRTUAL),
        'UPDATE novel_id_sequence SET next_id=4;',
      ];
      let blobIndex = 0;
      for (const [key, value] of allObjects) {
        const bytes = Buffer.from(typeof value === 'string' ? value : JSON.stringify(value));
        statements.push(
          `INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (${quote(key)},` +
            `${bytes.length},'2026-10-15T00:00:00+00:00','none',${crc32(bytes)},X'${bytes.toString('hex')}');`,
        );
        // 本文 (本文/ 配下) は content key なので S3 参照も作る。
        if (key.includes('/本文/')) {
          blobIndex += 1;
          const blobKey = `content/seed-${blobIndex}.bin`;
          const digest = createHash('sha256').update(bytes).digest('hex');
          statements.push(
            `INSERT INTO content_objects(object_key,blob_key,size,stored_size,encoding,crc32,sha256,updated_at,content_type,verified) ` +
              `VALUES (${quote(key)},${quote(blobKey)},${bytes.length},${bytes.length},'none',${crc32(bytes)},` +
              `${quote(digest)},'2026-10-15T00:00:00+00:00','application/yaml',1);`,
          );
          fixture.objects.set(`/${bucket}/${prefix}/${blobKey}`, bytes);
        }
      }
      // 移行済み相当へ切り替える (本文は S3 参照が優先される)。
      statements.push(
        `UPDATE app_state SET value_json='"s3"',value_yaml='s3' WHERE scope='inv' AND key='content_backend';`,
      );
      const file = join(workspace.work, 'seed.sql');
      writeFileSync(file, statements.join('\n'));
      runWrangler(
        [
          'd1',
          'execute',
          'DB',
          '--local',
          '--config',
          writerPath,
          '--persist-to',
          workspace.persist,
          '--file',
          file,
        ],
        { cwd: root },
      );
    },
    { fatal: true },
  );


  // --- 3. Worker を起動 ---------------------------------------------------
  await check(
    'writer and reader start as independent scripts',
    async () => {
      reader = await startDev({
        config: [readerPath, writerPath],
        port,
        persist: workspace.persist,
        cwd: root,
      });
      writer = { base: `${reader.base}/__writer` };
    },
    { fatal: true },
  );

  const fetchOn = (base, path, init = {}) => fetch(`${base}${path}`, {
    ...init,
    headers: { Connection: 'close', ...(init.headers ?? {}) },
    signal: init.signal ?? AbortSignal.timeout(120000),
  });

  // The running workerd owns D1; never start a CLI workerd against its SQLite.
  const sql = async (query) => {
    const response = await fetchOn(writer.base, '/__fixture__/sql', {
      method: 'POST',
      headers: { Authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' },
      body: JSON.stringify({ queries: Array.isArray(query) ? query : [query] }),
    });
    assert.equal(response.status, 200, await response.clone().text());
    return response.json();
  };
  const scalar = async query => Object.values((await sql(query))[0].results[0])[0];
  let tableNames;
  const snapshot = async () => {
    tableNames ??= (await sql(
      "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' ORDER BY name",
    ))[0].results.map(row => row.name);
    const results = await sql(tableNames.map(table => `SELECT * FROM "${table}"`));
    return Object.fromEntries(tableNames.map((table, index) => [
      table, results[index].results.map(row => JSON.stringify(row)).sort(),
    ]));
  };
  const admin = (base, path, init = {}) =>
    fetchOn(base, path, {
      ...init,
      headers: {
        Authorization: `Bearer ${adminToken}`,
        ...(init.headers ?? {}),
      },
    });

  const expectStatus = async (response, expected, label) => {
    if (response.status === expected) return;
    const body = await response.clone().text().catch(() => '');
    assert.equal(response.status, expected, `${label} must be ${expected}: ${body.slice(0, 400)}`);
  };

  // Writer readiness performs source initialization; reader readiness must not.
  await check(
    'both scripts can read the shared D1',
    async () => {
      for (const base of [writer.base, reader.base]) {
        const response = await admin(base, '/health/ready');
        await expectStatus(response, 200, `${base} readiness`);
        assert.equal((await response.json()).status, 'ready');
      }
    },
    { fatal: true },
  );

  // --- 4. 外部 API (reader が配信する) -----------------------------------
  let apiToken = null;
  await check('writer issues an external API token for the reader to use', async () => {
    const created = await admin(writer.base, '/api/external_api/token', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: 'read-worker-smoke' }),
    });
    await expectStatus(created, 200, 'POST /api/external_api/token');
    const body = await created.json();
    apiToken = body?.token?.token;
    assert.equal(typeof apiToken, 'string');
  });

  const external = (path, init = {}) =>
    fetchOn(reader.base, path, {
      ...init,
      headers: { Authorization: `Bearer ${apiToken}`, ...(init.headers ?? {}) },
    });

  await check('reader serves /api/v1 in json, jsonl and xml', async () => {
    const json = await external('/api/v1/novels?limit=10');
    await expectStatus(json, 200, 'GET /api/v1/novels');
    const payload = await json.json();
    assert(Array.isArray(payload?.data), 'the external list must return a data array');
    const titles = payload.data.map((entry) => entry.title);
    // 既定 (virtual=group) は Web UI と同じ 1 行にまとめるので、メンバー
    // ではなくバーチャル小説の行が並ぶ。
    assert(titles.includes(VIRTUAL.title), `the virtual novel must be listed: ${titles}`);
    assert(!titles.includes(NOVEL.title), `grouped members must not be listed: ${titles}`);

    const expanded = await external('/api/v1/novels?limit=10&virtual=expand');
    await expectStatus(expanded, 200, 'GET /api/v1/novels?virtual=expand');
    const expandedTitles = (await expanded.json()).data.map((entry) => entry.title);
    assert(
      expandedTitles.includes(NOVEL.title) && expandedTitles.includes(MEMBER.title),
      `expanded members must be listed individually: ${expandedTitles}`,
    );

    const jsonl = await external('/api/v1/novels?limit=10&format=jsonl');
    await expectStatus(jsonl, 200, 'GET /api/v1/novels?format=jsonl');
    const lines = (await jsonl.text()).split('\n').filter((line) => line.trim() !== '');
    assert(lines.length >= 1, `jsonl must contain one record per line: ${lines.length}`);
    for (const line of lines) JSON.parse(line);

    const xml = await external('/api/v1/novels?limit=10&format=xml');
    await expectStatus(xml, 200, 'GET /api/v1/novels?format=xml');
    const document = await xml.text();
    assert(document.startsWith('<?xml'), `xml output must be a document: ${document.slice(0, 40)}`);
    assert(document.includes(NOVEL.title), 'xml output must contain the novel title');
  });

  await check('reader refuses the external API without a credential', async () => {
    const response = await fetchOn(reader.base, '/api/v1/novels');
    await expectStatus(response, 401, 'GET /api/v1/novels without a token');
    const body = await response.text();
    assert(!body.includes(NOVEL.title), 'the library must not leak without a credential');
  });

  await check('reader serves the external API detail, TOC and section routes', async () => {
    const single = await external(`/api/v1/novels/${NOVEL.id}`);
    await expectStatus(single, 200, 'GET /api/v1/novels/1');
    const novel = await single.json();
    assert.equal(novel?.data?.title ?? novel?.title, NOVEL.title);

    const toc = await external(`/api/v1/novels/${NOVEL.id}/toc`);
    await expectStatus(toc, 200, 'GET /api/v1/novels/1/toc');
    const tocBody = await toc.text();
    for (const section of NOVEL.sections) {
      assert(tocBody.includes(section.subtitle), `the TOC must list ${section.subtitle}`);
    }

    const sections = await external(`/api/v1/novels/${NOVEL.id}/sections`);
    await expectStatus(sections, 200, 'GET /api/v1/novels/1/sections');
    const sectionsBody = await sections.text();
    for (const section of NOVEL.sections) {
      assert(sectionsBody.includes(section.body), `the body of ${section.subtitle} must be readable`);
    }
  });

  await check('reader serves MCP over POST', async () => {
    const response = await external('/api/v1/mcp', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/list' }),
    });
    await expectStatus(response, 200, 'POST /api/v1/mcp');
    const payload = await response.json();
    assert.equal(payload?.jsonrpc, '2.0');
    assert(Array.isArray(payload?.result?.tools), 'MCP must return its tool list');
  });

  // --- 5. ブラウザ read API (認証・CORS) ---------------------------------
  await check('reader enforces read API authentication', async () => {
    const anonymous = await fetchOn(reader.base, '/api/read/v1/novels');
    await expectStatus(anonymous, 401, 'GET /api/read/v1/novels without a credential');
    await anonymous.text();
    const authorized = await admin(reader.base, '/api/read/v1/novels');
    await expectStatus(authorized, 200, 'GET /api/read/v1/novels');
    const payload = await authorized.json();
    assert(Array.isArray(payload?.data), 'the read API must return a data array');
  });

  await check('reader applies the browser read API CORS policy', async () => {
    const allowed = await admin(reader.base, '/api/read/v1/novels', {
      headers: { Origin: allowedOrigin },
    });
    await expectStatus(allowed, 200, 'GET /api/read/v1/novels with an allowed origin');
    await allowed.text();
    assert.equal(
      allowed.headers.get('access-control-allow-origin'),
      allowedOrigin,
      'an allowed origin must be echoed back',
    );
    assert.equal(allowed.headers.get('access-control-allow-credentials'), 'true');

    const denied = await admin(reader.base, '/api/read/v1/novels', {
      headers: { Origin: 'https://attacker.example.test' },
    });
    await expectStatus(denied, 403, 'GET /api/read/v1/novels with a foreign origin');
    await denied.text();
    assert.equal(denied.headers.get('access-control-allow-origin'), null);

    const preflight = await fetchOn(reader.base, '/api/read/v1/novels', {
      method: 'OPTIONS',
      headers: {
        Origin: allowedOrigin,
        'Access-Control-Request-Method': 'GET',
        'Access-Control-Request-Headers': 'Range',
      },
    });
    await expectStatus(preflight, 204, 'OPTIONS /api/read/v1/novels');
    await preflight.text();
    assert.equal(preflight.headers.get('access-control-allow-origin'), allowedOrigin);

    const deniedPreflight = await fetchOn(reader.base, '/api/read/v1/novels', {
      method: 'OPTIONS',
      headers: {
        Origin: allowedOrigin,
        'Access-Control-Request-Method': 'POST',
      },
    });
    await expectStatus(deniedPreflight, 403, 'OPTIONS with a write method');
    await deniedPreflight.text();
  });

  await check('reader serves the browser favorite tags', async () => {
    const response = await admin(reader.base, '/api/read/v1/favorite-tags');
    await expectStatus(response, 200, 'GET /api/read/v1/favorite-tags');
    const payload = await response.json();
    assert.deepEqual(
      payload?.tags,
      ['favorites'],
      `unexpected favorite tags: ${JSON.stringify(payload)}`,
    );
  });

  // --- 6. EPUB (reader が生成する) ---------------------------------------
  const assertEpubBody = async (response, expected) => {
    await expectStatus(response, 200, 'EPUB response');
    assert.equal(response.headers.get('content-type'), 'application/epub+zip');
    const bytes = Buffer.from(await response.arrayBuffer());
    assertEpubShape(bytes);
    const text = epubText(bytes);
    for (const body of expected) {
      assert(text.includes(body), `the EPUB must contain ${body}`);
    }
    return bytes;
  };

  await check('reader serves the legacy EPUB route with real content', async () => {
    const response = await admin(reader.base, `/api/novels/${NOVEL.id}/download.epub`);
    await assertEpubBody(
      response,
      NOVEL.sections.map((section) => section.marker),
    );
  });

  await check('reader serves the UI EPUB route with real content', async () => {
    const response = await admin(reader.base, `/novels/${NOVEL.id}/download`);
    await assertEpubBody(
      response,
      NOVEL.sections.map((section) => section.marker),
    );
  });

  await check('reader serves the browser read API EPUB', async () => {
    const response = await admin(reader.base, `/api/read/v1/novels/${NOVEL.id}/epub`);
    await assertEpubBody(
      response,
      NOVEL.sections.map((section) => section.marker),
    );
  });

  await check('reader serves the browser read API EPUB for a virtual novel', async () => {
    const response = await fetchOn(
      reader.base,
      `/api/read/v1/novels/${VIRTUAL.id}/epub`,
      {
        headers: { Authorization: `Bearer ${adminToken}` },
        signal: AbortSignal.timeout(45000),
      },
    );
    await assertEpubBody(response, [
      ...NOVEL.sections.map((section) => section.marker),
      MEMBER.sections[0].marker,
    ]);
  });

  await check('reader serves the external API EPUB', async () => {
    const response = await external(`/api/v1/novels/${NOVEL.id}/epub`);
    await assertEpubBody(
      response,
      NOVEL.sections.map((section) => section.marker),
    );
  });

  await check('reader serves the virtual novel EPUB', async () => {
    const response = await admin(reader.base, `/api/virtual_novels/${VIRTUAL.id}/download.epub`);
    await assertEpubBody(response, [
      ...NOVEL.sections.map((section) => section.marker),
      MEMBER.sections[0].marker,
    ]);
  });

  await check('reader delivers EPUB console lines to the writer PushHub', async () => {

    // 挿絵を解決できない EPUB は警告行を PushHub へ配信する。その行が
    // writer 側の履歴に届くこと (= reader が writer の DO を更新できたこと)。
    const response = await admin(reader.base, `/api/novels/${NOVEL.id}/download.epub`);
    await assertEpubBody(
      response,
      NOVEL.sections.map((section) => section.marker),
    );

    let history = '';
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      const hub = await admin(writer.base, '/api/history?stream=stdout');
      await expectStatus(hub, 200, 'GET /api/history');
      history = await hub.text();
      if (history.includes('挿絵')) break;
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    assert(
      history.includes('挿絵'),
      `the reader EPUB warning must reach the writer PushHub: ${history.slice(0, 300)}`,
    );
  });

  // --- 7. 変更系は writer へ転送 (認証はそのまま) -------------------------
  await check('reader forwards mutations to the writer without bypassing auth', async () => {
    const anonymous = await fetchOn(reader.base, '/api/novels/freeze', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ids: [NOVEL.id] }),
    });
    await expectStatus(anonymous, 401, 'POST /api/novels/freeze without a credential');
    await anonymous.text();
    const rejected = await sql('SELECT COUNT(*) AS count FROM frozen_novels');
    assert.equal(rejected[0].results[0].count, 0, 'a rejected mutation must not reach the writer state');

    const authorized = await admin(reader.base, '/api/novels/freeze', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ids: [NOVEL.id] }),
    });
    await expectStatus(authorized, 200, 'POST /api/novels/freeze');
    await authorized.text();
    const applied = await sql(
      `SELECT COUNT(*) AS count FROM frozen_novels WHERE novel_id=${NOVEL.id};`,
    );
    assert.equal(
      applied[0].results[0].count,
      1,
      'the forwarded mutation must be applied by the writer',
    );

    const unfrozen = await admin(reader.base, '/api/novels/unfreeze', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ids: [NOVEL.id] }),
    });
    await expectStatus(unfrozen, 200, 'POST /api/novels/unfreeze');
    await unfrozen.text();
    const cleared = await sql(
      `SELECT COUNT(*) AS count FROM frozen_novels WHERE novel_id=${NOVEL.id};`,
    );
    assert.equal(
      cleared[0].results[0].count,
      0,
      'the forwarded unfreeze must be applied by the writer',
    );
  });

  // --- 8. reader は source を書き換えない --------------------------------
  await check('reader performs no D1 write and no S3 source write', async () => {
    const before = await snapshot();
    const s3Before = fixture.requests.length;
    const reportBefore = fixture.reports.length;

    const response = await admin(reader.base, `/api/novels/${NOVEL.id}/download.epub`);
    await assertEpubBody(
      response,
      NOVEL.sections.map((section) => section.marker),
    );
    const list = await external('/api/v1/novels?limit=10');
    await expectStatus(list, 200, 'GET /api/v1/novels');
    await list.text();

    const probe = await fetchOn(reader.base, '/__probe__/d1-writes');
    await expectStatus(probe, 200, 'GET /__probe__/d1-writes');
    const attempts = (await probe.json())?.attempts ?? [];
    assert.deepEqual(attempts, [], `the reader must not write D1: ${JSON.stringify(attempts)}`);
    assert.deepEqual(
      fixture.reports.slice(reportBefore),
      [],
      'the reader must not attempt a D1 write in any isolate',
    );

    const writes = fixture.requests
      .slice(s3Before)
      .filter((request) => request.method === 'PUT' || request.method === 'DELETE');
    const sourceWrites = writes.filter(
      (request) =>
        !request.logicalKey.startsWith('.narou-staging/') &&
        !request.logicalKey.startsWith('.narou-stage-journal/'),
    );
    assert.deepEqual(
      sourceWrites.map((request) => `${request.method} ${request.logicalKey}`),
      [],
      'the reader must not write or delete S3 source objects',
    );
    const staged = writes.filter(request => request.logicalKey.startsWith('.narou-staging/'));
    assert(staged.length > 0, 'the reader must stage EPUB data privately');

    assert.deepEqual(await snapshot(), before, 'the reader must not change the shared D1 database');
  });

  await check('reader removes its private staging objects after the EPUB', async () => {
    const deadline = Date.now() + 20000;
    let staged = fixture.stagedKeys();
    while (staged.length > 0 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 250));
      staged = fixture.stagedKeys();
    }
    assert.deepEqual(staged, [], 'private staging objects must be cleaned up');
  });

  // --- 9. writer の重い処理を保持しても reader は配信を続ける --------------
  await check('reader keeps serving while the writer holds a heavy operation', async () => {
    // writer の EPUB 生成|source fixture でブロックする (実 OOM は起こさない)。
    fixture.holdNextSourceRead();
    const blocked = fetch(
      `${writer.base}/api/novels/${NOVEL.id}/download.epub`,
      {
        headers: { Authorization: `Bearer ${adminToken}`, Connection: 'close' },
        signal: AbortSignal.timeout(120000),
      },
    ).catch((error) => error);
    const deadline = Date.now() + 60000;
    while (fixture.heldCount() === 0 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert(fixture.heldCount() > 0, 'the writer EPUB must be blocked on the source fixture');
    const busy = await admin(writer.base, `/api/novels/${NOVEL.id}/download.epub`);
    await expectStatus(busy, 503, 'a second heavy operation on the writer');
    assert.equal((await busy.json()).error, 'memory_capacity');

    const list = await external('/api/v1/novels?limit=10');
    await expectStatus(list, 200, 'GET /api/v1/novels while the writer is busy');
    const payload = await list.json();
    assert(Array.isArray(payload?.data));

    const epub = await admin(reader.base, `/api/novels/${NOVEL.id}/download.epub`);
    await assertEpubBody(
      epub,
      NOVEL.sections.map((section) => section.marker),
    );

    fixture.releaseHolds();
    const writerResult = await blocked;
    if (writerResult instanceof Error) {
      assert.fail(`the released writer request failed unexpectedly: ${writerResult.message}`);
    }
    await assertEpubBody(writerResult, NOVEL.sections.map(section => section.marker));
  });

  // --- 10. 配備 smoke (tests/read_worker_deployed.mjs) も同じ実 Worker で ----
  await check('the deployment smoke script passes against the reader', async () => {
    const script = join(root, 'tests', 'read_worker_deployed.mjs');
    const child = spawn(process.execPath, [script], {
      cwd: root,
      stdio: ['ignore', 'pipe', 'pipe'],
      env: {
        ...process.env,
        BASE_URL: reader.base,
        NAROU_ADMIN_TOKEN: adminToken,
        CF_ACCESS_CLIENT_ID: '',
        CF_ACCESS_CLIENT_SECRET: '',
      },
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    const status = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', resolve);
    });
    assert.equal(status, 0, `deployed reader smoke failed: ${stdout}\n${stderr}`);
    for (const line of stdout.trim().split('\n')) {
      console.log(`    ${line}`);
    }
  });

  // --- 11. 旧構成のマーカーが無い状態 -------------------------------------
  await check('reader keeps working without the legacy init markers', async () => {
    await sql("DELETE FROM app_state WHERE scope='inv' AND key IN ('content_backend','work_type_attributes_v2')");
    const before = await snapshot();

    const list = await external('/api/v1/novels?limit=10');
    await expectStatus(list, 200, 'GET /api/v1/novels');
    const payload = await list.json();
    assert(
      payload?.data?.some((entry) => entry.title === VIRTUAL.title),
      'the reader must still list the library',
    );

    const response = await admin(reader.base, `/api/novels/${NOVEL.id}/download.epub`);
    await assertEpubBody(response, NOVEL.sections.map(section => section.marker));

    const probe = await fetchOn(reader.base, '/__probe__/d1-writes');
    const attempts = (await probe.json())?.attempts ?? [];
    assert.deepEqual(attempts, [], `the reader must not write D1: ${JSON.stringify(attempts)}`);
    assert.deepEqual(
      fixture.reports.filter((report) => report.path.includes('d1-write')),
      [],
      'the reader must not attempt a D1 write in any isolate',
    );
    assert.equal(
      await scalar(
        "SELECT COUNT(*) AS count FROM app_state WHERE scope='inv' AND key='content_backend'",
      ),
      0,
      'the reader must not recreate the content_backend marker',
    );
    assert.deepEqual(await snapshot(), before, 'the reader must not change the shared D1 database');
  });

  const failed = steps.filter((step) => !step.ok);
  exitCode = failed.length === 0 ? 0 : 1;
  console.log(
    `${failed.length === 0 ? 'PASS' : 'FAIL'} read worker smoke: ` +
      `${steps.length - failed.length}/${steps.length} checks, ` +
      `${fixture.requests.length} S3 requests, ${fixture.reports.length} D1 write attempts`,
  );
  for (const step of failed) console.error(`failed check: ${step.name}`);
} catch (error) {
  console.error(error?.stack ?? error);
  exitCode = 1;
} finally {
    if (reader) await reader.stop();
    if (fixture) await fixture.close();
  if (workspace && !keep) workspace.cleanup();
  else if (workspace) console.log(`kept ${workspace.work}`);
}

process.exit(exitCode);
