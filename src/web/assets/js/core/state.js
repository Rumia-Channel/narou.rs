/**
 * Application state management
 */

const LS_PREFIX = 'narou-rs-webui-';

function lsGet(key, fallback) {
  try { const v = localStorage.getItem(LS_PREFIX + key); return v !== null ? v : fallback; }
  catch { return fallback; }
}

function lsSet(key, value) {
  try { localStorage.setItem(LS_PREFIX + key, value); } catch { /* quota */ }
}

function lsBool(key, fallback) {
  const v = lsGet(key, null);
  return v === null ? fallback : v === 'true';
}

function lsInt(key, fallback) {
  const raw = lsGet(key, '');
  const parsed = Number.parseInt(raw, 10);
  return Number.isFinite(parsed) ? parsed : fallback;
}

export { lsGet, lsSet, lsBool, lsInt };

export const State = {
  novels: [],
  selectedIds: new Set(),
  frozenIds: new Set(),
  tags: [],
  tagColors: {},
  queueStatus: { pending: 0, completed: 0, failed: 0, running: null },
  queueDetailed: { pending: [], running: [], pending_count: 0, running_count: 0 },
  queueRestorePrompted: false,
  queueRestoreCheckPending: true,
  filterText: '',

  // View flags (persisted to localStorage)
  viewFrozen: lsBool('view-frozen', true),
  viewNonfrozen: lsBool('view-nonfrozen', true),
  wideMode: lsBool('wide-mode', false),
  settingNewTab: lsBool('setting-new-tab', false),
  buttonsTop: lsBool('buttons-top', true),
  buttonsFooter: lsBool('buttons-footer', false),

  // Selection
  selectMode: lsGet('select-mode', 'hybrid'),

  // Sort
  sortCol: 'last_update',
  sortAsc: false,
  pageLength: lsInt('page-length', 50),
  currentPage: 1,

  // Console
  consoleExpanded: false,
  consoleHistory: [],
  consolePinned: { main: true, stdout2: true },
  concurrencyEnabled: false,

  // Config from server
  performanceMode: 'auto',
  tableReloadTiming: 'every',
  wsPort: null,
  theme: lsGet('theme', 'default'),
  pollIntervalSeconds: 600,
  language: lsGet('language', 'ja'),
};

/** Cached DOM elements */
export const El = {};

const ELEMENT_IDS = [
  'header-navbar', 'navbar-toggle-btn', 'navbar-collapse',
  'badge-selecting', 'queue-count', 'queue-display', 'queue-sizes',
  'filter-input', 'filter-clear', 'filter-search-icon',
  'console', 'console-stdout2', 'console-col-right',
  'console-cancel', 'console-history',
  'console-trash', 'console-expand', 'console-buttons',
  'main-control-panel', 'footer-control-panel', 'footer-navbar',
  'novel-list-body', 'novel-list', 'novel-list-container',
  'novel-list-length', 'novel-list-info', 'novel-list-pagination',
  'control-panel',
  'notepad-modal', 'notepad', 'notepad-close', 'save-notepad-button',
  'queue-modal', 'queue-modal-close', 'queue-clear-button', 'queue-reload-button',
  'queue-running-list', 'queue-pending-list', 'queue-pending-count',
  'queue-restore-modal', 'queue-restore-yes', 'queue-restore-no',
  'tag-list-canvas',
  'tag-edit-modal', 'tag-edit-close', 'tag-edit-cancel',
  'tag-editor-current', 'new-tag-input', 'add-tag-button',
  'about-modal', 'about-close', 'about-ok', 'about-version',
  'about-latest-version', 'about-check-latest',
  'confirm-modal', 'confirm-title', 'confirm-message',
  'confirm-cancel', 'confirm-ok',
  'remove-modal', 'remove-novel-list', 'remove-with-file',
  'remove-cancel', 'remove-ok',
  'diff-modal', 'diff-close', 'diff-list-container',
  'colvis-modal', 'colvis-close', 'colvis-ok', 'colvis-list',
  'colvis-show-all', 'colvis-hide-all', 'colvis-reset',
  'context-menu', 'select-color-menu',
  'theme-select',
  'notification-container',
  'move-to-top',
];

export function initElements() {
  for (const id of ELEMENT_IDS) {
    const key = id.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    El[key] = document.getElementById(id);
  }
}
