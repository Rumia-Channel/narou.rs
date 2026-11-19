export const TRACE_HEADER = 'x-narou-ci-seq';
// Fixture-only header present on every response the deployed Worker produced.
// An edge-generated response (route not propagated at this colo) lacks it.
export const DISPATCH_HEADER = 'x-test-transfer-dispatched';
export const MAX_TRACE_ATTEMPTS = 20_000;
export const PREFLIGHT_ATTEMPTS = 90;
export const TRACE_SCHEMA = `CREATE TABLE fixture_transfer_traces (
  seq INTEGER NOT NULL,
  outcome TEXT NOT NULL,
  http_status INTEGER NOT NULL,
  PRIMARY KEY (seq, outcome, http_status)
) WITHOUT ROWID;`;

const outcomes = new Set([
  'ok', 'canceled', 'exception', 'exceededCpu', 'exceededMemory',
  'unknown', 'scriptNotFound', 'responseStreamDisconnected',
]);
export const traceOutcome = value => outcomes.has(value) ? value : 'unknown';
export const isHttpStatus = value => Number.isInteger(value) && value >= 100 && value <= 599;

function entryStatus(entry) {
  return typeof entry === 'number' ? entry : entry.status;
}
function entryDispatched(entry) {
  // Legacy bare-status entries are always treated as dispatched so the
  // receipt requirement stays strict for callers that do not record it.
  return typeof entry === 'number' ? true : entry.dispatched === true;
}

export function verifyTraceReceipts(expected, rows) {
  const seen = new Map();
  for (const {seq, outcome, http_status: status} of rows) {
    if (!Number.isInteger(seq) || seq < 1 || seq > expected.length) {
      throw new Error('Cloudflare Tail Worker reported an unexpected request sequence');
    }
    if (outcome !== 'ok' && outcome !== 'canceled') {
      throw new Error(`Cloudflare Tail Worker request ${seq} ended with ${traceOutcome(outcome)}`);
    }
    const entry = expected[seq - 1];
    const wanted = entryStatus(entry);
    if (!entryDispatched(entry) && wanted !== 0) {
      throw new Error(`Cloudflare Tail Worker reported a receipt for request ${seq} the edge answered without dispatching`);
    }
    if (!(status === 0 || isHttpStatus(status)) ||
        (outcome === 'ok' && (!isHttpStatus(status) || (wanted !== 0 && wanted !== status))) ||
        (outcome === 'canceled' && wanted !== 0)) {
      throw new Error(`Cloudflare Tail Worker status or outcome did not match request ${seq}`);
    }
    const value = outcome === 'canceled' ? -status - 1 : status;
    if (seen.has(seq) && seen.get(seq) !== value) {
      throw new Error(`Cloudflare Tail Worker reported conflicting receipts for request ${seq}`);
    }
    seen.set(seq, value);
  }
  return expected.length - seen.size;
}

export function missingTraceEvidence(expected, rows, firstSequence) {
  const seen = new Set(rows.map(row => row.seq));
  const sample = [];
  let unaccounted = 0;
  let withoutResponse = 0;
  let edge = 0;
  for (const [index, entry] of expected.entries()) {
    const seq = firstSequence + index;
    if (seen.has(seq)) continue;
    const status = entryStatus(entry);
    if (status === 0) withoutResponse++;
    else if (entryDispatched(entry)) unaccounted++;
    else edge++;
    if (sample.length < 8) sample.push({seq, expected_status: status, dispatched: entryDispatched(entry)});
  }
  return {unaccounted, without_response: withoutResponse, edge, sample};
}

