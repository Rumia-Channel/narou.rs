import { fetchJson, postJson, request } from "./core/http.js";
import {
  cacheElements,
  elements,
  pruneSelection,
  selectedIds,
  state,
} from "./core/state.js";
import { bindActions } from "./ui/actions.js";
import { applyTranslations, loadPreferredLanguage, setLanguage, t } from "./ui/i18n.js";
import { appendEvent, renderNovels, renderTags, updateQueueUi } from "./ui/render.js";

document.addEventListener("DOMContentLoaded", () => {
  cacheElements();
  bindActions({
    batchMutation,
    changeLanguage,
    clearQueue,
    clearSelection,
    filterByTag,
    handleRowAction,
    loadNovels,
    queueConvert,
    queueDownload,
    queueUpdate,
    refreshAll,
    removeSelected,
    renderNovels,
    saveNotepad,
  });
  initialize().catch(handleError);
});

async function initialize() {
  state.language = loadPreferredLanguage();
  state.config = await fetchJson("/api/webui/config");
  applyWebUiConfig();
  appendEvent("info", t("initMessage"));

  const [devicesResponse] = await Promise.all([
    fetchJson("/api/devices"),
    loadNotepad(),
    loadTags(),
    loadQueueStatus(),
    loadNovels(),
  ]);
  state.devices = Array.isArray(devicesResponse.devices) ? devicesResponse.devices : [];

  await loadRecentLogs();
  connectWebSocket();
  startPolling();
}

function applyWebUiConfig() {
  document.documentElement.dataset.theme = state.config.theme || "Cerulean";
  if (state.config.performance_mode === "on") {
    state.performanceMode = true;
  }
  document.body.classList.toggle("performance-mode", state.performanceMode);
  applyTranslations();
}

function startPolling() {
  const pollInterval = resolvePollInterval();
  if (state.pollTimer) {
    clearInterval(state.pollTimer);
  }
  state.pollTimer = window.setInterval(async () => {
    const previousPending = state.queue.pending;
    await loadQueueStatus();
    if (shouldReloadNovels(previousPending)) {
      await loadNovels();
    }
  }, pollInterval);
}

function resolvePollInterval() {
  return state.performanceMode ? 15000 : 5000;
}

function shouldReloadNovels(previousPending) {
  if (!state.config) {
    return false;
  }
  if (state.config.reload_timing === "every") {
    return true;
  }
  return previousPending > 0 && state.queue.pending === 0;
}

async function refreshAll(force = false) {
  await Promise.all([loadNovels(), loadQueueStatus(), loadTags(), loadNotepad()]);
  if (force) {
    appendEvent("info", t("reloadMessage"));
  }
}

async function loadNovels() {
  const params = new URLSearchParams();
  params.set("draw", "1");
  params.set("start", "0");
  params.set("length", state.performanceMode ? "200" : "500");
  params.set("search[value]", state.search);
  params.set("order[0][column]", String(state.sortColumn));
  params.set("order[0][dir]", state.sortDir);

  const response = await fetchJson(`/api/list?${params.toString()}`);
  state.novels = Array.isArray(response.data) ? response.data : [];

  if (state.config?.performance_mode === "auto") {
    state.performanceMode = (response.records_total || 0) >= 2000;
    document.body.classList.toggle("performance-mode", state.performanceMode);
  }

  pruneSelection();
  renderNovels();
}

async function loadQueueStatus() {
  state.queue = await fetchJson("/api/queue/status");
  updateQueueUi(state.queue);
}

async function loadTags() {
  const response = await fetchJson("/api/tag_list");
  renderTags(Array.isArray(response.tags) ? response.tags : []);
}

async function loadNotepad() {
  const response = await fetchJson("/api/notepad/read");
  elements.notepad.value = response.content || "";
}

async function saveNotepad() {
  await postJson("/api/notepad/save", { content: elements.notepad.value });
  appendEvent("info", t("notepadSaved"));
}

async function loadRecentLogs() {
  const response = await fetchJson("/api/log/recent?count=50");
  const logs = Array.isArray(response.logs) ? response.logs : [];
  logs.forEach((entry) =>
    appendEvent(entry.level || "info", `${entry.timestamp || ""} ${entry.message || ""}`.trim()),
  );
}

function connectWebSocket() {
  if (!state.config?.ws_port) {
    return;
  }
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  const host = window.location.hostname || "127.0.0.1";
  const socket = new WebSocket(`${protocol}//${host}:${state.config.ws_port}/ws`);

  socket.addEventListener("message", async (event) => {
    try {
      const payload = JSON.parse(event.data);
      await handlePushEvent(payload);
    } catch {
      appendEvent("info", event.data);
    }
  });
  socket.addEventListener("open", () => appendEvent("info", t("wsConnected")));
  socket.addEventListener("close", () => appendEvent("warn", t("wsDisconnected")));
  socket.addEventListener("error", () => appendEvent("error", t("wsError")));
}

async function handlePushEvent(payload) {
  if (!payload || typeof payload !== "object") {
    return;
  }

  if (payload.type === "log") {
    appendEvent(payload.level || "info", payload.message || "");
    return;
  }

  appendEvent(payload.type || "event", payload.data || "");

  if (payload.type && payload.type.includes("queue")) {
    await loadQueueStatus();
  }

  if (
    payload.type === "queue_complete" ||
    payload.type === "queue_failed" ||
    payload.type === "freeze" ||
    payload.type === "unfreeze" ||
    payload.type === "batch_freeze" ||
    payload.type === "batch_unfreeze" ||
    payload.type === "batch_remove" ||
    payload.type === "tags_update"
  ) {
    await loadNovels();
  }
}

async function handleRowAction(action, id) {
  switch (action) {
    case "update":
      await postJson("/api/update", { ids: [id] });
      appendEvent("info", `${t("queueUpdateQueued")}: ${id}`);
      break;
    case "convert":
      await queueConvert([String(id)]);
      break;
    case "freeze":
      await postJson(`/api/novels/${id}/freeze`, {});
      break;
    case "unfreeze":
      await postJson(`/api/novels/${id}/unfreeze`, {});
      break;
    case "tags":
      await editTags(id);
      break;
    case "remove":
      if (!window.confirm(`ID:${id} ${t("rowRemoveConfirm")}`)) {
        return;
      }
      await request(`/api/novels/${id}`, { method: "DELETE" });
      state.selected.delete(id);
      await refreshAll();
      break;
    default:
      break;
  }
}

async function editTags(id) {
  const novel = state.novels.find((entry) => entry.id === id);
  const current = novel ? novel.tags.join(", ") : "";
  const response = window.prompt(t("tagsPrompt"), current);
  if (response === null) {
    return;
  }
  const tags = response
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  await request(`/api/novels/${id}/tags`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tags }),
  });
  await refreshAll();
}

async function queueDownload() {
  const input = window.prompt(t("queueDownloadPrompt"));
  if (!input) {
    return;
  }
  const targets = input.split(/\s+/).map((value) => value.trim()).filter(Boolean);
  if (targets.length === 0) {
    return;
  }
  await postJson("/api/download", { targets });
  appendEvent("info", `${t("queueDownloadQueued")}: ${targets.join(", ")}`);
  await loadQueueStatus();
}

async function queueUpdate() {
  const ids = selectedIds();
  if (ids.length === 0) {
    if (!window.confirm(t("queueUpdateAllConfirm"))) {
      return;
    }
    await postJson("/api/update", { all: true });
    appendEvent("info", t("queueUpdateAllQueued"));
  } else {
    await postJson("/api/update", { ids });
    appendEvent("info", `${t("queueUpdateQueued")}: ${ids.join(", ")}`);
  }
  await loadQueueStatus();
}

async function queueConvert(targets = null) {
  const selectedTargets = targets || selectedIds().map(String);
  if (selectedTargets.length === 0) {
    window.alert(t("queueConvertAlert"));
    return;
  }

  const availableNames = state.devices
    .filter((device) => device.available)
    .map((device) => device.name);
  const fallback = availableNames.includes("text") ? "text" : (availableNames[0] || "text");
  const response = window.prompt(
    `${t("queueConvertPrompt")} (${availableNames.join(", ") || "text"})`,
    fallback,
  );
  if (!response) {
    return;
  }
  await postJson("/api/convert", { targets: selectedTargets, device: response.trim() });
  appendEvent("info", `${t("queueConvertQueued")}: ${selectedTargets.join(", ")} -> ${response.trim()}`);
  await loadQueueStatus();
}

async function batchMutation(kind) {
  const ids = selectedIds();
  if (ids.length === 0) {
    window.alert(t("batchSelectAlert"));
    return;
  }
  await postJson(`/api/novels/${kind}`, { ids });
  await refreshAll();
}

async function removeSelected() {
  const ids = selectedIds();
  if (ids.length === 0) {
    window.alert(t("removeSelectedAlert"));
    return;
  }
  if (!window.confirm(`${ids.length} ${t("removeSelectedConfirm")}`)) {
    return;
  }
  await postJson("/api/novels/remove", { ids });
  clearSelection();
  await refreshAll();
}

async function clearQueue() {
  await postJson("/api/queue/clear", {});
  await loadQueueStatus();
  appendEvent("info", t("queueCleared"));
}

function clearSelection() {
  state.selected.clear();
  renderNovels();
}

async function filterByTag(tag) {
  state.search = tag;
  elements.searchInput.value = tag;
  await loadNovels();
}

async function changeLanguage(language) {
  setLanguage(language);
  renderNovels();
  appendEvent("info", `${t("languageChanged")}: ${state.language.toUpperCase()}`);
}

function handleError(error) {
  const message = error instanceof Error ? error.message : String(error);
  appendEvent("error", message);
  window.console.error(error);
}
