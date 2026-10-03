import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const source = fs.readFileSync(
  process.env.NAROU_RENDER_SOURCE || new URL('../src/web/assets/js/ui/render.js', import.meta.url),
  'utf8',
);
function functionSource(name) {
  const match = source.match(new RegExp(`function ${name}\\([^]*?\\n\\}`));
  assert.ok(match, `missing ${name} helper`);
  return match[0];
}
// Model browser text-node serialization, deliberately preserving quotation
// marks. Quotes are legal text and textContent/innerHTML does not escape them.
const document = {
  createElement() {
    return {
      textContent: '',
      get innerHTML() {
        return String(this.textContent).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
      },
    };
  },
};
const context = vm.createContext({ document, materialIcon: () => '' });
vm.runInContext(functionSource('esc') + '\n' + functionSource('escAttr'), context);
// The fallback makes this regression demonstrably fail against the old source.
if (source.includes('function renderNovelSourceLink(')) {
  vm.runInContext(functionSource('renderNovelSourceLink'), context);
} else {
  const fragment = source.match(/  const tocUrl = novel\.display_url[^]*?\n    : '';/);
  assert.ok(fragment, 'missing source-link renderer');
  vm.runInContext(`function renderNovelSourceLink(novel) {\n${fragment[0]}\nreturn tocLink;\n}`, context);
}
const render = novel => context.renderNovelSourceLink(novel);
const attrEscape = value => String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

for (const field of ['display_url', 'toc_url']) {
  test(`${field}: quotes remain inside href/title attributes`, () => {
    const url = 'https://ncode.syosetu.com/n1234ab/" onmouseover="this.dataset.probe=1';
    const html = render({ [field]: url });
    assert.ok(html.includes(`href="${attrEscape(url)}"`), html);
    assert.ok(html.includes(`title="${attrEscape(url)}"`), html);
    assert.ok(!html.includes('" onmouseover="'), 'must not create a separate event-handler attribute');
  });
}
test('preserves normal source URLs, Unicode, query strings and fragments', () => {
  const url = 'https://example.com/作品?a=1&b=2#話';
  const html = render({ display_url: url, toc_url: 'https://example.com/api' });
  assert.ok(html.includes(`href="${attrEscape(url)}"`));
  assert.ok(html.includes(`title="${attrEscape(url)}"`));
  assert.ok(!html.includes('example.com/api'));
});
test('falls back to TOC URL and omits an absent source link', () => {
  assert.ok(render({ display_url: '', toc_url: 'https://example.com/toc' }).includes('href="https://example.com/toc"'));
  assert.equal(render({}), '');
});
