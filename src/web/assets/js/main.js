/**
 * Main entry point — initialization and periodic refresh
 */
import { State, El, initElements } from './core/state.js';
import { fetchJson } from './core/http.js';
import { applyI18n } from './ui/i18n.js';
import { initDropdowns } from './ui/dropdown.js';
import { renderNovelList, renderQueueStatus, renderTagList, syncViewChecks } from './ui/render.js';
import { bindActions, refreshList, refreshQueue, refreshTags } from './ui/actions.js';

let ws = null;
const REBOOT_RETURN_TO_KEY = 'narou-rs-webui-reboot-return-to';

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

  // Load config from server
  try {
    const config = await fetchJson('/api/webui/config');
    if (config) {
      if (config.theme && !localStorage.getItem('narou-rs-webui-theme')) {
        State.theme = config.theme;
        document.documentElement.dataset.theme = config.theme === 'default' ? '' : config.theme;
        const sel = El.themeSelect;
        if (sel) sel.value = config.theme;
      }
      if (config.ws_port) State.wsPort = config.ws_port;
      if (config.performance_mode) State.performanceMode = config.performance_mode;
      if (config.reload_timing) State.tableReloadTiming = config.reload_timing;
      if (config.concurrency_enabled) {
        State.concurrencyEnabled = true;
        if (El.consoleColRight) El.consoleColRight.classList.remove('hide');
      }
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
  await Promise.all([refreshListWithUiState(), refreshQueue(), refreshTags()]);

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
  if (!State.wsPort) return;
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const url = `${proto}//${location.hostname}:${State.wsPort}/ws`;

  try {
    ws = new WebSocket(url);
  } catch {
    return;
  }

  ws.onopen = () => {
    const con = document.getElementById('console');
    if (con) con.innerHTML = '';
    const con2 = document.getElementById('console-stdout2');
    if (con2) con2.innerHTML = '';
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
      if (con) con.innerHTML = '';
      const con2 = document.getElementById('console-stdout2');
      if (con2) con2.innerHTML = '';
      break;
    }
    case 'error':
      appendConsole('[エラー] ' + (msg.data || msg.message || ''));
      break;
    case 'progressbar.init':
      initProgressBar(msg.data?.topic, msg.target_console);
      break;
    case 'progressbar.step':
      setProgressBar(msg.data?.percent, msg.data?.topic, msg.target_console);
      break;
    case 'progressbar.clear':
      removeProgressBar(msg.data?.topic, msg.target_console);
      break;
    default:
      console.debug('Unknown WS event:', msg);
      break;
  }
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
  if (targetConsole === 'stdout2' && State.concurrencyEnabled) {
    return document.getElementById('console-stdout2');
  }
  return El.console;
}

var progressBars = {};

function getProgressHost() {
  var host = document.getElementById('global-progressbars');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'global-progressbars';
  host.className = 'global-progressbars hide';
  document.body.appendChild(host);
  return host;
}

function updateProgressHostVisibility() {
  var host = document.getElementById('global-progressbars');
  if (!host) return;
  host.classList.toggle('hide', Object.keys(progressBars).length === 0);
}

function initProgressBar(topic, targetConsole) {
  var key = (targetConsole || 'stdout') + ':' + (topic || 'default');
  removeProgressBar(topic, targetConsole);
  var host = getProgressHost();
  var wrapper = document.createElement('div');
  wrapper.className = 'global-progress-item';
  if (topic) {
    var label = document.createElement('div');
    label.className = 'global-progress-topic';
    label.textContent = topic;
    wrapper.appendChild(label);
  }
  var progress = document.createElement('div');
  progress.className = 'progress';
  progress.innerHTML = '<div class="progress-bar" style="width:0%"></div>';
  wrapper.appendChild(progress);
  host.appendChild(wrapper);
  progressBars[key] = {
    wrapper: wrapper,
    bar: progress.querySelector('.progress-bar'),
  };
  updateProgressHostVisibility();
}

function setProgressBar(percent, topic, targetConsole) {
  var key = (targetConsole || 'stdout') + ':' + (topic || 'default');
  if (!progressBars[key]) initProgressBar(topic, targetConsole);
  progressBars[key].bar.style.width = (percent || 0) + '%';
}

function removeProgressBar(topic, targetConsole) {
  var key = (targetConsole || 'stdout') + ':' + (topic || 'default');
  if (!progressBars[key]) return;
  var wrapper = progressBars[key].wrapper;
  if (wrapper) wrapper.remove();
  delete progressBars[key];
  updateProgressHostVisibility();
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
  const wasBottom = (con.scrollTop + con.clientHeight >= con.scrollHeight - 4);

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
      // If text contains <span> tags (from color output), render as HTML
      if (/<span[\s>]/.test(lines[i])) {
        div.innerHTML = lines[i];
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

  if (wasBottom) con.scrollTop = con.scrollHeight;
}

document.addEventListener('DOMContentLoaded', init);
