// Test-only D1 faults around real import publication and cancellation cleanup.
// Storage, validation and request handling remain the production implementations.
import Worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

function failingDatabase(database, fault, state) {
  const originals = new WeakMap();
  const statements = new WeakMap();
  const publication = sql => /\bINSERT\s+INTO\s+novels\b/i.test(sql);
  const deletion = sql => /\bDELETE\s+FROM\s+objects\b/i.test(sql);
  const rejectDelete = (sql, values) => {
    if (state.publicationFailed && fault === 'delete' && deletion(sql)
        && values.some(value => typeof value === 'string' && value.startsWith('novels/'))) {
      // Bound the fixture itself: buggy recursive cleanup must fail the
      // assertion, rather than leave an endlessly failing test isolate.
      if (++state.deletes <= 8) throw new Error('fixture cleanup deletion unavailable');
    }
  };
  const statement = (target, sql, values = []) => {
    const proxy = new Proxy(target, {
      get(target, name) {
        if (name === 'constructor') return target.constructor;
        if (name === 'bind') return (...args) => statement(target.bind(...args), sql, args);
        if (['first', 'all', 'raw', 'run'].includes(name)) return async (...args) => {
          if (state.publicationFailed && /\bFROM\s+novels\b/i.test(sql)) {
            ++state.lookups;
            if (fault === 'lookup' && state.lookups <= 8) throw new Error('fixture cleanup lookup unavailable');
          }
          rejectDelete(sql, values);
          return target[name](...args);
        };
        const value = Reflect.get(target, name, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });
    originals.set(proxy, target);
    statements.set(proxy, { sql, values });
    return proxy;
  };
  const wrap = target => new Proxy(target, {
    get(target, name) {
      if (name === 'constructor') return target.constructor;
      if (name === 'prepare') return sql => statement(target.prepare(sql), sql);
      if (name === 'withSession') return (...args) => wrap(target.withSession(...args));
      if (name === 'batch') return async items => {
        const queries = items.map(item => statements.get(item) || { sql: '', values: [] });
        if (queries.some(({ sql }) => publication(sql))) {
          state.publicationFailed = true;
          throw new Error('fixture record publication unavailable');
        }
        for (const { sql, values } of queries) rejectDelete(sql, values);
        return target.batch(items.map(item => originals.get(item) || item));
      };
      const value = Reflect.get(target, name, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  return wrap(database);
}

export default class TransferFailureWorker extends Worker {
  async fetch(request) {
    const fault = request.headers.get('x-narou-test-cleanup-fault');
    if (!['lookup', 'delete'].includes(fault)) return super.fetch(request);
    const state = { publicationFailed: false, lookups: 0, deletes: 0 };
    const response = await new Worker(this.ctx, {
      ...this.env, DB: failingDatabase(this.env.DB, fault, state),
    }).fetch(request);
    const result = new Response(response.body, response);
    result.headers.set('x-test-cleanup-lookups', String(state.lookups));
    result.headers.set('x-test-cleanup-deletes', String(state.deletes));
    return result;
  }
}
