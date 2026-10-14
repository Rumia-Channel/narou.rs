/**
 * Main entry point — initialization and periodic refresh
 */
import { State, El, initElements } from './core/state.js';
import { fetchJson, postJson } from './core/http.js';
import { restoreSortState } from './core/sort_state.js';
import { setCapabilities, applyActionCapabilities, initCapabilityGuard } from './core/capabilities.js';
import { applyI18n } from './ui/i18n.js';
import { initDropdowns } from './ui/dropdown.js';
import { initFeatureTour, maybeShowPendingFeatureTour } from './ui/feature_tour.js';
import { renderNovelList, renderQueueStatus, renderTagList, showNotification, syncViewChecks } from './ui/render.js';
import {
  applyNotepadSnapshot,
  captureThemeRead,
  applyThemeSnapshot,
  bindActions,
  clearConsoleHistoryUi,
  handleRegisterParam,
  refreshList,
  refreshQueue,
  refreshQueueDetailed,
  refreshTags,
} from './ui/actions.js';

let ws = null;
const REBOOT_RETURN_TO_KEY = 'narou-rs-webui-reboot-return-to';
const CONSOLE_BOTTOM_THRESHOLD = 8;

function isPerformanceModeEnabled() {
  switch (State.performanceMode) {
    case 'on':
      return true;
    case 'off':
      return false;
    case 'auto':
    default:
      return State.novels.length >= 2000;
  }
}

function applyPerformanceMode() {
  document.body.classList.toggle('performance-mode', isPerformanceModeEnabled());
}

async function refreshListWithUiState(redraw = true) {
  await refreshList(redraw);
  applyPerformanceMode();
}

let pendingListRefresh = false;
let pendingTagRefresh = false;
let refreshTimer = null;

function scheduleTableRefresh(includeList) {
  pendingListRefresh ||= includeList;
  pendingTagRefresh = true;
  if (refreshTimer !== null) return;
  refreshTimer = setTimeout(async () => {
    const reloadList = pendingListRefresh;
    const reloadTags = pendingTagRefresh;
    pendingListRefresh = pendingTagRefresh = false;
    await Promise.all([
      reloadList ? refreshListWithUiState(false) : null,
      reloadTags ? refreshTags(false) : null,
    ]);
    renderNovelList();
    refreshTimer = null;
    if (pendingListRefresh || pendingTagRefresh) scheduleTableRefresh(pendingListRefresh);
  }, 50);
}

function isQueueModalOpen() {
  return Boolean(El.queueModal && !El.queueModal.classList.contains('hide'));
}

function refreshQueueDetailedIfOpen() {
  if (isQueueModalOpen()) {
    void refreshQueueDetailed();
  }
}

async function init() {
  initElements();
  initCapabilityGuard(message => showNotification(message, 'warning'));
  applyActionCapabilities();
  initDropdowns();
  initFeatureTour();
  applyI18n();
  bindActions();
  bindConsoleAutoScroll(El.console);
  bindConsoleAutoScroll(El.consoleStdout2);

  // Bookmarklet same-origin register flow: handle `?register=<url>` before
  // kicking off async data loads so the user sees the confirmation dialog
  // promptly. We strip the param from the URL after handing it off so a
  // refresh of the page does not re-prompt with the same URL.
  handleBookmarkletRegisterParam();

  // Load config and sort state from server。
  // 互いに独立なので直列に待たず、1 往復分の待ち時間で済ませる
  // (以前は config の応答を待ってから sort_state を投げていた)。
  const themeGeneration = captureThemeRead();
  const [config, sortState] = await Promise.all([
    fetchJson('/api/webui/config').catch(() => null),
    fetchJson('/api/sort_state').catch(() => null),
  ]);

  try {
    if (config) {
      applyWebConfig(config, themeGeneration);
    }
  } catch { /* use defaults */ }

  try {
    restoreSortState(sortState);
  } catch { /* use defaults */ }

  // 一覧の取得を待たずに投げておく (モーダル/通知の表示自体は各関数が
  // データを受け取ってから行うので、体感の順序は変わらない)。
  void maybeShowPendingFeatureTour();
  void maybeOfferLibraryBackup();

  // Initial data load
  await Promise.all([refreshListWithUiState(false), refreshQueue(), refreshQueueDetailed(), refreshTags(false)]);
  renderNovelList();

  // Sync UI state (check marks, wide mode, footer)
  syncViewChecks();

  // WebSocket
  connectWebSocket();

  // Periodic refresh
  const interval = Math.max(State.pollIntervalSeconds, 10) * 1000;
  setInterval(async () => {
    const progressRevision = progressStateRevision;
    await Promise.all([refreshListWithUiState(), refreshQueue()]);
    // イベント取りこぼしで残ったバーを、キューが空になった時点で片付ける。
    if (progressRevision === progressStateRevision) clearProgressBarsWhenQueueIdle(State.queueStatus);
    if (isQueueModalOpen()) {
      await refreshQueueDetailed();
    }
  }, interval);
}

function connectWebSocket() {
  const url = new URL('/ws', location.href);
  url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';

  try {
    ws = new WebSocket(url.toString());
  } catch {
    return;
  }

  ws.onopen = () => {
    const con = document.getElementById('console');
    if (con) {
      con.innerHTML = '';
      setConsolePinned(con, true);
    }
    const con2 = document.getElementById('console-stdout2');
    if (con2) {
      con2.innerHTML = '';
      setConsolePinned(con2, true);
    }
    clearAllProgressBars();
  };

  ws.onmessage = (event) => {
    try {
      const msg = JSON.parse(event.data);
      handleWsMessage(msg);
    } catch {
      appendConsole(event.data);
    }
  };

  ws.onclose = () => {
    setTimeout(connectWebSocket, 5000);
  };

  ws.onerror = () => {
    ws?.close();
  };
}

/**
 * Detect `?register=<url>` on initial load and hand it off to the register
 * confirmation modal. We strip the param from the address bar after a successful
 * hand-off so a manual refresh does not re-prompt. This is the same-origin
 * counterpart to the legacy `/api/download_request` POST, which is now blocked
 * by the request_guard_middleware Origin check.
 */
function handleBookmarkletRegisterParam() {
  let search;
  try {
    search = new URLSearchParams(location.search);
  } catch {
    return;
  }
  const target = search.get('register');
  if (!target) return;
  if (handleRegisterParam(target)) {
    stripRegisterParamFromUrl();
  }
}

function stripRegisterParamFromUrl() {
  try {
    const url = new URL(location.href);
    url.searchParams.delete('register');
    const next = url.pathname + (url.searchParams.toString() ? '?' + url.searchParams.toString() : '') + url.hash;
    history.replaceState(null, '', next);
  } catch {
    // If the URL API is unavailable for some reason, leave the URL alone;
    // the modal still works and a refresh will simply re-prompt, which is
    // a degraded but acceptable state.
  }
}

function applyWebConfig(config, themeGeneration) {
  if (config.theme) {
    applyThemeSnapshot(normalizeTheme(config.theme), themeGeneration);
  }
  setCapabilities(config.capabilities);
  if (config.performance_mode) State.performanceMode = config.performance_mode;
  State.debugMode = Boolean(config.debug_mode);
  setConcurrencyEnabled(Boolean(config.concurrency_enabled));
}

function normalizeTheme(theme) {
  return (theme === 'Cerulean' || theme === 'default') ? 'default' : theme;
}

// Debug Mode (`webui.debug-mode`) では、Web コンソールに出る行をブラウザの
// 開発者コンソールへも流す。Web コンソールを見ていなくても、挿絵が EPUB に
// 入らない等の内部判断を DevTools で追えるようにするための転送で、表示内容は
// 変えない (OFF のときは何もしない)。
function mirrorConsoleToDevtools(text, targetConsole) {
  if (!State.debugMode || typeof console === 'undefined' || !text) return;
  const line = String(text);
  const label = '[narou]';
  if (line.startsWith('[debug]')) {
    console.debug(`${label} ${line}`, targetConsole || 'stdout');
  } else if (targetConsole && targetConsole !== 'stdout') {
    console.error(`${label} ${line}`, targetConsole);
  } else {
    console.log(`${label} ${line}`, targetConsole || 'stdout');
  }
}

async function reloadWebConfig() {
  const themeGeneration = captureThemeRead();
  try {
    const config = await fetchJson('/api/webui/config?fresh=1');
    if (config) {
      applyWebConfig(config, themeGeneration);
      applyPerformanceMode();
    }
  } catch { /* keep current config */ }
}

function setConcurrencyEnabled(enabled) {
  State.concurrencyEnabled = enabled;
  if (El.consoleColRight) {
    El.consoleColRight.classList.toggle('hide', !enabled);
  }
  renderQueueStatus();
}

function handleWsMessage(msg) {
  switch (msg.type) {
    case 'log':
    case 'console':
      appendConsole(msg.text || msg.message || JSON.stringify(msg));
      break;
    case 'status':
    case 'queue':
    case 'notification.queue':
      refreshQueueAndSyncProgressBars();
      refreshQueueDetailedIfOpen();
      break;
    case 'refresh':
    case 'list_updated':
    case 'table.reload':
      scheduleTableRefresh(true);
      break;
    case 'tag.updateCanvas':
      scheduleTableRefresh(false);
      break;
    case 'webui.config.reload':
      reloadWebConfig();
      break;
    case 'notepad.change':
      if (msg.data) {
        const result = applyNotepadSnapshot(msg.data);
        if (result.keptLocalEdits) {
          showNotification('他の画面でメモ帳が更新されました。入力内容は保持しています', 'warning');
        }
      }
      break;
    case 'echo':
      appendConsole(msg.body || '', msg.target_console);
      break;
    case 'queue_start':
      removeProgressBarsForScope(queueEventJobId(msg.data));
      closedProgressScopes.delete(queueEventJobId(msg.data));
      progressStateRevision++;
      refreshQueue();
      refreshQueueDetailedIfOpen();
      break;
    case 'queue_complete':
      removeProgressBarsForScope(queueEventJobId(msg.data));
      refreshQueueAndSyncProgressBars();
      refreshQueueDetailedIfOpen();
      break;
    case 'queue_failed':
      removeProgressBarsForScope(queueEventJobId(msg.data));
      refreshQueueAndSyncProgressBars();
      refreshQueueDetailedIfOpen();
      notifyQueueFailure(msg.data);
      break;
    case 'queue_retry':
      removeProgressBarsForScope(queueEventJobId(msg.data));
      refreshQueue();
      refreshQueueDetailedIfOpen();
      break;
    case 'queue_partial':
    case 'queue.partial':
      removeProgressBarsForScope(queueEventJobId(msg.data));
      refreshQueueAndSyncProgressBars();
      refreshQueueDetailed();
      notifyQueuePartial(msg.data);
      break;
    case 'queue_cancelled':
    case 'queue.cancelled':
      removeProgressBarsForScope(queueEventJobId(msg.data));
      refreshQueueAndSyncProgressBars();
      refreshQueueDetailed();
      notifyQueueCancelled(msg.data);
      break;
    case 'library_backup.done': {
      const path = (msg.data && msg.data.path) || '';
      showNotification(`ライブラリのバックアップを作成しました: ${path}`, 'success');
      break;
    }
    case 'library_backup.failed': {
      const message = (msg.data && msg.data.message) || '不明なエラー';
      showNotification(`ライブラリのバックアップに失敗しました: ${message}`, 'error');
      break;
    }
    case 'shutdown':
      appendConsole('サーバーをシャットダウンしています...');
      break;
    case 'reboot':
      appendConsole('サーバーを再起動しています...');
      rememberRebootReturnTo();
      setTimeout(() => { location.href = '/_rebooting'; }, 500);
      break;
    case 'console.clear':
      clearConsoleHistoryUi();
      break;
    case 'error':
      appendConsole('[エラー] ' + (msg.data || msg.message || ''));
      break;
    case 'progressbar.init':
      setProgressBar(msg.data || {}, msg.target_console, true);
      break;
    case 'progressbar.step':
      setProgressBar(msg.data || {}, msg.target_console);
      break;
    case 'progressbar.clear':
      removeProgressBar(msg.data || {}, msg.target_console);
      break;
    default:
      console.debug('Unknown WS event:', msg);
      break;
  }
}

/**
 * 0.4.0 未満からのアップデート後初回表示で、ライブラリ全体バックアップを
 * 提案するモーダルを出す。サーバー側の marker (.narou/last-run-version) が
 * 境界を跨いだ場合だけ pending=true が返る。
 */
async function maybeOfferLibraryBackup() {
  let status;
  try {
    status = assertLibraryBackupSuccess(
      await fetchJson('/api/library_backup'),
      'ライブラリのバックアップ情報を取得できませんでした'
    );
  } catch (error) {
    showNotification(
      readApiErrorMessage(error, 'ライブラリのバックアップ情報を取得できませんでした'),
      'error'
    );
    return;
  }
  if (!status || !status.pending) return;

  const modal = El.libraryBackupModal;
  if (!modal) return;

  const sizeText = formatBytes(status.total_bytes);
  const freeText = status.free_bytes != null ? formatBytes(status.free_bytes) : '不明';
  if (El.libraryBackupSize) {
    El.libraryBackupSize.textContent = `対象: ライブラリフォルダ全体 (計 ${sizeText}) / 保存先ドライブの空き: ${freeText}`;
  }
  if (El.libraryBackupSpaceWarning) {
    El.libraryBackupSpaceWarning.classList.toggle('hide', status.enough_space !== false);
  }
  if (El.libraryBackupOutput) {
    El.libraryBackupOutput.value = status.default_output || '';
  }

  modal.classList.remove('hide');

  const close = () => modal.classList.add('hide');
  El.libraryBackupClose?.addEventListener('click', close, { once: true });
  El.libraryBackupDismiss?.addEventListener('click', async () => {
    close();
    try {
      const result = await postJson('/api/library_backup', { action: 'dismiss' });
      assertLibraryBackupSuccess(result, 'バックアップの提案を閉じられませんでした');
    } catch (error) {
      showNotification(readApiErrorMessage(error, 'バックアップの提案を閉じられませんでした'), 'error');
    }
  }, { once: true });
  El.libraryBackupCreate?.addEventListener('click', async () => {
    close();
    const output = El.libraryBackupOutput?.value?.trim() || '';
    try {
      const result = assertLibraryBackupSuccess(
        await postJson('/api/library_backup', { action: 'create', output }),
        'バックアップの開始に失敗しました'
      );
      if (result && result.started) {
        showNotification('ライブラリのバックアップを開始しました', 'info');
      }
    } catch (error) {
      showNotification(readApiErrorMessage(error, 'バックアップの開始に失敗しました'), 'error');
    }
  }, { once: true });
}

function assertLibraryBackupSuccess(result, fallbackMessage) {
  const error = result?.error;
  if (result?.success === false || error) {
    const message = result?.message
      || error?.message
      || (typeof error === 'string' ? error : '');
    throw new Error(message || fallbackMessage);
  }
  return result;
}

function readApiErrorMessage(error, fallbackMessage) {
  // `core/http.js` が応答本文のメッセージを抽出済みなので、そのまま使う。
  const message = error?.message;
  return typeof message === 'string' && message.trim() ? message : fallbackMessage;
}

function formatBytes(bytes) {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes)) return '不明';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${bytes} ${units[unit]}` : `${value.toFixed(1)} ${units[unit]}`;
}

function notifyQueueFailure(data) {
  const payload = (data && typeof data === 'object') ? data : {};
  const detail = formatFailureDetail(payload.detail);
  const reason = (typeof payload.reason === 'string') ? payload.reason.trim() : '';
  const baseMessage = detail
    ? '処理に失敗しました'
    : (reason ? `処理に失敗しました: ${reason}` : '処理に失敗しました。詳細はコンソールを確認してください');
  if (State.debugMode && detail) {
    showNotification(`${baseMessage}: ${detail}`, 'error');
    return;
  }
  showNotification(baseMessage, 'error');
}

function notifyQueuePartial(data) {
  const jobLabel = formatQueueJobLabel(data);
  showNotification(`${jobLabel}は一部未完了で終了しました。詳細はコンソールを確認してください`, 'warning');
}

function notifyQueueCancelled(data) {
  const jobLabel = formatQueueJobLabel(data);
  showNotification(`${jobLabel}を中断しました`, 'warning');
}

function formatQueueJobLabel(data) {
  const jobId = data && typeof data === 'object' ? data.job_id : null;
  if (typeof jobId === 'string' && jobId) {
    return `処理 (${jobId})`;
  }
  return '処理';
}

function formatFailureDetail(detail) {
  if (!State.debugMode || typeof detail !== 'string') return '';
  return detail
    .split('\n')
    .map(line => line.trim())
    .filter(Boolean)
    .join(' | ')
    .slice(0, 320);
}

function rememberRebootReturnTo() {
  if (location.pathname === '/_rebooting') return;
  try {
    sessionStorage.setItem(
      REBOOT_RETURN_TO_KEY,
      location.pathname + location.search + location.hash
    );
  } catch {
    // Ignore storage errors and fall back to root.
  }
}

function getConsoleEl(targetConsole) {
  if (targetConsole === 'stdout2') {
    if (!State.concurrencyEnabled) setConcurrencyEnabled(true);
    return document.getElementById('console-stdout2') || El.console;
  }
  return El.console;
}

function getConsolePinKeyByElement(consoleEl) {
  return consoleEl?.id === 'console-stdout2' ? 'stdout2' : 'main';
}

function isConsoleNearBottom(consoleEl) {
  return consoleEl.scrollTop + consoleEl.clientHeight >= consoleEl.scrollHeight - CONSOLE_BOTTOM_THRESHOLD;
}

function setConsolePinned(consoleEl, pinned) {
  if (!consoleEl) return;
  State.consolePinned[getConsolePinKeyByElement(consoleEl)] = pinned;
}

function isConsolePinned(consoleEl) {
  if (!consoleEl) return false;
  return State.consolePinned[getConsolePinKeyByElement(consoleEl)] !== false;
}

function scrollConsoleToBottom(consoleEl) {
  if (!consoleEl) return;
  consoleEl.scrollTop = consoleEl.scrollHeight;
}

function syncPinnedConsole(consoleEl) {
  if (isConsolePinned(consoleEl)) {
    scrollConsoleToBottom(consoleEl);
  }
}

function bindConsoleAutoScroll(consoleEl) {
  if (!consoleEl) return;
  setConsolePinned(consoleEl, true);
  consoleEl.addEventListener('scroll', () => {
    setConsolePinned(consoleEl, isConsoleNearBottom(consoleEl));
  }, { passive: true });
  if (typeof ResizeObserver !== 'undefined') {
    const observer = new ResizeObserver(() => syncPinnedConsole(consoleEl));
    observer.observe(consoleEl);
  }
}

var progressBars = {};
var closedProgressOperations = new Map();
var closedProgressScopes = new Map();
var progressStateRevision = 0;

function getProgressHost(targetConsole) {
  var consoleEl = getConsoleEl(targetConsole);
  if (!consoleEl) return null;
  var host = consoleEl.parentElement?.querySelector('.console-progress-host');
  if (host) return host;
  host = document.createElement('div');
  host.className = 'console-progress-host hide';
  consoleEl.parentElement?.appendChild(host);
  return host;
}

function updateProgressHostVisibility(targetConsole) {
  var consoleEl = getConsoleEl(targetConsole);
  var host = consoleEl?.parentElement?.querySelector('.console-progress-host');
  if (!host) return;
  host.classList.toggle('hide', host.childElementCount === 0);
  syncPinnedConsole(consoleEl);
}

function progressBarKey(data, targetConsole) {
  return JSON.stringify([
    targetConsole || 'stdout', data.scope || '', data.topic || 'default', data.operation || '',
  ]);
}

function rememberProgressCompletion(entries, key) {
  entries.delete(key);
  entries.set(key, true);
  if (entries.size > 512) entries.delete(entries.keys().next().value);
}

function discardProgressBar(key) {
  var entry = progressBars[key];
  if (!entry) return;
  entry.wrapper.remove();
  delete progressBars[key];
  progressStateRevision++;
  updateProgressHostVisibility(entry.console);
}

/** A terminal job event closes every operation in that job, never its peers. */
function removeProgressBarsForScope(scope, targetConsole) {
  if (typeof scope !== 'string' || !scope) return;
  rememberProgressCompletion(closedProgressScopes, scope);
  Object.keys(progressBars).forEach(function(key) {
    var entry = progressBars[key];
    if (entry.scope !== scope || (targetConsole && entry.console !== targetConsole)) return;
    if (entry.operation) rememberProgressCompletion(closedProgressOperations, key);
    discardProgressBar(key);
  });
  progressStateRevision++;
}

function queueEventJobId(data) {
  if (typeof data === 'string') return data;
  if (data && typeof data === 'object' && typeof data.job_id === 'string') return data.job_id;
  return '';
}

/** Unknown/partial queue status is not proof that a running job is idle. */
function clearProgressBarsWhenQueueIdle(queueStatus) {
  if (!queueStatus || !Number.isFinite(queueStatus.running_count) || !Number.isFinite(queueStatus.pending)) return;
  if (queueStatus.running_count !== 0 || queueStatus.pending !== 0) return;
  Object.keys(progressBars).forEach(function(key) {
    if (progressBars[key].scope) discardProgressBar(key);
  });
}

function refreshQueueAndSyncProgressBars() {
  var revision = progressStateRevision;
  var pending = refreshQueue();
  if (pending && typeof pending.then === 'function') {
    pending.then(function() {
      if (revision === progressStateRevision) clearProgressBarsWhenQueueIdle(State.queueStatus);
    }, function() {});
  }
}

function setProgressBar(data, targetConsole, isInit) {
  var key = progressBarKey(data, targetConsole);
  if (closedProgressScopes.has(data.scope) || closedProgressOperations.has(key)) return;
  var entry = progressBars[key];
  var hasSequence = Number.isSafeInteger(data.sequence) && data.sequence >= 0;
  if (entry && ((hasSequence && data.sequence <= entry.sequence) || (isInit && !hasSequence))) return;
  if (!entry) {
    var host = getProgressHost(targetConsole);
    if (!host) return;
    var wrapper = document.createElement('div');
    wrapper.className = 'console-progress-item';
    var label = document.createElement('div');
    label.className = 'console-progress-topic';
    wrapper.appendChild(label);
    var progress = document.createElement('div');
    progress.className = 'progress';
    progress.setAttribute('role', 'progressbar');
    progress.setAttribute('aria-valuemin', '0');
    progress.setAttribute('aria-valuemax', '100');
    var bar = document.createElement('div');
    bar.className = 'progress-bar';
    progress.appendChild(bar);
    wrapper.appendChild(progress);
    host.appendChild(wrapper);
    entry = progressBars[key] = {
      wrapper: wrapper, progress: progress, bar: bar, label: label,
      topic: data.topic || 'default', console: targetConsole || 'stdout',
      scope: data.scope || '', operation: data.operation || '', sequence: -1,
    };
    updateProgressHostVisibility(targetConsole);
  }
  if (hasSequence) entry.sequence = data.sequence;
  var total = Number.isFinite(data.total) && data.total > 0 ? data.total : null;
  var current = Number.isFinite(data.current) ? Math.max(0, data.current) : null;
  if (total !== null && current !== null) current = Math.min(current, total);
  var percent = total !== null && current !== null ? current / total * 100
    : Number.isFinite(data.percent) ? Math.max(0, Math.min(100, data.percent)) : null;
  var labelText = formatProgressLabel(current, total, percent);
  if (typeof data.message === 'string' && data.message) labelText += ' — ' + data.message;
  entry.label.textContent = labelText;
  entry.label.title = labelText;
  entry.progress.setAttribute('aria-valuetext', labelText);
  entry.progress.setAttribute('aria-label', data.message || entry.topic);
  entry.bar.classList.toggle('progress-bar-indeterminate', percent === null);
  entry.bar.style.width = percent === null ? 'var(--progress-indeterminate-width)' : percent + '%';
  if (percent === null) entry.progress.removeAttribute('aria-valuenow');
  else entry.progress.setAttribute('aria-valuenow', String(percent));
  progressStateRevision++;
}

function removeProgressBar(data, targetConsole) {
  if (data.scope && !data.topic && !data.operation) {
    removeProgressBarsForScope(data.scope, targetConsole);
    return;
  }
  var key = progressBarKey(data, targetConsole);
  var entry = progressBars[key];
  if (entry && Number.isSafeInteger(data.sequence) && data.sequence < entry.sequence) return;
  if (data.operation) rememberProgressCompletion(closedProgressOperations, key);
  discardProgressBar(key);
}

function clearAllProgressBars() {
  Object.keys(progressBars).forEach(discardProgressBar);
  closedProgressOperations.clear();
  closedProgressScopes.clear();
  progressStateRevision++;
}

function formatProgressLabel(current, total, percent) {
  if (percent === null) return current ? '確認済み ' + current + '（総数未確定）' : '進捗（準備中）';
  var counts = total !== null && current !== null ? '完了 ' + current + '/' + total + ' ' : '進捗 ';
  return counts + percent.toFixed(1) + '%';
}

function sanitizeConsoleSpanStyle(styleText) {
  var declarations = String(styleText || '')
    .split(';')
    .map(function(part) { return part.trim(); })
    .filter(Boolean);
  if (!declarations.length || declarations.length > 2) return '';

  var color = '';
  var fontWeight = '';
  for (var i = 0; i < declarations.length; i++) {
    var separator = declarations[i].indexOf(':');
    if (separator <= 0) return '';
    var name = declarations[i].slice(0, separator).trim().toLowerCase();
    var value = declarations[i].slice(separator + 1).trim();
    if (name === 'color') {
      if (!/^[#(),.%\sa-zA-Z0-9-]+$/.test(value)) return '';
      color = value;
    } else if (name === 'font-weight') {
      if (value.toLowerCase() !== 'bold') return '';
      fontWeight = 'bold';
    } else {
      return '';
    }
  }
  if (!color) return '';

  var parts = [];
  if (fontWeight) parts.push('font-weight:bold');
  parts.push('color:' + color);
  return parts.join(';');
}

function sanitizeConsoleNodes(source, target) {
  var children = source.childNodes || [];
  for (var i = 0; i < children.length; i++) {
    var child = children[i];
    if (child.nodeType === Node.TEXT_NODE) {
      target.appendChild(document.createTextNode(child.textContent || ''));
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && child.tagName === 'RUBY') {
      appendConsoleRubyBaseText(child, target);
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && (child.tagName === 'RT' || child.tagName === 'RP')) {
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && child.tagName === 'RB') {
      sanitizeConsoleNodes(child, target);
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && child.tagName === 'SPAN') {
      var style = sanitizeConsoleSpanStyle(child.getAttribute('style') || '');
      if (!style) {
        target.appendChild(document.createTextNode(child.textContent || ''));
        continue;
      }
      var span = document.createElement('span');
      span.setAttribute('style', style);
      sanitizeConsoleNodes(child, span);
      target.appendChild(span);
      continue;
    }
    target.appendChild(document.createTextNode(child.textContent || ''));
  }
}

function appendConsoleRubyBaseText(source, target) {
  var rbElements = source.querySelectorAll('rb');
  if (rbElements.length > 0) {
    for (var i = 0; i < rbElements.length; i++) {
      target.appendChild(document.createTextNode(rbElements[i].textContent || ''));
    }
    return;
  }

  var children = source.childNodes || [];
  for (var i = 0; i < children.length; i++) {
    var child = children[i];
    if (child.nodeType === Node.TEXT_NODE) {
      target.appendChild(document.createTextNode(child.textContent || ''));
    } else if (child.nodeType === Node.ELEMENT_NODE && child.tagName !== 'RT' && child.tagName !== 'RP') {
      sanitizeConsoleNodes(child, target);
    }
  }
}

function appendSanitizedConsoleHtml(target, html) {
  var template = document.createElement('template');
  template.innerHTML = html;
  sanitizeConsoleNodes(template.content, target);
}

var lastLineComplete = true;

function appendConsole(text, targetConsole) {
  const con = getConsoleEl(targetConsole);
  if (!con) return;
  State.consoleRevision++;

  mirrorConsoleToDevtools(text, targetConsole);

  // Ensure text ends with newline (worker strips \n from BufReader::lines())
  if (text.length > 0 && !text.endsWith('\n')) {
    text += '\n';
  }

  const maxLines = isPerformanceModeEnabled() ? 200 : 1000;
  const shouldStick = isConsolePinned(con);

  const lines = text.split('\n');
  // Remove trailing empty element from split (text ends with \n)
  if (lines.length > 0 && lines[lines.length - 1] === '') lines.pop();

  for (var i = 0; i < lines.length; i++) {
    // Ruby web mode sends <hr> for horizontal rules; also detect text HR (U+2015 × 30+)
    if (lines[i] === '<hr>' || /^―{30,}$/.test(lines[i])) {
      var hr = document.createElement('hr');
      hr.className = 'console-line console-hr';
      con.appendChild(hr);
    } else {
      var div = document.createElement('div');
      div.className = 'console-line';
      // Allow limited HTML in console output: styled spans and ruby stripped to base text.
      if (/<(?:span|ruby|rb|rt|rp)[\s>]/i.test(lines[i])) {
        appendSanitizedConsoleHtml(div, lines[i]);
      } else {
        div.textContent = lines[i];
      }
      con.appendChild(div);
    }
  }

  // Trim old lines (only console-line elements, preserve progress bars)
  var lineEls = con.querySelectorAll('.console-line');
  if (lineEls.length > maxLines) {
    var toRemove = lineEls.length - maxLines;
    for (var j = 0; j < toRemove; j++) {
      lineEls[j].remove();
    }
  }

  if (shouldStick) {
    scrollConsoleToBottom(con);
  }
}

document.addEventListener('DOMContentLoaded', init);
