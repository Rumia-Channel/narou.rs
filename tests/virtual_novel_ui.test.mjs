// バーチャル小説の Web UI ロジック (対象の決定) の回帰。
//
// 一覧で仮想小説をちょうど 1 件選択しているときは、ncode の入力を求めずに
// その作品を操作対象にする。選択が無い・複数あるときはダイアログで選ばせる。

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const uiDir = process.env.NAROU_JS_DIR || fileURLToPath(new URL('../src/web/assets/js/ui', import.meta.url));

function loadActions() {
  const code = fs
    .readFileSync(path.join(uiDir, 'actions.js'), 'utf8')
    .replace(/^import\s+[^]*?\s+from\s+['"][^'"]+['"];\s*$/gm, '')
    .replace(/^export\s+/gm, '');
  const context = vm.createContext({});
  vm.runInContext(code, context, { filename: 'actions.js' });
  return context;
}

test('isVirtualNcode only accepts vs_<number>', () => {
  const actions = loadActions();
  assert.equal(actions.isVirtualNcode('vs_12'), true);
  assert.equal(actions.isVirtualNcode('vs_'), false);
  assert.equal(actions.isVirtualNcode('n1234ab'), false);
  assert.equal(actions.isVirtualNcode(undefined), false);
  assert.equal(actions.isVirtualNcode(null), false);
});

test('the only selected virtual novel becomes the target', () => {
  const actions = loadActions();
  const novels = [
    { id: 1, ncode: 'n1234ab' },
    { id: 7, ncode: 'vs_7' },
    { id: 8, ncode: 'vs_8' },
  ];
  assert.equal(actions.selectedVirtualNovel(novels, new Set(['7'])).id, 7);
  assert.equal(actions.selectedVirtualNovel(novels, new Set(['7', '1'])).id, 7);
  assert.equal(
    actions.selectedVirtualNovel(novels, new Set(['7', '8'])),
    null,
    '仮想小説を 2 件選んでいるときは自動では決めない',
  );
  assert.equal(
    actions.selectedVirtualNovel(novels, new Set(['1'])),
    null,
    '通常の小説はバーチャル小説の対象にならない',
  );
  assert.equal(actions.selectedVirtualNovel(novels, new Set()), null);
  assert.equal(actions.selectedVirtualNovel([], new Set(['7'])), null);
});

test('author and site suggestions are unique, non-empty and sorted', () => {
  const actions = loadActions();
  const novels = [
    { id: 1, author: 'B', sitename: 'site2' },
    { id: 2, author: 'A', sitename: 'site1' },
    { id: 3, author: 'A', sitename: 'site1' },
    { id: 4, author: '', sitename: null },
  ];
  assert.deepEqual([...actions.uniqueValues(novels, 'author')], ['A', 'B']);
  assert.deepEqual([...actions.uniqueValues(novels, 'sitename')], ['site1', 'site2']);
  assert.equal(actions.sharedValue(novels.slice(1, 3), 'author'), 'A');
  assert.equal(
    actions.sharedValue(novels, 'author'),
    '',
    '共通でないときは初期値を入れない',
  );
});
