import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const uiDir = process.env.NAROU_JS_DIR || fileURLToPath(new URL('../src/web/assets/js/ui', import.meta.url));
const code = fs.readFileSync(path.join(uiDir, 'actions.js'), 'utf8')
  .replace(/^import\s+[^]*?\s+from\s+['"][^'"]+['"];\s*$/gm, '')
  .replace(/^export\s+/gm, '');
const context = vm.createContext({});
vm.runInContext(code, context, { filename: 'actions.js' });

test('hidden settings suggest unique existing tags, authors and sites', () => {
  const suggestions = context.hiddenSuggestions([
    { tags: ['B', 'A', 'vs_42'], author: 'Writer B', sitename: 'Site B' },
    { tags: ['A', '', null], author: 'Writer A', sitename: 'Site A' },
    { tags: ['C'], author: 'Writer A', sitename: 'Site A' },
    { tags: null, author: '', sitename: null },
  ]);
  assert.deepEqual([...suggestions.tags], ['A', 'B', 'C']);
  assert.deepEqual([...suggestions.authors], ['Writer A', 'Writer B']);
  assert.deepEqual([...suggestions.sites], ['Site A', 'Site B']);
  const empty = context.hiddenSuggestions([]);
  assert.deepEqual([...empty.tags], []);
  assert.deepEqual([...empty.authors], []);
  assert.deepEqual([...empty.sites], []);
});
