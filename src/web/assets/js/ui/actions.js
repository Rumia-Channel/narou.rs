/**
 * Event bindings for buttons, navbar actions, console, modals, and all menu items.
 * Mirrors narou.rb's full set of UI interactions.
 */
import { State, El, lsSet, lsBool } from '../core/state.js';
import { fetchJson, postJson, request, assertApiSuccess } from '../core/http.js';
import { createNotepadEditor } from '../core/notepad.js';
import { persistSortState, postSortedJson } from '../core/sort_state.js';
import { toggleLanguage, t } from './i18n.js';
import { openFeatureTour } from './feature_tour.js';
import { resetConsoleSize } from './console_size.js';
import {
  renderNovelList, renderTagList, renderQueueStatus, renderQueueDetailed,
  selectVisible, selectAll, clearSelection, getSelectedIdsInDisplayOrder,
  pruneSelectedIdsToCurrentList, updateEnableSelected,
  syncViewChecks, showNotification,
} from './render.js';
import { setShortcutHandlers, initShortcuts } from './shortcuts.js';
import {
  setContextHandlers, initContextMenu, initTagColorMenu,
  getStoredMenuStyle, setStoredMenuStyle, showTagColorMenu,
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
let notepadEditor = null;
let notepadOpenGeneration = 0;
let tagEditorGeneration = 0;
let tagReadGeneration = 0;
let tagMutationPending = false;
let diffGeneration = 0;
let removeTransaction = null;
let removePending = false;
let themeGeneration = 0;
let themeSaveTail = Promise.resolve();
let acknowledgedTheme = null;
let themeReadGeneration = 0;
let themePendingSaves = 0;

export function bindActions() {
  applyColumnVisibility();
  if (El.filterInput) El.filterInput.value = State.filterText || '';
  El.filterClear?.classList.toggle('hide', !State.filterText);

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


  El.consoleCancel?.addEventListener('click', async () => {
    await runGuardedAction(El.consoleCancel, async () => {
      await postJson('/api/cancel', {});
    }, '処理の中断に失敗しました');
  });

  El.consoleHistory?.addEventListener('click', async () => {
    const revision = ++State.consoleRevision;
    try {
      const mainData = await fetchJson('/api/history?format=json');
      assertHistoryPayload(mainData);
      let subData = null;
      if (State.concurrencyEnabled && El.consoleStdout2) {
        subData = await fetchJson('/api/history?format=json&stream=stdout2');
        assertHistoryPayload(subData);
      }
      if (revision !== State.consoleRevision) return;
      if (El.console) {
        replaceConsoleHistory(El.console, mainData.history);
      }
      if (subData && El.consoleStdout2) {
        replaceConsoleHistory(El.consoleStdout2, subData.history);
      }
    } catch (error) {
      if (revision !== State.consoleRevision) return;
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

  if (El.novelListShowHidden) {
    El.novelListShowHidden.checked = State.showHidden;
    El.novelListShowHidden.addEventListener('change', () => {
      State.showHidden = El.novelListShowHidden.checked;
      lsSet('show-hidden', String(State.showHidden));
      State.currentPage = 1;
      refreshList();
    });
  }

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
    El.controlPanel?.classList.toggle('hide', !State.buttonsTop);
    syncViewChecks();
  });

  on('action-view-toggle-buttons-footer', () => {
    State.buttonsFooter = !State.buttonsFooter;
    lsSet('buttons-footer', String(State.buttonsFooter));
    syncViewChecks();
  });

  on('action-view-reset-console-size', () => {
    resetConsoleSize();
    showNotification(t('consoleSizeReset'), 'info');
  });

  on('action-view-reset', () => {
    resetConsoleSize();
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
  on('action-option-hidden', openHiddenSettings);
  on('action-option-export', () => {
    const ids = getSelectedIdsInDisplayOrder();
    if (!ids.length) {
      showNotification('ダウンロードする作品を選択してください', 'error');
      return;
    }
    const anchor = document.createElement('a');
    anchor.href = `/api/transfer/export?ids=${ids.join(',')}`;
    anchor.download = 'narou-export.zip';
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
  });
  on('action-option-import', () => document.getElementById('transfer-import-file')?.click());
  document.getElementById('transfer-import-file')?.addEventListener('change', async (event) => {
    const file = event.target.files?.[0];
    event.target.value = '';
    if (!file) return;
    try {
      const response = await fetch('/api/transfer/import', {
        method: 'POST',
        headers: { 'Content-Type': 'application/zip' },
        body: file,
      });
      const result = await response.json().catch(() => null);
      if (!response.ok || !result?.success) {
        throw new Error(result?.message || 'アップロードに失敗しました');
      }
      const summary = `新規 ${result.created ?? 0} / 上書き ${result.overwritten ?? 0} / スキップ ${result.skipped ?? 0}`;
      showNotification(summary, 'info');
      await refreshList();
    } catch (error) {
      showNotification(error.message || 'アップロードに失敗しました', 'error');
    }
  });
  on('action-option-virtual-add', async () => {
    const ids = getSelectedIdsInDisplayOrder();
    if (!ids.length) {
      showNotification('まとめる作品を選択してください', 'error');
      return;
    }
    const title = window.prompt('バーチャル小説のタイトル');
    if (!title) return;
    const author = window.prompt('作者名', '') ?? '';
    const sitename = window.prompt('サイト名', '') ?? '';
    try {
      const response = await fetch('/api/virtual_novels', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ title, author, sitename, tags: [], members: ids.map(Number) }),
      });
      const result = await response.json();
      if (!response.ok || !result.success) {
        throw new Error(result.message || '登録に失敗しました');
      }
      showNotification(`バーチャル小説を登録しました: ${result.ncode}`, 'info');
      await refreshList();
    } catch (error) {
      showNotification(error.message || '登録に失敗しました', 'error');
    }
  });
  on('action-option-virtual-order', async () => {
    try {
      const response = await fetch('/api/virtual_novels');
      const result = await response.json();
      if (!result.success) throw new Error(result.message || '一覧を取得できません');
      if (!result.novels.length) {
        showNotification('バーチャル小説は登録されていません', 'error');
        return;
      }
      const menu = result.novels
        .map((novel) => `${novel.ncode} ${novel.title} (${novel.members.length}作品)`)
        .join('\n');
      const target = window.prompt(`対象の ncode を入力してください\n${menu}`, result.novels[0].ncode);
      if (!target) return;
      const novel = result.novels.find(
        (item) => item.ncode === target || String(item.id) === target,
      );
      if (!novel) throw new Error('対象のバーチャル小説が見つかりません');
      openVirtualOrderModal(novel);
    } catch (error) {
      showNotification(error.message || '並べ替えに失敗しました', 'error');
    }
  });
  on('action-option-virtual-convert', async () => {
    try {
      const response = await fetch('/api/virtual_novels');
      const result = await response.json();
      if (!result.success) throw new Error(result.message || '一覧を取得できません');
      if (!result.novels.length) {
        showNotification('バーチャル小説は登録されていません', 'error');
        return;
      }
      const menu = result.novels.map((novel) => `${novel.ncode} ${novel.title}`).join('\n');
      const target = window.prompt(`変換する ncode を入力してください\n${menu}`, result.novels[0].ncode);
      if (!target) return;
      const novel = result.novels.find(
        (item) => item.ncode === target || String(item.id) === target,
      );
      if (!novel) throw new Error('対象のバーチャル小説が見つかりません');
      showNotification('変換を開始しました', 'info');
      const converted = await fetch(`/api/virtual_novels/${novel.id}/convert`, { method: 'POST' });
      const convertedResult = await converted.json();
      if (!converted.ok || !convertedResult.success) {
        throw new Error(convertedResult.message || '変換に失敗しました');
      }
      showNotification(`出力しました: ${convertedResult.filename}`, 'info');
    } catch (error) {
      showNotification(error.message || '変換に失敗しました', 'error');
    }
  });
  on('action-option-virtual-remove', async () => {
    try {
      const response = await fetch('/api/virtual_novels');
      const result = await response.json();
      if (!result.success) throw new Error(result.message || '一覧を取得できません');
      if (!result.novels.length) {
        showNotification('バーチャル小説は登録されていません', 'error');
        return;
      }
      const menu = result.novels.map((novel) => `${novel.ncode} ${novel.title}`).join('\n');
      const target = window.prompt(`削除する ncode を入力してください\n${menu}`, result.novels[0].ncode);
      if (!target) return;
      const novel = result.novels.find(
        (item) => item.ncode === target || String(item.id) === target,
      );
      if (!novel) throw new Error('対象のバーチャル小説が見つかりません');
      if (!window.confirm(`${novel.ncode} ${novel.title} を削除しますか？（メンバー作品は残ります）`)) {
        return;
      }
      const removed = await fetch(`/api/virtual_novels/${novel.id}`, { method: 'DELETE' });
      const removedResult = await removed.json();
      if (!removed.ok || !removedResult.success) {
        throw new Error(removedResult.message || '削除に失敗しました');
      }
      showNotification('バーチャル小説を削除しました', 'info');
      await refreshList();
    } catch (error) {
      showNotification(error.message || '削除に失敗しました', 'error');
    }
  });
  on('hidden-close', () => closeModal(document.getElementById('hidden-modal')));
  on('virtual-order-close', () => closeModal(document.getElementById('virtual-order-modal')));
  on('virtual-order-cancel', () => closeModal(document.getElementById('virtual-order-modal')));
  on('virtual-order-save', saveVirtualOrder);
  on('hidden-cancel', () => closeModal(document.getElementById('hidden-modal')));
  on('hidden-novel-add', () => addHiddenValue('novels', 'hidden-novel-input'));
  on('hidden-tag-add', () => addHiddenValue('tags', 'hidden-tag-input'));
  on('hidden-author-add', () => addHiddenValue('authors', 'hidden-author-input'));
  on('hidden-site-add', () => addHiddenValue('sites', 'hidden-site-input'));
  on('hidden-all-clear', async () => {
    await postJson('/api/hidden', { action: 'clear' });
    await refreshHiddenSettings();
    await refreshList();
  });

  on('action-option-shutdown', async (e) => {
    if (!confirm('サーバをシャットダウンしますか？')) return;
    await runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/shutdown', {});
      assertApiSuccess(result, 'サーバを停止できませんでした');
    }, 'サーバを停止できませんでした');
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
    if (acknowledgedTheme === null) acknowledgedTheme = State.theme || 'default';
    const generation = ++themeGeneration;
    ++themeReadGeneration;
    ++themePendingSaves;
    const theme = El.themeSelect.value;
    State.theme = theme;
    lsSet('theme', theme);
    document.documentElement.dataset.theme = theme === 'default' ? '' : theme;
    try {
      const save = themeSaveTail.then(async () => {
        const result = await postJson('/api/global_setting', {
          settings: { 'webui.theme': theme === 'default' ? null : theme },
        });
        assertApiSuccess(result, 'テーマ設定の保存に失敗しました');
        acknowledgedTheme = theme;
      });
      themeSaveTail = save.catch(() => {});
      await save;
    } catch (error) {
      if (generation !== themeGeneration) return;
      State.theme = acknowledgedTheme;
      lsSet('theme', acknowledgedTheme);
      document.documentElement.dataset.theme = acknowledgedTheme === 'default' ? '' : acknowledgedTheme;
      if (El.themeSelect) El.themeSelect.value = acknowledgedTheme;
      showNotification(error.message || 'テーマ設定の保存に失敗しました', 'error');
    } finally {
      --themePendingSaves;
      ++themeReadGeneration;
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
  on('notepad-close', () => closeModal(El.notepadModal));
  on('save-notepad-button', (e) => {
    const generation = notepadOpenGeneration;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await getNotepadEditor().save();
      if (!result.dirty && generation === notepadOpenGeneration) closeModal(El.notepadModal);
      if (result.saved) showNotification(result.dirty
        ? '前の入力を保存しました。新しい入力は未保存です'
        : result.data.message || 'メモ帳を保存しました', result.dirty ? 'info' : 'success');
      return result;
    }, 'メモ帳の保存に失敗しました');
  });

  // --- Tag edit modal ---
  on('tag-edit-close', closeTagEditor);
  on('tag-edit-cancel', closeTagEditor);
  on('add-tag-button', addTagFromInput);
  on('favorite-tag-button', addFavoriteTagFromInput);
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
  on('about-update', async () => {
    if (latestVersionData?.variant_choice_required) {
      El.updateVariantModal?.classList.remove('hide');
      return;
    }
    await startSelfUpdate(null);
  });

  on('update-variant-close', () => El.updateVariantModal?.classList.add('hide'));
  on('update-variant-cancel', () => El.updateVariantModal?.classList.add('hide'));
  on('update-variant-standard', async () => {
    El.updateVariantModal?.classList.add('hide');
    await startSelfUpdate('standard');
  });
  on('update-variant-gpl', async () => {
    El.updateVariantModal?.classList.add('hide');
    await startSelfUpdate('gpl');
  });

  for (const [id, endpoint] of [
    ['queue-restore-yes', '/api/restore_pending_tasks'],
    ['queue-restore-no', '/api/defer_restore_pending_tasks'],
  ]) {
    on(id, (e) => {
      void runGuardedAction(e.currentTarget, async () => {
        const result = await postJson(endpoint, {});
        assertApiSuccess(result, '待機キューの復元操作に失敗しました');
        closeModal(El.queueRestoreModal);
        await refreshQueueDetailed();
      }, '待機キューの復元操作に失敗しました');
    });
  }

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

  // --- Bookmarklet register modal ---
  function clearRegisterUrl() {
    if (El.registerModal) delete El.registerModal.dataset.registerUrl;
  }
  function hideRegisterModal() {
    clearRegisterUrl();
    El.registerModal?.classList.add('hide');
  }
  on('register-modal-close', hideRegisterModal);
  on('register-cancel', hideRegisterModal);
  on('register-ok', (e) => {
    const triggerEl = e.currentTarget;
    const url = El.registerModal?.dataset?.registerUrl || '';
    if (!url) {
      hideRegisterModal();
      return;
    }
    void runGuardedAction(triggerEl, async () => {
      const result = await postSortedJson('/api/download', { targets: [url] });
      assertApiSuccess(result, 'ブックマークレット登録に失敗しました');
      hideRegisterModal();
      showNotification('ブックマークレットから登録しました', 'success');
    }, 'ブックマークレット登録に失敗しました');
  });

  // --- Diff modal ---
  on('diff-close', () => closeModal(El.diffModal));
  on('remove-cancel', () => closeModal(El.removeModal));
  on('remove-ok', confirmRemove);

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
      const result = await postSortedJson('/api/download', { targets, mail });
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
      const result = await postSortedJson('/api/download', { targets: ids, force: true });
      assertApiSuccess(result, '強制ダウンロード要求の送信に失敗しました');
    }, '強制ダウンロード要求の送信に失敗しました');
  });

  on('btn-update', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const total = Array.isArray(State.novels) ? State.novels.length : 0;
      const selectedCount = State.selectedIds.size;
      const allSelected = total > 0 && selectedCount >= total;
      const result = (selectedCount > 0 && !allSelected)
        ? await postSortedJson('/api/update', {
          targets: [...State.selectedIds],
        })
        : await postSortedJson('/api/update', { update_all: true });
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
    const result = await postSortedJson('/api/update_general_lastup', {
      option,
      is_update_modified: isUpdateModified
    });
    assertApiSuccess(result, '最新話掲載日確認の要求送信に失敗しました');
  };

  on('gl-update-submit', (e) => {
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
    const option = (glNarou && glOther) ? 'all' : (glNarou ? 'narou' : 'other');
    document.getElementById('gl-update-modal')?.classList.add('hide');
    void runGuardedAction(
      e.currentTarget,
      () => queueGeneralLastupUpdate(option, isUpdateModified),
      '最新話掲載日確認の要求送信に失敗しました'
    );
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
  on('update-by-tag-submit', async (e) => {
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
    await runGuardedAction(e.currentTarget, async () => {
      const result = await postSortedJson('/api/update_by_tag', { tags, exclusion_tags });
      assertApiSuccess(result, 'タグ指定の更新要求に失敗しました');
      document.getElementById('update-by-tag-modal')?.classList.add('hide');
    }, 'タグ指定の更新要求に失敗しました');
  });

  on('action-update-view', (e) => {
    const ids = getVisibleIds();
    if (ids.length === 0) return;
    void runGuardedAction(e.currentTarget, async () => {
      const total = Array.isArray(State.novels) ? State.novels.length : 0;
      const allVisible = total > 0 && ids.length >= total;
      const result = allVisible
        ? await postSortedJson('/api/update', { update_all: true })
        : await postSortedJson('/api/update', { targets: ids });
      assertApiSuccess(result, '表示中小説のアップデート要求に失敗しました');
    }, '表示中小説のアップデート要求に失敗しました');
  });

  on('action-update-force', (e) => {
    void runGuardedAction(e.currentTarget, async () => {
      const total = Array.isArray(State.novels) ? State.novels.length : 0;
      const selectedCount = State.selectedIds.size;
      const allSelected = total > 0 && selectedCount >= total;
      const result = (selectedCount > 0 && !allSelected)
        ? await postSortedJson('/api/update', {
          targets: [...State.selectedIds],
          force: true,
        })
        : await postSortedJson('/api/update', { update_all: true, force: true });
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
      const result = await postSortedJson('/api/update_by_tag', {
        tags: ['modified'],
        exclusion_tags: [],
      });
      assertApiSuccess(result, 'modifiedタグ更新の要求送信に失敗しました');
    }, 'modifiedタグ更新の要求送信に失敗しました');
  });

  on('btn-send', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postSortedJson('/api/send', { targets: ids });
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
      const result = await postSortedJson('/api/convert', { targets: ids });
      assertApiSuccess(result, '変換要求の送信に失敗しました');
    }, '変換要求の送信に失敗しました');
  });

  on('action-other-diff', () => {
    const ids = requireSelectedIds();
    if (!ids) return;
    openDiffList(ids);
  });

  on('action-other-inspect', (e) => {
    const ids = requireSelectedIds();
    if (!ids) return;
    void runGuardedAction(e.currentTarget, async () => {
      const result = await postJson('/api/inspect', { targets: ids });
      assertApiSuccess(result, '調査状況ログの表示に失敗しました');
    }, '調査状況ログの表示に失敗しました');
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
      const result = await postSortedJson('/api/backup', { targets: ids });
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
      const result = await postSortedJson('/api/mail', { targets: ids });
      assertApiSuccess(result, 'メール送信要求の送信に失敗しました');
    }, 'メール送信要求の送信に失敗しました');
  });

  on('action-view-link-to-edit-menu', () => {
    window.open('/edit_menu', '_blank');
  });

  on('action-view-select-menu-style', openMenuStyleModal);
  on('action-view-feature-tour', openFeatureTour);
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

      void persistSortState().catch(error => {
        showNotification(error.message || '並び順の保存に失敗しました', 'error');
      });
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
    closeModal,
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
    hiddenToggle: async (id) => {
      const current = await fetchJson('/api/hidden');
      const novels = Array.isArray(current?.novels) ? current.novels : [];
      const isHidden = novels.includes(Number(id));
      await postJson('/api/hidden', {
        action: isHidden ? 'remove' : 'add',
        novels: [Number(id)],
      });
      await refreshList();
    },
    freezeToggle: async (id) => {
      await postJson('/api/freeze', { ids: [Number(id)] });
      await refreshList();
    },
    updateSingle: (id) => postSortedJson('/api/update', { targets: [String(id)] }),
    updateForceSingle: (id) => postSortedJson('/api/update', { targets: [String(id)], force: true }),
    sendSingle: (id) => postSortedJson('/api/send', { targets: [String(id)] }),
    removeSingle: async (id) => {
      showRemoveModal([Number(id)]);
    },
    convertSingle: (id) => runGuardedAction(null, async () => {
      const result = await postSortedJson('/api/convert', { targets: [String(id)] });
      assertApiSuccess(result, '変換要求の送信に失敗しました');
    }, '変換要求の送信に失敗しました'),
    inspectSingle: (id) => runGuardedAction(null, async () => {
      const result = await postJson('/api/inspect', { targets: [String(id)] });
      assertApiSuccess(result, '調査状況ログの表示に失敗しました');
    }, '調査状況ログの表示に失敗しました'),
    folderSingle: (id) => openFolderTargets([String(id)]),
    backupSingle: (id) => postSortedJson('/api/backup', { targets: [String(id)] }),
    downloadForceSingle: (id) => postSortedJson('/api/download', { targets: [String(id)], force: true }),
    mailSingle: (id) => postSortedJson('/api/mail', { targets: [String(id)] }),
    authorComments: (id) => {
      const popup = window.open(
        '/novels/' + id + '/author_comments',
        'author_comments_' + id,
        'width=760,height=640,menubar=no,toolbar=no,location=no,status=no,resizable=yes,scrollbars=yes'
      );
      if (!popup) window.location.href = '/novels/' + id + '/author_comments';
    },
    refreshTags: () => refreshTags(),
    tagColorChanged: () => refreshOpenTagEditor(),
    refreshList: () => refreshList(),
    notifyError: message => showNotification(message, 'error'),
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

/**
 * Handle `?register=<url>` query parameter for the bookmarklet same-origin flow.
 *
 * When the user clicks the bookmarklet on a target site, it opens the WEB UI in a
 * new window with the URL passed as `?register=...`. We show a confirmation dialog
 * and, on approval, call the existing same-origin `/api/download` POST endpoint.
 * The Origin header is then the WEB UI itself, so the CSRF guard permits it.
 *
 * Caller (main.js) is expected to strip the `register` param from the URL after
 * dismissal (cancel or success) so refreshes do not re-prompt.
 */
export function handleRegisterParam(url) {
  if (!url || typeof url !== 'string') return false;
  if (!El.registerModal || !El.registerUrl) return false;
  // Defensive: only http(s) schemes are accepted. javascript:/data:/file: would
  // either be rejected by the API or, worse, trick the user. Reject anything else.
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return false;
  if (url.length > 2048) return false;

  // Stash the URL on the modal element so the OK handler (bound in bindActions)
  // can read it without sharing a closure over module-local state.
  El.registerModal.dataset.registerUrl = url;
  El.registerUrl.textContent = url;
  El.registerModal.classList.remove('hide');
  return true;
}

/* ===== Helpers ===== */

function on(id, handler) {
  document.getElementById(id)?.addEventListener('click', async (e) => {
    e.preventDefault();
    try {
      const result = await handler(e);
      assertApiSuccess(result, '操作に失敗しました');
    } catch (error) {
      showNotification(error.message || '操作に失敗しました', 'error');
    }
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

export function captureThemeRead() {
  return ++themeReadGeneration;
}

export function applyThemeSnapshot(theme, generation) {
  if (generation !== themeReadGeneration || themePendingSaves > 0) return;
  acknowledgedTheme = theme;
  State.theme = theme;
  lsSet('theme', theme);
  document.documentElement.dataset.theme = theme === 'default' ? '' : theme;
  if (El.themeSelect) El.themeSelect.value = theme;
}

function closeModal(modal) {
  if (!modal) return;
  if (modal === El.removeModal) removeTransaction = null;
  if (modal === El.tagEditModal) {
    ++tagEditorGeneration;
    ++tagReadGeneration;
    hideTagSuggestions();
  }
  if (modal === El.diffModal) ++diffGeneration;
  if (modal === El.notepadModal) {
    ++notepadOpenGeneration;
    notepadEditor?.invalidateReads();
  }
  modal.classList.add('hide');
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

function getNotepadEditor() {
  if (!notepadEditor) {
    notepadEditor = createNotepadEditor(El.notepad);
  }
  return notepadEditor;
}

export function applyNotepadSnapshot(data) {
  if (!El.notepad) return { keptLocalEdits: false };
  return getNotepadEditor().applySnapshot(data);
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

export function clearConsoleHistoryUi() {
  State.consoleRevision++;
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
  clone.querySelectorAll('[id]').forEach(el => {
    const originalId = el.id;
    el.dataset.mainControlId = originalId;
    el.id = 'footer-' + originalId;
  });
  clone.querySelectorAll('[data-dropdown]').forEach(el => {
    el.dataset.dropdown = 'footer-' + el.dataset.dropdown;
  });
  while (clone.firstChild) dst.appendChild(clone.firstChild);

  // Footer controls forward actions by captured source ID, never by label.
  dst.addEventListener('click', (e) => {
    const link = e.target.closest('a, button');
    if (!link || link.hasAttribute('data-dropdown')) return;
    const mainEl = document.getElementById(link.dataset.mainControlId);
    if (mainEl) {
      e.preventDefault();
      mainEl.click();
    }
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


/* ===== Tag editor ===== */

function isCurrentTagEditor(ids, generation = tagEditorGeneration) {
  return generation === tagEditorGeneration
    && El.tagEditModal && !El.tagEditModal.classList.contains('hide')
    && El.tagEditModal.dataset.ids === JSON.stringify(ids);
}

function setTagEditorPending() {
  for (const control of [El.addTagButton, El.newTagInput]) {
    if (control) control.disabled = tagMutationPending;
  }
  El.tagEditorCurrent?.querySelectorAll('.tag-remove').forEach(control => {
    control.disabled = tagMutationPending;
  });
}

async function mutateEditorTags(ids, states, fallbackMessage, onSuccess) {
  if (tagMutationPending || !isCurrentTagEditor(ids)) return;
  const generation = tagEditorGeneration;
  tagMutationPending = true;
  ++tagReadGeneration;
  setTagEditorPending();
  try {
    await applyBulkTagEdit(ids, states, fallbackMessage);
    if (isCurrentTagEditor(ids, generation)) onSuccess?.();
    await refreshList();
    await refreshTags();
    if (isCurrentTagEditor(ids, generation)) await refreshTagEditor(ids);
  } catch (error) {
    showNotification(error.message || fallbackMessage, 'error');
  } finally {
    tagMutationPending = false;
    setTagEditorPending();
  }
}

function closeTagEditor() {
  closeModal(El.tagEditModal);
}

async function refreshOpenTagEditor() {
  if (!El.tagEditModal || El.tagEditModal.classList.contains('hide')) return;
  const idsJson = El.tagEditModal.dataset.ids;
  if (!idsJson) return;
  const ids = JSON.parse(idsJson);
  if (Array.isArray(ids) && ids.length > 0) {
    await refreshTagEditor(ids);
  }
}

async function openTagEditor(ids) {
  const targetIds = [...(ids || requireSelectedIds() || [])];
  if (!targetIds || targetIds.length === 0) return;
  const generation = ++tagEditorGeneration;
  ++tagReadGeneration;

  El.tagEditModal?.classList.remove('hide');
  El.tagEditModal.dataset.ids = JSON.stringify(targetIds);
  El.tagEditModal.dataset.selectedCount = String(targetIds.length);
  renderTagEditorSummary(targetIds.length, { loading: true });
  renderTagEditorTags(targetIds, []);
  renderFavoriteTags();
  setTagEditorPending();

  if (El.newTagInput) {
    El.newTagInput.value = '';
    El.newTagInput.focus();
  }
  hideTagSuggestions();

  try {
    await refreshTagEditor(targetIds);
  } catch (error) {
    if (!isCurrentTagEditor(targetIds, generation)) return;
    renderTagEditorSummary(targetIds.length, { error: true });
    renderTagEditorTags(targetIds, []);
    showNotification(error.message || 'タグ情報の取得に失敗しました', 'error');
  }
}

async function addTagFromInput() {
  const input = El.newTagInput;
  if (!input || tagMutationPending) return;
  const submitted = input.value;
  const tags = splitTagInput(submitted);
  if (tags.length === 0) return;

  const idsJson = El.tagEditModal?.dataset.ids;
  const ids = idsJson ? JSON.parse(idsJson) : [...State.selectedIds];
  const states = Object.fromEntries(tags.map(tag => [tag, 2]));

  await mutateEditorTags(ids, states, 'タグの追加に失敗しました', () => {
    if (input.value === submitted) input.value = '';
    hideTagSuggestions();
  });
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

// 入力欄のタグをお気に入りに登録する (作品への付与はしない)。
async function addFavoriteTagFromInput() {
  const input = El.newTagInput;
  if (!input) return;
  const tags = splitTagInput(input.value);
  if (tags.length === 0) return;
  await setFavoriteTags({ action: 'add', tags });
  showNotification(`「${tags.join('」「')}」をお気に入りに登録しました`, 'success');
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
  if (!isCurrentTagEditor(ids)) return;
  const generation = ++tagReadGeneration;
  const editorGeneration = tagEditorGeneration;
  const selectionCount = ids.length;
  const taginfo = await postJson('/api/taginfo.json', {
    ids,
  });
  if (generation !== tagReadGeneration || !isCurrentTagEditor(ids, editorGeneration)) return;
  const selectedTagInfo = Array.isArray(taginfo)
    ? taginfo.filter(info => Number(info?.count || 0) > 0)
    : [];
  renderTagEditorSummary(selectionCount);
  renderTagEditorTags(ids, selectedTagInfo);
  syncFavoriteStars();
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

    const colorBtn = document.createElement('button');
    colorBtn.type = 'button';
    colorBtn.className = 'tag-color-edit';
    colorBtn.textContent = '色';
    colorBtn.title = `「${info.tag}」の色を変更`;
    colorBtn.setAttribute('aria-label', colorBtn.title);
    colorBtn.addEventListener('click', (event) => {
      showTagColorMenu(event, info.tag);
    });
    chip.appendChild(colorBtn);

    const favoriteBtn = document.createElement('button');
    favoriteBtn.type = 'button';
    favoriteBtn.className = 'tag-favorite-edit';
    const isFavorite = (State.favoriteTags || []).includes(info.tag);
    favoriteBtn.textContent = isFavorite ? '★' : '☆';
    favoriteBtn.title = isFavorite
      ? `「${info.tag}」をお気に入りから外す`
      : `「${info.tag}」をお気に入りに登録する`;
    favoriteBtn.setAttribute('aria-label', favoriteBtn.title);
    favoriteBtn.addEventListener('click', async (event) => {
      event.stopPropagation();
      await toggleFavoriteTag(info.tag);
    });
    chip.appendChild(favoriteBtn);
    chip.addEventListener('contextmenu', (event) => {
      showTagColorMenu(event, info.tag);
    });

    const removeBtn = document.createElement('button');
    removeBtn.type = 'button';
    removeBtn.className = 'tag-remove';
    removeBtn.disabled = tagMutationPending;
    removeBtn.textContent = '×';
    removeBtn.title =
      selectionCount > 1
        ? `選択中の${selectionCount}件から「${info.tag}」を外す`
        : `「${info.tag}」を外す`;
    removeBtn.addEventListener('click', async () => {
      await mutateEditorTags(ids, { [info.tag]: 0 }, 'タグの削除に失敗しました');
    });
    chip.appendChild(removeBtn);
    container.appendChild(chip);
  });
}

function tagColorClass(tag) {
  const colorName = State.tagColors?.[tag] || 'default';
  return TAG_COLOR_MAP[colorName] || 'tag-default';
}

/* ===== お気に入りタグ ===== */

// お気に入りタグの一覧を描画する。登録済みのタグは常設表示し、クリックで
// 入力欄へ入れる。解除ボタンで登録から外す。
function renderFavoriteTags() {
  const list = El.tagFavoritesList;
  const empty = El.tagFavoritesEmpty;
  if (!list) return;
  list.textContent = '';
  const favorites = Array.isArray(State.favoriteTags) ? State.favoriteTags : [];
  if (empty) {
    empty.classList.toggle('hide', favorites.length > 0);
  }
  favorites.forEach((tag) => {
    const chip = document.createElement('span');
    chip.className = `tag-label ${tagColorClass(tag)} tag-favorite`;
    chip.dataset.tag = tag;
    chip.textContent = tag;
    chip.title = `「${tag}」を入力欄へ入れる`;

    const addBtn = document.createElement('button');
    addBtn.type = 'button';
    addBtn.className = 'tag-favorite-add';
    addBtn.textContent = '追加';
    addBtn.title = `「${tag}」をタグに追加する`;
    addBtn.addEventListener('click', async (event) => {
      event.stopPropagation();
      await addFavoriteTagToSelection(tag);
    });
    chip.appendChild(addBtn);

    const removeBtn = document.createElement('button');
    removeBtn.type = 'button';
    removeBtn.className = 'tag-remove';
    removeBtn.textContent = '×';
    removeBtn.title = `「${tag}」をお気に入りから外す`;
    removeBtn.addEventListener('click', async (event) => {
      event.stopPropagation();
      await setFavoriteTags({ action: 'remove', tags: [tag] });
    });
    chip.appendChild(removeBtn);

    chip.addEventListener('click', () => {
      if (!El.newTagInput) return;
      El.newTagInput.value = tag;
      hideTagSuggestions();
      El.newTagInput.focus();
    });

    list.appendChild(chip);
  });
}

// お気に入りタグの登録 / 解除を切り替える。
async function toggleFavoriteTag(tag) {
  const isFavorite = (State.favoriteTags || []).includes(tag);
  await setFavoriteTags({
    action: isFavorite ? 'remove' : 'add',
    tags: [tag],
  });
}

// お気に入りの登録状態を更新して一覧を描き直す。
async function setFavoriteTags(body) {
  try {
    const result = await postJson('/api/favorite_tags', body);
    if (!result || result.success !== true) {
      showNotification(result?.message || 'お気に入りの更新に失敗しました', 'error');
      return;
    }
    State.favoriteTags = Array.isArray(result.tags) ? result.tags : [];
    renderFavoriteTags();
    syncFavoriteStars();
  } catch (error) {
    showNotification(error.message || 'お気に入りの更新に失敗しました', 'error');
  }
}

// 作品に付いているタグの ★ ボタンをお気に入りの状態に合わせて更新する。
function syncFavoriteStars() {
  const favorites = Array.isArray(State.favoriteTags) ? State.favoriteTags : [];
  El.tagEditorCurrent?.querySelectorAll('.tag-editable[data-tag]').forEach((chip) => {
    const button = chip.querySelector('.tag-favorite-edit');
    const tag = chip.dataset.tag;
    if (!button || !tag) return;
    const isFavorite = favorites.includes(tag);
    button.textContent = isFavorite ? '★' : '☆';
    button.title = isFavorite
      ? `「${tag}」をお気に入りから外す`
      : `「${tag}」をお気に入りに登録する`;
  });
}

// 選択中の小説へ、お気に入りタグを付ける。
async function addFavoriteTagToSelection(tag) {
  const ids = getCurrentEditorTargetIds();
  if (ids.length === 0) return;
  await mutateEditorTags(ids, { [tag]: 2 }, 'タグの追加に失敗しました');
}

// 現在のタグ編集モーダルで対象になっている小説 ID。
function getCurrentEditorTargetIds() {
  const idsJson = El.tagEditModal?.dataset.ids;
  if (idsJson) {
    try {
      const ids = JSON.parse(idsJson);
      if (Array.isArray(ids) && ids.length > 0) return ids;
    } catch { /* fall through */ }
  }
  return [...(State.selectedIds || [])];
}

async function applyBulkTagEdit(ids, states, fallbackMessage) {
  const result = await postJson('/api/edit_tag', {
    ids,
    states,
  });
  assertApiSuccess(result, fallbackMessage);
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

  const generation = ++notepadOpenGeneration;
  El.notepadModal?.classList.remove('hide');
  try {
    await getNotepadEditor().load();
    if (generation !== notepadOpenGeneration) return;
  } catch (error) {
    if (generation !== notepadOpenGeneration) return;
    showNotification(error.message || 'メモ帳の読み込みに失敗しました', 'error');
  }
}

/* ===== About ===== */

let virtualOrderState = null;

function openVirtualOrderModal(novel) {
  virtualOrderState = {
    id: novel.id,
    ncode: novel.ncode,
    members: novel.members.slice(),
    titles: (novel.member_titles || []).slice(),
  };
  const modal = document.getElementById('virtual-order-modal');
  if (!modal) return;
  modal.classList.remove('hide');
  renderVirtualOrderList();
}

function renderVirtualOrderList() {
  const list = document.getElementById('virtual-order-list');
  const state = virtualOrderState;
  if (!list || !state) return;
  list.replaceChildren();
  state.members.forEach((memberId, index) => {
    const row = document.createElement('div');
    row.className = 'hidden-item';
    const label = document.createElement('span');
    label.className = 'virtual-order-title';
    const title = state.titles[index] || `ID:${memberId}`;
    label.textContent = `${index + 1}. ${title} (ID:${memberId})`;
    row.appendChild(label);

    const up = document.createElement('button');
    up.type = 'button';
    up.className = 'btn btn-default btn-sm';
    up.textContent = '↑';
    up.title = '上へ';
    up.disabled = index === 0;
    up.addEventListener('click', () => {
      if (index === 0) return;
      [state.members[index - 1], state.members[index]] = [
        state.members[index],
        state.members[index - 1],
      ];
      [state.titles[index - 1], state.titles[index]] = [
        state.titles[index],
        state.titles[index - 1],
      ];
      renderVirtualOrderList();
    });
    row.appendChild(up);

    const down = document.createElement('button');
    down.type = 'button';
    down.className = 'btn btn-default btn-sm';
    down.textContent = '↓';
    down.title = '下へ';
    down.disabled = index === state.members.length - 1;
    down.addEventListener('click', () => {
      if (index === state.members.length - 1) return;
      [state.members[index + 1], state.members[index]] = [
        state.members[index],
        state.members[index + 1],
      ];
      [state.titles[index + 1], state.titles[index]] = [
        state.titles[index],
        state.titles[index + 1],
      ];
      renderVirtualOrderList();
    });
    row.appendChild(down);
    list.appendChild(row);
  });
}

async function saveVirtualOrder() {
  const state = virtualOrderState;
  if (!state) return;
  try {
    const response = await fetch(`/api/virtual_novels/${state.id}/order`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ members: state.members }),
    });
    const result = await response.json();
    if (!response.ok || !result.success) {
      throw new Error(result.message || '並べ替えに失敗しました');
    }
    closeModal(document.getElementById('virtual-order-modal'));
    showNotification(`順序を変更しました: ${state.ncode}`, 'info');
    await refreshList();
  } catch (error) {
    showNotification(error.message || '並べ替えに失敗しました', 'error');
  }
}

async function openHiddenSettings() {  const modal = document.getElementById('hidden-modal');
  if (!modal) return;
  modal.classList.remove('hide');
  await refreshHiddenSettings();
}

async function refreshHiddenSettings() {
  let data;
  try {
    data = await fetchJson('/api/hidden');
  } catch {
    return;
  }
  const render = (containerId, values, kind) => {
    const box = document.getElementById(containerId);
    if (!box) return;
    box.textContent = '';
    const list = Array.isArray(values) ? values : [];
    if (!list.length) {
      const empty = document.createElement('span');
      empty.className = 'hidden-empty';
      empty.textContent = 'なし';
      box.appendChild(empty);
      return;
    }
    for (const value of list) {
      const row = document.createElement('span');
      row.className = 'hidden-item';
      const label = document.createElement('span');
      label.textContent = String(value);
      const remove = document.createElement('button');
      remove.type = 'button';
      remove.className = 'hidden-remove';
      remove.textContent = '×';
      remove.title = '表示に戻す';
      remove.addEventListener('click', async () => {
        await postJson('/api/hidden', { action: 'remove', [kind]: [kind === 'novels' ? Number(value) : value] });
        await refreshHiddenSettings();
        await refreshList();
      });
      row.appendChild(label);
      row.appendChild(remove);
      box.appendChild(row);
    }
  };
  render('hidden-novel-list', data?.novels, 'novels');
  render('hidden-tag-list', data?.tags, 'tags');
  render('hidden-author-list', data?.authors, 'authors');
  render('hidden-site-list', data?.sites, 'sites');
}

async function addHiddenValue(kind, inputId) {
  const input = document.getElementById(inputId);
  if (!input) return;
  const raw = input.value.trim();
  if (!raw) return;
  const value = kind === 'novels' ? Number(raw) : raw;
  if (kind === 'novels' && !Number.isInteger(value)) {
    showNotification('作品IDを数字で入力してください', 'error');
    return;
  }
  await postJson('/api/hidden', { action: 'add', [kind]: [value] });
  input.value = '';
  await refreshHiddenSettings();
  await refreshList();
}

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
  if (El.aboutUpdate) {
    El.aboutUpdate.classList.add('hide');
    El.aboutUpdate.disabled = false;
  }
  if (El.aboutUpdateStatus) {
    El.aboutUpdateStatus.classList.add('hide');
    El.aboutUpdateStatus.textContent = '';
  }
  void updateLatestVersionInfo();
}

async function updateLatestVersionInfo() {
  if (!El.aboutLatestVersion) return;
  try {
    const data = await fetchJson('/api/version/latest.json');
    latestVersionData = data?.success ? data : null;
    if (data?.success) {
      const latest = data.latest_version || '-';
      const current = data.current_version || '-';
      El.aboutLatestVersion.textContent =
        `最新バージョン: ${latest}${data.update_available ? ' (更新あり)' : ' (最新)'} / 現在: ${current}`;
      if (El.aboutUpdate) {
        if (data.update_available && data.self_update_supported !== false) {
          El.aboutUpdate.classList.remove('hide');
          El.aboutUpdate.disabled = false;
        } else {
          El.aboutUpdate.classList.add('hide');
        }
      }
      if (El.aboutUpdateStatus) {
        if (data.update_available && data.self_update_supported === false) {
          El.aboutUpdateStatus.classList.remove('hide');
          El.aboutUpdateStatus.textContent = data.self_update_unavailable_reason ||
            'この実行環境ではアップデートボタンは表示されません。';
        } else if (data.update_available && data.develop) {
          El.aboutUpdateStatus.classList.remove('hide');
          El.aboutUpdateStatus.textContent =
            'develop ビルド扱いのためアップデートボタンは表示されません。' +
            ' (実行ファイルと同じフォルダに commitversion ファイルが無い場合に発生します)';
        } else {
          El.aboutUpdateStatus.classList.add('hide');
          El.aboutUpdateStatus.textContent = '';
        }
      }
      return;
    }
    El.aboutLatestVersion.textContent =
      `最新バージョン: 取得失敗${data?.message ? ' (' + data.message + ')' : ''}`;
  } catch (e) {
    El.aboutLatestVersion.textContent = '最新バージョン: 取得失敗';
  }
}

let latestVersionData = null;

async function startSelfUpdate(variant) {
  if (!confirm('最新バージョンへ更新します。WEBサーバーは一旦停止し、自動的に再起動します。よろしいですか？')) return;
  const btn = El.aboutUpdate;
  if (btn) btn.disabled = true;
  if (El.aboutUpdateStatus) {
    El.aboutUpdateStatus.classList.remove('hide');
    El.aboutUpdateStatus.textContent = 'アップデートを開始しています...';
  }
  try {
    const body = variant ? { variant } : {};
    const result = await postJson('/api/update/start', body);
    if (!result?.success) {
      throw new Error(result?.message || 'アップデート開始に失敗しました');
    }
    if (El.aboutUpdateStatus) {
      El.aboutUpdateStatus.textContent = 'ダウンロード中... 完了後に再起動します';
    }
    // Server triggers reboot; the existing /_rebooting page handles reconnection.
    setTimeout(() => { window.location.href = '/_rebooting'; }, 1500);
  } catch (error) {
    if (El.aboutUpdateStatus) {
      El.aboutUpdateStatus.textContent = `アップデート失敗: ${error.message || error}`;
    }
    if (btn) btn.disabled = false;
    showNotification(error.message || 'アップデートに失敗しました', 'error');
  }
}

/* ===== Diff list ===== */

async function openDiffList(ids) {
  const container = El.diffListContainer;
  if (!container) return;
  const generation = ++diffGeneration;
  const targetIds = [...ids];
  container.innerHTML = '<p>読み込み中...</p>';
  El.diffModal?.classList.remove('hide');

  try {
    const data = await postJson('/api/diff_list', { targets: targetIds });
    if (generation !== diffGeneration || El.diffModal?.classList.contains('hide')) return;
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
    if (generation !== diffGeneration || El.diffModal?.classList.contains('hide')) return;
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
  closeModal(El.removeModal);
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

  removeTransaction = { ids: numericIds };
  El.removeOk.disabled = removePending;
}

async function confirmRemove() {
  const transaction = removeTransaction;
  if (!transaction || removePending || El.removeModal?.classList.contains('hide')) return;
  const withFile = El.removeWithFile.checked;
  removePending = true;
  El.removeOk.disabled = true;
  try {
    const result = await postJson('/api/novels/remove', {
      ids: transaction.ids,
      with_file: withFile,
    });
    assertApiSuccess(result, '削除に失敗しました');
    if (removeTransaction === transaction) closeModal(El.removeModal);
    await refreshList();
  } catch (error) {
    showNotification(error.message || '削除に失敗しました', 'error');
  } finally {
    removePending = false;
    El.removeOk.disabled = false;
  }
}

/* ===== CSV ===== */

async function downloadCsv() {
  const resp = await request('/api/csv/download');
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

const refreshRequests = new Map();
let listRefreshGeneration = 0;

function runRefresh(key, action) {
  const existing = refreshRequests.get(key);
  if (existing) {
    existing.dirty = true;
    return existing.promise;
  }
  const work = { dirty: false, promise: null };
  work.promise = Promise.resolve().then(async () => {
    let result;
    do {
      work.dirty = false;
      result = await action();
    } while (work.dirty);
    return result;
  }).finally(() => refreshRequests.delete(key));
  refreshRequests.set(key, work);
  return work.promise;
}

export async function refreshList(redraw = true) {
  ++listRefreshGeneration;
  return runRefresh('list', async () => {
    const generation = listRefreshGeneration;
    try {
      const resp = await fetchJson(`/api/list?all=true${State.showHidden ? '&show_hidden=true' : ''}`);
      if (generation !== listRefreshGeneration) return;
      if (resp && Array.isArray(resp.data)) {
        State.novels = resp.data;
        State.frozenIds = new Set(resp.data.filter(n => n.frozen).map(n => String(n.id)));
        pruneSelectedIdsToCurrentList();
      }
    } catch { /* keep the last successfully loaded list */ }
    if (redraw && generation === listRefreshGeneration) renderNovelList();
  });
}

export async function refreshQueue() {
  return runRefresh('queue', async () => {
    try {
      const data = await fetchJson('/api/queue/status');
      if (data) {
        State.queueStatus = data;
        renderQueueStatus();
        return data;
      }
    } catch { /* keep the last successfully loaded status */ }
    return null;
  });
}

export async function refreshQueueDetailed() {
  return runRefresh('queue-details', async () => {
    try {
      const pending = [];
      const running = [];
      const cursors = new Set();
      let cursor = null;
      let data;
      do {
        const query = cursor ? '?cursor=' + encodeURIComponent(cursor) : '';
        data = await fetchJson('/api/get_pending_tasks' + query);
        if (!data || !Array.isArray(data.pending) || !Array.isArray(data.running)) {
          throw new Error('待機キューの応答が不正です');
        }
        pending.push(...data.pending);
        running.push(...data.running);
        cursor = data.next_cursor;
        if (cursor && cursors.has(cursor)) throw new Error('待機キューのページ取得が進みません');
        if (cursor) cursors.add(cursor);
      } while (cursor);
      State.queueDetailed = { ...data, pending, running };
      renderQueueDetailed();
    } catch (error) {
      showNotification(error.message || '待機キューを取得できませんでした', 'error');
    }
  });
}

export async function refreshTags(redraw = true) {
  return runRefresh('tags', async () => {
    try {
      const data = await fetchJson('/api/tag_list?format=json');
      if (data) {
        State.tags = data.tags || [];
        State.tagColors = data.tag_colors || data.colors || {};
        renderTagList();
        if (redraw) renderNovelList();
      }
    } catch { /* keep the last successfully loaded tags */ }
  });
}

// お気に入りタグの一覧を取得する (失敗しても操作は続けられる)。
export async function refreshFavoriteTags() {
  try {
    const data = await fetchJson('/api/favorite_tags');
    if (data && data.success !== false) {
      State.favoriteTags = Array.isArray(data.tags) ? data.tags : [];
      renderFavoriteTags();
    }
  } catch { /* keep the last successfully loaded favorites */ }
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
