export const state = {
  config: null,
  devices: [],
  novels: [],
  selected: new Set(),
  search: "",
  sortColumn: 0,
  sortDir: "asc",
  queue: { pending: 0, completed: 0, failed: 0 },
  performanceMode: false,
  eventCount: 0,
  pollTimer: null,
  language: "ja",
};

export const elements = {};

export function cacheElements() {
  elements.searchInput = document.getElementById("search-input");
  elements.novelsTbody = document.getElementById("novels-tbody");
  elements.selectAll = document.getElementById("select-all");
  elements.selectedCount = document.getElementById("selected-count");
  elements.queuePending = document.getElementById("queue-pending");
  elements.queueCompleted = document.getElementById("queue-completed");
  elements.queueFailed = document.getElementById("queue-failed");
  elements.queuePendingDetail = document.getElementById("queue-pending-detail");
  elements.queueCompletedDetail = document.getElementById("queue-completed-detail");
  elements.queueFailedDetail = document.getElementById("queue-failed-detail");
  elements.eventLog = document.getElementById("event-log");
  elements.notepad = document.getElementById("notepad");
  elements.tagList = document.getElementById("tag-list");
  elements.rowTemplate = document.getElementById("novel-row-template");
  elements.langJa = document.getElementById("lang-ja");
  elements.langEn = document.getElementById("lang-en");
}

export function selectedIds() {
  return Array.from(state.selected.values()).sort((left, right) => left - right);
}

export function pruneSelection() {
  const visibleIds = new Set(state.novels.map((novel) => novel.id));
  for (const id of Array.from(state.selected.values())) {
    if (!visibleIds.has(id)) {
      state.selected.delete(id);
    }
  }
}

export function syncSelectionUi() {
  elements.selectedCount.textContent = String(state.selected.size);
  const selectableCount = state.novels.length;
  elements.selectAll.checked = selectableCount > 0 && state.selected.size === selectableCount;
}
