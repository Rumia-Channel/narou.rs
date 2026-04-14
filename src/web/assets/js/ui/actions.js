/**
 * Event bindings for buttons, navbar actions, console, modals, and all menu items.
 * Mirrors narou.rb's full set of UI interactions.
 */
import { State, El, lsSet, lsBool } from '../core/state.js';
import { fetchJson, postJson } from '../core/http.js';
import { toggleLanguage } from './i18n.js';
import {
  renderNovelList, renderTagList, renderQueueStatus,
  selectVisible, selectAll, clearSelection, updateEnableSelected,
  syncViewChecks, showNotification,
} from './render.js';
import { setShortcutHandlers, initShortcuts } from './shortcuts.js';
import { setContextHandlers, initContextMenu, initTagColorMenu } from './context_menu.js';

export function bindActions() {
  // --- Navbar toggle (mobile) ---
  El.navbarToggleBtn?.addEventListener('click', () => {
    El.navbarCollapse?.classList.toggle('open');
  });

  // --- Console buttons ---
  El.consoleTrash?.addEventListener('click', () => {
    if (El.console) El.console.textContent = '';
    State.consoleHistory = [];
  });

  El.consoleExpand?.addEventListener('click', () => {
    El.console?.classList.toggle('expanded');
    State.consoleExpanded = !State.consoleExpanded;
    // Toggle icon
    const expand = El.consoleExpand?.querySelector('.expand-icon');
    const collapse = El.consoleExpand?.querySelector('.collapse-icon');
    if (expand) expand.classList.toggle('hide', State.consoleExpanded);
    if (collapse) collapse.classList.toggle('hide', !State.consoleExpanded);
  });

  El.consoleCancel?.addEventListener('click', async () => {
    await postJson('/api/queue/cancel', {});
  });

  El.consoleHistory?.addEventListener('click', async () => {
    try {
      const data = await fetchJson('/api/recent_logs');
      if (data?.logs && El.console) {
        El.console.textContent = data.logs;
        El.console.scrollTop = El.console.scrollHeight;
      }
    } catch { /* ignore */ }
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
  on('action-view-all', () => {
    State.viewFrozen = true;
    State.viewNonfrozen = true;
    lsSet('view-frozen', 'true');
    lsSet('view-nonfrozen', 'true');
    syncViewChecks();
    renderNovelList();
  });

  on('action-view-setting', () => {
    // TODO: column visibility configuration modal
    showNotification('表示項目設定は今後実装予定です', 'info');
  });

  on('action-view-novel-list-wide', () => {
    State.wideMode = !State.wideMode;
    lsSet('wide-mode', String(State.wideMode));
    syncViewChecks();
  });

  on('action-view-nonfrozen', () => {
    State.viewNonfrozen = !State.viewNonfrozen;
    lsSet('view-nonfrozen', String(State.viewNonfrozen));
    syncViewChecks();
    renderNovelList();
  });

  on('action-view-frozen', () => {
    State.viewFrozen = !State.viewFrozen;
    lsSet('view-frozen', String(State.viewFrozen));
    syncViewChecks();
    renderNovelList();
  });

  on('action-view-toggle-setting-new-tab', () => {
    State.settingNewTab = !State.settingNewTab;
    lsSet('setting-new-tab', String(State.settingNewTab));
    syncViewChecks();
  });

  on('action-view-toggle-buttons-top', () => {
    State.buttonsTop = !State.buttonsTop;
    lsSet('buttons-top', String(State.buttonsTop));
    const cp = El.controlPanel;
    if (cp) cp.classList.toggle('hide', !State.buttonsTop);
    syncViewChecks();
  });

  on('action-view-toggle-buttons-footer', () => {
    State.buttonsFooter = !State.buttonsFooter;
    lsSet('buttons-footer', String(State.buttonsFooter));
    syncViewChecks();
  });

  on('action-view-reset', () => {
    State.viewFrozen = false;
    State.viewNonfrozen = true;
    State.wideMode = false;
    State.settingNewTab = false;
    State.buttonsTop = true;
    State.buttonsFooter = false;
    ['view-frozen', 'view-nonfrozen', 'wide-mode',
     'setting-new-tab', 'buttons-top', 'buttons-footer'].forEach(k =>
      localStorage.removeItem('narou-rs-webui-' + k)
    );
    syncViewChecks();
    renderNovelList();
    showNotification('表示設定をリセットしました', 'info');
  });

  // --- Select menu ---
  on('action-select-view', () => selectVisible());
  on('action-select-all', () => selectAll());
  on('action-select-clear', () => clearSelection());

  on('action-select-mode-single', () => setSelectMode('single'));
  on('action-select-mode-multi', () => setSelectMode('rect'));
  on('action-select-mode-hybrid', () => setSelectMode('hybrid'));

  // --- Tag edit ---
  on('action-tag-edit', () => openTagEditor());

  // --- Tool menu ---
  on('action-tool-notepad', openNotepad);
  on('action-tool-csv-download', downloadCsv);
  on('action-tool-csv-import', () => {
    // Trigger hidden file input
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.csv';
    input.addEventListener('change', async () => {
      const file = input.files?.[0];
      if (!file) return;
      const text = await file.text();
      try {
        await postJson('/api/csv/import', { csv: text });
        showNotification('CSVインポート完了', 'success');
        await refreshList();
      } catch (e) {
        showNotification('CSVインポート失敗: ' + e.message, 'error');
      }
    });
    input.click();
  });

  // --- Options menu ---
  on('action-lang-toggle', () => {
    toggleLanguage();
    renderNovelList();
    renderTagList();
  });

  on('action-option-settings', () => {
    // Open settings page
    window.open('/settings', State.settingNewTab ? '_blank' : '_self');
  });

  on('action-option-help', () => {
    window.open('/help', '_blank');
  });

  on('action-option-about', openAbout);

  on('action-option-shutdown', async () => {
    if (!confirm('サーバをシャットダウンしますか？')) return;
    await postJson('/api/shutdown', {});
  });

  on('action-option-server-reboot', async () => {
    if (!confirm('サーバを再起動しますか？')) return;
    await postJson('/api/reboot', {});
    showNotification('サーバを再起動中...', 'info');
  });

  // Theme selection
  El.themeSelect?.addEventListener('change', () => {
    const theme = El.themeSelect.value;
    State.theme = theme;
    lsSet('theme', theme);
    document.documentElement.dataset.theme = theme === 'default' ? '' : theme;
  });

  // --- Queue display ---
  El.queueDisplay?.addEventListener('click', () => {
    El.queueModal?.classList.remove('hide');
    refreshQueue();
  });

  on('queue-modal-close', () => El.queueModal?.classList.add('hide'));
  on('queue-clear-button', async () => {
    await postJson('/api/queue/clear', {});
    await refreshQueue();
  });

  // --- Notepad modal ---
  on('notepad-close', () => El.notepadModal?.classList.add('hide'));
  on('save-notepad-button', async () => {
    const text = El.notepad?.value || '';
    await postJson('/api/notepad/save', { text });
    El.notepadModal?.classList.add('hide');
    showNotification('メモ帳を保存しました', 'success');
  });

  // --- Tag edit modal ---
  on('tag-edit-close', () => El.tagEditModal?.classList.add('hide'));
  on('tag-edit-cancel', () => El.tagEditModal?.classList.add('hide'));
  on('add-tag-button', addTagFromInput);
  El.newTagInput?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      addTagFromInput();
    }
  });

  // --- About modal ---
  on('about-close', () => El.aboutModal?.classList.add('hide'));
  on('about-ok', () => El.aboutModal?.classList.add('hide'));

  // --- Confirm modal ---
  on('confirm-cancel', () => El.confirmModal?.classList.add('hide'));

  // --- Diff modal ---
  on('diff-close', () => El.diffModal?.classList.add('hide'));

  // --- Control panel buttons ---
  on('btn-download', () => {
    const url = prompt('ダウンロードURLを入力:');
    if (url) postJson('/api/download', { targets: [url] });
  });

  on('action-download-force', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/download', { targets: [...State.selectedIds], force: true });
  });

  on('btn-update', () => {
    if (State.selectedIds.size > 0) {
      postJson('/api/update', { targets: [...State.selectedIds] });
    } else {
      postJson('/api/update', { targets: [] });
    }
  });

  on('action-update-general-lastup', () => {
    postJson('/api/update', { targets: ['--gl'] });
  });

  on('action-update-by-tag', () => {
    const tag = prompt('更新するタグ名を入力:');
    if (tag) postJson('/api/update', { targets: ['--tag', tag] });
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

  on('btn-gl-modified', () => {
    postJson('/api/update', { targets: ['--tag', 'modified'] });
  });

  on('btn-send', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/send', { targets: [...State.selectedIds] });
  });

  on('action-freeze-on', () => batchAction('/api/novels/freeze'));
  on('action-freeze-off', () => batchAction('/api/novels/unfreeze'));

  on('btn-remove', async () => {
    if (State.selectedIds.size === 0) return;
    if (!confirm('選択した小説を削除しますか？')) return;
    await batchAction('/api/novels/remove');
  });

  on('btn-convert', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/convert', { targets: [...State.selectedIds] });
  });

  on('action-other-diff', () => {
    if (State.selectedIds.size === 0) return;
    openDiffList([...State.selectedIds]);
  });

  on('action-other-inspect', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/inspect', { targets: [...State.selectedIds] });
  });

  on('action-other-folder', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/folder', { targets: [...State.selectedIds] });
  });

  on('action-other-backup', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/backup', { targets: [...State.selectedIds] });
  });

  on('action-other-setting-burn', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/setting_burn', { targets: [...State.selectedIds] });
  });

  on('action-other-mail', () => {
    if (State.selectedIds.size === 0) return;
    postJson('/api/mail', { targets: [...State.selectedIds] });
  });

  // --- Table header sort ---
  document.querySelectorAll('.sortable').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.sort;
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

  // --- Scroll-to-top ---
  const moveTop = El.moveToTop;
  if (moveTop) {
    window.addEventListener('scroll', () => {
      moveTop.classList.toggle('hide', window.scrollY < 200);
    });
    moveTop.addEventListener('click', () => {
      window.scrollTo({ top: 0, behavior: 'smooth' });
    });
  }

  // --- Context menu + keyboard shortcuts ---
  const handlers = {
    selectView: () => selectVisible(),
    selectAll: () => selectAll(),
    selectClear: () => clearSelection(),
    refreshAll: () => { refreshList(); refreshQueue(); refreshTags(); },
    toggleWide: () => {
      State.wideMode = !State.wideMode;
      lsSet('wide-mode', String(State.wideMode));
      syncViewChecks();
    },
    viewFrozen: () => {
      State.viewFrozen = !State.viewFrozen;
      lsSet('view-frozen', String(State.viewFrozen));
      syncViewChecks();
      renderNovelList();
    },
    viewNonfrozen: () => {
      State.viewNonfrozen = !State.viewNonfrozen;
      lsSet('view-nonfrozen', String(State.viewNonfrozen));
      syncViewChecks();
      renderNovelList();
    },
    selectModeSingle: () => setSelectMode('single'),
    selectModeRect: () => setSelectMode('rect'),
    selectModeHybrid: () => setSelectMode('hybrid'),
    tagEdit: () => openTagEditor(),

    // Context menu single-novel actions
    openSetting: (id) => {
      const url = `/novels/${id}/setting`;
      if (State.settingNewTab) window.open(url, '_blank');
      else window.location.href = url;
    },
    showDiff: (id) => openDiffList([id]),
    tagEditSingle: (id) => openTagEditor([id]),
    freezeToggle: async (id) => {
      const novel = State.novels.find(n => n.id === id);
      const endpoint = novel?.frozen ? '/api/novels/unfreeze' : '/api/novels/freeze';
      await postJson(endpoint, { ids: [String(id)] });
      await refreshList();
    },
    updateSingle: (id) => postJson('/api/update', { targets: [String(id)] }),
    updateForceSingle: (id) => postJson('/api/update', { targets: [String(id)], force: true }),
    sendSingle: (id) => postJson('/api/send', { targets: [String(id)] }),
    removeSingle: async (id) => {
      const novel = State.novels.find(n => n.id === id);
      if (!confirm(`「${novel?.title || id}」を削除しますか？`)) return;
      await postJson('/api/novels/remove', { ids: [String(id)] });
      await refreshList();
    },
    convertSingle: (id) => postJson('/api/convert', { targets: [String(id)] }),
    inspectSingle: (id) => postJson('/api/inspect', { targets: [String(id)] }),
    folderSingle: (id) => postJson('/api/folder', { targets: [String(id)] }),
    backupSingle: (id) => postJson('/api/backup', { targets: [String(id)] }),
    downloadForceSingle: (id) => postJson('/api/download', { targets: [String(id)], force: true }),
    mailSingle: (id) => postJson('/api/mail', { targets: [String(id)] }),
    refreshTags: () => refreshTags(),
    refreshList: () => refreshList(),
  };

  setShortcutHandlers(handlers);
  setContextHandlers(handlers);
  initShortcuts();
  initContextMenu();
  initTagColorMenu();

  // Initial sync
  syncViewChecks();

  // Apply theme
  if (State.theme && State.theme !== 'default') {
    document.documentElement.dataset.theme = State.theme;
    if (El.themeSelect) El.themeSelect.value = State.theme;
  }
}

/* ===== Helpers ===== */

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

function setSelectMode(mode) {
  State.selectMode = mode;
  lsSet('select-mode', mode);
  syncViewChecks();
}

async function batchAction(endpoint) {
  if (State.selectedIds.size === 0) return;
  await postJson(endpoint, { ids: [...State.selectedIds] });
  await refreshList();
}

/* ===== Tag editor ===== */

function openTagEditor(ids) {
  const targetIds = ids || [...State.selectedIds];
  if (targetIds.length === 0) return;

  El.tagEditModal?.classList.remove('hide');
  El.tagEditModal.dataset.ids = JSON.stringify(targetIds);

  // Show current tags of first selected novel
  const firstId = targetIds[0];
  const novel = State.novels.find(n => String(n.id) === String(firstId));
  const currentTags = novel?.tags || [];

  const container = El.tagEditorCurrent;
  if (container) {
    container.innerHTML = '';
    for (const tag of currentTags) {
      const span = document.createElement('span');
      span.className = 'tag-label tag-default tag-editable';
      span.textContent = tag;
      const removeBtn = document.createElement('span');
      removeBtn.className = 'tag-remove';
      removeBtn.textContent = '×';
      removeBtn.addEventListener('click', async () => {
        for (const id of targetIds) {
          await postJson(`/api/novels/${id}/tags/remove`, { tags: [tag] });
        }
        span.remove();
        await refreshList();
        await refreshTags();
      });
      span.appendChild(removeBtn);
      container.appendChild(span);
    }
  }

  if (El.newTagInput) {
    El.newTagInput.value = '';
    El.newTagInput.focus();
  }
}

async function addTagFromInput() {
  const input = El.newTagInput;
  if (!input) return;
  const tag = input.value.trim();
  if (!tag) return;

  const idsJson = El.tagEditModal?.dataset.ids;
  const ids = idsJson ? JSON.parse(idsJson) : [...State.selectedIds];

  for (const id of ids) {
    await postJson(`/api/novels/${id}/tags`, { tags: [tag] });
  }

  input.value = '';
  await refreshList();
  await refreshTags();

  // Re-open to refresh display
  openTagEditor(ids);
}

/* ===== Notepad ===== */

async function openNotepad() {
  const data = await fetchJson('/api/notepad/read');
  if (El.notepad) El.notepad.value = data?.text || '';
  El.notepadModal?.classList.remove('hide');
}

/* ===== About ===== */

async function openAbout() {
  try {
    const data = await fetchJson('/api/version');
    if (El.aboutVersion) {
      El.aboutVersion.textContent = data?.version || '-';
    }
  } catch { /* ignore */ }
  El.aboutModal?.classList.remove('hide');
}

/* ===== Diff list ===== */

async function openDiffList(ids) {
  const container = El.diffListContainer;
  if (!container) return;
  container.innerHTML = '<p>読み込み中...</p>';
  El.diffModal?.classList.remove('hide');

  try {
    const data = await postJson('/api/diff_list', { targets: ids });
    if (data?.diffs) {
      container.innerHTML = data.diffs.map(d =>
        `<div class="diff-entry">
          <h5>${escHtml(d.title || d.id)}</h5>
          <pre class="diff-content">${escHtml(d.content || 'No diff')}</pre>
        </div>`
      ).join('');
    } else {
      container.innerHTML = '<p>差分データがありません</p>';
    }
  } catch {
    container.innerHTML = '<p>差分の取得に失敗しました</p>';
  }
}

function escHtml(s) {
  const div = document.createElement('div');
  div.textContent = String(s);
  return div.innerHTML;
}

/* ===== CSV ===== */

async function downloadCsv() {
  const resp = await fetch('/api/csv/download');
  if (!resp.ok) return;
  const blob = await resp.blob();
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = 'novels.csv';
  a.click();
  URL.revokeObjectURL(url);
}

/* ===== Data refresh ===== */

export async function refreshList() {
  try {
    const resp = await fetchJson('/api/list');
    if (resp && Array.isArray(resp.data)) {
      State.novels = resp.data;
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
