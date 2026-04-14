import { elements, state } from "../core/state.js";

const I18N = {
  ja: {
    pageTitle: "narou.rs WEB UI",
    queueLabel: "キュー",
    selectedLabel: "選択",
    filterLabel: "絞り込み",
    filterPlaceholder: "タイトル / 作者 / タグ",
    reloadButton: "再読込",
    downloadButton: "Download",
    updateButton: "Update",
    convertButton: "Convert",
    freezeButton: "Freeze",
    unfreezeButton: "Unfreeze",
    clearButton: "クリア",
    removeButton: "Remove",
    columnId: "ID",
    columnTitle: "タイトル",
    columnAuthor: "作者",
    columnSite: "サイト",
    columnUpdated: "更新日",
    columnLatest: "最新話掲載日",
    columnTags: "タグ",
    columnStatus: "状態",
    columnActions: "操作",
    queueHeading: "キュー",
    clearQueueButton: "キュー消去",
    queuePending: "待機",
    queueCompleted: "完了",
    queueFailed: "失敗",
    consoleHeading: "コンソール",
    notepadHeading: "メモ帳",
    saveButton: "保存",
    notepadPlaceholder: "メモ帳",
    tagsHeading: "タグ",
    emptyState: "対象の小説がありません。",
    statusFrozen: "凍結",
    statusEnd: "完結",
    statusActive: "通常",
    actionUpdate: "Update",
    actionConvert: "Convert",
    actionFreeze: "Freeze",
    actionUnfreeze: "Unfreeze",
    actionTags: "タグ",
    actionRemove: "Remove",
    initMessage: "WEB UI を初期化しました",
    reloadMessage: "一覧・タグ・キュー・メモ帳を再読込しました",
    notepadSaved: "メモ帳を保存しました",
    wsConnected: "WebSocket に接続しました",
    wsDisconnected: "WebSocket が切断されました",
    wsError: "WebSocket エラー",
    queueDownloadPrompt: "ダウンロード対象を空白または改行区切りで指定",
    queueDownloadQueued: "ダウンロードをキューに追加しました",
    queueUpdateAllConfirm: "選択がないため全小説を update しますか?",
    queueUpdateAllQueued: "全小説の update をキューに追加しました",
    queueUpdateQueued: "update をキューに追加しました",
    queueConvertAlert: "変換する小説を選択して下さい",
    queueConvertPrompt: "変換 device を指定",
    queueConvertQueued: "convert をキューに追加しました",
    batchSelectAlert: "小説を選択して下さい",
    removeSelectedAlert: "削除する小説を選択して下さい",
    removeSelectedConfirm: "件の小説を削除しますか?",
    rowRemoveConfirm: "を削除しますか?",
    tagsPrompt: "タグをカンマ区切りで指定",
    queueCleared: "キューを消去しました",
    languageChanged: "言語を切り替えました",
  },
  en: {
    pageTitle: "narou.rs WEB UI",
    queueLabel: "Queue",
    selectedLabel: "Selected",
    filterLabel: "Filter",
    filterPlaceholder: "title / author / tag",
    reloadButton: "Reload",
    downloadButton: "Download",
    updateButton: "Update",
    convertButton: "Convert",
    freezeButton: "Freeze",
    unfreezeButton: "Unfreeze",
    clearButton: "Clear",
    removeButton: "Remove",
    columnId: "ID",
    columnTitle: "Title",
    columnAuthor: "Author",
    columnSite: "Site",
    columnUpdated: "Updated",
    columnLatest: "Latest",
    columnTags: "Tags",
    columnStatus: "Status",
    columnActions: "Actions",
    queueHeading: "Queue",
    clearQueueButton: "Clear queue",
    queuePending: "Pending",
    queueCompleted: "Completed",
    queueFailed: "Failed",
    consoleHeading: "Console",
    notepadHeading: "Notepad",
    saveButton: "Save",
    notepadPlaceholder: "Notepad",
    tagsHeading: "Tags",
    emptyState: "No novels found.",
    statusFrozen: "Frozen",
    statusEnd: "End",
    statusActive: "Active",
    actionUpdate: "Update",
    actionConvert: "Convert",
    actionFreeze: "Freeze",
    actionUnfreeze: "Unfreeze",
    actionTags: "Tags",
    actionRemove: "Remove",
    initMessage: "WEB UI initialized",
    reloadMessage: "Reloaded list, tags, queue, and notepad",
    notepadSaved: "Notepad saved",
    wsConnected: "WebSocket connected",
    wsDisconnected: "WebSocket disconnected",
    wsError: "WebSocket error",
    queueDownloadPrompt: "Enter download targets separated by spaces or newlines",
    queueDownloadQueued: "Queued download",
    queueUpdateAllConfirm: "No novels selected. Queue update for all novels?",
    queueUpdateAllQueued: "Queued update for all novels",
    queueUpdateQueued: "Queued update",
    queueConvertAlert: "Select at least one novel to convert",
    queueConvertPrompt: "Choose convert device",
    queueConvertQueued: "Queued convert",
    batchSelectAlert: "Select at least one novel",
    removeSelectedAlert: "Select at least one novel to remove",
    removeSelectedConfirm: "novels will be removed. Continue?",
    rowRemoveConfirm: "will be removed. Continue?",
    tagsPrompt: "Edit tags as comma-separated values",
    queueCleared: "Queue cleared",
    languageChanged: "Language switched",
  },
};

export function loadPreferredLanguage() {
  const saved = window.localStorage.getItem("narou-rs-webui-language");
  return saved === "en" ? "en" : "ja";
}

export function setLanguage(language) {
  state.language = language === "en" ? "en" : "ja";
  window.localStorage.setItem("narou-rs-webui-language", state.language);
  applyTranslations();
}

export function applyTranslations() {
  document.documentElement.lang = state.language;
  document.title = t("pageTitle");

  document.querySelectorAll("[data-i18n]").forEach((element) => {
    element.textContent = t(element.dataset.i18n);
  });
  document.querySelectorAll("[data-i18n-placeholder]").forEach((element) => {
    element.placeholder = t(element.dataset.i18nPlaceholder);
  });

  elements.langJa.classList.toggle("is-active", state.language === "ja");
  elements.langEn.classList.toggle("is-active", state.language === "en");
}

export function t(key) {
  return I18N[state.language]?.[key] || I18N.ja[key] || key;
}
