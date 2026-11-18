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
