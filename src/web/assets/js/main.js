/**
 * Main entry point — initialization and periodic refresh
 */
import { State, El, initElements } from './core/state.js';
import { fetchJson } from './core/http.js';
import { applyI18n } from './ui/i18n.js';
import { initDropdowns } from './ui/dropdown.js';
import { renderNovelList, renderQueueStatus, renderTagList, showNotification, syncViewChecks } from './ui/render.js';
import {
  applyNotepadSnapshot,
  bindActions,
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

async function refreshListWithUiState() {
  await refreshList();
  applyPerformanceMode();
}

async function refreshListAndTags() {
  await refreshListWithUiState();
  await refreshTags();
}

async function init() {
  initElements();
  initDropdowns();
  applyI18n();
  bindActions();
  bindConsoleAutoScroll(El.console);
  bindConsoleAutoScroll(El.consoleStdout2);

  // Load config from server
  try {
    const config = await fetchJson('/api/webui/config');
    if (config) {
      applyWebConfig(config);
    }
  } catch { /* use defaults */ }

  // Load sort state from server
  try {
    const sortState = await fetchJson('/api/sort_state');
    if (sortState) {
      const colMap = { 0: 'id', 1: 'last_update', 2: 'general_lastup', 3: 'last_check_date',
        4: 'title', 5: 'author', 6: 'sitename', 7: 'novel_type', 9: 'general_all_no', 10: 'length' };
      const col = colMap[sortState.column] || 'last_update';
      State.sortCol = col;
      State.sortAsc = sortState.dir === 'asc';
    }
  } catch { /* use defaults */ }

  // Initial data load
  await Promise.all([refreshListWithUiState(), refreshQueue(), refreshQueueDetailed(), refreshTags()]);

  // Sync UI state (check marks, wide mode, footer)
  syncViewChecks();

  // WebSocket
  connectWebSocket();

  // Periodic refresh
  const interval = Math.max(State.pollIntervalSeconds, 10) * 1000;
  setInterval(async () => {
    await refreshListWithUiState();
    await refreshQueue();
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

function applyWebConfig(config) {
  if (config.theme) {
    const theme = normalizeTheme(config.theme);
    State.theme = theme;
    document.documentElement.dataset.theme = theme === 'default' ? '' : theme;
    const sel = El.themeSelect;
    if (sel) sel.value = theme;
  }
  if (config.ws_port) State.wsPort = config.ws_port;
  if (config.performance_mode) State.performanceMode = config.performance_mode;
  if (config.reload_timing) State.tableReloadTiming = config.reload_timing;
  State.debugMode = Boolean(config.debug_mode);
  setConcurrencyEnabled(Boolean(config.concurrency_enabled));
}

function normalizeTheme(theme) {
  return (theme === 'Cerulean' || theme === 'default') ? 'default' : theme;
}

async function reloadWebConfig() {
  try {
    const config = await fetchJson('/api/webui/config');
    if (config) {
      applyWebConfig(config);
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
      refreshQueue();
      break;
    case 'refresh':
    case 'list_updated':
    case 'table.reload':
      refreshListAndTags();
      break;
    case 'tag.updateCanvas':
      refreshTags();
      break;
    case 'webui.config.reload':
      reloadWebConfig();
      break;
    case 'notepad.change':
      if (msg.data) {
        const result = applyNotepadSnapshot(msg.data, { keepLocalEdits: true });
        if (result.keptLocalEdits) {
          showNotification('他の画面でメモ帳が更新されました。保存時に再読み込みされます', 'warning');
        }
      }
      break;
    case 'echo':
      appendConsole(msg.body || '', msg.target_console);
      break;
    case 'queue_start':
      refreshQueue();
      break;
    case 'queue_complete':
      refreshQueue();
      break;
    case 'queue_failed':
      refreshQueue();
      notifyQueueFailure(msg.data);
      break;
    case 'queue_partial':
    case 'queue.partial':
      refreshQueue();
      refreshQueueDetailed();
      notifyQueuePartial(msg.data);
      break;
    case 'queue_cancelled':
    case 'queue.cancelled':
      refreshQueue();
      refreshQueueDetailed();
      notifyQueueCancelled(msg.data);
      break;
    case 'shutdown':
      appendConsole('サーバーをシャットダウンしています...');
      break;
    case 'reboot':
      appendConsole('サーバーを再起動しています...');
      rememberRebootReturnTo();
      setTimeout(() => { location.href = '/_rebooting'; }, 500);
      break;
    case 'console.clear': {
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
      break;
    }
    case 'error':
      appendConsole('[エラー] ' + (msg.data || msg.message || ''));
      break;
    case 'progressbar.init':
      initProgressBar(msg.data?.topic, msg.target_console, msg.data?.scope);
      break;
    case 'progressbar.step':
      setProgressBar(
        msg.data?.percent,
        msg.data?.current,
        msg.data?.total,
        msg.data?.topic,
        msg.target_console,
        msg.data?.scope
      );
      break;
    case 'progressbar.clear':
      removeProgressBar(msg.data?.topic, msg.target_console, msg.data?.scope);
      break;
    default:
      console.debug('Unknown WS event:', msg);
      break;
  }
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
  var hasItems = host.childElementCount > 0;
  host.classList.toggle('hide', !hasItems);
  consoleEl.classList.toggle('console-with-progress', hasItems);
  syncPinnedConsole(consoleEl);
}

function progressBarKey(topic, targetConsole, scope) {
  return (targetConsole || 'stdout') + ':' + (scope || topic || 'default');
}

function initProgressBar(topic, targetConsole, scope) {
  var key = progressBarKey(topic, targetConsole, scope);
  removeProgressBar(topic, targetConsole, scope);
  var host = getProgressHost(targetConsole);
  if (!host) return;
  var wrapper = document.createElement('div');
  wrapper.className = 'console-progress-item';
  var label = document.createElement('div');
  label.className = 'console-progress-topic';
  label.textContent = formatProgressLabel(0, 0, 0);
  wrapper.appendChild(label);
  var progress = document.createElement('div');
  progress.className = 'progress';
  progress.innerHTML = '<div class="progress-bar" style="width:0%"></div>';
  wrapper.appendChild(progress);
  host.appendChild(wrapper);
  progressBars[key] = {
    wrapper: wrapper,
    bar: progress.querySelector('.progress-bar'),
    label: label,
  };
  updateProgressHostVisibility(targetConsole);
}

function setProgressBar(percent, current, total, topic, targetConsole, scope) {
  var key = progressBarKey(topic, targetConsole, scope);
  if (!progressBars[key]) initProgressBar(topic, targetConsole, scope);
  progressBars[key].bar.style.width = (percent || 0) + '%';
  progressBars[key].label.textContent = formatProgressLabel(current, total, percent);
}

function removeProgressBar(topic, targetConsole, scope) {
  var key = progressBarKey(topic, targetConsole, scope);
  if (!progressBars[key]) return;
  var wrapper = progressBars[key].wrapper;
  if (wrapper) wrapper.remove();
  delete progressBars[key];
  updateProgressHostVisibility(targetConsole);
}

function clearAllProgressBars() {
  Object.keys(progressBars).forEach(function(key) {
    var wrapper = progressBars[key].wrapper;
    if (wrapper) wrapper.remove();
  });
  progressBars = {};
  ['stdout', 'stdout2'].forEach(function(targetConsole) {
    updateProgressHostVisibility(targetConsole);
  });
}

function formatProgressLabel(current, total, percent) {
  var currentValue = Number.isFinite(current) ? current : 0;
  var totalValue = Number.isFinite(total) ? total : 0;
  var percentValue = Number.isFinite(percent) ? percent : 0;
  return '進捗 ' + currentValue + '/' + totalValue + ' ' + percentValue.toFixed(1) + '%';
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
