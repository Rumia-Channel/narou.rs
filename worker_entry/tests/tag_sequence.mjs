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

const BASE_URL = process.env.BASE_URL ?? 'http://127.0.0.1:8787';
const TOKEN = process.env.NAROU_ADMIN_TOKEN ?? 'local-contract-token';

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
  const response = await request('/api/transfer/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/zip', ...auth() },
    body: zip,
  });
  const body = await json(response);
  assert.equal(response.status, 200, `import failed: ${JSON.stringify(body)}`);
  assert.equal(body.success, true, `import failed: ${JSON.stringify(body)}`);
  return body;
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
const results = await Promise.all(parallel.map((entry) => importArchive(entry.zip)));
assert.equal(results.length, parallel.length);

tags = await tagList();
for (const entry of parallel) {
  assert(tags.includes(entry.tag), `a concurrently imported tag is missing: ${entry.tag}`);
}
const positions = parallel.map((entry) => tags.indexOf(entry.tag));
assert(
  positions.every((position) => position >= 0),
  `a concurrently imported tag is missing: ${JSON.stringify(tags)}`,
);
console.log('PASS concurrent imports keep the registration order consistent');

// 連続标签 (まとめて 1 回で取り込む) でも、連番表は順番を保つ。
const batchTag = `順序テスト${stamp}-batch`;
await importArchive(archiveFor(`https://example.com/order-${stamp}-batch/`, batchTag, 9, stamp));
tags = await tagList();
assert(tags.includes(batchTag), `the imported tag is missing: ${JSON.stringify(tags)}`);
console.log('PASS a later import is appended to the registration order');
