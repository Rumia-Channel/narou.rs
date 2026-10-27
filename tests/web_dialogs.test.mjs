// ブラウザ標準の prompt / confirm / alert の代わりに使うダイアログの挙動。
//
// 実際の DOM の代わりに最小限のスタブを使い、OK / キャンセル / Escape で
// Promise がどう解決するかだけを固定する (見た目はブラウザで確認する)。

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const uiDir = process.env.NAROU_JS_DIR || fileURLToPath(new URL('../src/web/assets/js/ui', import.meta.url));

function matchesSelector(element, selector) {
  const idMatch = /^#(.+)$/.exec(selector);
  if (idMatch) return element.id === idMatch[1];
  const typed = /^([a-z]+)\[type="([a-z]+)"\](:checked)?$/i.exec(selector);
  if (typed) {
    const [, tag, type, checked] = typed;
    return element.tagName === tag.toUpperCase()
      && element.type === type
      && (!checked || element.checked === true);
  }
  return element.tagName === selector.toUpperCase();
}

function createDom() {
  function createElement(tag) {
    const listeners = new Map();
    const classes = new Set();
    const attributes = new Map();
    const element = {
      tagName: String(tag).toUpperCase(),
      children: [],
      id: '',
      type: '',
      value: '',
      placeholder: '',
      checked: false,
      textContent: '',
      get className() {
        return [...classes].join(' ');
      },
      set className(value) {
        classes.clear();
        for (const name of String(value).split(/\s+/).filter(Boolean)) classes.add(name);
      },
      classList: {
        add(name) { classes.add(name); },
        remove(name) { classes.delete(name); },
        toggle(name, force) {
          const enabled = force === undefined ? !classes.has(name) : Boolean(force);
          if (enabled) classes.add(name);
          else classes.delete(name);
          return enabled;
        },
        contains(name) { return classes.has(name); },
      },
      setAttribute(name, value) { attributes.set(name, String(value)); },
      getAttribute(name) { return attributes.has(name) ? attributes.get(name) : null; },
      appendChild(child) { element.children.push(child); return child; },
      append(...children) { element.children.push(...children); },
      replaceChildren(...children) { element.children = [...children]; },
      addEventListener(name, handler) {
        if (!listeners.has(name)) listeners.set(name, []);
        listeners.get(name).push(handler);
      },
      dispatch(name) {
        const event = {
          target: element,
          key: '',
          preventDefault() {},
          stopPropagation() {},
        };
        for (const handler of listeners.get(name) || []) handler(event);
      },
      focus() { document.activeElement = element; },
      querySelector(selector) { return findFirst(element, selector); },
      querySelectorAll(selector) { return findAll(element, selector); },
    };
    return element;
  }

  function findFirst(root, selector) {
    for (const child of root.children) {
      if (matchesSelector(child, selector)) return child;
      const nested = findFirst(child, selector);
      if (nested) return nested;
    }
    return null;
  }

  function findAll(root, selector) {
    const found = [];
    for (const child of root.children) {
      if (matchesSelector(child, selector)) found.push(child);
      found.push(...findAll(child, selector));
    }
    return found;
  }

  const document = {
    body: createElement('body'),
    activeElement: null,
    keyHandlers: [],
    createElement(tag) { return createElement(tag); },
    addEventListener(name, handler) {
      if (name === 'keydown') document.keyHandlers.push(handler);
    },
    removeEventListener(name, handler) {
      if (name !== 'keydown') return;
      document.keyHandlers = document.keyHandlers.filter((entry) => entry !== handler);
    },
    press(key) {
      const event = { key, preventDefault() {}, stopPropagation() {} };
      for (const handler of [...document.keyHandlers]) handler(event);
    },
  };

  return {
    document,
    root() {
      return document.body.children[0];
    },
  };
}

function loadDialog() {
  const dom = createDom();
  const code = fs
    .readFileSync(path.join(uiDir, 'dialog.js'), 'utf8')
    .replace(/^export\s+/gm, '');
  const context = vm.createContext({ document: dom.document, Symbol, Promise });
  vm.runInContext(code, context, { filename: 'dialog.js' });
  return { dom, context };
}

test('confirmDialog resolves true on OK and false on cancel', async () => {
  const { dom, context } = loadDialog();
  const root = () => dom.root();

  const accepted = context.confirmDialog({ title: '削除の確認', message: '削除しますか？' });
  assert.equal(root().querySelector('#app-dialog-title').textContent, '削除の確認');
  assert.equal(root().querySelector('#app-dialog-message').textContent, '削除しますか？');
  root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(await accepted, true);
  assert.equal(root().classList.contains('hide'), true);

  const rejected = context.confirmDialog({ message: 'もう一度' });
  root().querySelector('#app-dialog-cancel').dispatch('click');
  assert.equal(await rejected, false);
});

test('Escape cancels the open dialog', async () => {
  const { dom, context } = loadDialog();
  const pending = context.confirmDialog({ message: 'x' });
  dom.document.press('Escape');
  assert.equal(await pending, false);
});

test('promptDialog returns the typed value and keeps invalid input open', async () => {
  const { dom, context } = loadDialog();
  const pending = context.promptDialog({ title: '名前', value: 'old' });
  const root = () => dom.root();
  const input = root().querySelector('input[type="text"]');
  assert.equal(input.value, 'old');

  input.value = '   ';
  root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(root().classList.contains('hide'), false, '空の必須入力では閉じない');
  assert.equal(input.classList.contains('app-dialog-input-invalid'), true);

  input.value = '  新しい名前  ';
  root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(await pending, '新しい名前');
});

test('promptDialog allows an empty value when not required', async () => {
  const { dom, context } = loadDialog();
  const pending = context.promptDialog({ required: false, value: 'x' });
  const input = dom.root().querySelector('input[type="text"]');
  input.value = '';
  dom.root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(await pending, '');
});

test('selectDialog resolves the chosen option', async () => {
  const { dom, context } = loadDialog();
  const pending = context.selectDialog({
    title: '対象を選択',
    options: [
      { value: '1', label: 'vs_1 一つ目' },
      { value: '2', label: 'vs_2 二つ目', detail: '3 作品' },
    ],
    value: '1',
  });
  const root = () => dom.root();
  const radios = root().querySelectorAll('input[type="radio"]');
  assert.equal(radios.length, 2);
  assert.equal(radios[0].checked, true, '初期選択が入る');

  radios[0].checked = false;
  radios[1].checked = true;
  root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(await pending, '2');
});

test('formDialog returns every field and validates required ones', async () => {
  const { dom, context } = loadDialog();
  const pending = context.formDialog({
    title: '登録',
    fields: [
      { name: 'title', label: 'タイトル', required: true },
      { name: 'author', label: '作者名' },
    ],
  });
  const root = () => dom.root();
  const inputs = root().querySelectorAll('input[type="text"]');
  assert.equal(inputs.length, 2);

  inputs[1].value = 'Author';
  root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(root().classList.contains('hide'), false, 'タイトル未入力では閉じない');

  inputs[0].value = 'XXX';
  root().querySelector('#app-dialog-ok').dispatch('click');
  const values = await pending;
  assert.equal(values.title, 'XXX');
  assert.equal(values.author, 'Author');
});

test('formDialog attaches searchable suggestions to a field', async () => {
  const { dom, context } = loadDialog();
  const pending = context.formDialog({
    title: '登録',
    fields: [
      { name: 'title', label: 'タイトル', required: true },
      { name: 'author', label: '作者名', suggestions: ['あてきち', '城野白'] },
    ],
  });
  const root = () => dom.root();
  const inputs = root().querySelectorAll('input[type="text"]');
  const listId = inputs[1].getAttribute('list');
  assert.ok(listId, '候補を持つ入力欄は datalist を参照する');
  const datalist = root().querySelector('datalist');
  assert.equal(datalist.id, listId);
  assert.deepEqual(datalist.children.map((option) => option.value), ['あてきち', '城野白']);
  assert.equal(inputs[0].getAttribute('list'), null, '候補の無い入力欄には付けない');
  root().querySelector('#app-dialog-cancel').dispatch('click');
  await pending;
});

test('alertDialog hides the cancel button and resolves on OK', async () => {
  const { dom, context } = loadDialog();
  const pending = context.alertDialog({ message: '設定しました' });
  const root = () => dom.root();
  assert.equal(root().querySelector('#app-dialog-cancel').classList.contains('hide'), true);
  root().querySelector('#app-dialog-ok').dispatch('click');
  await pending;
  assert.equal(root().classList.contains('hide'), true);
});

test('a new dialog replaces one that is still open', async () => {
  const { dom, context } = loadDialog();
  const first = context.confirmDialog({ message: '最初' });
  const second = context.confirmDialog({ message: '次' });
  assert.equal(await first, false, '置き換えられたダイアログはキャンセル扱い');
  dom.root().querySelector('#app-dialog-ok').dispatch('click');
  assert.equal(await second, true);
});
