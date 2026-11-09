import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const relaySource = fs.readFileSync(new URL('../src/web/assets/js/ui/transfer-download-sw.js', import.meta.url), 'utf8');
function relay() {
  const handlers = new Map(), messages = [];
  let lifetime, response;
  const port = { postMessage: (value) => messages.push(value), close() {}, onmessage: null };
  const self = { location: new URL('https://example.test/assets/js/ui/transfer-download-sw.js'),
    addEventListener: (name, handler) => handlers.set(name, handler), skipWaiting: async () => {}, clients: { claim: async () => {} } };
  vm.runInNewContext(relaySource, { self, URL, Response, ReadableStream, ArrayBuffer, Uint8Array, setTimeout, clearTimeout, console });
  const token = '12345678-1234-1234-1234-123456789abc';
  handlers.get('message')({ data: { type: 'transfer-download', token }, ports: [port], waitUntil: (value) => { lifetime = value; } });
  assert.equal(messages[0].type, 'ready');
  handlers.get('fetch')({ request: { url: `https://example.test/assets/js/ui/transfer-download/${token}` }, respondWith: (value) => { response = value; } });
  return { messages, reader: response.body.getReader(), send: (data) => port.onmessage({ data }), lifetime };
}

test('Firefox download relay reads only on demand and forwards cancellation', async () => {
  const download = relay();
  await new Promise(setImmediate);
  assert.deepEqual(download.messages.map(value => value.type), ['ready']);
  const first = download.reader.read();
  await new Promise(setImmediate);
  assert.equal(download.messages.at(-1).type, 'pull');
  download.send({ type: 'chunk', bytes: new Uint8Array(64 * 1024).fill(7).buffer });
  const chunk = await first;
  assert.equal(chunk.value.length, 64 * 1024);
  assert.equal(chunk.value[0], 7);
  const before = download.messages.length;
  await new Promise(setImmediate);
  assert.equal(download.messages.length, before, 'no speculative read while browser is paused');
  await download.reader.cancel('user cancelled');
  assert.equal(download.messages.at(-1).type, 'error');
  await download.lifetime;
});

test('download relay errors rather than returning successful EOF on failure', async () => {
  const download = relay();
  const next = download.reader.read();
  await new Promise(setImmediate);
  download.send({ type: 'abort', message: 'upstream failed' });
  await assert.rejects(next, /upstream failed/);
  await download.lifetime;
});

test('download relay closes only after an explicit successful end', async () => {
  const download = relay();
  const next = download.reader.read();
  await new Promise(setImmediate);
  download.send({ type: 'end' });
  assert.equal((await next).done, true);
  assert.equal(download.messages.at(-1).type, 'closed');
  await download.lifetime;
});

const uiSource = fs.readFileSync(new URL('../src/web/assets/js/ui/transfer.js', import.meta.url), 'utf8');
const consumeSource = uiSource.slice(uiSource.indexOf('async function consumeEvents('), uiSource.indexOf('async function downloadSink('));
test('progress response consumption waits for the disk sink before reading again', async () => {
  const context = vm.createContext({ MAX_FRAME_CHARS: 256 * 1024, TextDecoder, Error, JSON,
    failureMessage: () => 'HTTP failure' });
  vm.runInContext(consumeSource, context);
  let pulls = 0, release, entered;
  const started = new Promise(resolve => { entered = resolve; });
  const gate = new Promise(resolve => { release = resolve; });
  const encoder = new TextEncoder();
  const response = new Response(new ReadableStream({ pull(controller) {
    pulls += 1;
    if (pulls === 1) controller.enqueue(encoder.encode('{"type":"data","data":"YQ=="}\n'));
    else { controller.enqueue(encoder.encode('{"type":"complete","bytes":1}\n')); controller.close(); }
  } }, { highWaterMark: 0 }), { headers: { 'content-type': 'application/x-ndjson' } });
  const done = context.consumeEvents(response, async frame => { if (frame.type === 'data') { entered(); await gate; } });
  await started;
  await new Promise(setImmediate);
  assert.equal(pulls, 1, 'response is not read while the sink is applying backpressure');
  release();
  assert.equal((await done).bytes, 1);
});
