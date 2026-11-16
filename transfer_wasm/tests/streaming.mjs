// Real wasm-bindgen Node bindings; build first, then pass the generated .js path.
// Exercises handles, bounded callbacks, validation-before-output, resume, and escaping
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
const readBody = async (index, path, offset, length, fileId) => {
  assert(index === 0 || index === 1); assert.equal(fileId, index); assert.equal(path, 'toc.yaml');
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
  const building = wasm.build_zip(ranges(metadata), String((await metadata.stat()).size), readBody,
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
  const info = JSON.parse(wasm.codec_info());
  assert.equal(info.protocol_version, 2); assert.equal(info.abi_version, 2); assert(info.build_id);
  assert.equal(wasm.parse_zip, undefined); assert.equal(wasm.plan_zip, undefined);
  const handle = await wasm.open_archive(ranges(archive), String(size), null);
  assert.throws(() => wasm.catalog_page(handle, null), /not validated/);
  const verified = JSON.parse(await wasm.validate_archive(handle, () => {}));
  assert.equal(verified.files, 2); assert.equal(verified.novels, 2);
  assert.equal(verified.total_uncompressed, String(body.length * 2));
  const page = JSON.parse(wasm.catalog_page(handle, null));
  assert.deepEqual(page.items.filter(item => item.kind === 'novel').map(item => item.title), ['one', 'two']);
  assert(page.items.filter(item => item.kind === 'file').every(item => item.sha256 === bodyHash && item.size === String(body.length)));
  const wireChunks = [];
  const summary = JSON.parse(await wasm.write_transfer_manifest(handle, JSON.stringify({ profile: 'runtime', extra_roles: [], policy_version: 1, allow_raw: true, novels: [1] }), outputChunks(bytes => wireChunks.push(Buffer.from(bytes)))));
  const wireBytes = Buffer.concat(wireChunks), wire = wireBytes.toString('utf8').trim().split('\n').map(JSON.parse);
  assert.equal(summary.wire_sha256, createHash('sha256').update(wireBytes).digest('hex'));
  assert.equal(summary.wire_size, String(wireBytes.length));
  assert.equal(summary.files, 1);
  assert(wire.filter(record => record.type === 'file').every(record => record.novel === 0 && record.file_id === 0));
  const prefix = createHash('sha256').update(body.subarray(0, 7)).digest('hex'), streamed = [];
  const resumed = JSON.parse(await wasm.stream_entry(handle, 0, '7', prefix, outputChunks(bytes => streamed.push(Buffer.from(bytes)))));
  assert.equal(resumed.sha256, bodyHash); assert.equal(resumed.emitted, String(body.length - 7));
  assert.deepEqual(Buffer.concat(streamed), body.subarray(7));
  let rejectedWrites = 0;
  await assert.rejects(wasm.stream_entry(handle, 0, '7', '0'.repeat(64), () => { rejectedWrites++; }));
  await assert.rejects(wasm.stream_entry(handle, 0, String(body.length + 1), prefix, () => { rejectedWrites++; }));
  assert.equal(rejectedWrites, 0);
  wasm.close_archive(handle);
  assert.throws(() => wasm.catalog_page(handle, null), /unknown archive/);
  const parsed = structuredClone(source);
  await close(archive);

  const wrongSource = structuredClone(source); wrongSource.novels[0].files[0].sha256 = '0'.repeat(64);
  const wrongPath = join(work, 'wrong-manifest.json'); await writeFile(wrongPath, JSON.stringify(wrongSource));
  const wrong = await file(wrongPath, 'r');
  await assert.rejects(wasm.build_zip(ranges(wrong), String((await wrong.stat()).size), readBody, () => {}), /sha256 mismatch/);
  await close(wrong);

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
  const corruptHandle = await wasm.open_archive(ranges(corrupt), String(damaged.length), null);
  await assert.rejects(wasm.validate_archive(corruptHandle, () => {}));
  await assert.rejects(wasm.write_transfer_manifest(corruptHandle, JSON.stringify({ policy_version: 1 }), async () => { published += 1; }));
  wasm.close_archive(corruptHandle);
  assert.equal(published, 0, 'late invalid entry must fail before any metadata output');
  await close(corrupt);

  // A parent may appear first in metadata; selecting it must include both
  // required members. References are ordered URLs, not source-library IDs.
  const parentSettings = Buffer.from('enable_yokogaki=true\n');
  const grouped = {
    format_version: 2,
    novels: [{
      toc_url: 'https://narou.rs/virtual/0123456789abcdef0123456789abcdef',
      title: '結合する作品', author: 'Tester', sitename: 'Group', novel_type: 0,
      virtual_members: [source.novels[1].toc_url, source.novels[0].toc_url],
      files: [{ path: 'setting.ini', size: parentSettings.length, sha256: createHash('sha256').update(parentSettings).digest('hex') }],
    }, ...source.novels],
  };
  const groupedMetadataPath = join(work, 'grouped-manifest.json');
  await writeFile(groupedMetadataPath, JSON.stringify(grouped));
  const groupedMetadata = await file(groupedMetadataPath, 'r');
  const groupedZipPath = join(work, 'grouped.zip');
  const groupedZip = await file(groupedZipPath, 'w');
  await wasm.build_zip(ranges(groupedMetadata), String((await groupedMetadata.stat()).size),
    async (index, path, offset, length) => {
      assert.equal(path, index === 0 ? 'setting.ini' : 'toc.yaml');
      assert(index >= 0 && index <= 2);
      return (index === 0 ? parentSettings : body).subarray(offset, offset + length);
    }, outputChunks(bytes => writeAll(groupedZip, bytes)));
  await close(groupedZip); await close(groupedMetadata);
  const groupedArchive = await file(groupedZipPath, 'r');
  const groupedSize = (await groupedArchive.stat()).size;
  const groupHandle = await wasm.open_archive(ranges(groupedArchive), String(groupedSize), null);
  await wasm.validate_archive(groupHandle, () => {});
  const groupedChunks = [];
  const groupSummary = JSON.parse(await wasm.write_transfer_manifest(groupHandle, JSON.stringify({ policy_version: 1, novels: [0] }), outputChunks(bytes => groupedChunks.push(Buffer.from(bytes)))));
  const groupRecords = Buffer.concat(groupedChunks).toString('utf8').trim().split('\n').map(JSON.parse);
  assert.equal(groupSummary.novels, 3, 'selecting the parent includes both required members');
  assert.deepEqual(groupRecords.filter(record => record.type === 'text' && record.field === 'member').map(record => record.value), grouped.novels[0].virtual_members);
  assert(groupRecords.filter(record => record.type === 'file').some(record => record.path === 'setting.ini' && record.sha256 === createHash('sha256').update(parentSettings).digest('hex')));
  wasm.close_archive(groupHandle);
  await close(groupedArchive);

  grouped.novels[0].virtual_members.push('https://example.test/missing/');
  const invalidMetadataPath = join(work, 'missing-member.json');
  await writeFile(invalidMetadataPath, JSON.stringify(grouped));
  const invalidMetadata = await file(invalidMetadataPath, 'r');
  let invalidWrites = 0;
  await assert.rejects(wasm.build_zip(ranges(invalidMetadata), String((await invalidMetadata.stat()).size), readBody,
    async () => { invalidWrites += 1; }));
  assert.equal(invalidWrites, 0, 'missing member must be rejected before ZIP output');
  await close(invalidMetadata);

  // Six MiB of escaped NULs serialize to 36 MiB. The former duplicated Value
  // trees + whole JSON String + JS return copy exceeded the Worker heap budget.
  parsed.novels.length = 1; parsed.novels[0].title = '';
  const halves = JSON.stringify(parsed).split('"title":""');
  assert.equal(halves.length, 2);
  const titlePrefix = Buffer.from(`${halves[0]}"title":"`), titleSuffix = Buffer.from(`"${halves[1]}`);
  const repeated = Buffer.from('\\u0000'.repeat(4096));
  const expected = createHash('sha256');
  const largePath = join(work, 'large-manifest.json');
  const largeOutput = await file(largePath, 'w');
  for (const bytes of [titlePrefix]) { expected.update(bytes); await writeAll(largeOutput, bytes); }
  for (let count = 0; count < 6 * 1024 * 1024 / 4096; count++) {
    expected.update(repeated); await writeAll(largeOutput, repeated);
  }
  expected.update(titleSuffix); await writeAll(largeOutput, titleSuffix); await close(largeOutput);
  const largeInput = await file(largePath, 'r');
  const largeZipPath = join(work, 'large.zip');
  const largeZip = await file(largeZipPath, 'w');
  await wasm.build_zip(ranges(largeInput), String((await largeInput.stat()).size), readBody,
    outputChunks(bytes => writeAll(largeZip, bytes)));
  await close(largeZip); await close(largeInput);
  const largeArchive = await file(largeZipPath, 'r');
  const largeHandle = await wasm.open_archive(ranges(largeArchive), String((await largeArchive.stat()).size), null);
  await wasm.validate_archive(largeHandle, () => {});
  const actual = createHash('sha256'), titleExpected = createHash('sha256');
  const zeroBlock = Buffer.alloc(32768); for (let index = 0; index < 192; index++) titleExpected.update(zeroBlock);
  let emitted = 0, titleBytes = 0, titleParts = 0, tail = '';
  const decoder = new TextDecoder();
  await wasm.write_transfer_manifest(largeHandle, JSON.stringify({ policy_version: 1 }), outputChunks(bytes => {
    emitted += bytes.length; tail += decoder.decode(bytes, { stream: true });
    for (let newline; (newline = tail.indexOf('\n')) !== -1;) {
      const record = JSON.parse(tail.slice(0, newline)); tail = tail.slice(newline + 1);
      if (record.type === 'text' && record.field === 'title') {
        assert.equal(record.part, titleParts++); const value = Buffer.from(record.value);
        assert(value.length <= 32768); actual.update(value); titleBytes += value.length;
      }
    }
  }));
  assert.equal(tail, ''); assert.equal(titleBytes, 6 * 1024 * 1024); assert.equal(titleParts, 192);
  assert.equal(actual.digest('hex'), titleExpected.digest('hex'), 'fragmented wire metadata preserves all escaped content');
  wasm.close_archive(largeHandle);
  const heapBytes = instance.exports.memory.buffer.byteLength;
  assert(heapBytes < 64 * 1024 * 1024, `metadata serialization amplified Wasm heap to ${heapBytes}`);
  console.log(JSON.stringify({ result: 'PASS', wasm_heap_bytes: heapBytes, metadata_bytes: emitted,
    max_read_chunk: maximumRead, max_write_chunk: maximumWrite,
    checks: ['backpressure', 'SHA/size roundtrip', 'dense projected file IDs', 'prefix-hash resume', 'virtual selection dependencies with ordered URLs', 'invalid virtual graph before output', 'validation before output', 'escaped metadata without whole JSON copies'] }));
} finally {
  await Promise.allSettled([...handles].map(value => value.close()));
  await rm(work, { recursive: true, force: true });
}
