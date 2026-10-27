import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const jsDir = process.env.NAROU_JS_DIR || fileURLToPath(new URL('../src/web/assets/js/core', import.meta.url));
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

// 最小限の DOM (テキスト・span だけを展開できる)。
function makeDocument() {
  function textNode(text) {
    return { nodeType: 3, textContent: text, childNodes: [] };
  }
  function elementNode(tag) {
    return {
      nodeType: 1,
      tagName: String(tag || 'div').toUpperCase(),
      childNodes: [],
      attributes: {},
      setAttribute(name, value) { this.attributes[name] = value; },
      getAttribute(name) { return this.attributes[name] || ''; },
      appendChild(child) { this.childNodes.push(child); return child; },
      get textContent() {
        return this.childNodes.map((child) => child.textContent || '').join('');
      },
      set textContent(value) { this.childNodes = [textNode(value)]; },
      querySelectorAll() { return []; },
    };
  }
  function parse(html, into) {
    // <span style="...">text</span> と素のテキストだけを展開する。
    const pattern = /<span([^>]*)>([\s\S]*?)<\/span>|([^<]+)/gi;
    let match;
    while ((match = pattern.exec(html)) !== null) {
      if (match[3] !== undefined) {
        into.appendChild(textNode(match[3]));
        continue;
      }
      const styleMatch = /style="([^"]*)"/i.exec(match[1] || '');
      const span = elementNode('span');
      if (styleMatch) span.setAttribute('style', styleMatch[1]);
      parse(match[2], span);
      into.appendChild(span);
    }
  }
  const document = {
    createElement(tag) {
      const node = elementNode(tag);
      if (String(tag).toLowerCase() === 'template') {
        const content = elementNode('fragment');
        node.content = content;
        Object.defineProperty(node, 'innerHTML', {
          set(value) { parse(value, content); },
          get() { return content.textContent; },
        });
      }
      return node;
    },
    createTextNode: textNode,
  };
  return document;
}

function consoleHarness() {
  const document = makeDocument();
  const context = loadModule('console_html.js', { document, Node: { TEXT_NODE: 3, ELEMENT_NODE: 1 } });
  return { document, context };
}

test('console lines with span markup are rendered as elements, not raw tags', () => {
  const h = consoleHarness();
  const target = h.document.createElement('div');
  h.context.renderConsoleLine(target, '<span style="color:#bbb">更新を開始します</span>');
  const line = target.childNodes[0];
  assert.equal(line.className, 'console-line');
  const spans = line.childNodes.filter((child) => child.tagName === 'SPAN');
  assert.equal(spans.length, 1);
  assert.equal(spans[0].getAttribute('style'), 'color:#bbb');
  assert.equal(spans[0].textContent, '更新を開始します');
  assert.ok(!line.textContent.includes('<span'));
});

test('plain console lines are inserted as text', () => {
  const h = consoleHarness();
  const target = h.document.createElement('div');
  h.context.renderConsoleLine(target, 'ただの行');
  const line = target.childNodes[0];
  assert.equal(line.textContent, 'ただの行');
  assert.equal(line.childNodes[0].nodeType, 3);
});

test('horizontal rules are rendered as hr', () => {
  const h = consoleHarness();
  const target = h.document.createElement('div');
  h.context.renderConsoleLine(target, '<hr>');
  assert.equal(target.childNodes[0].tagName, 'HR');
});

test('unsafe span styles are dropped and only the text is kept', () => {
  const h = consoleHarness();
  const target = h.document.createElement('div');
  h.context.renderConsoleLine(target, '<span style="background:url(javascript:alert(1))">危険</span>');
  const line = target.childNodes[0];
  const spans = line.childNodes.filter((child) => child.tagName === 'SPAN');
  assert.equal(spans.length, 0, '危険な style は span として残さない');
  assert.equal(line.textContent, '危険');
});

test('the color formats the live console allows are kept', () => {
  const h = consoleHarness();
  for (const color of ['#bbb', 'rgb(200,200,200)', 'lime']) {
    const target = h.document.createElement('div');
    h.context.renderConsoleLine(target, `<span style="color:${color}">色</span>`);
    const spans = target.childNodes[0].childNodes.filter((child) => child.tagName === 'SPAN');
    assert.equal(spans.length, 1, `${color} は span として残す`);
    assert.equal(spans[0].getAttribute('style'), `color:${color}`);
  }
});
