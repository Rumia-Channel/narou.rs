import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import test from 'node:test';
import { transferClient, waitForDeployedTransfer } from './transfer.mjs';

const advance = '/api/transfer/v2/exports/sess_transport/advance';
const init = {
  method: 'POST',
  headers: { 'content-type': 'application/json', 'x-narou-transfer-secret': 'fixture' },
  body: JSON.stringify({ operation_id: 'stable-operation' }),
};

async function serve(t, handler) {
  const server = createServer(handler).listen(0, '127.0.0.1');
  t.after(() => new Promise(resolve => {
    server.close(resolve);
    server.closeAllConnections();
  }));
  await once(server, 'listening');
  return transferClient(`http://127.0.0.1:${server.address().port}`, 'fixture-token', { verifyPrimary: true });
}

test('a lost advance response replays the same operation and reads its receipt', async t => {
  const received = [];
  const receipt = { state: 'preparing', operation_id: 'next-operation', revision: '8' };
  const client = await serve(t, async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    received.push({ path: req.url, method: req.method, body: Buffer.concat(chunks).toString() });
    if (received.length === 1) {
      req.socket.resetAndDestroy();
      return;
    }
    res.writeHead(200, { 'content-type': 'application/json', 'x-test-transfer-primary-only': 'true' });
    res.end(JSON.stringify(receipt));
  });
  const result = await client.json(advance, init);
  assert.deepEqual(result.body, receipt);
  assert.deepEqual(received, Array.from({ length: 2 }, () => ({ path: advance, method: 'POST', body: init.body })));
});

test('no reset replay for an unrecognized route or an unidentified operation', async t => {
  let requests = 0;
  const client = await serve(t, (req) => {
    requests++;
    req.socket.resetAndDestroy();
  });
  for (const [path, request] of [
    ['/api/transfer/v2/exports', init], [advance, { ...init, body: '{}' }],
  ]) {
    await assert.rejects(client.request(path, request), { name: 'TypeError' });
  }
  assert.equal(requests, 2);
});

test('repeated resets exhaust the bounded advance replay budget', async t => {
  let requests = 0;
  const client = await serve(t, (req) => {
    requests++;
    req.socket.resetAndDestroy();
  });
  await assert.rejects(client.request(advance, init), { name: 'TypeError' });
  assert.equal(requests, 4);
});

const seal = '/api/transfer/v2/imports/sess_transport/manifest/seal';
const sealRequest = {
  method: 'POST',
  headers: { ...init.headers, 'if-match': '"2"' },
  body: JSON.stringify({ operation_id: 'seal-operation', size: '7', sha256: 'a'.repeat(64) }),
};

test('a markerless Cloudflare 404 during seal replays the original operation and revision', async t => {
  const received = [];
  const receipt = { state: 'planning', revision: '3' };
  const client = await serve(t, async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    received.push([req.url, req.method, req.headers['x-narou-transfer-secret'],
      req.headers['if-match'], Buffer.concat(chunks).toString()]);
    if (received.length === 1) {
      res.writeHead(404, { 'cf-ray': 'fixture' }).end('Not Found');
      return;
    }
    res.writeHead(200, { 'cf-ray': 'fixture', 'x-test-transfer-primary-only': 'true' })
      .end(JSON.stringify(receipt));
  });
  assert.deepEqual((await client.json(seal, sealRequest)).body, receipt);
  assert.deepEqual(received, Array.from({ length: 2 }, () => [seal, 'POST', 'fixture', '"2"', sealRequest.body]));
});

test('a persistent markerless edge 404 still fails primary verification after three replays', async t => {
  let requests = 0;
  const client = await serve(t, (_req, res) => {
    requests++;
    res.writeHead(404, { 'cf-ray': 'fixture' }).end('Not Found');
  });
  await assert.rejects(client.json(seal, sealRequest), /HTTP 404, marker absent/);
  assert.equal(requests, 4);
});

test('a Worker 404 or a seal without an operation ID is not replayed', async t => {
  let requests = 0;
  const client = await serve(t, async (req, res) => {
    for await (const _chunk of req) { /* drain request body */ }
    const headers = { 'cf-ray': 'fixture' };
    if (++requests === 1) headers['x-test-transfer-primary-only'] = 'true';
    res.writeHead(404, headers).end('{}');
  });
  assert.equal((await client.json(seal, sealRequest)).response.status, 404);
  await assert.rejects(client.json(seal, { ...sealRequest, body: '{}' }), /HTTP 404, marker absent/);
  assert.equal(requests, 2);
});

test('startup waits for the Cloudflare route, then requires the real primary marker', async t => {
  let requests = 0;
  const client = await serve(t, (req, res) => {
    assert.equal(req.url, '/api/transfer/v2/capabilities');
    requests++;
    const headers = { 'cf-ray': 'fixture', 'content-type': 'application/json' };
    if (requests === 1) {
      res.writeHead(404, headers).end('{}');
    } else {
      res.writeHead(200, { ...headers, 'x-test-transfer-primary-only': 'true' })
        .end(JSON.stringify({ protocol_version: 2, storage: { import: { allowed: true } } }));
    }
  });
  await waitForDeployedTransfer(client);
  assert.equal(requests, 2);
});

test('startup fails immediately when the Worker responds without primary proof', async t => {
  let requests = 0;
  const client = await serve(t, (_req, res) => {
    requests++;
    res.writeHead(200, { 'cf-ray': 'fixture', 'x-test-transfer-primary-only': 'false' }).end('{}');
  });
  await assert.rejects(waitForDeployedTransfer(client), /must check primary access/);
  assert.equal(requests, 1);
});
