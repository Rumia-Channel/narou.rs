/**
 * Novel list rendering: table rows with click-to-select (no per-row action buttons)
 */
import { State, El } from '../core/state.js';
import { t } from './i18n.js';

const TAG_COLOR_MAP = {
  green: 'tag-green',
  yellow: 'tag-yellow',
  blue: 'tag-blue',
  magenta: 'tag-magenta',
  cyan: 'tag-cyan',
  red: 'tag-red',
  white: 'tag-white',
};

/**
 * Render the full novel table body
 */
export function renderNovelList() {
  const tbody = El.novelListBody;
  if (!tbody) return;

  const filtered = getFilteredNovels();
  const sorted = sortNovels(filtered);

  const fragment = document.createDocumentFragment();
  for (const novel of sorted) {
    fragment.appendChild(createRow(novel));
  }
  tbody.textContent = '';
  tbody.appendChild(fragment);

  updateSelectionBadge();
  updateEnableSelected();
}

function getFilteredNovels() {
  let list = State.novels;

  // View mode filter
  if (State.viewMode === 'nonfrozen') {
    list = list.filter(n => !State.frozenIds.has(String(n.id)));
  } else if (State.viewMode === 'frozen') {
    list = list.filter(n => State.frozenIds.has(String(n.id)));
  }

  // Text filter
  if (State.filterText) {
    const q = State.filterText.toLowerCase();
    list = list.filter(n => {
      const searchable = [
        n.title || '', n.author || '', n.sitename || '',
        String(n.id), ...(n.tags || []),
      ].join(' ').toLowerCase();
      return searchable.includes(q);
    });
  }

  return list;
}

function sortNovels(novels) {
  const col = State.sortCol;
  const asc = State.sortAsc;

  const keyFn = (n) => {
    switch (col) {
      case 0: return n.id || 0;
      case 1: return n.last_update || '';
      case 2: return n.general_lastup || '';
      case 3: return (n.title || '').toLowerCase();
      case 4: return (n.author || '').toLowerCase();
      case 5: return (n.sitename || '').toLowerCase();
      default: return '';
    }
  };

  return [...novels].sort((a, b) => {
    const ka = keyFn(a);
    const kb = keyFn(b);
    let cmp = 0;
    if (ka < kb) cmp = -1;
    else if (ka > kb) cmp = 1;
    return asc ? cmp : -cmp;
  });
}

function createRow(novel) {
  const tr = document.createElement('tr');
  tr.dataset.id = novel.id;

  const isFrozen = State.frozenIds.has(String(novel.id));
  const isSelected = State.selectedIds.has(String(novel.id));

  if (isFrozen) tr.classList.add('frozen');
  if (isSelected) tr.classList.add('selected');

  tr.addEventListener('click', (e) => {
    if (e.target.closest('.tag-label')) return;
    toggleSelect(novel.id);
  });

  const idText = isFrozen ? `＊${novel.id}` : String(novel.id);

  tr.innerHTML = `
    <td class="col-id">${esc(idText)}</td>
    <td class="col-update">${formatDate(novel.last_update)}</td>
    <td class="col-latest">${formatDate(novel.general_lastup)}</td>
    <td class="col-title">${esc(novel.title || '')}</td>
    <td class="col-author">${esc(novel.author || '')}</td>
    <td class="col-site">${esc(novel.sitename || '')}</td>
    <td class="col-tags">${renderTags(novel.tags || [])}</td>
    <td class="col-status">${renderStatus(novel)}</td>
  `;

  return tr;
}

function toggleSelect(id) {
  const key = String(id);
  if (State.selectedIds.has(key)) {
    State.selectedIds.delete(key);
  } else {
    State.selectedIds.add(key);
  }

  const row = El.novelListBody?.querySelector(`tr[data-id="${id}"]`);
  if (row) row.classList.toggle('selected', State.selectedIds.has(key));

  updateSelectionBadge();
  updateEnableSelected();
}

export function selectVisible() {
  const rows = El.novelListBody?.querySelectorAll('tr[data-id]') || [];
  for (const row of rows) {
    State.selectedIds.add(row.dataset.id);
    row.classList.add('selected');
  }
  updateSelectionBadge();
  updateEnableSelected();
}

export function selectAll() {
  for (const n of State.novels) {
    State.selectedIds.add(String(n.id));
  }
  renderNovelList();
}

export function clearSelection() {
  State.selectedIds.clear();
  renderNovelList();
}

function updateSelectionBadge() {
  if (El.badgeSelecting) {
    const count = State.selectedIds.size;
    El.badgeSelecting.textContent = count > 0 ? String(count) : '0';
  }
}

function updateEnableSelected() {
  const hasSelection = State.selectedIds.size > 0;
  document.querySelectorAll('.enable-selected').forEach(el => {
    el.classList.toggle('active', hasSelection);
  });
}

function renderTags(tags) {
  return tags.map(tag => {
    const colorName = State.tagColors[tag] || 'default';
    const cls = TAG_COLOR_MAP[colorName] || 'tag-default';
    return `<span class="tag-label ${cls}" data-tag="${esc(tag)}">${esc(tag)}</span>`;
  }).join('');
}

function renderStatus(novel) {
  const parts = [];
  if (novel.new_arrivals) {
    parts.push(`<span class="status-new">${t('statusNew')}</span>`);
  }
  if (novel.general_lastup) {
    const badge = getTimeBadge(novel.general_lastup);
    if (badge) parts.push(badge);
  }
  return parts.join(' ');
}

function getTimeBadge(dateStr) {
  if (!dateStr) return '';
  const d = new Date(dateStr);
  if (isNaN(d.getTime())) return '';
  const diffMs = Date.now() - d.getTime();
  const hours = diffMs / (1000 * 60 * 60);

  if (hours < 1) return '<span class="gl-badge gl-1h">1h</span>';
  if (hours < 6) return '<span class="gl-badge gl-6h">6h</span>';
  if (hours < 24) return '<span class="gl-badge gl-24h">24h</span>';
  if (hours < 72) return '<span class="gl-badge gl-3d">3d</span>';
  if (hours < 168) return '<span class="gl-badge gl-1w">1w</span>';
  return '';
}

function formatDate(dateStr) {
  if (!dateStr) return '';
  const d = new Date(dateStr);
  if (isNaN(d.getTime())) return dateStr;
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  const h = String(d.getHours()).padStart(2, '0');
  const min = String(d.getMinutes()).padStart(2, '0');
  return `${y}/${m}/${day} ${h}:${min}`;
}

function esc(s) {
  const div = document.createElement('div');
  div.textContent = s;
  return div.innerHTML;
}

/**
 * Render tag list in navbar dropdown
 */
export function renderTagList() {
  const canvas = El.tagListCanvas;
  if (!canvas) return;
  canvas.textContent = '';

  for (const tag of State.tags) {
    const colorName = State.tagColors[tag] || 'default';
    const cls = TAG_COLOR_MAP[colorName] || 'tag-default';
    const span = document.createElement('span');
    span.className = `tag-label ${cls}`;
    span.textContent = tag;
    span.dataset.tag = tag;
    span.addEventListener('click', () => {
      State.filterText = tag;
      if (El.filterInput) El.filterInput.value = tag;
      if (El.filterClear) El.filterClear.classList.remove('hide');
      renderNovelList();
    });
    canvas.appendChild(span);
  }
}

/**
 * Update the queue display
 */
export function renderQueueStatus() {
  const qs = State.queueStatus;
  if (El.queueCount) {
    El.queueCount.textContent = String(qs.pending || 0);
  }
  const pending = document.getElementById('queue-pending-detail');
  const completed = document.getElementById('queue-completed-detail');
  const failed = document.getElementById('queue-failed-detail');
  if (pending) pending.textContent = String(qs.pending || 0);
  if (completed) completed.textContent = String(qs.completed || 0);
  if (failed) failed.textContent = String(qs.failed || 0);
}
