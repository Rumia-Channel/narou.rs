import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// Exercise production WebSocket handling against a small observable DOM.
const source = fs.readFileSync(
  process.env.NAROU_MAIN_SOURCE || new URL('../src/web/assets/js/main.js', import.meta.url),
  'utf8',
);
const start = source.indexOf('var progressBars = {};');
// The console renderer moved to `core/console_html.js`, so the block ends at
// the console helpers instead of the sanitizer.
const end = source.indexOf('function appendConsole(');
assert.ok(start >= 0, 'missing progressBars declaration');
assert.ok(end > start, 'missing progressBars block boundary');
const block = source.slice(start, end);
const handlerStart = source.indexOf('function handleWsMessage(');
const handlerEnd = source.indexOf('\nfunction ', handlerStart + 1);
const handler = source.slice(handlerStart, handlerEnd);

class FakeElement {
  constructor(className = '') {
    this.className = className;
    this.children = [];
    this.parentElement = null;
    this.textContent = '';
    this.style = {};
    this.attributes = new Map();
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 0;
    this.classList = {
      toggle: (name, force) => {
        const names = new Set(this.className.split(/\s+/));
        if (force ?? !names.has(name)) names.add(name);
        else names.delete(name);
        this.className = [...names].join(' ');
      },
    };
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
    return this.children.find((child) => child.className.split(/\s+/).includes(selector.slice(1))) || null;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  removeAttribute(name) {
    this.attributes.delete(name);
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
    scheduleConsoleStick: () => {},
    setConsolePinned: () => {},
    State: { concurrencyEnabled: true },
    refreshQueue: () => Promise.resolve(),
    refreshQueueDetailedIfOpen: () => {},
    refreshQueueDetailed: () => {},
    notifyQueueFailure: () => {},
    notifyQueuePartial: () => {},
    notifyQueueCancelled: () => {},
  });
  vm.runInContext(block + '\n' + handler, context);
  return {
    context,
    send: (type, data, target_console = 'stdout') => context.handleWsMessage({ type, data, target_console }),
    rows: (consoleName = 'stdout') => {
      const host = consoles[consoleName].parentElement.querySelector('.console-progress-host');
      return (host?.children || []).map((row) => ({
        label: row.children[0].textContent,
        width: row.children[1].children[0].style.width,
        attributes: row.children[1].attributes,
      }));
    },
  };
}

function progress(scope, operation, current, total, sequence, message = '') {
  return { topic: 'download', scope, operation, current, total, sequence, message };
}

test('interleaved jobs and operations retain independent visible progress', () => {
  const ui = createHarness();
  ui.send('progressbar.step', progress('A', 'a1', 1, 4, 1, 'Alpha'));
  ui.send('progressbar.step', progress('B', 'b1', 2, 4, 1, 'Beta'));
  ui.send('progressbar.step', progress('A', 'a2', 3, 6, 1, 'Gamma'));
  ui.send('progressbar.step', progress('A', 'a1', 2, 4, 2, 'Alpha'));
  assert.deepEqual(ui.rows().map((row) => row.width), ['50%', '50%', '50%']);
  ui.send('queue_complete', 'A');
  ui.send('progressbar.step', progress('A', 'a2', 4, 6, 99, 'Gamma'));
  assert.deepEqual(ui.rows().map((row) => row.width), ['50%']);
  assert.equal(ui.rows()[0].attributes.get('aria-label'), 'Beta');
});

test('late init, updates and clear cannot rewind or resurrect an operation', () => {
  const ui = createHarness();
  ui.send('progressbar.step', progress('A', 'a1', 3, 4, 5));
  ui.send('progressbar.init', progress('A', 'a1', 0, 0, 0));
  ui.send('progressbar.step', progress('A', 'a1', 1, 4, 4));
  ui.send('progressbar.clear', progress('A', 'a1', 0, 4, 3));
  assert.equal(ui.rows()[0].width, '75%');
  ui.send('progressbar.clear', progress('A', 'a1', 3, 4, 6));
  ui.send('progressbar.step', progress('A', 'a1', 4, 4, 7));
  assert.deepEqual(ui.rows(), []);
});

test('retry closes the old attempt while the next attempt can progress', () => {
  const ui = createHarness();
  ui.send('progressbar.step', progress('A', 'old', 2, 4, 4));
  ui.send('queue_retry', { job_id: 'A' });
  assert.deepEqual(ui.rows(), []);
  ui.send('queue_start', { job_id: 'A' });
  ui.send('progressbar.init', progress('A', 'new', 0, 0, 0));
  ui.send('progressbar.step', progress('A', 'old', 4, 4, 99));
  ui.send('progressbar.step', progress('A', 'new', 1, 5, 1));
  assert.deepEqual(ui.rows().map((row) => row.width), ['20%']);
});

test('unknown totals remain indeterminate until supplied and shrinking totals clamp', () => {
  const ui = createHarness();
  ui.send('progressbar.init', progress('A', 'a1', 0, 0, 0));
  assert.equal(ui.rows()[0].attributes.has('aria-valuenow'), false);
  ui.send('progressbar.step', progress('A', 'a1', 0, 4, 1));
  assert.equal(ui.rows()[0].width, '0%');
  assert.equal(ui.rows()[0].attributes.get('aria-valuenow'), '0');
  ui.send('progressbar.step', progress('A', 'a1', 3, 2, 2));
  assert.equal(ui.rows()[0].width, '100%');
  assert.equal(ui.rows()[0].attributes.get('aria-valuenow'), '100');
});

test('partial queue status cannot clear jobs and idle preserves manual progress', () => {
  const ui = createHarness();
  ui.send('progressbar.step', progress('A', 'a1', 1, 4, 1));
  ui.send('progressbar.step', { topic: 'self-update', percent: 40 }, 'stdout2');
  for (const status of [{}, { pending: 0 }, { running_count: 0 }, { running_count: 1, pending: 0 }]) {
    ui.context.clearProgressBarsWhenQueueIdle(status);
    assert.equal(ui.rows().length, 1);
  }
  ui.context.clearProgressBarsWhenQueueIdle({ running_count: 0, pending: 0 });
  assert.deepEqual(ui.rows(), []);
  assert.equal(ui.rows('stdout2')[0].width, '40%');
  // A stale idle snapshot does not permanently close a still-running operation.
  ui.send('progressbar.step', progress('A', 'a1', 2, 4, 2));
  assert.equal(ui.rows()[0].width, '50%');
});

test('a delayed idle response cannot remove progress received during the request', async () => {
  const ui = createHarness();
  let resolve;
  ui.context.refreshQueue = () => new Promise((done) => { resolve = done; });
  ui.context.State.queueStatus = { running_count: 0, pending: 0 };
  ui.send('progressbar.step', progress('A', 'a1', 1, 4, 1));
  ui.context.refreshQueueAndSyncProgressBars();
  ui.send('progressbar.step', progress('B', 'b1', 2, 4, 1));
  resolve();
  await Promise.resolve();
  assert.equal(ui.rows().length, 2);
});

test('scope clear affects only that job and reconnect accepts its current snapshot', () => {
  const ui = createHarness();
  ui.send('progressbar.step', progress('A', 'a1', 1, 4, 1));
  ui.send('progressbar.step', progress('B', 'b1', 2, 4, 1));
  ui.send('progressbar.step', progress('C', 'c1', 3, 4, 1), 'stdout2');
  ui.send('progressbar.clear', { scope: 'A' });
  assert.equal(ui.rows()[0].width, '50%');
  assert.equal(ui.rows('stdout2')[0].width, '75%');
  ui.context.clearAllProgressBars();
  ui.send('progressbar.step', progress('A', 'a1', 3, 4, 8));
  assert.equal(ui.rows()[0].width, '75%');
});

test('a reclaimed attempt replaces abandoned bars without removing peer jobs', () => {
  const ui = createHarness();
  ui.send('progressbar.step', progress('A', 'abandoned', 2, 4, 1));
  ui.send('progressbar.step', progress('B', 'peer', 3, 4, 1));
  ui.send('queue_start', 'A');
  ui.send('progressbar.step', progress('A', 'resumed', 1, 5, 1));
  ui.send('progressbar.step', progress('A', 'abandoned', 4, 4, 2));
  assert.deepEqual(ui.rows().map((row) => row.width), ['75%', '20%']);
});

test('failure and cancellation close only their job and reject late progress', () => {
  for (const terminal of ['queue_failed', 'queue_partial', 'queue_cancelled']) {
    const ui = createHarness();
    ui.send('progressbar.step', progress('A', 'a1', 2, 4, 1));
    ui.send('progressbar.step', progress('B', 'b1', 3, 4, 1));
    ui.send(terminal, { job_id: 'A' });
    ui.send('progressbar.step', progress('A', 'a1', 4, 4, 2));
    assert.deepEqual(ui.rows().map((row) => row.width), ['75%']);
  }
});
