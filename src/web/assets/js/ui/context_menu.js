/**
 * Context menu — right-click actions on table rows
 */
import { State, El } from '../core/state.js';
import { postJson } from '../core/http.js';

let contextTarget = null;
let actionHandlers = {};

export function setContextHandlers(handlers) {
  actionHandlers = handlers;
}

export function initContextMenu() {
  const menu = El.contextMenu;
  if (!menu) return;

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

  // Bind menu item clicks
  bindItem('ctx-setting', () => actionHandlers.openSetting?.(contextTarget));
  bindItem('ctx-diff', () => actionHandlers.showDiff?.(contextTarget));
  bindItem('ctx-tag-edit', () => actionHandlers.tagEditSingle?.(contextTarget));
  bindItem('ctx-freeze-toggle', () => actionHandlers.freezeToggle?.(contextTarget));
  bindItem('ctx-update', () => actionHandlers.updateSingle?.(contextTarget));
  bindItem('ctx-update-force', () => actionHandlers.updateForceSingle?.(contextTarget));
  bindItem('ctx-send', () => actionHandlers.sendSingle?.(contextTarget));
  bindItem('ctx-remove', () => actionHandlers.removeSingle?.(contextTarget));
  bindItem('ctx-convert', () => actionHandlers.convertSingle?.(contextTarget));
  bindItem('ctx-inspect', () => actionHandlers.inspectSingle?.(contextTarget));
  bindItem('ctx-folder', () => actionHandlers.folderSingle?.(contextTarget));
  bindItem('ctx-backup', () => actionHandlers.backupSingle?.(contextTarget));
  bindItem('ctx-download-force', () => actionHandlers.downloadForceSingle?.(contextTarget));
  bindItem('ctx-mail', () => actionHandlers.mailSingle?.(contextTarget));
}

function bindItem(id, handler) {
  const el = document.getElementById(id);
  if (!el) return;
  el.addEventListener('click', (e) => {
    e.preventDefault();
    El.contextMenu.classList.add('hide');
    handler();
  });
}

export function showContextMenu(e, novelId) {
  e.preventDefault();
  contextTarget = novelId;

  const menu = El.contextMenu;
  if (!menu) return;

  // Update freeze label
  const freezeEl = document.getElementById('ctx-freeze-toggle');
  if (freezeEl) {
    const novel = State.novels.find(n => n.id === novelId);
    freezeEl.textContent = novel?.frozen ? '凍結解除' : '凍結';
  }

  // Position menu
  menu.classList.remove('hide');
  const menuW = menu.offsetWidth;
  const menuH = menu.offsetHeight;
  let x = e.clientX;
  let y = e.clientY;

  if (x + menuW > window.innerWidth) x = window.innerWidth - menuW - 4;
  if (y + menuH > window.innerHeight) y = window.innerHeight - menuH - 4;
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
