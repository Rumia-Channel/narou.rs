/**
 * Settings page logic for Narou.rs WEB UI
 * Loads settings from API, renders tabs/forms, and handles save.
 */

(function() {
  'use strict';

  let settingsData = null;
  let activeTab = null;
  let saveInFlight = false;

  // ─── Init ──────────────────────────────────────────────
  document.addEventListener('DOMContentLoaded', init);

  async function init() {
    try {
      await reloadSettingsView();
      bindEvents();
    } catch (e) {
      console.error('Settings load error:', e);
      document.getElementById('settings-tab-content').innerHTML =
        '<div class="panel-settings"><div class="panel-heading" style="color:var(--danger-color)">設定の読み込みに失敗しました: ' + e.message + '</div></div>';
    }
  }

  // ─── Tab rendering ─────────────────────────────────────
  function renderTabs() {
    const ul = document.getElementById('settings-tabs');
    ul.innerHTML = '';
    settingsData.tabs.forEach(function(tab, i) {
      const li = document.createElement('li');
      li.setAttribute('role', 'presentation');
      if (i === 0) li.classList.add('active');
      const a = document.createElement('a');
      a.href = '#';
      a.setAttribute('role', 'tab');
      a.dataset.tab = tab.id;
      a.textContent = tab.label;
      li.appendChild(a);
      ul.appendChild(li);
    });
  }

  function renderTabContent() {
    const container = document.getElementById('settings-tab-content');
    container.innerHTML = '';

    settingsData.tabs.forEach(function(tab, i) {
      const pane = document.createElement('div');
      pane.className = 'tab-pane' + (i === 0 ? ' active' : '');
      pane.id = 'tab-' + tab.id;
      pane.setAttribute('role', 'tabpanel');

      if (tab.id === 'replace') {
        pane.innerHTML = renderReplaceTab();
      } else if (tab.id === 'login') {
        pane.innerHTML = renderLoginTab();
        bindLoginPane(pane);
      } else {
        pane.innerHTML = renderSettingsPanel(tab);
      }

      container.appendChild(pane);
    });
  }

  function renderSettingsPanel(tab) {
    let html = '<div class="panel-settings">';

    // Panel heading (tab info)
    if (tab.info) {
      html += '<div class="panel-heading">' + escapeHtml(tab.info) + '</div>';
    }

    // Filter settings for this tab
    const items = settingsData.settings.filter(function(s) {
      return s.tab === tab.id && !s.invisible;
    });

    if (items.length === 0) {
      html += '<div class="list-group"><div class="list-group-item"><em>この分類に該当する設定はありません</em></div></div>';
      if (tab.id === 'webui') html += renderStorageModeBlock();
      html += '</div>';
      return html;
    }

    html += '<div class="list-group">';
    items.forEach(function(setting) {
      html += renderSettingItem(setting);
    });
    html += '</div>';
    if (tab.id === 'webui') html += renderStorageModeBlock();
    html += '</div>';
    return html;
  }

  // ─── データ管理方式 (YAML / SQLite) ──────────────────────
  function renderStorageModeBlock() {
    return '<div class="list-group" id="storage-mode-panel">' +
      '<div class="list-group-item">' +
      '<h4 class="list-group-item-heading">データ管理方式</h4>' +
      '<div class="list-group-item-text">' +
      '<div class="setting-help">作品データを従来の YAML ファイルで管理するか、SQLite データベースで管理するかを選びます。' +
      'SQLite へ移行すると旧 YAML は <code>*.imported-*</code> へ退避され、戻すときは YAML を書き出してから切り替えます。' +
      'どちらの方式でも作品データはそのまま使えます。</div>' +
      '<div id="storage-mode-status"><em>読み込み中…</em></div>' +
      '<div class="storage-mode-actions">' +
      '<button type="button" class="btn btn-primary" id="storage-mode-to-sqlite">SQLite 管理へ移行</button>' +
      '<button type="button" class="btn btn-default" id="storage-mode-to-yaml">YAML 管理へ戻す</button>' +
      '</div>' +
      '</div></div></div>';
  }

  async function loadStorageMode() {
    const panel = document.getElementById('storage-mode-panel');
    if (!panel) return;
    const status = document.getElementById('storage-mode-status');
    const toSqlite = document.getElementById('storage-mode-to-sqlite');
    const toYaml = document.getElementById('storage-mode-to-yaml');
    const bind = function(button, mode) {
      if (!button || button.dataset.bound === '1') return;
      button.dataset.bound = '1';
      button.addEventListener('click', function() { switchStorageMode(mode, button); });
    };
    try {
      const resp = await fetch('/api/storage/mode');
      const data = await resp.json();
      const sqlite = data.mode === 'sqlite';
      const locked = Boolean(data.locked_by_env);
      let html = '<p class="storage-mode-current">現在: <strong>' +
        (sqlite ? 'SQLite 管理' : 'YAML 管理') + '</strong>' +
        (locked ? '（' + escapeHtml(data.reason || 'この環境では固定されています') + '）' : '') + '</p>';
      if (data.marker) {
        html += '<p class="setting-help storage-mode-paths">切替ファイル: <code>' + escapeHtml(data.marker) + '</code>' +
          (data.database ? ' / データベース: <code>' + escapeHtml(data.database) + '</code>' : '') + '</p>';
      }
      status.innerHTML = html;
      if (toSqlite) toSqlite.classList.toggle('hide', sqlite || locked);
      if (toYaml) toYaml.classList.toggle('hide', !sqlite || locked);
      bind(toSqlite, 'sqlite');
      bind(toYaml, 'yaml');
    } catch (e) {
      status.innerHTML = '<em>読み込みに失敗しました: ' + escapeHtml(e.message) + '</em>';
    }
  }

  async function switchStorageMode(mode, button) {
    if (button && button.disabled) return;
    if (mode === 'sqlite' && !window.confirm('作品データの管理を SQLite へ移行します。旧 YAML は自動で退避されます。よろしいですか？')) return;
    if (mode === 'yaml' && !window.confirm('YAML を書き出してから YAML 管理へ戻します。よろしいですか？')) return;
    if (button) button.disabled = true;
    try {
      const resp = await fetch('/api/storage/mode', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ mode: mode }),
      });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '切り替えに失敗しました');
      showToast(result.message || '切り替えました', 'success');
      setTimeout(function() { window.location.reload(); }, 1200);
    } catch (e) {
      showToast('切り替えに失敗しました: ' + e.message, 'error');
      if (button) button.disabled = false;
    }
  }

  function renderSettingItem(setting) {
    let html = '<div class="list-group-item" data-setting="' + escapeAttr(setting.name) + '">';
    html += '<h4 class="list-group-item-heading">' + escapeHtml(setting.name) + '</h4>';
    html += '<div class="list-group-item-text">';
    html += renderControl(setting);

    // Help text
    if (setting.help) {
      html += '<p class="setting-help">' + renderHelpHtml(setting.help) + '</p>';
    }

    html += '</div></div>';
    return html;
  }

  function renderControl(setting) {
    const name = setting.name;
    const value = setting.value;
    const type = setting.var_type;

    if (type === 'boolean') {
      if (setting.three_way) {
        return renderThreeWay(name, value);
      }
      return renderToggle(name, value);
    }

    if (type === 'select') {
      return renderSelect(name, value, setting.select_keys || [], setting.select_summaries || []);
    }

    if (type === 'multiple') {
      return renderMultiple(name, value, setting.select_keys || [], setting.select_summaries || []);
    }

    // text / integer / float / string / directory
    const placeholder = getPlaceholder(type);
    const strVal = (value !== null && value !== undefined) ? String(value) : '';
    return '<input type="text" class="setting-input" data-name="' + escapeAttr(name) +
           '" value="' + escapeAttr(strVal) + '" placeholder="' + escapeAttr(placeholder) + '">';
  }

  function renderToggle(name, value) {
    const checked = value === true ? ' checked' : '';
    return '<label class="switch-light">' +
           '<input type="checkbox" data-name="' + escapeAttr(name) + '"' + checked + '>' +
           '<span class="switch-track"></span>' +
           '<span class="switch-label-text">' + (value ? 'はい' : 'いいえ') + '</span>' +
           '</label>';
  }

  function renderThreeWay(name, value) {
    const nilChecked = (value === null || value === undefined) ? ' checked' : '';
    const offChecked = (value === false) ? ' checked' : '';
    const onChecked = (value === true) ? ' checked' : '';

    return '<div class="switch-3way">' +
           '<input type="radio" id="' + escapeAttr(name) + '-nil" name="' + escapeAttr(name) + '" value="nil"' + nilChecked + '>' +
           '<label for="' + escapeAttr(name) + '-nil">未設定</label>' +
           '<input type="radio" id="' + escapeAttr(name) + '-off" name="' + escapeAttr(name) + '" value="off"' + offChecked + '>' +
           '<label for="' + escapeAttr(name) + '-off">いいえ</label>' +
           '<input type="radio" id="' + escapeAttr(name) + '-on" name="' + escapeAttr(name) + '" value="on"' + onChecked + '>' +
           '<label for="' + escapeAttr(name) + '-on">はい</label>' +
           '</div>';
  }

  function renderSelect(name, value, keys, summaries) {
    let html = '<select class="setting-select" data-name="' + escapeAttr(name) + '">';
    const isTheme = (name === 'webui.theme');
    const isNewTagColor = (name === 'webui.new-tag-color');
    if (!isNewTagColor) {
      html += '<option value="">' + (isTheme ? 'デフォルト' : '未設定') + '</option>';
    }
    keys.forEach(function(key, index) {
      const selected = (value === key) ? ' selected' : '';
      const label = summaries[index] || key;
      html += '<option value="' + escapeAttr(key) + '"' + selected + '>' + escapeHtml(label) + '</option>';
    });
    html += '</select>';
    return html;
  }

  function renderMultiple(name, value, keys, summaries) {
    let selectedItems = [];
    if (Array.isArray(value)) {
      selectedItems = value;
    } else if (typeof value === 'string' && value) {
      selectedItems = value.split(',').map(function(s) { return s.trim(); });
    }

    let html = '<select class="setting-select" data-name="' + escapeAttr(name) + '" multiple>';
    keys.forEach(function(key, index) {
      const selected = selectedItems.includes(key) ? ' selected' : '';
      const label = summaries[index] || key;
      html += '<option value="' + escapeAttr(key) + '"' + selected + '>' + escapeHtml(label) + '</option>';
    });
    html += '</select>';
    return html;
  }

  function renderReplaceTab() {
    const content = settingsData.replace_content || '';
    return '<div class="panel-settings">' +
           '<div class="panel-heading">全小説対象の置換設定</div>' +
           '<div class="list-group"><div class="list-group-item">' +
           '<ul class="replace-info">' +
           '<li>・全ての小説に対する置換設定を行うことが出来ます</li>' +
           '<li>・変更を反映させるには再度変換を実行する必要があります</li>' +
           '</ul>' +
           '<textarea class="replace-textarea" id="replace-content">' + escapeHtml(content) + '</textarea>' +
           '</div></div></div>';
  }

  // ─── Login tab ─────────────────────────────────────────
  // The browser half is the separate narou_rs_login executable; this pane is
  // the receiving end: it reads the export that writes (by picking the file or
  // pasting its text) and stores the credentials encrypted at rest.
  function renderLoginTab() {
    return '<div class="panel-settings">' +
      '<div class="panel-heading">ログイン情報 (Cookie) の管理</div>' +
      '<div class="list-group">' +
      '<div class="list-group-item">' +
      '<h4 class="list-group-item-heading">保存済みのログイン</h4>' +
      '<div id="login-hosts" class="login-hosts"><em>読み込み中…</em></div>' +
      '<div class="setting-help" id="login-list-help">上から順にログインを試行します。値はマスクして表示しています。</div>' +
      '<div style="margin-top:0.5rem">' +
      '<button type="button" class="btn btn-default" id="login-refresh">再読み込み</button> ' +
      '<button type="button" class="btn btn-danger" id="login-clear-all">すべて削除</button>' +
      '</div>' +
      '</div>' +
      '<div class="list-group-item">' +
      '<h4 class="list-group-item-heading">書き出しファイルを取り込む</h4>' +
      '<div class="setting-help">narou_rs_login が書き出したファイル (YAML) を選択するか、内容を貼り付けて取り込みます。</div>' +
      '<div class="login-form">' +
      '<input type="file" class="login-envelope-file" id="login-envelope-file" accept=".yaml,.yml,.txt,.json">' +
      '<span class="login-file-name" id="login-file-name"></span>' +
      '</div>' +
      '<div class="login-form">' +
      '<input type="text" class="setting-input" id="login-import-name" placeholder="名前 (例: 本垢 / サブ垢。空ならファイルの名前を使います)">' +
      '</div>' +
      '<textarea class="replace-textarea login-envelope" id="login-envelope" placeholder="version: 1&#10;encrypted: true&#10;…"></textarea>' +
      '<div class="login-form">' +
      '<input type="password" class="setting-input" id="login-passphrase" placeholder="パスフレーズ (暗号化されている場合)">' +
      '<label class="login-replace"><input type="checkbox" id="login-replace"> 取り込みに含まれないサイトを削除する</label>' +
      '</div>' +
      '<div style="margin-top:0.5rem">' +
      '<button type="button" class="btn btn-primary" id="login-import">取り込む</button>' +
      '</div>' +
      '</div>' +
      '</div></div>';
  }

  function renderLoginSite(entry) {
    const logins = entry.logins || [];
    const state = entry.encrypted ? '暗号化済み' : '未暗号';
    let html = '<div class="login-site">' +
      '<div class="login-site-head">' +
      '<span class="login-site-name">' + escapeHtml(entry.site) + '</span>' +
      '<span class="login-site-state">' + state + '</span>' +
      '<button type="button" class="btn btn-default login-remove-site" data-site="' + escapeAttr(entry.site) + '">サイトを削除</button>' +
      '</div>';
    html += logins.map(function(login, index) {
      return renderLoginEntry(entry.site, login, index, logins.length);
    }).join('');
    html += '</div>';
    return html;
  }

  function renderLoginEntry(site, login, index, total) {
    const name = login.display_name || login.label || site;
    const hosts = login.hosts || [];
    const up = index > 0
      ? '<button type="button" class="login-cred-up" data-site="' + escapeAttr(site) + '" data-index="' + index + '" title="上へ"><span class="material-symbols-outlined icon-only" aria-hidden="true">keyboard_arrow_up</span></button>'
      : '<button type="button" class="login-cred-up" disabled title="上へ"><span class="material-symbols-outlined icon-only" aria-hidden="true">keyboard_arrow_up</span></button>';
    const down = index < total - 1
      ? '<button type="button" class="login-cred-down" data-site="' + escapeAttr(site) + '" data-index="' + index + '" title="下へ"><span class="material-symbols-outlined icon-only" aria-hidden="true">keyboard_arrow_down</span></button>'
      : '<button type="button" class="login-cred-down" disabled title="下へ"><span class="material-symbols-outlined icon-only" aria-hidden="true">keyboard_arrow_down</span></button>';
    const meta = [];
    if (typeof login.host_count === 'number') meta.push(login.host_count + ' ホスト');
    if (login.short_id) meta.push('ID: ' + login.short_id);
    if (login.added_at) meta.push(formatLoginAddedAt(login.added_at));
    return '<div class="login-cred-row">' +
      '<span class="login-cred-index">' + (index + 1) + '.</span>' +
      '<div class="login-cred-info">' +
      '<div class="login-cred-label">' + escapeHtml(name) + '</div>' +
      (meta.length ? '<div class="login-cred-added">' + escapeHtml(meta.join(' · ')) + '</div>' : '') +
      '<div class="login-cred-hosts">' +
      hosts.map(function(host) {
        const names = (host.names || []).join(', ');
        return '<div class="login-cred-host">' + escapeHtml(host.host) +
          (names ? ' <span class="login-cred-names">' + escapeHtml(names) + '</span>' : '') +
          (host.cookies ? '<div class="login-cred-cookies">' + escapeHtml(host.cookies) + '</div>' : '') +
          '</div>';
      }).join('') +
      '</div>' +
      '</div>' +
      '<span class="login-cred-actions">' +
      '<button type="button" class="btn btn-default login-cred-rename" data-site="' + escapeAttr(site) + '" data-index="' + index + '" data-label="' + escapeAttr(login.label || '') + '">名前を変更</button>' +
      up + down +
      '<button type="button" class="btn btn-default login-cred-remove" data-site="' + escapeAttr(site) + '" data-index="' + index + '">削除</button>' +
      '</span>' +
      '</div>';
  }

  function formatLoginAddedAt(value) {
    const date = new Date(value);
    if (isNaN(date.getTime())) return value;
    return date.toLocaleString();
  }

  function bindLoginPane(pane) {
    const refresh = pane.querySelector('#login-refresh');
    const clearAll = pane.querySelector('#login-clear-all');
    const importBtn = pane.querySelector('#login-import');
    const envelopeFile = pane.querySelector('#login-envelope-file');
    if (refresh) refresh.addEventListener('click', loadLoginHosts);
    if (clearAll) clearAll.addEventListener('click', clearAllLogin);
    if (importBtn) importBtn.addEventListener('click', importLoginEnvelope);
    if (envelopeFile) envelopeFile.addEventListener('change', readLoginEnvelopeFile);
  }

  /// The name to give what an import brings in: what the user typed, or the
  /// file's own name (minus its extension) when they left it empty.
  function importName() {
    const input = document.getElementById('login-import-name');
    const typed = input && input.value.trim();
    if (typed) return typed;
    const file = document.getElementById('login-envelope-file');
    const name = file && file.files && file.files[0] ? file.files[0].name : '';
    return name.replace(/\.[^.]*$/, '');
  }

  function readLoginEnvelopeFile() {
    const input = document.getElementById('login-envelope-file');
    const envelope = document.getElementById('login-envelope');
    const name = document.getElementById('login-file-name');
    if (!input || !envelope) return;
    const file = input.files && input.files[0];
    if (!file) {
      if (name) name.textContent = '';
      return;
    }
    const reader = new FileReader();
    reader.onload = function() {
      envelope.value = String(reader.result || '');
      if (name) name.textContent = file.name + ' を読み込みました';
    };
    reader.onerror = function() {
      if (name) name.textContent = '';
      showToast('ファイルを読み込めませんでした: ' + file.name, 'error');
    };
    reader.readAsText(file);
  }

  async function loadLoginHosts() {
    const container = document.getElementById('login-hosts');
    if (!container) return;
    try {
      const resp = await fetch('/api/login');
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '読み込みに失敗しました');
      renderLoginHosts(result.data);
    } catch (e) {
      container.innerHTML = '<em>読み込みに失敗しました: ' + escapeHtml(e.message) + '</em>';
    }
  }

  // Re-render the list from a mutation response's `data` (same shape as
  // GET /api/login), refetching when the response carried none.
  function refreshLoginHosts(data) {
    if (data && data.sites) {
      renderLoginHosts(data);
    } else {
      loadLoginHosts();
    }
  }

  function renderLoginHosts(data) {
    const container = document.getElementById('login-hosts');
    if (!container) return;
    const sites = (data && data.sites) || [];
    if (sites.length === 0) {
      container.innerHTML = '<em>保存されたログイン情報はありません。narou_rs_login で書き出したファイルを取り込んでください。</em>';
    } else {
      container.innerHTML = sites.map(renderLoginSite).join('');
      container.querySelectorAll('.login-cred-rename').forEach(function(btn) {
        btn.addEventListener('click', function() {
          renameLogin(btn.dataset.site, parseInt(btn.dataset.index, 10), btn.dataset.label || '');
        });
      });
      container.querySelectorAll('.login-cred-remove').forEach(function(btn) {
        btn.addEventListener('click', function() {
          removeLoginEntry(btn.dataset.site, parseInt(btn.dataset.index, 10));
        });
      });
      container.querySelectorAll('.login-remove-site').forEach(function(btn) {
        btn.addEventListener('click', function() { removeLoginSite(btn.dataset.site); });
      });
      const wireReorder = function(selector, direction) {
        container.querySelectorAll(selector).forEach(function(btn) {
          btn.addEventListener('click', function() {
            const site = btn.closest('.login-site');
            const total = site ? site.querySelectorAll('.login-cred-row').length : 0;
            moveLogin(btn.dataset.site, parseInt(btn.dataset.index, 10), direction, total);
          });
        });
      };
      wireReorder('.login-cred-up', -1);
      wireReorder('.login-cred-down', 1);
    }
    const help = document.getElementById('login-list-help');
    if (help && data) {
      const siteCount = (typeof data.sites_count === 'number') ? data.sites_count : sites.length;
      const loginCount = (typeof data.logins_count === 'number') ? data.logins_count : 0;
      let text = '保存済み: ' + siteCount + ' サイト / ' + loginCount + ' 件。上から順にログインを試行します。値はマスクして表示しています。';
      if (data.key_source) text += ' 鍵: ' + data.key_source;
      help.textContent = text;
    }
  }

  async function importLoginEnvelope() {
    const envelope = document.getElementById('login-envelope');
    const passphrase = document.getElementById('login-passphrase');
    const replace = document.getElementById('login-replace');
    if (!envelope || !envelope.value.trim()) {
      showToast('書き出しファイルを選択するか、内容を貼り付けてください', 'error');
      return;
    }
    try {
      const resp = await fetch('/api/login/import', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          envelope: envelope.value,
          passphrase: passphrase ? passphrase.value || null : null,
          replace: !!(replace && replace.checked),
          name: importName() || null,
        }),
      });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '取り込みに失敗しました');
      showToast(result.message || '取り込みました', 'success');
      envelope.value = '';
      if (passphrase) passphrase.value = '';
      const fileInput = document.getElementById('login-envelope-file');
      const fileName = document.getElementById('login-file-name');
      if (fileInput) fileInput.value = '';
      if (fileName) fileName.textContent = '';
      const nameInput = document.getElementById('login-import-name');
      if (nameInput) nameInput.value = '';
      refreshLoginHosts(result.data);
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function renameLogin(site, index, current) {
    const input = window.prompt(site + ' の ' + (index + 1) + ' 番目のログインの名前を入力してください。空にすると名前を消します。', current || '');
    if (input === null) return;
    try {
      const resp = await fetch('/api/login/rename', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ site: site, index: index, label: input.trim() }),
      });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '名前の変更に失敗しました');
      showToast(result.message || '名前を変更しました', 'success');
      refreshLoginHosts(result.data);
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function removeLoginEntry(site, index) {
    try {
      const resp = await fetch('/api/login/' + encodeURIComponent(site) + '/' + index, { method: 'DELETE' });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '削除に失敗しました');
      showToast(result.message || '削除しました', 'success');
      refreshLoginHosts(result.data);
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function moveLogin(site, index, direction, total) {
    const swap = index + direction;
    if (swap < 0 || swap >= total) return;
    const order = [];
    for (let i = 0; i < total; i++) order.push(i);
    const tmp = order[index];
    order[index] = order[swap];
    order[swap] = tmp;
    try {
      const resp = await fetch('/api/login/order', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ site: site, order: order }),
      });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '並べ替えに失敗しました');
      showToast(result.message || '並べ替えました', 'success');
      refreshLoginHosts(result.data);
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function removeLoginSite(site) {
    if (!window.confirm('サイト ' + site + ' のログイン情報をすべて削除します。よろしいですか？')) return;
    try {
      const resp = await fetch('/api/login/' + encodeURIComponent(site), { method: 'DELETE' });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '削除に失敗しました');
      showToast(result.message || '削除しました', 'success');
      refreshLoginHosts(result.data);
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function clearAllLogin() {
    if (!window.confirm('保存されているすべてのログイン情報を削除します。よろしいですか？')) return;
    try {
      const resp = await fetch('/api/login', { method: 'DELETE' });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '削除に失敗しました');
      showToast(result.message || '削除しました', 'success');
      refreshLoginHosts(result.data);
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  // ─── Events ────────────────────────────────────────────
  function bindEvents() {
    // Tab switching
    document.getElementById('settings-tabs').addEventListener('click', function(e) {
      const a = e.target.closest('a[data-tab]');
      if (!a) return;
      e.preventDefault();
      switchTab(a.dataset.tab);
    });

    // Save buttons
    document.getElementById('btn-save-settings').addEventListener('click', saveSettings);
    document.getElementById('btn-save-settings-bottom').addEventListener('click', saveSettings);

    // Toggle label update
    document.addEventListener('change', function(e) {
      if (e.target.type === 'checkbox' && e.target.closest('.switch-light')) {
        const label = e.target.parentElement.querySelector('.switch-label-text');
        if (label) {
          label.textContent = e.target.checked ? 'はい' : 'いいえ';
        }
      }
    });
  }

  function switchTab(tabId) {
    // Update tab pills
    document.querySelectorAll('#settings-tabs li').forEach(function(li) {
      li.classList.remove('active');
    });
    const targetLink = document.querySelector('#settings-tabs a[data-tab="' + tabId + '"]');
    if (targetLink) targetLink.parentElement.classList.add('active');

    // Update panes
    document.querySelectorAll('#settings-tab-content .tab-pane').forEach(function(pane) {
      pane.classList.remove('active');
    });
    const targetPane = document.getElementById('tab-' + tabId);
    if (targetPane) targetPane.classList.add('active');
    // 一覧は pane が DOM に乗ってから読む (タブを開くたびに最新化する)。
    if (tabId === 'login') loadLoginHosts();
    if (tabId === 'webui') loadStorageMode();

    // Remember active tab
    activeTab = tabId;
    try { localStorage.setItem('narou_settings_active_tab', tabId); } catch(e) {}
  }

  function restoreActiveTab(preferredTabId) {
    if (preferredTabId && document.querySelector('#settings-tabs a[data-tab="' + preferredTabId + '"]')) {
      switchTab(preferredTabId);
      return;
    }
    try {
      const saved = localStorage.getItem('narou_settings_active_tab');
      if (saved && document.querySelector('#settings-tabs a[data-tab="' + saved + '"]')) {
        switchTab(saved);
        return;
      }
    } catch(e) {}
    const firstTab = document.querySelector('#settings-tabs a[data-tab]');
    if (firstTab) {
      switchTab(firstTab.dataset.tab);
    }
  }

  // ─── Save ──────────────────────────────────────────────
  async function saveSettings() {
    if (saveInFlight) return;
    const settings = collectFormData();
    const body = { settings: settings };
    const tabToRestore = activeTab || getCurrentTabId();

    // Include replace content
    const replaceEl = document.getElementById('replace-content');
    if (replaceEl) {
      body.replace_content = replaceEl.value;
    }

    try {
      saveInFlight = true;
      setSaveButtonsDisabled(true);
      const resp = await fetch('/api/global_setting', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body)
      });
      if (!resp.ok) {
        throw new Error('設定の保存に失敗しました');
      }
      const result = await resp.json();
      if (!result || result.success !== true) {
        showToast(result?.message || '保存に失敗しました', 'error');
        return;
      }
      try {
        await reloadSettingsView(tabToRestore);
        showToast(result.message || '設定を保存しました', 'success');
      } catch (reloadError) {
        showToast((result.message || '設定を保存しました') + '（画面の再読み込みに失敗しました: ' + reloadError.message + '）', 'error');
      }
    } catch(e) {
      showToast('保存に失敗しました: ' + e.message, 'error');
    } finally {
      saveInFlight = false;
      setSaveButtonsDisabled(false);
    }
  }

  function collectFormData() {
    const data = {};

    // Checkboxes (normal boolean)
    document.querySelectorAll('.switch-light input[type="checkbox"]').forEach(function(input) {
      data[input.dataset.name] = input.checked;
    });

    // Radio buttons (3-way)
    document.querySelectorAll('.switch-3way').forEach(function(group) {
      const checked = group.querySelector('input[type="radio"]:checked');
      if (checked) {
        const name = checked.name;
        const val = checked.value;
        if (val === 'nil') {
          data[name] = null;
        } else if (val === 'off') {
          data[name] = false;
        } else {
          data[name] = true;
        }
      }
    });

    // Selects (single)
    document.querySelectorAll('select.setting-select:not([multiple])').forEach(function(sel) {
      const name = sel.dataset.name;
      const val = sel.value;
      data[name] = val === '' ? null : val;
    });

    // Selects (multiple)
    document.querySelectorAll('select.setting-select[multiple]').forEach(function(sel) {
      const name = sel.dataset.name;
      const selected = Array.from(sel.selectedOptions).map(function(opt) { return opt.value; });
      data[name] = selected.length > 0 ? selected.join(',') : null;
    });

    // Text inputs
    document.querySelectorAll('input.setting-input[type="text"]').forEach(function(input) {
      const name = input.dataset.name;
      const val = input.value.trim();
      data[name] = val === '' ? null : val;
    });

    return data;
  }

  function applyLoadedTheme() {
    const setting = findSetting('webui.theme');
    if (setting) {
      applyThemeValue(setting.value);
    }
  }

  function applyThemeValue(value) {
    const theme = normalizeTheme(value);
    try {
      if (theme === 'default') {
        localStorage.removeItem('narou-rs-webui-theme');
      } else {
        localStorage.setItem('narou-rs-webui-theme', theme);
      }
    } catch(e) {}
    document.documentElement.dataset.theme = theme === 'default' ? '' : theme;
  }

  function normalizeTheme(value) {
    return (!value || value === 'Cerulean' || value === 'default') ? 'default' : value;
  }

  function findSetting(name) {
    if (!settingsData || !Array.isArray(settingsData.settings)) return null;
    return settingsData.settings.find(function(setting) {
      return setting.name === name;
    }) || null;
  }

  async function reloadSettingsView(preferredTabId) {
    settingsData = await fetchSettingsData();
    applyLoadedTheme();
    renderTabs();
    renderTabContent();
    restoreActiveTab(preferredTabId);
  }

  async function fetchSettingsData() {
    const resp = await fetch('/api/global_setting');
    if (!resp.ok) {
      throw new Error('Failed to load settings');
    }
    return resp.json();
  }

  function getCurrentTabId() {
    return document.querySelector('#settings-tabs li.active a[data-tab]')?.dataset.tab || activeTab;
  }

  function setSaveButtonsDisabled(disabled) {
    [
      document.getElementById('btn-save-settings'),
      document.getElementById('btn-save-settings-bottom')
    ].forEach(function(button) {
      if (!button) return;
      button.disabled = disabled;
      button.setAttribute('aria-busy', disabled ? 'true' : 'false');
    });
  }

  // ─── Helpers ───────────────────────────────────────────
  function getPlaceholder(type) {
    switch (type) {
      case 'integer': return '整数を入力';
      case 'float': return '小数を入力';
      case 'directory': return 'フォルダパスを入力';
      default: return '値を入力';
    }
  }

  function renderHelpHtml(help) {
    if (!help) return '';
    return String(help).replace(/\n/g, '<br>');
  }

  function showToast(msg, type) {
    const toast = document.getElementById('settings-toast');
    toast.textContent = msg;
    toast.className = 'settings-toast ' + type;
    toast.style.display = 'block';
    // Force reflow
    toast.offsetHeight;
    toast.classList.add('show');
    setTimeout(function() {
      toast.classList.remove('show');
      setTimeout(function() { toast.style.display = 'none'; }, 300);
    }, 3000);
  }

  function escapeHtml(str) {
    if (!str) return '';
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function escapeAttr(str) {
    if (str === null || str === undefined) return '';
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/"/g, '&quot;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

})();
