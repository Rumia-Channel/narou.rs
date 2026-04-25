/**
 * Event bindings for buttons, navbar actions, console, modals, and all menu items.
 * Mirrors narou.rb's full set of UI interactions.
 */
import { State, El, lsSet, lsBool } from '../core/state.js';
import { fetchJson, postJson } from '../core/http.js';
import { toggleLanguage } from './i18n.js';
import {
  renderNovelList, renderTagList, renderQueueStatus, renderQueueDetailed,
  selectVisible, selectAll, clearSelection, getSelectedIdsInDisplayOrder,
  pruneSelectedIdsToCurrentList, updateEnableSelected,
  syncViewChecks, showNotification,
} from './render.js';
import { setShortcutHandlers, initShortcuts } from './shortcuts.js';
import {
  setContextHandlers, initContextMenu, initTagColorMenu,
  getStoredMenuStyle, setStoredMenuStyle,
} from './context_menu.js';

const REBOOT_RETURN_TO_KEY = 'narou-rs-webui-reboot-return-to';
let tagSuggestionIndex = -1;
const TAG_COLOR_MAP = {
  green: 'tag-green',
  yellow: 'tag-yellow',
  blue: 'tag-blue',
  magenta: 'tag-magenta',
  cyan: 'tag-cyan',
  red: 'tag-red',
  white: 'tag-white',
};
const SORT_STATE_COLUMN_INDEX = {
  id: 0,
  last_update: 1,
  general_lastup: 2,
  last_check_date: 3,
  title: 4,
  author: 5,
  sitename: 6,
  novel_type: 7,
  general_all_no: 9,
  length: 10,
};

export function bindActions() {
  applyColumnVisibility();

  // --- Navbar toggle (mobile) ---
  El.navbarToggleBtn?.addEventListener('click', () => {
    El.navbarCollapse?.classList.toggle('open');
  });

  // --- Console buttons ---
  El.consoleTrash?.addEventListener('click', async () => {
    try {
      const result = await postJson('/api/clear_history', {});
      assertApiSuccess(result, '履歴のクリアに失敗しました');
      clearConsoleHistoryUi();
      showNotification(result.message || '履歴を消去しました', 'success');
    } catch (error) {
      showNotification(error.message || '履歴のクリアに失敗しました', 'error');
    }
  });

  El.consoleExpand?.addEventListener('click', () => {
    El.console?.classList.toggle('expanded');
    El.consoleStdout2?.classList.toggle('expanded');
    State.consoleExpanded = !State.consoleExpanded;
    // Toggle icon
    const expand = El.consoleExpand?.querySelector('.expand-icon');
    const collapse = El.consoleExpand?.querySelector('.collapse-icon');
    if (expand) expand.classList.toggle('hide', State.consoleExpanded);
    if (collapse) collapse.classList.toggle('hide', !State.consoleExpanded);
  });

  El.consoleCancel?.addEventListener('click', async () => {
    await runGuardedAction(El.consoleCancel, async () => {
      await postJson('/api/cancel', {});
    }, '処理の中断に失敗しました');
  });

  El.consoleHistory?.addEventListener('click', async () => {
    try {
      const mainData = await fetchJson('/api/history?format=json');
      assertHistoryPayload(mainData);
      let subData = null;
      if (State.concurrencyEnabled && El.consoleStdout2) {
        subData = await fetchJson('/api/history?format=json&stream=stdout2');
        assertHistoryPayload(subData);
      }
      if (El.console) {
        replaceConsoleHistory(El.console, mainData.history);
      }
      if (subData && El.consoleStdout2) {
        replaceConsoleHistory(El.consoleStdout2, subData.history);
      }
    } catch (error) {
      showNotification(error.message || '履歴の取得に失敗しました', 'error');
    }
  });

  // --- Filter ---
  El.filterInput?.addEventListener('input', () => {
    State.filterText = El.filterInput.value;
    State.currentPage = 1;
    El.filterClear?.classList.toggle('hide', !State.filterText);
    renderNovelList();
  });

  El.filterClear?.addEventListener('click', () => {
    State.filterText = '';
    State.currentPage = 1;
    if (El.filterInput) El.filterInput.value = '';
    El.filterClear?.classList.add('hide');
    renderNovelList();
  });

  El.novelListLength?.addEventListener('change', () => {
    const next = Number.parseInt(El.novelListLength.value, 10);
    State.pageLength = Number.isFinite(next) ? next : 50;
    State.currentPage = 1;
    lsSet('page-length', String(State.pageLength));
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
    applyColumnVisibility();
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
    State.viewFrozen = true;
    State.viewNonfrozen = true;
    State.wideMode = false;
    State.settingNewTab = false;
    State.buttonsTop = true;
    State.buttonsFooter = false;
    ['view-frozen', 'view-nonfrozen', 'wide-mode',
      'setting-new-tab', 'buttons-top', 'buttons-footer'].forEach(k =>
      localStorage.removeItem('narou-rs-webui-' + k)
    );
    localStorage.removeItem('menu_style');
    clearHiddenColsPreference();
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
  on('action-tag-edit', (e) => {
    void withButtonGuard(e.currentTarget, async () => {
      await openTagEditor();
    });
  });

  // --- Tool menu ---
  on('action-tool-notepad', () => { window.location.href = '/notepad'; });
  on('action-tool-notepad-popup', openNotepad);
  on('action-tool-csv-download', downloadCsv);
  on('action-tool-csv-import', (e) => {
    const triggerEl = e.currentTarget;
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.csv';
    input.addEventListener('change', async () => {
      const file = input.files?.[0];
      if (!file) return;
      const text = await file.text();
      await runGuardedAction(triggerEl, async () => {
        const result = await postJson('/api/csv/import', { csv: text });
        assertApiSuccess(result, 'CSVインポートに失敗しました');
        showNotification(result.message || 'CSVインポート完了', 'success');
        await refreshList();
      }, 'CSVインポートに失敗しました');
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
    rememberRebootReturnTo();
    try {
      const result = await postJson('/api/reboot', {});
      assertApiSuccess(result, 'サーバの再起動に失敗しました');
      showNotification(result.message || 'サーバを再起動しています', 'success');
      window.location.href = '/_rebooting';
    } catch (error) {
      clearRebootReturnTo();
      showNotification(error.message || 'サーバの再起動に失敗しました', 'error');
    }
  });

  // Theme selection
  El.themeSelect?.addEventListener('change', async () => {
    const previousTheme = State.theme || 'default';
    const theme = El.themeSelect.value;
    State.theme = theme;
    lsSet('theme', theme);
    document.documentElement.dataset.theme = theme === 'default' ? '' : theme;
    try {
      const result = await postJson('/api/global_setting', {
        settings: { 'webui.theme': theme === 'default' ? null : theme },
      });
      assertApiSuccess(result, 'テーマ設定の保存に失敗しました');
    } catch (error) {
      State.theme = previousTheme;
      lsSet('theme', previousTheme);
      document.documentElement.dataset.theme = previousTheme === 'default' ? '' : previousTheme;
      if (El.themeSelect) El.themeSelect.value = previousTheme;
      showNotification(error.message || 'テーマ設定の保存に失敗しました', 'error');
    }
  });

  // --- Queue display ---
  El.queueDisplay?.addEventListener('click', () => {
    El.queueModal?.classList.remove('hide');
    refreshQueueDetailed();
  });

  on('queue-modal-close', () => El.queueModal?.classList.add('hide'));
  on('queue-clear-button', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/queue/clear', {});
      assertApiSuccess(result, 'キューの消去に失敗しました');
      await refreshQueue();
      await refreshQueueDetailed();
    }, 'キューの消去に失敗しました');
  });
  on('queue-reload-button', async () => {
    await refreshQueueDetailed();
  });

  // --- Notepad modal ---
  on('notepad-close', () => El.notepadModal?.classList.add('hide'));
  on('save-notepad-button', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const text = El.notepad?.value || '';
      const result = await postJson('/api/notepad/save', {
        text,
        content: text,
        object_id: State.notepadObjectId,
      });
      if (result?.conflict) {
        await reloadNotepadFromServer(
          result.message || '他の画面で変更されたため、メモ帳を再読み込みしました',
          'warning'
        );
        return null;
      }
      assertApiSuccess(result, 'メモ帳の保存に失敗しました');
      applyNotepadSnapshot(result);
      El.notepadModal?.classList.add('hide');
      showNotification(result.message || 'メモ帳を保存しました', 'success');
      return result;
    }, 'メモ帳の保存に失敗しました');
  });

  // --- Tag edit modal ---
  on('tag-edit-close', closeTagEditor);
  on('tag-edit-cancel', closeTagEditor);
  on('add-tag-button', addTagFromInput);
  El.newTagInput?.addEventListener('input', renderTagSuggestions);
  El.newTagInput?.addEventListener('focus', renderTagSuggestions);
  El.newTagInput?.addEventListener('blur', () => {
    setTimeout(hideTagSuggestions, 120);
  });
  El.newTagInput?.addEventListener('keydown', (e) => {
    if (handleTagSuggestionKeydown(e)) return;
    if (e.key === 'Enter') {
      e.preventDefault();
      addTagFromInput();
    }
  });

  // --- About modal ---
  on('about-close', () => El.aboutModal?.classList.add('hide'));
  on('about-ok', () => El.aboutModal?.classList.add('hide'));
  on('about-check-latest', async () => {
    await updateLatestVersionInfo();
  });

  on('queue-restore-yes', async () => {
    El.queueRestoreModal?.classList.add('hide');
    await postJson('/api/restore_pending_tasks', {});
    await refreshQueueDetailed();
  });
  on('queue-restore-no', async () => {
    El.queueRestoreModal?.classList.add('hide');
    await postJson('/api/defer_restore_pending_tasks', {});
    await refreshQueueDetailed();
  });

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
    resetColvisCheckboxesToDefault();
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

  on('download-submit', (e) => {
    const triggerEl = e.currentTarget;
    const text = downloadInput?.value?.trim();
    if (!text) return;
    const targets = text.split(/[\s\n]+/).filter(Boolean);
    if (targets.length === 0) return;
    const mail = document.getElementById('download-mail')?.checked || false;
    void runGuardedAction(triggerEl, async () => {
      const result = await postJson('/api/download', { targets, mail });
      assertApiSuccess(result, 'ダウンロード要求の送信に失敗しました');
      downloadModal?.classList.add('hide');
    }, 'ダウンロード要求の送信に失敗しました');
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

  on('action-download-force', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/download', { targets: ids, force: true });
      assertApiSuccess(result, '強制ダウンロード要求の送信に失敗しました');
    }, '強制ダウンロード要求の送信に失敗しました');
  });

  on('btn-update', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const result = State.selectedIds.size > 0
        ? await postJson('/api/update', {
          targets: [...State.selectedIds],
          ...currentSortStatePayload(),
        })
        : await postJson('/api/update', { update_all: true });
      assertApiSuccess(result, 'アップデート要求の送信に失敗しました');
    }, 'アップデート要求の送信に失敗しました');
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
  const queueGeneralLastupUpdate = async (option, isUpdateModified = false) => {
    const result = await postJson('/api/update_general_lastup', {
      option,
      is_update_modified: isUpdateModified
    });
    assertApiSuccess(result, '最新話掲載日確認の要求送信に失敗しました');
  };

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
    queueGeneralLastupUpdate(option, isUpdateModified);
    document.getElementById('gl-update-modal')?.classList.add('hide');
  });

  on('action-update-by-tag', (e) => {
    void withButtonGuard(e.currentTarget, async () => {
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
            escAttr(info.tag) + '"> ' + info.html + '&nbsp;&nbsp;';
          includeDiv.appendChild(lbl);
        });
        taginfo.forEach(info => {
          const lbl = document.createElement('label');
          lbl.style.cssText = 'display:inline-block;margin:0.2em 0.5em;cursor:pointer';
          lbl.innerHTML = '<input type="checkbox" data-exclusion-tagname="' +
            escAttr(info.tag) + '"> ' +
            (info.exclusion_html || info.html) + '&nbsp;&nbsp;';
          excludeDiv.appendChild(lbl);
        });
        document.getElementById('update-by-tag-modal').classList.remove('hide');
      } catch {
        showNotification('タグ情報の取得に失敗しました', 'error');
      }
    });
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
    postJson('/api/update_by_tag', { tags, exclusion_tags, ...currentSortStatePayload() });
    document.getElementById('update-by-tag-modal')?.classList.add('hide');
  });

  on('action-update-view', (e) => {
    const ids = getVisibleIds();
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/update', { targets: ids, ...currentSortStatePayload() });
      assertApiSuccess(result, '表示中小説のアップデート要求に失敗しました');
    }, '表示中小説のアップデート要求に失敗しました');
  });

  on('action-update-force', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/update', { update_all: true, force: true });
      assertApiSuccess(result, '強制アップデート要求の送信に失敗しました');
    }, '強制アップデート要求の送信に失敗しました');
  });

  on('btn-gl-narou', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      await queueGeneralLastupUpdate('narou');
    }, '最新話掲載日確認の要求送信に失敗しました');
  });

  on('btn-gl-other', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      await queueGeneralLastupUpdate('other');
    }, '最新話掲載日確認の要求送信に失敗しました');
  });

  on('btn-gl-modified', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/update_by_tag', {
        tags: ['modified'],
        exclusion_tags: [],
        ...currentSortStatePayload(),
      });
      assertApiSuccess(result, 'modifiedタグ更新の要求送信に失敗しました');
    }, 'modifiedタグ更新の要求送信に失敗しました');
  });

  on('btn-send', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/send', { targets: ids });
      assertApiSuccess(result, '送信要求の送信に失敗しました');
    }, '送信要求の送信に失敗しました');
  });

  on('action-send-backup-bookmark', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/backup_bookmark', {});
      assertApiSuccess(result, 'しおりバックアップ要求の送信に失敗しました');
    }, 'しおりバックアップ要求の送信に失敗しました');
  });

  on('action-freeze-on', (e) => {
    void batchAction('/api/novels/freeze', e.currentTarget, '凍結に失敗しました');
  });
  on('action-freeze-off', (e) => {
    void batchAction('/api/novels/unfreeze', e.currentTarget, '凍結解除に失敗しました');
  });

  on('btn-remove', async () => {
    const ids = requireSelectedIds();
    if (!ids) return;
    showRemoveModal(ids);
  });

  on('btn-convert', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/convert', { targets: ids, ...currentSortStatePayload() });
      assertApiSuccess(result, '変換要求の送信に失敗しました');
    }, '変換要求の送信に失敗しました');
  });

  on('action-other-diff', () => {
    const ids = requireSelectedIds();
    if (!ids) return;
    openDiffList(ids);
  });

  on('action-other-inspect', () => {
    const ids = requireSelectedIds();
    if (!ids) return;
    postJson('/api/inspect', { targets: ids });
  });

  on('action-other-folder', () => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void openFolderTargets(ids);
  });

  on('action-other-backup', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/backup', { targets: ids });
      assertApiSuccess(result, 'バックアップ要求の送信に失敗しました');
    }, 'バックアップ要求の送信に失敗しました');
  });

  on('action-other-setting-burn', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/setting_burn', { targets: ids });
      assertApiSuccess(result, '設定焼き込み要求の送信に失敗しました');
    }, '設定焼き込み要求の送信に失敗しました');
  });

  on('action-other-mail', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/mail', { targets: ids });
      assertApiSuccess(result, 'メール送信要求の送信に失敗しました');
    }, 'メール送信要求の送信に失敗しました');
  });

  on('action-view-link-to-edit-menu', () => {
    window.open('/edit_menu', '_blank');
  });

  on('action-view-select-menu-style', openMenuStyleModal);
  on('menu-style-close', closeMenuStyleModal);
  on('menu-style-cancel', closeMenuStyleModal);
  on('menu-style-save', () => {
    const checked = document.querySelector('input[name="menu-style"]:checked');
    setStoredMenuStyle(checked?.value || 'windows');
    closeMenuStyleModal();
    showNotification('個別メニューの表示スタイルを保存しました', 'success');
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
      State.currentPage = 1;
      document.querySelectorAll('.sortable').forEach(h => {
        h.classList.remove('active-sort', 'sort-asc');
      });
      th.classList.add('active-sort');
      if (State.sortAsc) th.classList.add('sort-asc');
      renderNovelList();

      // Persist sort state to server
      const { sort_state } = currentSortStatePayload();
      if (sort_state) {
        postJson('/api/sort_state', sort_state).catch(() => {});
      }
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
      applyColumnVisibility();
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
      await postJson('/api/freeze', { ids: [Number(id)] });
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
    folderSingle: (id) => openFolderTargets([String(id)]),
    backupSingle: (id) => postJson('/api/backup', { targets: [String(id)] }),
    downloadForceSingle: (id) => postJson('/api/download', { targets: [String(id)], force: true }),
    mailSingle: (id) => postJson('/api/mail', { targets: [String(id)] }),
    authorComments: (id) => {
      const popup = window.open(
        '/novels/' + id + '/author_comments',
        'author_comments_' + id,
        'width=760,height=640,menubar=no,toolbar=no,location=no,status=no,resizable=yes,scrollbars=yes'
      );
      if (!popup) window.location.href = '/novels/' + id + '/author_comments';
    },
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

async function withButtonGuard(buttonEl, action) {
  if (!(buttonEl instanceof HTMLElement)) {
    return action();
  }
  const supportsDisabled = 'disabled' in buttonEl;
  if (
    buttonEl.dataset.busy === 'true'
    || buttonEl.classList.contains('disabled')
    || buttonEl.getAttribute('aria-disabled') === 'true'
    || (supportsDisabled && buttonEl.disabled)
  ) {
    return null;
  }

  const hadDisabledClass = buttonEl.classList.contains('disabled');
  const previousAriaDisabled = buttonEl.getAttribute('aria-disabled');
  const previousTabIndex = buttonEl.getAttribute('tabindex');
  const previousDisabled = supportsDisabled ? buttonEl.disabled : false;

  buttonEl.dataset.busy = 'true';
  buttonEl.classList.add('disabled');
  buttonEl.setAttribute('aria-disabled', 'true');
  if (supportsDisabled) {
    buttonEl.disabled = true;
  }
  if (buttonEl.tagName === 'A') {
    buttonEl.setAttribute('tabindex', '-1');
  }

  try {
    return await action();
  } finally {
    delete buttonEl.dataset.busy;
    if (supportsDisabled) {
      buttonEl.disabled = previousDisabled;
    }
    if (!hadDisabledClass) {
      buttonEl.classList.remove('disabled');
    }
    if (previousAriaDisabled === null) {
      buttonEl.removeAttribute('aria-disabled');
    } else {
      buttonEl.setAttribute('aria-disabled', previousAriaDisabled);
    }
    if (buttonEl.tagName === 'A') {
      if (previousTabIndex === null) {
        buttonEl.removeAttribute('tabindex');
      } else {
        buttonEl.setAttribute('tabindex', previousTabIndex);
      }
    }
  }
}

export async function runGuardedAction(buttonEl, action, errorMessage) {
  try {
    return await withButtonGuard(buttonEl, action);
  } catch (error) {
    showNotification(error.message || errorMessage, 'error');
    return null;
  }
}

function assertApiSuccess(result, fallbackMessage) {
  if (result && result.success === false) {
    throw new Error(result.message || fallbackMessage);
  }
  return result;
}

function assertHistoryPayload(result) {
  if (!result || typeof result.history !== 'string') {
    throw new Error('履歴の取得結果が不正です');
  }
  return result;
}

function rememberRebootReturnTo() {
  if (window.location.pathname === '/_rebooting') return;
  try {
    sessionStorage.setItem(
      REBOOT_RETURN_TO_KEY,
      window.location.pathname + window.location.search + window.location.hash
    );
  } catch {
    // Ignore storage errors and fall back to root.
  }
}

function getNotepadText(data) {
  if (data && typeof data === 'object') {
    return data.text || data.content || '';
  }
  return typeof data === 'string' ? data : '';
}

function getNotepadObjectId(data) {
  if (!data || typeof data !== 'object') return null;
  return typeof data.object_id === 'string' ? data.object_id : null;
}

export function applyNotepadSnapshot(data, options = {}) {
  const text = getNotepadText(data);
  const objectId = getNotepadObjectId(data);
  const keepLocalEdits = options.keepLocalEdits === true
    && El.notepad instanceof HTMLTextAreaElement
    && document.activeElement === El.notepad
    && El.notepad.value !== text;

  if (!keepLocalEdits && El.notepad) {
    El.notepad.value = text;
  }
  if (!keepLocalEdits || !El.notepad || El.notepad.value === text) {
    State.notepadObjectId = objectId;
  }

  return { text, objectId, keptLocalEdits: keepLocalEdits };
}

async function reloadNotepadFromServer(message, type = 'warning') {
  const data = await fetchJson('/api/notepad/read');
  applyNotepadSnapshot(data);
  if (message) {
    showNotification(message, type);
  }
  return data;
}

function clearRebootReturnTo() {
  try {
    sessionStorage.removeItem(REBOOT_RETURN_TO_KEY);
  } catch {
    // Ignore storage errors.
  }
}

async function openFolderTargets(ids) {
  try {
    const result = await postJson('/api/folder', { targets: ids });
    assertApiSuccess(result, '保存フォルダを開けませんでした');
    if (result?.message) {
      showNotification(result.message, 'success');
    }
    return result;
  } catch (error) {
    showNotification(error.message || '保存フォルダを開けませんでした', 'error');
    throw error;
  }
}

function clearConsoleHistoryUi() {
  if (El.console) {
    var lines = El.console.querySelectorAll('.console-line');
    lines.forEach(function(el) { el.remove(); });
    State.consolePinned.main = true;
  }
  if (El.consoleStdout2) {
    var lines2 = El.consoleStdout2.querySelectorAll('.console-line');
    lines2.forEach(function(el) { el.remove(); });
    State.consolePinned.stdout2 = true;
  }
  State.consoleHistory = [];
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

async function batchAction(endpoint, triggerEl, errorMessage = '操作に失敗しました') {
  const ids = requireSelectedIds();
  if (!ids) return;
  await runGuardedAction(triggerEl, async () => {
    const result = await postJson(endpoint, { ids: ids.map(Number) });
    assertApiSuccess(result, errorMessage);
    await refreshList();
  }, errorMessage);
}

function requireSelectedIds() {
  const ids = getSelectedIdsInDisplayOrder();
  if (ids.length > 0) return ids;
  showNotification('小説を選択してください', 'warning');
  return null;
}

function currentSortStatePayload() {
  const column = SORT_STATE_COLUMN_INDEX[State.sortCol];
  if (column == null) {
    return {
      timestamp: Date.now(),
    };
  }
  return {
    sort_state: {
      column,
      dir: State.sortAsc ? 'asc' : 'desc',
    },
    timestamp: Date.now(),
  };
}

/* ===== Tag editor ===== */

function closeTagEditor() {
  hideTagSuggestions();
  El.tagEditModal?.classList.add('hide');
}

async function openTagEditor(ids) {
  const targetIds = ids || requireSelectedIds();
  if (!targetIds || targetIds.length === 0) return;

  El.tagEditModal?.classList.remove('hide');
  El.tagEditModal.dataset.ids = JSON.stringify(targetIds);
  El.tagEditModal.dataset.selectedCount = String(targetIds.length);
  renderTagEditorSummary(targetIds.length, { loading: true });
  renderTagEditorTags(targetIds, []);

  if (El.newTagInput) {
    El.newTagInput.value = '';
    El.newTagInput.focus();
  }
  hideTagSuggestions();

  try {
    await refreshTagEditor(targetIds);
  } catch (error) {
    renderTagEditorSummary(targetIds.length, { error: true });
    renderTagEditorTags(targetIds, []);
    showNotification(error.message || 'タグ情報の取得に失敗しました', 'error');
  }
}

async function addTagFromInput() {
  const input = El.newTagInput;
  if (!input) return;
  const tags = splitTagInput(input.value);
  if (tags.length === 0) return;

  const idsJson = El.tagEditModal?.dataset.ids;
  const ids = idsJson ? JSON.parse(idsJson) : [...State.selectedIds];
  const states = Object.fromEntries(tags.map(tag => [tag, 2]));

  try {
    await applyBulkTagEdit(ids, states, 'タグの追加に失敗しました');
    input.value = '';
    hideTagSuggestions();
    await refreshList();
    await refreshTags();
    await refreshTagEditor(ids);
  } catch (error) {
    showNotification(error.message || 'タグの追加に失敗しました', 'error');
  }
}

function splitTagInput(value) {
  const seen = new Set();
  const tags = [];
  for (const raw of String(value || '').split(/[\s　]+/u)) {
    const tag = raw.trim();
    if (!tag || seen.has(tag)) continue;
    seen.add(tag);
    tags.push(tag);
  }
  return tags;
}

function renderTagSuggestions() {
  const input = El.newTagInput;
  const box = El.tagSuggestions;
  if (!input || !box) return;

  const fragment = getActiveTagFragment(input.value);
  const query = normalizeTagCandidate(fragment.value);
  if (!query) {
    hideTagSuggestions();
    return;
  }

  const currentTags = getCurrentEditorTags();
  const candidates = State.tags
    .filter(tag => !currentTags.has(tag))
    .map(tag => ({ tag, score: tagSuggestionScore(tag, query) }))
    .filter(item => item.score >= 0)
    .sort((a, b) => a.score - b.score || a.tag.localeCompare(b.tag, 'ja'))
    .slice(0, 10);

  if (candidates.length === 0) {
    hideTagSuggestions();
    return;
  }

  tagSuggestionIndex = Math.min(Math.max(tagSuggestionIndex, 0), candidates.length - 1);
  box.innerHTML = '';
  candidates.forEach((item, index) => {
    const option = document.createElement('button');
    option.type = 'button';
    option.className = 'tag-suggestion-option';
    option.classList.toggle('active', index === tagSuggestionIndex);
    option.textContent = item.tag;
    option.dataset.tag = item.tag;
    option.addEventListener('mousedown', (e) => {
      e.preventDefault();
    });
    option.addEventListener('click', () => {
      replaceActiveTagFragment(item.tag);
    });
    box.appendChild(option);
  });
  box.classList.remove('hide');
}

function handleTagSuggestionKeydown(event) {
  const box = El.tagSuggestions;
  if (!box || box.classList.contains('hide')) return false;
  const options = Array.from(box.querySelectorAll('.tag-suggestion-option'));
  if (options.length === 0) return false;

  if (event.key === 'ArrowDown') {
    event.preventDefault();
    tagSuggestionIndex = (tagSuggestionIndex + 1) % options.length;
    updateTagSuggestionActive(options);
    return true;
  }
  if (event.key === 'ArrowUp') {
    event.preventDefault();
    tagSuggestionIndex = (tagSuggestionIndex + options.length - 1) % options.length;
    updateTagSuggestionActive(options);
    return true;
  }
  if (event.key === 'Tab' || event.key === 'Enter') {
    const option = options[tagSuggestionIndex] || options[0];
    if (option?.dataset.tag) {
      event.preventDefault();
      replaceActiveTagFragment(option.dataset.tag);
      return true;
    }
  }
  if (event.key === 'Escape') {
    event.preventDefault();
    hideTagSuggestions();
    return true;
  }

  return false;
}

function updateTagSuggestionActive(options) {
  options.forEach((option, index) => {
    option.classList.toggle('active', index === tagSuggestionIndex);
  });
}

function hideTagSuggestions() {
  tagSuggestionIndex = -1;
  if (El.tagSuggestions) {
    El.tagSuggestions.innerHTML = '';
    El.tagSuggestions.classList.add('hide');
  }
}

function getCurrentEditorTags() {
  const tags = new Set();
  El.tagEditorCurrent?.querySelectorAll('.tag-editable[data-tag]').forEach(el => {
    const presentCount = Number.parseInt(el.dataset.presentCount || '0', 10);
    const selectedCount = Number.parseInt(el.dataset.selectedCount || '1', 10);
    if (el.dataset.tag && presentCount >= selectedCount) {
      tags.add(el.dataset.tag);
    }
  });
  return tags;
}

async function refreshTagEditor(ids) {
  const selectionCount = ids.length;
  const taginfo = await postJson('/api/taginfo.json', {
    ids,
    ...currentSortStatePayload(),
  });
  const selectedTagInfo = Array.isArray(taginfo)
    ? taginfo.filter(info => Number(info?.count || 0) > 0)
    : [];
  renderTagEditorSummary(selectionCount);
  renderTagEditorTags(ids, selectedTagInfo);
}

function renderTagEditorSummary(selectionCount, options = {}) {
  if (!El.tagEditorSummary) return;
  if (options.loading) {
    El.tagEditorSummary.textContent = 'タグ情報を読み込み中です...';
    return;
  }
  if (options.error) {
    El.tagEditorSummary.textContent =
      selectionCount > 1
        ? `${selectionCount}件選択中。追加・削除は選択中すべてに反映されます。`
        : 'この小説のタグを編集できます。';
    return;
  }
  El.tagEditorSummary.textContent =
    selectionCount > 1
      ? `${selectionCount}件選択中。表示中のタグは付与済み件数を n/${selectionCount} で示し、追加・削除は選択中すべてに反映されます。`
      : 'この小説の現在のタグです。';
}

function renderTagEditorTags(ids, taginfo) {
  const container = El.tagEditorCurrent;
  if (!container) return;
  const selectionCount = ids.length;
  container.innerHTML = '';

  if (!Array.isArray(taginfo) || taginfo.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'tag-editor-empty';
    empty.textContent =
      selectionCount > 1
        ? '選択中の小説に付いているタグはありません。下の入力欄から追加できます。'
        : 'この小説にはタグがありません。下の入力欄から追加できます。';
    container.appendChild(empty);
    return;
  }

  taginfo.forEach(info => {
    if (!info?.tag) return;
    const presentCount = Number(info.count || 0);
    const chip = document.createElement('span');
    chip.className = `tag-label ${tagColorClass(info.tag)} tag-editable`;
    if (selectionCount > 1 && presentCount < selectionCount) {
      chip.classList.add('tag-editable-partial');
    }
    chip.dataset.tag = info.tag;
    chip.dataset.presentCount = String(presentCount);
    chip.dataset.selectedCount = String(selectionCount);
    chip.textContent = info.tag;

    if (selectionCount > 1) {
      const count = document.createElement('span');
      count.className = 'tag-edit-count';
      count.textContent = ` ${presentCount}/${selectionCount}`;
      chip.appendChild(count);
    }

    const removeBtn = document.createElement('span');
    removeBtn.className = 'tag-remove';
    removeBtn.textContent = '×';
    removeBtn.title =
      selectionCount > 1
        ? `選択中の${selectionCount}件から「${info.tag}」を外す`
        : `「${info.tag}」を外す`;
    removeBtn.addEventListener('click', async () => {
      try {
        await applyBulkTagEdit(ids, { [info.tag]: 0 }, 'タグの削除に失敗しました');
        await refreshList();
        await refreshTags();
        await refreshTagEditor(ids);
      } catch (error) {
        showNotification(error.message || 'タグの削除に失敗しました', 'error');
      }
    });
    chip.appendChild(removeBtn);
    container.appendChild(chip);
  });
}

function tagColorClass(tag) {
  const colorName = State.tagColors?.[tag] || 'default';
  return TAG_COLOR_MAP[colorName] || 'tag-default';
}

async function applyBulkTagEdit(ids, states, fallbackMessage) {
  const result = await postJson('/api/edit_tag', {
    ids,
    states,
    ...currentSortStatePayload(),
  });
  if (!result || result.success !== true) {
    throw new Error(result?.error || result?.message || fallbackMessage);
  }
  return result;
}

function getActiveTagFragment(value) {
  const text = String(value || '');
  const match = text.match(/^(.*?)([^\s　]*)$/u);
  if (!match) return { prefix: '', value: text };
  return { prefix: match[1], value: match[2] };
}

function replaceActiveTagFragment(tag) {
  const input = El.newTagInput;
  if (!input) return;
  const fragment = getActiveTagFragment(input.value);
  input.value = fragment.prefix + tag;
  hideTagSuggestions();
  input.focus();
}

function tagSuggestionScore(tag, query) {
  const normalized = normalizeTagCandidate(tag);
  if (normalized === query) return 0;
  if (normalized.startsWith(query)) return 1;
  if (normalized.includes(query)) return 2;
  return -1;
}

function normalizeTagCandidate(value) {
  return toHiragana(String(value || '').normalize('NFKC').toLowerCase()).replace(/\s+/gu, '');
}

function toHiragana(value) {
  return value.replace(/[\u30a1-\u30f6]/g, ch =>
    String.fromCharCode(ch.charCodeAt(0) - 0x60)
  );
}

/* ===== Notepad ===== */

async function openNotepad() {
  const popup = window.open(
    '/notepad',
    'narou_notepad',
    'width=760,height=720,menubar=no,toolbar=no,location=no,status=no,resizable=yes,scrollbars=yes'
  );
  if (popup) {
    popup.focus();
    return;
  }

  try {
    const data = await fetchJson('/api/notepad/read');
    applyNotepadSnapshot(data);
    El.notepadModal?.classList.remove('hide');
  } catch (error) {
    showNotification(error.message || 'メモ帳の読み込みに失敗しました', 'error');
  }
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
  if (El.aboutLatestVersion) {
    El.aboutLatestVersion.textContent = '最新バージョン: 確認中...';
  }
  void updateLatestVersionInfo();
}

async function updateLatestVersionInfo() {
  if (!El.aboutLatestVersion) return;
  try {
    const data = await fetchJson('/api/version/latest.json');
    if (data?.success) {
      const latest = data.latest_version || '-';
      const current = data.current_version || '-';
      El.aboutLatestVersion.textContent =
        `最新バージョン: ${latest}${data.update_available ? ' (更新あり)' : ' (最新)'} / 現在: ${current}`;
      return;
    }
    El.aboutLatestVersion.textContent =
      `最新バージョン: 取得失敗${data?.message ? ' (' + data.message + ')' : ''}`;
  } catch (e) {
    El.aboutLatestVersion.textContent = '最新バージョン: 取得失敗';
  }
}

/* ===== Diff list ===== */

async function openDiffList(ids) {
  const container = El.diffListContainer;
  if (!container) return;
  container.innerHTML = '<p>読み込み中...</p>';
  El.diffModal?.classList.remove('hide');

  try {
    const data = await postJson('/api/diff_list', { targets: ids });
    if (data?.error) {
      throw new Error(data.error);
    }
    if (Array.isArray(data?.diffs)) {
      container.innerHTML = data.diffs.map(d =>
        `<div class="diff-entry" data-diff-id="${escHtml(String(d.id))}">
          <div class="diff-header">
            <h5>${escHtml(d.title || d.id)}</h5>
            <button class="btn btn-sm btn-diff-clean" data-id="${escHtml(String(d.id))}" title="差分キャッシュを削除"><span class="material-symbols-outlined icon-leading" aria-hidden="true">delete</span>クリア</button>
          </div>
          <pre class="diff-content">${escHtml(d.content || 'No diff')}</pre>
        </div>`
      ).join('');
      container.querySelectorAll('.btn-diff-clean').forEach(btn => {
        btn.addEventListener('click', async () => {
          const id = btn.dataset.id;
          try {
            const result = await postJson('/api/diff_clean', { target: id });
            assertApiSuccess(result, '差分キャッシュの削除に失敗しました');
            const entry = btn.closest('.diff-entry');
            if (entry) {
              const pre = entry.querySelector('.diff-content');
              if (pre) pre.textContent = 'No diff';
            }
            showNotification(result.message || '差分キャッシュを削除しました', 'success');
          } catch (error) {
            showNotification(error.message || '差分キャッシュの削除に失敗しました', 'error');
          }
        });
      });
    } else {
      container.innerHTML = '<p>差分データがありません</p>';
    }
  } catch (error) {
    container.innerHTML = '<p>' + escHtml(error.message || '差分の取得に失敗しました') + '</p>';
  }
}

function escHtml(s) {
  const div = document.createElement('div');
  div.textContent = String(s);
  return div.innerHTML;
}

function escAttr(s) {
  return escHtml(s).replace(/"/g, '&quot;');
}

/* ===== Remove Confirm Modal ===== */

function showRemoveModal(ids) {
  if (!ids || ids.length === 0) return;
  const numericIds = ids.map(id => Number(id)).filter(id => Number.isFinite(id));
  if (numericIds.length === 0) {
    showNotification('削除対象のIDが不正です', 'error');
    return;
  }
  // Build novel title list
  const items = numericIds.map(id => {
    const n = State.novels.find(n => String(n.id) === String(id));
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
    await runGuardedAction(El.removeOk, async () => {
      const withFile = El.removeWithFile.checked;
      const result = await postJson('/api/novels/remove', {
        ids: numericIds,
        with_file: withFile,
        ...currentSortStatePayload(),
      });
      assertApiSuccess(result, '削除に失敗しました');
      cleanup();
      await refreshList();
    }, '削除に失敗しました');
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
  { cls: 'col-average-length', label: '平均文字数' },
  { cls: 'col-status', label: '状態' },
  { cls: 'col-url', label: 'リンク' },
  { cls: 'col-download', label: 'ＤＬ' },
  { cls: 'col-folder', label: '保存先' },
  { cls: 'col-update-action', label: '更新' },
  { cls: 'col-story', label: 'あらすじ' },
  { cls: 'col-menu', label: '個別' },
];

const COLVIS_STORAGE_KEY = 'narou-rs-webui-hidden-cols';
const COLVIS_DEFAULT_VISIBLE_DESKTOP = new Set([
  'col-id',
  'col-update',
  'col-general-lastup',
  'col-author',
  'col-site',
  'col-tags',
  'col-status',
  'col-url',
  'col-folder',
  'col-update-action',
  'col-menu',
]);
const COLVIS_DEFAULT_VISIBLE_NARROW = new Set([
  'col-download',
  'col-menu',
]);

function getDefaultVisibleCols() {
  if (window.matchMedia('(max-width: 48em)').matches) {
    return COLVIS_DEFAULT_VISIBLE_NARROW;
  }
  return COLVIS_DEFAULT_VISIBLE_DESKTOP;
}

function getDefaultHiddenCols() {
  const visible = getDefaultVisibleCols();
  return COLVIS_COLUMNS
    .map(col => col.cls)
    .filter(cls => !visible.has(cls));
}

function getHiddenCols() {
  const raw = localStorage.getItem(COLVIS_STORAGE_KEY);
  if (raw === null) return getDefaultHiddenCols();
  try {
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) {
      const allowed = new Set(COLVIS_COLUMNS.map(col => col.cls));
      return parsed.filter(cls => allowed.has(cls));
    }
  } catch { /* ignore */ }
  return getDefaultHiddenCols();
}

function setHiddenCols(arr) {
  const allowed = new Set(COLVIS_COLUMNS.map(col => col.cls));
  const sanitized = arr.filter(cls => allowed.has(cls));
  localStorage.setItem(COLVIS_STORAGE_KEY, JSON.stringify(sanitized));
}

function clearHiddenColsPreference() {
  localStorage.removeItem(COLVIS_STORAGE_KEY);
}

function updateEpisodesWidthTarget(hidden = new Set(getHiddenCols())) {
  const table = El.novelList;
  if (!table) return;
  const expandEpisodes = hidden.has('col-length')
    && hidden.has('col-average-length');
  table.classList.toggle('episodes-width-target', expandEpisodes);
}

function applyColumnVisibility() {
  const hidden = new Set(getHiddenCols());
  const style = document.getElementById('colvis-style') || (() => {
    const s = document.createElement('style');
    s.id = 'colvis-style';
    document.head.appendChild(s);
    return s;
  })();
  updateEpisodesWidthTarget(hidden);
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

function resetColvisCheckboxesToDefault() {
  const hidden = new Set(getDefaultHiddenCols());
  El.colvisList?.querySelectorAll('input[type="checkbox"]').forEach(cb => {
    cb.checked = !hidden.has(cb.dataset.col);
  });
}

function openMenuStyleModal() {
  const style = getStoredMenuStyle();
  const windowsRadio = document.getElementById('menu-style-windows');
  const macRadio = document.getElementById('menu-style-mac');
  if (windowsRadio) windowsRadio.checked = style === 'windows';
  if (macRadio) macRadio.checked = style === 'mac';
  document.getElementById('menu-style-modal')?.classList.remove('hide');
}

function closeMenuStyleModal() {
  document.getElementById('menu-style-modal')?.classList.add('hide');
}

/* ===== Data refresh ===== */

export async function refreshList() {
  try {
    const resp = await fetchJson('/api/list?all=true');
    if (resp && Array.isArray(resp.data)) {
      State.novels = resp.data;
      State.frozenIds = new Set(
        resp.data.filter(n => n.frozen).map(n => String(n.id))
      );
      pruneSelectedIdsToCurrentList();
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
      return data;
    }
  } catch { /* ignore */ }
  return null;
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
    const data = await fetchJson('/api/tag_list?format=json');
    if (data) {
      State.tags = data.tags || [];
      State.tagColors = data.tag_colors || data.colors || {};
      renderTagList();
      renderNovelList();
    }
  } catch { /* ignore */ }
}

function replaceConsoleHistory(consoleEl, history) {
  var lines = consoleEl.querySelectorAll('.console-line');
  lines.forEach(function(el) { el.remove(); });
  String(history || '').split('\n').forEach(function(line) {
    var div = document.createElement('div');
    div.className = 'console-line';
    div.textContent = line;
    consoleEl.appendChild(div);
  });
  if (consoleEl.id === 'console-stdout2') {
    State.consolePinned.stdout2 = true;
  } else {
    State.consolePinned.main = true;
  }
  consoleEl.scrollTop = consoleEl.scrollHeight;
}
