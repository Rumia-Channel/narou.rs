/**
 * Novel list rendering — full narou.rb column set with time badges, status marks,
 * context menu, click-to-select, and per-row action buttons.
 */
import { State, El, lsSet } from '../core/state.js';
import { fetchJson, postJson } from '../core/http.js';
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
const PAGE_LENGTH_OPTIONS = [20, 50, 100, 200, 500, -1];
const ANNOTATION_COLOR_TIME_LIMIT_MS = 6 * 60 * 60 * 1000;

let selectionDrag = null;
let queueDragTaskId = null;

function materialIcon(name, extraClass = '') {
  const cls = extraClass
    ? `material-symbols-outlined ${extraClass}`
    : 'material-symbols-outlined';
  return `<span class="${cls}" aria-hidden="true">${name}</span>`;
}

/* ===== Rendering ===== */

export function renderNovelList() {
  const tbody = El.novelListBody;
  if (!tbody) return;

  const sorted = getFilteredSortedNovels();
  const pageLength = normalizePageLength(State.pageLength);
  const totalCount = sorted.length;
  const totalPages = pageLength === -1 ? 1 : Math.max(1, Math.ceil(totalCount / pageLength));
  State.currentPage = Math.min(Math.max(State.currentPage || 1, 1), totalPages);
  const start = pageLength === -1 ? 0 : (State.currentPage - 1) * pageLength;
  const end = pageLength === -1 ? totalCount : Math.min(start + pageLength, totalCount);
  const visible = pageLength === -1 ? sorted : sorted.slice(start, end);

  const fragment = document.createDocumentFragment();
  for (let i = 0; i < visible.length; i++) {
    fragment.appendChild(createRow(visible[i], i));
  }
  tbody.textContent = '';
  tbody.appendChild(fragment);
  renderPageLengthSelector(pageLength);
  renderPageInfo(totalCount, start, end);
  renderPagination(totalPages);

  updateSelectionBadge();
  updateEnableSelected();
}

function normalizePageLength(value) {
  return PAGE_LENGTH_OPTIONS.includes(value) ? value : 50;
}

function renderPageLengthSelector(pageLength) {
  if (El.novelListLength) {
    El.novelListLength.value = String(pageLength);
  }
}

function renderPageInfo(totalCount, start, end) {
  if (!El.novelListInfo) return;
  if (totalCount === 0) {
    El.novelListInfo.textContent = '該当なし';
    return;
  }
  if (State.pageLength === -1) {
    El.novelListInfo.textContent = `全 ${totalCount} 件`;
    return;
  }
  El.novelListInfo.textContent = `${start + 1} - ${end} / ${totalCount} 件`;
}

function renderPagination(totalPages) {
  const containers = [El.novelListPaginationTop, El.novelListPagination].filter(Boolean);
  if (containers.length === 0) return;
  if (State.pageLength === -1 || totalPages <= 1) {
    containers.forEach(container => {
      container.innerHTML = '';
    });
    return;
  }

  const pages = visiblePageNumbers(totalPages, State.currentPage);
  const buttons = [
    renderPageButton('&laquo;', 1, State.currentPage === 1),
    renderPageButton('&lsaquo;', State.currentPage - 1, State.currentPage === 1),
    ...pages.map(page => renderPageButton(String(page), page, false, page === State.currentPage)),
    renderPageButton('&rsaquo;', State.currentPage + 1, State.currentPage === totalPages),
    renderPageButton('&raquo;', totalPages, State.currentPage === totalPages),
  ];
  const html = buttons.join('');
  containers.forEach(container => {
    container.innerHTML = html;
    container.querySelectorAll('button[data-page]').forEach(btn => {
      btn.addEventListener('click', () => {
        const nextPage = Number.parseInt(btn.dataset.page || '', 10);
        if (!Number.isFinite(nextPage)) return;
        State.currentPage = nextPage;
        renderNovelList();
      });
    });
  });
}

function visiblePageNumbers(totalPages, currentPage) {
  if (totalPages <= 7) {
    return Array.from({ length: totalPages }, (_, index) => index + 1);
  }
  const start = Math.max(1, currentPage - 2);
  const end = Math.min(totalPages, start + 4);
  const adjustedStart = Math.max(1, end - 4);
  return Array.from({ length: end - adjustedStart + 1 }, (_, index) => adjustedStart + index);
}

function renderPageButton(label, page, disabled, active = false) {
  const classes = ['btn', active ? 'btn-primary' : 'btn-default'];
  return `<button type="button" class="${classes.join(' ')}" data-page="${page}"${disabled ? ' disabled' : ''}>${label}</button>`;
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
  const query = (State.filterText || '').trim();
  if (query) {
    const groups = splitFilterGroups(query);
    list = list.filter(novel => groups.some(group => group.every(token => matchFilterToken(novel, token))));
  }

  return list;
}

function getFilteredSortedNovels() {
  return sortNovels(getFilteredNovels());
}

function splitFilterGroups(query) {
  return [splitFilterTerms(query)];
}

function splitFilterTerms(group) {
  const terms = [];
  let current = '';
  let quoted = false;

  for (let i = 0; i < group.length; i++) {
    const ch = group[i];
    if (ch === '"') {
      quoted = !quoted;
      current += ch;
      continue;
    }
    if (/\s/.test(ch) && !quoted) {
      if (current.trim()) terms.push(current.trim());
      current = '';
      continue;
    }
    current += ch;
  }

  if (current.trim()) terms.push(current.trim());
  return terms.map(parseFilterToken);
}

function parseFilterToken(rawToken) {
  const token = rawToken.trim();
  const negate = token.startsWith('-') || token.startsWith('^') || token.startsWith('!');
  const body = negate ? token.slice(1) : token;
  const colon = body.indexOf(':');
  const field = colon > 0 ? body.slice(0, colon).toLowerCase() : '';
  const value = (colon > 0 ? body.slice(colon + 1) : body).trim().toLowerCase();
  return { negate, field, value };
}

function stripFilterQuotes(value) {
  if (value.length >= 2 && value.startsWith('"') && value.endsWith('"')) {
    return value.slice(1, -1);
  }
  return value;
}

function splitOrValues(value) {
  const values = [];
  let current = '';
  let quoted = false;

  for (let i = 0; i < value.length; i++) {
    const ch = value[i];
    if (ch === '"') {
      quoted = !quoted;
      current += ch;
      continue;
    }
    if (ch === '|' && !quoted) {
      const trimmed = stripFilterQuotes(current.trim());
      if (trimmed) values.push(trimmed);
      current = '';
      continue;
    }
    current += ch;
  }

  const trimmed = stripFilterQuotes(current.trim());
  if (trimmed) values.push(trimmed);
  return values;
}

function matchFilterToken(novel, token) {
  const target = (text) => String(text || '').toLowerCase();
  const tags = (novel.tags || []).map(tag => target(tag));
  const statusText = target(getStatusText(novel));
  let matched = false;
  const values = splitOrValues(token.value);
  const matchAny = (predicate) => values.some(predicate);
  const plainText = [
    target(novel.title),
    target(novel.author),
    target(novel.sitename),
    statusText,
    ...tags,
  ].join(' ');

  switch (token.field) {
    case 'tag':
      matched = matchAny(v =>
        tags.some(tag => tag.includes(v))
      );
      break;
    case 'author':
      matched = matchAny(v => target(novel.author).includes(v));
      break;
    case 'site':
    case 'sitename':
      matched = matchAny(v => target(novel.sitename).includes(v));
      break;
    case 'title':
      matched = matchAny(v => target(novel.title).includes(v));
      break;
    case 'id':
      matched = matchAny(v => String(novel.id) === v);
      break;
    case 'status':
      matched = matchAny(v => statusText.includes(v));
      break;
    default:
      matched = values.length > 0
        ? values.some(v => plainText.includes(v))
        : plainText.includes(token.value);
      break;
  }

  return token.negate ? !matched : matched;
}

function applyFilterFromClick(kind, value, event) {
  const normalizedKind = kind === 'site' ? 'sitename' : kind;
  const normalizedValue = String(value || '').trim();
  if (!normalizedKind || !normalizedValue) return;

  const next = buildStructuredFilter(
    State.filterText || '',
    normalizedKind,
    normalizedValue,
    {
      negate: !!event.shiftKey,
      mode: event.ctrlKey || event.metaKey ? 'or' : 'and',
    },
  );

  State.filterText = next;
  State.currentPage = 1;
  if (El.filterInput) El.filterInput.value = next;
  El.filterClear?.classList.toggle('hide', !next);
  renderNovelList();
}

function buildStructuredFilter(currentFilter, field, value, options) {
  const terms = splitRawFilterTerms(String(currentFilter || '').trim());
  const newTerm = formatFilterTerm(field, value, options.negate);

  if (terms.some(term => filterTermContainsValue(term, field, value, options.negate))) {
    return terms.join(' ');
  }

  if (options.mode === 'or') {
    for (let i = terms.length - 1; i >= 0; i--) {
      const parsed = parseRawFieldToken(terms[i]);
      if (!parsed || parsed.field !== field || parsed.negate !== options.negate) continue;
      parsed.values.push(value);
      terms[i] = formatFilterTerm(field, parsed.values, options.negate);
      return terms.join(' ');
    }
  }

  terms.push(newTerm);
  return terms.join(' ');
}

function splitRawFilterTerms(query) {
  if (!query) return [];
  const terms = [];
  let current = '';
  let quoted = false;

  for (let i = 0; i < query.length; i++) {
    const ch = query[i];
    if (ch === '"') {
      quoted = !quoted;
      current += ch;
      continue;
    }
    if (/\s/.test(ch) && !quoted) {
      if (current.trim()) terms.push(current.trim());
      current = '';
      continue;
    }
    current += ch;
  }

  if (current.trim()) terms.push(current.trim());
  return terms;
}

function parseRawFieldToken(rawTerm) {
  const term = String(rawTerm || '').trim();
  const negate = term.startsWith('-') || term.startsWith('^') || term.startsWith('!');
  const body = negate ? term.slice(1) : term;
  const colon = body.indexOf(':');
  if (colon <= 0) return null;
  const field = body.slice(0, colon).toLowerCase();
  const values = splitOrValues(body.slice(colon + 1)).map(stripFilterQuotes);
  return { negate, field, values };
}

function filterTermContainsValue(term, field, value, negate) {
  const parsed = parseRawFieldToken(term);
  if (!parsed || parsed.field !== field || parsed.negate !== negate) return false;
  const needle = normalizeFilterValue(value);
  return parsed.values.some(existing => normalizeFilterValue(existing) === needle);
}

function formatFilterTerm(field, values, negate) {
  const parts = Array.isArray(values) ? values : [values];
  const value = parts.map(formatFilterValue).join('|');
  return `${negate ? '-' : ''}${field}:${value}`;
}

function formatFilterValue(value) {
  const text = String(value || '').trim();
  if (/[\s|"]/u.test(text)) {
    return `"${text.replace(/"/g, '')}"`;
  }
  return text;
}

function normalizeFilterValue(value) {
  return String(value || '').trim().toLowerCase();
}

function sortNovels(novels) {
  const col = State.sortCol;
  const asc = State.sortAsc;

  const keyFn = (n) => {
    switch (col) {
      case 'id': return n.id || 0;
      case 'last_update': return n.last_update || 0;
      case 'general_lastup': return n.general_lastup || 0;
      case 'last_check_date': return n.last_check_date || 0;
      case 'title': return (n.title || '').toLowerCase();
      case 'author': return (n.author || '').toLowerCase();
      case 'sitename': return (n.sitename || '').toLowerCase();
      case 'novel_type': return n.novel_type || 0;
      case 'general_all_no': return n.general_all_no || 0;
      case 'length': return n.length || 0;
      case 'average_length': return getAverageLengthValue(n);
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

function createRow(novel, rowIndex) {
  const tr = document.createElement('tr');
  tr.dataset.id = novel.id;
  tr.dataset.rowIndex = String(rowIndex);
  tr.draggable = false;

  const isFrozen = novel.frozen;
  const isSelected = State.selectedIds.has(String(novel.id));

  if (isFrozen) tr.classList.add('frozen');
  if (isSelected) tr.classList.add('selected');

  bindRowSelection(tr, novel, rowIndex);

  // Right-click context menu
  tr.addEventListener('contextmenu', (e) => {
    showContextMenu(e, novel.id);
  });

  const idText = isFrozen ? `＊${novel.id}` : String(novel.id);

  // narou.rb parity: last_update cell — new-arrivals/new-update class goes only
  // on the inline span (matches `span.new-arrivals:after { content: " 新着" }`).
  // The date text itself is NOT colored, only the pseudo-element label.
  const updateMarkClass = getLastUpdateMarkClass(novel);
  const updateCell = formatDateCell(novel.last_update, {
    inlineClass: updateMarkClass,
  });

  // narou.rb parity: general_lastup cell —
  //   <div class="hint-new-arrival?"><span class="general-lastup gl-XXhour">date<br>time</span></div>
  let glCell = '';
  if (novel.general_lastup) {
    const glBadgeClass = getGeneralLastupClass(novel.general_lastup);
    const glHint = hasGeneralLastupHint(novel);
    glCell = formatDateCell(novel.general_lastup, {
      wrapperClass: glHint ? 'hint-new-arrival' : '',
      spanClass: `general-lastup ${glBadgeClass}`.trim(),
    });
  }

  // last_check_date
  const checkCell = novel.last_check_date ? formatDateCell(novel.last_check_date) : '';

  // Tags
  const tagsHtml = renderTags(novel.tags || []);

  // Status
  const statusText = getStatusText(novel);

  // TOC URL link button
  const tocUrl = novel.toc_url || '';
  const tocLink = tocUrl
    ? `<a href="${esc(tocUrl)}" target="_blank" rel="noopener" class="btn-link-icon" title="${esc(tocUrl)}">${materialIcon('link', 'icon-only')}</a>`
    : '';

  // Episode count with "話" suffix (narou.rb style)
  const episodes = getEpisodeCount(novel);
  const episodesText = episodes + '話';

  // Character count with "字" suffix (narou.rb style)
  const charCount = novel.length;
  const lengthText = charCount != null && charCount > 0 ? unitizeNumeric(charCount) + '字' : '';

  // Average characters per episode (narou.rb style)
  const averageLength = getAverageLengthValue(novel);
  const averageLengthText = averageLength > 0 ? averageLength.toLocaleString() : '';

  // Novel type
  const novelTypeText = getNovelTypeText(novel);

  // Menu button (opens context menu) — glyphicon-option-horizontal equivalent
  const downloadLink = `<a href="/novels/${encodeURIComponent(String(novel.id))}/download" class="row-action-btn btn-download" title="書籍データをダウンロード">${materialIcon('download', 'icon-only')}</a>`;
  const folderBtn = `<button class="row-action-btn btn-folder" data-folder-id="${novel.id}" type="button" title="保存先を開く">${materialIcon('folder_open', 'icon-only')}</button>`;
  const updateBtn = `<button class="row-action-btn btn-update-action" data-update-id="${novel.id}" type="button" title="凍結済みでも更新">${materialIcon('refresh', 'icon-only')}</button>`;
  const menuBtn = `<button class="row-action-btn btn-menu-icon" data-menu-id="${novel.id}" type="button" title="個別メニュー">${materialIcon('more_horiz', 'icon-only')}</button>`;

  tr.innerHTML = `
    <td class="col-id" style="text-align:center">${esc(idText)}</td>
    <td class="col-update">${updateCell}</td>
    <td class="col-general-lastup">${glCell}</td>
    <td class="col-last-check">${checkCell}</td>
    <td class="col-title">${esc(novel.title || '')}</td>
    <td class="col-author"><span class="filterable" data-filter-kind="author" data-filter-value="${escAttr(novel.author || '')}">${esc(novel.author || '')}</span></td>
    <td class="col-site"><span class="filterable" data-filter-kind="sitename" data-filter-value="${escAttr(novel.sitename || '')}">${esc(novel.sitename || '')}</span></td>
    <td class="col-novel-type" style="text-align:center">${novelTypeText}</td>
    <td class="col-tags">${tagsHtml}</td>
    <td class="col-episodes" style="text-align:center">${episodesText}</td>
    <td class="col-length">${lengthText}</td>
    <td class="col-average-length">${averageLengthText}</td>
    <td class="col-status" style="text-align:center">${esc(statusText)}</td>
    <td class="col-url" style="text-align:center">${tocLink}</td>
    <td class="col-download" style="text-align:center">${downloadLink}</td>
    <td class="col-folder" style="text-align:center">${folderBtn}</td>
    <td class="col-update-action" style="text-align:center">${updateBtn}</td>
    <td class="col-story" style="text-align:center"><button class="row-action-btn btn-story" data-story-id="${novel.id}" type="button" title="あらすじ">${materialIcon('info', 'icon-only')}</button></td>
    <td class="col-menu" style="text-align:center">${menuBtn}</td>
  `;

  // Bind per-row menu button
  const btn = tr.querySelector('.btn-menu-icon');
  if (btn) {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      showContextMenu(e, novel.id);
    });
  }

  const folder = tr.querySelector('.btn-folder');
  if (folder) {
    folder.addEventListener('click', async (e) => {
      e.stopPropagation();
      try {
        const result = await postJson('/api/folder', { targets: [String(novel.id)] });
        if (result?.success === false) {
          showNotification(result.message || '保存先を開けませんでした', 'warning');
        }
      } catch {
        showNotification('保存先を開けませんでした', 'warning');
      }
    });
  }

  const update = tr.querySelector('.btn-update-action');
  if (update) {
    update.addEventListener('click', async (e) => {
      e.stopPropagation();
      try {
        const result = await postJson('/api/update', { targets: [String(novel.id)], force: true });
        if (result?.success === false) {
          showNotification(result.message || '更新ジョブを追加できませんでした', 'warning');
        }
      } catch {
        showNotification('更新ジョブを追加できませんでした', 'warning');
      }
    });
  }

  // Bind story popover button
  const storyBtn = tr.querySelector('.btn-story');
  if (storyBtn) {
    storyBtn.addEventListener('click', async (e) => {
      e.stopPropagation();
      const existing = document.querySelector('.story-popover');
      if (existing) existing.remove();

      const nid = storyBtn.dataset.storyId;
      try {
        const data = await fetchJson(`/api/story?id=${nid}`);
        if (!data) return;
        const pop = document.createElement('div');
        pop.className = 'story-popover';
        pop.innerHTML = `<div class="story-popover-title">${esc(data.title || '')}</div>
          <div class="story-popover-body">${renderMultilineHtml(data.story, '(あらすじなし)')}</div>`;
        document.body.appendChild(pop);
        const rect = storyBtn.getBoundingClientRect();
        pop.style.top = (rect.bottom + window.scrollY + 4) + 'px';
        pop.style.left = Math.max(0, rect.left + window.scrollX - 200) + 'px';

        const dismiss = (ev) => {
          if (!pop.contains(ev.target) && ev.target !== storyBtn) {
            pop.remove();
            document.removeEventListener('click', dismiss);
          }
        };
        setTimeout(() => document.addEventListener('click', dismiss), 0);
      } catch { /* ignore */ }
    });
  }

  // Bind filterable clicks (author/site → structured filter)
  tr.querySelectorAll('.filterable').forEach(el => {
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      applyFilterFromClick(el.dataset.filterKind, el.dataset.filterValue, e);
    });
  });

  // Bind tag click filtering (click tag in row → structured filter by that tag)
  tr.querySelectorAll('.tag-label').forEach(el => {
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      applyFilterFromClick('tag', el.dataset.tag, e);
    });
  });

  return tr;
}

/* ===== Selection ===== */

function syncSelectionClasses() {
  const rows = El.novelListBody?.querySelectorAll('tr[data-id]') || [];
  for (const row of rows) {
    row.classList.toggle('selected', State.selectedIds.has(String(row.dataset.id)));
  }
  updateSelectionBadge();
  updateEnableSelected();
}

function setSelectedIds(ids, replace = true) {
  if (replace) {
    State.selectedIds.clear();
  }
  for (const id of ids) {
    State.selectedIds.add(String(id));
  }
  syncSelectionClasses();
}

function getRowIndexFromElement(row) {
  const idx = Number.parseInt(row?.dataset.rowIndex || '', 10);
  return Number.isFinite(idx) ? idx : -1;
}

function bindRowSelection(tr, novel, rowIndex) {
  const interactiveSelector = '.tag-label, .filterable, .row-action-btn, a[href], button, input, textarea, select';

  tr.addEventListener('mousedown', (e) => {
    if (e.button !== 0 || e.target.closest(interactiveSelector)) return;
    if (State.selectMode === 'single') return;

    selectionDrag = {
      startIndex: rowIndex,
      lastIndex: rowIndex,
      moved: false,
      startX: e.clientX,
      startY: e.clientY,
      mode: State.selectMode,
    };
  });

  tr.addEventListener('click', (e) => {
    if (e.target.closest(interactiveSelector)) return;
    if (selectionDrag?.moved) return;

    if (State.selectMode === 'single') {
      clearSelection();
      toggleSelect(novel.id);
      return;
    }

    toggleSelect(novel.id);
  });
}

function handleSelectionDragMove(e) {
  if (!selectionDrag || e.buttons !== 1) return;
  const dx = Math.abs(e.clientX - selectionDrag.startX);
  const dy = Math.abs(e.clientY - selectionDrag.startY);
  if (!selectionDrag.moved && dx < 4 && dy < 4) return;

  const rows = Array.from(El.novelListBody?.querySelectorAll('tr[data-id]') || []);
  const hit = document.elementFromPoint(e.clientX, e.clientY)?.closest?.('tr[data-id]');
  if (!hit) return;

  const currentIndex = getRowIndexFromElement(hit);
  if (currentIndex < 0 || currentIndex === selectionDrag.lastIndex) {
    selectionDrag.moved = true;
    return;
  }

  selectionDrag.moved = true;
  selectionDrag.lastIndex = currentIndex;

  const start = Math.min(selectionDrag.startIndex, currentIndex);
  const end = Math.max(selectionDrag.startIndex, currentIndex);
  const ids = rows.slice(start, end + 1).map(row => row.dataset.id);
  setSelectedIds(ids, true);
}

function handleSelectionDragEnd() {
  if (!selectionDrag) return;
  selectionDrag = null;
}

document.addEventListener('mousemove', handleSelectionDragMove);
document.addEventListener('mouseup', handleSelectionDragEnd);

export function toggleSelect(id) {
  const key = String(id);
  if (State.selectedIds.has(key)) {
    State.selectedIds.delete(key);
  } else {
    State.selectedIds.add(key);
  }

  syncSelectionClasses();
}

export function selectVisible() {
  const rows = El.novelListBody?.querySelectorAll('tr[data-id]') || [];
  for (const row of rows) {
    State.selectedIds.add(row.dataset.id);
  }
  syncSelectionClasses();
}

export function selectAll() {
  for (const n of State.novels) {
    State.selectedIds.add(String(n.id));
  }
  syncSelectionClasses();
}

export function clearSelection() {
  State.selectedIds.clear();
  syncSelectionClasses();
}

export function getSelectedIdsInDisplayOrder() {
  const orderedIds = [];
  const seen = new Set();
  for (const novel of getFilteredSortedNovels()) {
    const id = String(novel.id);
    if (!State.selectedIds.has(id)) continue;
    orderedIds.push(id);
    seen.add(id);
  }
  for (const id of State.selectedIds) {
    if (seen.has(id)) continue;
    orderedIds.push(id);
  }
  return orderedIds;
}

export function pruneSelectedIdsToCurrentList() {
  const visibleIds = new Set(getFilteredNovels().map(novel => String(novel.id)));
  for (const id of [...State.selectedIds]) {
    if (!visibleIds.has(id)) {
      State.selectedIds.delete(id);
    }
  }
}

function updateSelectionBadge() {
  if (El.badgeSelecting) {
    const count = State.selectedIds.size;
    El.badgeSelecting.textContent = count > 0 ? String(count) : '0';
  }
}

function getEpisodeCount(novel) {
  return Number.isFinite(Number(novel.general_all_no)) ? Number(novel.general_all_no) : 0;
}

function getAverageLengthValue(novel) {
  const episodes = getEpisodeCount(novel);
  const length = Number(novel.length || 0);
  if (episodes <= 0 || length <= 0) return 0;
  return Math.floor(length / episodes);
}

function getNovelTypeText(novel) {
  return Number(novel.novel_type) === 2 ? '短編' : '連載';
}

function getStatusText(novel) {
  const tags = Array.isArray(novel.tags) ? novel.tags : [];
  const status = [];
  if (novel.frozen) status.push('凍結');
  if (tags.includes('end') || novel.end === true || novel.end === 1) status.push('完結');
  if (tags.includes('404')) status.push('削除');
  if (novel.suspend === true) status.push('中断');
  return status.join(', ');
}

export function updateEnableSelected() {
  const hasSelection = State.selectedIds.size > 0;
  document.querySelectorAll('.enable-selected').forEach(el => {
    if (el.tagName === 'BUTTON') {
      el.disabled = !hasSelection;
    } else {
      el.classList.toggle('disabled', !hasSelection);
    }
  });
}

/* ===== Tags ===== */

function renderTags(tags) {
  return tags.map(tag => {
    const colorName = State.tagColors[tag] || 'default';
    const cls = TAG_COLOR_MAP[colorName] || 'tag-default';
    return `<span class="tag-label ${cls}" data-tag="${escAttr(tag)}">${esc(tag)}</span>`;
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
    // Left-click: filter by tag. Ctrl/Shift mirror narou.rb's tag search modes.
    span.addEventListener('click', (e) => {
      applyFilterFromClick('tag', tag, e);
    });
    // Right-click: color picker
    span.addEventListener('contextmenu', (e) => {
      showTagColorMenu(e, tag);
    });
    canvas.appendChild(span);
  }
}

/* ===== Queue ===== */

function queueLaneSizes(qs) {
  if (Array.isArray(qs.lane_sizes) && qs.lane_sizes.length >= 2) {
    return [
      Number(qs.lane_sizes[0]) || 0,
      Number(qs.lane_sizes[1]) || 0,
    ];
  }
  const runningCount = typeof qs.running_count === 'number'
    ? qs.running_count
    : (qs.running ? 1 : 0);
  return [
    (qs.pending || 0) + runningCount,
    0,
  ];
}

export function renderQueueStatus() {
  const qs = State.queueStatus;
  const [defaultCount, secondaryCount] = queueLaneSizes(qs);
  const showSecondary = State.concurrencyEnabled;
  if (El.queueCount) {
    El.queueCount.textContent = String(defaultCount);
    El.queueCount.classList.toggle('queue-size-active', defaultCount > 0);
  }
  if (El.queueCountDivider) {
    El.queueCountDivider.hidden = !showSecondary;
  }
  if (El.queueCountConvert) {
    El.queueCountConvert.hidden = !showSecondary;
    El.queueCountConvert.textContent = String(secondaryCount);
    El.queueCountConvert.classList.toggle('queue-size-active', secondaryCount > 0);
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

/* ===== Queue Detailed ===== */

const JOB_TYPE_LABELS = {
  download: 'ダウンロード',
  download_force: '強制ダウンロード',
  update: '更新',
  update_by_tag: '更新',
  update_general_lastup: '最新話掲載日更新',
  auto_update: '自動アップデート',
  convert: '変換',
  mail: 'メール送信',
  send: '端末送信',
  freeze: '凍結',
  remove: '削除',
  backup: 'バックアップ',
  inspect: 'inspect',
  diff: '差分確認',
  diff_clean: '差分確認(clean)',
  setting_burn: '設定焼き込み',
  backup_bookmark: 'しおりバックアップ',
  eject: '端末の取り外し',
};

function formatTaskTime(epoch) {
  if (!epoch) return '';
  const d = new Date(epoch * 1000);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  return `${hh}:${mm}:${ss}`;
}

function renderTaskItem(task, isRunning, idx, total) {
  const label = JOB_TYPE_LABELS[task.type] || task.type;
  const time = formatTaskTime(task.created_at);
  const icon = isRunning
    ? materialIcon('play_arrow', 'icon-only')
    : materialIcon('schedule', 'icon-only');
  let actionBtns = '';
  if (isRunning) {
    actionBtns = `<button class="queue-task-cancel" data-task-id="${esc(task.id)}" title="中止">${materialIcon('stop', 'icon-only')}</button>`;
  } else {
    const upBtn = idx > 0
      ? `<button class="queue-task-up" data-task-idx="${idx}" title="上へ">${materialIcon('keyboard_arrow_up', 'icon-only')}</button>`
      : `<button class="queue-task-up" disabled title="上へ">${materialIcon('keyboard_arrow_up', 'icon-only')}</button>`;
    const downBtn = idx < total - 1
      ? `<button class="queue-task-down" data-task-idx="${idx}" title="下へ">${materialIcon('keyboard_arrow_down', 'icon-only')}</button>`
      : `<button class="queue-task-down" disabled title="下へ">${materialIcon('keyboard_arrow_down', 'icon-only')}</button>`;
    actionBtns = `${upBtn}${downBtn}<button class="queue-task-delete" data-task-id="${esc(task.id)}" title="削除">${materialIcon('delete', 'icon-only')}</button>`;
  }
  return `<div class="queue-task-item${isRunning ? ' queue-running' : ''}" data-task-id="${esc(task.id)}"${isRunning ? '' : ' draggable="true"'}>
    <span class="queue-task-icon">${icon}</span>
    <span class="queue-task-label">${esc(label)}</span>
    <span class="queue-task-target">${esc(task.display_target || task.target || '')}</span>
    <span class="queue-task-time">${time}</span>
    <span class="queue-task-actions">${actionBtns}</span>
  </div>`;
}

export function renderQueueDetailed() {
  const qd = State.queueDetailed;
  if (El.queueRunningList) {
    if (qd.running && qd.running.length > 0) {
      El.queueRunningList.innerHTML = qd.running.map((t, i) => renderTaskItem(t, true, i, qd.running.length)).join('');
      El.queueRunningList.querySelectorAll('.queue-task-cancel').forEach(btn => {
        btn.addEventListener('click', async () => {
          await postJson('/api/cancel_running_task', { task_id: btn.dataset.taskId });
          const { refreshQueueDetailed } = await import('./actions.js');
          await refreshQueueDetailed();
        });
      });
    } else {
      El.queueRunningList.textContent = 'なし';
    }
  }
  if (El.queuePendingList) {
    if (qd.pending && qd.pending.length > 0) {
      El.queuePendingList.innerHTML = qd.pending.map((t, i) => renderTaskItem(t, false, i, qd.pending.length)).join('');
      El.queuePendingList.querySelectorAll('.queue-task-delete').forEach(btn => {
        btn.addEventListener('click', async () => {
          const taskId = btn.dataset.taskId;
          await postJson('/api/remove_pending_task', { task_id: taskId });
          const { refreshQueueDetailed } = await import('./actions.js');
          await refreshQueueDetailed();
        });
      });
      // Up/down reorder buttons
      const wireReorder = (selector, direction) => {
        El.queuePendingList.querySelectorAll(selector).forEach(btn => {
          btn.addEventListener('click', async () => {
            const idx = parseInt(btn.dataset.taskIdx, 10);
            const ids = qd.pending.map(t => t.id);
            const swapIdx = idx + direction;
            if (swapIdx >= 0 && swapIdx < ids.length) {
              [ids[idx], ids[swapIdx]] = [ids[swapIdx], ids[idx]];
              await postJson('/api/reorder_pending_tasks', { task_ids: ids });
              const { refreshQueueDetailed } = await import('./actions.js');
              await refreshQueueDetailed();
            }
          });
        });
      };
      wireReorder('.queue-task-up', -1);
      wireReorder('.queue-task-down', 1);
      wireQueueDragDrop();
    } else {
      El.queuePendingList.textContent = 'なし';
    }
  }
  if (El.queuePendingCount) {
    El.queuePendingCount.textContent = `(${qd.pending_count || 0})`;
  }
  if (State.queueRestoreCheckPending) {
    State.queueRestoreCheckPending = false;
    State.queueRestorePrompted = !!(qd.restore_prompt_pending && qd.restorable_tasks_available);
    if (State.queueRestorePrompted) {
      El.queueRestoreModal?.classList.remove('hide');
    }
  }
}

function wireQueueDragDrop() {
  if (!El.queuePendingList) return;
  const rows = Array.from(El.queuePendingList.querySelectorAll('.queue-task-item[data-task-id]'));

  rows.forEach(row => {
    row.addEventListener('dragstart', (e) => {
      queueDragTaskId = row.dataset.taskId || null;
      row.classList.add('queue-drag-source');
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', queueDragTaskId || '');
    });
    row.addEventListener('dragend', () => {
      queueDragTaskId = null;
      row.classList.remove('queue-drag-source');
      El.queuePendingList?.querySelectorAll('.queue-drop-before, .queue-drop-after')
        .forEach(el => el.classList.remove('queue-drop-before', 'queue-drop-after'));
    });
    row.addEventListener('dragover', (e) => {
      if (!queueDragTaskId || queueDragTaskId === row.dataset.taskId) return;
      e.preventDefault();
      const rect = row.getBoundingClientRect();
      const before = e.clientY < rect.top + rect.height / 2;
      row.classList.toggle('queue-drop-before', before);
      row.classList.toggle('queue-drop-after', !before);
    });
    row.addEventListener('dragleave', () => {
      row.classList.remove('queue-drop-before', 'queue-drop-after');
    });
    row.addEventListener('drop', async (e) => {
      if (!queueDragTaskId || queueDragTaskId === row.dataset.taskId) return;
      e.preventDefault();
      const rect = row.getBoundingClientRect();
      const before = e.clientY < rect.top + rect.height / 2;
      await reorderQueuedTask(queueDragTaskId, row.dataset.taskId, before);
    });
  });

  if (El.queuePendingList.dataset.dragParentBound !== 'true') {
    El.queuePendingList.dataset.dragParentBound = 'true';
    El.queuePendingList.addEventListener('dragover', (e) => {
      if (!queueDragTaskId) return;
      e.preventDefault();
    });
    El.queuePendingList.addEventListener('drop', async (e) => {
      if (!queueDragTaskId) return;
      const target = e.target.closest?.('.queue-task-item[data-task-id]');
      if (target) return;
      e.preventDefault();
      await reorderQueuedTask(queueDragTaskId, null, false);
    });
  }
}

async function reorderQueuedTask(sourceId, targetId, before) {
  queueDragTaskId = null;
  const pending = State.queueDetailed?.pending || [];
  const ids = pending.map(t => t.id);
  const from = ids.indexOf(sourceId);
  if (from < 0) return;
  ids.splice(from, 1);
  if (targetId) {
    let to = ids.indexOf(targetId);
    if (to < 0) to = ids.length;
    if (!before) to += 1;
    if (to < 0) to = 0;
    if (to > ids.length) to = ids.length;
    ids.splice(to, 0, sourceId);
  } else {
    ids.push(sourceId);
  }
  await postJson('/api/reorder_pending_tasks', { task_ids: ids });
  const { refreshQueueDetailed } = await import('./actions.js');
  await refreshQueueDetailed();
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

  // Wide mode — toggle on .container-main (which has max-width in responsive.css)
  const containerMain = document.querySelector('.container-main');
  if (containerMain) containerMain.classList.toggle('wide-mode', State.wideMode);

  // Footer navbar
  if (El.footerNavbar) {
    El.footerNavbar.classList.toggle('hide', !State.buttonsFooter);
  }

  // Sort column highlight
  document.querySelectorAll('.sortable').forEach(th => {
    th.classList.remove('active-sort', 'sort-asc');
    if (th.dataset.sort === State.sortCol) {
      th.classList.add('active-sort');
      if (State.sortAsc) th.classList.add('sort-asc');
    }
  });
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

function getGeneralLastupClass(dateStr) {
  const d = parseDateValue(dateStr);
  if (!d) return 'gl-other';
  const diffSec = (Date.now() - d.getTime()) / 1000;
  // narou.rb GENERAL_LASTUP_CLASSES thresholds (seconds, class)
  if (diffSec <= 60 * 60) return 'gl-60minutes';
  if (diffSec <= 6 * 60 * 60) return 'gl-6hour';
  if (diffSec <= 24 * 60 * 60) return 'gl-24hour';
  if (diffSec <= 3 * 24 * 60 * 60) return 'gl-3days';
  if (diffSec <= 7 * 24 * 60 * 60) return 'gl-1week';
  return 'gl-other';
}

function parseDateValue(dateStr) {
  if (dateStr == null || dateStr === '') return null;
  const d = typeof dateStr === 'number'
    ? new Date(dateStr * 1000)
    : new Date(String(dateStr).replace(/-/g, '/'));
  return Number.isNaN(d.getTime()) ? null : d;
}

function getLastUpdateMarkClass(novel) {
  const lastUpdate = parseDateValue(novel.last_update);
  if (!lastUpdate) return '';
  const newArrivalsDate = parseDateValue(novel.new_arrivals_date);
  const now = Date.now();
  if (
    newArrivalsDate &&
    newArrivalsDate.getTime() >= lastUpdate.getTime() &&
    newArrivalsDate.getTime() + ANNOTATION_COLOR_TIME_LIMIT_MS >= now
  ) {
    return 'new-arrivals';
  }
  if (lastUpdate.getTime() + ANNOTATION_COLOR_TIME_LIMIT_MS >= now) {
    return 'new-update';
  }
  return '';
}

function hasGeneralLastupHint(novel) {
  const generalLastup = parseDateValue(novel.general_lastup);
  const lastUpdate = parseDateValue(novel.last_update);
  return !!(generalLastup && lastUpdate && generalLastup.getTime() > lastUpdate.getTime());
}

function formatDate(dateStr) {
  if (dateStr == null || dateStr === '') return { date: '', time: '' };
  if (typeof dateStr === 'number') {
    const d = parseDateValue(dateStr);
    if (!d) return { date: '', time: '' };
    const y = d.getFullYear();
    const mo = String(d.getMonth() + 1).padStart(2, '0');
    const day = String(d.getDate()).padStart(2, '0');
    const h = String(d.getHours()).padStart(2, '0');
    const min = String(d.getMinutes()).padStart(2, '0');
    return { date: `${y}/${mo}/${day}`, time: `${h}:${min}` };
  }
  // Backward-compatible fallback for pre-timestamp payloads
  const m = dateStr.match(/^(\d{4})-(\d{2})-(\d{2})\s+(\d{2}:\d{2})/);
  if (m) return { date: `${m[1]}/${m[2]}/${m[3]}`, time: m[4] };
  // Fallback: try Date parsing
  const d = parseDateValue(dateStr);
  if (!d) return { date: dateStr, time: '' };
  const y = d.getFullYear();
  const mo = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  const h = String(d.getHours()).padStart(2, '0');
  const min = String(d.getMinutes()).padStart(2, '0');
  return { date: `${y}/${mo}/${day}`, time: `${h}:${min}` };
}

function formatDateCell(dateStr, options = {}) {
  const { date, time } = formatDate(dateStr);
  if (!date) return '';
  const inlineExtra = options.inlineExtra || '';
  const inlineClass = options.inlineClass || '';
  const extraLine = options.extraLine || '';
  const label = options.label || '';
  const labelClass = options.labelClass || '';
  const wrapperClass = options.wrapperClass || '';
  const spanClass = options.spanClass || '';

  // narou.rb parity: when spanClass is provided, wrap the date content in a
  // single <span class="{spanClass}"> inside the outer div — matching
  //   <div class="hint-new-arrival?"><span class="general-lastup gl-XXhour">date<br>time</span></div>
  let inner = '';
  inner += `<span class="date-cell-date">${date}</span>`;
  if (time || label || inlineExtra) {
    inner += `<span class="date-cell-inline${inlineClass ? ' ' + inlineClass : ''}">`;
    if (time) {
      inner += `<span class="date-cell-time">${time}</span>`;
    }
    if (label) {
      inner += `<span class="date-cell-label${labelClass ? ' ' + labelClass : ''}">${label}</span>`;
    }
    if (inlineExtra) {
      inner += `<span class="date-cell-inline-extra">${inlineExtra}</span>`;
    }
    inner += '</span>';
  }
  if (extraLine) {
    inner += `<span class="date-cell-extra">${extraLine}</span>`;
  }

  let html = `<div class="date-cell${wrapperClass ? ' ' + wrapperClass : ''}">`;
  if (spanClass) {
    html += `<span class="${spanClass}">${inner}</span>`;
  } else {
    html += inner;
  }
  html += '</div>';
  return html;
}

function formatLength(n) {
  if (n == null) return '';
  if (n >= 10000) return (n / 10000).toFixed(1) + '万';
  if (n >= 1000) return (n / 1000).toFixed(1) + '千';
  return String(n);
}

/** Narou.rb-compatible unitizeNumeric: 10000→"1.0万", 1000→"1,000" etc. */
function unitizeNumeric(num) {
  if (num == null) return '';
  if (num >= 10000) {
    return (num / 10000).toFixed(1) + '万';
  }
  return num.toLocaleString();
}

function esc(s) {
  const div = document.createElement('div');
  div.textContent = String(s);
  return div.innerHTML;
}

function escAttr(s) {
  return esc(s).replace(/"/g, '&quot;');
}

function renderMultilineHtml(value, fallback) {
  const normalized = String(value || fallback || '').replace(/<br\s*\/?>/gi, '\n');
  return esc(normalized).replace(/\n/g, '<br>');
}
