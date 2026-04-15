/**
 * Internationalization: JP/EN translations
 */
const DICT = {
  ja: {
    menuView: '表示',
    menuSelect: '選択',
    menuTag: 'タグ',
    menuTool: 'ツール',
    viewAll: '全ての小説を表示',
    viewNonfrozen: '凍結中以外を表示',
    viewFrozen: '凍結中を表示',
    viewWide: '小説リストの幅を広げる',
    selectVisible: '表示されている小説を選択',
    selectAll: '全ての小説を選択',
    selectClear: '選択を全て解除',
    tagEdit: '選択した小説のタグを編集',
    toolNotepad: 'メモ帳',
    toolCsvDownload: 'CSV形式でリストをダウンロード',
    serverShutdown: 'サーバをシャットダウン',
    notepadTitle: 'メモ帳',
    saveButton: '保存',
    queueTitle: 'キュー',
    queuePending: '待機',
    queueCompleted: '完了',
    queueFailed: '失敗',
    clearQueueButton: 'キュー消去',
    colLastUpdate: '最終更新日',
    colGeneralLastup: '最新話掲載日',
    colTitle: 'タイトル',
    colAuthor: '作者',
    colSite: 'サイト名',
    colTags: 'タグ',
    colStatus: '状態',
    updateView: '表示されている小説を更新',
    updateForce: '凍結済みでも更新',
    freezeOn: '選択した小説を凍結',
    freezeOff: '選択した小説の凍結を解除',
    otherDiff: '選択した小説の最新の差分を表示',
    otherFolder: '選択した小説の保存フォルダを開く',
    otherBackup: '選択した小説のバックアップを作成',
    otherMail: '選択した小説をメールで送信',
    statusNew: '新着',
    statusUpdated: '更新',
    confirmRemove: '選択した小説を本当に削除しますか？',
    confirmShutdown: 'サーバをシャットダウンしますか？',
    remove_confirm_title: '選択した小説を削除しますか？',
    remove_with_file: '保存フォルダ・ファイルも一緒に削除する',
    cancel: 'キャンセル',
    remove_btn: '削除する',
  },
  en: {
    menuView: 'View',
    menuSelect: 'Select',
    menuTag: 'Tags',
    menuTool: 'Tools',
    viewAll: 'Show all novels',
    viewNonfrozen: 'Show non-frozen',
    viewFrozen: 'Show frozen only',
    viewWide: 'Widen novel list',
    selectVisible: 'Select visible novels',
    selectAll: 'Select all novels',
    selectClear: 'Clear selection',
    tagEdit: 'Edit tags for selected novels',
    toolNotepad: 'Notepad',
    toolCsvDownload: 'Download list as CSV',
    serverShutdown: 'Shutdown server',
    notepadTitle: 'Notepad',
    saveButton: 'Save',
    queueTitle: 'Queue',
    queuePending: 'Pending',
    queueCompleted: 'Completed',
    queueFailed: 'Failed',
    clearQueueButton: 'Clear Queue',
    colLastUpdate: 'Last Update',
    colGeneralLastup: 'Latest Post',
    colTitle: 'Title',
    colAuthor: 'Author',
    colSite: 'Site',
    colTags: 'Tags',
    colStatus: 'Status',
    updateView: 'Update visible novels',
    updateForce: 'Update even if frozen',
    freezeOn: 'Freeze selected',
    freezeOff: 'Unfreeze selected',
    otherDiff: 'Show latest diff',
    otherFolder: 'Open novel folder',
    otherBackup: 'Create backup',
    otherMail: 'Send by mail',
    statusNew: 'New',
    statusUpdated: 'Updated',
    confirmRemove: 'Really remove the selected novels?',
    confirmShutdown: 'Shutdown the server?',
    remove_confirm_title: 'Remove selected novels?',
    remove_with_file: 'Also delete saved folders and files',
    cancel: 'Cancel',
    remove_btn: 'Remove',
  },
};

export function t(key) {
  const lang = localStorage.getItem('narou-rs-webui-language') || 'ja';
  return (DICT[lang] && DICT[lang][key]) || (DICT.ja[key]) || key;
}

export function applyI18n() {
  const lang = localStorage.getItem('narou-rs-webui-language') || 'ja';
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const key = el.getAttribute('data-i18n');
    const text = (DICT[lang] && DICT[lang][key]) || (DICT.ja[key]);
    if (text) el.textContent = text;
  });
}

export function toggleLanguage() {
  const current = localStorage.getItem('narou-rs-webui-language') || 'ja';
  const next = current === 'ja' ? 'en' : 'ja';
  localStorage.setItem('narou-rs-webui-language', next);
  applyI18n();
}
