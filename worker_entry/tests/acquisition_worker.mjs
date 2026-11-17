// 取得レーン構成の観測 wrapper (テスト専用)。
//
// 本番はすべての delivery / 台帳 / admission / settlement を所有する。この
// wrapper は観測と注入だけを行い、本番の判断を代用しない:
//
//   * 永続 lane / 台帳の読み取り専用スナップショットを返す。
//   * どの script がどの delivery を受け取ったかを報告する。
//   * 本番自身が過去に発行した message (捕獲した version-3 envelope や
//     旧 version-2 入口 message) を実キューへ再生し、本番経路がどう反応する
//     かを観察できるようにする。
//
// 注入は「実 message を実キューへ send する」だけである。台帳の行を書き換え
// たり、結果を捏造したり、envelope を書き換えたりはしない。
import Worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

const roleOf = env => env.NAROU_WORKER_ROLE ?? 'writer';

const laneOf = env => {
  const raw = env.NAROU_ACQUISITION_LANE;
  if (raw === undefined || raw === null || raw === '') return null;
  return Number(raw);
};

// Pause only the real producer send, after D1 reserved its lane. This exposes
// the reservation-to-delivery gap without inventing admission or settlement.
function observedEnvironment(env) {
  const wrapped = { ...env };
  const bindings = ['CONVERT_QUEUE', ...Array.from({ length: 4 }, (_, lane) => `ACQUISITION_QUEUE_${lane}`)];
  for (const binding of bindings) {
    const queue = env[binding];
    if (!queue) continue;
    wrapped[binding] = new Proxy(queue, {
      get(target, name) {
        if (name === 'constructor') return target.constructor;
        if (name === 'send') return async (body, ...options) => {
          // Conversion is held after real publication, so its consumer can
          // race the still-awaiting cron without fabricated delivery.
          if (binding === 'CONVERT_QUEUE') await target.send(body, ...options);
          const response = await fetch(`${env.NAROU_ACQUISITION_FIXTURE_ORIGIN}/__fixture/send`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ body, binding }),
          });
          if (!response.ok) throw new Error(`send observation failed: ${response.status}`);
          await response.text();
          if (binding !== 'CONVERT_QUEUE') return target.send(body, ...options);
        };
        const value = Reflect.get(target, name, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });
  }
  return wrapped;
}

export default class AcquisitionObserverWorker extends Worker {
  constructor(ctx, env) {
    super(ctx, observedEnvironment(env));
    // Replay bypasses the send barrier, but still uses the real queue binding.
    this.fixtureQueues = env;
  }

  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname.startsWith('/__fixture/acquisition')) {
      return this.fixture(request, url);
    }
    return super.fetch(request);
  }

  async fixture(request, url) {
    if (request.headers.get('Authorization') !== `Bearer ${this.env.NAROU_ADMIN_TOKEN}`) {
      return new Response('Unauthorized', { status: 401 });
    }
    if (url.pathname === '/__fixture/acquisition' && request.method === 'GET') {
      return Response.json(await this.snapshot());
    }
    if (url.pathname === '/__fixture/acquisition/inject' && request.method === 'POST') {
      const body = await request.json();
      const binding = String(body.binding ?? '');
      const queue = this.fixtureQueues[binding];
      if (!queue || typeof queue.send !== 'function') {
        return Response.json({ ok: false, error: `unknown queue binding ${binding}` }, { status: 400 });
      }
      await queue.send(body.body);
      return Response.json({ ok: true, binding });
    }
    return new Response('Not Found', { status: 404 });
  }

  /** 読み取り専用の lane / 台帳スナップショット。 */
  async snapshot() {
    const jobs = await this.env.DB.prepare(
      `SELECT job_id,kind,target,status,attempts,recovery_attempts,site_key,normalized_target,
              admission_waiting,wait_reason,lease_until,execution_token,checkpoint_json,last_error
         FROM worker_jobs ORDER BY created_at,job_id`,
    ).all();
    const lanes = (await this.env.DB.prepare(
      'SELECT lane,job_id,site_key,dispatch_token,lease_until FROM worker_acquisition_lanes ORDER BY lane',
    ).all()).results;
    return {
      role: roleOf(this.env),
      lane: laneOf(this.env),
      jobs: jobs.results,
      lanes,
    };
  }

  async queue(batch) {
    const messages = [];
    for (const message of Array.from(batch.messages ?? [])) {
      messages.push({ id: message.id, body: message.body });
    }
    const queue = typeof batch.queue === 'function' ? batch.queue() : batch.queue;
    // 'before' は delivery 到着の観測、'after' は production が delivery を
    // 処理し終えた観測。どちらも結果を変えない。
    await this.report({ phase: 'before', queue, messages });
    const result = await super.queue(batch);
    await this.report({ phase: 'after', queue, messages });
    return result;
  }

  async report(payload) {
    const origin = this.env.NAROU_ACQUISITION_FIXTURE_ORIGIN;
    if (!origin) return;
    try {
      await fetch(`${origin}/__fixture/observation`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ role: roleOf(this.env), lane: laneOf(this.env), ...payload }),
      });
    } catch (error) {
      // 観測の失敗は delivery の結果を変えない。
      console.error(`acquisition observation failed: ${error}`);
    }
  }
}
