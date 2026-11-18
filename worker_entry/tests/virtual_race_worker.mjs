// Test-only wrapper: let two real D1 snapshots complete before either caller
// continues. SQL, stored values, transactions and HTTP handlers remain real.
import worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

// ── ブラウザ確認 (AWS WAF) の合成上流 ───────────────────────────────────────
//
// 予約済みホストだけを合成回答で受け、返り値は Worker の Rust 側
// (downloader::http_policy と Job の失敗区分) がそのまま処理する。実ホスト
// には触れない: 合成回答は完了した HTTP 応答だけ。
const WAF_CHALLENGE_HOST = 'waf-challenge.invalid';
const WAF_RELAY_HOST = 'waf-relay.invalid';
const WAF_RELAY_ENDPOINT = `https://${WAF_RELAY_HOST}/`;
const WAF_RELAY_TOKEN = 'waf-fixture-token';
const WAF_RELAY_VARS = {
  NAROU_RELAY_ENDPOINT: WAF_RELAY_ENDPOINT,
  NAROU_RELAY_KEY: WAF_RELAY_TOKEN,
};

function fixtureResponse(status, headers, body) {
  return new Response(body, { status, headers });
}

/**
 * リレー呼び出し (POST /proxy) への合成回答。
 *
 * 踏み台は取得先の URL を本体に含む。合成上流のときだけブラウザ確認の答え
 * (405 + wafAction) を返し、それ以外は踏み台側の拒否として扱う (呼び出し
 * 側は元の回答を保つ)。
 */
async function relayFixture(input) {
  let target = '';
  try {
    const request = input instanceof Request ? input : new Request(input);
    const payload = await request.json();
    target = String(payload?.url ?? '');
  } catch {
    target = '';
  }
  if (target.includes(WAF_CHALLENGE_HOST) || target.includes('localhost')) {
    return fixtureResponse(200, { 'content-type': 'application/json' }, JSON.stringify({
      status: 405,
      contentType: 'text/html',
      wafAction: 'captcha',
      setCookie: [],
      via: 'curl',
      body: '',
    }));
  }
  return fixtureResponse(200, { 'content-type': 'application/json' }, JSON.stringify({
    error: 'fixture relay: the upstream is not part of this fixture',
  }));
}

// 予約済みホストの回答だけを差し替える。他の取得は元の fetch に渡す。
// 差し替えられない環境でも他の検査は動くようにしておく。
const realFetch = globalThis.fetch;
const interceptedFetch = async (input, init) => {
  let host = '';
  try {
    host = new URL(typeof input === 'string' ? input : (input?.url ?? '')).hostname;
  } catch {
    host = '';
  }
  if (host === WAF_CHALLENGE_HOST) {
    // 202 + ブラウザ確認 (本文は空)。認証が必要な回答。
    return fixtureResponse(202, { 'x-amzn-waf-action': 'challenge' }, '');
  }
  if (host === 'localhost') {
    // 403 (ブラウザ確認のヘッダ無し)。接続を試す段は接続拒否で失敗する。
    return fixtureResponse(403, {}, '');
  }
  if (host === WAF_RELAY_HOST) {
    return relayFixture(input);
  }
  return realFetch(input, init);
};
try {
  Object.defineProperty(globalThis, 'fetch', {
    value: interceptedFetch,
    configurable: true,
    writable: true,
    enumerable: false,
  });
} catch {
  try {
    globalThis.fetch = interceptedFetch;
  } catch {
    // 差し替え不能な環境: 他の検査はそのまま働く。
  }
}

let jobsPaused = false;
const waitTick = () => new Promise(resolve => setTimeout(resolve, 25));
async function waitForJobs() {
  // Timers belong to the waiting invocation. Resolving a shared Promise from
  // a later HTTP request can cancel workerd's cross-request continuation.
  while (jobsPaused) await waitTick();
}
async function waitForGate(condition, description) {
  const deadline = Date.now() + 10000;
  while (!condition()) {
    if (Date.now() >= deadline) throw new Error(`D1 snapshot ${description} timed out`);
    await waitTick();
  }
}
const barriers = new Map();
async function meet(name) {
  let barrier = barriers.get(name);
  if (!barrier) {
    barrier = { arrived: 0, completed: 0 };
    barriers.set(name, barrier);
  }
  barrier.arrived += 1;
  try {
    await waitForGate(() => barrier.arrived === 2, `barrier ${name}`);
  } finally {
    if (++barrier.completed === 2) barriers.delete(name);
  }
}

const pauses = new Map();
function pauseFor(name) {
  if (!pauses.has(name)) {
    pauses.set(name, { entered: false, released: false });
  }
  return pauses.get(name);
}
async function pause(name) {
  const gate = pauseFor(name);
  gate.entered = true;
  try {
    await waitForGate(() => gate.released, `pause ${name}`);
  } finally {
    pauses.delete(name);
  }
}

function gatedDatabase(database, phase, paused) {
  const gated = new Set();
  const originals = new WeakMap();
  const sqlByStatement = new WeakMap();
  const statement = (target, sql, values = []) => {
    const proxy = new Proxy(target, {
      get(target, name) {
        if (name === 'constructor') return target.constructor;
        if (name === 'bind') return (...bound) => statement(target.bind(...bound), sql, bound);
        if (['first', 'all', 'raw', 'run'].includes(name)) return async (...args) => {
          const result = await target[name](...args);
          if (phase && /^\s*SELECT\b/i.test(sql) && /\bapp_state\b/i.test(sql)) {
            const key = ['virtual_novel_members', ...(phase === 'membership-create' ? ['tag_sequence'] : [])]
              .find(key => sql.includes(key) || values.includes(key));
            if (key && !gated.has(key)) {
              gated.add(key);
              await meet(`${phase}:${key}`);
            }
          }
          if (paused === 'member-classification' && !gated.has('member-rows') && /^\s*SELECT\b/i.test(sql) &&
              /\bFROM novels n\b/.test(sql) && /\bn\.id IN\b/.test(sql)) {
            gated.add('member-rows');
            await pause(paused);
          }
          return result;
        };
        const value = Reflect.get(target, name, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });
    originals.set(proxy, target);
    sqlByStatement.set(proxy, sql);
    return proxy;
  };
  const wrap = target => new Proxy(target, {
    get(target, name) {
      if (name === 'constructor') return target.constructor;
      if (name === 'prepare') return sql => statement(target.prepare(sql), sql);
      if (name === 'batch') return async statements => {
        if (paused === 'create-namespace' && !gated.has('creation-batch') && statements.some(value => {
          const sql = sqlByStatement.get(value) || '';
          return /INSERT INTO novel_id_sequence/.test(sql) && /novel_namespace_claims/.test(sql);
        })) {
          gated.add('creation-batch');
          await pause(paused);
        }
        return target.batch(statements.map(value => originals.get(value) || value));
      };
      if (name === 'withSession') return (...args) => wrap(target.withSession(...args));
      const value = Reflect.get(target, name, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  return wrap(database);
}

export default class SnapshotRaceWorker extends worker {
  async fetch(request) {
    const jobs = new URL(request.url).pathname.match(/^\/__test\/jobs\/(pause|resume|wait)$/);
    if (jobs) {
      if (jobs[1] === 'wait') {
        await waitForJobs();
      } else {
        jobsPaused = jobs[1] === 'pause';
      }
      return Response.json({ success: true });
    }
    const control = new URL(request.url).pathname.match(/^\/__test\/virtual-pause\/([^/]+)\/(wait|release)$/);
    if (control) {
      const gate = pauseFor(control[1]);
      if (control[2] === 'wait') await waitForGate(() => gate.entered, `entry ${control[1]}`);
      else gate.released = true;
      return Response.json({ success: true });
    }
    const phase = request.headers.get('x-narou-test-snapshot-race');
    const paused = request.headers.get('x-narou-test-snapshot-pause');
    if (!phase && !paused) return super.fetch(request);
    return new worker(this.ctx, { ...this.env, DB: gatedDatabase(this.env.DB, phase, paused) }).fetch(request);
  }

  async queue(batch) {
    // The writer owns the pause barrier; lane isolates consult the same gate.
    // Wait outside production, without ACK, retry or ledger-state fabrication.
    if (this.env.NAROU_WORKER_ROLE === 'acquisition') {
      const response = await this.env.NAROU_WRITER.fetch('http://fixture/__test/jobs/wait');
      if (!response.ok) throw new Error(`queue pause barrier: ${response.status}`);
      await response.text();
    } else {
      await waitForJobs();
    }
    // ブラウザ確認の合成リレーだけを設定する。踏み台の答えは合成上流に
    // 対するもので、他の取得先の回答には触れない。
    return new worker(this.ctx, { ...this.env, ...WAF_RELAY_VARS }).queue(batch);
  }
}
