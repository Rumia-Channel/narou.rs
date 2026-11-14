import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

function harness(selected) {
  const source = fs.readFileSync(new URL('../src/web/assets/js/ui/actions.js', import.meta.url), 'utf8')
    .replace(/^import\s+[^]*?\s+from\s+['"][^'"]+['"];\s*$/gm, '')
    .replace(/^export\s+/gm, '');
  const requests = [];
  const errors = [];
  const context = vm.createContext({
    getSelectedIdsInDisplayOrder: () => [...selected],
    postJson: async (endpoint, body) => { requests.push({ endpoint, ids: [...body.ids] }); return { success: true }; },
    HTMLElement: class {},
    assertApiSuccess(result, message) { if (!result.success) throw new Error(message); },
    showNotification: (message, type) => errors.push({ message, type }),
  });
  vm.runInContext(source, context);
  // Only persistence and rendering are replaced; the action and guard run unchanged.
  vm.runInContext('refreshList = async () => { ++listReads; }; refreshTags = async () => { ++tagReads; };',
    Object.assign(context, { listReads: 0, tagReads: 0 }));
  return { context, requests, errors };
}

test('right-click on a selected novel toggles the entire selection in display order', async () => {
  const h = harness(['3', '1', '2']);
  await h.context.toggleFreeze(1);
  assert.deepEqual(h.requests, [{ endpoint: '/api/freeze', ids: [3, 1, 2] }]);
  assert.equal(h.context.listReads, 1);
  assert.equal(h.context.tagReads, 1);
  assert.deepEqual(h.errors, []);
});

test('right-click outside the selection does not toggle unrelated selected novels', async () => {
  const h = harness(['3', '1']);
  await h.context.toggleFreeze(0);
  assert.deepEqual(h.requests, [{ endpoint: '/api/freeze', ids: [0] }]);
});

test('explicit freeze and unfreeze submit the same complete selection and refresh tags', async () => {
  const h = harness(['3', '1', '2']);
  await h.context.batchAction('/api/novels/freeze', null);
  await h.context.batchAction('/api/novels/unfreeze', null);
  assert.deepEqual(h.requests, [
    { endpoint: '/api/novels/freeze', ids: [3, 1, 2] },
    { endpoint: '/api/novels/unfreeze', ids: [3, 1, 2] },
  ]);
  assert.equal(h.context.listReads, 2);
  assert.equal(h.context.tagReads, 2);
});

test('failed context freeze is reported and does not replace the displayed state', async () => {
  const h = harness(['1', '2']);
  h.context.postJson = async () => ({ success: false });
  await h.context.toggleFreeze(1);
  assert.equal(h.context.listReads, 0);
  assert.equal(h.context.tagReads, 0);
  assert.equal(h.errors.length, 1);
  assert.equal(h.errors[0].type, 'error');
});
