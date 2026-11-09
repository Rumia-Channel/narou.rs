// Real wasm-bindgen Node bindings; build first, then pass the generated .js path.
// Exercises bounded callbacks, validation-before-output, planning, and escaping
// amplification without materializing the large manifest in JavaScript either.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, open, readFile, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';

assert(process.argv[2], 'usage: node transfer_wasm/tests/streaming.mjs <generated-bindings.js>');
const bindings = resolve(process.argv[2]);
const OriginalInstance = WebAssembly.Instance;
let instance;
WebAssembly.Instance = class extends OriginalInstance {
  constructor(...args) { super(...args); instance = this; }
};
let wasm;
try { wasm = createRequire(import.meta.url)(bindings); }
finally { WebAssembly.Instance = OriginalInstance; }
assert(instance?.exports.memory instanceof WebAssembly.Memory, 'measure the executing Wasm instance');
const work = await mkdtemp(join(dirname(bindings), 'transfer-stream-'));
const handles = new Set();
let maximumRead = 0, maximumWrite = 0, fileReads = 0;
async function file(path, mode) { const value = await open(path, mode); handles.add(value); return value; }
async function close(value) { await value.close(); handles.delete(value); }
async function writeAll(output, bytes) {
  for (let offset = 0; offset < bytes.length;) {
    const { bytesWritten } = await output.write(bytes, offset, bytes.length - offset);
    assert(bytesWritten > 0, 'disk output must advance');
    offset += bytesWritten;
  }
}
function ranges(input) {
  return async (offset, length) => {
    assert(Number.isSafeInteger(offset) && offset >= 0);
    assert(Number.isSafeInteger(length) && length > 0 && length <= 64 * 1024);
    maximumRead = Math.max(maximumRead, length);
    const bytes = Buffer.alloc(length);
    for (let filled = 0; filled < length;) {
      const { bytesRead } = await input.read(bytes, filled, length - filled, offset + filled);
      assert(bytesRead > 0, 'immutable range must exist');
      filled += bytesRead;
    }
    return bytes;
  };
}
function outputChunks(accept) {
  return async bytes => {
    assert(bytes instanceof Uint8Array && bytes.length > 0 && bytes.length <= 64 * 1024);
    maximumWrite = Math.max(maximumWrite, bytes.length);
    await accept(bytes);
  };
}
const body = Buffer.from('title: Streaming fixture\nauthor: Tester\nsubtitles: []\n');
const bodyHash = createHash('sha256').update(body).digest('hex');
const readBody = async (index, path, offset, length) => {
  assert(index === 0 || index === 1); assert.equal(path, 'toc.yaml');
  assert(length <= 64 * 1024); fileReads += 1;
  return body.subarray(offset, offset + length);
};
try {
  const source = {
    format_version: 1, app_version: 'streaming-test', exported_at: '2026-01-01T00:00:00Z',
    novels: ['one', 'two'].map(name => ({
      toc_url: `https://example.test/${name}/`, title: name, author: 'Tester',
      sitename: 'Fixture', domain: 'example.test', tags: ['fixture'], novel_type: 1,
      files: [{ path: 'toc.yaml', size: body.length, sha256: bodyHash }],
    })),
  };
  const metadataPath = join(work, 'manifest.json');
  await writeFile(metadataPath, JSON.stringify(source));
  const metadata = await file(metadataPath, 'r');
  const zipPath = join(work, 'archive.zip');
  const zip = await file(zipPath, 'w');
  let entered, release, writes = 0;
  const waiting = new Promise(resolve => { entered = resolve; });
  const gate = new Promise(resolve => { release = resolve; });
  const building = wasm.build_zip(ranges(metadata), (await metadata.stat()).size, readBody,
    outputChunks(async bytes => {
      if (writes++ === 0) { entered(); await gate; }
      await writeAll(zip, bytes);
    }));
  await waiting;
  const pausedReads = fileReads;
  await new Promise(setImmediate);
  assert.equal(fileReads, pausedReads, 'paused output must stop upstream file reads');
  release(); await building; await close(zip); await close(metadata);
  const archive = await file(zipPath, 'r');
  const size = (await archive.stat()).size;
  const smallChunks = [];
  await wasm.parse_zip(ranges(archive), size, outputChunks(bytes => { smallChunks.push(Buffer.from(bytes)); }));
  const parsed = JSON.parse(Buffer.concat(smallChunks).toString('utf8'));
  assert.deepEqual(parsed.novels.map(novel => novel.title), ['one', 'two']);
  assert(parsed.novels.every(novel => novel.files[0].sha256 === bodyHash && novel.files[0].size === body.length));
  for (const overwrite of [false, true]) {
    const decisions = [];
    await wasm.plan_zip(ranges(archive), size, async (index, url) => {
      assert.equal(url, source.novels[index].toc_url); return index === 0;
    }, overwrite, outputChunks(bytes => { decisions.push(Buffer.from(bytes)); }));
    assert.deepEqual(Buffer.concat(decisions).toString('utf8').trim().split('\n').map(JSON.parse), [
      { index: 0, action: overwrite ? 'Overwrite' : 'Skip' }, { index: 1, action: 'Create' },
    ]);
  }
  await close(archive);

  // Corrupt the second file: validating the first must not publish metadata yet.
  const damaged = await readFile(zipPath);
  const name = damaged.indexOf(Buffer.from('novels/1/toc.yaml'));
  const header = name - 30;
  assert(header >= 0 && damaged.readUInt32LE(header) === 0x04034b50);
  const data = header + 30 + damaged.readUInt16LE(header + 26) + damaged.readUInt16LE(header + 28);
  damaged[data] ^= 0xff;
  const corruptPath = join(work, 'corrupt.zip'); await writeFile(corruptPath, damaged);
  const corrupt = await file(corruptPath, 'r');
  let published = 0;
  await assert.rejects(wasm.parse_zip(ranges(corrupt), damaged.length, async () => { published += 1; }));
  assert.equal(published, 0, 'late invalid entry must fail before any metadata output');
  await close(corrupt);

  // Six MiB of escaped NULs serialize to 36 MiB. The former duplicated Value
  // trees + whole JSON String + JS return copy exceeded the Worker heap budget.
  parsed.novels.length = 1; parsed.novels[0].title = '';
  const halves = JSON.stringify(parsed).split('"title":""');
  assert.equal(halves.length, 2);
  const prefix = Buffer.from(`${halves[0]}"title":"`), suffix = Buffer.from(`"${halves[1]}`);
  const repeated = Buffer.from('\\u0000'.repeat(4096));
  const expected = createHash('sha256');
  const largePath = join(work, 'large-manifest.json');
  const largeOutput = await file(largePath, 'w');
  for (const bytes of [prefix]) { expected.update(bytes); await writeAll(largeOutput, bytes); }
  for (let count = 0; count < 6 * 1024 * 1024 / 4096; count++) {
    expected.update(repeated); await writeAll(largeOutput, repeated);
  }
  expected.update(suffix); await writeAll(largeOutput, suffix); await close(largeOutput);
  const largeInput = await file(largePath, 'r');
  const largeZipPath = join(work, 'large.zip');
  const largeZip = await file(largeZipPath, 'w');
  await wasm.build_zip(ranges(largeInput), (await largeInput.stat()).size, readBody,
    outputChunks(bytes => writeAll(largeZip, bytes)));
  await close(largeZip); await close(largeInput);
  const largeArchive = await file(largeZipPath, 'r');
  const actual = createHash('sha256');
  let emitted = 0;
  await wasm.parse_zip(ranges(largeArchive), (await largeArchive.stat()).size,
    outputChunks(bytes => { emitted += bytes.length; actual.update(bytes); }));
  assert.equal(actual.digest('hex'), expected.digest('hex'), 'streamed metadata must preserve all escaped content');
  const heapBytes = instance.exports.memory.buffer.byteLength;
  assert(heapBytes < 64 * 1024 * 1024, `metadata serialization amplified Wasm heap to ${heapBytes}`);
  console.log(JSON.stringify({ result: 'PASS', wasm_heap_bytes: heapBytes, metadata_bytes: emitted,
    max_read_chunk: maximumRead, max_write_chunk: maximumWrite,
    checks: ['backpressure', 'SHA/size roundtrip', 'skip/overwrite/create', 'validation before output', 'escaped metadata without whole JSON copies'] }));
} finally {
  await Promise.allSettled([...handles].map(value => value.close()));
  await rm(work, { recursive: true, force: true });
}
