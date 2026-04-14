/**
 * Keyboard shortcuts — mirrors narou.rb's hotkey bindings
 */
import { State, El, lsSet } from '../core/state.js';

let actionHandlers = {};

export function setShortcutHandlers(handlers) {
  actionHandlers = handlers;
}

export function initShortcuts() {
  document.addEventListener('keydown', handleKeyDown);
}

function isInputFocused() {
  const el = document.activeElement;
  if (!el) return false;
  const tag = el.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el.isContentEditable;
}

function handleKeyDown(e) {
  // Don't capture when typing in inputs/textareas
  if (isInputFocused()) return;

  const key = e.key.toUpperCase();
  const ctrl = e.ctrlKey || e.metaKey;
  const shift = e.shiftKey;

  // Ctrl+A — select visible novels
  if (ctrl && !shift && key === 'A') {
    e.preventDefault();
    actionHandlers.selectView?.();
    return;
  }

  // Shift+A — select ALL novels
  if (!ctrl && shift && key === 'A') {
    e.preventDefault();
    actionHandlers.selectAll?.();
    return;
  }

  // Ctrl+Shift+A — deselect all
  if (ctrl && shift && key === 'A') {
    e.preventDefault();
    actionHandlers.selectClear?.();
    return;
  }

  // ESC — clear selection / close modals
  if (key === 'ESCAPE') {
    e.preventDefault();
    // Close any open modal first
    const openModal = document.querySelector('.modal-overlay:not(.hide)');
    if (openModal) {
      openModal.classList.add('hide');
      return;
    }
    // Close context menu
    const ctx = El.contextMenu;
    if (ctx && !ctx.classList.contains('hide')) {
      ctx.classList.add('hide');
      return;
    }
    actionHandlers.selectClear?.();
    return;
  }

  // F5 — refresh (standard browser behavior, but also refresh list)
  if (key === 'F5') {
    actionHandlers.refreshAll?.();
    return;
  }

  // W — toggle wide mode
  if (key === 'W') {
    actionHandlers.toggleWide?.();
    return;
  }

  // F — show frozen
  if (!shift && key === 'F') {
    actionHandlers.viewFrozen?.();
    return;
  }

  // Shift+F — show non-frozen
  if (shift && key === 'F') {
    actionHandlers.viewNonfrozen?.();
    return;
  }

  // S — single select mode
  if (key === 'S') {
    actionHandlers.selectModeSingle?.();
    return;
  }

  // R — rect select mode
  if (key === 'R') {
    actionHandlers.selectModeRect?.();
    return;
  }

  // H — hybrid select mode
  if (key === 'H') {
    actionHandlers.selectModeHybrid?.();
    return;
  }

  // T — tag edit (if selection exists)
  if (key === 'T') {
    if (State.selectedIds.size > 0) {
      actionHandlers.tagEdit?.();
    }
    return;
  }
}
