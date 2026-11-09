import { showNotification } from './render.js';

let activeTransfer = null;
const MAX_ARCHIVE_BYTES = 512 * 1024 * 1024;
const MAX_FRAME_CHARS = 256 * 1024;

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function bytesText(bytes) {
  return bytes >= 1024 * 1024 ? `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
    : `${(bytes / 1024).toFixed(1)} KiB`;
}

function progressScreen(title, importing = false) {
  if (activeTransfer) {
    activeTransfer.focus();
    return null;
  }
  const previous = document.activeElement;
  const root = element('div', 'modal-overlay transfer-progress-overlay');
  root.id = 'transfer-progress';
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-modal', 'true');
  root.setAttribute('aria-labelledby', 'transfer-progress-title');
  const dialog = element('div', 'modal-dialog modal-dialog-sm');
  const content = element('div', 'modal-content');
  const header = element('div', 'modal-header');
  const heading = element('h4', 'modal-title', title);
  heading.id = 'transfer-progress-title';
  header.append(heading);
  const body = element('div', 'modal-body');
  const status = element('p', 'transfer-progress-status', '準備しています…');
  status.id = 'transfer-progress-status';
  status.setAttribute('role', 'status');
  status.setAttribute('aria-live', 'polite');
  const bar = element('progress', 'transfer-progress-bar');
  bar.setAttribute('aria-label', title);
  const detail = element('p', 'transfer-progress-detail');
  detail.id = 'transfer-progress-detail';
  const received = element('p', 'transfer-progress-detail');
  const warning = element('p', 'transfer-progress-detail', 'この画面を開いたままお待ちください。');
  body.append(status, bar, detail, received, warning);
  const footer = element('div', 'modal-footer');
  const button = element('button', 'btn btn-default', '通信を中止');
  button.type = 'button';
  button.id = 'transfer-progress-close';
  footer.append(button);
  content.append(header, body, footer);
  dialog.append(content);
  root.append(dialog);
  document.body.append(root);
  let finished = false;
  let cancel = () => {};
  const close = () => {
    document.removeEventListener('keydown', keys, true);
    root.remove();
    activeTransfer = null;
    if (previous?.isConnected) previous.focus();
  };
  const keys = (event) => {
    if (event.key === 'Escape') {
      event.preventDefault(); event.stopImmediatePropagation();
      if (finished) close();
    } else if (event.key === 'Tab') {
      event.preventDefault(); button.focus();
    }
  };
  document.addEventListener('keydown', keys, true);
  button.addEventListener('click', () => {
    if (finished) close();
    else {
      button.disabled = true;
      cancel();
    }
  });
  const screen = {
    focus() { button.focus(); },
    cancelWith(handler) { cancel = handler; },
    phase(text) { status.textContent = text; bar.removeAttribute('value'); },
    update(frame) {
      const total = Number(frame.total), completed = Number(frame.completed);
      status.textContent = frame.phase === 'import' ? '小説データを取り込んでいます…' : 'ZIP を作成・受信しています…';
      if (frame.phase === 'validate') {
        status.textContent = 'ZIP の内容を検証しています…';
        bar.removeAttribute('value');
        return;
      }
      if (Number.isFinite(total) && total > 0 && Number.isFinite(completed)) {
        bar.max = total; bar.value = completed;
        detail.textContent = `${completed.toLocaleString()} / ${total.toLocaleString()} 作品${frame.title ? ` — ${frame.title}` : ''}`;
      }
    },
    bytes(count, total, upload = false) {
      received.textContent = `${upload ? '送信済み' : '受信済み'}: ${bytesText(count)}${total ? ` / ${bytesText(total)}` : ''}`;
      if (total) { bar.max = total; bar.value = count; }
    },
    done(message, error = false) {
      finished = true;
      status.textContent = message;
      status.setAttribute('role', error ? 'alert' : 'status');
      root.dataset.outcome = error ? 'error' : 'success';
      if (error) bar.removeAttribute('value');
      else { bar.max = 1; bar.value = 1; }
      warning.textContent = error
        ? (importing ? '途中で取り込みが完了した作品がある場合、その作品は保持されます。'
          : 'ブラウザのダウンロード一覧と、表示されたエラーをご確認ください。')
        : '処理が完了しました。';
      button.disabled = false;
      button.textContent = '閉じる';
      button.focus();
    },
  };
  activeTransfer = screen;
  button.focus();
  return screen;
}

function failureMessage(response, body) {
  let message;
  try {
    const value = JSON.parse(body);
    message = value.message || value.errors?.join('\n') || value.error?.message || value.error;
  } catch { /* A proxy may return HTML instead of an application error. */ }
  return typeof message === 'string' && message
    ? `HTTP ${response.status}: ${message.slice(0, 2000)}`
    : `転送に失敗しました (HTTP ${response.status})。サーバーまたは接続先の制限・通信状態を確認してください。`;
}

async function consumeEvents(response, onEvent) {
  if (!response.ok) throw new Error(failureMessage(response, await response.text()));
  if (!response.headers.get('content-type')?.includes('application/x-ndjson')) {
    throw new Error('サーバーが進捗付き転送に対応していません。サーバーを更新してから再度お試しください。');
  }
  if (!response.body) throw new Error('転送データを受信できませんでした。');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = '', complete = null;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      pending += decoder.decode(value, { stream: !done });
      let newline;
      while ((newline = pending.indexOf('\n')) !== -1) {
        const line = pending.slice(0, newline); pending = pending.slice(newline + 1);
        if (line.length > MAX_FRAME_CHARS) throw new Error('転送データの応答が大きすぎます。');
        if (!line.trim()) continue;
        const frame = JSON.parse(line);
        if (frame.type === 'error') throw new Error(frame.message || 'サーバーの転送処理に失敗しました。');
        if (complete) throw new Error('完了後に不正な転送データを受信しました。');
        if (frame.type === 'complete') complete = frame;
        await onEvent(frame);
      }
      if (pending.length > MAX_FRAME_CHARS) throw new Error('転送データの応答が大きすぎます。');
      if (done) break;
    }
    if (pending.trim() || !complete) throw new Error('転送が完了する前に接続が切れました。保存・取り込みの結果を確認してください。');
    return complete;
  } finally {
    try { await reader.cancel(); } catch { /* The connection may already be closed. */ }
    reader.releaseLock();
  }
}

async function downloadSink(controller) {
  const { signal } = controller;
  if (typeof window.showSaveFilePicker === 'function') {
    const handle = await window.showSaveFilePicker({
      suggestedName: 'narou-export.zip',
      types: [{ description: 'ZIP archive', accept: { 'application/zip': ['.zip'] } }],
    });
    return handle.createWritable();
  }
  // Firefox has no File System Access API. A scoped service worker hands the
  // stream to the browser's download manager, with one pull/chunk in flight.
  const script = new URL('./transfer-download-sw.js', import.meta.url);
  const registration = await navigator.serviceWorker.register(script, { scope: new URL('./', script).pathname });
  let worker = registration.active;
  if (!worker) {
    worker = registration.installing || registration.waiting;
    await new Promise((resolve, reject) => {
      const changed = () => {
        if (worker.state === 'activated') { worker.removeEventListener('statechange', changed); resolve(); }
        else if (worker.state === 'redundant') { worker.removeEventListener('statechange', changed); reject(new Error('ダウンロードの準備に失敗しました。')); }
      };
      worker.addEventListener('statechange', changed); changed();
    });
  }
  if (signal.aborted) throw new DOMException('Aborted', 'AbortError');
  const channel = new MessageChannel();
  const token = crypto.randomUUID();
  let demand = false, failure, readyResolve, readyReject, writeResolve, writeReject, closeResolve, closeReject;
  const ready = new Promise((resolve, reject) => { readyResolve = resolve; readyReject = reject; });
  const frame = document.createElement('iframe');
  frame.hidden = true;
  const dispose = () => { signal.removeEventListener('abort', cancelled); frame.remove(); channel.port1.close(); };
  const fail = (error) => {
    failure = error; readyReject(error); writeReject?.(error); closeReject?.(error); dispose();
    controller.abort();
  };
  const cancelled = () => {
    channel.port1.postMessage({ type: 'abort', message: 'Download cancelled' });
    fail(new DOMException('Aborted', 'AbortError'));
  };
  signal.addEventListener('abort', cancelled, { once: true });
  channel.port1.onmessage = ({ data }) => {
    if (data.type === 'ready') readyResolve();
    else if (data.type === 'pull') { demand = true; writeResolve?.(); writeResolve = null; writeReject = null; }
    else if (data.type === 'closed') { closeResolve?.(); dispose(); }
    else if (data.type === 'error') fail(new Error(data.message));
  };
  worker.postMessage({ type: 'transfer-download', token }, [channel.port2]);
  await ready;
  frame.src = new URL(`./transfer-download/${token}`, script).href;
  document.body.append(frame);
  return {
    async write(bytes) {
      if (failure) throw failure;
      if (!demand) await new Promise((resolve, reject) => { writeResolve = resolve; writeReject = reject; });
      if (failure) throw failure;
      demand = false;
      // The decoder owns exactly this chunk; transfer it instead of copying.
      channel.port1.postMessage({ type: 'chunk', bytes: bytes.buffer }, [bytes.buffer]);
    },
    async close() {
      if (failure) throw failure;
      const closed = new Promise((resolve, reject) => { closeResolve = resolve; closeReject = reject; });
      channel.port1.postMessage({ type: 'end' });
      await closed;
    },
    async abort(reason) {
      channel.port1.postMessage({ type: 'abort', message: String(reason || 'Download cancelled') });
      dispose();
    },
  };
}

export async function exportSelectedNovels(ids) {
  if (!ids.length) { showNotification('ダウンロードする作品を選択してください', 'error'); return; }
  const screen = progressScreen('小説データのダウンロード');
  if (!screen) return;
  const controller = new AbortController();
  screen.cancelWith(() => controller.abort());
  screen.phase(`選択した ${ids.length.toLocaleString()} 作品の書き出しを開始しています…`);
  let size = 0;
  let sink;
  try {
    sink = await downloadSink(controller);
    const response = await fetch(`/api/transfer/export?progress=1&ids=${ids.map(encodeURIComponent).join(',')}`, {
      signal: controller.signal,
    });
    const complete = await consumeEvents(response, async (frame) => {
      if (frame.type === 'progress') screen.update(frame);
      else if (frame.type === 'data') {
        if (typeof frame.data !== 'string') throw new Error('不正な ZIP データを受信しました。');
        const binary = atob(frame.data);
        if (binary.length > 64 * 1024) throw new Error('ZIP チャンクが上限を超えています。');
        const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
        size += bytes.length;
        if (size > MAX_ARCHIVE_BYTES) throw new Error('ZIP が上限の 512 MiB を超えています。');
        await sink.write(bytes);
        screen.bytes(size);
      }
    });
    if (!Number.isSafeInteger(complete.bytes) || complete.bytes !== size || !size) {
      throw new Error('ZIP の受信サイズが完了応答と一致しません。');
    }
    await sink.close();
    sink = null;
    screen.done('ZIP の受信が完了しました。ブラウザのダウンロード一覧で保存結果をご確認ください。');
  } catch (error) {
    screen.done(error.name === 'AbortError' ? 'ダウンロードの通信を中止しました。' : `ダウンロードに失敗しました。\n${error.message}`, true);
  } finally {
    controller.abort();
    if (sink) { try { await sink.abort('Transfer failed'); } catch { /* Already cancelled. */ } }
  }
}

export async function importNovelArchive(file, onImported) {
  if (!file) return;
  const screen = progressScreen('小説データのアップロード', true);
  if (!screen) return;
  if (file.size > MAX_ARCHIVE_BYTES) {
    screen.done('ZIP が上限の 512 MiB を超えています。', true); return;
  }
  const controller = new AbortController();
  screen.cancelWith(() => controller.abort());
  screen.phase(`${file.name} を送信・検証しています…`);
  try {
    // File is streamed by fetch; never copy it into an ArrayBuffer or WASM.
    const response = await fetch('/api/transfer/import?progress=1', {
      method: 'POST', headers: { 'Content-Type': 'application/zip' },
      body: file, signal: controller.signal,
    });
    const complete = await consumeEvents(response, (frame) => {
      if (frame.type === 'progress') screen.update(frame);
    });
    const result = complete.result;
    if (!result || !Array.isArray(result.errors)) throw new Error('取り込み結果の形式が不正です。');
    const summary = `新規 ${result.created ?? 0} / 上書き ${result.overwritten ?? 0} / スキップ ${result.skipped ?? 0}`;
    screen.done(result.success ? `取り込みが完了しました。${summary}`
      : `一部の取り込みに失敗しました。${summary}\n${result.errors.join('\n')}`, !result.success);
    if (onImported) {
      try { await onImported(); }
      catch (error) { showNotification(`取り込み結果は確定しましたが、一覧を更新できませんでした。\n${error.message}`, 'error'); }
    }
  } catch (error) {
    screen.done(error.name === 'AbortError' ? '通信を中止しました。取り込み済みの作品は保持されます。'
      : `アップロード・取り込みに失敗しました。\n${error.message}`, true);
  } finally { controller.abort(); }
}
