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
      html += '</div>';
      return html;
    }

    html += '<div class="list-group">';
    items.forEach(function(setting) {
      html += renderSettingItem(setting);
    });
    html += '</div></div>';
    return html;
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
  // the receiving end: it uploads the export it writes (or pastes a cookie
  // header) and stores the credentials encrypted at rest.
  function renderLoginTab() {
    return '<div class="panel-settings">' +
      '<div class="panel-heading">ログイン情報 (Cookie) の管理</div>' +
      '<div class="list-group">' +
      '<div class="list-group-item">' +
      '<h4 class="list-group-item-heading">保存済みのサイト</h4>' +
      '<div id="login-hosts" class="login-hosts"><em>読み込み中…</em></div>' +
      '<div class="setting-help">値は表示しません。削除はサイトごと、またはすべて行えます。</div>' +
      '<div style="margin-top:0.5rem">' +
      '<button type="button" class="btn btn-default" id="login-refresh">再読み込み</button> ' +
      '<button type="button" class="btn btn-danger" id="login-clear-all">すべて削除</button>' +
      '</div>' +
      '</div>' +
      '<div class="list-group-item">' +
      '<h4 class="list-group-item-heading">書き出しファイルを取り込む</h4>' +
      '<div class="setting-help">narou_rs_login が書き出したファイル (YAML) を貼り付けて取り込みます。</div>' +
      '<textarea class="replace-textarea login-envelope" id="login-envelope" placeholder="version: 1&#10;encrypted: true&#10;…"></textarea>' +
      '<div class="login-form">' +
      '<input type="password" class="setting-input" id="login-passphrase" placeholder="パスフレーズ (暗号化されている場合)">' +
      '<label class="login-replace"><input type="checkbox" id="login-replace"> 取り込みに含まれないサイトを削除する</label>' +
      '</div>' +
      '<div style="margin-top:0.5rem">' +
      '<button type="button" class="btn btn-primary" id="login-import">取り込む</button>' +
      '</div>' +
      '</div>' +
      '<div class="list-group-item">' +
      '<h4 class="list-group-item-heading">Cookie を直接登録する</h4>' +
      '<div class="setting-help">ブラウザからコピーした Cookie 文字列を 1 サイト分だけ保存します。</div>' +
      '<div class="login-form">' +
      '<input type="text" class="setting-input" id="login-host" placeholder="サイト (例: ncode.syosetu.com)">' +
      '<input type="text" class="setting-input" id="login-cookie" placeholder="Cookie 文字列 (例: over18=yes; ses=…)">' +
      '</div>' +
      '<div style="margin-top:0.5rem">' +
      '<button type="button" class="btn btn-primary" id="login-set">保存する</button>' +
      '</div>' +
      '</div>' +
      '</div></div>';
  }

  function bindLoginPane(pane) {
    const refresh = pane.querySelector('#login-refresh');
    const clearAll = pane.querySelector('#login-clear-all');
    const importBtn = pane.querySelector('#login-import');
    const setBtn = pane.querySelector('#login-set');
    if (refresh) refresh.addEventListener('click', loadLoginHosts);
    if (clearAll) clearAll.addEventListener('click', clearAllLogin);
    if (importBtn) importBtn.addEventListener('click', importLoginEnvelope);
    if (setBtn) setBtn.addEventListener('click', saveLoginCookie);
    loadLoginHosts();
  }

  async function loadLoginHosts() {
    const container = document.getElementById('login-hosts');
    if (!container) return;
    try {
      const resp = await fetch('/api/login');
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '読み込みに失敗しました');
      const hosts = (result.data && result.data.hosts) || [];
      if (hosts.length === 0) {
        container.innerHTML = '<em>保存されたログイン情報はありません。narou_rs_login で取得して取り込んでください。</em>';
      } else {
        container.innerHTML = hosts.map(function(entry) {
          return '<div class="login-host-row">' +
            '<span class="login-host-name">' + escapeHtml(entry.host) + '</span>' +
            '<span class="login-host-cookies">' + escapeHtml(entry.cookies) + '</span>' +
            '<button type="button" class="btn btn-default btn-xs login-remove" data-host="' + escapeAttr(entry.host) + '">削除</button>' +
            '</div>';
        }).join('');
        container.querySelectorAll('.login-remove').forEach(function(btn) {
          btn.addEventListener('click', function() { removeLoginHost(btn.dataset.host); });
        });
      }
      const key = (result.data && result.data.key_source) || '';
      const help = container.nextElementSibling;
      if (help && key) {
        help.textContent = '値は表示しません。鍵: ' + key;
      }
    } catch (e) {
      container.innerHTML = '<em>読み込みに失敗しました: ' + escapeHtml(e.message) + '</em>';
    }
  }

  async function importLoginEnvelope() {
    const envelope = document.getElementById('login-envelope');
    const passphrase = document.getElementById('login-passphrase');
    const replace = document.getElementById('login-replace');
    if (!envelope || !envelope.value.trim()) {
      showToast('書き出しファイルの内容を貼り付けてください', 'error');
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
        }),
      });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '取り込みに失敗しました');
      showToast(result.message || '取り込みました', 'success');
      envelope.value = '';
      if (passphrase) passphrase.value = '';
      loadLoginHosts();
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function saveLoginCookie() {
    const host = document.getElementById('login-host');
    const cookie = document.getElementById('login-cookie');
    if (!host || !cookie || !host.value.trim() || !cookie.value.trim()) {
      showToast('サイトと Cookie の両方を入力してください', 'error');
      return;
    }
    try {
      const resp = await fetch('/api/login/set', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ host: host.value.trim(), cookie: cookie.value }),
      });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '保存に失敗しました');
      showToast(result.message || '保存しました', 'success');
      host.value = '';
      cookie.value = '';
      loadLoginHosts();
    } catch (e) {
      showToast(e.message, 'error');
    }
  }

  async function removeLoginHost(host) {
    try {
      const resp = await fetch('/api/login/' + encodeURIComponent(host), { method: 'DELETE' });
      const result = await resp.json();
      if (!result.success) throw new Error(result.message || '削除に失敗しました');
      showToast(result.message || '削除しました', 'success');
      loadLoginHosts();
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
      loadLoginHosts();
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
