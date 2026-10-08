import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// main.js の進捗バー実装 (progressBars から formatProgressLabel まで) を
// そのまま切り出して動かす。並びが変わったらここで気付けるよう、境界は
// マーカー文字列で取る。
const source = fs.readFileSync(
  process.env.NAROU_MAIN_SOURCE || new URL('../src/web/assets/js/main.js', import.meta.url),
  'utf8',
);
const start = source.indexOf('var progressBars = {};');
const end = source.indexOf('function sanitizeConsoleSpanStyle');
assert.ok(start >= 0, 'missing progressBars declaration');
assert.ok(end > start, 'missing sanitizeConsoleSpanStyle boundary');
const block = source.slice(start, end);

class FakeElement {
  constructor(className = '') {
    this.className = className;
    this.children = [];
    this.parentElement = null;
    this.textContent = '';
    this.style = {};
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 0;
    this.classList = { toggle() {}, add() {}, remove() {} };
  }

  appendChild(child) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  remove() {
    if (!this.parentElement) return;
    const parent = this.parentElement;
    parent.children = parent.children.filter((child) => child !== this);
    this.parentElement = null;
  }

  get childElementCount() {
    return this.children.length;
  }

  querySelector(selector) {
    if (selector === '.progress-bar') {
      return this.children.find((child) => child.className === 'progress-bar') || null;
    }
    if (selector === '.console-progress-host') {
      return this.children.find((child) => child.className === 'console-progress-host') || null;
    }
    return null;
  }

  set innerHTML(value) {
    if (typeof value === 'string' && value.includes('progress-bar')) {
      this.appendChild(new FakeElement('progress-bar'));
    }
  }
}

function createHarness() {
  const consoles = {
    stdout: new FakeElement('console'),
    stdout2: new FakeElement('console'),
  };
  for (const element of Object.values(consoles)) {
    element.parentElement = new FakeElement('console-col');
  }
  const context = vm.createContext({
    document: {
      createElement: (tag) => new FakeElement(tag),
      getElementById: (id) => (id === 'console-stdout2' ? consoles.stdout2 : consoles.stdout),
    },
    getConsoleEl: (targetConsole) => (targetConsole === 'stdout2' ? consoles.stdout2 : consoles.stdout),
    syncPinnedConsole: () => {},
    setConsolePinned: () => {},
    State: { concurrencyEnabled: true },
    refreshQueue: () => Promise.resolve(),
  });
  vm.runInContext(block, context);
  return { context, consoles, bars: () => context.progressBars };
}

function barCount(context) {
  return Object.keys(context.progressBars).length;
}

test('same topic on the same console keeps a single bar when the job scope changes', () => {
  const { context } = createHarness();
  context.initProgressBar('update', 'stdout', 'job-1');
  context.initProgressBar('update', 'stdout', 'job-2');
  assert.equal(barCount(context), 1, 'stale bar for the previous job must be replaced');
  assert.ok(context.progressBars['stdout:job-2'], 'the newest job keeps the bar');

  context.initProgressBar('update', 'stdout', 'job-3');
  assert.equal(barCount(context), 1);
  assert.ok(context.progressBars['stdout:job-3']);
});

test('different topics and consoles still get their own bar', () => {
  const { context } = createHarness();
  context.initProgressBar('update', 'stdout', 'job-1');
  context.initProgressBar('convert', 'stdout', 'job-2');
  context.initProgressBar('convert', 'stdout2', 'job-3');
  assert.equal(barCount(context), 3);
});

test('a bar without a job scope does not evict job bars of the same topic', () => {
  const { context } = createHarness();
  context.initProgressBar('update', 'stdout', 'job-1');
  // 自己アップデート等が topic だけで作るバー。
  context.initProgressBar('update', 'stdout');
  assert.equal(barCount(context), 2, 'scope-less bar must not remove job bars');
});

test('progressbar.clear removes the bar for its scope', () => {
  const { context } = createHarness();
  context.setProgressBar(50, 5, 10, 'update', 'stdout', 'job-1');
  assert.equal(barCount(context), 1);
  context.removeProgressBar('update', 'stdout', 'job-1');
  assert.equal(barCount(context), 0);
});

test('terminal queue events clear the bar by job id', () => {
  const { context } = createHarness();
  context.initProgressBar('update', 'stdout', 'job-1');
  context.initProgressBar('convert', 'stdout2', 'job-2');
  context.removeProgressBarsForScope('job-1');
  assert.equal(barCount(context), 1);
  assert.ok(context.progressBars['stdout2:job-2'], 'other jobs keep their bar');

  assert.equal(context.queueEventJobId('job-9'), 'job-9');
  assert.equal(context.queueEventJobId({ job_id: 'job-9' }), 'job-9');
  assert.equal(context.queueEventJobId({ reason: 'x' }), '');
  assert.equal(context.queueEventJobId(null), '');
});

test('an idle queue drops job scoped bars but keeps non-job bars', () => {
  const { context } = createHarness();
  context.initProgressBar('update', 'stdout', 'job-1');
  context.initProgressBar('update', 'stdout2', 'job-2');
  // 自己アップデートのように job id を持たないバー (scope 無し) は残す。
  context.initProgressBar('update', 'stdout');

  context.clearProgressBarsWhenQueueIdle({ running_count: 1, pending: 0 });
  assert.equal(barCount(context), 3, 'running jobs keep their bars');

  context.clearProgressBarsWhenQueueIdle({ running_count: 0, pending: 2 });
  assert.equal(barCount(context), 3, 'pending jobs keep their bars');

  context.clearProgressBarsWhenQueueIdle({ running_count: 0, pending: 0 });
  assert.equal(barCount(context), 1, 'only the scope-less bar survives');
  assert.ok(context.progressBars['stdout:update']);
});

test('setProgressBar creates a missing bar from a step event', () => {
  const { context } = createHarness();
  context.setProgressBar(25, 1, 4, 'download', 'stdout', 'job-1');
  assert.equal(barCount(context), 1);
  const entry = context.progressBars['stdout:job-1'];
  assert.equal(entry.label.textContent, '進捗 1/4 25.0%');
  assert.equal(entry.bar.style.width, '25%');
});
