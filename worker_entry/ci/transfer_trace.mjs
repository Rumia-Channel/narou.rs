export const TRACE_HEADER = 'x-narou-ci-seq';
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

export function verifyTraceReceipts(expected, rows) {
  const seen = new Map();
  for (const {seq, outcome, http_status: status} of rows) {
    if (!Number.isInteger(seq) || seq < 1 || seq > expected.length) {
      throw new Error('Cloudflare Tail Worker reported an unexpected request sequence');
    }
    if (outcome !== 'ok' && outcome !== 'canceled') {
      throw new Error(`Cloudflare Tail Worker request ${seq} ended with ${traceOutcome(outcome)}`);
    }
    const wanted = expected[seq - 1];
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
