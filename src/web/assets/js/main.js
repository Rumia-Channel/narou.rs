/**
 * Main entry point — initialization and periodic refresh
 */
import { State, El, initElements } from './core/state.js';
import { fetchJson } from './core/http.js';
import { applyI18n } from './ui/i18n.js';
import { initDropdowns } from './ui/dropdown.js';
import { renderNovelList, renderQueueStatus, renderTagList } from './ui/render.js';
import { bindActions, refreshList, refreshQueue, refreshTags } from './ui/actions.js';

let ws = null;

async function init() {
  initElements();
  initDropdowns();
  applyI18n();
  bindActions();

  // Load config
  try {
    const config = await fetchJson('/api/webui/config');
    if (config) {
      if (config.theme) {
        State.theme = config.theme;
        document.documentElement.dataset.theme = config.theme;
      }
      if (config.ws_port) State.wsPort = config.ws_port;
      if (config.performance_mode) State.performanceMode = config.performance_mode;
      if (typeof config.reload_timing === 'number') State.reloadTiming = config.reload_timing;
    }
  } catch { /* use defaults */ }

  if (State.performanceMode) {
    document.body.classList.add('performance-mode');
  }

  // Initial data load
  await Promise.all([refreshList(), refreshQueue(), refreshTags()]);

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
  if (msg.type === 'log' || msg.type === 'console') {
    appendConsole(msg.text || msg.message || JSON.stringify(msg));
  } else if (msg.type === 'status' || msg.type === 'queue') {
    refreshQueue();
  } else if (msg.type === 'refresh' || msg.type === 'list_updated') {
    refreshList();
    refreshTags();
  } else {
    appendConsole(msg.text || msg.message || JSON.stringify(msg));
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
