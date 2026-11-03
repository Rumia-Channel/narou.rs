// Actual Worker/DO/D1 regression: interrupt a download before its terminal event.
// Build first. PROGRESS_WORKER_MAIN can select an older binary for before/after proof.
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const windows = process.platform === 'win32';
const npx = windows ? 'npx.cmd' : 'npx';
const parent = process.env.PROGRESS_FIXTURE_PARENT || join(root, '.wrangler');
mkdirSync(parent, { recursive: true });
const work = mkdtempSync(join(parent, 'progress-replay-'));
const config = join(work, 'wrangler.toml');
const persist = join(work, 'state');
const token = 'isolated-progress-replay-token';
const delay = (ms) => new Promise(resolve => setTimeout(resolve, ms));
let child, port, logs = '', enteredFirst = false, enteredSecond = false;
let releaseFirst, releaseSecond;
const firstHeld = new Promise(resolve => { releaseFirst = resolve; });
const secondHeld = new Promise(resolve => { releaseSecond = resolve; });
const handler = async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  if (path === '/replay/') {
    res.end('<h1>Replay Fixture</h1><span class="author">Author</span><a href="/replay/1/">First</a><a href="/replay/2/">Second</a>');
  } else if (path === '/replay/1/' || path === '/replay/2/') {
    if (path.endsWith('/1/')) { enteredFirst = true; await firstHeld; }
    else { enteredSecond = true; await secondHeld; }
    res.end('<div class="body"><p>Controlled saved episode.</p></div>');
  } else { res.writeHead(404); res.end(); }
};
const source = http.createServer(handler);
const sourceV6 = http.createServer(handler);
const sockets = new Set();
function run(args) {
  const result = spawnSync(npx, ['--yes', 'wrangler@4.151.3', ...args], { cwd: root, shell: windows, encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr + result.stdout);
  return result.stdout;
}
function sql(command) {
  const file = join(work, 'query.sql');
  writeFileSync(file, command);
  const out = run(['d1', 'execute', 'DB', '--local', '--config', config, '--persist-to', persist, '--file', file, '--json']);
  return JSON.parse(out.slice(out.indexOf('[\n')))[0].results;
}
async function until(predicate, label, timeout = 30000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await delay(100);
  }
  throw new Error(`Timed out: ${label}\n${logs}`);
}
async function start() {
  logs = '';
  child = spawn(npx, ['--yes', 'wrangler@4.151.3', 'dev', '--config', config, '--persist-to', persist, '--port', String(port)], {
    cwd: root, shell: windows, detached: !windows, stdio: ['ignore', 'pipe', 'pipe'],
  });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { logs = (logs + chunk).slice(-100000); });
  await until(async () => {
    if (child.exitCode !== null) throw new Error(logs);
    try { return (await fetch(`http://127.0.0.1:${port}/health/live`)).ok; } catch { return false; }
  }, 'Worker startup', 120000);
}
async function stop() {
  for (const socket of sockets) socket.close();
  sockets.clear();
  if (!child || child.exitCode !== null) return;
  const exited = once(child, 'exit');
  if (windows) spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
  else process.kill(-child.pid, 'SIGTERM');
  await exited;
  child = undefined;
}
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(`http://127.0.0.1:${port}${path}`, {
    method, headers: { Authorization: `Bearer ${token}`, 'Content-Type': typeof body === 'string' ? 'application/yaml' : 'application/json' },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
  });
  assert.equal(response.status, 200, await response.clone().text());
  return response.json();
}
async function connect() {
  const socket = new WebSocket(`ws://127.0.0.1:${port}/ws`);
  sockets.add(socket);
  const messages = [];
  socket.addEventListener('message', event => { messages.push(JSON.parse(event.data)); });
  await once(socket, 'open');
  return { socket, messages };
}
const progressFor = (messages, job) => messages.filter(message => message.type?.startsWith('progressbar.') && message.data?.scope === job);

try {
  source.listen(0, '127.0.0.1'); await once(source, 'listening');
  const sourcePort = source.address().port;
  sourceV6.listen(sourcePort, '::1'); await once(sourceV6, 'listening');
  const probe = http.createServer(); probe.listen(0, '127.0.0.1'); await once(probe, 'listening');
  port = probe.address().port; await new Promise(resolve => probe.close(resolve));
  const main = process.env.PROGRESS_WORKER_MAIN || join(root, 'build/worker/shim.mjs');
  let toml = readFileSync(join(root, 'wrangler.toml'), 'utf8').replace(/\[build\][^\[]*/, '')
    .replace(/^main = .*$/m, `main = ${JSON.stringify(main.replaceAll('\\', '/'))}`)
    .replace(/^directory = "public"$/m, `directory = ${JSON.stringify(join(root, 'public').replaceAll('\\', '/'))}`)
    .replace(/^migrations_dir = "migrations"$/m, `migrations_dir = ${JSON.stringify(join(root, 'migrations').replaceAll('\\', '/'))}`)
    .replace('NAROU_AUTH_REQUIRED = "true"', 'NAROU_AUTH_REQUIRED = "false"');
  writeFileSync(config, toml);
  writeFileSync(join(work, '.dev.vars'), `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32, 7).toString('base64')}\nS3_ACCESS_KEY_ID=local-test-key\nS3_SECRET_ACCESS_KEY=local-test-secret\n`);
  run(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','\"d1\"','d1'),('inv','asset_backend','\"d1\"','d1'),('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
  await start();
  const target = `http://localhost:${sourcePort}/replay/`;
  const definition = `name: Replay Fixture\ndomain: localhost\ntop_url: http://localhost:${sourcePort}\nurl: 'http://localhost:${sourcePort}/(?<ncode>replay)/?'\nsitename: Replay Fixture\nversion: 1.0\nconfirm_over18: no\ntoc_url: '${target}'\ntitle: '<h1>(?<title>.+?)</h1>'\nauthor: '<span class="author">(?<author>.+?)</span>'\nsubtitles: '<a href="(?<href>/replay/(?<index>\\d+)/)">(?<subtitle>.+?)</a>'\nbody_pattern: '<div class="body">(?<body>.+?)</div>'\nmin_interval: 0.01\n`;
  assert((await api('/api/sites/localhost.yaml', definition, 'PUT')).success);
  const live = await connect();
  const accepted = await api('/api/download', { targets: [target] });
  assert(accepted.success, JSON.stringify(accepted));
  const job = accepted.results[0].job_id;
  await until(() => enteredFirst && progressFor(live.messages, job).length > 0, 'live progress before body');
  assert((await api('/api/clear_history', {})).success);
  const afterClear = await connect();
  await until(() => progressFor(afterClear.messages, job).length > 0, 'active progress survives console history clear');
  afterClear.socket.close();
  console.log('PASS console history clear preserves active progress; it is not a progress-reset operation');
  releaseFirst();
  await until(() => enteredSecond && progressFor(live.messages, job).some(message => message.data.current >= 1), 'saved first episode and second body boundary');
  await until(async () => JSON.stringify(await api('/api/history?format=json')).includes('First'), 'saved console history');
  await stop();
  releaseSecond();
  // Keep the crash's queue envelope from delivering a terminal event on restart.
  toml = toml.replace(/\[\[queues\.consumers\]\][\s\S]*?(?=\n\[|$)/g, '');
  writeFileSync(config, toml);
  sql(`UPDATE worker_jobs SET status='permanent',admission_waiting=0,execution_token=NULL,lease_until=NULL WHERE job_id='${job}';`);
  const records = sql('SELECT * FROM worker_jobs ORDER BY job_id');
  const stored = sql('SELECT object_key,size,crc32 FROM objects ORDER BY object_key');
  await start();
  const queue = await api('/api/queue/status');
  assert.equal(queue.pending, 0); assert.equal(queue.running_count, 0);
  if (process.env.PROGRESS_BROWSER === '1') {
    console.log(`Progress browser fixture ready http://127.0.0.1:${port}`);
    await new Promise(resolve => process.once('SIGTERM', resolve));
  } else {
    for (let attempt = 0; attempt < 2; attempt++) {
      const replay = await connect();
      await delay(500);
      assert.deepEqual(progressFor(replay.messages, job), [], 'terminal jobs must not replay stale progress');
      assert(JSON.stringify(await api('/api/history?format=json')).includes('First'), 'repair must preserve console history');
      assert.deepEqual(sql('SELECT * FROM worker_jobs ORDER BY job_id'), records, 'replay must not rewrite jobs');
      assert.deepEqual(sql('SELECT object_key,size,crc32 FROM objects ORDER BY object_key'), stored, 'repair must preserve saved novel objects');
      await stop();
      if (attempt === 0) await start();
    }
    console.log('PASS lost terminal delivery is repaired on reconnect, survives restart and preserves jobs, saved objects and console history');
  }
} catch (error) {
  console.error(logs);
  throw error;
} finally {
  releaseFirst(); releaseSecond();
  await stop();
  source.closeAllConnections(); sourceV6.closeAllConnections();
  await Promise.all([new Promise(resolve => source.close(resolve)), new Promise(resolve => sourceV6.close(resolve))]);
  rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
