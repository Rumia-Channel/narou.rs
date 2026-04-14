/**
 * Event bindings for buttons, navbar actions, console, modals
 */
import { State, El } from '../core/state.js';
import { fetchJson, postJson } from '../core/http.js';
import { t, toggleLanguage, applyI18n } from './i18n.js';
import {
  renderNovelList, renderTagList, renderQueueStatus,
  selectVisible, selectAll, clearSelection,
} from './render.js';

export function bindActions() {
  // --- Navbar toggle (mobile) ---
  El.navbarToggleBtn?.addEventListener('click', () => {
    El.navbarCollapse?.classList.toggle('open');
  });

  // --- Console ---
  El.consoleTrash?.addEventListener('click', () => {
    if (El.console) El.console.textContent = '';
  });

  El.consoleExpand?.addEventListener('click', () => {
    El.console?.classList.toggle('expanded');
    State.consoleExpanded = !State.consoleExpanded;
  });

  // --- Filter ---
  El.filterInput?.addEventListener('input', () => {
    State.filterText = El.filterInput.value;
    El.filterClear?.classList.toggle('hide', !State.filterText);
    renderNovelList();
  });

  El.filterClear?.addEventListener('click', () => {
    State.filterText = '';
    if (El.filterInput) El.filterInput.value = '';
    El.filterClear?.classList.add('hide');
    renderNovelList();
  });

  // --- View menu ---
  on('action-view-all', () => { State.viewMode = 'all'; renderNovelList(); });
  on('action-view-nonfrozen', () => { State.viewMode = 'nonfrozen'; renderNovelList(); });
  on('action-view-frozen', () => { State.viewMode = 'frozen'; renderNovelList(); });
  on('action-view-wide', () => {
    State.wideMode = !State.wideMode;
    document.querySelectorAll('.container-main').forEach(el => {
      el.style.maxWidth = State.wideMode ? 'none' : '';
    });
  });

  // --- Select menu ---
  on('action-select-view', () => selectVisible());
  on('action-select-all', () => selectAll());
  on('action-select-clear', () => clearSelection());

  // --- Tag edit ---
  on('action-tag-edit', () => {
    if (State.selectedIds.size === 0) return;
    const newTag = prompt(t('tagEdit'));
    if (!newTag) return;
    batchTagAction('add', newTag);
  });

  // --- Tool menu ---
  on('action-tool-notepad', openNotepad);
  on('action-tool-csv-download', downloadCsv);

  // --- Options menu ---
  on('action-lang-toggle', () => {
    toggleLanguage();
    renderNovelList();
    renderTagList();
  });
  on('action-option-shutdown', async () => {
    if (!confirm(t('confirmShutdown'))) return;
    await postJson('/api/shutdown', {});
  });

  // --- Queue display ---
  El.queueDisplay?.addEventListener('click', () => {
    document.getElementById('queue-modal')?.classList.remove('hide');
  });

  on('queue-modal-close', () => {
    document.getElementById('queue-modal')?.classList.add('hide');
  });

  on('queue-clear-button', async () => {
    await postJson('/api/queue/clear', {});
    await refreshQueue();
  });

  // --- Notepad modal ---
  on('notepad-close', () => {
    document.getElementById('notepad-modal')?.classList.add('hide');
  });

  on('save-notepad-button', async () => {
    const text = El.notepad?.value || '';
    await postJson('/api/notepad/save', { text });
    document.getElementById('notepad-modal')?.classList.add('hide');
  });

  // --- Control panel buttons ---
  on('btn-download', () => {
    const url = prompt('Download URL:');
    if (url) postJson('/api/download', { targets: [url] });
  });

  on('btn-update', () => {
    postJson('/api/update', { targets: [] });
  });

  on('action-update-view', () => {
    const ids = getVisibleIds();
    postJson('/api/update', { targets: ids });
  });

  on('action-update-force', () => {
    postJson('/api/update', { targets: [], force: true });
  });

  on('btn-gl-narou', () => {
    postJson('/api/update', { targets: ['--gl', 'narou'] });
  });

  on('btn-gl-other', () => {
    postJson('/api/update', { targets: ['--gl', 'other'] });
  });

  on('btn-send', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/send', { targets: [...State.selectedIds] });
  });

  on('action-freeze-on', () => batchAction('/api/novels/freeze'));
  on('action-freeze-off', () => batchAction('/api/novels/unfreeze'));

  on('btn-remove', () => {
    if (State.selectedIds.size === 0) return;
    if (!confirm(t('confirmRemove'))) return;
    batchAction('/api/novels/remove');
  });

  on('btn-convert', () => {
    if (State.selectedIds.size === 0) return;
    const ids = [...State.selectedIds];
    postJson('/api/convert', { targets: ids });
  });

  on('action-other-diff', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/diff', { targets: [...State.selectedIds] });
  });

  on('action-other-folder', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/folder', { targets: [...State.selectedIds] });
  });

  on('action-other-backup', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/backup', { targets: [...State.selectedIds] });
  });

  on('action-other-mail', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/mail', { targets: [...State.selectedIds] });
  });

  // --- Table header sort ---
  document.querySelectorAll('.sortable').forEach(th => {
    th.addEventListener('click', () => {
      const col = parseInt(th.dataset.sort, 10);
      if (State.sortCol === col) {
        State.sortAsc = !State.sortAsc;
      } else {
        State.sortCol = col;
        State.sortAsc = false;
      }
      document.querySelectorAll('.sortable').forEach(h => {
        h.classList.remove('active-sort', 'sort-asc');
      });
      th.classList.add('active-sort');
      if (State.sortAsc) th.classList.add('sort-asc');
      renderNovelList();
    });
  });
}

// --- Helpers ---

function on(id, handler) {
  document.getElementById(id)?.addEventListener('click', (e) => {
    e.preventDefault();
    handler(e);
  });
}

function getVisibleIds() {
  const rows = El.novelListBody?.querySelectorAll('tr[data-id]') || [];
  return Array.from(rows).map(r => r.dataset.id);
}

async function batchAction(endpoint) {
  if (State.selectedIds.size === 0) return;
  await postJson(endpoint, { ids: [...State.selectedIds] });
  await refreshList();
}

async function batchTagAction(action, tag) {
  const ids = [...State.selectedIds];
  for (const id of ids) {
    if (action === 'add') {
      await postJson(`/api/novels/${id}/tags`, { tags: [tag] });
    }
  }
  await refreshList();
}

async function openNotepad() {
  const data = await fetchJson('/api/notepad/read');
  if (El.notepad) El.notepad.value = data?.text || '';
  document.getElementById('notepad-modal')?.classList.remove('hide');
}

async function downloadCsv() {
  const resp = await fetch('/api/csv');
  if (!resp.ok) return;
  const blob = await resp.blob();
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = 'novels.csv';
  a.click();
  URL.revokeObjectURL(url);
}

export async function refreshList() {
  try {
    const resp = await fetchJson('/api/list');
    if (resp && Array.isArray(resp.data)) {
      State.novels = resp.data;
      // Build frozen set from each record's frozen flag
      State.frozenIds = new Set(
        resp.data.filter(n => n.frozen).map(n => String(n.id))
      );
    }
  } catch { /* ignore */ }
  renderNovelList();
}

export async function refreshQueue() {
  try {
    const data = await fetchJson('/api/queue/status');
    if (data) {
      State.queueStatus = data;
      renderQueueStatus();
    }
  } catch { /* ignore */ }
}

export async function refreshTags() {
  try {
    const data = await fetchJson('/api/tag_list');
    if (data) {
      State.tags = data.tags || [];
      State.tagColors = data.colors || {};
      renderTagList();
    }
  } catch { /* ignore */ }
}
