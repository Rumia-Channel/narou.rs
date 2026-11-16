// Capability-addressed, same-origin pull relay for locally generated ZIP bytes.
// Exactly one <=64 KiB chunk may be outstanding. Nothing is cached or fetched
// from the transfer API; `closed` acknowledges browser delivery, not disk durability.
const RELAY_VERSION = 3;
const downloads = new Map();
self.addEventListener('install', (event) => event.waitUntil(self.skipWaiting()));
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));
self.addEventListener('message', (event) => {
  if (event.data?.type === 'transfer-download-ping') {
    const { token, sequence, version } = event.data;
    if (typeof token !== 'string' || !/^[a-f0-9-]{36}$/.test(token) || !Number.isSafeInteger(sequence) || sequence <= 0) return;
    const state = downloads.get(token);
    event.source?.postMessage({ type: 'transfer-download-pong', version: RELAY_VERSION, token, sequence,
      active: version === RELAY_VERSION && !!state && !state.finished });
    return;
  }
  if (event.data?.type !== 'transfer-download' || !event.ports[0]) return;
  const port = event.ports[0], { token } = event.data;
  if (event.data.version !== RELAY_VERSION || typeof token !== 'string'
    || !/^[a-f0-9-]{36}$/.test(token) || downloads.has(token)) {
    port.postMessage({ type: 'error', message: 'ダウンロード中継の版・識別子が不正です。ページを再読み込みしてください。' });
    port.close(); return;
  }
  const name = typeof event.data.name === 'string' && event.data.name ? event.data.name : 'narou-export.zip';
  // Only bridge registration to its first fetch; an entire download may
  // exceed the browser's maximum ExtendableMessageEvent lifetime.
  let resolveStartup;
  const startup = new Promise((resolve) => { resolveStartup = resolve; });
  const state = { port, name, resolveStartup, finished: false, claimed: false };
  const finish = () => {
    if (state.finished) return;
    state.finished = true; downloads.delete(token); clearTimeout(state.timeout); resolveStartup(); port.close();
  };
  state.finish = finish;
  state.timeout = setTimeout(() => {
    if (!state.finished && downloads.has(token)) {
      port.postMessage({ type: 'error', message: 'ダウンロードが開始されませんでした。' }); finish();
    }
  }, 60_000);
  port.onmessage = ({ data }) => { if (data?.type === 'abort') finish(); };
  downloads.set(token, state); port.postMessage({ type: 'ready', version: RELAY_VERSION });
  event.waitUntil(startup);
});
self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  const prefix = new URL('./transfer-download/', self.location.href).pathname;
  if (url.origin !== self.location.origin || !url.pathname.startsWith(prefix)) return;
  if (event.request.method && event.request.method !== 'GET') {
    event.respondWith(new Response('Method not allowed', { status: 405 })); return;
  }
  const token = url.pathname.slice(prefix.length), state = downloads.get(token);
  if (!state || state.claimed) { event.respondWith(new Response('Expired download', { status: 404 })); return; }
  state.claimed = true; clearTimeout(state.timeout); state.resolveStartup();
  let pending = null, controller;
  const abort = (reason) => {
    if (state.finished) return;
    const error = reason instanceof Error ? reason : new Error(String(reason));
    state.port.postMessage({ type: 'error', message: error.message });
    try { controller.error(error); } catch { /* browser already cancelled */ }
    pending?.reject(error); pending = null; state.finish();
  };
  state.port.onmessageerror = () => abort(new Error('Invalid download message'));
  state.port.onmessage = ({ data }) => {
    if (state.finished) return;
    if (data?.type === 'abort') { abort(new Error(data.message || 'Download cancelled')); return; }
    if (data?.type === 'end') {
      controller.close(); pending?.resolve(); pending = null;
      state.port.postMessage({ type: 'closed' }); state.finish(); return;
    }
    if (data?.type !== 'chunk' || !pending || !(data.bytes instanceof ArrayBuffer)
      || !data.bytes.byteLength || data.bytes.byteLength > 64 * 1024) {
      abort(new Error('Invalid download chunk')); return;
    }
    controller.enqueue(new Uint8Array(data.bytes)); pending.resolve(); pending = null;
  };
  const body = new ReadableStream({
    start(value) { controller = value; },
    pull() {
      return new Promise((resolve, reject) => {
        pending = { resolve, reject }; state.port.postMessage({ type: 'pull' });
      });
    },
    cancel(reason) {
      if (state.finished) return;
      state.port.postMessage({ type: 'error', message: String(reason || 'ブラウザでダウンロードを中止しました。') });
      pending?.resolve(); pending = null; state.finish();
    },
  }, { highWaterMark: 0 });
  const filename = state.name.replace(/[^\w. ()\[\]-]/g, '_');
  event.respondWith(new Response(body, { headers: {
    'Content-Type': 'application/zip',
    'Content-Disposition': `attachment; filename="${filename}"`,
    'Cache-Control': 'no-store',
    'X-Content-Type-Options': 'nosniff',
  } }));
});
