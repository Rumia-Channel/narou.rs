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
    // Support tag: prefix for tag-only filtering (supports OR with |)
    if (q.startsWith('tag:')) {
      const tagQ = q.slice(4);
      const tagParts = tagQ.split('|').map(t => t.trim()).filter(Boolean);
      list = list.filter(n =>
        tagParts.some(tp =>
          (n.tags || []).some(t => t.toLowerCase().includes(tp))
        )
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
      case 'last_check_date': return n.last_check_date || '';
      case 'title': return (n.title || '').toLowerCase();
      case 'author': return (n.author || '').toLowerCase();
      case 'sitename': return (n.sitename || '').toLowerCase();
      case 'novel_type': return n.novel_type || 0;
      case 'general_all_no': return n.general_all_no || 0;
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

  // general_lastup with time badge + hint-new-arrival when newer than last_update
  let glCell = '';
  let glHint = false;
  if (novel.general_lastup) {
    const badge = getTimeBadge(novel.general_lastup);
    const dateStr = formatDate(novel.general_lastup);
    glCell = badge ? `${badge} ${dateStr}` : dateStr;
    // narou.rb highlights when general_lastup > last_update (new content available)
    if (novel.general_lastup > novel.last_update) {
      glHint = true;
    }
  }

  // last_check_date
  const checkCell = novel.last_check_date ? formatDate(novel.last_check_date) : '';

  // Tags
  const tagsHtml = renderTags(novel.tags || []);

  // Status
  const statusParts = [];
  if (novel.end === false || novel.end === 0) statusParts.push('連載中');
  else if (novel.end === true || novel.end === 1) statusParts.push('完結');

  // TOC URL link button
  const tocUrl = novel.toc_url || '';
  const tocLink = tocUrl
    ? `<a href="${esc(tocUrl)}" target="_blank" rel="noopener" class="btn-link-icon" title="${esc(tocUrl)}">&#x1F517;</a>`
    : '';

  // Episode count with "話" suffix (narou.rb style)
  const episodes = novel.general_all_no != null ? novel.general_all_no : 0;
  const episodesText = episodes ? episodes + '話' : '';

  // Character count with "字" suffix (narou.rb style)
  const charCount = novel.length;
  const lengthText = charCount != null && charCount > 0 ? unitizeNumeric(charCount) + '字' : '';

  // Novel type
  const novelTypeText = novel.novel_type === 2 ? '短編' : '';

  // Menu button (opens context menu) — glyphicon-option-horizontal equivalent
  const menuBtn = `<button class="row-action-btn btn-menu-icon" data-menu-id="${novel.id}" type="button" title="個別メニュー">⋯</button>`;

  tr.innerHTML = `
    <td class="col-id">${esc(idText)}</td>
    <td class="col-update">${updateCell}</td>
    <td class="col-general-lastup${glHint ? ' hint-new-arrival' : ''}">${glCell}</td>
    <td class="col-last-check">${checkCell}</td>
    <td class="col-title">${esc(novel.title || '')}</td>
    <td class="col-author"><span class="filterable" data-filter="${esc(novel.author || '')}">${esc(novel.author || '')}</span></td>
    <td class="col-site"><span class="filterable" data-filter="${esc(novel.sitename || '')}">${esc(novel.sitename || '')}</span></td>
    <td class="col-novel-type">${novelTypeText}</td>
    <td class="col-tags">${tagsHtml}</td>
    <td class="col-episodes">${episodesText}</td>
    <td class="col-length">${lengthText}</td>
    <td class="col-status">${statusParts.join(', ')}</td>
    <td class="col-url">${tocLink}</td>
    <td class="col-story"><button class="row-action-btn btn-story" data-story-id="${novel.id}" type="button" title="あらすじ">ℹ</button></td>
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
        pop.innerHTML = `<div class="story-popover-title">${data.title || ''}</div>
          <div class="story-popover-body">${data.story || '(あらすじなし)'}</div>`;
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

  // Bind tag click filtering (click tag in row → filter by that tag)
  tr.querySelectorAll('.tag-label').forEach(el => {
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      const tag = el.dataset.tag;
      if (tag) {
        State.filterText = 'tag:' + tag;
        if (El.filterInput) El.filterInput.value = 'tag:' + tag;
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
  const icon = isRunning ? '&#x25B6;' : '&#x23F3;';
  let actionBtns = '';
  if (isRunning) {
    actionBtns = `<button class="queue-task-cancel" data-task-id="${esc(task.id)}" title="中止">&#x23F9;</button>`;
  } else {
    const upBtn = idx > 0
      ? `<button class="queue-task-up" data-task-idx="${idx}" title="上へ">&#x25B2;</button>`
      : `<button class="queue-task-up" disabled title="上へ">&#x25B2;</button>`;
    const downBtn = idx < total - 1
      ? `<button class="queue-task-down" data-task-idx="${idx}" title="下へ">&#x25BC;</button>`
      : `<button class="queue-task-down" disabled title="下へ">&#x25BC;</button>`;
    actionBtns = `${upBtn}${downBtn}<button class="queue-task-delete" data-task-id="${esc(task.id)}" title="削除">&#x1F5D1;</button>`;
  }
  return `<div class="queue-task-item${isRunning ? ' queue-running' : ''}" data-task-id="${esc(task.id)}">
    <span class="queue-task-icon">${icon}</span>
    <span class="queue-task-label">${esc(label)}</span>
    <span class="queue-task-target">${esc(task.target)}</span>
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
    } else {
      El.queuePendingList.textContent = 'なし';
    }
  }
  if (El.queuePendingCount) {
    El.queuePendingCount.textContent = `(${qd.pending_count || 0})`;
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

function getTimeBadge(dateStr) {
  if (!dateStr) return '';
  const d = new Date(dateStr.replace(/-/g, '/'));
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
  // API returns "YYYY-MM-DD HH:MM" — convert dashes to slashes for display
  const m = dateStr.match(/^(\d{4})-(\d{2})-(\d{2})\s+(\d{2}:\d{2})/);
  if (m) return `${m[1]}/${m[2]}/${m[3]} ${m[4]}`;
  // Fallback: try Date parsing
  const d = new Date(dateStr.replace(/-/g, '/'));
  if (isNaN(d.getTime())) return dateStr;
  const y = d.getFullYear();
  const mo = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  const h = String(d.getHours()).padStart(2, '0');
  const min = String(d.getMinutes()).padStart(2, '0');
  return `${y}/${mo}/${day} ${h}:${min}`;
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
