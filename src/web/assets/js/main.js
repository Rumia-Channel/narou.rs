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
      if (typeof config.reload_timing === 'number') State.reloadTiming = config.reload_timing;
    }
  } catch { /* use defaults */ }

  if (State.performanceMode) {
    document.body.classList.add('performance-mode');
  }

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
  await Promise.all([refreshList(), refreshQueue(), refreshTags()]);

  // Sync UI state (check marks, wide mode, footer)
  syncViewChecks();

  // WebSocket
  connectWebSocket();

  // Periodic refresh
  const interval = Math.max(State.reloadTiming, 10) * 1000;
  setInterval(async () => {
    await refreshList();
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
      refreshList();
      refreshTags();
      break;
    case 'tag.updateCanvas':
      refreshTags();
      break;
    case 'echo':
      appendConsole(msg.body || '');
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
      setTimeout(() => { location.href = '/_rebooting'; }, 500);
      break;
    case 'console.clear': {
      const con = document.getElementById('console');
      if (con) con.innerHTML = '';
      break;
    }
    case 'error':
      appendConsole('[エラー] ' + (msg.data || msg.message || ''));
      break;
    case 'progressbar.init':
      initProgressBar(msg.data?.topic);
      break;
    case 'progressbar.step':
      setProgressBar(msg.data?.percent, msg.data?.topic);
      break;
    case 'progressbar.clear':
      removeProgressBar(msg.data?.topic);
      break;
    default:
      console.debug('Unknown WS event:', msg);
      break;
  }
}

var progressBars = {};

function initProgressBar(topic) {
  var key = topic || 'default';
  removeProgressBar(key);
  var con = El.console;
  if (!con) return;
  var wrapper = document.createElement('div');
  wrapper.className = 'progress';
  wrapper.innerHTML = '<div class="progress-bar" style="width:0%"></div>';
  con.appendChild(wrapper);
  progressBars[key] = wrapper.querySelector('.progress-bar');
  con.scrollTop = con.scrollHeight;
}

function setProgressBar(percent, topic) {
  var key = topic || 'default';
  if (!progressBars[key]) initProgressBar(key);
  progressBars[key].style.width = (percent || 0) + '%';
}

function removeProgressBar(topic) {
  var key = topic || 'default';
  if (!progressBars[key]) return;
  var wrapper = progressBars[key].parentElement;
  if (wrapper) wrapper.remove();
  delete progressBars[key];
}

var lastLineComplete = true;

function appendConsole(text) {
  const con = El.console;
  if (!con) return;

  // Ensure text ends with newline (worker strips \n from BufReader::lines())
  if (text.length > 0 && !text.endsWith('\n')) {
    text += '\n';
  }

  const maxLines = State.performanceMode ? 200 : 1000;
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
      div.textContent = lines[i];
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
