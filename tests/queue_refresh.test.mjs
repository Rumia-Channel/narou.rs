import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const jsDir = process.env.NAROU_JS_DIR || fileURLToPath(new URL('../src/web/assets/js/ui', import.meta.url));
function source(file) {
  return fs.readFileSync(path.join(jsDir, file), 'utf8');
}
function loadModule(file, globals = {}) {
  // Execute the actual module body, retaining module-level request counters.
  // Its imports are dependency boundaries replaced by deterministic test stubs.
  const code = source(file).replace(/^import\s+[^]*?\s+from\s+['"][^'"]+['"];\s*$/gm, '').replace(/^export\s+/gm, '');
  const context = vm.createContext(globals);
  vm.runInContext(code, context, { filename: file });
  return context;
}
function queueHelper() { return loadModule('render.js', { document: { addEventListener() {} }, window: { addEventListener() {} } }).assertQueueActionSuccess; }

test('cancel response: native/Worker legacy error is rejected with its message', () => {
  assert.throws(() => queueHelper()({ error: 'cancel failed' }, 'fallback'), /cancel failed/);
});
test('queue response: nested error envelope is rejected with its message', () => {
  assert.throws(() => queueHelper()({ error: { code: 'ledger_error', message: 'ledger unavailable' } }, 'fallback'), /ledger unavailable/);
});
test('queue response: success:false remains rejected and success formats are preserved', () => {
  const check = queueHelper();
  assert.throws(() => check({ success: false, message: 'failed' }, 'fallback'), /failed/);
  assert.throws(() => check({ success: false }, 'fallback'), /fallback/);
  for (const response of [{ status: 'ok' }, { success: true }]) assert.equal(check(response, 'fallback'), response);
});

function listHarness() {
  const requests = [];
  const state = { novels: [{ id: 7, title: 'initial', frozen: false }], frozenIds: new Set() };
  let renderCount = 0;
  let pruneCount = 0;
  const context = loadModule('actions.js', {
    State: state,
    fetchJson: () => new Promise((resolve, reject) => requests.push({ resolve, reject })),
    renderNovelList: () => { renderCount++; },
    pruneSelectedIdsToCurrentList: () => { pruneCount++; },
  });
  return { state, requests, refresh: () => context.refreshList(), counts: () => ({ renderCount, pruneCount }) };
}
test('late pre-deletion response cannot overwrite a newer list or frozen state', async () => {
  const h = listHarness();
  const older = h.refresh();
  const newer = h.refresh();
  const current = [{ id: 2, title: 'kept', frozen: true }];
  h.requests[1].resolve({ data: current });
  await newer;
  assert.equal(h.state.novels, current);
  const acceptedCounts = h.counts();
  h.requests[0].resolve({ data: [{ id: 1, title: 'deleted', frozen: false }, { id: 2, frozen: false }] });
  await older;
  assert.equal(h.state.novels, current, 'newer accepted list must remain authoritative');
  assert.deepEqual(Array.from(h.state.frozenIds), ['2']);
  assert.equal(h.counts().pruneCount, acceptedCounts.pruneCount, 'stale response must not prune selection');
  assert.equal(h.counts().renderCount, acceptedCounts.renderCount, 'stale response must not rerender');
});
test('latest successful response still updates list and selection', async () => {
  const h = listHarness();
  const pending = h.refresh();
  const current = [{ id: 3, frozen: true }];
  h.requests[0].resolve({ data: current });
  await pending;
  assert.equal(h.state.novels, current);
  assert.deepEqual(Array.from(h.state.frozenIds), ['3']);
  assert.equal(h.counts().pruneCount, 1);
  assert.equal(h.counts().renderCount, 1);
});
test('request failure leaves previously loaded state intact', async () => {
  const h = listHarness();
  const original = h.state.novels;
  const pending = h.refresh();
  h.requests[0].reject(new Error('offline'));
  await pending;
  assert.equal(h.state.novels, original);
  assert.equal(h.counts().pruneCount, 0);
});

test('newer request failure does not reauthorize an older successful response', async () => {
  const h = listHarness();
  const original = h.state.novels;
  const older = h.refresh();
  const newer = h.refresh();
  h.requests[1].reject(new Error('offline'));
  await newer;
  const acceptedCounts = h.counts();
  h.requests[0].resolve({ data: [{ id: 1, title: 'stale', frozen: true }] });
  await older;
  assert.equal(h.state.novels, original);
  assert.deepEqual(Array.from(h.state.frozenIds), []);
  assert.deepEqual(h.counts(), acceptedCounts);
});
test('older request failure must not rerender over newer successful request', async () => {
  const h = listHarness();
  const older = h.refresh();
  const newer = h.refresh();
  const current = [{ id: 2, frozen: true }];
  h.requests[1].resolve({ data: current });
  await newer;
  const acceptedCounts = h.counts();
  h.requests[0].reject(new Error('old request timed out'));
  await older;
  assert.equal(h.state.novels, current);
  assert.deepEqual(Array.from(h.state.frozenIds), ['2']);
  assert.deepEqual(h.counts(), acceptedCounts);
});
