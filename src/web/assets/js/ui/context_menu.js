/**
 * Context menu — right-click actions on table rows
 */
import { El } from '../core/state.js';
import { postJson } from '../core/http.js';

let contextTarget = null;
let actionHandlers = {};
let menuTextCache = null;

const MENU_TEXT_KEY = 'context_menu_text';
const MENU_STYLE_KEY = 'menu_style';
const DEFAULT_COMMANDS = [
  'setting', 'diff', 'edit_tag', 'divider',
  'freeze_toggle', 'update', 'update_force', 'send', 'divider',
  'remove', 'convert', 'inspect', 'divider', 'folder', 'backup', 'download_force',
  'mail', 'author_comments',
];
const MENU_ITEMS = [
  { label: '――――――――(区切り)', command: 'divider' },
  { label: '小説の変換設定', command: 'setting' },
  { label: '差分を表示', command: 'diff' },
  { label: 'タグを編集', command: 'edit_tag' },
  { label: '凍結 or 解凍', command: 'freeze_toggle' },
  { label: '更新', command: 'update' },
  { label: '凍結済みでも更新', command: 'update_force' },
  { label: '送信', command: 'send' },
  { label: '削除', command: 'remove' },
  { label: '変換', command: 'convert' },
  { label: '調査状況ログを表示', command: 'inspect' },
  { label: '保存フォルダを開く', command: 'folder' },
  { label: 'バックアップを作成', command: 'backup' },
  { label: '再ダウンロード', command: 'download_force' },
  { label: 'メールで送信', command: 'mail' },
  { label: '作者コメント表示', command: 'author_comments' },
];
const MENU_COMMAND_HANDLERS = {
  setting: () => actionHandlers.openSetting?.(contextTarget),
  diff: () => actionHandlers.showDiff?.(contextTarget),
  edit_tag: () => actionHandlers.tagEditSingle?.(contextTarget),
  freeze_toggle: () => actionHandlers.freezeToggle?.(contextTarget),
  update: () => actionHandlers.updateSingle?.(contextTarget),
  update_force: () => actionHandlers.updateForceSingle?.(contextTarget),
  send: () => actionHandlers.sendSingle?.(contextTarget),
  remove: () => actionHandlers.removeSingle?.(contextTarget),
  convert: () => actionHandlers.convertSingle?.(contextTarget),
  inspect: () => actionHandlers.inspectSingle?.(contextTarget),
  folder: () => actionHandlers.folderSingle?.(contextTarget),
  backup: () => actionHandlers.backupSingle?.(contextTarget),
  download_force: () => actionHandlers.downloadForceSingle?.(contextTarget),
  mail: () => actionHandlers.mailSingle?.(contextTarget),
  author_comments: () => actionHandlers.authorComments?.(contextTarget),
};

export function setContextHandlers(handlers) {
  actionHandlers = handlers;
}

export function initContextMenu() {
  const menu = El.contextMenu;
  if (!menu) return;

  rebuildContextMenu();

  // Close on outside click
  document.addEventListener('click', (e) => {
    if (!menu.contains(e.target)) {
      menu.classList.add('hide');
    }
  });

  // Close on scroll
  window.addEventListener('scroll', () => {
    menu.classList.add('hide');
  });

  menu.addEventListener('click', (e) => {
    const link = e.target.closest('a[data-command]');
    if (!link || !menu.contains(link)) return;
    e.preventDefault();
    menu.classList.add('hide');
    const command = link.dataset.command;
    MENU_COMMAND_HANDLERS[command]?.();
  });

  window.addEventListener('storage', (e) => {
    if (!e.key || e.key === MENU_TEXT_KEY) {
      rebuildContextMenu();
    }
  });
}

export function showContextMenu(e, novelId) {
  e.preventDefault();
  contextTarget = novelId;

  const menu = El.contextMenu;
  if (!menu) return;
  rebuildContextMenu();

  // Position menu
  menu.classList.remove('hide');
  const menuW = menu.offsetWidth;
  const menuH = menu.offsetHeight;
  const style = getStoredMenuStyle();
  let x = e.clientX;
  let y = e.clientY;

  if (window.innerWidth < x + menuW) {
    x -= menuW;
  }
  if (window.innerHeight < y + menuH) {
    if (style === 'windows') {
      y -= menuH;
    } else {
      y -= (y + menuH) - window.innerHeight + 5;
    }
  }
  if (x < 0) x = 0;
  if (y < 0) y = 0;

  menu.style.left = x + 'px';
  menu.style.top = y + 'px';
}

// Tag color context menu
export function initTagColorMenu() {
  const menu = El.selectColorMenu;
  if (!menu) return;

  document.addEventListener('click', (e) => {
    if (!menu.contains(e.target)) {
      menu.classList.add('hide');
    }
  });

  menu.querySelectorAll('[data-color]').forEach(el => {
    el.addEventListener('click', (e) => {
      e.preventDefault();
      const color = el.dataset.color;
      const tagName = menu.dataset.tagName;
      if (tagName && color) {
        changeTagColor(tagName, color);
      }
      menu.classList.add('hide');
    });
  });
}

export function showTagColorMenu(e, tagName) {
  e.preventDefault();
  const menu = El.selectColorMenu;
  if (!menu) return;

  menu.dataset.tagName = tagName;
  menu.classList.remove('hide');

  let x = e.clientX;
  let y = e.clientY;
  menu.style.left = x + 'px';
  menu.style.top = y + 'px';
}

async function changeTagColor(tagName, color) {
  try {
    await postJson('/api/tag/change_color', { tag: tagName, color });
    actionHandlers.refreshTags?.();
    actionHandlers.refreshList?.();
  } catch { /* ignore */ }
}

export function getStoredMenuStyle() {
  return getStorageValue(MENU_STYLE_KEY, 'windows') === 'mac' ? 'mac' : 'windows';
}

export function setStoredMenuStyle(style) {
  setStorageValue(MENU_STYLE_KEY, style === 'mac' ? 'mac' : 'windows');
}

function rebuildContextMenu() {
  const menu = El.contextMenu;
  if (!menu) return;

  const menuText = getStoredMenuText();
  if (menuText === menuTextCache) return;

  menuTextCache = menuText;
  menu.textContent = '';

  for (const line of menuText.split('\n')) {
    const [label = '', command = ''] = line.split('<>');
    if (!command) continue;

    const li = document.createElement('li');
    if (command === 'divider') {
      li.className = 'divider';
      menu.appendChild(li);
      continue;
    }

    li.className = `context-menu-${command}`;
    const link = document.createElement('a');
    link.href = '#';
    link.dataset.command = command;
    link.textContent = label;
    li.appendChild(link);
    menu.appendChild(li);
  }
}

function getStoredMenuText() {
  return getStorageValue(MENU_TEXT_KEY, createDefaultMenuText()).trim();
}

function createDefaultMenuText() {
  const lines = [];
  for (const command of DEFAULT_COMMANDS) {
    const item = MENU_ITEMS.find(entry => entry.command === command);
    if (item) lines.push(item.label + '<>' + item.command);
  }
  return lines.join('\n');
}

function getStorageValue(key, fallback) {
  try {
    const value = localStorage.getItem(key);
    return value !== null ? value : fallback;
  } catch {
    return fallback;
  }
}

function setStorageValue(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch { /* ignore */ }
}
