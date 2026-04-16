/**
 * Event bindings for buttons, navbar actions, console, modals, and all menu items.
 * Mirrors narou.rb's full set of UI interactions.
 */
import { State, El, lsSet, lsBool } from '../core/state.js';
import { fetchJson, postJson } from '../core/http.js';
import { toggleLanguage } from './i18n.js';
import {
  renderNovelList, renderTagList, renderQueueStatus, renderQueueDetailed,
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
  El.consoleTrash?.addEventListener('click', async () => {
    if (El.console) {
      var lines = El.console.querySelectorAll('.console-line');
      lines.forEach(function(el) { el.remove(); });
    }
    State.consoleHistory = [];
    try { await postJson('/api/clear_history', {}); } catch { /* ignore */ }
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
    await postJson('/api/cancel', {});
  });

  El.consoleHistory?.addEventListener('click', async () => {
    try {
      const data = await fetchJson('/api/history');
      if (data?.history !== undefined && El.console) {
        var lines = El.console.querySelectorAll('.console-line');
        lines.forEach(function(el) { el.remove(); });
        var histLines = data.history.split('\n');
        histLines.forEach(function(line) {
          var div = document.createElement('div');
          div.className = 'console-line';
          div.textContent = line;
          El.console.appendChild(div);
        });
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
    setHiddenCols([]);
    applyColumnVisibility();
    syncViewChecks();
    renderNovelList();
  });

  on('action-view-setting', () => {
    openColvisModal();
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
    setHiddenCols([]);
    applyColumnVisibility();
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
  on('action-tool-notepad', () => { window.location.href = '/notepad'; });
  on('action-tool-notepad-popup', openNotepad);
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
  on('action-tool-dnd-window', () => {
    window.open('/widget/drag_and_drop', 'dnd_window',
      'width=400,height=350,menubar=no,toolbar=no,location=no,status=no,resizable=yes,scrollbars=yes');
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
    window.location.href = '/_rebooting';
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
    refreshQueueDetailed();
  });

  on('queue-modal-close', () => El.queueModal?.classList.add('hide'));
  on('queue-clear-button', async () => {
    await postJson('/api/queue/clear', {});
    await refreshQueueDetailed();
  });
  on('queue-reload-button', async () => {
    await refreshQueueDetailed();
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

  // --- Column visibility modal ---
  on('colvis-close', () => El.colvisModal?.classList.add('hide'));
  on('colvis-ok', () => {
    const cbs = El.colvisList?.querySelectorAll('input[type="checkbox"]') || [];
    const hidden = [];
    cbs.forEach(cb => { if (!cb.checked) hidden.push(cb.dataset.col); });
    setHiddenCols(hidden);
    applyColumnVisibility();
    El.colvisModal?.classList.add('hide');
  });
  on('colvis-show-all', () => {
    El.colvisList?.querySelectorAll('input[type="checkbox"]').forEach(cb => cb.checked = true);
  });
  on('colvis-hide-all', () => {
    El.colvisList?.querySelectorAll('input[type="checkbox"]').forEach(cb => cb.checked = false);
  });
  on('colvis-reset', () => {
    El.colvisList?.querySelectorAll('input[type="checkbox"]').forEach(cb => cb.checked = true);
  });

  // --- Confirm modal ---
  on('confirm-cancel', () => El.confirmModal?.classList.add('hide'));

  // --- Diff modal ---
  on('diff-close', () => El.diffModal?.classList.add('hide'));

  // --- Download modal ---
  const downloadModal = document.getElementById('download-modal');
  const downloadInput = document.getElementById('download-input');
  const downloadDropHere = document.getElementById('download-link-drop-here');

  on('btn-download', () => {
    if (downloadModal) {
      downloadInput.value = '';
      downloadModal.classList.remove('hide');
      setTimeout(() => downloadInput?.focus(), 100);
    }
  });

  on('download-modal-close', () => downloadModal?.classList.add('hide'));
  on('download-cancel', () => downloadModal?.classList.add('hide'));

  on('download-submit', () => {
    const text = downloadInput?.value?.trim();
    if (!text) return;
    const targets = text.split(/[\s\n]+/).filter(Boolean);
    if (targets.length === 0) return;
    const mail = document.getElementById('download-mail')?.checked || false;
    postJson('/api/download', { targets, mail });
    downloadModal?.classList.add('hide');
  });

  // D&D support for download modal
  if (downloadDropHere) {
    const dropArea = downloadDropHere.parentElement;
    dropArea.addEventListener('dragenter', (e) => {
      e.preventDefault();
      downloadDropHere.classList.add('dragover');
    });
    dropArea.addEventListener('dragover', (e) => {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
    });
    dropArea.addEventListener('dragleave', () => {
      downloadDropHere.classList.remove('dragover');
    });
    dropArea.addEventListener('drop', (e) => {
      e.preventDefault();
      downloadDropHere.classList.remove('dragover');
      const text = e.dataTransfer.getData('text/uri-list') || e.dataTransfer.getData('text/plain') || '';
      if (text) {
        const current = downloadInput.value;
        downloadInput.value = current ? current + '\n' + text : text;
      }
    });
  }

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
    // Restore saved checkbox state from localStorage
    const saved = JSON.parse(localStorage.getItem('gl_update_checked') || '{}');
    const cbNarou = document.getElementById('gl-update-narou');
    const cbOther = document.getElementById('gl-update-other');
    const cbModified = document.getElementById('gl-update-modified');
    if (cbNarou) cbNarou.checked = saved.narou !== undefined ? saved.narou : true;
    if (cbOther) cbOther.checked = saved.other !== undefined ? saved.other : false;
    if (cbModified) cbModified.checked = saved.updateModified !== undefined ? saved.updateModified : false;
    document.getElementById('gl-update-modal')?.classList.remove('hide');
  });

  on('gl-update-close', () => document.getElementById('gl-update-modal')?.classList.add('hide'));
  on('gl-update-cancel', () => document.getElementById('gl-update-modal')?.classList.add('hide'));
  on('gl-update-submit', () => {
    const glNarou = document.getElementById('gl-update-narou')?.checked;
    const glOther = document.getElementById('gl-update-other')?.checked;
    const isUpdateModified = document.getElementById('gl-update-modified')?.checked;
    // Save state
    localStorage.setItem('gl_update_checked', JSON.stringify({
      narou: glNarou, other: glOther, updateModified: isUpdateModified
    }));
    if (!glNarou && !glOther) {
      document.getElementById('gl-update-modal')?.classList.add('hide');
      return;
    }
    let option = (glNarou && glOther) ? 'all' : (glNarou ? 'narou' : 'other');
    postJson('/api/update_general_lastup', {
      option: option,
      is_update_modified: isUpdateModified
    });
    document.getElementById('gl-update-modal')?.classList.add('hide');
  });

  on('action-update-by-tag', async () => {
    try {
      const taginfo = await postJson('/api/taginfo.json', { ids: [0], with_exclusion: true });
      if (!Array.isArray(taginfo) || taginfo.length === 0) {
        showNotification('タグが登録されていません', 'warning');
        return;
      }
      const includeDiv = document.getElementById('update-by-tag-include');
      const excludeDiv = document.getElementById('update-by-tag-exclude');
      includeDiv.innerHTML = '';
      excludeDiv.innerHTML = '';
      taginfo.forEach(info => {
        const lbl = document.createElement('label');
        lbl.style.cssText = 'display:inline-block;margin:0.2em 0.5em;cursor:pointer';
        lbl.innerHTML = '<input type="checkbox" data-tagname="' +
          info.tag.replace(/"/g, '&quot;') + '"> ' + info.html + '&nbsp;&nbsp;';
        includeDiv.appendChild(lbl);
      });
      taginfo.forEach(info => {
        const lbl = document.createElement('label');
        lbl.style.cssText = 'display:inline-block;margin:0.2em 0.5em;cursor:pointer';
        lbl.innerHTML = '<input type="checkbox" data-exclusion-tagname="' +
          info.tag.replace(/"/g, '&quot;') + '"> ' +
          (info.exclusion_html || info.html) + '&nbsp;&nbsp;';
        excludeDiv.appendChild(lbl);
      });
      document.getElementById('update-by-tag-modal').classList.remove('hide');
    } catch (e) {
      showNotification('タグ情報の取得に失敗しました', 'error');
    }
  });

  on('update-by-tag-close', () => document.getElementById('update-by-tag-modal')?.classList.add('hide'));
  on('update-by-tag-cancel', () => document.getElementById('update-by-tag-modal')?.classList.add('hide'));
  on('update-by-tag-submit', () => {
    const tags = [];
    const exclusion_tags = [];
    document.querySelectorAll('#update-by-tag-include input[data-tagname]:checked').forEach(cb => {
      tags.push(cb.dataset.tagname);
    });
    document.querySelectorAll('#update-by-tag-exclude input[data-exclusion-tagname]:checked').forEach(cb => {
      exclusion_tags.push(cb.dataset.exclusionTagname);
    });
    if (tags.length === 0 && exclusion_tags.length === 0) {
      showNotification('タグを選択してください', 'warning');
      return;
    }
    postJson('/api/update_by_tag', { tags, exclusion_tags });
    document.getElementById('update-by-tag-modal')?.classList.add('hide');
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

  on('action-send-backup-bookmark', () => {
    postJson('/api/backup_bookmark', {});
  });

  on('action-freeze-on', () => batchAction('/api/novels/freeze'));
  on('action-freeze-off', () => batchAction('/api/novels/unfreeze'));

  on('btn-remove', async () => {
    if (State.selectedIds.size === 0) return;
    showRemoveModal([...State.selectedIds]);
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

  on('action-view-link-to-edit-menu', () => {
    window.open('/edit_menu', '_blank');
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

      // Persist sort state to server
      const reverseColMap = { id: 0, last_update: 1, general_lastup: 2, last_check_date: 3,
        title: 4, author: 5, sitename: 6, novel_type: 7, general_all_no: 9, length: 10 };
      const colIdx = reverseColMap[State.sortCol] ?? 2;
      postJson('/api/sort_state', { column: colIdx, dir: State.sortAsc ? 'asc' : 'desc' }).catch(() => {});
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
      await postJson(endpoint, { ids: [Number(id)] });
      await refreshList();
    },
    updateSingle: (id) => postJson('/api/update', { targets: [String(id)] }),
    updateForceSingle: (id) => postJson('/api/update', { targets: [String(id)], force: true }),
    sendSingle: (id) => postJson('/api/send', { targets: [String(id)] }),
    removeSingle: async (id) => {
      showRemoveModal([Number(id)]);
    },
    convertSingle: (id) => postJson('/api/convert', { targets: [String(id)] }),
    inspectSingle: (id) => postJson('/api/inspect', { targets: [String(id)] }),
    folderSingle: (id) => postJson('/api/folder', { targets: [String(id)] }),
    backupSingle: (id) => postJson('/api/backup', { targets: [String(id)] }),
    downloadForceSingle: (id) => postJson('/api/download', { targets: [String(id)], force: true }),
    mailSingle: (id) => postJson('/api/mail', { targets: [String(id)] }),
    authorComments: (id) => { window.location.href = '/novels/' + id + '/author_comments'; },
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
  applyColumnVisibility();
  updateEnableSelected();
  populateFooterPanel();

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

function populateFooterPanel() {
  const src = El.mainControlPanel;
  const dst = El.footerControlPanel;
  if (!src || !dst || dst.children.length > 0) return;
  const clone = src.cloneNode(true);
  clone.removeAttribute('id');
  // Remap IDs on cloned elements to avoid duplicate IDs;
  // use click delegation instead
  clone.querySelectorAll('[id]').forEach(el => el.removeAttribute('id'));
  while (clone.firstChild) dst.appendChild(clone.firstChild);

  // Delegate clicks from footer panel to main panel buttons by class/text
  dst.addEventListener('click', (e) => {
    const link = e.target.closest('a, button');
    if (!link) return;
    // Find the matching element in the main control panel
    const mainEl = findMainPanelMatch(link);
    if (mainEl) {
      e.preventDefault();
      mainEl.click();
    }
  });
}

function findMainPanelMatch(clonedEl) {
  const src = El.mainControlPanel;
  if (!src) return null;
  // Match by original ID attribute (stored as data-orig-id) or by text content
  const text = clonedEl.textContent.trim();
  const title = clonedEl.getAttribute('title');
  const candidates = src.querySelectorAll('a, button');
  for (const c of candidates) {
    if (title && c.getAttribute('title') === title) return c;
    if (c.textContent.trim() === text) return c;
  }
  return null;
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
  await postJson(endpoint, { ids: [...State.selectedIds].map(Number) });
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
    const data = await fetchJson('/api/version/current.json');
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
        `<div class="diff-entry" data-diff-id="${escHtml(String(d.id))}">
          <div class="diff-header">
            <h5>${escHtml(d.title || d.id)}</h5>
            <button class="btn btn-sm btn-diff-clean" data-id="${escHtml(String(d.id))}" title="差分キャッシュを削除">🗑 クリア</button>
          </div>
          <pre class="diff-content">${escHtml(d.content || 'No diff')}</pre>
        </div>`
      ).join('');
      container.querySelectorAll('.btn-diff-clean').forEach(btn => {
        btn.addEventListener('click', async () => {
          const id = btn.dataset.id;
          await postJson('/api/diff_clean', { target: id });
          const entry = btn.closest('.diff-entry');
          if (entry) {
            const pre = entry.querySelector('.diff-content');
            if (pre) pre.textContent = 'No diff';
          }
        });
      });
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

/* ===== Remove Confirm Modal ===== */

function showRemoveModal(ids) {
  if (!ids || ids.length === 0) return;
  // Build novel title list
  const items = ids.map(id => {
    const n = State.novels.find(n => n.id === id);
    return '<li>' + escHtml(n?.title || String(id)) + '</li>';
  }).join('');
  El.removeNovelList.innerHTML = '<ul>' + items + '</ul>';
  El.removeWithFile.checked = false;
  El.removeModal?.classList.remove('hide');

  // One-shot handlers
  const cleanup = () => {
    El.removeModal?.classList.add('hide');
    El.removeOk.removeEventListener('click', onOk);
    El.removeCancel.removeEventListener('click', onCancel);
  };
  const onOk = async () => {
    const withFile = El.removeWithFile.checked;
    cleanup();
    await postJson('/api/novels/remove', { ids: ids, with_file: withFile });
    await refreshList();
  };
  const onCancel = () => cleanup();
  El.removeOk.addEventListener('click', onOk);
  El.removeCancel.addEventListener('click', onCancel);
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

/* ===== Column Visibility ===== */

const COLVIS_COLUMNS = [
  { cls: 'col-id', label: 'ID' },
  { cls: 'col-update', label: '更新日' },
  { cls: 'col-general-lastup', label: '最新話掲載日' },
  { cls: 'col-last-check', label: '更新チェック日' },
  { cls: 'col-author', label: '作者名' },
  { cls: 'col-site', label: '掲載' },
  { cls: 'col-novel-type', label: '種別' },
  { cls: 'col-tags', label: 'タグ' },
  { cls: 'col-episodes', label: '話数' },
  { cls: 'col-length', label: '文字数' },
  { cls: 'col-status', label: '状態' },
  { cls: 'col-url', label: 'リンク' },
  { cls: 'col-story', label: 'あらすじ' },
  { cls: 'col-menu', label: '個別' },
];

// title is always visible — not in the list
const COLVIS_DEFAULT = COLVIS_COLUMNS.map(c => c.cls);

function getHiddenCols() {
  const raw = localStorage.getItem('narou-rs-webui-hidden-cols');
  if (!raw) return [];
  try { return JSON.parse(raw); } catch { return []; }
}

function setHiddenCols(arr) {
  localStorage.setItem('narou-rs-webui-hidden-cols', JSON.stringify(arr));
}

function applyColumnVisibility() {
  const hidden = new Set(getHiddenCols());
  const style = document.getElementById('colvis-style') || (() => {
    const s = document.createElement('style');
    s.id = 'colvis-style';
    document.head.appendChild(s);
    return s;
  })();
  if (hidden.size === 0) {
    style.textContent = '';
    return;
  }
  style.textContent = [...hidden].map(cls =>
    `.${cls} { display: none !important; }`
  ).join('\n');
}

function openColvisModal() {
  const list = El.colvisList;
  if (!list) return;
  list.innerHTML = '';
  const hidden = new Set(getHiddenCols());

  for (const col of COLVIS_COLUMNS) {
    const li = document.createElement('li');
    const label = document.createElement('label');
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = !hidden.has(col.cls);
    cb.dataset.col = col.cls;
    label.appendChild(cb);
    label.appendChild(document.createTextNode(col.label));
    li.appendChild(label);
    list.appendChild(li);
  }

  El.colvisModal?.classList.remove('hide');
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

export async function refreshQueueDetailed() {
  try {
    const data = await fetchJson('/api/get_pending_tasks');
    if (data) {
      State.queueDetailed = data;
      renderQueueDetailed();
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
