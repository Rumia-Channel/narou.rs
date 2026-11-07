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
  let deferConfigs = false;
  let favoriteTags = [];
  const configRequests = [];
  const timers = [];
  const fetchJson = async (url) => {
    if (url.startsWith('/api/webui/config')) {
      return deferConfigs ? new Promise(resolve => configRequests.push(resolve)) : config;
    }
    if (url === '/api/favorite_tags') return { success: true, tags: favoriteTags };
    if (url.startsWith('/api/tag_list')) {
      if (offline) throw new Error('offline');
      return { tags, tag_colors: {} };
    }
    throw new Error(`unexpected request: ${url}`);
  };
  const render = loadModule('render.js', { State: state, El: { tagListCanvas: canvas }, document, window: {} });
  const actions = loadModule('actions.js', {
    State: state, El: {}, fetchJson, renderTagList: render.renderTagList, renderNovelList() {},
  });
  const main = loadModule('../main.js', {
    State: state, El: {}, document, fetchJson,
    refreshTags: actions.refreshTags, refreshList: async () => {},
    refreshFavoriteTags: actions.refreshFavoriteTags,
    setTimeout: callback => { timers.push(callback); return timers.length; },
    captureThemeRead() {}, setCapabilities() {}, syncAttributeTagControls() {},
    syncTagSortOrderSelect() {}, renderQueueStatus() {}, renderNovelList() {},
    captureTagSortRead: actions.captureTagSortRead, applyTagSortOrder: actions.applyTagSortOrder,
    renderTagList: render.renderTagList, createConsoleHistoryPoller: () => ({}),
  });
  render.renderTagList();
  return {
    state,
    reload: (controls = false) => main.reloadWebConfig(controls),
    displayed: () => canvas.children.map((child) => child.textContent),
    configure(order, orderedTags) { config = { ...config, tag_sort_order: order }; tags = orderedTags; },
    offline() { offline = true; },
    deferConfigs() { deferConfigs = true; },
    configRequests,
    setServerFavorites(next) { favoriteTags = next; },
    async flushEvents() { while (timers.length) await timers.shift()(); },
    favoritesChanged(next, orderedTags) {
      favoriteTags = next; tags = orderedTags;
      main.handleWsMessage({ type: 'favorite_tags.reload' });
    },
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

function tagRefreshHarness() {
  const requests = [];
  const state = { tags: ['existing'], tagColors: { existing: 'blue' } };
  const displayed = [];
  const context = loadModule('actions.js', {
    State: state, El: {},
    fetchJson: () => new Promise((resolve, reject) => requests.push({ resolve, reject })),
    renderTagList: () => displayed.push([...state.tags]), renderNovelList() {},
  });
  return { state, requests, displayed, refresh: () => context.refreshTags() };
}

test('an old tag response cannot replace a newer order when the trailing request fails', async () => {
  const h = tagRefreshHarness();
  const first = h.refresh();
  await new Promise(setImmediate);
  const second = h.refresh();
  h.requests[0].resolve({ tags: ['stale'], tag_colors: {} });
  await new Promise(setImmediate);
  assert.deepEqual(h.state.tags, ['existing']);
  h.requests[1].reject(new Error('offline'));
  await Promise.all([first, second]);
  assert.deepEqual(h.state.tags, ['existing']);
  assert.deepEqual(h.state.tagColors, { existing: 'blue' });
  assert.deepEqual(h.displayed, []);
});

test('an invalid tag payload preserves the last visible list and colors', async () => {
  const h = tagRefreshHarness();
  const pending = h.refresh();
  await new Promise(setImmediate);
  h.requests[0].resolve({ success: false, message: 'store unavailable' });
  await pending;
  assert.deepEqual(h.state.tags, ['existing']);
  assert.deepEqual(h.state.tagColors, { existing: 'blue' });
});

function tagSortHarness() {
  const writes = [];
  const state = { tagSortOrder: 'count', tags: ['Zulu', 'Alpha'], tagColors: {} };
  let saved = 'count';
  const context = loadModule('actions.js', {
    State: state, El: {},
    postJson: (_url, body) => new Promise((resolve, reject) => writes.push({
      value: body.settings['webui.tag-sort-order'],
      succeed() { saved = this.value; resolve({ success: true }); },
      fail() { reject(new Error('store unavailable')); },
    })),
    fetchJson: async url => url.startsWith('/api/webui/config')
      ? { tag_sort_order: saved }
      : { tags: saved === 'alphabetical' ? ['Alpha', 'Zulu'] : ['Zulu', 'Alpha'] },
    assertApiSuccess(result) { if (!result.success) throw new Error(result.message); },
    renderTagList() {}, renderNovelList() {}, showNotification() {},
  });
  return { state, writes, context, saved: () => saved, externalSave(value) { saved = value; } };
}

test('rapid tag order changes save in selection order and ignore a stale config read', async () => {
  const h = tagSortHarness();
  const read = h.context.captureTagSortRead();
  const first = h.context.saveTagSortOrder('alphabetical');
  const second = h.context.saveTagSortOrder('favorites_first');
  await new Promise(setImmediate);
  assert.equal(h.writes.length, 1, 'the second write must wait for the first');
  h.context.applyTagSortOrder('count', read);
  assert.equal(h.state.tagSortOrder, 'favorites_first');
  h.writes[0].succeed();
  await new Promise(setImmediate);
  h.writes[1].succeed();
  await Promise.all([first, second]);
  h.context.applyTagSortOrder('count', read);
  assert.equal(h.saved(), 'favorites_first');
  assert.equal(h.state.tagSortOrder, 'favorites_first');
  assert.deepEqual(Array.from(h.state.tags), ['Zulu', 'Alpha']);
});

test('a config snapshot read during a save cannot arrive afterward and undo that save', async () => {
  const h = tagSortHarness();
  const save = h.context.saveTagSortOrder('alphabetical');
  await new Promise(setImmediate);
  const duringSave = h.context.captureTagSortRead();
  const staleValue = h.saved();
  h.writes[0].succeed();
  await save;
  h.context.applyTagSortOrder(staleValue, duringSave);
  assert.equal(h.saved(), 'alphabetical');
  assert.equal(h.state.tagSortOrder, 'alphabetical');
  assert.deepEqual(Array.from(h.state.tags), ['Alpha', 'Zulu']);
});

test('a failed last order save restores the last acknowledged order, not an unsaved selection', async () => {
  const h = tagSortHarness();
  const first = h.context.saveTagSortOrder('alphabetical');
  const second = h.context.saveTagSortOrder('favorites_first');
  await new Promise(setImmediate);
  h.writes[0].fail();
  await new Promise(setImmediate);
  h.writes[1].fail();
  await Promise.all([first, second]);
  assert.equal(h.saved(), 'count');
  assert.equal(h.state.tagSortOrder, 'count');
});

test('a favorite mutation refreshes the visible tag order and invalidates an earlier favorite read', async () => {
  const state = { tags: ['Alpha', 'Zulu'], tagColors: {}, favoriteTags: ['Alpha'] };
  let resolveOldRead;
  let saved = false;
  let visible = [...state.tags];
  const context = loadModule('actions.js', {
    State: state, El: {},
    fetchJson: (url) => url === '/api/favorite_tags'
      ? new Promise(resolve => { resolveOldRead = resolve; })
      : Promise.resolve({ tags: saved ? ['Zulu', 'Alpha'] : ['Alpha', 'Zulu'], tag_colors: {} }),
    postJson: async () => { saved = true; return { success: true, tags: ['Zulu', 'Alpha'] }; },
    renderTagList: () => { visible = [...state.tags]; }, renderNovelList() {}, showNotification() {},
  });
  const oldRead = context.refreshFavoriteTags();
  await context.setFavoriteTags({ action: 'reorder', tags: ['Zulu', 'Alpha'] });
  resolveOldRead({ success: true, tags: ['Alpha'] });
  await oldRead;
  assert.deepEqual(visible, ['Zulu', 'Alpha']);
  assert.deepEqual(Array.from(state.favoriteTags), ['Zulu', 'Alpha']);
});

test('two rapid favorite toggles apply in order using the acknowledged state', async () => {
  const state = { favoriteTags: [], tags: ['Zulu'], tagColors: {} };
  const writes = [];
  const context = loadModule('actions.js', {
    State: state, El: {},
    postJson: (_url, body) => new Promise(resolve => writes.push({
      action: body.action,
      finish() { resolve({ success: true, tags: body.action === 'add' ? ['Zulu'] : [] }); },
    })),
    fetchJson: async () => ({ tags: ['Zulu'], tag_colors: {} }),
    renderTagList() {}, renderNovelList() {}, showNotification() {},
  });
  const first = context.toggleFavoriteTag('Zulu');
  const second = context.toggleFavoriteTag('Zulu');
  await new Promise(setImmediate);
  assert.equal(writes.length, 1);
  assert.equal(writes[0].action, 'add');
  writes[0].finish();
  await new Promise(setImmediate);
  assert.equal(writes[1].action, 'remove');
  writes[1].finish();
  await Promise.all([first, second]);
  assert.deepEqual(Array.from(state.favoriteTags), []);
});

test('favorite stars expose the current action after membership changes', () => {
  const state = { favoriteTags: [] };
  const button = { attributes: {}, setAttribute(name, value) { this.attributes[name] = value; } };
  const chip = { dataset: { tag: '1' }, querySelector: () => button };
  const context = loadModule('actions.js', {
    State: state, El: { tagEditorCurrent: { querySelectorAll: () => [chip] } },
  });
  context.syncFavoriteStars();
  button.setAttribute('aria-label', button.title);
  const originalLabel = button.title;
  state.favoriteTags = ['1'];
  context.syncFavoriteStars();
  assert.notEqual(button.title, originalLabel);
  assert.equal(button.attributes['aria-label'], button.title);
  state.favoriteTags = [];
  context.syncFavoriteStars();
  assert.equal(button.attributes['aria-label'], originalLabel);
});

test('capability recovery restores original descriptions after repeated unavailable passes', () => {
  for (const originalTitle of ['', 'Open the selected folder']) {
    const state = { capabilities: null, selectedIds: new Set([1]) };
    const element = {
      dataset: {}, title: originalTitle, disabled: false, attributes: {},
      classList: { contains: () => false, toggle() {} },
      setAttribute(name, value) { this.attributes[name] = value; },
    };
    const context = loadModule('../core/capabilities.js', { State: state });
    context.markActionCapability(element, 'open_folder');
    context.markActionCapability(element, 'open_folder');
    assert.equal(element.disabled, true);
    state.capabilities = { actions: { open_folder: true } };
    context.markActionCapability(element, 'open_folder');
    assert.equal(element.disabled, false);
    assert.equal(element.attributes['aria-disabled'], 'false');
    assert.equal(element.title, originalTitle);
  }
});

test('a failed favorite registration reports failure without a success notification', async () => {
  const notifications = [];
  const state = { favoriteTags: ['existing'] };
  const context = loadModule('actions.js', {
    State: state, El: { newTagInput: { value: 'New' } },
    postJson: async () => ({ success: false, message: 'store unavailable' }),
    showNotification: (message, level) => notifications.push({ message, level }),
  });
  await context.addFavoriteTagFromInput();
  assert.deepEqual(state.favoriteTags, ['existing']);
  assert.deepEqual(notifications, [{ message: 'store unavailable', level: 'error' }]);
});

test('an older config response cannot undo a newer external tag order', async () => {
  const h = tagConfigHarness();
  h.deferConfigs();
  const older = h.reload();
  const newer = h.reload();
  h.configRequests[1]({ tag_sort_order: 'alphabetical', attribute_tags_enabled: false });
  await newer;
  h.configRequests[0]({ tag_sort_order: 'favorites_first', attribute_tags_enabled: true });
  await older;
  assert.equal(h.state.tagSortOrder, 'alphabetical');
  assert.equal(h.state.attributeTagsEnabled, false);
  assert.deepEqual(h.displayed(), ['Alpha', 'Middle', 'Zulu']);
});

test('favorite invalidations update an already-open menu and registered favorites', async () => {
  const h = tagConfigHarness();
  h.favoritesChanged(['Zulu', 'Alpha'], ['Zulu', 'Alpha', 'Middle']);
  h.favoritesChanged(['Alpha', 'Zulu'], ['Alpha', 'Zulu', 'Middle']);
  await h.flushEvents();
  assert.deepEqual(h.displayed(), ['Alpha', 'Zulu', 'Middle']);
  assert.deepEqual(h.state.favoriteTags, ['Alpha', 'Zulu']);
});

test('a sort invalidation during a pending save is reconciled after its response', async () => {
  const h = tagSortHarness();
  const saving = h.context.saveTagSortOrder('alphabetical');
  await new Promise(setImmediate);
  h.writes[0].succeed();
  h.externalSave('favorites_first');
  h.context.applyTagSortOrder('favorites_first', h.context.captureTagSortRead());
  await saving;
  assert.equal(h.state.tagSortOrder, 'favorites_first');
  assert.deepEqual(h.state.tags, ['Zulu', 'Alpha']);
});

test('favorite invalidations during a save retain a later change from another tab', async () => {
  const state = { favoriteTags: ['Zulu'], tags: ['Zulu', 'Alpha', 'Middle'], tagColors: {} };
  let finishSave;
  let serverFavorites = ['Zulu'];
  const context = loadModule('actions.js', {
    State: state, El: {},
    postJson: () => new Promise(resolve => { finishSave = resolve; }),
    fetchJson: async url => url === '/api/favorite_tags'
      ? { success: true, tags: [...serverFavorites] }
      : { tags: ['Zulu', 'Alpha', 'Middle'], tag_colors: {} },
    renderTagList() {}, renderNovelList() {}, showNotification() {},
  });
  const saving = context.setFavoriteTags({ action: 'add', tags: ['Alpha'] });
  await new Promise(setImmediate);
  serverFavorites = ['Zulu', 'Alpha', 'Middle'];
  await context.refreshFavoriteTags();
  finishSave({ success: true, tags: ['Zulu', 'Alpha'] });
  await saving;
  assert.deepEqual(state.favoriteTags, ['Zulu', 'Alpha', 'Middle']);
});

test('a newer config read preserves a reconnect recovery request', async () => {
  const h = tagConfigHarness();
  h.configure('count', ['Middle', 'Zulu', 'Alpha']);
  h.setServerFavorites(['Middle']);
  h.deferConfigs();
  const recovery = h.reload(true);
  const update = h.reload();
  h.configRequests[1]({ tag_sort_order: 'count', attribute_tags_enabled: false });
  await update;
  h.configRequests[0]({ tag_sort_order: 'count', attribute_tags_enabled: false });
  await recovery;
  assert.deepEqual(h.displayed(), ['Middle', 'Zulu', 'Alpha']);
  assert.deepEqual(h.state.favoriteTags, ['Middle']);
});
