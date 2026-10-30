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
  const code = source(file)
    .replace(/^import\s+[^]*?\s+from\s+['"][^'"]+['"];\s*$/gm, '')
    .replace(/^export\s+/gm, '');
  const context = vm.createContext(globals);
  vm.runInContext(code, context, { filename: file });
  return context;
}

function element(tag) {
  const children = [];
  const listeners = {};
  const element = {
    tagName: 'SPAN',
    className: '',
    dataset: {},
    style: {},
    title: '',
    children,
    listeners,
    draggable: false,
    options: [],
    // textContent を空にすると子要素も消える (DOM と同じ挙動)。
    textContent: '',
    classList: {
      _set: new Set(),
      add(name) { this._set.add(name); },
      remove(name) { this._set.delete(name); },
      toggle(name, force) { if (force) this._set.add(name); else this._set.delete(name); },
      contains(name) { return this._set.has(name); },
    },
    appendChild(child) { children.push(child); return child; },
    addEventListener(name, handler) {
      (listeners[name] ||= []).push(handler);
    },
    remove() {},
    querySelectorAll() { return []; },
    closest() { return null; },
    getBoundingClientRect() { return { left: 0, width: 0 }; },
  };
  // textContent を代入すると子要素も消える (DOM と同じ挙動)。
  let text = '';
  Object.defineProperty(element, 'textContent', {
    get() { return text; },
    set(value) {
      text = value;
      if (value === '') children.length = 0;
    },
  });
  return element;
}

function favoriteHarness() {
  const posted = [];
  const state = { favoriteTags: [], selectedIds: new Set([3]) };
  const list = element('div');
  const empty = element('p');
  const context = loadModule('actions.js', {
    State: state,
    El: { tagFavoritesList: list, tagFavoritesEmpty: empty },
    postJson: (url, body) => {
      posted.push({ url, body });
      if (body.action === 'add') state.favoriteTags = [...state.favoriteTags, ...body.tags];
      if (body.action === 'remove') state.favoriteTags = state.favoriteTags.filter((t) => !body.tags.includes(t));
      return Promise.resolve({ success: true, tags: state.favoriteTags });
    },
    fetchJson: () => Promise.resolve({ success: true, tags: ['A', 'B'] }),
    showNotification() {},
    document: { createElement: (tag) => element(tag) },
    window: { location: { href: '' } },
  });
  return { state, posted, list, empty, context };
}

test('favorite tags are posted to the API and the state is refreshed', async () => {
  const h = favoriteHarness();
  await h.context.refreshFavoriteTags();
  assert.deepEqual(h.state.favoriteTags, ['A', 'B']);
});

test('the favorites list renders one chip per registered tag', async () => {
  const h = favoriteHarness();
  h.state.favoriteTags = ['小説', 'ゲーム'];
  h.context.renderFavoriteTags();
  assert.equal(h.list.children.length, 2);
  // チップは「タグ名 (× ボタン)」の段と「追加ボタン」の段の 2 段構成。
  const names = h.list.children.map((chip) => chip.children[0].textContent);
  assert.deepEqual(names, ['小説', 'ゲーム']);
  assert.equal(h.empty.classList.contains('hide'), true, '空の表示は隠す');
});

test('an empty favorites list shows the guidance instead of chips', () => {
  const h = favoriteHarness();
  h.context.renderFavoriteTags();
  assert.equal(h.list.children.length, 0);
  assert.equal(h.empty.classList.contains('hide'), false);
});

test('the reorder action posts the requested order', async () => {
  const h = favoriteHarness();
  h.state.favoriteTags = ['A', 'B', 'C'];
  await h.context.setFavoriteTags({ action: 'reorder', tags: ['C', 'A', 'B'] });
  const reorder = h.posted.filter((entry) => entry.body.action === 'reorder');
  assert.equal(reorder.length, 1);
  assert.deepEqual(reorder[0].body.tags, ['C', 'A', 'B']);
});

test('a chip is draggable only when there is more than one favorite', () => {
  const h = favoriteHarness();
  h.state.favoriteTags = ['A'];
  h.context.renderFavoriteTags();
  assert.equal(h.list.children.length, 1);
  assert.notEqual(h.list.children[0].draggable, true, '1 件だけなら並べ替え不要');

  h.state.favoriteTags = ['A', 'B'];
  h.context.renderFavoriteTags();
  assert.equal(h.list.children.length, 2);
  assert.equal(h.list.children[0].draggable, true, '2 件以上ならドラッグできる');
  assert.equal(h.list.children[1].draggable, true);
});

test('dropping a chip before another one reorders the favorites', async () => {
  const h = favoriteHarness();
  h.state.favoriteTags = ['A', 'B', 'C'];
  h.context.renderFavoriteTags();

  const source = h.list.children[0]; // A
  const target = h.list.children[2]; // C
  // カーソルを左半分に置くと「前」に入れる。
  target.getBoundingClientRect = () => ({ left: 0, width: 100 });
  const drop = { preventDefault() {}, stopPropagation() {}, clientX: 10, currentTarget: target };
  // ドラッグ開始 → ドロップの順で動かす。
  source.listeners.dragstart[0]({ currentTarget: source, dataTransfer: null });
  await target.listeners.drop[0](drop);
  await new Promise((resolve) => setImmediate(resolve));

  const reorder = h.posted.filter((entry) => entry.body.action === 'reorder');
  assert.equal(reorder.length, 1);
  // A を外すと B, C が残り、C の前に A を入れるので B, A, C。
  assert.deepEqual(reorder[0].body.tags, ['B', 'A', 'C']);
});

test('dropping a chip after another one reorders the favorites', async () => {
  const h = favoriteHarness();
  h.state.favoriteTags = ['A', 'B', 'C'];
  h.context.renderFavoriteTags();

  const source = h.list.children[2]; // C
  const target = h.list.children[0]; // A
  target.getBoundingClientRect = () => ({ left: 0, width: 100 });
  const drop = { preventDefault() {}, stopPropagation() {}, clientX: 90, currentTarget: target };
  source.listeners.dragstart[0]({ currentTarget: source, dataTransfer: null });
  await target.listeners.drop[0](drop);
  await new Promise((resolve) => setImmediate(resolve));

  const reorder = h.posted.filter((entry) => entry.body.action === 'reorder');
  assert.equal(reorder.length, 1);
  // C を外すと A, B が残り、A の後ろ (B の前) に C を入れるので A, C, B。
  assert.deepEqual(reorder[0].body.tags, ['A', 'C', 'B']);
});

test('dropping a chip on itself does not post a reorder', async () => {
  const h = favoriteHarness();
  h.state.favoriteTags = ['A', 'B'];
  h.context.renderFavoriteTags();

  const source = h.list.children[0]; // A
  const drop = { preventDefault() {}, stopPropagation() {}, clientX: 10, currentTarget: source };
  source.listeners.dragstart[0]({ currentTarget: source, dataTransfer: null });
  await source.listeners.drop[0](drop);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(h.posted.filter((entry) => entry.body.action === 'reorder').length, 0);
});
