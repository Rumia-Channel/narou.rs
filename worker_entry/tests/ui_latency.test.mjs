import test from 'node:test';
import assert from 'node:assert/strict';
import { validateBase, percentile, measure } from './ui_latency.mjs';

const valid = value => Array.isArray(value?.data);
const response = (value, status = 200, headers = {}) => new Response(JSON.stringify(value), {
  status, headers: { 'content-type': 'application/json', ...headers },
});

test('validates origin and refuses credential-bearing URLs', () => {
  assert.equal(validateBase('https://example.com'), 'https://example.com');
  assert.equal(validateBase('http://127.0.0.1:8787'), 'http://127.0.0.1:8787');
  for (const url of ['http://example.com', 'https://u:p@example.com', 'https://example.com/api', 'https://example.com/?token=x', 'file:///tmp/a']) {
    assert.throws(() => validateBase(url));
  }
});

test('percentiles sort numerically without mutating observations', () => {
  const values = [90, 1, 20, 2];
  assert.equal(percentile(values, 0.5), 2);
  assert.equal(percentile(values, 0.95), 90);
  assert.equal(percentile([], 0.95), null);
  assert.deepEqual(values, [90, 1, 20, 2]);
});

test('probe uses GET, does not follow redirects, and returns metrics only', async () => {
  let received;
  const record = await measure('https://example.com', '/api/list', valid,
    { authorization: 'Bearer test-secret' }, 1000, async (url, init) => {
      received = init;
      return response({ data: [{ title: 'private-title' }] }, 200, { 'server-timing': 'list;dur=20.0' });
    });
  assert.equal(received.method, 'GET');
  assert.equal(received.redirect, 'manual');
  assert.equal(received.headers.authorization, 'Bearer test-secret');
  assert.ok(record.body_bytes > 0);
  assert.equal(record.server_timing, 'list;dur=20.0');
  assert.ok(!JSON.stringify(record).includes('private-title'));
  assert.ok(!JSON.stringify(record).includes('test-secret'));
});

test('probe rejects HTTP errors without echoing the response body', async () => {
  await assert.rejects(
    measure('https://example.com', '/api/list', valid, {}, 1000, async () => response({ secret: 'hidden' }, 503)),
    error => error.message.includes('HTTP 503') && !error.message.includes('hidden'),
  );
});

test('probe rejects redirects', async () => {
  await assert.rejects(measure('https://example.com', '/api/list', valid, {}, 1000,
    async () => new Response(null, { status: 302, headers: { location: '/login' } })), /HTTP 302/);
});

test('probe rejects login HTML and wrong JSON shapes', async () => {
  await assert.rejects(measure('https://example.com', '/api/list', valid, {}, 1000,
    async () => new Response('<html>login</html>', { headers: { 'content-type': 'text/html' } })), /expected JSON/);
  await assert.rejects(measure('https://example.com', '/api/list', valid, {}, 1000,
    async () => response({ success: false })), /response shape/);
});

test('probe rejects invalid JSON', async () => {
  await assert.rejects(measure('https://example.com', '/api/list', valid, {}, 1000,
    async () => new Response('private non-JSON', { headers: { 'content-type': 'application/json' } })), /invalid JSON/);
});

test('probe bounds response buffering', async () => {
  await assert.rejects(measure('https://example.com', '/api/list', valid, {}, 1000,
    async () => new Response(new Uint8Array(8 * 1024 * 1024 + 1), { headers: { 'content-type': 'application/json' } })), /8 MiB/);
});
