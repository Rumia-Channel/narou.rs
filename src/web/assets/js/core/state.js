/**
 * Application state management
 */
export const State = {
  novels: [],
  selectedIds: new Set(),
  frozenIds: new Set(),
  tags: [],
  tagColors: {},
  queueStatus: { pending: 0, completed: 0, failed: 0 },
  filterText: '',
  viewMode: 'nonfrozen',
  sortCol: 1,
  sortAsc: false,
  wideMode: false,
  consoleExpanded: false,
  performanceMode: false,
  wsPort: null,
  theme: 'default',
  reloadTiming: 600,
  language: localStorage.getItem('narou-rs-webui-language') || 'ja',
};

/** Cached DOM elements */
export const El = {};

const ELEMENT_IDS = [
  'header-navbar', 'navbar-toggle-btn', 'navbar-collapse',
  'badge-selecting', 'queue-count', 'queue-display',
  'filter-input', 'filter-clear',
  'console', 'console-trash', 'console-expand',
  'novel-list-body',
  'notepad-modal', 'notepad', 'notepad-close', 'save-notepad-button',
  'queue-modal', 'queue-modal-close', 'queue-pending-detail',
  'queue-completed-detail', 'queue-failed-detail', 'queue-clear-button',
  'tag-list-canvas',
];

export function initElements() {
  for (const id of ELEMENT_IDS) {
    const key = id.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    El[key] = document.getElementById(id);
  }
}
