// Test-only wrapper for the delivery (reader) Worker.
//
// The reader must never write the shared source data: no D1 write and no S3
// source PUT/DELETE. This wrapper keeps the real Rust Worker (same artifact as
// the writer) but rejects every D1 write statement, so a lazy initialization
// (content_backend marker, attribute backfill, ...) cannot pass unnoticed.
//
// Rejected statements are reported two ways:
//   - `GET /__probe__/d1-writes` returns the attempts seen by this isolate.
//   - every attempt is also POSTed to `NAROU_READ_PROBE_REPORT`, because
//     workerd isolates do not share module state.
import worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

const WRITE_STATEMENT = /\b(?:INSERT|UPDATE|DELETE|REPLACE|CREATE|DROP|ALTER|TRUNCATE)\b/i;

const attempts = [];
let reportEndpoint = null;

function record(sql) {
  const entry = { sql: String(sql).replace(/\s+/g, ' ').trim().slice(0, 300), at: Date.now() };
  attempts.push(entry);
  if (reportEndpoint) {
    // 別 isolate でも拾えるよう fixture へ直ちに報告する。
    fetch(reportEndpoint, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(entry),
    }).catch(() => {});
  }
  return entry;
}

function guardedDatabase(database) {
  const wrapStatement = (statement, sql) => {
    const executable = sql.replace(/'(?:''|[^'])*'|"(?:""|[^"])*"|--[^\n]*|\/\*[\s\S]*?\*\//g, '');
    if (WRITE_STATEMENT.test(executable)) {
      record(sql);
      throw new Error(`read worker probe: D1 write rejected: ${sql}`);
    }
    return statement;
  };
  const wrap = (target) =>
    new Proxy(target, {
      get(target, name) {
        if (name === 'constructor') return target.constructor;
        if (name === 'prepare') {
          return (sql) => wrapStatement(target.prepare(sql), sql);
        }
        if (name === 'exec') {
          return (sql) => {
            record(sql);
            throw new Error(`read worker probe: D1 exec rejected: ${sql}`);
          };
        }
        if (name === 'batch') {
          return (statements) => target.batch(statements);
        }
        if (name === 'withSession') {
          return (...args) => wrap(target.withSession(...args));
        }
        const value = Reflect.get(target, name, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });
  return wrap(database);
}

export default class ReadWorkerProbe extends worker {
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname.startsWith('/__writer/')) {
      url.pathname = url.pathname.slice('/__writer'.length);
      return this.env.NAROU_WRITER.fetch(new Request(url, request));
    }
    if (url.pathname === '/__probe__/d1-writes') {
      return Response.json({ attempts });
    }
    const env = { ...this.env };
    reportEndpoint = env.NAROU_READ_PROBE_REPORT ?? null;
    if (env.DB) env.DB = guardedDatabase(env.DB);
    return new worker(this.ctx, env).fetch(request);
  }
}
