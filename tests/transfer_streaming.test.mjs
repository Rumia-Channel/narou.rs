import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import { createHash, randomUUID, webcrypto } from 'node:crypto';
import { MessageChannel } from 'node:worker_threads';

const workerSource = fs.readFileSync(new URL('../src/web/assets/js/ui/transfer-worker.js', import.meta.url), 'utf8');
const uiSource = fs.readFileSync(new URL('../src/web/assets/js/ui/transfer.js', import.meta.url), 'utf8');
const relaySource = fs.readFileSync(new URL('../src/web/assets/js/ui/transfer-download-sw.js', import.meta.url), 'utf8');
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const emptyHash = hash(new Uint8Array());
const json = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { 'content-type': 'application/json' } });
const tick = () => new Promise(setImmediate);
const gate = () => { let resolve; const promise = new Promise((value) => { resolve = value; }); return { promise, resolve }; };

function workerHarness() {
  const events = [], requests = [];
  const h = { ui: async () => null, fetch: async () => json({ error: { code: 'unexpected_request', message: 'Unexpected request' } }, 400) };
  const scope = vm.createContext({
    TextEncoder, TextDecoder, Uint8Array, ArrayBuffer, DataView, Blob, Response, Headers, URL,
    AbortController, setTimeout, clearTimeout, console, structuredClone,
    crypto: { subtle: webcrypto.subtle, getRandomValues: (bytes) => webcrypto.getRandomValues(bytes), randomUUID },
    btoa: (text) => Buffer.from(text, 'binary').toString('base64'),
    self: { location: new URL('https://example.test/assets/transfer/worker.js'), postMessage(message, transfer = []) {
      const delivered = structuredClone(message, { transfer }); events.push(delivered);
      if (delivered.type === 'request') {
        Promise.resolve().then(() => h.ui(delivered.name, delivered.payload)).then(
          (value) => h.api.resolvePending(h.ctx, { id: delivered.id, ok: true, value }),
          (error) => h.api.resolvePending(h.ctx, { id: delivered.id, ok: false, error: { code: error.code, message: error.message } }));
      }
    } },
    fetch: async (url, init) => { requests.push({ url, init }); return h.fetch(url, init); },
    loadGlue: async () => h.glue,
  });
  // Only the module loader is injected; production control flow and codec ABI checks run unchanged.
  vm.runInContext(workerSource.replace(/^export /gm, '').replace('await import(manifest.glue_url)', 'await loadGlue(manifest.glue_url)'), scope);
  h.api = vm.runInContext('({ LIMITS, MANIFEST_FILE_ID, ByteSpool, SessionStore, encodeRecord, decodeFrames, BatchBuilder, canonicalSelection, summarizeCatalog, selectionDigest, adoptStatus, initWorker, runImport, runExport, runCommand, handleMessage, uploadFiles, uploadManifest, sendBatch, sessionOperation, recoverCreation, restoreSessionRecord, pendingSessions, discardSession, resolvePending, readBoundedBody, createExportReader, createImportSession })', scope);
  h.ctx = { limits: { ...h.api.LIMITS }, abort: false, abortListeners: new Set(), controller: new AbortController(),
    store: new h.api.SessionStore(null), pendingUi: new Map(), uiSequence: 0, codec: { build_id: 'test-build' },
    capabilities: { protocol_version: 2, build: { wasm_build_id: 'test-build', abi_version: '2' },
      limits: { binary_body_bytes: h.api.LIMITS.binaryBodyBytes, chunk_bytes: h.api.LIMITS.chunkBytes, codec_io_bytes: h.api.LIMITS.codecIoBytes,
        batch_records: 100, record_header_bytes: 512, control_body_bytes: 256 * 1024, manifest_bytes: 64 * 1024 * 1024, manifest_metadata_bytes: 24 * 1024 * 1024, page_items: 100 },
      storage: { policy_version: 1, unpersistable_roles: [], import: { allowed: true }, export: { allowed: true } }, creation_ticket: 'ticket' },
    requestId: '', sessionId: '', revision: '', direction: '', secretHeader: '', status: null };
  // Retry tests use a deterministic clock; no network request or codec operation is bypassed.
  vm.runInContext('sleep = async () => {};', scope);
  Object.assign(h, { scope, events, requests });
  return h;
}

function status(h, fields = {}) {
  return { protocol_version: 2, session_id: 'session', revision: '1', state: 'transferring', direction: 'import',
    confirmed_bytes: '0', chunk_bytes: String(h.ctx.limits.chunkBytes), next_action: 'upload', operation_id: null,
    manifest_confirmed_bytes: '0', manifest_next_chunk_index: 0, catalog_revision: '7', resume_mode: 'continue', ...fields };
}
function decisions(send, selected = false, reason = null) { return { send, default: selected, reason }; }
function catalogFile(role, size, entry = 0) {
  const optional = role === 'raw' || role === 'history';
  return { kind: 'file', entry, novel: 0, path: `${role}/${entry}`, size: String(size), role, sha256: emptyHash,
    runtime: optional ? decisions('optional') : decisions('required'),
    complete: optional ? decisions('optional', true) : decisions('required') };
}
function binary(bytes, fields = {}) {
  return new Response(bytes, { headers: { 'content-type': 'application/octet-stream',
    'x-transfer-body-bytes': String(bytes.byteLength), ...fields } });
}
function receipt(record, confirmed) {
  return { file_id: record.header.file_id, chunk_index: record.header.chunk_index, outcome: record.header.final ? 'sealed' : 'accepted',
    confirmed_bytes: String(confirmed), confirmed_prefix_sha256: hash(record.payload) };
}

test('actual codec_info is called and compared to the compiled artifact identity', async () => {
  const h = workerHarness(); let inspected = 0;
  h.glue = Object.fromEntries(['open_archive', 'validate_archive', 'catalog_page', 'write_transfer_manifest', 'stream_entry', 'close_archive', 'build_zip'].map((name) => [name, () => {}]));
  h.glue.default = async () => {};
  h.glue.codec_info = () => { inspected += 1; return JSON.stringify({ protocol_version: 2, abi_version: 2, build_id: 'wrong-build' }); };
  const manifest = { protocol_version: 2, abi_version: 2, build_id: 'test-build',
    glue_url: 'https://example.test/glue.js', wasm_url: 'https://example.test/codec.wasm', worker_url: 'https://example.test/worker.js' };
  await assert.rejects(h.api.initWorker(h.ctx, manifest, h.ctx.capabilities), (error) => error.code === 'abi_mismatch');
  assert.equal(inspected, 1); assert.equal(h.ctx.wasm, undefined); assert.equal(h.requests.length, 0);
});

test('late local ZIP validation failure performs no create, upload or publication request', async () => {
  const h = workerHarness(); let closed = false, projected = false;
  h.ctx.wasm = {
    open_archive: async (read, size) => { assert.equal(size, '16'); assert.equal((await read(0, 16)).byteLength, 16); return 1; },
    validate_archive: async (_handle, report) => { report(1, 2); throw new Error('last entry CRC mismatch'); },
    write_transfer_manifest: async () => { projected = true; }, close_archive: () => { closed = true; },
  };
  await assert.rejects(h.api.runImport(h.ctx, { file: new Blob([new Uint8Array(16)]) }), /CRC mismatch/);
  assert.equal(h.requests.length, 0); assert.equal(projected, false); assert.equal(closed, true);
});

test('selection occurs after full verification and before any server create/body request', async () => {
  const h = workerHarness(); let verified = false;
  h.ctx.wasm = { open_archive: async () => 1, validate_archive: async () => { verified = true;
    return JSON.stringify({ fingerprint: emptyHash, files: 3, novels: 1, total_uncompressed: '30' }); },
    catalog_page: () => JSON.stringify({ items: [catalogFile('body', 10), catalogFile('raw', 10, 1), catalogFile('unknown', 10, 2)], next_cursor: null }), close_archive() {},
  };
  h.ui = async (name, payload) => {
    assert.equal(name, 'import-select'); assert.equal(verified, true); assert.equal(h.requests.length, 0);
    assert.equal(payload.byProfile.runtime.roles.find((role) => role.role === 'unknown').requiredFiles, 1);
    return null;
  };
  await assert.rejects(h.api.runImport(h.ctx, { file: new Blob(['source']) }), (error) => error.code === 'aborted');
  assert.equal(h.requests.length, 0);
});

test('canonical selection has extra_roles/allow_raw, never a second roles-map convention', () => {
  const h = workerHarness();
  const selected = h.api.canonicalSelection({ profile: 'runtime', roles: ['raw', 'history', 'generated', 'body', 'unknown'] }, 4, new Set(['raw']));
  assert.deepEqual(JSON.parse(JSON.stringify(selected)), { profile: 'runtime', extra_roles: ['history'], policy_version: 4, allow_raw: false });
  const summary = h.api.summarizeCatalog([catalogFile('body', 1), catalogFile('illustration_index', 2), catalogFile('unknown', 3), catalogFile('raw', 4)], 'runtime');
  assert.equal(summary.roles.filter((role) => role.requiredFiles).length, 3);
  assert.equal(summary.roles.find((role) => role.role === 'raw').optionalDefault, 0);
});

test('reselected ZIP fingerprint mismatch stops without creating another identity', async () => {
  const h = workerHarness();
  await h.ctx.store.put({ requestId: 'saved', direction: 'imports', sessionId: 'old-session', secretHeader: 'secret', fingerprint: 'a'.repeat(64) });
  h.ctx.wasm = { open_archive: async () => 1, validate_archive: async () => JSON.stringify({ fingerprint: 'b'.repeat(64) }), close_archive() {} };
  await assert.rejects(h.api.runImport(h.ctx, { requestId: 'saved', file: new Blob(['changed']) }), (error) => error.code === 'source_mismatch');
  assert.equal(h.requests.length, 0); assert.equal((await h.ctx.store.all()).length, 1);
});

test('catalog digest binds the revision and every logical identity in shared canonical order', async () => {
  const h = workerHarness();
  const items = [{ novel_ordinal: 1, logical_path: 'unknown/note', size: '3', role: 'unknown' },
    { novel_ordinal: 0, logical_path: '本文/1.yaml', size: '17', role: 'body' }];
  const rows = items.map((item) => `${item.novel_ordinal}\0${item.logical_path}\0${item.size}\0${item.role}`).sort();
  const expected = hash(`v2\0${'7'}\0runtime\0history,raw\0${rows.map((row) => `${row}\x01`).join('')}`);
  assert.equal(await h.api.selectionDigest('runtime', ['raw', 'history'], '7', items), expected);
  assert.notEqual(await h.api.selectionDigest('runtime', ['raw', 'history'], '8', items), expected);
});

test('catalog digest uses Rust UTF-8 lexical order for supplementary Unicode paths', async () => {
  const h = workerHarness();
  const items = ['😀', '\uE000'].map((path) => ({ novel_ordinal: 0, logical_path: path, size: '1', role: 'unknown' }));
  const rows = items.map((item) => `${item.novel_ordinal}\0${item.logical_path}\0${item.size}\0${item.role}`)
    .sort((left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right)));
  assert.equal(await h.api.selectionDigest('runtime', [], '1', items),
    hash(`v2\0${'1'}\0runtime\0\0${rows.map((row) => `${row}\x01`).join('')}`));
});

test('framing enforces payload, header, body and record budgets including truncated boundaries', () => {
  const h = workerHarness(), header = { file_id: 0, chunk_index: 0, final: true, sha256: emptyHash };
  const frame = h.api.encodeRecord(header, new Uint8Array(h.api.LIMITS.chunkBytes));
  assert.ok(frame.byteLength <= h.api.LIMITS.binaryBodyBytes);
  const builder = new h.api.BatchBuilder();
  for (let index = 0; index < 100; index += 1) assert.equal(builder.addRecord({ ...header, file_id: index }, new Uint8Array()), true);
  assert.equal(builder.addRecord(header, new Uint8Array()), false);
  assert.equal(h.api.decodeFrames(builder.build()).length, 100);
  assert.throws(() => h.api.decodeFrames(frame.subarray(0, frame.byteLength - 1)), (error) => error.code === 'truncated_frame');
  assert.throws(() => h.api.encodeRecord(header, new Uint8Array(h.api.LIMITS.chunkBytes + 1)), (error) => error.code === 'invalid_frame');
  const numeric = new TextEncoder().encode(JSON.stringify({ ...header, payload_size: 0 }));
  const invalid = new Uint8Array(4 + numeric.length); new DataView(invalid.buffer).setUint32(0, numeric.length); invalid.set(numeric, 4);
  assert.throws(() => h.api.decodeFrames(invalid), (error) => error.code === 'invalid_frame');
});

test('create identity and exact request survive an ACK loss and a worker reload', async () => {
  const h = workerHarness(); h.ctx.direction = 'imports'; h.ctx.fingerprint = emptyHash; h.ctx.conflictPolicy = 'skip';
  const bodies = [], secrets = []; let attempts = 0;
  h.fetch = async (url, init) => {
    if (url.endsWith('/capabilities')) return json(h.ctx.capabilities);
    bodies.push(init.body); secrets.push(init.headers['x-narou-transfer-secret']);
    const persisted = await h.ctx.store.get(JSON.parse(init.body).request_id);
    assert.equal(persisted.createRequest.creation_ticket, 'ticket'); assert.equal(persisted.secretHeader, secrets.at(-1));
    if (++attempts === 1) throw new TypeError('response lost after server commit');
    return json(status(h, { state: 'receiving_manifest' }));
  };
  await h.api.createImportSession(h.ctx, { profile: 'runtime', extra_roles: [], policy_version: 1 }, 12n, emptyHash);
  assert.equal(bodies[0], bodies[1]); assert.equal(secrets[0], secrets[1]);
  const saved = await h.ctx.store.get(h.ctx.requestId); saved.sessionId = '';
  h.api.restoreSessionRecord(h.ctx, saved); await h.api.recoverCreation(h.ctx);
  assert.equal(bodies[2], bodies[0]); assert.equal(secrets[2], secrets[0]);
});

test('control replay preserves operation_id, body and original If-Match across ACK loss', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret', revision: '9' });
  const attempts = [];
  h.fetch = async (_url, init) => {
    attempts.push({ body: init.body, revision: init.headers['if-match'] });
    assert.ok((await h.ctx.store.get('request')).pendingOperation);
    if (attempts.length === 1) throw new TypeError('lost publication ACK');
    return json(status(h, { revision: '10', state: 'publishing', next_action: 'advance', operation_id: 'advance' }));
  };
  await h.api.sessionOperation(h.ctx, '/imports/session/publish', { plan_revision: 'plan' });
  assert.deepEqual(attempts[0], attempts[1]); assert.equal(attempts[0].revision, '"9"');
  assert.equal((await h.ctx.store.get('request')).pendingOperation, null);
});

test('in-flight progress reconciles a Busy control retry instead of replaying a stale revision', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret', revision: '989' });
  // Server model: a committed batch may still advance the revision while a
  // rejected control holds nothing; the session lock is busy while one
  // invocation is in flight. Revision gate rejects before the lock check, and
  // an ambiguous Busy failure replays the identical receipt until a definitive
  // mismatch re-fetches the authoritative revision.
  let serverRevision = 991, inFlight = false, gets = 0;
  const sends = [], bodies = [];
  h.fetch = async (url, init) => {
    if (inFlight) { serverRevision += 1; inFlight = false; }
    if (url.endsWith('/pause')) {
      sends.push(init.headers['if-match']); bodies.push(init.body);
      if (BigInt(sends.at(-1).replaceAll('"', '')) !== BigInt(serverRevision)) {
        return json({ error: { code: 'revision_mismatch', message: `session revision is now ${serverRevision}`, retryable: false }, revision: String(serverRevision) }, 412);
      }
      if (sends.length === 2) {
        inFlight = true;
        return json({ error: { code: 'operation_busy', message: 'another invocation holds this transfer session', retryable: true }, revision: String(serverRevision) }, 423);
      }
      serverRevision += 1;
      return json(status(h, { revision: String(serverRevision), state: 'paused', next_action: 'none' }));
    }
    gets += 1;
    return json(status(h, { revision: String(serverRevision) }));
  };
  const result = await h.api.sessionOperation(h.ctx, '/imports/session/pause');
  // Busy replays the identical "991" receipt; only the next 412 reconciles.
  assert.deepEqual(sends, ['"989"', '"991"', '"991"', '"992"']);
  assert.equal(gets, 2);
  assert.ok(bodies.every((body) => body === bodies[0]));
  assert.equal(result.state, 'paused');
  assert.equal(h.ctx.pendingOperation, null);
  assert.equal((await h.ctx.store.get('request')).pendingOperation, null);
});

test('a Busy replay of a committed operation dedups its original receipt instead of rebasing', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret', revision: '9' });
  // First send commits under If-Match "9" but its ACK is lost; the retried send
  // finds the server still holding the same operation's session lock (423).
  // Replaying the identical "9" receipt must dedup to the committed result;
  // rebasing to the fresh revision would conflict with that fingerprint.
  let applied = 0, revision = 9, busyOnce = true;
  const sends = [], bodies = [];
  h.fetch = async (url, init) => {
    if (!url.endsWith('/pause')) return json(status(h, { revision: String(revision), state: 'paused', next_action: 'none' }));
    sends.push(init.headers['if-match']); bodies.push(init.body);
    const sent = sends.at(-1).replaceAll('"', '');
    if (sends.length === 1) {
      assert.equal(sent, '9'); applied += 1; revision = 10; throw new TypeError('lost pause ACK');
    }
    if (busyOnce) {
      busyOnce = false;
      return json({ error: { code: 'operation_busy', message: 'same operation still in flight', retryable: true }, revision: String(revision) }, 423);
    }
    if (sent === '9') return json(status(h, { revision: '10', state: 'paused', next_action: 'none' }));
    return json({ error: { code: 'operation_conflict', message: 'operation_id was already used with a different request', retryable: false }, revision: '10' }, 409);
  };
  const result = await h.api.sessionOperation(h.ctx, '/imports/session/pause');
  assert.equal(applied, 1);
  assert.deepEqual(sends, ['"9"', '"9"', '"9"']);
  assert.ok(bodies.every((body) => body === bodies[0]));
  assert.equal(result.state, 'paused');
  assert.equal(h.ctx.pendingOperation, null);
});

test('an exhausted control conflict retains the same operation receipt without claiming progress', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret', revision: '989' });
  const sends = [], bodies = []; let reconciles = 0;
  h.fetch = async (url, init) => {
    if (url.endsWith('/pause')) {
      sends.push(init.headers['if-match']); bodies.push(init.body);
      return json({ error: { code: 'revision_mismatch', message: 'session revision is now 991', retryable: false }, revision: '991' }, 412);
    }
    reconciles += 1;
    return json(status(h, { revision: '991' }));
  };
  await assert.rejects(h.api.sessionOperation(h.ctx, '/imports/session/pause'), (error) => error.httpStatus === 412);
  assert.equal(sends.length, 6); // MAX_CONTROL_ATTEMPTS sends, reconcile before each replay
  assert.equal(sends[0], '"989"'); assert.ok(sends.slice(1).every((tag) => tag === '"991"'));
  assert.ok(bodies.every((body) => body === bodies[0]));
  assert.equal(reconciles, 5);
  const operationId = JSON.parse(bodies[0]).operation_id;
  assert.equal(h.ctx.pendingOperation.operationId, operationId);
  const saved = await h.ctx.store.get('request');
  assert.equal(saved.pendingOperation.operationId, operationId);
  assert.equal(saved.pendingOperation.revision, '991');
  assert.notEqual(h.ctx.status?.state, 'paused');
});

test('an exhausted Busy control retains the unresolved receipt for later recovery', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret', revision: '9' });
  const sends = [], bodies = [];
  h.fetch = async (url, init) => {
    assert(url.endsWith('/pause'), 'an ambiguous Busy must not rebase its receipt');
    sends.push(init.headers['if-match']); bodies.push(init.body);
    return json({ error: { code: 'operation_busy', message: 'the original invocation is still in flight', retryable: true }, revision: '10' }, 423);
  };
  await assert.rejects(h.api.sessionOperation(h.ctx, '/imports/session/pause'), (error) => error.httpStatus === 423);
  assert.deepEqual(sends, Array(6).fill('"9"'));
  assert.ok(bodies.every((body) => body === bodies[0]));
  const operationId = JSON.parse(bodies[0]).operation_id;
  assert.equal(h.ctx.pendingOperation?.operationId, operationId);
  const saved = await h.ctx.store.get('request');
  assert.equal(saved.pendingOperation?.operationId, operationId);
  assert.equal(saved.pendingOperation.revision, '9');
  assert.notEqual(h.ctx.status?.state, 'paused');
});

test('stale status never rolls back revision or next-action identity', () => {
  const h = workerHarness();
  h.api.adoptStatus(h.ctx, status(h, { revision: '12', operation_id: 'new' }));
  h.api.adoptStatus(h.ctx, status(h, { revision: '8', operation_id: 'old' }));
  assert.equal(h.ctx.revision, '12'); assert.equal(h.ctx.operationId, 'new');
});

test('manifest resume uses server receipts and sends one framed record at each chunk path', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret' });
  h.ctx.limits.chunkBytes = 4;
  const spool = new h.api.ByteSpool(); await spool.append(new TextEncoder().encode('0123456789')); await spool.finalize();
  const chunks = [];
  h.fetch = async (url, init) => {
    const records = h.api.decodeFrames(init.body, h.ctx.limits); assert.equal(records.length, 1);
    const record = records[0]; chunks.push(record.header.chunk_index);
    assert.equal(record.header.file_id, h.api.MANIFEST_FILE_ID); assert.ok(url.endsWith(`/manifest/chunks/${record.header.chunk_index}`));
    return json({ receipts: [receipt(record, Math.min((record.header.chunk_index + 1) * 4, 10))], failure: null,
      status: status(h, { state: 'receiving_manifest', revision: String(record.header.chunk_index + 1) }) });
  };
  await h.api.uploadManifest(h.ctx, spool, 10n, { manifest_confirmed_bytes: '4', manifest_next_chunk_index: 1 });
  assert.deepEqual(chunks, [1, 2]); await spool.close();
});

test('resumed entry streams only the unconfirmed suffix after validating the server prefix', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request' });
  h.ctx.limits.chunkBytes = 8; const wholeHash = 'a'.repeat(64), prefix = 'b'.repeat(64), sent = [];
  h.ctx.wasm = { stream_entry: async (_handle, id, offset, prefixSha, write) => {
    assert.equal(id, 0); assert.equal(offset, '8'); assert.equal(prefixSha, prefix); await write(new Uint8Array([9, 10, 11, 12]));
    return JSON.stringify({ emitted: '4', file_size: '12', sha256: wholeHash });
  } };
  h.fetch = async (_url, init) => {
    const records = h.api.decodeFrames(init.body, h.ctx.limits); sent.push(...records);
    return json({ receipts: records.map((record) => ({ ...receipt(record, 12), confirmed_prefix_sha256: wholeHash })), failure: null, status: status(h, { revision: '2', confirmed_bytes: '12' }) });
  };
  await h.api.uploadFiles(h.ctx, 1, [{ items: [{ file_id: 0, manifest_file_index: 0, size: '12', sha256: wholeHash,
    confirmed_bytes: '8', confirmed_prefix_sha256: prefix, next_chunk_index: 1, state: 'receiving', upload_required: true }] }]);
  assert.equal(sent.length, 1); assert.equal(sent[0].header.chunk_index, 1); assert.equal(sent[0].header.final, true);
  assert.deepEqual([...sent[0].payload], [9, 10, 11, 12]);
});

test('empty import files send a SHA-verified final record without a WASM byte callback', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request' });
  h.ctx.wasm = { stream_entry: async () => JSON.stringify({ emitted: '0', file_size: '0', sha256: emptyHash }) };
  let sent; h.fetch = async (_url, init) => {
    [sent] = h.api.decodeFrames(init.body); return json({ receipts: [receipt(sent, 0)], failure: null, status: status(h) });
  };
  await h.api.uploadFiles(h.ctx, 1, [{ items: [{ file_id: 0, manifest_file_index: 0, size: '0', sha256: emptyHash,
    confirmed_bytes: '0', next_chunk_index: 0, state: 'pending', upload_required: true }] }]);
  assert.equal(sent.payload.length, 0); assert.equal(sent.header.final, true); assert.equal(sent.header.sha256, emptyHash);
});

test('partial batch rejection retains earlier durable receipts instead of claiming success', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request' });
  const builder = new h.api.BatchBuilder();
  for (let file = 0; file < 2; file += 1) builder.addRecord({ file_id: file, chunk_index: 0, final: true, sha256: emptyHash }, new Uint8Array());
  h.fetch = async (_url, init) => { const [first] = h.api.decodeFrames(init.body);
    return json({ receipts: [receipt(first, 0)], failure: { file_id: 1, chunk_index: 0, code: 'chunk_conflict', message: 'different bytes' }, status: status(h, { revision: '2' }) }); };
  await assert.rejects(h.api.sendBatch(h.ctx, '/imports/session/chunks', builder.build()), (error) => error.code === 'chunk_conflict');
  assert.equal((await h.ctx.store.get('request')).receipts.length, 1);
});

test('export retains each payload through all codec reads and does not infer EOF from record count', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'exports', sessionId: 'session' });
  h.ctx.limits.chunkBytes = 128 * 1024; const payload = new Uint8Array(128 * 1024).fill(7);
  h.fetch = async (url) => {
    const index = Number(url.split('/').at(-1)); assert.ok(url.includes('/batches/'));
    const frame = h.api.encodeRecord({ file_id: 0, chunk_index: index, final: index === 1 }, payload, h.ctx.limits);
    return binary(frame, { 'x-transfer-record-count': '1', 'x-transfer-last': String(index === 1) });
  };
  const reader = h.api.createExportReader(h.ctx, { read: async () => new Uint8Array() });
  for (let offset = 0; offset < 256 * 1024; offset += 64 * 1024) {
    assert.equal((await reader.readChunk(0, 'body', offset, 64 * 1024, 0)).byteLength, 64 * 1024);
    if (offset === 0) { await tick(); assert.equal(h.requests.length, 1, 'no read-ahead while consumer is stopped'); }
  }
  await reader.finish(); assert.equal(h.requests.length, 2, 'two batches, not one retry GET per 64 KiB');
});

test('export consumes empty file records even though build_zip has no callback for empty files', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'exports', sessionId: 'session' });
  const builder = new h.api.BatchBuilder();
  builder.addRecord({ file_id: 0, chunk_index: 0, final: true }, new Uint8Array());
  builder.addRecord({ file_id: 1, chunk_index: 0, final: true }, new Uint8Array([42]));
  h.fetch = async () => binary(builder.build(), { 'x-transfer-record-count': '2', 'x-transfer-last': 'true' });
  const reader = h.api.createExportReader(h.ctx, {}); assert.deepEqual([...await reader.readChunk(0, 'note', 0, 1, 1)], [42]); await reader.finish();
});

test('export rejects omitted batch bytes, even when a truncated body ends at a valid frame boundary', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'exports', sessionId: 'session' });
  const frame = h.api.encodeRecord({ file_id: 0, chunk_index: 0, final: true }, new Uint8Array([1]));
  h.fetch = async () => binary(frame, { 'x-transfer-record-count': '2', 'x-transfer-last': 'true' });
  const reader = h.api.createExportReader(h.ctx, {});
  await assert.rejects(reader.readChunk(0, 'note', 0, 1, 0), (error) => error.code === 'truncated_frame');
});

test('export waits for selection before snapshot and for sink ACK before fetching the next batch', async () => {
  const h = workerHarness(), selection = gate(), selectionEntered = gate(), sink = gate(), sinkEntered = gate();
  h.ctx.limits.chunkBytes = 64 * 1024;
  let state = 'selecting', revision = 1, writes = 0;
  const manifest = new TextEncoder().encode('{}'), payload = new Uint8Array(64 * 1024).fill(5);
  const row = { novel_ordinal: 0, logical_path: '本文/1.yaml', size: String(payload.length * 2), role: 'body',
    runtime: decisions('required'), complete: decisions('required') };
  const current = () => status(h, { direction: 'export', state, revision: String(revision), next_action: state === 'preparing' ? 'advance' : state === 'selecting' ? 'select' : 'read', operation_id: state === 'preparing' ? 'next' : null });
  h.fetch = async (url, init) => {
    if (url.endsWith('/capabilities')) return json(h.ctx.capabilities);
    if (url.endsWith('/exports') || url.endsWith('/exports/session')) return json(current());
    if (url.endsWith('/catalog')) return json({ items: [row], next_cursor: null });
    if (url.endsWith('/select')) {
      assert.equal(JSON.parse(init.body).profile, 'runtime'); state = 'preparing'; revision += 1; return json(current());
    }
    if (url.endsWith('/advance')) { state = 'ready'; revision += 1; return json(current()); }
    if (url.includes('/manifest/chunks/')) return binary(manifest, { etag: `"${hash(manifest)}"`, 'x-transfer-manifest-bytes': String(manifest.length), 'x-transfer-last': 'true' });
    if (url.includes('/batches/')) {
      const index = Number(url.split('/').at(-1));
      return binary(h.api.encodeRecord({ file_id: 0, chunk_index: index, final: index === 1 }, payload, h.ctx.limits),
        { 'x-transfer-record-count': '1', 'x-transfer-last': String(index === 1) });
    }
    if (url.endsWith('/complete')) { state = 'released'; revision += 1; return json(current()); }
    throw new Error(`Unexpected ${url}`);
  };
  h.ui = async (name) => {
    if (name === 'export-select') { selectionEntered.resolve(); await selection.promise; return { confirm: true, profile: 'runtime', roles: [] }; }
    if (name === 'sink-write' && ++writes === 1) { sinkEntered.resolve(); await sink.promise; }
    return null;
  };
  h.ctx.wasm = { build_zip: async (readManifest, size, read, write) => {
    assert.equal(size, '2'); assert.deepEqual([...await readManifest(0, 2)], [...manifest]);
    for (let offset = 0; offset < payload.length * 2; offset += payload.length) {
      const bytes = await read(0, '本文/1.yaml', offset, payload.length, 0);
      await write(bytes); assert.equal(bytes.byteLength, payload.length, 'codec input memory was not detached');
    }
  } };
  const done = h.api.runExport(h.ctx, { ids: [0] });
  await selectionEntered.promise; assert.equal(h.requests.some(({ url }) => /advance|manifest\/chunks|batches/.test(url)), false);
  selection.resolve(); await sinkEntered.promise;
  assert.equal(h.requests.filter(({ url }) => url.includes('/batches/')).length, 1);
  assert.equal(h.requests.some(({ url }) => url.endsWith('/complete')), false);
  sink.resolve(); const result = await done;
  assert.equal(result.state, 'released'); assert.equal(h.requests.filter(({ url }) => url.includes('/batches/')).length, 2);
});

for (const action of ['pause', 'cancel']) {
  test(`active export ${action} awaits a durable control receipt and preserves only resumable identity`, async () => {
    const h = workerHarness(), blocked = gate(), entered = gate();
    let state = 'ready', revision = 1; const manifest = new TextEncoder().encode('{}');
    const current = () => status(h, { direction: 'export', state, revision: String(revision),
      next_action: state === 'cancelling' ? 'advance' : state === 'paused' || state === 'cancelled' ? 'none' : 'read',
      operation_id: state === 'cancelling' ? 'cancel-step' : null });
    h.fetch = async (url, init) => {
      if (url.endsWith('/capabilities')) return json(h.ctx.capabilities);
      if (url.endsWith('/exports') || url.endsWith('/exports/session')) return json(current());
      if (url.includes('/manifest/chunks/')) return binary(manifest, { etag: `"${hash(manifest)}"`,
        'x-transfer-manifest-bytes': String(manifest.length), 'x-transfer-last': 'true' });
      if (url.includes('/batches/')) return binary(h.api.encodeRecord({ file_id: 0, chunk_index: 0, final: true }, new Uint8Array([1])),
        { 'x-transfer-record-count': '1', 'x-transfer-last': 'true' });
      if (url.includes('?operation_id=')) {
        assert.equal(init.method, 'DELETE'); assert.ok(init.headers['if-match']);
        state = 'cancelling'; revision += 1; return json(current());
      }
      if (url.endsWith('/pause')) { state = 'paused'; revision += 1; return json(current()); }
      if (url.endsWith('/advance')) { state = 'cancelled'; revision += 1; return json(current()); }
      throw new Error(`Unexpected ${url}`);
    };
    h.ctx.wasm = { build_zip: async (_manifest, _size, read, write) => { await write(await read(0, 'body', 0, 1, 0)); } };
    h.ui = async (name) => { if (name === 'sink-write') { entered.resolve(); await blocked.promise; } };
    const running = h.api.runCommand(h.ctx, { name: 'export', payload: { ids: [0] } });
    await entered.promise; await h.api.handleMessage(h.ctx, { type: 'command', name: 'abort', payload: { action } });
    await running; blocked.resolve();
    const result = h.events.findLast((event) => event.type === 'result');
    assert.equal(result.ok, false); assert.equal(result.error.stopState, action === 'cancel' ? 'cancelled' : 'paused');
    assert.equal((await h.ctx.store.all()).length, action === 'cancel' ? 0 : 1);
  });
}

test('authentication retry keeps the session and binary record identity unchanged', async () => {
  const h = workerHarness(); Object.assign(h.ctx, { direction: 'imports', sessionId: 'session', requestId: 'request', secretHeader: 'secret' });
  const builder = new h.api.BatchBuilder(); builder.addRecord({ file_id: 0, chunk_index: 0, final: true, sha256: emptyHash }, new Uint8Array());
  const bodies = []; let auth = 0;
  h.ui = async (name) => { assert.equal(name, 'auth'); auth += 1; return true; };
  h.fetch = async (_url, init) => {
    bodies.push(hash(init.body)); assert.equal(init.headers['x-narou-transfer-secret'], 'secret');
    if (bodies.length === 1) return json({ error: { code: 'authentication_required', message: 'login' } }, 401);
    const [record] = h.api.decodeFrames(init.body); return json({ receipts: [receipt(record, 0)], failure: null, status: status(h) });
  };
  await h.api.sendBatch(h.ctx, '/imports/session/chunks', builder.build());
  assert.equal(auth, 1); assert.deepEqual(bodies, [bodies[0], bodies[0]]); assert.equal(h.ctx.sessionId, 'session');
});

test('secret-lost sessions can be cancelled administratively with persisted operation replay', async () => {
  const h = workerHarness(); let deletes = 0, polls = 0; const operations = [];
  const remote = status(h, { session_id: 'remote', state: 'paused', direction: 'import', revision: '5', resume_mode: 'none' });
  h.fetch = async (url, init) => {
    if (init.method === 'DELETE') {
      deletes += 1; operations.push(new URL(url, 'https://example.test').searchParams.get('operation_id'));
      assert.equal(init.headers['x-narou-transfer-secret'], undefined); assert.equal(init.headers['if-match'], '"5"');
      assert.equal((await h.ctx.store.get('admin:remote')).operationId, operations.at(-1));
      if (deletes === 1) throw new TypeError('lost administrative cancellation ACK');
      return json({ ...remote, state: 'cancelling', revision: '6' });
    }
    polls += 1; return json({ items: polls <= 2 ? [{ ...remote, state: polls === 1 ? 'paused' : 'cancelling' }] : [], next_cursor: null });
  };
  const pending = await h.api.pendingSessions(h.ctx); assert.equal(pending.length, 1); assert.equal(pending[0].managed, true);
  assert.equal(pending[0].resumeMode, 'none');
  await h.api.discardSession(h.ctx, pending[0]);
  assert.equal(deletes, 2); assert.equal(operations[0], operations[1]); assert.equal((await h.ctx.store.all()).length, 0);
});

test('response limits are checked from actual bytes even without Content-Length', async () => {
  const h = workerHarness(); let cancelled = false;
  const response = new Response(new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array(17)); }, cancel() { cancelled = true; } }));
  await assert.rejects(h.api.readBoundedBody(response, 16), (error) => error.code === 'response_too_large');
  assert.equal(cancelled, true);
});

function relay(version = 3) {
  const handlers = new Map(), messages = []; let response, startup, closed = false;
  const port = { postMessage: (value) => messages.push(value), close() { closed = true; }, onmessage: null };
  const self = { location: new URL('https://example.test/assets/js/ui/transfer-download-sw.js'),
    addEventListener: (name, handler) => handlers.set(name, handler), skipWaiting: async () => {}, clients: { claim: async () => {} } };
  vm.runInNewContext(relaySource, { self, URL, Response, ReadableStream, ArrayBuffer, Uint8Array, Error, setTimeout, clearTimeout });
  const token = '12345678-1234-1234-1234-123456789abc';
  handlers.get('message')({ data: { type: 'transfer-download', token, version }, ports: [port], waitUntil(value) { startup = value; } });
  if (version !== 3) return { messages, get closed() { return closed; } };
  assert.equal(messages[0].type, 'ready');
  handlers.get('fetch')({ request: { url: `https://example.test/assets/js/ui/transfer-download/${token}` }, respondWith: (value) => { response = value; } });
  return { messages, reader: response.body.getReader(), send: (data) => port.onmessage({ data }), startup, get closed() { return closed; } };
}

test('Firefox relay rejects stale handshake versions before starting a download', () => {
  const download = relay(2); assert.equal(download.messages[0].type, 'error'); assert.equal(download.closed, true);
});
test('Firefox relay reads only on demand and forwards browser cancellation', async () => {
  const download = relay(); await tick(); assert.deepEqual(download.messages.map((value) => value.type), ['ready']);
  const first = download.reader.read(); await tick(); assert.equal(download.messages.at(-1).type, 'pull');
  download.send({ type: 'chunk', bytes: new Uint8Array(64 * 1024).fill(7).buffer });
  assert.equal((await first).value.length, 64 * 1024); const before = download.messages.length;
  await tick(); assert.equal(download.messages.length, before);
  await download.reader.cancel('user cancelled'); assert.equal(download.messages.at(-1).type, 'error'); assert.equal(download.closed, true);
});
test('Firefox relay rejects oversized output instead of successful EOF', async () => {
  const download = relay(), next = download.reader.read(); await tick();
  download.send({ type: 'chunk', bytes: new Uint8Array(64 * 1024 + 1).buffer });
  await assert.rejects(next, /Invalid download chunk/); assert.equal(download.closed, true);
});
test('Firefox relay closes only after explicit successful end', async () => {
  const download = relay(), next = download.reader.read(); await tick(); download.send({ type: 'end' });
  assert.equal((await next).done, true); assert.equal(download.messages.at(-1).type, 'closed'); assert.equal(download.closed, true);
});
test('Firefox relay fails an in-flight pull on upstream cancellation', async () => {
  const download = relay(), next = download.reader.read(); await tick(); download.send({ type: 'abort', message: 'upstream failed' });
  await assert.rejects(next, /upstream failed/); assert.equal(download.closed, true);
});

function browserSinkHarness() {
  let now = 0, handlers, reader, heldHeartbeat = false, pendingHeartbeat = null;
  const intervals = new Map(), ports = new Set(), listeners = new Set(), lifetimes = new Set(), started = gate();
  const worker = { state: 'activated', postMessage(data, transferred = []) {
    for (const port of transferred) ports.add(port);
    const deliver = () => handlers.get('message')({ data, ports: transferred, source: client,
      waitUntil(promise) { lifetimes.add(promise); promise.finally(() => lifetimes.delete(promise)); } });
    if (heldHeartbeat && data.type === 'transfer-download-ping') pendingHeartbeat = deliver;
    else queueMicrotask(deliver);
  } };
  const client = { postMessage(data) { queueMicrotask(() => { for (const listener of listeners) listener({ data, source: worker }); }); } };
  const load = () => {
    handlers = new Map();
    const self = { location: new URL('https://example.test/assets/js/ui/transfer-download-sw.js'),
      addEventListener: (name, handler) => handlers.set(name, handler), skipWaiting: async () => {}, clients: { claim: async () => {} } };
    vm.runInNewContext(relaySource, { self, URL, Response, ReadableStream, ArrayBuffer, Uint8Array, Error, Number, setTimeout, clearTimeout });
  };
  load();
  const serviceWorker = { async register() { return { active: worker, async update() {} }; },
    addEventListener(name, listener) { if (name === 'message') listeners.add(listener); },
    removeEventListener(name, listener) { if (name === 'message') listeners.delete(listener); } };
  const document = { createElement() { return { remove() {} }; }, body: { append(frame) {
    let response;
    handlers.get('fetch')({ request: { url: frame.src, method: 'GET' }, respondWith(value) { response = value; } });
    reader = response.body.getReader(); started.resolve();
  } } };
  const scope = vm.createContext({ window: {}, navigator: { serviceWorker }, URL, Uint8Array, ArrayBuffer,
    AbortController, DOMException, MessageChannel, crypto: { randomUUID }, setTimeout, clearTimeout,
    setInterval(callback) { const id = Symbol(); intervals.set(id, callback); return id; }, clearInterval(id) { intervals.delete(id); },
    Date: class extends Date { static now() { return now; } } });
  vm.runInContext(uiSource.replace(/^import .*;$/gm, '').replace(/^export /gm, '').replaceAll('import.meta.url', '"https://example.test/assets/js/ui/transfer.js"'), scope);
  scope.document = document;
  return { scope, started,
    read() { return reader.read(); },
    async advance(milliseconds) { now += milliseconds; for (const callback of [...intervals.values()]) callback(); await tick(); },
    async restart(cancelResponse = false) { if (cancelResponse && reader) await reader.cancel('worker terminated'); for (const port of ports) port.close(); ports.clear(); lifetimes.clear(); load(); await tick(); },
    holdNextHeartbeat() { heldHeartbeat = true; },
    releaseHeartbeat() { heldHeartbeat = false; const deliver = pendingHeartbeat; pendingHeartbeat = null; deliver?.(); },
    async dispose(controller) { controller.abort(); if (reader) await reader.cancel('fixture cleanup'); for (const port of ports) port.close(); intervals.clear(); lifetimes.clear(); },
  };
}

test('confirmation delays do not leave the eventual ZIP attached to an expired browser relay', async () => {
  const h = browserSinkHarness(), controller = new AbortController();
  const sink = await h.scope.downloadSink(controller);
  try {
    await h.advance(360_000); await h.restart(true);
    const writing = sink.write(Uint8Array.of(7, 19, 255)); writing.catch(() => {});
    await h.started.promise;
    const first = await h.read(); assert.deepEqual(Array.from(first.value || []), [7, 19, 255]); await writing;
    const closing = sink.close(); assert.equal((await h.read()).done, true); await closing;
  } finally { await h.dispose(controller); }
});

test('a silently lost browser relay rejects a waiting ZIP write instead of hanging forever', async () => {
  const h = browserSinkHarness(), controller = new AbortController();
  const sink = await h.scope.downloadSink(controller);
  try {
    let failure;
    const writing = sink.write(Uint8Array.of(23)); writing.catch(error => { failure = error; });
    await h.started.promise; await h.restart(); await h.advance(15_000); await tick();
    assert.match(failure?.message || 'write is still pending', /状態が失われました/);
  } finally { await h.dispose(controller); }
});

test('a heartbeat delivered after EOF cannot turn an acknowledged ZIP close into a failure', async () => {
  const h = browserSinkHarness(), controller = new AbortController();
  const sink = await h.scope.downloadSink(controller);
  try {
    const writing = sink.write(Uint8Array.of(31)); writing.catch(() => {});
    await h.started.promise; assert.deepEqual(Array.from((await h.read()).value), [31]); await writing;
    h.holdNextHeartbeat(); await h.advance(15_000);
    const closing = sink.close(); closing.catch(() => {});
    assert.equal((await h.read()).done, true);
    h.releaseHeartbeat(); await closing;
  } finally { await h.dispose(controller); }
});

test('FSA picker is invoked synchronously and its write is awaited before close', async () => {
  const blocked = gate(), entered = gate(); let picked = false, writes = 0, closed = false;
  const writable = { async write() { writes += 1; entered.resolve(); await blocked.promise; }, async close() { closed = true; }, async abort() {} };
  const scope = vm.createContext({ window: { showSaveFilePicker() { picked = true; return Promise.resolve({ createWritable: async () => writable }); } },
    navigator: {}, URL, Uint8Array, ArrayBuffer, AbortController, DOMException, setTimeout, clearTimeout });
  vm.runInContext(uiSource.replace(/^import .*;$/gm, '').replace(/^export /gm, '').replaceAll('import.meta.url', '"https://example.test/assets/js/ui/transfer.js"'), scope);
  const pending = scope.downloadSink(new AbortController()); assert.equal(picked, true); const sink = await pending;
  const write = sink.write(new Uint8Array(64 * 1024)); await entered.promise; assert.equal(writes, 1); assert.equal(closed, false);
  blocked.resolve(); await write; await sink.close(); assert.equal(closed, true);
});
