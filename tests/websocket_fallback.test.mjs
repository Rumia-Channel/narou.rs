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

// 任意の時刻まで進められる偽のタイマー。
function fakeTimers() {
  const pending = new Map();
  let nextId = 1;
  return {
    setTimeoutImpl(fn, delay) {
      const id = nextId++;
      pending.set(id, { fn, delay });
      return id;
    },
    clearTimeoutImpl(id) {
      pending.delete(id);
    },
    // 登録済みのタイマーをすべて発火させる。
    fireAll() {
      const entries = [...pending.entries()];
      pending.clear();
      for (const [, timer] of entries) timer.fn();
    },
    pendingCount() {
      return pending.size;
    },
  };
}

// イベントを一切発火しない WebSocket (handshake が保留になる proxy の再現)。
function silentSocket() {
  return {
    onopen: null,
    onmessage: null,
    onerror: null,
    onclose: null,
    closed: false,
    close() {
      this.closed = true;
      if (this.onclose) this.onclose({ code: 1000 });
    },
  };
}

test('a WebSocket that never opens starts the fallback after the timeout', () => {
  const context = loadModule('websocket.js', { setTimeout, clearTimeout, console });
  const timers = fakeTimers();
  const events = [];
  const socket = silentSocket();

  const connection = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => socket,
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    connectTimeoutMs: 5000,
    onFallbackStart: () => events.push('fallback-start'),
    onFallbackStop: () => events.push('fallback-stop'),
    onOpen: () => events.push('open'),
    onClose: () => events.push('close'),
  });

  connection.connect();
  assert.deepEqual(events, [], '接続が保留の時点では切り替えない');
  assert.equal(timers.pendingCount(), 1, '接続のタイマーを仕掛ける');

  timers.fireAll();
  assert.deepEqual(events, ['fallback-start'], '時間制限で履歴取得へ切り替える');
  assert.equal(connection.isFallbackActive(), true);
});

test('the fallback stops when the socket finally opens', () => {
  const context = loadModule('websocket.js', { setTimeout, clearTimeout, console });
  const timers = fakeTimers();
  const events = [];
  const socket = silentSocket();

  const connection = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => socket,
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    connectTimeoutMs: 5000,
    onFallbackStart: () => events.push('fallback-start'),
    onFallbackStop: () => events.push('fallback-stop'),
    onOpen: () => events.push('open'),
  });

  connection.connect();
  timers.fireAll();
  socket.onopen();
  assert.deepEqual(events, ['fallback-start', 'fallback-stop', 'open']);
  assert.equal(connection.isFallbackActive(), false);
  assert.equal(timers.pendingCount(), 0, '開いたらタイマーを取り消す');
});

test('a socket that closes starts the fallback and a later open stops it', () => {
  const context = loadModule('websocket.js', { setTimeout, clearTimeout, console });
  const timers = fakeTimers();
  const events = [];
  const socket = silentSocket();

  const connection = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => socket,
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    connectTimeoutMs: 5000,
    onFallbackStart: () => events.push('fallback-start'),
    onFallbackStop: () => events.push('fallback-stop'),
    onClose: () => events.push('close'),
  });

  connection.connect();
  socket.onclose({ code: 1006 });
  assert.deepEqual(events, ['fallback-start', 'close']);
  assert.equal(connection.isFallbackActive(), true);
});

test('a constructor failure starts the fallback immediately', () => {
  const context = loadModule('websocket.js', { setTimeout, clearTimeout, console });
  const timers = fakeTimers();
  const events = [];

  const connection = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => {
      throw new Error('blocked');
    },
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    onFallbackStart: () => events.push('fallback-start'),
  });

  connection.connect();
  assert.deepEqual(events, ['fallback-start']);
  assert.equal(connection.isFallbackActive(), true);
});

test('the fallback is reported only once while it stays active', () => {
  const context = loadModule('websocket.js', { setTimeout, clearTimeout, console });
  const timers = fakeTimers();
  const events = [];
  const socket = silentSocket();

  const connection = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => socket,
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    connectTimeoutMs: 5000,
    onFallbackStart: () => events.push('fallback-start'),
    onFallbackStop: () => events.push('fallback-stop'),
  });

  connection.connect();
  timers.fireAll();
  socket.onclose({ code: 1006 });
  assert.deepEqual(events, ['fallback-start'], '二重に通知しない');
});

test('a replacement socket stops the fallback started by the previous socket', () => {
  const context = loadModule('websocket.js', { setTimeout, clearTimeout, console });
  const timers = fakeTimers();
  const events = [];

  // 接続 A: handshake が保留になり、履歴取得へ切り替わってから切断する。
  const socketA = silentSocket();
  const connectionA = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => socketA,
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    connectTimeoutMs: 5000,
    onFallbackStart: () => events.push('fallback-start'),
    onFallbackStop: () => events.push('fallback-stop'),
  });
  connectionA.connect();
  timers.fireAll();
  socketA.onclose({ code: 1006 });
  assert.deepEqual(events, ['fallback-start']);

  // 接続 B: 作り直された接続。開いた時点で履歴取得を止める。
  // 止めないと WebSocket と履歴取得が同時にコンソールへ追記する。
  const socketB = silentSocket();
  const connectionB = context.createResilientWebSocket({
    url: 'ws://127.0.0.1:8123/ws',
    createSocket: () => socketB,
    setTimeoutImpl: timers.setTimeoutImpl,
    clearTimeoutImpl: timers.clearTimeoutImpl,
    connectTimeoutMs: 5000,
    onFallbackStart: () => events.push('fallback-start'),
    onFallbackStop: () => events.push('fallback-stop'),
  });
  connectionB.connect();
  socketB.onopen();
  assert.deepEqual(events, ['fallback-start', 'fallback-stop'], '別の接続が始めた履歴取得も止める');
});
