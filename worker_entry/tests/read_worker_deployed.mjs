// Non-mutating deployment probe. The isolated two-Worker regression is read_worker.mjs.
import assert from 'node:assert/strict';

const base = new URL(process.env.BASE_URL ?? '');
assert(['https:', 'http:'].includes(base.protocol) && !base.username && !base.password);
const token = process.env.NAROU_ADMIN_TOKEN;
const authRequired = (process.env.NAROU_AUTH_REQUIRED ?? 'true').toLowerCase() !== 'false';
if (authRequired && !token) {
  console.error('Reader smoke needs NAROU_ADMIN_TOKEN when UI authentication is enabled');
  process.exit(2);
}
const headers = new Headers();
if (token) headers.set('Authorization', `Bearer ${token}`);
if (process.env.CF_ACCESS_CLIENT_ID && process.env.CF_ACCESS_CLIENT_SECRET) {
  headers.set('CF-Access-Client-Id', process.env.CF_ACCESS_CLIENT_ID);
  headers.set('CF-Access-Client-Secret', process.env.CF_ACCESS_CLIENT_SECRET);
}

async function get(path) {
  let response;
  try {
    response = await fetch(new URL(path, base), {
      headers, redirect: 'manual', signal: AbortSignal.timeout(300_000),
    });
  } catch {
    console.error('Reader deployment is unreachable; not verified');
    process.exit(4);
  }
  if (response.status === 403 || (response.status >= 300 && response.status < 400
      && (response.headers.get('location') ?? '').includes('cloudflareaccess.com'))) {
    await response.body?.cancel();
    console.error('Reader deployment is blocked by Access/WAF; not verified');
    process.exit(3);
  }
  if (response.status !== 200) {
    await response.body?.cancel();
    assert.fail(`Reader ${path} returned HTTP ${response.status}`);
  }
  return response;
}

const listing = await get('/api/read/v1/novels?limit=1');
assert.match(listing.headers.get('content-type') ?? '', /^application\/json\b/);
const library = await listing.json();
assert(Array.isArray(library.data));
const favorites = await get('/api/read/v1/favorite-tags');
const favoriteState = await favorites.json();
assert.equal(favoriteState.success, true);
assert(Array.isArray(favoriteState.tags));
console.log('Reader listing and favorite tags: verified');

if (!library.data.length) {
  console.log('Reader EPUB: skipped (empty library); not verified');
} else {
  const id = library.data[0].id;
  assert(Number.isSafeInteger(id));
  const response = await get(`/api/read/v1/novels/${id}/epub`);
  assert.match(response.headers.get('content-type') ?? '', /^application\/epub\+zip\b/);
  let prefix = Buffer.alloc(0);
  let tail = Buffer.alloc(0);
  for await (const chunk of response.body) {
    const bytes = Buffer.from(chunk);
    if (prefix.length < 96) prefix = Buffer.concat([prefix, bytes.subarray(0, 96 - prefix.length)]);
    tail = Buffer.concat([tail, bytes]).subarray(-65_557);
  }
  assert.equal(prefix.readUInt32LE(0), 0x04034b50, 'EPUB local header');
  assert.equal(prefix.readUInt16LE(8), 0, 'mimetype must not be compressed');
  const nameLength = prefix.readUInt16LE(26);
  const extraLength = prefix.readUInt16LE(28);
  assert.equal(prefix.subarray(30, 30 + nameLength).toString(), 'mimetype');
  const payload = 30 + nameLength + extraLength;
  assert.equal(prefix.subarray(payload, payload + 20).toString(), 'application/epub+zip');
  const end = tail.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  assert(end >= 0 && end + 22 <= tail.length, 'EPUB central directory must complete');
  assert.equal(end + 22 + tail.readUInt16LE(end + 20), tail.length, 'EPUB must reach its ZIP end record');
  console.log('Reader EPUB: verified complete ZIP stream');
}
