import { showNotification } from './render.js';
import { confirmDialog, alertDialog } from './dialog.js';

/**
 * クライアント主体転送の UI 側。
 *
 * ZIP の生成・検証・選択・チャンク転送はすべて ESM Dedicated Worker
 * (js/ui/transfer-worker.js) + WASM で行う。このモジュールは DOM・
 * dialog・保存先 (File System Access / download Service Worker) だけを担当し、
 * バイトは transferable ArrayBuffer で受け渡す。サーバー製 ZIP・ZIP 全体の
 * POST・Base64 経路は存在しない。
 *
 * artifact manifest (/assets/transfer/manifest.json) と
 * GET /api/transfer/v2/capabilities が揃わない環境では、理由を表示して
 * メニューを無効化する (fail closed)。
 */

const PROTOCOL_VERSION = 2;
const ABI_VERSION = 2;
const MANIFEST_URL = '/assets/transfer/manifest.json';
const CAPABILITIES_URL = '/api/transfer/v2/capabilities';
const SW_RELAY_VERSION = 3;
const SW_CHUNK_BYTES = 64 * 1024;
const WORKER_BOOT_TIMEOUT_MS = 60_000;
const SW_HEARTBEAT_INTERVAL_MS = 15_000;
const SW_HEARTBEAT_TIMEOUT_MS = 90_000;

let activeTransfer = null;
let transferStarting = false;
let probePromise = null;
let workerHandle = null;

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function bytesText(value) {
  const bytes = typeof value === 'bigint' ? value : (typeof value === 'string' ? BigInt(value || '0') : BigInt(Math.trunc(value || 0)));
  const mib = 1024n * 1024n;
  if (bytes >= mib) return `${(Number(bytes) / Number(mib)).toFixed(1)} MiB`;
  if (bytes >= 1024n) return `${(Number(bytes) / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

const ROLE_LABELS = Object.freeze({
  body: '本文',
  toc: '目次',
  metadata: '作品情報',
  settings: '変換設定',
  illustration: '挿絵',
  illustration_index: '挿絵索引',
  raw: '取得時の raw データ',
  history: '旧本文・差分履歴',
  generated: '再生成可能な出力',
  unknown: '分類できないファイル',
  internal: '内部データ',
});
const ROLE_ORDER = ['body', 'toc', 'metadata', 'settings', 'illustration',
  'illustration_index', 'raw', 'history', 'generated', 'unknown', 'internal'];
function roleLabel(role) {
  return ROLE_LABELS[role] || `その他 (${String(role)})`;
}
function sortedRoles(roles) {
  return [...roles].sort((a, b) => {
    const ia = ROLE_ORDER.indexOf(a.role), ib = ROLE_ORDER.indexOf(b.role);
    return (ia === -1 ? ROLE_ORDER.length : ia) - (ib === -1 ? ROLE_ORDER.length : ib);
  });
}

// ---------------------------------------------------------------------------
// 環境 / artifact / capabilities の調査 (fail closed)
// ---------------------------------------------------------------------------

function environmentProblems() {
  const problems = [];
  if (!window.isSecureContext) problems.push('安全なコンテキスト (HTTPS/localhost) ではありません');
  if (typeof Worker !== 'function') problems.push('Web Worker に対応していません');
  if (typeof WebAssembly !== 'object') problems.push('WebAssembly に対応していません');
  if (typeof fetch !== 'function' || typeof crypto?.subtle?.digest !== 'function'
    || typeof crypto?.randomUUID !== 'function') problems.push('必要なブラウザ API (fetch/crypto) がありません');
  if (typeof File === 'undefined' || typeof File.prototype?.slice !== 'function'
    || typeof File.prototype?.arrayBuffer !== 'function') problems.push('File API に対応していません');
  return problems;
}

function sinkProblems() {
  if (typeof window.showSaveFilePicker === 'function') return [];
  if (navigator.serviceWorker && typeof ReadableStream === 'function') return [];
  return ['保存先として File System Access も Service Worker ダウンロードも使えません'];
}

async function fetchJsonOk(url) {
  const response = await fetch(url, { cache: 'no-store', credentials: 'same-origin', redirect: 'manual' });
  if (!response.ok || !(response.headers.get('content-type') || '').includes('json') || !response.body) {
    try { await response.body?.cancel(); } catch { /* closed */ }
    return null;
  }
  const reader = response.body.getReader(), buffer = new Uint8Array(256 * 1024);
  let used = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(buffer.subarray(0, used)));
      if (used + value.byteLength > buffer.byteLength) return null;
      buffer.set(value, used); used += value.byteLength;
    }
  } catch { return null; }
  finally { try { await reader.cancel(); } catch { /* closed */ } reader.releaseLock(); }
}

/**
 * 転送が使えるかを 1 度だけ調べる。失敗理由を返し、server ZIP へは戻らない。
 */
async function probeTransfer() {
  const problems = environmentProblems();
  const artifact = problems.length ? null : await fetchJsonOk(MANIFEST_URL);
  if (!problems.length && !artifact) {
    problems.push('転送 WASM の artifact manifest (/assets/transfer/manifest.json) がありません。配布物が転送対応ではありません');
  }
  if (!problems.length && (artifact.protocol_version !== PROTOCOL_VERSION || artifact.abi_version !== ABI_VERSION)) {
    problems.push(`転送 artifact の版 (protocol ${artifact.protocol_version}/abi ${artifact.abi_version}) がクライアントと一致しません。ページを再読み込みしてください`);
  }
  if (!problems.length && (!artifact.glue_url || !artifact.wasm_url || !artifact.worker_url || !artifact.build_id)) {
    problems.push('転送 artifact manifest に glue/wasm/worker の URL または build_id がありません');
  }
  let capabilities = null;
  if (!problems.length) {
    capabilities = await fetchJsonOk(CAPABILITIES_URL);
    if (!capabilities) {
      problems.push('サーバーが転送 API v2 (/api/transfer/v2) に対応していません');
    }
  }
  if (!problems.length && capabilities.protocol_version !== PROTOCOL_VERSION) {
    problems.push(`サーバーの転送 protocol_version ${capabilities.protocol_version} は ${PROTOCOL_VERSION} と一致しません`);
  }
  if (!problems.length && (!capabilities.build?.wasm_build_id
    || capabilities.build.wasm_build_id !== artifact.build_id
    || String(capabilities.build.abi_version) !== String(ABI_VERSION))) {
    problems.push('サーバーと配布 WASM の build/ABI が一致しません。ページを再読み込みしてください');
  }
  const storage = capabilities?.storage ?? {};
  const exportProblems = [...problems, ...sinkProblems()];
  return {
    problems,
    artifact: problems.length ? null : artifact,
    capabilities: problems.length ? null : capabilities,
    importOk: !problems.length && storage.import?.allowed !== false,
    importReason: storage.import?.reason || null,
    exportOk: !exportProblems.length && storage.export?.allowed !== false,
    exportReason: exportProblems.length ? exportProblems.join('、')
      : (storage.export?.reason || null),
  };
}

function ensureProbe() {
  if (!probePromise) probePromise = probeTransfer().catch(() => ({
    problems: ['転送環境の確認に失敗しました'],
    artifact: null, capabilities: null, importOk: false, exportOk: false,
  }));
  return probePromise;
}

function menuButton(id) { return document.getElementById(id); }

function decorateMenu(result) {
  const exportButton = menuButton('action-option-export');
  const importButton = menuButton('action-option-import');
  for (const [button, ok, reason] of [
    [exportButton, result.exportOk, result.exportReason || result.problems.join('、')],
    [importButton, result.importOk, result.importReason || result.problems.join('、')],
  ]) {
    if (!button) continue;
    if (ok) {
      button.classList.remove('disabled');
      button.removeAttribute('aria-disabled');
      button.title = '';
    } else {
      button.classList.add('disabled');
      button.setAttribute('aria-disabled', 'true');
      button.title = `小説データ転送を利用できません: ${reason || '環境が不足しています'}`;
    }
  }
}

// 読み込み時に一度だけ調べ、使えなければ理由付きでメニューを無効化する。
if (typeof document !== 'undefined') {
  queueMicrotask(() => {
    ensureProbe().then(decorateMenu).catch(() => {});
  });
}

// ---------------------------------------------------------------------------
// Worker 管理
// ---------------------------------------------------------------------------

function terminateWorker(reason = new Error('転送 Worker を停止しました。')) {
  const handle = workerHandle;
  workerHandle = null;
  if (!handle) return;
  handle.ready = false; handle.failure = reason;
  for (const pending of handle.commandResolvers.values()) pending.reject(reason);
  handle.commandResolvers.clear();
  handle.worker.terminate();
}

async function ensureWorker(artifact, capabilities) {
  if (workerHandle?.ready && workerHandle.buildId === artifact.build_id) return workerHandle;
  terminateWorker();
  const url = new URL(artifact.worker_url, location.href);
  if (url.origin !== location.origin) throw new Error('転送 Worker は同一 origin である必要があります。');
  const worker = new Worker(url.href, { type: 'module' });
  const handle = { worker, ready: false, buildId: artifact.build_id, commandResolvers: new Map(), onEvent: null, onRequest: null };
  workerHandle = handle;
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => fail(new Error('転送 Worker/WASM の起動がタイムアウトしました。')), WORKER_BOOT_TIMEOUT_MS);
    const fail = (error) => {
      clearTimeout(timeout); reject(error);
      handle.onEvent?.('fatal', { error });
      if (workerHandle === handle) terminateWorker(error);
    };
    worker.onerror = (event) => fail(new Error(event.message || '転送 Worker が停止しました。'));
    worker.onmessageerror = () => fail(new Error('転送 Worker のメッセージを受け取れません。'));
    worker.onmessage = ({ data }) => {
      if (!data || typeof data !== 'object') return;
      if (data.type === 'booted') worker.postMessage({ type: 'init', manifest: artifact, capabilities });
      else if (data.type === 'ready') {
        clearTimeout(timeout); handle.ready = true; handle.codec = data.codec; resolve();
      } else if (data.type === 'fatal') fail(workerError(data.error));
      else if (data.type === 'event') handle.onEvent?.(data.name, data.payload || {});
      else if (data.type === 'request') void handleUiRequest(handle, data);
      else if (data.type === 'result') {
        const pending = handle.commandResolvers.get(data.name);
        if (!pending) return;
        handle.commandResolvers.delete(data.name);
        if (data.ok) pending.resolve(data.value); else pending.reject(workerError(data.error));
      }
    };
  });
  return handle;
}

function workerError(data) {
  const error = new Error(data?.message || '転送処理に失敗しました。');
  error.code = data?.code || '';
  error.retryable = data?.retryable === true;
  error.stopState = data?.stopState || '';
  return error;
}

function sendCommand(handle, name, payload, transfer = []) {
  if (!handle.ready || handle.failure) return Promise.reject(handle.failure || new Error('転送 Worker が停止しています。'));
  if (handle.commandResolvers.has(name)) return Promise.reject(new Error('同じ転送操作が既に実行中です。'));
  return new Promise((resolve, reject) => {
    handle.commandResolvers.set(name, { resolve, reject });
    try { handle.worker.postMessage({ type: 'command', name, payload }, transfer); }
    catch (error) { handle.commandResolvers.delete(name); reject(error); }
  });
}

async function handleUiRequest(handle, data) {
  const respond = (ok, value) => {
    try { handle.worker.postMessage({ type: 'response', id: data.id, ok, ...(ok ? { value } : { error: value }) }); }
    catch { /* the worker may have been terminated while a dialog was open */ }
  };
  try {
    if (!handle.onRequest) throw new Error('転送 UI が要求を受け取れません。');
    const result = await handle.onRequest(data.name, data.payload ?? {});
    respond(true, result ?? null);
  } catch (error) {
    respond(false, { code: typeof error?.code === 'string' ? error.code
      : error?.name === 'QuotaExceededError' ? 'storage_quota' : error?.name === 'AbortError' ? 'aborted' : 'ui_rejected',
      message: String(error?.message || error) });
  }
}

// ---------------------------------------------------------------------------
// 進捗画面 (既存の modal 構造を再利用)
// ---------------------------------------------------------------------------

const PHASE_LABELS = Object.freeze({
  validate: 'ZIP の内容を検証しています…',
  catalog: '内容を分類しています…',
  manifest: '転送 manifest を作成しています…',
  manifest_upload: '転送 manifest を送信しています…',
  resume: '中断した転送を再開しています…',
  planning: 'サーバー側で計画を立てています…',
  files: '必要なファイルを確認しています…',
  upload: 'ファイルを送信しています…',
  ready: '公開の準備をしています…',
  publish: '公開しています…',
  select: '選択を確定しています…',
  prepare: 'サーバー側で snapshot を準備しています…',
  download: 'ZIP を作成・保存しています…',
  complete: '転送を完了しています…',
  retry: '通信を再試行しています…',
});

function progressScreen(title, importing = false) {
  if (activeTransfer) { activeTransfer.focus(); return null; }
  const previous = document.activeElement;
  const root = element('div', 'modal-overlay transfer-progress-overlay');
  root.id = 'transfer-progress'; root.setAttribute('role', 'dialog'); root.setAttribute('aria-modal', 'true');
  root.setAttribute('aria-labelledby', 'transfer-progress-title');
  const dialog = element('div', 'modal-dialog modal-dialog-sm');
  const content = element('div', 'modal-content');
  const header = element('div', 'modal-header');
  const heading = element('h4', 'modal-title', title); heading.id = 'transfer-progress-title'; header.append(heading);
  const body = element('div', 'modal-body');
  const status = element('p', 'transfer-progress-status', '準備しています…');
  status.id = 'transfer-progress-status'; status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite');
  const bar = element('progress', 'transfer-progress-bar'); bar.setAttribute('aria-label', title);
  const detail = element('p', 'transfer-progress-detail'); detail.id = 'transfer-progress-detail';
  const network = element('p', 'transfer-progress-detail');
  const confirmed = element('p', 'transfer-progress-detail');
  const output = element('p', 'transfer-progress-detail');
  const warning = element('p', 'transfer-progress-detail', '一時停止は確定済みデータを保持します。取消は未公開分だけを破棄し、公開済みの作品は残します。');
  body.append(status, bar, detail, network, confirmed, output, warning);
  const footer = element('div', 'modal-footer');
  const pause = element('button', 'btn btn-default', '一時停止'); pause.type = 'button'; pause.id = 'transfer-progress-close';
  const cancel = element('button', 'btn btn-danger', '未公開分を取り消す'); cancel.type = 'button';
  footer.append(pause, cancel); content.append(header, body, footer); dialog.append(content); root.append(dialog); document.body.append(root);
  let finished = false, handler = () => {}, currentPhase = '';
  const metrics = {};
  const close = () => {
    document.removeEventListener('keydown', keys, true); root.remove(); activeTransfer = null;
    if (previous?.isConnected) previous.focus();
  };
  const keys = (event) => {
    if (document.querySelector('.transfer-choice-overlay, #app-dialog:not(.hide)')) return;
    if (event.key === 'Escape') { event.preventDefault(); event.stopImmediatePropagation(); if (finished) close(); }
    if (event.key === 'Tab') {
      const buttons = [pause, cancel].filter((button) => !button.disabled && !button.hidden);
      if (!buttons.length) return;
      if ((event.shiftKey && document.activeElement === buttons[0])
        || (!event.shiftKey && document.activeElement === buttons.at(-1))) {
        event.preventDefault(); (event.shiftKey ? buttons.at(-1) : buttons[0]).focus();
      }
    }
  };
  document.addEventListener('keydown', keys, true);
  const stop = (action) => { pause.disabled = true; cancel.disabled = true; handler(action); };
  pause.addEventListener('click', () => finished ? close() : stop('pause'));
  cancel.addEventListener('click', async () => {
    if (await confirmDialog({ title: '転送を取り消す', message: '未公開の stage・snapshot を破棄します。公開済みの作品は取り消しません。', okLabel: '取り消す', danger: true })) {
      if (!finished) stop('cancel');
    }
  });
  const screen = {
    focus() { pause.focus(); },
    cancelWith(value) { handler = value; },
    phase(name, label) {
      currentPhase = name; status.textContent = label || PHASE_LABELS[name] || '処理しています…'; bar.removeAttribute('value');
    },
    event(name, payload) {
      if (name === 'fatal') { screen.done('転送 Worker が停止しました。再開情報は保持しています。', true); return; }
      if (name === 'phase') screen.phase(payload.phase, payload.label);
      if (name !== 'progress' && name !== 'phase') return;
      if (payload.phase && payload.phase !== currentPhase) screen.phase(payload.phase, payload.label);
      else if (payload.label) status.textContent = payload.label;
      for (const key of ['processed', 'produced', 'sent', 'received', 'confirmed', 'written', 'published']) {
        if (payload[key] != null) metrics[key] = BigInt(payload[key]);
      }
      if (payload.error?.message) detail.textContent = payload.error.message;
      else if (currentPhase === 'validate' && metrics.processed != null) detail.textContent = `ローカル全体検証: ${metrics.processed} / ${payload.total || '?'} ファイル`;
      else if (payload.produced != null) detail.textContent = `ローカル生成・展開: ${bytesText(metrics.produced)}`;
      if (metrics.sent != null || metrics.received != null) network.textContent = `通信: 送信 ${bytesText(metrics.sent || 0n)} / 受信 ${bytesText(metrics.received || 0n)}`;
      if (metrics.confirmed != null) confirmed.textContent = `サーバー確定: ${bytesText(metrics.confirmed)}${payload.state ? ` (${payload.state})` : ''}${metrics.published != null ? ` / 公開 ${metrics.published} 作品` : ''}`;
      if (metrics.written != null) output.textContent = `ZIP 出力済み: ${bytesText(metrics.written)}`;
      const total = BigInt(payload.total || '0');
      const amount = BigInt(payload.processed ?? payload.sent ?? payload.received ?? payload.produced ?? '0');
      if (total > 0n) { bar.max = 1; bar.value = Math.min(1, Number(amount) / Number(total)); }
    },
    done(message, error = false) {
      finished = true; status.textContent = message; status.setAttribute('role', error ? 'alert' : 'status');
      root.dataset.outcome = error ? 'error' : 'success';
      if (error) bar.removeAttribute('value'); else { bar.max = 1; bar.value = 1; }
      warning.textContent = error
        ? (importing ? '公開済みの作品は保持されます。再開時は同じ ZIP を選び、確定 receipt を照合します。' : '保存し直す場合は、同じ snapshot から新しい ZIP を生成します。途中ファイルへの追記はしません。')
        : '処理が完了しました。';
      pause.disabled = false; pause.textContent = '閉じる'; cancel.hidden = true; pause.focus();
    },
  };
  activeTransfer = screen; pause.focus(); return screen;
}

// ---------------------------------------------------------------------------
// 選択・確認 dialog (custom overlay: modal クラスを再利用)
// ---------------------------------------------------------------------------

function openTransferDialog({ title, build, read, okLabel = '開始', danger = false }) {
  const previous = document.activeElement;
  const progress = document.getElementById('transfer-progress');
  if (progress) progress.inert = true;
  const root = element('div', 'modal-overlay transfer-choice-overlay');
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-modal', 'true');
  const dialog = element('div', 'modal-dialog');
  const content = element('div', 'modal-content');
  const header = element('div', 'modal-header');
  const heading = element('h4', 'modal-title', title);
  heading.id = `transfer-dialog-${crypto.randomUUID()}`;
  root.setAttribute('aria-labelledby', heading.id);
  const closeButton = element('button', 'modal-close', '×');
  closeButton.type = 'button';
  closeButton.setAttribute('aria-label', 'キャンセル');
  header.append(heading, closeButton);
  const body = element('div', 'modal-body');
  const footer = element('div', 'modal-footer');
  const cancelButton = element('button', 'btn btn-default', 'キャンセル');
  cancelButton.type = 'button';
  const okButton = element('button', danger ? 'btn btn-danger' : 'btn btn-primary', okLabel);
  okButton.type = 'button';
  footer.append(cancelButton, okButton);
  content.append(header, body, footer);
  dialog.append(content);
  root.append(dialog);
  document.body.append(root);

  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      document.removeEventListener('keydown', keys, true);
      root.remove();
      if (progress) progress.inert = false;
      if (previous?.isConnected) previous.focus();
      resolve(value);
    };
    const keys = (event) => {
      if (document.querySelector('#app-dialog:not(.hide)')) return;
      if (event.key === 'Escape') {
        event.preventDefault(); event.stopImmediatePropagation();
        finish(null);
      } else if (event.key === 'Tab') {
        // focus を dialog 内に留める簡易トラップ。
        const focusables = [...dialog.querySelectorAll('button, input, select, [tabindex]')]
          .filter((node) => !node.disabled && !node.hidden && node.tabIndex !== -1);
        if (!focusables.length) return;
        const first = focusables[0], last = focusables[focusables.length - 1];
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault(); last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault(); first.focus();
        }
      }
    };
    document.addEventListener('keydown', keys, true);
    closeButton.addEventListener('click', () => finish(null));
    cancelButton.addEventListener('click', () => finish(null));
    root.addEventListener('click', (event) => { if (event.target === root) finish(null); });
    okButton.addEventListener('click', () => {
      const value = read ? read() : true;
      if (value !== null) finish(value);
    });
    try { build?.(body, { okButton, finish }); } catch { finish(null); }
    okButton.focus();
  });
}

/**
 * profile と追加 role を選ぶ dialog。
 * payload: { fileName?, roles, policy:{forbiddenRoles,reasons}, direction }
 * roles: [{role,label,requiredFiles,requiredBytes,optionalFiles,optionalBytes,
 *          optionalDefault,excludedFiles,excludedBytes,reasons[]}]
 * 戻り値: {confirm:true, profile, roles:[追加した role]} / null
 */
async function selectionDialog({ title, summary, byProfile, policy, note, importing = false, resumeSupported = true, availableBytes }) {
  const forbidden = new Set(policy?.forbiddenRoles || []);
  let profile = 'runtime', conflictPolicy = 'skip', acknowledge = false;
  const chosen = new Set();
  return openTransferDialog({
    title, okLabel: '確認して開始',
    build(body, { okButton }) {
      body.append(element('p', 'transfer-select-summary', summary));
      const profiles = element('div', 'transfer-profile');
      const name = `transfer-profile-${crypto.randomUUID()}`;
      for (const [value, label, detail] of [
        ['runtime', '通常移行 (既定)', '本文・目次・設定・挿絵・索引・不明なファイルを保持します。raw・履歴・確認済み生成物は既定で送りません。完全な backup ではありません。'],
        ['complete', '完全保全', '同梱可能な raw・履歴・生成物も保持します。保存先の制約で保持できないものは下に表示します。'],
      ]) {
        const row = element('label', 'transfer-profile-option'), input = element('input');
        input.type = 'radio'; input.name = name; input.value = value; input.checked = profile === value;
        input.addEventListener('change', () => { profile = value; render(); });
        row.append(input, element('strong', null, label), element('span', 'transfer-profile-detail', detail)); profiles.append(row);
      }
      body.append(profiles);
      if (importing) {
        const conflict = element('fieldset', 'transfer-profile');
        conflict.append(element('legend', 'transfer-role-detail', '登録済みの作品'));
        const name = `transfer-conflict-${crypto.randomUUID()}`;
        for (const [value, label] of [['skip', 'スキップする (既定)'], ['overwrite', 'アーカイブにある内容を上書きする']]) {
          const row = element('label', 'transfer-profile-option'), input = element('input');
          input.type = 'radio'; input.name = name; input.value = value; input.checked = value === 'skip';
          input.addEventListener('change', () => { conflictPolicy = value; }); row.append(input, element('span', null, label)); conflict.append(row);
        }
        body.append(conflict);
      }
      const list = element('div', 'transfer-role-list'), total = element('p', 'transfer-role-total');
      total.setAttribute('aria-live', 'polite');
      const acknowledgment = element('label', 'transfer-policy-ack');
      const ack = element('input'); ack.type = 'checkbox';
      ack.addEventListener('change', () => { acknowledge = ack.checked; update(); });
      acknowledgment.append(ack, element('span', null, '保存先が非対応の内容と欠落する用途を確認し、これらを送信・新規保存しないことに同意します。元の ZIP と元のライブラリは変更しません。'));
      body.append(list, total, acknowledgment);
      let blocked = false, policyFiles = 0;
      const buckets = () => byProfile?.[profile]?.roles || [];
      function update() {
        let sentFiles = 0, excludedFiles = 0, sentBytes = 0n, excludedBytes = 0n;
        blocked = false; policyFiles = 0;
        for (const bucket of buckets()) {
          const required = Number(bucket.requiredFiles || 0), optional = Number(bucket.optionalFiles || 0), excluded = Number(bucket.excludedFiles || 0);
          const requiredBytes = BigInt(bucket.requiredBytes || '0'), optionalBytes = BigInt(bucket.optionalBytes || '0'), excludedB = BigInt(bucket.excludedBytes || '0');
          const banned = forbidden.has(bucket.role);
          const selected = !banned && (bucket.optionalDefault > 0 || chosen.has(bucket.role));
          if (banned) {
            policyFiles += required + optional + (bucket.role === 'internal' ? 0 : excluded);
            blocked ||= required > 0;
          }
          sentFiles += (banned ? 0 : required) + (selected ? optional : 0);
          sentBytes += (banned ? 0n : requiredBytes) + (selected ? optionalBytes : 0n);
          excludedFiles += excluded + (banned ? required : 0) + (selected ? 0 : optional);
          excludedBytes += excludedB + (banned ? requiredBytes : 0n) + (selected ? 0n : optionalBytes);
        }
        total.textContent = `送る: ${sentFiles.toLocaleString()} 件・${bytesText(sentBytes)} / 送らない: ${excludedFiles.toLocaleString()} 件・${bytesText(excludedBytes)}。除外対象の実バイトは snapshot 作成も本文通信も行いません。`;
        acknowledgment.hidden = policyFiles === 0;
        okButton.disabled = blocked || (policyFiles > 0 && !acknowledge);
      }
      function render() {
        list.replaceChildren();
        for (const bucket of sortedRoles(buckets())) {
          const row = element('label', 'transfer-role-label'), box = element('input'), text = element('span', 'transfer-role-text');
          const required = Number(bucket.requiredFiles || 0), optional = Number(bucket.optionalFiles || 0), excluded = Number(bucket.excludedFiles || 0);
          const banned = forbidden.has(bucket.role);
          box.type = 'checkbox';
          box.disabled = banned || !optional || bucket.optionalDefault > 0;
          box.checked = !banned && (required > 0 || bucket.optionalDefault > 0 || (optional > 0 && chosen.has(bucket.role)));
          box.addEventListener('change', () => { if (box.checked) chosen.add(bucket.role); else chosen.delete(bucket.role); update(); });
          text.append(element('strong', null, bucket.label || roleLabel(bucket.role)),
            element('span', 'transfer-role-detail', `必須 ${required} 件・${bytesText(bucket.requiredBytes || '0')} / 選択可 ${optional} 件・${bytesText(bucket.optionalBytes || '0')} / 除外 ${excluded} 件・${bytesText(bucket.excludedBytes || '0')}`));
          if (required > 0) text.append(element('span', 'transfer-role-detail', banned ? '必須データを保存できないため開始できません。' : '必須のため外せません。'));
          if (bucket.role === 'raw') text.append(element('span', 'transfer-role-detail', 'raw を省くと Native の挿絵索引再構築・orphan 判定・mitemin 移行の保守材料が減ります。現在の本文・挿絵は保持します。'));
          if (banned && (required + optional + excluded > 0)) text.append(element('span', 'transfer-role-detail transfer-role-reason', bucket.role === 'raw' ? 'この保存先は raw HTML を新規保存しません。完全保全でも除外します。' : 'この保存先へは新規保存できません。'));
          for (const reason of bucket.reasons || []) text.append(element('span', 'transfer-role-detail', reason));
          row.append(box, text); list.append(row);
        }
        update();
      }
      render();
      if (availableBytes != null) body.append(element('p', 'transfer-select-note', `保存先の空き容量目安: ${bytesText(availableBytes)} (書込み成功の保証ではありません)。`));
      if (!resumeSupported) body.append(element('p', 'transfer-select-note transfer-role-reason', '永続的な再開情報を保存できません。このページを閉じた後の再開はできません。'));
      if (note) body.append(element('p', 'transfer-select-note', note));
      body.append(element('p', 'transfer-select-note', '分類できないファイルは既定で保持します。YAML という拡張子や「cache」という名前だけでは必要データを除外しません。'));
    },
    read() { return { confirm: true, profile, roles: [...chosen], conflictPolicy }; },
  });
}

/** plan (create/overwrite/skip/reject) の確認 dialog。 */
async function planConfirmDialog(summary) {
  const { counts, errors, samples, total, stale, message } = summary;
  const lines = [];
  if (message) lines.push(message);
  lines.push(`${total.toLocaleString()} 作品: 新規 ${counts.create} / 上書き ${counts.overwrite} / スキップ ${counts.skip}${counts.reject ? ` / 拒否 ${counts.reject}` : ''}${counts.other ? ` / その他 ${counts.other}` : ''}`);
  if (stale) lines.push('以前の plan は無効になりました。最新の内容です。');
  const result = await openTransferDialog({
    title: '公開内容の確認',
    okLabel: '公開する',
    danger: counts.overwrite > 0,
    build(body) {
      for (const line of lines) body.append(element('p', 'transfer-select-summary', line));
      if (errors?.length) {
        const box = element('div', 'transfer-role-list');
        for (const error of errors.slice(0, 20)) box.append(element('p', 'transfer-role-detail transfer-role-reason', error));
        body.append(element('p', 'transfer-select-note', '拒否・注意が必要な項目:'), box);
      }
      if (samples?.length) {
        const box = element('div', 'transfer-role-list');
        for (const sample of samples.slice(0, 20)) box.append(element('p', 'transfer-role-detail', sample));
        body.append(element('p', 'transfer-select-note', '対象の例 (先頭 20 件):'), box);
      }
      body.append(element('p', 'transfer-select-note',
        '公開すると既存の作品データが置き換わります。完了した作品は中断後も残り、失敗した作品の旧内容は保持されます。'));
    },
    read: () => true,
  });
  return result === true;
}

/** 中断セッションを再開/破棄する選択 dialog。 */
async function pendingSessionDialog(sessions, direction) {
  const matching = sessions.filter((session) => session.direction === direction);
  if (!matching.length) return { mode: 'new' };
  let selected = matching[0];
  return openTransferDialog({
    title: '中断した転送', okLabel: '再開・結果を確認',
    build(body, { okButton, finish }) {
      body.append(element('p', 'transfer-select-summary', '再開する転送を選んでください。新規開始は別の identity を作成します。'));
      const list = element('div', 'transfer-role-list'), group = `transfer-pending-${crypto.randomUUID()}`;
      const canResume = () => !selected.managed && (selected.resumeMode !== 'none'
        || ['completed', 'completed_with_errors', 'creation_unconfirmed', 'unknown'].includes(selected.state));
      for (const session of matching) {
        const row = element('label', 'transfer-profile-option'), input = element('input');
        input.type = 'radio'; input.name = group; input.checked = session === selected;
        input.addEventListener('change', () => { selected = session; okButton.disabled = !canResume(); });
        row.append(input, element('span', null, `${session.fileName || (direction === 'imports' ? '取り込み' : '書き出し')} (${session.state})`));
        if (session.error) row.append(element('span', 'transfer-role-detail', session.error));
        list.append(row);
      }
      body.append(list, element('p', 'transfer-select-note', direction === 'imports'
        ? '同じ ZIP を選び直して全体を再検証します。内容が異なる場合は停止し、勝手に新規転送へ変更しません。'
        : '同じ snapshot から新しい ZIP に保存し直します。途中の ZIP への追記は行いません。'));
      const actions = element('div', 'transfer-session-actions');
      const discard = element('button', 'btn btn-danger', '選択した転送を破棄'); discard.type = 'button';
      discard.addEventListener('click', async () => {
        if (await confirmDialog({ title: '転送を破棄', message: '未公開分を破棄します。公開済みの作品は保持します。', okLabel: '破棄する', danger: true })) {
          finish({ mode: 'discard', session: selected });
        }
      });
      const fresh = element('button', 'btn btn-default', '保持して新規開始'); fresh.type = 'button';
      fresh.addEventListener('click', () => finish({ mode: 'new' }));
      actions.append(discard, fresh); body.append(actions); okButton.disabled = !canResume();
    },
    read: () => ({ mode: 'resume', session: selected }),
  });
}

// ---------------------------------------------------------------------------
// 保存先 (FSA / download SW)
// ---------------------------------------------------------------------------

async function downloadSink(controller) {
  const { signal } = controller;
  if (typeof window.showSaveFilePicker === 'function') {
    // This invocation must remain before the first await in the click handler.
    const handle = await window.showSaveFilePicker({ suggestedName: 'narou-export.zip',
      types: [{ description: 'ZIP archive', accept: { 'application/zip': ['.zip'] } }] });
    if (signal.aborted) throw new DOMException('Aborted', 'AbortError');
    const writable = await handle.createWritable();
    let closed = false;
    const abort = async () => {
      if (closed) return;
      closed = true; signal.removeEventListener('abort', cancelled);
      await writable.abort();
    };
    const cancelled = () => { void abort().catch(() => {}); };
    signal.addEventListener('abort', cancelled, { once: true });
    if (signal.aborted) { await abort(); throw new DOMException('Aborted', 'AbortError'); }
    return {
      mode: 'fsa',
      async write(bytes) {
        if (closed || signal.aborted) throw new DOMException('Aborted', 'AbortError');
        const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
        if (view.byteLength > SW_CHUNK_BYTES) throw new Error('保存 chunk が64 KiBを超えました。');
        await writable.write(view);
      },
      async close() {
        if (closed || signal.aborted) throw new DOMException('Aborted', 'AbortError');
        await writable.close(); closed = true; signal.removeEventListener('abort', cancelled);
      },
      abort,
    };
  }
  if (!navigator.serviceWorker) throw new Error('File System Access / Service Worker 保存に対応していません。');
  // A confirmation dialog may outlive a Service Worker. Do not start its
  // download response until the producer has the first actual ZIP bytes.
  let output = null, stopped = false;
  const start = () => {
    if (stopped || signal.aborted) throw new DOMException('Aborted', 'AbortError');
    return output ||= serviceWorkerDownloadSink(controller);
  };
  return {
    mode: 'sw',
    async write(bytes) { await (await start()).write(bytes); },
    async close() { await (await start()).close(); stopped = true; },
    async abort(reason) { if (stopped) return; stopped = true; if (output) await (await output).abort(reason); },
  };
}

async function serviceWorkerDownloadSink(controller) {
  const { signal } = controller;
  const script = new URL('./transfer-download-sw.js', import.meta.url);
  script.search = '';
  const registration = await navigator.serviceWorker.register(script.href,
    { scope: new URL('./', script).pathname, updateViaCache: 'none' });
  await registration.update();
  let worker = registration.installing || registration.waiting || registration.active;
  if (!worker) throw new Error('ダウンロード中継の Service Worker を起動できません。');
  if (worker.state !== 'activated') {
    await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => finish(new Error('Service Worker の更新がタイムアウトしました。')), WORKER_BOOT_TIMEOUT_MS);
      const finish = (error) => { clearTimeout(timeout); worker.removeEventListener('statechange', changed); error ? reject(error) : resolve(); };
      const changed = () => {
        if (worker.state === 'activated') finish();
        else if (worker.state === 'redundant') finish(new Error('Service Worker の更新に失敗しました。'));
      };
      worker.addEventListener('statechange', changed); changed();
    });
  }
  if (signal.aborted) throw new DOMException('Aborted', 'AbortError');
  const channel = new MessageChannel(), token = crypto.randomUUID();
  const frame = document.createElement('iframe'); frame.hidden = true;
  let demand = false, failure = null, disposed = false, closed = false, readyResolve, readyReject, writeResolve, writeReject, closeResolve, closeReject;
  let heartbeatTimer = null, heartbeatSequence = 0, heartbeatAcknowledged = 0, lastActivity = Date.now();
  const ready = new Promise((resolve, reject) => { readyResolve = resolve; readyReject = reject; });
  const cleanup = () => {
    clearTimeout(handshakeTimeout); clearInterval(heartbeatTimer);
    navigator.serviceWorker.removeEventListener('message', receiveHeartbeat);
    signal.removeEventListener('abort', cancelled); frame.remove(); channel.port1.close();
  };
  const fail = (error) => {
    if (disposed) return;
    disposed = true; failure = error;
    try { channel.port1.postMessage({ type: 'abort', message: error.message }); } catch { /* closed */ }
    readyReject(error); writeReject?.(error); closeReject?.(error); cleanup();
    if (!signal.aborted) controller.abort();
  };
  const receiveHeartbeat = (event) => {
    const data = event.data;
    if (disposed || closeResolve || event.source !== worker || data?.type !== 'transfer-download-pong' || data.token !== token
      || !Number.isSafeInteger(data.sequence) || data.sequence <= heartbeatAcknowledged || data.sequence > heartbeatSequence) return;
    heartbeatAcknowledged = data.sequence;
    if (data.version !== SW_RELAY_VERSION || data.active !== true) {
      fail(new Error('ブラウザのダウンロード中継の状態が失われました。同じ snapshot から保存し直してください。')); return;
    }
    lastActivity = Date.now();
  };
  const heartbeat = () => {
    if (disposed) return;
    if (Date.now() - lastActivity >= SW_HEARTBEAT_TIMEOUT_MS) {
      fail(new Error('ブラウザのダウンロード中継との接続が切れました。同じ snapshot から保存し直してください。')); return;
    }
    try { worker.postMessage({ type: 'transfer-download-ping', version: SW_RELAY_VERSION, token, sequence: ++heartbeatSequence }); }
    catch (error) { fail(error); }
  };
  const cancelled = () => fail(new DOMException('Aborted', 'AbortError'));
  const handshakeTimeout = setTimeout(() => fail(new Error('ダウンロード中継の版確認がタイムアウトしました。')), WORKER_BOOT_TIMEOUT_MS);
  signal.addEventListener('abort', cancelled, { once: true });
  navigator.serviceWorker.addEventListener('message', receiveHeartbeat);
  channel.port1.onmessageerror = () => fail(new Error('ダウンロード中継の通信に失敗しました。'));
  channel.port1.onmessage = ({ data }) => {
    if (disposed) return;
    if (data.type === 'ready') {
      if (data.version !== SW_RELAY_VERSION) { fail(new Error('ダウンロード中継の版が一致しません。ページを再読み込みしてください。')); return; }
      clearTimeout(handshakeTimeout); readyResolve();
      lastActivity = Date.now();
    } else if (data.type === 'pull') {
      if (demand) { fail(new Error('ダウンロード中継から重複した pull が届きました。')); return; }
      lastActivity = Date.now();
      demand = true; writeResolve?.(); writeResolve = null; writeReject = null;
    } else if (data.type === 'closed') {
      if (!closeResolve) { fail(new Error('終了要求の前にダウンロードが閉じました。')); return; }
      closed = true; disposed = true; closeResolve(); cleanup();
    } else if (data.type === 'error') fail(new Error(data.message || 'ダウンロードが中止されました。'));
  };
  worker.postMessage({ type: 'transfer-download', token, version: SW_RELAY_VERSION, name: 'narou-export.zip' }, [channel.port2]);
  await ready;
  if (failure || signal.aborted) throw failure || new DOMException('Aborted', 'AbortError');
  heartbeatTimer = setInterval(heartbeat, SW_HEARTBEAT_INTERVAL_MS); heartbeat();
  if (failure || signal.aborted) throw failure || new DOMException('Aborted', 'AbortError');
  frame.src = new URL(`./transfer-download/${token}`, script).href; document.body.append(frame);
  return {
    mode: 'sw',
    async write(bytes) {
      if (failure) throw failure;
      if (closed || disposed || writeResolve) throw new Error('保存先への並行・終了後の書込みです。');
      const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      if (!view.byteLength || view.byteLength > SW_CHUNK_BYTES) throw new Error('保存 chunk の長さが不正です。');
      if (!demand) await new Promise((resolve, reject) => { writeResolve = resolve; writeReject = reject; });
      if (failure) throw failure;
      demand = false;
      const owned = view.byteOffset === 0 && view.byteLength === view.buffer.byteLength ? view : view.slice();
      channel.port1.postMessage({ type: 'chunk', bytes: owned.buffer }, [owned.buffer]);
    },
    async close() {
      if (failure) throw failure;
      if (closed) return;
      if (writeResolve) throw new Error('保存先への書込みが完了していません。');
      clearInterval(heartbeatTimer);
      const timer = setTimeout(() => fail(new Error('ダウンロード中継の終了確認がタイムアウトしました。')), WORKER_BOOT_TIMEOUT_MS);
      try {
        await new Promise((resolve, reject) => {
          closeResolve = resolve; closeReject = reject; channel.port1.postMessage({ type: 'end' });
        });
      } finally { clearTimeout(timer); }
    },
    async abort(reason) { if (!closed) fail(new Error(String(reason || 'Download cancelled'))); },
  };
}

// ---------------------------------------------------------------------------
// export / import エントリポイント
// ---------------------------------------------------------------------------

/** 中断 session の確認と再開/新規の選択。戻り値: {mode:'new'} | {mode:'resume',session} | null(中止) */
async function choosePending(handle, direction) {
  for (;;) {
    const sessions = await sendCommand(handle, 'pending');
    const choice = await pendingSessionDialog(sessions || [], direction);
    if (choice?.mode !== 'discard') return choice;
    await sendCommand(handle, 'discard', { requestId: choice.session.requestId, sessionId: choice.session.sessionId,
      managed: choice.session.managed === true, direction: choice.session.direction, revision: choice.session.revision });
  }
}

function transferRequests(screen, sink) {
  return async (name, payload) => {
    switch (name) {
      case 'sink-write':
        if (!sink || !(payload.bytes instanceof ArrayBuffer)) throw new Error('保存データの形式が不正です。');
        await sink.write(new Uint8Array(payload.bytes)); return null;
      case 'sink-close':
        if (!sink) throw new Error('保存先がありません。');
        await sink.close(); return null;
      case 'sink-abort':
        await sink?.abort('前回の完了 receipt を確認しました。'); return null;
      case 'import-select':
      case 'export-select':
        return selectionDialog({
          title: name === 'import-select' ? '取り込む内容の選択' : '書き出す内容の選択',
          summary: name === 'import-select'
            ? `${payload.fileName} (${bytesText(payload.fileSize || '0')}) / ${payload.novels} 作品・${payload.files} ファイル・展開後 ${bytesText(payload.uncompressed || '0')}`
            : `${payload.ids.length} 作品 / ${payload.totalFiles} ファイル・${bytesText(payload.totalBytes || '0')}`,
          byProfile: payload.byProfile, policy: payload.policy, importing: name === 'import-select',
          resumeSupported: payload.resumeSupported, availableBytes: payload.availableBytes,
          note: name === 'import-select' ? 'ZIP 全体のローカル検証は完了しています。この確認前には本文 PUT・公開を一度も行っていません。'
            : 'まだ snapshot は作成していません。確認したファイルだけを snapshot・読出しの対象にします。',
        });
      case 'plan-confirm': return planConfirmDialog(payload);
      case 'auth': return confirmDialog({ title: '転送を待機して再認証',
        message: payload.message || '別のタブでログインし直してください。同じ session identity を維持します。', okLabel: '再認証して再試行' });
      case 'notice': await alertDialog({ title: '転送', message: payload.message || '' }); return null;
      default: throw new Error(`不明な UI 要求 ${name}`);
    }
  };
}

function stoppedMessage(error, action) {
  if (error.stopState === 'cancelled') return '未公開分の取消が確定しました。公開済みの作品は保持しています。';
  if (error.stopState === 'paused') return '転送を一時停止しました。再開時に確定 receipt を照合します。';
  if (action === 'cancel') return `取消の結果はまだ確定できません。再開情報を保持しています。\n${error.message}`;
  return `転送を停止しました。再開情報を保持しています。\n${error.message}`;
}

export async function exportSelectedNovels(ids) {
  if (activeTransfer || transferStarting) { activeTransfer?.focus(); return; }
  if (!ids.length) { showNotification('書き出す作品を選択してください', 'error'); return; }
  transferStarting = true;
  const sinkController = new AbortController();
  // Attach a rejection handler immediately: the picker can be cancelled while probing.
  const saving = downloadSink(sinkController).then((sink) => ({ sink }), (error) => ({ error }));
  let sink, handle, screen, stopAction = 'pause', stopped = false;
  const interrupt = () => {
    if (stopped) return;
    stopped = true;
    try { handle?.worker.postMessage({ type: 'command', name: 'abort', payload: { action: stopAction } }); } catch { /* worker already failed */ }
  };
  try {
    const probe = await ensureProbe();
    const selected = await saving;
    if (selected.error) throw selected.error;
    sink = selected.sink;
    if (!probe.exportOk) throw new Error(probe.exportReason || probe.problems.join('、'));
    screen = progressScreen('小説データの書き出し'); transferStarting = false;
    if (!screen) return;
    screen.cancelWith((action) => { stopAction = action; interrupt(); sinkController.abort(); });
    handle = await ensureWorker(probe.artifact, probe.capabilities);
    if (stopped || sinkController.signal.aborted) throw new DOMException('Aborted', 'AbortError');
    sinkController.signal.addEventListener('abort', interrupt, { once: true });
    handle.onEvent = (name, payload) => screen.event(name, payload);
    handle.onRequest = transferRequests(screen, sink);
    const pending = await choosePending(handle, 'exports');
    if (!pending || stopped) throw new DOMException('Aborted', 'AbortError');
    const result = await sendCommand(handle, pending.mode === 'resume' ? 'resume-export' : 'export',
      pending.mode === 'resume' ? { requestId: pending.session.requestId } : { ids });
    screen.done(result.alreadyCompleted
      ? '前回の転送は完了 receipt が確定しています。前回の保存先を確認してください。'
      : sink.mode === 'sw'
        ? `転送完了: ${bytesText(result.bytes)} をブラウザのダウンロード管理へ渡しました。保存結果はブラウザで確認してください。`
        : `ZIP の保存が完了しました。${bytesText(result.bytes)} を書き出しました。`);
  } catch (error) {
    if (screen) screen.done(stopped || error.name === 'AbortError' || error.code === 'aborted'
      ? stoppedMessage(error, stopAction) : `書き出しに失敗しました。\n${error.message}`, true);
    else if (error.name !== 'AbortError') showNotification(`転送を開始できません。\n${error.message}`, 'error');
  } finally {
    transferStarting = false;
    sinkController.signal.removeEventListener('abort', interrupt);
    if (handle) { handle.onEvent = null; handle.onRequest = null; }
    try { await sink?.abort('Transfer finished'); } catch { /* an earlier error was already shown */ }
    sinkController.abort();
  }
}

export async function importNovelArchive(file, onImported) {
  if (!file || transferStarting) return;
  if (activeTransfer) { activeTransfer.focus(); return; }
  transferStarting = true;
  let handle, screen, stopped = false, stopAction = 'pause';
  try {
    const probe = await ensureProbe();
    if (!probe.importOk) throw new Error(probe.importReason || probe.problems.join('、'));
    screen = progressScreen('小説データの取り込み', true); transferStarting = false;
    if (!screen) return;
    screen.cancelWith((action) => {
      stopAction = action; stopped = true;
      try { handle?.worker.postMessage({ type: 'command', name: 'abort', payload: { action } }); } catch { /* already failed */ }
    });
    handle = await ensureWorker(probe.artifact, probe.capabilities);
    if (stopped) throw new DOMException('Aborted', 'AbortError');
    handle.onEvent = (name, payload) => screen.event(name, payload);
    handle.onRequest = transferRequests(screen);
    const pending = await choosePending(handle, 'imports');
    if (!pending || stopped) throw new DOMException('Aborted', 'AbortError');
    const result = await sendCommand(handle, pending.mode === 'resume' ? 'resume-import' : 'import',
      { file, ...(pending.mode === 'resume' ? { requestId: pending.session.requestId } : {}) });
    const counts = result.counts || {}, errors = result.errors || [];
    const summary = `新規 ${counts.create || 0} / 上書き ${counts.overwrite || 0} / スキップ ${counts.skip || 0} / 拒否 ${counts.reject || 0} / 失敗 ${counts.failed || 0}`;
    screen.done(result.state === 'completed' ? `取り込みが完了しました。${summary}`
      : `一部の取り込みに失敗しました。${summary}\n${errors.slice(0, 10).join('\n')}`, result.state !== 'completed');
    if (onImported) {
      try { await onImported(); }
      catch (error) { showNotification(`取り込み結果は確定しましたが、一覧を更新できませんでした。\n${error.message}`, 'error'); }
    }
  } catch (error) {
    if (screen) screen.done(stopped || error.name === 'AbortError' || error.code === 'aborted'
      ? stoppedMessage(error, stopAction) : `取り込みに失敗しました。\n${error.message}`, true);
    else showNotification(`転送を開始できません。\n${error.message}`, 'error');
  } finally {
    transferStarting = false;
    if (handle) { handle.onEvent = null; handle.onRequest = null; }
  }
}
