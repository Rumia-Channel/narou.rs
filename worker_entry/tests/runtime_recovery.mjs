// Real workerd + local D1/Queues/DOs: kill an owner awaiting an HTTP response,
// retain its state, then drive production scheduled recovery. Build first:
//   node tests/runtime_recovery.mjs
// No deployment, user credentials, production hooks, or replica transition SQL.
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdirSync, mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { renderAcquisitionTopology } from './acquisition_topology.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const windows = process.platform === 'win32';
const npx = windows ? 'npx.cmd' : 'npx';
const version = process.env.NAROU_WRANGLER_VERSION || '4.151.3';
assert.match(version, /^\d+\.\d+\.\d+$/, 'Wrangler version must be exact');
const wrangler = ['--yes', `wrangler@${version}`];
const token = 'isolated-runtime-recovery-token';
// The documented lost-execution policy, independent of queue.max-retries.
const MAX_RECOVERIES = 3;
// More source requests than the real 850-subrequest budget; no reduced budget.
const EPISODES = 900;
const EXPIRED = '2000-01-01T00:00:00.000000000Z';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const quote = value => `'${String(value).replaceAll("'", "''")}'`;
const environment = { ...process.env, CI: 'true', WRANGLER_SEND_METRICS: 'false' };
for (const key of Object.keys(environment)) {
  if (/^(CLOUDFLARE_|CF_API_|NAROU_|S3_|AWS_)/.test(key)) delete environment[key];
}
mkdirSync(join(root, '.wrangler'), { recursive: true });
const work = mkdtempSync(join(root, '.wrangler', 'runtime-recovery-'));
let config = join(work, 'wrangler.toml');
const persist = join(work, 'state');
let topology;
let child, port, inspectorPort, logs = '', processError, fixtureError;
let allowedJobs = null; // null allows all real deliveries; a Set selects job IDs.
const deliveryGates = new Set();
const deliveries = [];
const holds = [];
const requests = new Map();
const completedEpisodes = new Map();
let holdProgressFirst = true;
let holdProgressContinuation = true;

function reply(res, body) {
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(body);
}

function hold(path, res, release) {
  const entry = { path, res, release, closed: false };
  holds.push(entry);
  res.once('close', () => { entry.closed = true; });
  // Deliberately do not write headers, finish, time out, or throw: production
  // is actually awaiting its upstream, not receiving a synthetic failure.
}

function releaseDeliveries(ids = null) {
  allowedJobs = ids === null ? null : new Set(ids);
  for (const gate of deliveryGates) {
    if (allowedJobs === null || gate.ids.every(id => allowedJobs.has(id))) {
      deliveryGates.delete(gate);
      if (!gate.res.destroyed) gate.res.end('continue');
    }
  }
}

const handler = async (req, res) => {
  try {
    const path = new URL(req.url, 'http://localhost').pathname;
    if (req.method === 'POST' && path.startsWith('/__fixture/queue/')) {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const { ids } = JSON.parse(Buffer.concat(chunks).toString());
      assert(Array.isArray(ids) && ids.length > 0 && ids.every(id => typeof id === 'string'));
      if (path === '/__fixture/queue/after') {
        deliveries.push(...ids);
        res.end('observed');
      } else {
        assert.equal(path, '/__fixture/queue/before');
        if (allowedJobs === null || ids.every(id => allowedJobs.has(id))) res.end('continue');
        else {
          const gate = { ids, res };
          deliveryGates.add(gate);
          res.once('close', () => deliveryGates.delete(gate));
        }
      }
      return;
    }
    assert.equal(req.method, 'GET');
    requests.set(path, (requests.get(path) ?? 0) + 1);
    const toc = /^\/(runtime-(?:crash|successor|progress))\/$/.exec(path);
    if (toc) {
      const name = toc[1];
      const count = name === 'runtime-progress' ? EPISODES : 1;
      const links = Array.from({ length: count }, (_, index) =>
        `<a href="/${name}/${index + 1}/">第${index + 1}話</a>`).join('');
      const respond = () => reply(res, `<h1>${name}</h1><span class="author">Recovery Author</span>${links}`);
      if (name === 'runtime-progress' && completedEpisodes.size > 0 && holdProgressContinuation) {
        hold(path, res, respond);
      } else respond();
      return;
    }
    const episode = /^\/(runtime-(?:crash|successor|progress))\/(\d+)\/$/.exec(path);
    if (episode) {
      const [, name, number] = episode;
      const index = Number(number);
      assert(index >= 1 && index <= (name === 'runtime-progress' ? EPISODES : 1));
      const respond = () => {
        if (name === 'runtime-progress') {
          completedEpisodes.set(index, (completedEpisodes.get(index) ?? 0) + 1);
        }
        reply(res, `<div class="body"><p>${name} 第${index}話の保存された本文です。</p></div>`);
      };
      if (name === 'runtime-crash' || (name === 'runtime-progress' && index === 1 && holdProgressFirst)) {
        hold(path, res, respond);
      } else respond();
      return;
    }
    throw new Error(`Unexpected fixture request: ${req.method} ${path}`);
  } catch (error) {
    fixtureError = error;
    if (!res.headersSent) res.writeHead(500);
    res.end(String(error));
  }
};
const source = http.createServer(handler);
const sourceV6 = http.createServer(handler);

function localWrangler(args) {
  assert(!child, 'never open a second workerd against the live local SQLite file');
  const result = spawnSync(npx, [...wrangler, ...args], {
    cwd: root, shell: windows, encoding: 'utf8', env: environment,
    timeout: 180000, maxBuffer: 16 * 1024 * 1024,
  });
  assert.equal(result.status, 0, `${result.error ?? ''}\n${result.stderr}\n${result.stdout}`);
  return result.stdout;
}

function offlineSql(command) {
  const file = join(work, 'query.sql');
  writeFileSync(file, command);
  const output = localWrangler(['d1', 'execute', 'DB', '--local', '--config', config,
    '--persist-to', persist, '--file', file, '--json']);
  const start = output.indexOf('[\n');
  assert(start >= 0, output);
  return JSON.parse(output.slice(start))[0].results;
}

async function until(predicate, label, timeout = 60000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (fixtureError) throw fixtureError;
    if (processError) throw processError;
    if (child && (child.exitCode !== null || child.signalCode !== null)) {
      throw new Error(`Worker exited while waiting for ${label}\n${logs}`);
    }
    const result = await predicate();
    if (result) return result;
    await delay(100);
  }
  throw new Error(`Timed out: ${label}\n${logs}`);
}

async function start() {
  assert(!child);
  processError = undefined;
  logs += '\n--- starting preserved local state ---\n';
  child = spawn(npx, [...wrangler, 'dev', '--local', '--test-scheduled',
    ...topology.configs.flatMap((path) => ['--config', path]),
    '--persist-to', persist, '--ip', '127.0.0.1', '--port', String(port),
    '--inspector-port', String(inspectorPort)], {
    cwd: root, shell: windows, detached: !windows, env: environment, stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.once('error', error => { processError = error; });
  for (const stream of [child.stdout, child.stderr]) {
    stream.on('data', chunk => { logs = (logs + chunk).slice(-300000); });
  }
  await until(async () => {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/health/live`, {
        headers: { Connection: 'close' }, signal: AbortSignal.timeout(2000),
      });
      await response.text();
      return response.ok;
    } catch { return false; }
  }, 'Worker startup', 120000);
}

async function killWorker() {
  if (!child) return;
  const stopped = child;
  const exited = stopped.exitCode !== null || stopped.signalCode !== null
    ? Promise.resolve() : once(stopped, 'exit');
  if (windows) {
    const result = spawnSync('taskkill', ['/pid', String(stopped.pid), '/T', '/F'], { encoding: 'utf8' });
    assert(result.status === 0 || stopped.exitCode !== null || stopped.signalCode !== null,
      `taskkill failed: ${result.stderr}\n${result.stdout}`);
  } else {
    try { process.kill(-stopped.pid, 'SIGKILL'); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  await exited;
  child = undefined;
  // Only close source/barrier sockets AFTER forced process-tree termination.
  // Closing an upstream first would exercise ordinary retry, not abrupt loss.
  source.closeAllConnections();
  sourceV6.closeAllConnections();
}

async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(`http://127.0.0.1:${port}${path}`, {
    method, headers: { Authorization: `Bearer ${token}`, Connection: 'close',
      'Content-Type': typeof body === 'string' ? 'application/yaml' : 'application/json' },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
    signal: AbortSignal.timeout(60000),
  });
  const text = await response.text();
  assert.equal(response.status, 200, `${path}: ${text}`);
  return path.startsWith('/__scheduled') ? text : JSON.parse(text);
}
const snapshot = () => api('/__fixture/runtime-recovery');
async function job(id) {
  const row = (await snapshot()).jobs.find(row => row.job_id === id);
  assert(row, `Missing ledger row ${id}`);
  return row;
}
const tick = () => api('/__scheduled?cron=*+*+*+*+*');
async function enqueue(target) {
  const accepted = await api('/api/download', { targets: [target] });
  assert(accepted.success, JSON.stringify(accepted));
  assert.equal(accepted.results.length, 1, JSON.stringify(accepted));
  assert.equal(typeof accepted.results[0].job_id, 'string', JSON.stringify(accepted));
  return accepted.results[0].job_id;
}
async function held(path, after = 0) {
  return until(() => holds.slice(after).find(entry => entry.path === path && !entry.closed && !entry.res.destroyed),
    `upstream response held at ${path}`, 300000);
}
// The creation contract now persists a durable planned checkpoint before the
// first owned TOC/section request: numeric reservation plus original writer
// prefix/epoch, still without a novel row or a body cursor.
function plannedCreation(row) {
  assert(typeof row.checkpoint_json === 'string',
    `a claimed new download must carry its durable planned creation checkpoint: ${JSON.stringify(row)}`);
  const checkpoint = JSON.parse(row.checkpoint_json);
  assert.equal(checkpoint.job_id, row.job_id);
  assert.equal(checkpoint.novel_id, null, 'the reservation is not yet a publication');
  assert.equal(checkpoint.phase, 'planned');
  assert.equal(checkpoint.next_section_index, null, 'no accepted body may pose as progress');
  assert(checkpoint.write_origin && typeof checkpoint.write_origin.prefix === 'string'
    && Number.isInteger(checkpoint.write_origin.epoch),
    `the first writer epoch must be durable: ${row.checkpoint_json}`);
  assert(Array.isArray(checkpoint.creation_reservation)
    && checkpoint.creation_reservation.length === 2
    && checkpoint.creation_reservation.every(Number.isInteger),
    `the exact original ID/generation must be durable: ${row.checkpoint_json}`);
  return checkpoint;
}

// The checkpoint claim is only durable if the same ID/generation, namespace
// and gate epoch are still owned by this exact execution.
function assertClaimed(state, row, checkpoint) {
  const [id, generation] = checkpoint.creation_reservation;
  const owed = state.reservations.find(r => r.id === id && r.generation === generation);
  assert(owed, `reservation ${id}:${generation} must exist while owned`);
  assert.equal(owed.available, 0, 'an owned reservation must not reenter the pool');
  assert.equal(owed.job_id, row.job_id);
  const claim = state.claims.find(c => c.novel_id === id);
  assert(claim, 'the running owner must hold its namespace claim');
  assert.equal(claim.reservation_generation, generation);
  assert.equal(claim.owner, `create:${id}:${generation}`);
  assert.equal(claim.job_id, row.job_id);
  assert.equal(claim.epoch, checkpoint.write_origin.epoch);
  assert.equal(claim.prefix, checkpoint.write_origin.prefix.replace(/\/+$/, ''));
  const gate = state.gates.find(g => g.prefix === claim.prefix);
  assert(gate, 'the claimed namespace gate must exist');
  assert.equal(gate.closed, 0);
  assert.equal(gate.epoch, checkpoint.write_origin.epoch);
  assert.equal(gate.origin_url_fold, claim.toc_url_fold);
  assert(!state.novels.some(n => n.id === id), 'no novel row exists before publication');
}

// Completed cleanup re-opens the gate at the next epoch (fencing the dead
// writer), returns the exact ID to the pool and deletes the namespace claim.
function assertCreationReleased(state, checkpoint, label) {
  const [id, generation] = checkpoint.creation_reservation;
  const prefix = checkpoint.write_origin.prefix.replace(/\/+$/, '');
  const released = state.reservations.find(r => r.id === id && r.generation === generation);
  assert(released && released.available === 1 && released.job_id === null,
    `${label}: reservation ${id}:${generation} must return to the pool, got ${JSON.stringify(released)}`);
  assert(!state.claims.some(c => c.novel_id === id), `${label}: namespace claim must be retired`);
  const gate = state.gates.find(g => g.prefix === prefix);
  assert(gate, `${label}: namespace gate must still exist`);
  assert.equal(gate.closed, 0, `${label}: cleanup must re-open the gate`);
  assert.equal(gate.pending_readers, 0, `${label}: cleanup cannot reopen before dead readers expire`);
  assert.equal(gate.epoch, checkpoint.write_origin.epoch + 1,
    `${label}: close-and-reopen must fence the original epoch`);
  assert.equal(gate.origin_url_fold, '', `${label}: an empty-owned namespace must lose its origin`);
}

function assertRunning(row, recoveries, checkpoint = undefined) {
  assert.equal(row.status, 'running', JSON.stringify(row));
  assert.equal(row.attempts, 0, 'abrupt loss must not masquerade as a handled business failure');
  assert.equal(row.recovery_attempts, recoveries, JSON.stringify(row));
  assert.equal(row.admission_waiting, 0);
  assert(row.site_key && row.normalized_target, 'real YAML admission must own the site and target');
  assert(row.execution_token, 'the real consumer must have claimed the execution');
  assert(Date.parse(row.lease_until) > Date.now(), 'the running owner must have a live lease');
  if (checkpoint === undefined) return plannedCreation(row);
  assert.equal(row.checkpoint_json, checkpoint, 'reattachment must keep the exact original claim');
}

async function expireAndRestart(id, expected) {
  // The ONLY recovery fault injection is moving this local lease into the past.
  // Status, attempts, recovery_attempts, tokens and checkpoints stay untouched.
  // The lane reservation carries its own lease: a real crash loses both, so
  // both are expired (an early steal would not be a real crash).
  const [before] = offlineSql(`SELECT * FROM worker_jobs WHERE job_id=${quote(id)};`);
  assert.equal(before.status, expected.status);
  assert.equal(before.execution_token, expected.execution_token);
  assert.equal(before.recovery_attempts, expected.recovery_attempts);
  assert.equal(before.attempts, expected.attempts);
  assert.equal(before.checkpoint_json, expected.checkpoint_json);
  offlineSql(`UPDATE worker_jobs SET lease_until=${quote(EXPIRED)} WHERE job_id=${quote(id)};`);
  offlineSql(`UPDATE worker_acquisition_lanes SET lease_until=${quote(EXPIRED)} WHERE job_id=${quote(id)};`);
  const [after] = offlineSql(`SELECT * FROM worker_jobs WHERE job_id=${quote(id)};`);
  assert.deepEqual(after, { ...before, lease_until: EXPIRED }, 'expiration changes no recovery state');
  releaseDeliveries([]);
  await start();
  assert.deepEqual(await job(id), { ...expected, lease_until: EXPIRED }, 'crashed ownership survives restart');
}

async function recover(id, previous, recoveries) {
  await expireAndRestart(id, previous);
  await tick();
  const recovered = await job(id);
  assert.equal(recovered.status, 'pending', JSON.stringify(recovered));
  assert.equal(recovered.attempts, 0);
  assert.equal(recovered.recovery_attempts, recoveries, 'the real cron must charge the lost execution');
  assert.equal(recovered.execution_token, null, 'cron retires the dead owner before redelivery');
  assert.equal(recovered.checkpoint_json, previous.checkpoint_json,
    'the pending hand-off must carry the same durable checkpoint claim');
  releaseDeliveries();
}

async function drained(id, timeout = 300000) {
  const state = await until(async () => {
    const current = await snapshot();
    const acquisition = current.jobs.find(row => row.job_id === id);
    if (acquisition && ['permanent', 'retryable', 'blocked'].includes(acquisition.status)) {
      assert.fail(`Unexpected acquisition failure: ${JSON.stringify(acquisition)}`);
    }
    if (acquisition?.status !== 'succeeded') return false;
    const conversions = current.jobs.filter(row => row.kind === 'convert');
    for (const row of conversions) {
      assert(!['permanent', 'retryable', 'blocked'].includes(row.status), JSON.stringify(row));
    }
    return conversions.length > 0 && conversions.every(row => row.status === 'succeeded')
      && current.jobs.every(row => !['pending', 'running', 'retryable'].includes(row.status))
      && current.jobs.filter(row => row.status === 'succeeded').every(row => deliveries.includes(row.job_id))
      ? current : false;
  }, 'acquisition and follow-up conversion drain', timeout);
  const completed = state.jobs.find(row => row.job_id === id);
  assert.equal(completed.attempts, 0);
  return state;
}

async function unusedPort() {
  const probe = http.createServer();
  try {
    probe.listen(0, '127.0.0.1');
    await once(probe, 'listening');
    return probe.address().port;
  } finally { await new Promise(resolve => probe.close(resolve)); }
}

try {
  source.listen(0, '127.0.0.1'); await once(source, 'listening');
  const sourcePort = source.address().port;
  sourceV6.listen(sourcePort, '::1'); await once(sourceV6, 'listening');
  port = await unusedPort();
  inspectorPort = await unusedPort();
  // 取得は writer 1 本 + 取得専用 4 本で実行する (writer は実行しない)。
  topology = renderAcquisitionTopology({
    work,
    target: 'local',
    main: join(root, 'tests/runtime_recovery_worker.mjs').replaceAll('\\', '/'),
    s3Endpoint: `http://127.0.0.1:${sourcePort}`,
    vars: { RECOVERY_FIXTURE_ORIGIN: `http://127.0.0.1:${sourcePort}` },
  });
  config = topology.writerPath;
  writeFileSync(join(work, '.dev.vars'), `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32, 7).toString('base64')}\nS3_ACCESS_KEY_ID=local-test-key\nS3_SECRET_ACCESS_KEY=local-test-secret\n`);
  localWrangler(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
  offlineSql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','\"d1\"','d1'),('inv','asset_backend','\"d1\"','d1'),('local','download.interval','0','0'),('local','download.wait-steps','0','0');");
  await start();
  const definition = `name: Runtime Recovery Fixture
 domain: localhost
 top_url: http://localhost:${sourcePort}
 url: 'http://localhost:${sourcePort}/(?<ncode>runtime-(?:crash|successor|progress))/?'
 sitename: Runtime Recovery Fixture
 version: 1.0
 confirm_over18: no
 toc_url: 'http://localhost:${sourcePort}/\\k<ncode>/'
 title: '<h1>(?<title>.+?)</h1>'
 author: '<span class="author">(?<author>.+?)</span>'
 subtitles: '<a href="(?<href>/runtime-(?:crash|successor|progress)/(?<index>\\d+)/)">(?<subtitle>.+?)</a>'
 body_pattern: '<div class="body">(?<body>.+?)</div>'
 min_interval: 0.01
`.replace(/^ /gm, '');
  assert((await api('/api/sites/localhost.yaml', definition, 'PUT')).success);
  assert((await api('/api/global_setting', { settings: {
    'queue.max-retries': 0, 'update.auto-schedule.enable': false, 'update.disabled-author-tracking': true,
  } })).success);

  const target = name => `http://localhost:${sourcePort}/runtime-${name}/`;
  const crashId = await enqueue(target('crash'));
  await held('/runtime-crash/1/');
  let owner = await job(crashId);
  const crashCheckpoint = assertRunning(owner, 0);
  const originalClaim = owner.checkpoint_json;
  assertClaimed(await snapshot(), owner, crashCheckpoint);
  const site = owner.site_key;
  const tokens = new Set([owner.execution_token]);
  for (let attempt = 1; attempt <= MAX_RECOVERIES; attempt++) {
    const mark = holds.length;
    await killWorker();
    await recover(crashId, owner, attempt);
    await held('/runtime-crash/1/', mark);
    owner = await job(crashId);
    assertRunning(owner, attempt, originalClaim);
    assertClaimed(await snapshot(), owner, crashCheckpoint);
    assert.equal(owner.site_key, site);
    // A held body proves no durable progress: reattachment replays the TOC
    // under the same reservation and never repays the lost-execution budget.
    assert.equal(requests.get('/runtime-crash/'), attempt + 1,
      'a recovered execution replays discovery under the same identity');
    assert(!tokens.has(owner.execution_token), 'each recovery must receive a fresh fencing token');
    tokens.add(owner.execution_token);
  }

  // Restart BEFORE expiration. No invocation owns isolate memory now, so a
  // second queued work must be refused by durable SITE admission, not capacity.
  await killWorker();
  releaseDeliveries([]);
  await start();
  assert.deepEqual(await job(crashId), owner);
  const successorId = await enqueue(target('successor'));
  releaseDeliveries([successorId]);
  const waiting = await job(successorId);
  assert(!(await snapshot()).lanes.some(lane => lane.job_id === successorId),
    'same-site work must wait before lane delivery, not consume a lane');
  assert(!deliveries.includes(successorId), 'same-site waiting must not burn a physical delivery');
  assert.equal(waiting.status, 'pending', JSON.stringify(waiting));
  assert.equal(waiting.admission_waiting, 1, 'same-site work must durably wait, not retry');
  assert.equal(waiting.site_key, site);
  assert.equal(waiting.attempts, 0);
  assert.equal(waiting.recovery_attempts, 0);
  assert.equal(waiting.execution_token, null);
  assert.equal(waiting.lease_until, null);
  assert.equal(requests.get('/runtime-successor/'), undefined, 'blocked work must not reach its source');
  await tick();
  assert.deepEqual(await job(crashId), owner, 'cron must not recover a live lease');
  assert.deepEqual(await job(successorId), waiting, 'cron must respect live site ownership');

  await killWorker();
  await expireAndRestart(crashId, owner);
  await tick();
  const exhausted = await job(crashId);
  assert.equal(exhausted.status, 'permanent', JSON.stringify(exhausted));
  assert.equal(exhausted.attempts, 0);
  assert.equal(exhausted.recovery_attempts, MAX_RECOVERIES);
  assert.match(exhausted.last_error, /acquisition execution recovery exhausted without checkpoint progress/);
  assert.equal(exhausted.lease_until, null);
  assert.equal(exhausted.execution_token, null);
  assert.equal(exhausted.admission_waiting, 0);
  // Killed executions cannot release their download reader leases. Cron must
  // retain the original claim and closed gate until their real wall-clock TTL
  // expires; extra ticks cannot accelerate that expiry. Use the existing wait
  // deadline, without changing leases, recovery limits or production budgets.
  let observedReaderDrain = false;
  const tombstone = await until(async () => {
    const state = await snapshot();
    const current = state.jobs.find(row => row.job_id === crashId);
    assert(current, 'the original tombstone must remain durable');
    if (current.checkpoint_json === null) return current;
    assert.equal(current.status, 'permanent', JSON.stringify(current));
    assert.equal(current.checkpoint_json, originalClaim,
      'pending creation cleanup must keep the exact original claim');
    const stillOwed = state.reservations
      .find(r => r.id === crashCheckpoint.creation_reservation[0]
        && r.generation === crashCheckpoint.creation_reservation[1]);
    assert(stillOwed && stillOwed.available === 0 && stillOwed.job_id === crashId,
      `a retained cleanup checkpoint must still owe its reservation: ${JSON.stringify(stillOwed)}`);
    const gate = state.gates.find(row => row.prefix === crashCheckpoint.write_origin.prefix.replace(/\/+$/, ''));
    assert(gate && gate.closed === 1 && gate.owner,
      'pending cleanup must keep admission closed under its removal owner');
    assert.equal(gate.epoch, crashCheckpoint.write_origin.epoch + 1);
    observedReaderDrain ||= gate.pending_readers > 0;
    await tick();
    return false;
  }, 'creation cleanup after natural dead-reader expiration');
  assert.deepEqual(tombstone, { ...exhausted, checkpoint_json: null },
    'closed cleanup clears only the checkpoint proof');
  assertCreationReleased(await snapshot(), crashCheckpoint, 'exhausted creation');
  releaseDeliveries();
  const afterRelease = await drained(successorId);
  const successor = afterRelease.jobs.find(row => row.job_id === successorId);
  assert.equal(successor.site_key, site, 'the same canonical site must become available');
  assert.equal(successor.recovery_attempts, 0, 'admission waiting must not spend the crash budget');
  assert.equal(requests.get('/runtime-crash/1/'), MAX_RECOVERIES + 1, 'no execution beyond the recovery bound');
  assert.equal(requests.get('/runtime-successor/1/'), 1);
  assert.equal(afterRelease.jobs.filter(row => row.target === target('crash')).length, 1,
    'recovery must keep the same durable job, not create replacement identities');
  assert.deepEqual(await job(crashId), tombstone, 'later queue deliveries cannot revive the tombstone');
  console.log(`PASS abrupt attempts=0 crashes exhaust three recoveries; exact creation identity survives natural reader drain (${observedReaderDrain}) and real site admission releases queued work`);

  // Actual progress earns a fresh crash budget. Cross the unchanged production
  // subrequest bound and stop the next invocation at its first TOC fetch.
  let mark = holds.length;
  const progressId = await enqueue(target('progress'));
  await held('/runtime-progress/1/', mark);
  let progress = await job(progressId);
  const progressCreation = assertRunning(progress, 0);
  const progressClaim = progress.checkpoint_json;
  await killWorker();
  mark = holds.length;
  await recover(progressId, progress, 1);
  const first = await held('/runtime-progress/1/', mark);
  progress = await job(progressId);
  // The reattached execution still owns only the original planned claim: the
  // lost delivery is charged, never repaid by reattachment alone.
  assertRunning(progress, 1, progressClaim);
  assertClaimed(await snapshot(), progress, progressCreation);
  const recoveredToken = progress.execution_token;
  mark = holds.length;
  holdProgressFirst = false;
  first.release();
  const continuation = await held('/runtime-progress/', mark);
  await until(() => deliveries.includes(progressId), 'preceding bounded queue invocation return');
  progress = await job(progressId);
  assertRunning(progress, 0, progress.checkpoint_json);
  const checkpoint = JSON.parse(progress.checkpoint_json);
  assert.equal(checkpoint.job_id, progressId);
  assert.equal(checkpoint.phase, 'fetching');
  assert(checkpoint.next_section_index > 0 && checkpoint.next_section_index < EPISODES,
    'a real bounded continuation must persist an advancing section cursor');
  // Real progress repays the crash budget while carrying forward the same
  // original reservation and writer epoch — never a new creation identity.
  assert.deepEqual(checkpoint.creation_reservation, progressCreation.creation_reservation);
  assert.deepEqual(checkpoint.write_origin, progressCreation.write_origin);
  assert.notEqual(progress.execution_token, recoveredToken, 'continuation must run as a fresh queue claim');
  assert.equal(completedEpisodes.size, checkpoint.next_section_index);
  assert(!continuation.closed);

  // A subsequent loss charges one recovery again, and cannot erase/rewind the
  // saved cursor. Finish from it through real queue continuations and conversion.
  await killWorker();
  holdProgressContinuation = false;
  await recover(progressId, progress, 1);
  const finished = await drained(progressId, 600000);
  assert.equal(completedEpisodes.size, EPISODES);
  assert([...completedEpisodes.values()].every(count => count === 1), 'completed source sections must never be fetched again');
  assert.equal(finished.novels.find(row => row.ncode === 'runtime-progress')?.general_all_no, EPISODES);
  assert.equal(finished.jobs.find(row => row.job_id === progressId).recovery_attempts, 0);
  assert.equal(finished.jobs.filter(row => row.target === target('progress')).length, 1);
  console.log('PASS durable bounded continuation resets crash count and resumes all 900 episodes without refetch');
} catch (error) {
  console.error(logs);
  throw error;
} finally {
  try { await killWorker(); }
  finally {
    source.closeAllConnections();
    sourceV6.closeAllConnections();
    await Promise.all([source, sourceV6].map(server => new Promise(resolve => server.close(resolve))));
    rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  }
}
