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
test('a refresh requested during deletion skips the old payload and applies the next payload', async () => {
  const h = listHarness();
  const older = h.refresh();
  await new Promise(setImmediate);
  const newer = h.refresh();
  assert.equal(h.requests.length, 1, 'refreshes share the in-flight HTTP request');
  const initial = h.state.novels;
  h.requests[0].resolve({ data: [{ id: 1, title: 'deleted', frozen: false }] });
  await new Promise(setImmediate);
  assert.equal(h.state.novels, initial, 'pre-deletion data must not reappear');
  assert.equal(h.counts().pruneCount, 0);
  const current = [{ id: 2, title: 'kept', frozen: true }];
  h.requests[1].resolve({ data: current });
  await Promise.all([older, newer]);
  assert.equal(h.state.novels, current);
  assert.deepEqual(Array.from(h.state.frozenIds), ['2']);
});
test('latest successful response still updates list and selection', async () => {
  const h = listHarness();
  const pending = h.refresh();
  await new Promise(setImmediate);
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
  await new Promise(setImmediate);
  h.requests[0].reject(new Error('offline'));
  await pending;
  assert.equal(h.state.novels, original);
  assert.equal(h.counts().pruneCount, 0);
});

test('failure of a queued refresh cannot reauthorize the previous stale payload', async () => {
  const h = listHarness();
  const original = h.state.novels;
  const older = h.refresh();
  await new Promise(setImmediate);
  const newer = h.refresh();
  h.requests[0].resolve({ data: [{ id: 1, title: 'stale', frozen: true }] });
  await new Promise(setImmediate);
  h.requests[1].reject(new Error('offline'));
  await Promise.all([older, newer]);
  assert.equal(h.state.novels, original);
  assert.deepEqual(Array.from(h.state.frozenIds), []);
  assert.equal(h.counts().pruneCount, 0);
});

function tagConfigHarness() {
  const state = { tags: ['Zulu', 'Alpha', 'Middle'], tagColors: {}, tagSortOrder: 'count', attributeTagsEnabled: false, performanceMode: 'off' };
  const canvas = {
    children: [],
    set textContent(_) { this.children = []; },
    appendChild(child) { this.children.push(child); },
    closest() { return null; },
  };
  const document = {
    body: { classList: { toggle() {} } },
    addEventListener() {},
    getElementById() { return null; },
    createElement() { return { dataset: {}, addEventListener() {} }; },
  };
  let config = { tag_sort_order: 'alphabetical', attribute_tags_enabled: false };
  let tags = ['Alpha', 'Middle', 'Zulu'];
  let offline = false;
  const fetchJson = async (url) => {
    if (url.startsWith('/api/webui/config')) return config;
    if (url.startsWith('/api/tag_list')) {
      if (offline) throw new Error('offline');
      return { tags, tag_colors: {} };
    }
    throw new Error(`unexpected request: ${url}`);
  };
  const render = loadModule('render.js', { State: state, El: { tagListCanvas: canvas }, document, window: {} });
  const actions = loadModule('actions.js', {
    State: state, fetchJson, renderTagList: render.renderTagList, renderNovelList() {},
  });
  const main = loadModule('../main.js', {
    State: state, El: {}, document, fetchJson,
    refreshTags: actions.refreshTags, refreshList: async () => {},
    captureThemeRead() {}, setCapabilities() {}, syncAttributeTagControls() {},
    syncTagSortOrderSelect() {}, renderQueueStatus() {}, renderNovelList() {},
    renderTagList: render.renderTagList, createConsoleHistoryPoller: () => ({}),
  });
  render.renderTagList();
  return {
    state,
    reload: () => main.reloadWebConfig(),
    displayed: () => canvas.children.map((child) => child.textContent),
    configure(order, orderedTags) { config = { ...config, tag_sort_order: order }; tags = orderedTags; },
    offline() { offline = true; },
  };
}

test('an open tag menu follows an external order change without reloading the page', async () => {
  const h = tagConfigHarness();
  assert.deepEqual(h.displayed(), ['Zulu', 'Alpha', 'Middle']);
  await h.reload();
  assert.deepEqual(h.displayed(), ['Alpha', 'Middle', 'Zulu']);
  h.configure('favorites_first', ['Middle', 'Zulu', 'Alpha']);
  await h.reload();
  assert.deepEqual(h.displayed(), ['Middle', 'Zulu', 'Alpha']);
});

test('a failed tag refresh preserves the visible tags after an order change', async () => {
  const h = tagConfigHarness();
  h.offline();
  await h.reload();
  assert.deepEqual(h.displayed(), ['Zulu', 'Alpha', 'Middle']);
});
