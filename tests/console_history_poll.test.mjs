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

function makeTarget(stream = 'stdout') {
  const state = { replaced: [], appended: [] };
  return {
    state,
    stream,
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

test('each stream keeps its own cursor', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const stdout = makeTarget('stdout');
  const stdout2 = makeTarget('stdout2');
  const requested = [];
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async (url) => {
      requested.push(url);
      if (url.includes('stream=stdout2')) return { history: 'second console', id: 20 };
      return { history: 'first console', id: 11 };
    },
    getTargets: () => [stdout, stdout2],
  });

  await poller.poll();
  assert.match(requested[0], /stream=stdout$/);
  assert.match(requested[1], /stream=stdout2$/);
  assert.deepEqual(stdout.state.replaced, ['first console']);
  assert.deepEqual(stdout2.state.replaced, ['second console']);

  // 2 回目の取得はストリームごとのカーソルを使う。
  await poller.poll();
  assert.match(requested[2], /stream=stdout&after=11/);
  assert.match(requested[3], /stream=stdout2&after=20/);
});

test('a failed stream keeps its cursor so no line is skipped', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const stdout = makeTarget('stdout');
  const stdout2 = makeTarget('stdout2');
  const requested = [];
  let failStdout2 = true;
  const poller = context.createConsoleHistoryPoller({
    fetchJson: async (url) => {
      requested.push(url);
      if (url.includes('stream=stdout2')) {
        if (failStdout2) throw new Error('network');
        return { history: 'second console new', id: 20 };
      }
      return { history: 'first console', id: 11 };
    },
    getTargets: () => [stdout, stdout2],
  });

  await poller.poll();
  assert.deepEqual(stdout.state.replaced, ['first console']);
  assert.deepEqual(stdout2.state.replaced, [], '取得に失敗したストリームは描かない');

  // 取得に失敗したストリームはカーソルが進んでいないので、
  // 次の取得で未取得の行が飛ばされない。
  failStdout2 = false;
  await poller.poll();
  assert.match(requested[2], /stream=stdout&after=11/);
  assert.match(requested[3], /stream=stdout2$/);
  assert.ok(!requested[3].includes('after='), '失敗したストリームはカーソルなし');
  assert.deepEqual(stdout2.state.replaced, ['second console new']);
});

test('a fetch that resolves after stop does not render', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget('stdout');
  let release = null;
  const poller = context.createConsoleHistoryPoller({
    fetchJson: () => new Promise((resolve) => { release = resolve; }),
    getTargets: () => [target],
  });

  const pending = poller.poll();
  // 取得の途中で接続が切り替わり (stop)、遅れて結果が返る。
  poller.stop();
  release({ history: 'late line', id: 5 });
  await pending;

  assert.deepEqual(target.state.replaced, [], '停止後に届いた結果は描かない');
  assert.deepEqual(target.state.appended, []);
});

test('overlapping ticks do not apply the same delta twice', async () => {
  const context = loadModule('console_history.js', { setInterval, clearInterval, setTimeout, console });
  const target = makeTarget('stdout');
  const requested = [];
  let release = null;
  const poller = context.createConsoleHistoryPoller({
    fetchJson: () => {
      requested.push('request');
      return new Promise((resolve) => { release = resolve; });
    },
    getTargets: () => [target],
  });

  const first = poller.poll();
  // 前の取得が終わらないうちに次の tick が走っても重ねない。
  const second = poller.poll();
  assert.equal(requested.length, 1, '取得は重ねない');
  release({ history: 'line', id: 3 });
  await first;
  await second;

  assert.deepEqual(target.state.replaced, ['line']);
  assert.deepEqual(target.state.appended, []);
});
