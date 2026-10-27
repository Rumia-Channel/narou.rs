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
  return {
    tagName: 'SPAN',
    className: '',
    dataset: {},
    style: {},
    textContent: '',
    title: '',
    children,
    classList: {
      _set: new Set(),
      add(name) { this._set.add(name); },
      remove(name) { this._set.delete(name); },
      toggle(name, force) { if (force) this._set.add(name); else this._set.delete(name); },
      contains(name) { return this._set.has(name); },
    },
    appendChild(child) { children.push(child); return child; },
    addEventListener() {},
    remove() {},
    querySelectorAll() { return []; },
    closest() { return null; },
  };
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
