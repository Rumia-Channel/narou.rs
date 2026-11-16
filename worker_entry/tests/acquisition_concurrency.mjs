// 取得レーン (acquisition lane) の回帰スイート。
//
//   node worker_entry/tests/acquisition_concurrency.mjs
//
// 実 workerd + 実 D1 + 実 Queues + 実 HTTP ソースで、取得専用の 4 Worker が
// 本番と同じ判断 (admission / 割当 / claim / recovery / settlement) をして
// いることを確かめる。fixture は設定・ストレージ・観測だけを持ち、本番の
// 結果 (job 完了・失敗・ack) は一切作らない。
//
// 前提: `worker-build --release` (worker_entry) でビルド済みであること。
//
// 隔離: 設定・資格情報・D1 は .wrangler/acquisition-* に作る。
// 開発用の .dev.vars と .wrangler/state には触らない。
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { join } from 'node:path';
import process from 'node:process';
import { createS3Fixture, requireBuild } from './read_worker_fixture.mjs';
import {
  LANE_COUNT,
  acquisitionQueueName,
  applyMigrations,
  createAcquisitionWorkspace,
  createObservationSink,
  familySiteDefinition,
  offlineSql,
  observerMain,
  renderAcquisitionTopology,
  root,
  seedSql,
  startAcquisitionTopology,
  startFamilySource,
  unusedPort,
  writeTopologyDevVars,
} from './acquisition_topology.mjs';

const windows = process.platform === 'win32';
const token = 'isolated-acquisition-lane-token';
const loginKey = Buffer.alloc(32, 7).toString('base64');
const bucket = 'narou-acq-smoke';
const prefix = 'narou/acq';
const target = `proof-${randomUUID().replaceAll('-', '').slice(0, 12)}`;
const EXPIRED = '2000-01-01T00:00:00.000000000Z';
const TAG = 'レーン検証';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

// 5 系列 (4 レーン + 1 つ超過分)。系列ごとに別ポートにして rate-limit bucket
// も別にし、並列 downloads と「同じ系列は直列」を同時に確かめられるようにする。
const FAMILIES = ['family-a', 'family-b', 'family-c', 'family-d', 'family-e'];
// 同じ系列に 2 作品置けるのは「同じ系列の 2 つめは待つ」を確かめるため。
const NOVELS = {
  'family-a': ['book1', 'book2'],
  'family-b': ['book1'],
  'family-c': ['book1'],
  'family-d': ['book1'],
  'family-e': ['book1'],
};

requireBuild();

const workspace = createAcquisitionWorkspace();
const observation = createObservationSink();
const sources = new Map();
let s3 = null;
let dev = null;
let port = 0;
let base = '';
let topology = null;
let crashEnvelope = null;

const steps = [];
async function check(name, fn) {
  try {
    await fn();
    steps.push({ name, ok: true });
    console.log(`PASS ${name}`);
  } catch (error) {
    steps.push({ name, ok: false, error });
    console.error(`FAIL ${name}\n${error?.stack ?? error}`);
    throw error;
  }
}

async function api(path, { method = 'GET', body, contentType = 'application/json' } = {}) {
  const response = await fetch(`${base}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      Connection: 'close',
      ...(body === undefined ? {} : { 'Content-Type': contentType }),
    },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
    signal: AbortSignal.timeout(60000),
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    json = null;
  }
  return { status: response.status, text, json };
}

async function until(predicate, label, timeout = 120000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const result = await predicate();
    if (result) return result;
    if (Date.now() > deadline) {
      throw new Error(`Timed out: ${label}\n${dev?.logs ?? ''}`);
    }
    await delay(200);
  }
}

/** Invoke the real writer scheduled handler, including production recovery. */
async function tick() {
  let response = await api('/__scheduled?cron=*+*+*+*+*');
  if (response.status === 404) {
    response = await api('/cdn-cgi/handler/scheduled?cron=*+*+*+*+*');
  }
  assert.equal(response.status, 200, `scheduled trigger: ${response.text}`);
}

async function observe() {
  const response = await api('/__fixture/acquisition');
  assert.equal(response.status, 200, response.text);
  assert.ok(response.json && Array.isArray(response.json.lanes), response.text);
  return response.json;
}
const jobRow = async id => (await observe()).jobs.find(row => row.job_id === id);
const laneRows = async () => (await observe()).lanes;

const targetUrl = (family, novel = NOVELS[family][0]) =>
  `http://localhost:${sources.get(family).port}/${family}/${family}-${novel}/`;

async function enqueueDownload(family, novel) {
  const response = await api('/api/download', {
    method: 'POST',
    body: { targets: [targetUrl(family, novel)] },
  });
  assert.equal(response.status, 200, response.text);
  assert.equal(response.json?.success, true, response.text);
  assert.equal(response.json.results.length, 1, response.text);
  return response.json.results[0].job_id;
}

/** version-3 delivery の観測 (どの script が実行したか)。 */
function laneDeliveries(jobId, phase = 'before') {
  return observation.deliveries.flatMap(delivery =>
    (delivery.messages ?? [])
      .filter(message => message.body?.job_id === jobId && message.body?.version === 3)
      .filter(() => (delivery.phase ?? 'before') === phase)
      .map(message => ({
        role: delivery.role,
        lane: delivery.lane,
        queue: delivery.queue,
        body: message.body,
      })));
}

function legacyDeliveries(jobId) {
  return observation.deliveries.flatMap(delivery =>
    (delivery.messages ?? [])
      .filter(message => message.body?.job_id === jobId && message.body?.version === 2)
      .map(message => ({
        role: delivery.role,
        lane: delivery.lane,
        queue: delivery.queue,
        body: message.body,
      })));
}

/** version-1 入口の delivery (job_id は無く計画だけを持つ)。 */
function legacyPlanDeliveries(target) {
  return observation.deliveries.flatMap(delivery =>
    (delivery.messages ?? [])
      .filter(message => message.body?.version === 1
        && message.body?.job?.targets?.some(value => String(value).includes(target)))
      .map(message => ({
        role: delivery.role,
        lane: delivery.lane,
        queue: delivery.queue,
        body: message.body,
      })));
}

async function waitForSuccess(jobId, label, timeout = 180000) {
  return until(async () => {
    const row = await jobRow(jobId);
    if (!row) return false;
    if (['permanent', 'retryable', 'blocked'].includes(row.status)) {
      throw new Error(`unexpected acquisition failure: ${JSON.stringify(row)}`);
    }
    return row.status === 'succeeded' ? row : false;
  }, label, timeout);
}

/** Wait for physical lane release and the real follow-up conversion queue. */
async function assertDrained(extra = []) {
  await until(async () => {
    const state = await observe();
    for (const row of state.jobs) {
      assert.ok(!['permanent', 'blocked', 'retryable'].includes(row.status), JSON.stringify(row));
    }
    return state.jobs.every(row => ['succeeded', 'partial'].includes(row.status))
      && state.lanes.every(lane => lane.job_id === null);
  }, 'all lanes and follow-up conversions drain', 300000);
  for (const id of extra) {
    const row = await jobRow(id);
    assert.equal(row.status, 'succeeded', JSON.stringify(row));
  }
}

async function killTopology() {
  const child = dev?.child;
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  if (windows) {
    spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
  } else {
    try {
      process.kill(-child.pid, 'SIGKILL');
    } catch (error) {
      if (error.code !== 'ESRCH') throw error;
    }
  }
  await exited;
}

async function startTopology() {
  dev = await startAcquisitionTopology({
    configs: topology.configs,
    port,
    persist: workspace.persist,
    extraArgs: ['--test-scheduled'],
  });
  base = dev.base;
  const ready = await api('/health/ready');
  assert.equal(ready.status, 200, ready.text);
  return dev;
}

try {
  // --- 隔離した環境 ------------------------------------------------------
  const sink = await observation.listen();
  s3 = await createS3Fixture({ bucket, prefix });
  for (const family of FAMILIES) {
    sources.set(family, await startFamilySource({ family }));
  }
  port = await unusedPort();

  topology = renderAcquisitionTopology({
    work: workspace.work,
    target,
    main: observerMain(),
    assetsDirectory: join(root, 'public').replaceAll('\\', '/'),
    migrationsDirectory: join(root, 'migrations').replaceAll('\\', '/'),
    s3Endpoint: s3.endpoint,
    bucket,
    prefix,
    controlOrigin: sink.origin,
  });
  writeTopologyDevVars(workspace.devVars, {
    NAROU_ADMIN_TOKEN: token,
    NAROU_RS_LOGIN_KEY: loginKey,
    S3_ACCESS_KEY_ID: 'local-test-key',
    S3_SECRET_ACCESS_KEY: 'local-test-secret',
  });
  applyMigrations({ config: topology.writerPath, persist: workspace.persist });
  seedSql({
    config: topology.writerPath,
    persist: workspace.persist,
    sql: "INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES "
      + "('local','download.interval','0','0'),('local','download.wait-steps','0','0');",
  });

  await startTopology();

  // サイト定義は本番と同じ入口 (PUT /api/sites/<name>.yaml) で保存する。
  // 保存後は全 isolate が定義を読み直す必要があるので構成を作り直す。
  for (const family of FAMILIES) {
    for (const novel of NOVELS[family]) {
      const definition = familySiteDefinition({
        family,
        novel,
        port: sources.get(family).port,
      });
      const response = await api(`/api/sites/${family}-${novel}.yaml`, {
        method: 'PUT',
        body: definition,
        contentType: 'application/yaml',
      });
      assert.equal(response.status, 200, response.text);
      assert.equal(response.json?.success, true, response.text);
    }
  }
  await dev.stop();
  await startTopology();
  assert.equal(topology.laneNames.length, LANE_COUNT);

  // --- 1. 別系列は同時に held upstream まで到達する ----------------------
  await check('distinct site families reach the held upstream concurrently', async () => {
    const families = FAMILIES.slice(0, LANE_COUNT);
    for (const family of families) sources.get(family).hold();
    const jobs = {};
    for (const family of families) jobs[family] = await enqueueDownload(family);

    // 4 系列が同時に保留中 (応答していない) になる。
    await until(
      () => families.every(family => sources.get(family).pending() > 0),
      'four families reach the held upstream',
    );
    for (const family of families) {
      assert.ok(sources.get(family).pending() > 0, `${family} must still be held upstream`);
    }

    // 4 本の別 script が実行した (同じ script の 4 並列ではない)。
    const rows = await until(async () => {
      const lanes = (await laneRows()).filter(row => row.job_id);
      return lanes.length === LANE_COUNT ? lanes : false;
    }, 'four lane reservations');
    assert.equal(new Set(rows.map(row => row.lane)).size, LANE_COUNT, JSON.stringify(rows));
    const observedLanes = new Set(
      Object.values(jobs).flatMap(id => laneDeliveries(id).map(delivery => delivery.lane)),
    );
    assert.equal(observedLanes.size, LANE_COUNT, `expected four distinct lanes, got ${[...observedLanes]}`);
    for (const id of Object.values(jobs)) {
      const deliveries = laneDeliveries(id);
      assert.ok(deliveries.length >= 1, `no version-3 delivery for ${id}`);
      assert.equal(deliveries[0].role, 'acquisition', JSON.stringify(deliveries[0]));
      assert.equal(
        deliveries[0].queue,
        acquisitionQueueName(target, deliveries[0].lane),
        JSON.stringify(deliveries[0]),
      );
    }

    // 保留を解くと 4 件とも成功する。
    for (const family of families) sources.get(family).release();
    for (const id of Object.values(jobs)) await waitForSuccess(id, `job ${id} succeeds`);
    await assertDrained(Object.values(jobs));
  });

  // --- 2. 同じ系列の 2 つめは最初の解除前には始められない -----------------
  await check('a second same-family job cannot start before the first releases', async () => {
    sources.get('family-a').hold();
    const first = await enqueueDownload('family-a', 'book1');
    await until(() => sources.get('family-a').pending() > 0, 'first same-family job reaches the upstream');

    // 同じ系列の 2 つめ。
    const second = await enqueueDownload('family-a', 'book2');
    // 対照: 別系列は同時に走る (dispatcher と空きレーンが生きている証明)。
    sources.get('family-b').hold();
    const control = await enqueueDownload('family-b');
    await until(() => sources.get('family-b').pending() > 0, 'control family reaches the upstream');

    const row = await jobRow(second);
    assert.notEqual(row.status, 'running', JSON.stringify(row));
    assert.equal(row.site_key, 'family-a', JSON.stringify(row));
    const reserved = (await laneRows()).filter(lane => lane.job_id === second);
    assert.equal(reserved.length, 0, `the second same-family job must not own a lane: ${JSON.stringify(reserved)}`);
    assert.equal(
      sources.get('family-a').paths().filter(path => path.includes('/family-a-book2/')).length,
      0,
      'the second same-family job must not reach the upstream',
    );

    sources.get('family-a').release();
    await until(
      () => sources.get('family-a').paths().some(path => path.includes('/family-a-book2/')),
      'the second same-family job starts after the release',
    );
    sources.get('family-b').release();
    await waitForSuccess(first, 'first same-family job succeeds');
    await waitForSuccess(second, 'second same-family job succeeds');
    await waitForSuccess(control, 'control job succeeds');
    await assertDrained([first, second, control]);
  });

  // --- 3. 4 レーンが 4 系列を受け、5 つめは待ってから進む -----------------
  await check('four free lanes admit four families and the fifth waits then advances', async () => {
    const families = FAMILIES.slice(0, LANE_COUNT + 1);
    for (const family of families) sources.get(family).hold();
    const jobs = {};
    for (const family of families) jobs[family] = await enqueueDownload(family);

    await until(
      () => families.slice(0, LANE_COUNT).every(family => sources.get(family).pending() > 0),
      'four families occupy the lanes',
    );
    // 5 つめは upstream に到達していない (固定 hash ではなく空き枠待ち)。
    assert.equal(sources.get('family-e').count(), 0, 'the fifth family must wait for a free lane');
    const waiting = await jobRow(jobs['family-e']);
    assert.notEqual(waiting.status, 'running', JSON.stringify(waiting));
    assert.equal(
      (await laneRows()).filter(lane => lane.job_id === jobs['family-e']).length,
      0,
      'the fifth family must not own a lane yet',
    );

    // 1 つ解放すると、待っていた 5 つめが進む。
    sources.get('family-a').release();
    await until(() => sources.get('family-e').pending() > 0, 'the waiting family advances after a release');
    for (const family of families.slice(1)) sources.get(family).release();
    for (const id of Object.values(jobs)) await waitForSuccess(id, `job ${id} succeeds`);
    await assertDrained(Object.values(jobs));
  });

  // --- 4. 保留された EPUB 生成は取得枠を消費しない ------------------------
  await check('a held EPUB generation does not consume acquisition slots', async () => {
    // 1 作品目を取得済み (S1 の family-a/book1)。EPUB 生成の材料になる。
    const novels = (await api('/api/novels?limit=100')).json;
    const record = novels.data.find(entry => entry.toc_url === targetUrl('family-a'));
    assert.ok(record, 'the downloaded novel must be in the library');

    // オブジェクトストアの読み出しを 1 つ保留する → EPUB 生成が writer の中で
    // 止まったまま (writer の heavy permit を保持したまま) になる。
    s3.holdNextSourceRead();
    const epub = fetch(`${base}/api/novels/${record.id}/download.epub`, {
      headers: { Authorization: `Bearer ${token}`, Connection: 'close' },
    });
    let epubDone = false;
    epub.then(() => { epubDone = true; }, () => { epubDone = true; });
    await until(() => s3.heldCount() > 0, 'the EPUB generation is held on the object store');

    // EPUB が保留されている間にも取得レーンは 4 系列を同時に始める。
    const families = FAMILIES.slice(1, LANE_COUNT + 1);
    for (const family of families) sources.get(family).hold();
    const jobs = {};
    for (const family of families) jobs[family] = await enqueueDownload(family);
    await until(
      () => families.every(family => sources.get(family).pending() > 0),
      'four lanes start while the EPUB generation is held',
    );
    assert.equal(epubDone, false, 'the EPUB generation must still be held');
    const occupied = (await laneRows()).filter(lane => lane.job_id);
    assert.equal(occupied.length, LANE_COUNT, JSON.stringify(occupied));

    s3.releaseHolds();
    const response = await epub;
    assert.equal(response.status, 200, await response.clone().text());
    assert.equal(epubDone, true);

    for (const family of families) sources.get(family).release();
    for (const id of Object.values(jobs)) await waitForSuccess(id, `job ${id} succeeds`);
    await assertDrained(Object.values(jobs));
    const familyBIds = (await observe()).jobs.filter(row => row.site_key === 'family-b').map(row => row.job_id);
    const familyBLanes = new Set(familyBIds.flatMap(id => laneDeliveries(id).map(delivery => delivery.lane)));
    assert.ok(familyBLanes.size > 1, 'one family must use different free lanes, not a fixed hash');
  });

  await check('a running queued conversion keeps the writer memory permit', async () => {
    const record = (await api('/api/novels?limit=100')).json.data
      .find(entry => entry.toc_url === targetUrl('family-a'));
    assert.ok(record);
    let job;
    s3.holdNextSourceRead();
    try {
      const queued = await api('/api/jobs', {
        method: 'POST',
        body: { kind: 'Convert', targets: [String(record.id)], options: [] },
      });
      assert.equal(queued.status, 200, queued.text);
      job = queued.json.ids[0];
      await until(() => s3.heldCount() > 0, 'the queued conversion is held on the object store');
      assert.equal((await jobRow(job)).status, 'running');
      const response = await fetch(`${base}/api/novels/${record.id}/download.epub`, {
        headers: { Authorization: `Bearer ${token}`, Connection: 'close' },
      });
      const body = await response.text();
      assert.equal(response.status, 503, body);
      assert.equal(JSON.parse(body).error, 'memory_capacity');
    } finally {
      s3.releaseHolds();
    }
    await waitForSuccess(job, 'the queued conversion completes after the release');
    await assertDrained([job]);
  });

  await check('cron releases its memory permit before waking a capacity-parked conversion', async () => {
    const record = (await api('/api/novels?limit=100')).json.data
      .find(entry => entry.toc_url === targetUrl('family-a'));
    assert.ok(record);
    s3.holdNextListing();
    const cron = tick();
    cron.catch(() => {});
    let job;
    try {
      await until(() => s3.heldCount() > 0, 'the real cron is held inside stage collection');
      const queued = await api('/api/jobs', {
        method: 'POST',
        body: { kind: 'Convert', targets: [String(record.id)], options: [] },
      });
      assert.equal(queued.status, 200, queued.text);
      job = queued.json.ids[0];
      await until(() => observation.deliveries.some(delivery => delivery.phase === 'after'
        && delivery.messages.some(message => message.body?.job_id === job)),
      'conversion delivery finishes while cron owns the memory permit');
      const parked = await jobRow(job);
      assert.equal(parked.status, 'pending', JSON.stringify(parked));
      assert.equal(parked.admission_waiting, 1, JSON.stringify(parked));
      assert.equal(parked.attempts, 0, JSON.stringify(parked));
      assert.equal(parked.recovery_attempts, 0, JSON.stringify(parked));

      // Stop the wake AFTER publication. The real consumer now runs while
      // cron still awaits send(), exposing an under-permit wake deterministically.
      observation.holdConvertSend();
      s3.releaseHolds();
      await until(() => observation.heldConvertSends().length > 0, 'cron publishes its conversion wake');
      await waitForSuccess(job, 'the woken conversion runs before cron send returns', 15000);
    } finally {
      observation.releaseConvertSend();
      s3.releaseHolds();
      await cron;
    }
    await assertDrained([job]);
  });

  // --- 5. 異常終了 / 期限切れ lease --------------------------------------
  await check('an abrupt crash recovers an expired lease without laundering attempts', async () => {
    const beforeRequests = sources.get('family-a').paths().filter(path => path === '/family-a/family-a-book1/').length;
    sources.get('family-a').hold();
    const job = await enqueueDownload('family-a');
    await until(() => sources.get('family-a').pending() > 0, 'the lane reaches the held upstream');

    const captured = laneDeliveries(job);
    assert.ok(captured.length >= 1, 'the delivery must be observed before the crash');
    const envelope = captured[0].body;
    assert.equal(envelope.version, 3, JSON.stringify(envelope));
    const lane = captured[0].lane;
    assert.equal(typeof lane, 'number', JSON.stringify(captured[0]));
    crashEnvelope = { ...envelope, lane };

    const running = await jobRow(job);
    assert.equal(running.status, 'running', JSON.stringify(running));
    assert.equal(running.site_key, 'family-a', JSON.stringify(running));
    assert.equal(running.attempts, 0, JSON.stringify(running));
    assert.equal(running.execution_token, envelope.dispatch_token, JSON.stringify(running));
    const held = (await laneRows()).find(row => row.lane === lane);
    assert.equal(held.job_id, job, JSON.stringify(held));
    assert.equal(held.dispatch_token, envelope.dispatch_token, JSON.stringify(held));

    // 異常終了: delivery は ack されていないのでキューに残ったまま。
    await killTopology();

    // 唯一変えるのは lease の期限。status / token / attempts / checkpoint は
    // そのまま。lane 側の lease も同時に失効させる (実 crash なら双方の lease
    // が同時に切れ、早い奪取はしない)。
    const quote = value => `'${String(value).replaceAll("'", "''")}'`;
    const before = offlineSql({
      config: topology.writerPath,
      persist: workspace.persist,
      sql: `SELECT * FROM worker_jobs WHERE job_id=${quote(job)};`,
    })[0];
    offlineSql({
      config: topology.writerPath,
      persist: workspace.persist,
      sql: `UPDATE worker_jobs SET lease_until=${quote(EXPIRED)} WHERE job_id=${quote(job)};`,
    });
    offlineSql({
      config: topology.writerPath,
      persist: workspace.persist,
      sql: `UPDATE worker_acquisition_lanes SET lease_until=${quote(EXPIRED)} WHERE job_id=${quote(job)};`,
    });
    const expired = offlineSql({
      config: topology.writerPath,
      persist: workspace.persist,
      sql: `SELECT * FROM worker_jobs WHERE job_id=${quote(job)};`,
    })[0];
    assert.deepEqual(expired, { ...before, lease_until: EXPIRED }, 'expiration changes no recovery state');

    await startTopology();
    await tick();

    const recovered = await until(async () => {
      const row = await jobRow(job);
      return row && row.recovery_attempts >= 1 ? row : false;
    }, 'the lost execution is reclaimed');
    assert.equal(recovered.recovery_attempts, 1, JSON.stringify(recovered));
    assert.equal(recovered.attempts, 0, 'a lost execution must not masquerade as a handled business failure');
    assert.equal(recovered.checkpoint_json, before.checkpoint_json, JSON.stringify(recovered));

    // 回復した実行は実際に走るので、保留していた upstream を解いて終端まで進める。
    sources.get('family-a').release();
    await waitForSuccess(job, 'the recovered job succeeds');
    await assertDrained([job]);
    const finished = await jobRow(job);
    assert.equal(finished.attempts, 0, JSON.stringify(finished));
    assert.equal(sources.get('family-a').paths().filter(path => path === '/family-a/family-a-book1/').length,
      beforeRequests + 2, 'exactly one pre-crash execution and one recovered execution');
  });

  // --- 5b. 古い assignment の配送は ack されるだけ ------------------------
  await check('a stale assignment delivery is acked without work and without releasing the replacement', async () => {
    assert.ok(crashEnvelope, 'the crashed delivery must be captured first');
    const original = crashEnvelope;

    // Hold real sends after D1 reservations, before delivery. A held running
    // replacement cannot receive a second delivery with max_concurrency=1.
    const families = FAMILIES.slice(0, LANE_COUNT);
    for (let lane = 0; lane < LANE_COUNT; lane += 1) observation.holdLaneSend(lane);
    const accepted = Promise.all(families.map(family => enqueueDownload(family)));
    accepted.catch(() => {}); // Cleanup still awaits/cancels the real requests.
    const occupied = await until(async () => {
      const rows = (await laneRows()).filter(lane => lane.job_id);
      return rows.length === LANE_COUNT
        && rows.every(row => observation.heldSends(row.lane).length > 0) ? rows : false;
    }, 'four replacement reservations reach the producer-send barrier');
    const replacement = occupied.find(lane => lane.lane === original.lane);
    assert.ok(replacement, `lane ${original.lane} must have a replacement reservation`);

    // 古い token の envelope を実キューへ再生する。
    const before = await jobRow(original.job_id);
    const beforeLane = (await laneRows()).find(lane => lane.lane === original.lane);
    const beforeCounts = Object.fromEntries(families.map(family => [family, sources.get(family).count()]));
    const beforeDeliveries = laneDeliveries(original.job_id).length;
    const beforeHandled = laneDeliveries(original.job_id, 'after').length;
    const injected = await api('/__fixture/acquisition/inject', {
      method: 'POST',
      body: {
        binding: `ACQUISITION_QUEUE_${original.lane}`,
        body: {
          version: 3,
          job_id: original.job_id,
          lane: original.lane,
          dispatch_token: original.dispatch_token,
        },
      },
    });
    assert.equal(injected.status, 200, injected.text);
    assert.equal(injected.json?.ok, true, injected.text);

    // 再生された delivery が production へ届き、処理まで終えるのを待つ。
    await until(
      () => laneDeliveries(original.job_id).length > beforeDeliveries,
      'the stale delivery arrives at the lane',
    );
    await until(
      () => laneDeliveries(original.job_id, 'after').length > beforeHandled,
      'the stale delivery is handled',
    );

    // 古い token は仕事をして execution を持ち替えず、replacement を解放もしない。
    const after = await jobRow(original.job_id);
    const afterLane = (await laneRows()).find(lane => lane.lane === original.lane);
    assert.deepEqual(after, before, 'a stale delivery must not mutate the terminal job row');
    assert.deepEqual(afterLane, beforeLane, 'a stale delivery must not release the replacement lane');
    for (const family of families) {
      assert.equal(
        sources.get(family).count(),
        beforeCounts[family],
        `a stale delivery must not run work for ${family}`,
      );
    }

    for (let lane = 0; lane < LANE_COUNT; lane += 1) observation.releaseLaneSend(lane);
    const jobs = await accepted;
    for (const id of jobs) await waitForSuccess(id, `replacement job ${id} succeeds`);
    await assertDrained(jobs);
  });

  // --- 6. 旧 version-2 入口は新レーンへ drain される ---------------------
  await check('a legacy ingress message is drained into the execution lanes', async () => {
    const families = FAMILIES.slice(0, LANE_COUNT);
    for (const family of families) sources.get(family).hold();
    const holders = {};
    for (const family of families) holders[family] = await enqueueDownload(family);
    await until(
      () => families.every(family => sources.get(family).pending() > 0),
      'four lanes are occupied',
    );

    // 旧 version-1 入口: 計画ごと message に入っているので writer が台帳行を
    // 作り、そのまま dispatcher へ載せる (旧入口経路そのものの証明)。
    sources.get('family-e').hold();
    const known = new Set((await observe()).jobs.map(row => row.job_id));
    const legacy = await api('/__fixture/acquisition/inject', {
      method: 'POST',
      body: {
        binding: 'NAROU_JOBS',
        body: {
          version: 1,
          job: { kind: 'Download', targets: [targetUrl('family-e')], options: [] },
        },
      },
    });
    assert.equal(legacy.status, 200, legacy.text);
    assert.equal(legacy.json?.ok, true, legacy.text);
    const v1Delivery = await until(() => {
      const found = legacyPlanDeliveries('/family-e/');
      return found.length >= 1 ? found : false;
    }, 'the writer receives the version-1 ingress message');
    assert.equal(v1Delivery[0].role, 'writer', JSON.stringify(v1Delivery[0]));
    const created = await until(async () => {
      const rows = (await observe()).jobs.filter(row => !known.has(row.job_id));
      return rows.find(row => row.site_key === 'family-e') ?? false;
    }, 'the legacy ingress message creates a durable row with its identity');
    assert.notEqual(created.status, 'running', JSON.stringify(created));

    // 旧 version-2 入口: 既存行を drain する。writer は実行しない。
    const v2 = await api('/__fixture/acquisition/inject', {
      method: 'POST',
      body: { binding: 'NAROU_JOBS', body: { version: 2, job_id: created.job_id } },
    });
    assert.equal(v2.status, 200, v2.text);
    assert.equal(v2.json?.ok, true, v2.text);
    const drained = await until(() => {
      const found = legacyDeliveries(created.job_id);
      return found.length >= 1 ? found : false;
    }, 'the writer drains the version-2 ingress message');
    assert.equal(drained[0].role, 'writer', JSON.stringify(drained[0]));
    assert.equal(drained[0].lane, null, JSON.stringify(drained[0]));

    // 空きが出たら待っていた job は新レーンで実行される。
    sources.get('family-a').release();
    await until(() => sources.get('family-e').pending() > 0, 'the drained job advances into a lane');
    sources.get('family-e').release();
    for (const family of families.slice(1)) sources.get(family).release();
    for (const id of Object.values(holders)) await waitForSuccess(id, `holder job ${id} succeeds`);
    await waitForSuccess(created.job_id, 'the drained job succeeds');
    const executed = laneDeliveries(created.job_id);
    assert.ok(executed.length >= 1, 'the drained job must be executed by a lane');
    assert.equal(executed[0].role, 'acquisition', JSON.stringify(executed[0]));
    await assertDrained([...Object.values(holders), created.job_id]);
  });

  // --- 7. 全件 / 手動 / タグの入口がすべて dispatcher へ入る ---------------
  await check('automatic, manual and tag ingress all feed the lane dispatcher', async () => {
    // 手動: POST /api/download。
    sources.get('family-c').hold();
    const manual = await enqueueDownload('family-c');
    await until(() => sources.get('family-c').pending() > 0, 'the manual job reaches the upstream');
    assert.ok(laneDeliveries(manual).length >= 1, 'the manual job must run in a lane');
    sources.get('family-c').release();
    await waitForSuccess(manual, 'the manual job succeeds');

    // タグ: POST /api/update_by_tag。
    const novels = (await api('/api/novels?limit=100')).json;
    const tagged = novels.data.find(entry => entry.toc_url === targetUrl('family-b'));
    assert.ok(tagged, 'a library novel is required for the tag ingress');
    const tagResponse = await api(`/api/novels/${tagged.id}/tags`, {
      method: 'POST',
      body: { tags: [TAG] },
    });
    assert.equal(tagResponse.status, 200, tagResponse.text);
    const known = new Set((await observe()).jobs.map(row => row.job_id));
    const tagUpdate = await api('/api/update_by_tag', { method: 'POST', body: { tags: [TAG] } });
    assert.equal(tagUpdate.status, 200, tagUpdate.text);
    assert.equal(tagUpdate.json?.success, true, tagUpdate.text);
    assert.ok(tagUpdate.json.count >= 1, tagUpdate.text);
    const taggedJob = await until(async () => {
      const rows = (await observe()).jobs.filter(row => row.kind === 'update' && !known.has(row.job_id));
      return rows[0] ?? false;
    }, 'the tag ingress enqueues an update job');
    await until(
      () => laneDeliveries(taggedJob.job_id).length >= 1,
      'the tag update job is delivered to a lane',
    );
    await waitForSuccess(taggedJob.job_id, 'the tag update job succeeds');

    // 自動更新: POST /api/update。
    const automatic = await api('/api/update', {
      method: 'POST',
      body: { targets: [targetUrl('family-a')], force: true },
    });
    assert.equal(automatic.status, 200, automatic.text);
    assert.equal(automatic.json?.success, true, automatic.text);
    assert.ok(Array.isArray(automatic.json?.job_ids), automatic.text);
    assert.equal(automatic.json.job_ids.length, 1, automatic.text);
    const automaticJob = automatic.json.job_ids[0];
    await until(
      () => laneDeliveries(automaticJob).length >= 1,
      'the automatic update job is delivered to a lane',
    );
    await waitForSuccess(automaticJob, 'the automatic update job succeeds');

    await assertDrained([manual, taggedJob.job_id, automaticJob]);

    // The actual automatic scheduler is a separate ingress from manual Update.
    const knownBeforeCron = new Set((await observe()).jobs.map(row => row.job_id));
    const settings = await api('/api/global_setting', { method: 'POST', body: { settings: {
      'update.auto-schedule.enable': true,
      'update.auto-schedule': '0000',
      'update.auto-schedule.timezone': 'UTC',
      'update.disabled-author-tracking': true,
    } } });
    assert.equal(settings.status, 200, settings.text);
    assert.equal(settings.json.success, true, settings.text);
    await tick();
    const scheduled = await until(async () => {
      const rows = (await observe()).jobs.filter(row => row.kind === 'update' && !knownBeforeCron.has(row.job_id));
      return rows.length === novels.data.length ? rows : false;
    }, 'the real automatic scheduler enqueues every library novel');
    for (const row of scheduled) {
      await waitForSuccess(row.job_id, `scheduled update ${row.job_id} succeeds`);
      assert.ok(laneDeliveries(row.job_id).length > 0, 'scheduled work must execute in a lane');
    }
    await assertDrained(scheduled.map(row => row.job_id));
  });

  console.log(`\n${steps.filter(step => step.ok).length}/${steps.length} acquisition lane checks passed`);
} catch (error) {
  if (dev?.logs) console.error(dev.logs);
  throw error;
} finally {
  await killTopology();
  for (const source of sources.values()) {
    await source.close();
  }
  await observation.close();
  if (s3) await s3.close();
  workspace.cleanup();
}
