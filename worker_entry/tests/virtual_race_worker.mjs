// Test-only wrapper: let two real D1 snapshots complete before either caller
// continues. SQL, stored values, transactions and HTTP handlers remain real.
import worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

const barriers = new Map();
async function meet(name) {
  let barrier = barriers.get(name);
  if (!barrier) {
    let release;
    const ready = new Promise(resolve => { release = resolve; });
    barrier = { arrived: 0, completed: 0, ready, release };
    barriers.set(name, barrier);
  }
  if (++barrier.arrived === 2) barrier.release();
  let timer;
  try {
    await Promise.race([
      barrier.ready,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`D1 snapshot barrier timed out: ${name}`)), 10000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
    if (++barrier.completed === 2) barriers.delete(name);
  }
}

const pauses = new Map();
function pauseFor(name) {
  if (!pauses.has(name)) {
    let arrive, release;
    const entered = new Promise(resolve => { arrive = resolve; });
    const resumed = new Promise(resolve => { release = resolve; });
    pauses.set(name, { entered, resumed, arrive, release });
  }
  return pauses.get(name);
}
async function pause(name) {
  const gate = pauseFor(name);
  gate.arrive();
  let timer;
  try {
    await Promise.race([gate.resumed, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`D1 snapshot pause timed out: ${name}`)), 10000);
    })]);
  } finally {
    clearTimeout(timer);
    pauses.delete(name);
  }
}

function gatedDatabase(database, phase, paused) {
  const gated = new Set();
  const originals = new WeakMap();
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
          if (paused && !gated.has('member-rows') && /^\s*SELECT\b/i.test(sql) &&
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
    return proxy;
  };
  const wrap = target => new Proxy(target, {
    get(target, name) {
      if (name === 'constructor') return target.constructor;
      if (name === 'prepare') return sql => statement(target.prepare(sql), sql);
      if (name === 'batch') return statements => target.batch(statements.map(value => originals.get(value) || value));
      if (name === 'withSession') return (...args) => wrap(target.withSession(...args));
      const value = Reflect.get(target, name, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  return wrap(database);
}

export default class SnapshotRaceWorker extends worker {
  async fetch(request) {
    const control = new URL(request.url).pathname.match(/^\/__test\/virtual-pause\/([^/]+)\/(wait|release)$/);
    if (control) {
      const gate = pauseFor(control[1]);
      if (control[2] === 'wait') await gate.entered;
      else gate.release();
      return Response.json({ success: true });
    }
    const phase = request.headers.get('x-narou-test-snapshot-race');
    const paused = request.headers.get('x-narou-test-snapshot-pause');
    if (!phase && !paused) return super.fetch(request);
    return new worker(this.ctx, { ...this.env, DB: gatedDatabase(this.env.DB, phase, paused) }).fetch(request);
  }
}
