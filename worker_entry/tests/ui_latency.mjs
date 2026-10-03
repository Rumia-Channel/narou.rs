#!/usr/bin/env node
// Node 22+, no packages. Sequential GETs; no jobs, settings writes or deployment.
// Credentials are read from the environment and never included in the report.
import { writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

export const endpoints = [
  ['/api/list?length=50', value => Array.isArray(value?.data)],
  ['/api/tag_list?format=json', value => Array.isArray(value?.tags)],
  ['/api/queue/status', value => typeof value?.pending === 'number'],
  ['/api/get_queue_size', value => Array.isArray(value) && value.length === 2],
  ['/api/global_setting', value => Array.isArray(value?.settings)],
];

export function validateBase(raw) {
  let url;
  try { url = new URL(raw); } catch { throw new Error('--base must be a Worker origin URL'); }
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname);
  if (url.protocol !== 'https:' && !(local && url.protocol === 'http:')) {
    throw new Error('HTTPS is required except for loopback development');
  }
  if (url.username || url.password || url.search || url.hash || url.pathname !== '/') {
    throw new Error('--base must contain only the origin, without credentials, path or query');
  }
  return url.origin;
}

export function percentile(values, p) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.max(0, Math.ceil(sorted.length * p) - 1)];
}

export async function measure(base, path, valid, headers = {}, timeoutMs = 30000, fetchImpl = fetch) {
  const started = performance.now();
  const response = await fetchImpl(new URL(path, base), {
    method: 'GET', headers, redirect: 'manual', signal: AbortSignal.timeout(timeoutMs),
  });
  const headersMs = performance.now() - started;
  if (!response.ok || !response.headers.get('content-type')?.includes('application/json')) {
    await response.body?.cancel();
    throw new Error(`${path}: expected JSON success, got HTTP ${response.status}`);
  }
  const chunks = [];
  let bytes = 0;
  const reader = response.body?.getReader();
  if (!reader) throw new Error(`${path}: missing response body`);
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > 8 * 1024 * 1024) {
        await reader.cancel();
        throw new Error(`${path}: diagnostic body exceeds 8 MiB`);
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const totalMs = performance.now() - started;
  let value;
  try { value = JSON.parse(Buffer.concat(chunks).toString('utf8')); }
  catch { throw new Error(`${path}: invalid JSON response`); }
  if (!valid(value)) throw new Error(`${path}: unexpected API response shape`);
  return {
    headers_ms: Number(headersMs.toFixed(2)), total_ms: Number(totalMs.toFixed(2)),
    body_bytes: bytes, server_timing: response.headers.get('server-timing'),
  };
}

function options(args) {
  const out = { base: process.env.NAROU_BASE_URL, samples: 5, pause_ms: 0, timeout_ms: 30000 };
  const numeric = { '--samples': ['samples', 1, 100], '--pause-ms': ['pause_ms', 0, 60000], '--timeout-ms': ['timeout_ms', 1, 120000] };
  for (let i = 0; i < args.length; i++) {
    const key = args[i];
    if (key === '--help') return { help: true };
    const value = args[++i];
    if (value === undefined) throw new Error(`missing value for ${key}`);
    if (key === '--base') out.base = value;
    else if (key === '--output') out.output = value;
    else if (numeric[key]) {
      const [name, min, max] = numeric[key];
      const n = Number(value);
      if (!Number.isInteger(n) || n < min || n > max) throw new Error(`${key} must be an integer from ${min} to ${max}`);
      out[name] = n;
    } else throw new Error(`unknown option: ${key}`);
  }
  out.base = validateBase(out.base);
  return out;
}

export async function main(args) {
  const config = options(args);
  if (config.help) {
    console.log('node worker_entry/tests/ui_latency.mjs --base https://WORKER --samples 10 --output before.json\nCredentials: NAROU_ADMIN_TOKEN; optionally CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET.');
    return;
  }
  const headers = { accept: 'application/json' };
  if (process.env.NAROU_ADMIN_TOKEN) headers.authorization = `Bearer ${process.env.NAROU_ADMIN_TOKEN}`;
  const accessId = process.env.CF_ACCESS_CLIENT_ID;
  const accessSecret = process.env.CF_ACCESS_CLIENT_SECRET;
  if (Boolean(accessId) !== Boolean(accessSecret)) throw new Error('both Cloudflare Access credentials must be set');
  if (accessId) {
    headers['CF-Access-Client-Id'] = accessId;
    headers['CF-Access-Client-Secret'] = accessSecret;
  }
  const report = {
    captured_at: new Date().toISOString(),
    note: 'First and repeated client observations, not guaranteed cold/warm isolates. Server-Timing is post-auth wall time, not CPU or SQL execution time.',
    results: [],
  };
  for (const [path, valid] of endpoints) {
    const first = await measure(config.base, path, valid, headers, config.timeout_ms);
    const repeated = [];
    for (let i = 0; i < config.samples; i++) {
      if (config.pause_ms) await delay(config.pause_ms);
      repeated.push(await measure(config.base, path, valid, headers, config.timeout_ms));
    }
    report.results.push({
      path, first, repeated,
      repeated_headers_p50_ms: percentile(repeated.map(x => x.headers_ms), 0.5),
      repeated_headers_p95_ms: percentile(repeated.map(x => x.headers_ms), 0.95),
      repeated_total_p50_ms: percentile(repeated.map(x => x.total_ms), 0.5),
      repeated_total_p95_ms: percentile(repeated.map(x => x.total_ms), 0.95),
    });
  }
  const json = JSON.stringify(report, null, 2) + '\n';
  if (config.output) await writeFile(config.output, json, { mode: 0o600, flag: 'wx' });
  else process.stdout.write(json);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).catch(error => {
    // Do not print response bodies or fetch error causes (may contain URLs).
    console.error(error instanceof Error ? error.message : 'latency probe failed');
    process.exitCode = 1;
  });
}
