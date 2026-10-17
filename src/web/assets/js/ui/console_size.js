import { State, El, lsGet, lsSet, lsRemove } from '../core/state.js';

const HEIGHT_KEY = 'console-height-vh';
let container;
let panes;
let handle;
let drag = null;
let refreshFrame = null;

function bounds() {
  const style = getComputedStyle(panes);
  const min = Number.parseFloat(style.minHeight);
  return { min, max: Math.max(min, Number.parseFloat(style.maxHeight)) };
}

function refreshSize(pinned = { ...State.consolePinned }) {
  if (refreshFrame !== null) cancelAnimationFrame(refreshFrame);
  refreshFrame = requestAnimationFrame(() => {
    refreshFrame = null;
    for (const [key, log] of [['main', El.console], ['stdout2', El.consoleStdout2]]) {
      if (log && pinned[key]) log.scrollTop = log.scrollHeight;
    }
    const range = bounds();
    const height = panes.getBoundingClientRect().height;
    handle.setAttribute('aria-valuemin', String(Math.round(range.min)));
    handle.setAttribute('aria-valuemax', String(Math.round(range.max)));
    handle.setAttribute('aria-valuenow', String(Math.round(height)));
    handle.setAttribute('aria-valuetext', `${Math.round(height / innerHeight * 100)}%`);
  });
}

function setExpanded(expanded, pinned) {
  State.consoleExpanded = expanded;
  panes.classList.toggle('expanded', expanded);
  El.console?.classList.toggle('expanded', expanded);
  El.consoleStdout2?.classList.toggle('expanded', expanded);
  El.consoleExpand?.setAttribute('aria-expanded', String(expanded));
  El.consoleExpand?.setAttribute('aria-label', expanded ? 'コンソールを縮小' : 'コンソールを拡大');
  El.consoleExpand?.querySelector('.expand-icon')?.classList.toggle('hide', expanded);
  El.consoleExpand?.querySelector('.collapse-icon')?.classList.toggle('hide', !expanded);
  refreshSize(pinned);
}

function setHeight(height, pinned) {
  const range = bounds();
  const value = Math.min(range.max, Math.max(range.min, height)) / innerHeight * 100;
  container.style.setProperty('--console-user-height', `${value}vh`);
  setExpanded(false, pinned);
  return value;
}

function finishDrag(commit) {
  const session = drag;
  if (!session) return;
  drag = null;
  if (handle.hasPointerCapture(session.pointerId)) handle.releasePointerCapture(session.pointerId);
  document.body.classList.remove('console-resizing');
  if (commit && session.changed) {
    lsSet(HEIGHT_KEY, String(session.height));
  } else if (!commit) {
    if (session.originalHeight) container.style.setProperty('--console-user-height', session.originalHeight);
    else container.style.removeProperty('--console-user-height');
    setExpanded(session.expanded, session.pinned);
  }
}

export function resetConsoleSize() {
  if (!container) return;
  finishDrag(false);
  lsRemove(HEIGHT_KEY);
  container.style.removeProperty('--console-user-height');
  setExpanded(false);
}

export function initConsoleSizing() {
  container = El.consoleContainer;
  handle = El.consoleResizeHandle;
  panes = container?.querySelector('.console-panes');
  if (!panes || !handle) return;

  const stored = lsGet(HEIGHT_KEY, null);
  if (stored !== null) {
    const height = Number(stored);
    if (Number.isFinite(height) && height > 0) setHeight(height / 100 * innerHeight);
    else lsRemove(HEIGHT_KEY);
  }
  refreshSize();

  El.consoleExpand?.addEventListener('click', () => {
    finishDrag(false);
    setExpanded(!State.consoleExpanded);
  });
  handle.addEventListener('pointerdown', (event) => {
    if (event.button !== 0 || !event.isPrimary || drag) return;
    event.preventDefault();
    handle.focus({ preventScroll: true });
    drag = {
      pointerId: event.pointerId,
      startY: event.clientY,
      startHeight: panes.getBoundingClientRect().height,
      originalHeight: container.style.getPropertyValue('--console-user-height'),
      expanded: State.consoleExpanded,
      pinned: { ...State.consolePinned },
      changed: false,
    };
    handle.setPointerCapture(event.pointerId);
    document.body.classList.add('console-resizing');
  });
  handle.addEventListener('pointermove', (event) => {
    if (!drag || drag.pointerId !== event.pointerId) return;
    if (!drag.changed && event.clientY === drag.startY) return;
    drag.changed = true;
    drag.height = setHeight(drag.startHeight + event.clientY - drag.startY, drag.pinned);
  });
  handle.addEventListener('pointerup', (event) => {
    if (drag?.pointerId === event.pointerId) finishDrag(true);
  });
  handle.addEventListener('pointercancel', () => finishDrag(false));
  handle.addEventListener('lostpointercapture', () => finishDrag(false));
  window.addEventListener('blur', () => finishDrag(false));
  window.addEventListener('resize', () => {
    finishDrag(false);
    refreshSize();
  });
  handle.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && drag) {
      event.preventDefault();
      event.stopPropagation();
      finishDrag(false);
      return;
    }
    const direction = event.key === 'ArrowUp' ? -1 : event.key === 'ArrowDown' ? 1 : 0;
    if (!direction && event.key !== 'Home' && event.key !== 'End') return;
    event.preventDefault();
    event.stopPropagation();
    finishDrag(false);
    const range = bounds();
    const step = Number.parseFloat(getComputedStyle(document.body).lineHeight) * (event.shiftKey ? 5 : 1);
    const height = event.key === 'Home' ? range.min : event.key === 'End' ? range.max
      : panes.getBoundingClientRect().height + direction * step;
    lsSet(HEIGHT_KEY, String(setHeight(height)));
  });
}
