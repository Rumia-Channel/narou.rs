// A same-origin, capability-addressed download relay. No archive is cached:
// each browser download pull requests exactly one <=64 KiB chunk from its tab.
const downloads = new Map();
self.addEventListener('install', (event) => event.waitUntil(self.skipWaiting()));
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));
self.addEventListener('message', (event) => {
  if (event.data?.type !== 'transfer-download' || !event.ports[0]) return;
  const { token } = event.data;
  if (typeof token !== 'string' || !/^[a-f0-9-]{36}$/.test(token)) return;
  const port = event.ports[0];
  let resolveLifetime;
  const lifetime = new Promise((resolve) => { resolveLifetime = resolve; });
  const state = { port, resolveLifetime, cancel: null };
  downloads.set(token, state);
  // Registration is acknowledged before navigating the hidden download frame.
  port.postMessage({ type: 'ready' });
  const timeout = setTimeout(() => {
    if (downloads.delete(token)) {
      port.postMessage({ type: 'error', message: 'ダウンロードが開始されませんでした。' });
      resolveLifetime(); port.close();
    }
  }, 60_000);
  state.timeout = timeout;
  port.onmessage = (message) => {
    if (message.data?.type === 'abort') {
      downloads.delete(token); clearTimeout(timeout);
      state.cancel?.(new Error(message.data.message || 'Download cancelled'));
      resolveLifetime(); port.close();
    }
  };
  event.waitUntil(lifetime);
});
self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  const prefix = new URL('./transfer-download/', self.location.href).pathname;
  if (url.origin !== self.location.origin || !url.pathname.startsWith(prefix)) return;
  const token = url.pathname.slice(prefix.length);
  const state = downloads.get(token);
  if (!state) { event.respondWith(new Response('Expired download', { status: 404 })); return; }
  downloads.delete(token); clearTimeout(state.timeout);
  let pending = null, controller;
  const finish = () => { state.resolveLifetime(); state.port.close(); };
  const abort = (reason) => {
    state.port.postMessage({ type: 'error', message: String(reason?.message || reason) });
    controller.error(reason);
    pending?.reject(reason); pending = null;
    finish();
  };
  state.cancel = abort;
  state.port.onmessage = (message) => {
    const data = message.data;
    if (data?.type === 'abort') { abort(new Error(data.message || 'Download cancelled')); return; }
    if (data?.type === 'end') {
      controller.close(); pending?.resolve(); pending = null;
      state.port.postMessage({ type: 'closed' }); finish(); return;
    }
    if (data?.type !== 'chunk' || !pending || !(data.bytes instanceof ArrayBuffer) || data.bytes.byteLength > 64 * 1024) {
      abort(new Error('Invalid download chunk')); return;
    }
    controller.enqueue(new Uint8Array(data.bytes));
    pending.resolve(); pending = null;
  };
  const body = new ReadableStream({
    start(value) { controller = value; },
    pull() {
      return new Promise((resolve, reject) => {
        pending = { resolve, reject };
        state.port.postMessage({ type: 'pull' });
      });
    },
    cancel(reason) {
      state.port.postMessage({ type: 'error', message: String(reason || 'ブラウザでダウンロードを中止しました。') });
      pending?.resolve(); pending = null; finish();
    },
  }, { highWaterMark: 0 });
  event.respondWith(new Response(body, { headers: {
    'Content-Type': 'application/zip',
    'Content-Disposition': 'attachment; filename="narou-export.zip"',
    'Cache-Control': 'no-store',
  } }));
});
