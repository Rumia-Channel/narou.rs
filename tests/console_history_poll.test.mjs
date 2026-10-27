import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const jsDir = process.env.NAROU_JS_DIR || fileURLToPath(new URL('../src/web/assets/js/core', import.meta.url));
function loadModule(file, globals = {}) {
  const code = fs
    .readFileSync(path.join(jsDir, file), 'utf8')
    .replace(/^import\s+[^]*?\s+from\s+['"][^'"]+['"];\s*$/gm, '')
    .replace(/^export\s+/gm, '');
  const context = vm.createContext(globals);
  vm.runInContext(code, context, { filename: file });
  return context;
}

function makeTarget() {
  const state = { replaced: [], appended: [] };
  return {
    state,
    stream: 'stdout',
    replace(history) { state.replaced.push(history); },
    append(history) { state.appended.push(history); },
  };
}

test('the poller replaces the console with the full history on the first poll', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget();
  const requested = [];
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async (url) => {
      requested.push(url);
      return { history: 'first\nsecond', id: 7 };
    },
    getTargets: () => [target],
  });

  await poller.poll();
  assert.deepEqual(target.state.replaced, ['first\nsecond']);
  assert.deepEqual(target.state.appended, []);
  assert.match(requested[0], /format=json/);
  assert.ok(!requested[0].includes('after='), '最初は差分指定しない');
});

test('the poller appends only the newer lines after the cursor is known', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget();
  const requested = [];
  let cursor = 7;
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async (url) => {
      requested.push(url);
      if (url.includes('after=7')) return { history: 'third', id: 8 };
      return { history: 'first\nsecond', id: 7 };
    },
    getTargets: () => [target],
  });

  await poller.poll();
  await poller.poll();
  assert.deepEqual(target.state.replaced, ['first\nsecond']);
  assert.deepEqual(target.state.appended, ['third']);
  assert.match(requested[1], /after=7/);
});

test('a response without an id replaces the console (Worker)', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget();
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async () => ({ history: 'worker history' }),
    getTargets: () => [target],
  });

  await poller.poll();
  await poller.poll();
  assert.deepEqual(target.state.replaced, ['worker history', 'worker history']);
  assert.deepEqual(target.state.appended, []);
});

test('the poller notifies once and stops cleanly', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget();
  let notified = 0;
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async () => ({ history: 'x', id: 1 }),
    getTargets: () => [target],
    notify: () => { notified += 1; },
    intervalMs: 100000,
  });

  poller.start();
  poller.start();
  assert.equal(notified, 1, '通知は一度だけ');
  assert.equal(poller.isRunning(), true);
  poller.stop();
  assert.equal(poller.isRunning(), false);
});

test('a failing request keeps the console content', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget();
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async () => { throw new Error('network'); },
    getTargets: () => [target],
  });

  await poller.poll();
  assert.deepEqual(target.state.replaced, []);
  assert.deepEqual(target.state.appended, []);
});
