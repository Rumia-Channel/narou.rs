import assert from 'node:assert/strict';
import {DatabaseSync} from 'node:sqlite';
import test from 'node:test';
import {MAX_TRACE_ATTEMPTS, TRACE_HEADER, TRACE_SCHEMA, missingTraceEvidence, verifyTraceReceipts} from '../ci/transfer_trace.mjs';
import collector from '../ci/transfer_trace_worker.mjs';

const run = '0123456789abcdef';
function event(seq, outcome, status, nonce = run) {
  return {outcome, event: {
    request: {headers: {[TRACE_HEADER]: `${nonce}:${seq}`, authorization: 'REDACTED'}},
    ...(status === undefined ? {} : {response: {status}}),
  }};
}
function fixture(t) {
  const db = new DatabaseSync(':memory:');
  t.after(() => db.close());
  db.exec(TRACE_SCHEMA);
  const env = {NAROU_TRACE_RUN: run, TRACE_DB: {
    prepare: sql => ({bind: (...params) => () => db.prepare(sql).run(...params)}),
    batch: async statements => { for (const statement of statements) statement(); },
  }};
  return {
    async collect(...events) {
      let completion;
      collector.tail(events, env, {waitUntil: pending => { completion = pending; }});
      await completion;
    },
    verify(expected) {
      return verifyTraceReceipts(expected, db.prepare('SELECT seq,outcome,http_status FROM fixture_transfer_traces').all());
    },
  };
}

test('out-of-order and duplicate tail delivery cannot hide a missing earlier wire attempt', async t => {
  const trace = fixture(t);
  await trace.collect(event(2, 'ok', 200), event(2, 'ok', 200), event(1, 'ok', 200, '0000000000000000'),
    event(`1, ${run}:2`, 'ok', 200), event(MAX_TRACE_ATTEMPTS + 1, 'ok', 200));
  assert.equal(trace.verify([0, 200]), 1);
  await trace.collect(event(1, 'canceled'));
  assert.equal(trace.verify([0, 200]), 0);
});

test('an earlier over-memory invocation stays fatal after a successful wire retry', async t => {
  const trace = fixture(t);
  await trace.collect(event(1, 'exceededMemory'), event(2, 'ok', 200));
  assert.throws(() => trace.verify([0, 200]), /request 1 ended with exceededMemory/);
});

test('a completed HTTP 503 is not an isolate failure, but a different status or a cancellation contradicts it', async t => {
  const trace = fixture(t);
  await trace.collect(event(1, 'ok', 503));
  assert.equal(trace.verify([503]), 0);
  assert.throws(() => trace.verify([200]), /did not match request 1/);
  await trace.collect(event(1, 'canceled'));
  assert.throws(() => trace.verify([503]), /did not match request 1/);
  assert.throws(() => trace.verify([0]), /conflicting receipts for request 1/);
});

test('unrecognized provider outcomes are fatal without emitting the raw value', async t => {
  const trace = fixture(t);
  await trace.collect(event(1, 'unrecognized-private-value', 200));
  assert.throws(() => trace.verify([200]), /request 1 ended with unknown$/);
});

test('a missing receipt is fatal only for a request that received a response', () => {
  const expected = [200, 0, 404, 0, 503];
  const rows = [{seq: 1, outcome: 'ok', http_status: 200}, {seq: 3, outcome: 'ok', http_status: 404}];
  assert.deepEqual(missingTraceEvidence(expected, rows, 1), {
    unaccounted: 1,
    without_response: 2,
    sample: [{seq: 2, expected_status: 0}, {seq: 4, expected_status: 0}, {seq: 5, expected_status: 503}],
  });
});

test('a missing no-response attempt samples its sequence without counting as dispatched', () => {
  assert.deepEqual(missingTraceEvidence([0, 200], [], 10), {
    unaccounted: 1,
    without_response: 1,
    sample: [{seq: 10, expected_status: 0}, {seq: 11, expected_status: 200}],
  });
});
