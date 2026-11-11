// Worker のタグ登録順 (`app_state` の `tag_sequence`) の契約テスト。
//
// 連番表の更新は作品と同じ D1 batch の中に入れる。batch は 1 トランザクション
// なので、作品と順序が同時に確定し、片方だけ確定した状態にならない。
//
// ここで確かめること:
//   * 取り込み (apply_batch) でタグを付けたとき、連番表に並ぶ
//   * 並行した取り込みでも連番表が壊れない (更新の競合は再試行する)
//   * 「登録順」の並びで、付けた順番が保たれる
//
//   node tests/tag_sequence.mjs      # run.mjs から呼ばれる
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import process from 'node:process';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';

const BASE_URL = process.env.BASE_URL ?? 'http://127.0.0.1:8787';
const TOKEN = process.env.NAROU_ADMIN_TOKEN ?? 'local-contract-token';
const execFileAsync = promisify(execFile);

function auth() {
  return { Authorization: `Bearer ${TOKEN}` };
}

async function json(response) {
  const text = await response.text();
  try {
    return text ? JSON.parse(text) : null;
  } catch {
    return text;
  }
}

async function request(path, init = {}) {
  return fetch(`${BASE_URL}${path}`, init);
}

// --- ZIP (stored) ------------------------------------------------------------

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
    local.writeUInt16LE(0x0800, 6);
    local.writeUInt16LE(0, 8);
    local.writeUInt16LE(0, 10);
    local.writeUInt16LE(0x21, 12);
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
    directory.writeUInt32LE(0, 38);
    directory.writeUInt32LE(offset, 42);
    central.push(Buffer.concat([directory, name]));
    locals.push(header);
    offset += header.length;
  }
  const centralBytes = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralBytes.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, centralBytes, end]);
}

function sha256(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

// --- アーカイブ --------------------------------------------------------------

function tocYaml() {
  return [
    'title: 順序テスト',
    'author: 順序テスト',
    'sitename: 例サイト',
    'domain: example.com',
    'ncode: n00000000',
    'story: あらすじ',
    'novel_type: 1',
    '',
  ].join('\n');
}

function sectionYaml(index, name) {
  return [
    '---',
    `index: '${index}'`,
    `href: https://example.com/${index}/`,
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
    '    本文です。',
    '',
  ].join('\n');
}

/** 1 作品だけのアーカイブを作る (タグは 1 個だけ)。 */
function archiveFor(tocUrl, tag, index, stamp) {
  const files = [
    { path: 'toc.yaml', data: Buffer.from(tocYaml()) },
    {
      path: `本文/${index} 第一話.yaml`,
      data: Buffer.from(sectionYaml(index, '第一話')),
    },
  ];
  const manifest = {
    format_version: 1,
    app_version: '0.5.0',
    exported_at: '2026-10-29T00:00:00Z',
    novels: [
      {
        toc_url: tocUrl,
        title: `順序テスト ${index}`,
        author: '順序テスト',
        sitename: '例サイト',
        domain: 'example.com',
        tags: [tag],
        novel_type: 1,
        // 保存先は ncode から作るので、実行ごとに違う値にする (前回の
        // 取り込みと衝突して保存先に.exists になるのを避ける)。
        ncode: `n${stamp}${index}`,
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

async function importArchive(zip) {
  const deadline = Date.now() + 30_000;
  for (;;) {
    const response = await request('/api/transfer/import', {
      method: 'POST',
      headers: { 'Content-Type': 'application/zip', ...auth() },
      body: zip,
    });
    const body = await json(response);
    if (response.status === 503 && body.error === 'memory_capacity' && Date.now() < deadline) {
      const delay = Number(response.headers.get('Retry-After'));
      assert.ok(Number.isFinite(delay) && delay > 0 && delay <= 10, 'capacity response must give a bounded retry delay');
      await new Promise(resolve => setTimeout(resolve, delay * 1000));
      continue;
    }
    assert.equal(response.status, 200, `import failed: ${JSON.stringify(body)}`);
    assert.equal(body.success, true, `import failed: ${JSON.stringify(body)}`);
    return body;
  }
}

async function tagList() {
  const response = await request('/api/tag_list?format=json', { headers: auth() });
  const body = await json(response);
  assert.equal(response.status, 200, `tag list failed: ${JSON.stringify(body)}`);
  return body.tags ?? [];
}

// --- 検証 -------------------------------------------------------------------

// 「登録順」で並べる。
const settings = await request('/api/global_setting', {
  method: 'POST',
  headers: { 'content-type': 'application/json', ...auth() },
  body: JSON.stringify({ settings: { 'webui.tag-sort-order': 'registered' } }),
});
const settingsBody = await json(settings);
assert.equal(settings.status, 200, `settings save failed: ${JSON.stringify(settingsBody)}`);
assert.equal(settingsBody.success, true, `settings save failed: ${JSON.stringify(settingsBody)}`);

const stamp = Date.now();
const firstTag = `順序テスト${stamp}-1`;
const secondTag = `順序テスト${stamp}-2`;

// 1 作品ずつ、順番に取り込む。
await importArchive(archiveFor(`https://example.com/order-${stamp}-1/`, firstTag, 1, stamp));
await importArchive(archiveFor(`https://example.com/order-${stamp}-2/`, secondTag, 2, stamp));

let tags = await tagList();
const firstAt = tags.indexOf(firstTag);
const secondAt = tags.indexOf(secondTag);
assert(firstAt >= 0, `the imported tag is missing: ${JSON.stringify(tags)}`);
assert(secondAt >= 0, `the imported tag is missing: ${JSON.stringify(tags)}`);
assert(firstAt < secondAt, `the registration order must follow the import order: ${JSON.stringify(tags)}`);
console.log('PASS the registration order follows the import order');

// 4 作品を並行して取り込む。連番表の更新が競合しても、作品と順序が
// 同時に確定するので、取り込みはどれも成功し、連番表も壊れない。
const parallel = [];
for (let index = 3; index < 7; index += 1) {
  const tag = `順序テスト${stamp}-${index}`;
  parallel.push({ tag, zip: archiveFor(`https://example.com/order-${stamp}-${index}/`, tag, index, stamp) });
}
await Promise.all(parallel.map((entry) => importArchive(entry.zip)));

tags = await tagList();
for (const entry of parallel) {
  assert(tags.includes(entry.tag), `a concurrently imported tag is missing: ${entry.tag}`);
}
console.log('PASS concurrent imports keep the registration order consistent');

// 連続标签 (まとめて 1 回で取り込む) でも、連番表は順番を保つ。
const batchTag = `順序テスト${stamp}-batch`;
await importArchive(archiveFor(`https://example.com/order-${stamp}-batch/`, batchTag, 9, stamp));
tags = await tagList();
assert(tags.indexOf(batchTag) > Math.max(...parallel.map(entry => tags.indexOf(entry.tag))),
  `a later tag must follow every completed import: ${JSON.stringify(tags)}`);
console.log('PASS a later import is appended to the registration order');

async function favoriteMutation(body) {
  const response = await request('/api/favorite_tags', {
    method: 'POST', headers: { 'content-type': 'application/json', ...auth() },
    body: JSON.stringify(body),
  });
  const result = await json(response);
  assert.equal(response.status, 200, JSON.stringify(result));
  assert.equal(result.success, true, JSON.stringify(result));
  return result.tags;
}

async function setTagOrder(order) {
  const response = await request('/api/global_setting', {
    method: 'POST', headers: { 'content-type': 'application/json', ...auth() },
    body: JSON.stringify({ settings: { 'webui.tag-sort-order': order } }),
  });
  const result = await json(response);
  assert.equal(response.status, 200, JSON.stringify(result));
  assert.equal(result.success, true, JSON.stringify(result));
}

const favorites = await favoriteMutation({ action: 'add', tags: [firstTag, secondTag] });
await favoriteMutation({
  action: 'reorder',
  tags: [secondTag, firstTag, ...favorites.filter(tag => tag !== firstTag && tag !== secondTag)],
});
for (const order of ['favorites_first', 'alphabetical', 'favorites_first', 'alphabetical']) {
  await setTagOrder(order);
  tags = await tagList();
  if (order === 'favorites_first') {
    assert.deepEqual(tags.slice(0, 2), [secondTag, firstTag]);
  } else {
    assert(tags.indexOf(firstTag) < tags.indexOf(secondTag), JSON.stringify(tags));
  }
}
console.log('PASS favorite changes and alphabetical round trips update the tag list');

const concurrentFavorites = parallel.map(entry => entry.tag);
await Promise.all(concurrentFavorites.map(tag => favoriteMutation({ action: 'add', tags: [tag] })));
const favoriteResponse = await request('/api/favorite_tags', { headers: auth() });
const favoriteState = await json(favoriteResponse);
assert.equal(favoriteResponse.status, 200);
for (const tag of concurrentFavorites) {
  assert(favoriteState.tags.includes(tag), `a concurrent favorite addition was lost: ${tag}`);
}
console.log('PASS concurrent favorite additions are reapplied after CAS conflicts');

const rejected = await request('/api/favorite_tags', {
  method: 'POST', headers: { 'content-type': 'application/json', ...auth() },
  body: JSON.stringify({ action: 'unknown', tags: [batchTag] }),
});
assert.equal(rejected.status, 400);
assert.deepEqual(await json(await request('/api/favorite_tags', { headers: auth() })), favoriteState);
await setTagOrder('registered');
console.log('PASS an invalid favorite action leaves the saved state unchanged');

// Only the isolated local runner exposes its dedicated database. Force real
// snapshot conflicts with SQLite triggers; scheduling alone need not overlap.
if (process.env.NAROU_TEST_CONFIG && process.env.NAROU_TEST_PERSIST) {
  assert(['127.0.0.1', 'localhost'].includes(new URL(BASE_URL).hostname));
  const config = process.env.NAROU_TEST_CONFIG;
  const persist = process.env.NAROU_TEST_PERSIST;
  const queryFile = join(dirname(config), 'favorite-fixture.sql');
  const quote = value => `'${value.replaceAll("'", "''")}'`;
  // Keep the HTTP client's event loop running while Wrangler prepares SQL.
  async function sql(command) {
    writeFileSync(queryFile, command);
    const result = await execFileAsync(process.platform === 'win32' ? 'npx.cmd' : 'npx', [
      '--yes', `wrangler@${process.env.NAROU_WRANGLER_VERSION || '4.151.3'}`,
      'd1', 'execute', 'DB', '--local', '--config', config,
      '--persist-to', persist, '--file', queryFile, '--json',
    ], { encoding: 'utf8', shell: process.platform === 'win32' });
    return JSON.parse(result.stdout.slice(result.stdout.indexOf('[\n')));
  }
  const replace = async raw => (await sql(`INSERT INTO app_state(scope,key,value_json,value_yaml)
    VALUES ('inv','favorite_tags',${quote(raw)},${quote(raw)})
    ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml;`));
  const original = JSON.stringify({ tags: favoriteState.tags });
  try {
    (await replace(JSON.stringify([firstTag, '1023', '2'])));
    assert.deepEqual(await favoriteMutation({ action: 'add', tags: [secondTag] }),
      [firstTag, '1023', '2', secondTag]);
    console.log('PASS a legacy D1 favorite array supports read-modify-write');

    const corrupt = '{"tags":"not-a-list"}';
    (await replace(corrupt));
    await setTagOrder('favorites_first');
    for (const path of ['/api/favorite_tags', '/api/tag_list?format=json']) {
      assert.equal((await request(path, { headers: auth() })).status, 503, path);
    }
    const mutation = await request('/api/favorite_tags', {
      method: 'POST', headers: { 'content-type': 'application/json', ...auth() },
      body: JSON.stringify({ action: 'add', tags: [batchTag] }),
    });
    assert.equal(mutation.status, 503);
    assert.equal((await sql("SELECT value_json FROM app_state WHERE scope='inv' AND key='favorite_tags'"))[0].results[0].value_json, corrupt);
    console.log('PASS malformed D1 favorites are reported without being erased');

    const initial = JSON.stringify({ tags: [firstTag] });
    const competing = JSON.stringify({ tags: [firstTag, secondTag] });
    (await replace(initial));
    (await sql(`CREATE TRIGGER favorite_update_conflict BEFORE UPDATE OF value_json ON app_state
      WHEN OLD.scope='inv' AND OLD.key='favorite_tags' AND OLD.value_json=${quote(initial)}
        AND NEW.value_json<>${quote(competing)}
      BEGIN
        UPDATE app_state SET value_json=${quote(competing)}, value_yaml=${quote(competing)}
          WHERE scope='inv' AND key='favorite_tags';
        SELECT RAISE(IGNORE);
      END;`));
    assert.deepEqual(await favoriteMutation({ action: 'add', tags: [batchTag] }),
      [firstTag, secondTag, batchTag]);
    (await sql('DROP TRIGGER favorite_update_conflict;'));
    console.log('PASS a forced CAS conflict preserves the intervening addition');

    (await sql("DELETE FROM app_state WHERE scope='inv' AND key='favorite_tags';"));
    const firstInsert = JSON.stringify({ tags: [secondTag] });
    (await sql(`CREATE TRIGGER favorite_insert_conflict BEFORE INSERT ON app_state
      WHEN NEW.scope='inv' AND NEW.key='favorite_tags' AND NEW.value_json<>${quote(firstInsert)}
        AND NOT EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='favorite_tags')
      BEGIN
        INSERT INTO app_state(scope,key,value_json,value_yaml)
          VALUES ('inv','favorite_tags',${quote(firstInsert)},${quote(firstInsert)});
        SELECT RAISE(IGNORE);
      END;`));
    assert.deepEqual(await favoriteMutation({ action: 'add', tags: [batchTag] }), [secondTag, batchTag]);
    console.log('PASS a competing first insert is preserved on retry');
  } finally {
    (await sql('DROP TRIGGER IF EXISTS favorite_update_conflict; DROP TRIGGER IF EXISTS favorite_insert_conflict;'));
    (await replace(original));
    rmSync(queryFile, { force: true });
    await setTagOrder('registered');
  }
}
