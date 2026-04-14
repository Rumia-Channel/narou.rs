/**
 * Novel list rendering — full narou.rb column set with time badges, status marks,
 * context menu, click-to-select, and per-row action buttons.
 */
import { State, El, lsSet } from '../core/state.js';
import { showContextMenu, showTagColorMenu } from './context_menu.js';

const TAG_COLOR_MAP = {
  green: 'tag-green',
  yellow: 'tag-yellow',
  blue: 'tag-blue',
  magenta: 'tag-magenta',
  cyan: 'tag-cyan',
  red: 'tag-red',
  white: 'tag-white',
};

/* ===== Rendering ===== */

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

  // Frozen/nonfrozen visibility
  if (!State.viewFrozen && !State.viewNonfrozen) {
    return [];
  }
  if (!State.viewFrozen) {
    list = list.filter(n => !n.frozen);
  } else if (!State.viewNonfrozen) {
    list = list.filter(n => n.frozen);
  }

  // Text/tag filter
  if (State.filterText) {
    const q = State.filterText.toLowerCase();
    // Support tag: prefix for tag-only filtering
    if (q.startsWith('tag:')) {
      const tagQ = q.slice(4);
      list = list.filter(n =>
        (n.tags || []).some(t => t.toLowerCase().includes(tagQ))
      );
    } else {
      list = list.filter(n => {
        const searchable = [
          n.title || '', n.author || '', n.sitename || '',
          String(n.id), ...(n.tags || []),
        ].join(' ').toLowerCase();
        return searchable.includes(q);
      });
    }
  }

  return list;
}

function sortNovels(novels) {
  const col = State.sortCol;
  const asc = State.sortAsc;

  const keyFn = (n) => {
    switch (col) {
      case 'id': return n.id || 0;
      case 'last_update': return n.last_update || '';
      case 'general_lastup': return n.general_lastup || '';
      case 'title': return (n.title || '').toLowerCase();
      case 'author': return (n.author || '').toLowerCase();
      case 'sitename': return (n.sitename || '').toLowerCase();
      case 'length': return n.length || 0;
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

  const isFrozen = novel.frozen;
  const isSelected = State.selectedIds.has(String(novel.id));

  if (isFrozen) tr.classList.add('frozen');
  if (isSelected) tr.classList.add('selected');

  // Click to select (hybrid: small movement = toggle; drag not implemented yet)
  tr.addEventListener('click', (e) => {
    if (e.target.closest('.tag-label') || e.target.closest('.row-action-btn')
        || e.target.closest('a[href]')) return;
    toggleSelect(novel.id);
  });

  // Right-click context menu
  tr.addEventListener('contextmenu', (e) => {
    showContextMenu(e, novel.id);
  });

  const idText = isFrozen ? `＊${novel.id}` : String(novel.id);

  // New arrival mark + time badge on last_update
  let updateCell = '';
  if (novel.new_arrivals) {
    updateCell += '<span class="status-new-dot" title="新着">●</span> ';
  }
  const updateBadge = getTimeBadge(novel.last_update);
  if (updateBadge) updateCell += updateBadge + ' ';
  updateCell += formatDate(novel.last_update);

  // general_lastup with time badge
  let glCell = '';
  if (novel.general_lastup) {
    const badge = getTimeBadge(novel.general_lastup);
    glCell = badge
      ? `${badge} ${formatDate(novel.general_lastup)}`
      : formatDate(novel.general_lastup);
  }

  // Tags
  const tagsHtml = renderTags(novel.tags || []);

  // Status
  const statusParts = [];
  if (novel.end === false || novel.end === 0) statusParts.push('連載中');
  else if (novel.end === true || novel.end === 1) statusParts.push('完結');

  // TOC URL
  const tocUrl = novel.toc_url || '';
  const tocLink = tocUrl
    ? `<a href="${esc(tocUrl)}" target="_blank" rel="noopener" class="toc-link" title="${esc(tocUrl)}">&#x1F517;</a>`
    : '';

  // Length (episode count)
  const episodeCount = novel.general_all_no != null ? novel.general_all_no : novel.length;
  const lengthText = episodeCount != null ? String(episodeCount) : '';

  // Menu button (opens context menu)
  const menuBtn = `<button class="row-action-btn btn-menu-icon" data-menu-id="${novel.id}" type="button" title="メニュー">&#x22EE;</button>`;

  tr.innerHTML = `
    <td class="col-id">${esc(idText)}</td>
    <td class="col-update">${updateCell}</td>
    <td class="col-general-lastup">${glCell}</td>
    <td class="col-title">${esc(novel.title || '')}</td>
    <td class="col-author"><span class="filterable" data-filter="${esc(novel.author || '')}">${esc(novel.author || '')}</span></td>
    <td class="col-site"><span class="filterable" data-filter="${esc(novel.sitename || '')}">${esc(novel.sitename || '')}</span></td>
    <td class="col-url">${tocLink}</td>
    <td class="col-length">${lengthText}</td>
    <td class="col-status">${statusParts.join(', ')}</td>
    <td class="col-tags">${tagsHtml}</td>
    <td class="col-menu">${menuBtn}</td>
  `;

  // Bind per-row menu button
  const btn = tr.querySelector('.btn-menu-icon');
  if (btn) {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      showContextMenu(e, novel.id);
    });
  }

  // Bind filterable clicks (author/site → filter)
  tr.querySelectorAll('.filterable').forEach(el => {
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      const val = el.dataset.filter;
      if (val) {
        State.filterText = val;
        if (El.filterInput) El.filterInput.value = val;
        if (El.filterClear) El.filterClear.classList.remove('hide');
        renderNovelList();
      }
    });
  });

  return tr;
}

/* ===== Selection ===== */

export function toggleSelect(id) {
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

export function updateEnableSelected() {
  const hasSelection = State.selectedIds.size > 0;
  document.querySelectorAll('.enable-selected').forEach(el => {
    el.classList.toggle('active', hasSelection);
  });
}

/* ===== Tags ===== */

function renderTags(tags) {
  return tags.map(tag => {
    const colorName = State.tagColors[tag] || 'default';
    const cls = TAG_COLOR_MAP[colorName] || 'tag-default';
    return `<span class="tag-label ${cls}" data-tag="${esc(tag)}">${esc(tag)}</span>`;
  }).join('');
}

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
    // Left-click: filter by tag
    span.addEventListener('click', () => {
      State.filterText = 'tag:' + tag;
      if (El.filterInput) El.filterInput.value = 'tag:' + tag;
      if (El.filterClear) El.filterClear.classList.remove('hide');
      renderNovelList();
    });
    // Right-click: color picker
    span.addEventListener('contextmenu', (e) => {
      showTagColorMenu(e, tag);
    });
    canvas.appendChild(span);
  }
}

/* ===== Queue ===== */

export function renderQueueStatus() {
  const qs = State.queueStatus;
  if (El.queueCount) {
    const total = (qs.pending || 0) + (qs.running ? 1 : 0);
    El.queueCount.textContent = String(total);
    El.queueCount.classList.toggle('queue-size-active', total > 0);
  }

  // Queue modal lists
  if (El.queueRunningList) {
    if (qs.running) {
      El.queueRunningList.innerHTML =
        `<div class="queue-task-item queue-running">${esc(qs.running)}</div>`;
    } else {
      El.queueRunningList.textContent = 'なし';
    }
  }
  if (El.queuePendingList) {
    const items = qs.pending_items || [];
    if (items.length > 0) {
      El.queuePendingList.innerHTML = items.map(item =>
        `<div class="queue-task-item">${esc(item)}</div>`
      ).join('');
    } else {
      El.queuePendingList.textContent = `${qs.pending || 0} 件`;
    }
  }
  if (El.queuePendingCount) {
    El.queuePendingCount.textContent = `(${qs.pending || 0})`;
  }
}

/* ===== Notifications ===== */

export function showNotification(message, type = 'info') {
  const container = El.notificationContainer;
  if (!container) return;

  const div = document.createElement('div');
  div.className = `notification notification-${type}`;
  div.textContent = message;
  container.appendChild(div);

  // Fade out after 4 seconds
  setTimeout(() => {
    div.classList.add('notification-fadeout');
    setTimeout(() => div.remove(), 500);
  }, 4000);
}

/* ===== View state sync ===== */

export function syncViewChecks() {
  setCheck('action-view-nonfrozen', State.viewNonfrozen);
  setCheck('action-view-frozen', State.viewFrozen);
  setCheck('action-view-novel-list-wide', State.wideMode);
  setCheck('action-view-toggle-setting-new-tab', State.settingNewTab);
  setCheck('action-view-toggle-buttons-top', State.buttonsTop);
  setCheck('action-view-toggle-buttons-footer', State.buttonsFooter);

  // Selection mode
  setCheck('action-select-mode-single', State.selectMode === 'single');
  setCheck('action-select-mode-multi', State.selectMode === 'rect');
  setCheck('action-select-mode-hybrid', State.selectMode === 'hybrid');

  // Wide mode
  const container = El.novelListContainer;
  if (container) container.classList.toggle('wide-mode', State.wideMode);

  // Footer navbar
  if (El.footerNavbar) {
    El.footerNavbar.classList.toggle('hide', !State.buttonsFooter);
  }
}

function setCheck(id, checked) {
  const el = document.getElementById(id);
  if (!el) return;
  const mark = el.querySelector('.check-mark');
  if (mark) {
    mark.classList.toggle('checked', checked);
  }
}

/* ===== Helpers ===== */

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

function formatLength(n) {
  if (n == null) return '';
  if (n >= 10000) return (n / 10000).toFixed(1) + '万';
  if (n >= 1000) return (n / 1000).toFixed(1) + '千';
  return String(n);
}

function esc(s) {
  const div = document.createElement('div');
  div.textContent = String(s);
  return div.innerHTML;
}
