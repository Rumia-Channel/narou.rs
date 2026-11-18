import {isHttpStatus, MAX_TRACE_ATTEMPTS, TRACE_HEADER, traceOutcome} from './transfer_trace.mjs';

const identity = /^([a-f0-9]{16}):([1-9][0-9]{0,4})$/;
const insert = 'INSERT OR IGNORE INTO fixture_transfer_traces (seq,outcome,http_status) VALUES (?,?,?)';

async function store(events, {TRACE_DB: db, NAROU_TRACE_RUN: nonce}) {
  let batch = [];
  for (const {event, outcome} of events) {
    // Only inspect the default redacted view. Never retain URLs, arbitrary
    // header values, console output, or exception payloads.
    const header = event?.request?.headers?.[TRACE_HEADER];
    const match = typeof header === 'string' && identity.exec(header);
    if (!match || match[1] !== nonce) continue;
    const seq = Number(match[2]);
    if (seq > MAX_TRACE_ATTEMPTS) continue;
    const status = event.response?.status;
    batch.push(db.prepare(insert).bind(seq, traceOutcome(outcome), isHttpStatus(status) ? status : 0));
    if (batch.length === 100) {
      await db.batch(batch);
      batch = [];
    }
  }
  if (batch.length) await db.batch(batch);
}

export default {
  tail(events, env, ctx) {
    ctx.waitUntil(store(events, env));
  },
};
