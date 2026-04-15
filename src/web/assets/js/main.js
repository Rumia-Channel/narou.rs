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
    default:
      appendConsole(msg.text || msg.message || JSON.stringify(msg));
      break;
  }
}

function appendConsole(text) {
  const con = El.console;
  if (!con) return;

  const maxLines = State.performanceMode ? 200 : 1000;
  con.textContent += text + '\n';

  // Trim old lines
  const lines = con.textContent.split('\n');
  if (lines.length > maxLines) {
    con.textContent = lines.slice(-maxLines).join('\n');
  }

  con.scrollTop = con.scrollHeight;
}

document.addEventListener('DOMContentLoaded', init);
