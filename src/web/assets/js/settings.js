/**
 * Settings page logic for Narou.rs WEB UI
 * Loads settings from API, renders tabs/forms, and handles save.
 */

(function() {
  'use strict';

  let settingsData = null;
  let activeTab = null;

  // ─── Init ──────────────────────────────────────────────
  document.addEventListener('DOMContentLoaded', init);

  async function init() {
    try {
      const resp = await fetch('/api/global_setting');
      if (!resp.ok) throw new Error('Failed to load settings');
      settingsData = await resp.json();
      renderTabs();
      renderTabContent();
      restoreActiveTab();
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
      html += '<p class="setting-help">' + escapeHtml(setting.help).replace(/\n/g, '<br>') + '</p>';
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
      return renderSelect(name, value, setting.select_keys || []);
    }

    if (type === 'multiple') {
      return renderMultiple(name, value, setting.select_keys || []);
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

  function renderSelect(name, value, keys) {
    let html = '<select class="setting-select" data-name="' + escapeAttr(name) + '">';
    const isTheme = (name === 'webui.theme');
    html += '<option value="">' + (isTheme ? 'デフォルト' : '未設定') + '</option>';
    keys.forEach(function(key) {
      const selected = (value === key) ? ' selected' : '';
      html += '<option value="' + escapeAttr(key) + '"' + selected + '>' + escapeHtml(key) + '</option>';
    });
    html += '</select>';
    return html;
  }

  function renderMultiple(name, value, keys) {
    let selectedItems = [];
    if (Array.isArray(value)) {
      selectedItems = value;
    } else if (typeof value === 'string' && value) {
      selectedItems = value.split(',').map(function(s) { return s.trim(); });
    }

    let html = '<select class="setting-select" data-name="' + escapeAttr(name) + '" multiple>';
    keys.forEach(function(key) {
      const selected = selectedItems.includes(key) ? ' selected' : '';
      html += '<option value="' + escapeAttr(key) + '"' + selected + '>' + escapeHtml(key) + '</option>';
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

  function restoreActiveTab() {
    try {
      const saved = localStorage.getItem('narou_settings_active_tab');
      if (saved && document.querySelector('#settings-tabs a[data-tab="' + saved + '"]')) {
        switchTab(saved);
      }
    } catch(e) {}
  }

  // ─── Save ──────────────────────────────────────────────
  async function saveSettings() {
    const settings = collectFormData();
    const body = { settings: settings };

    // Include replace content
    const replaceEl = document.getElementById('replace-content');
    if (replaceEl) {
      body.replace_content = replaceEl.value;
    }

    try {
      const resp = await fetch('/api/global_setting', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body)
      });
      const result = await resp.json();
      if (result.success) {
        showToast(result.message || '設定を保存しました', 'success');
      } else {
        showToast(result.message || '保存に失敗しました', 'error');
      }
    } catch(e) {
      showToast('保存に失敗しました: ' + e.message, 'error');
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

  // ─── Helpers ───────────────────────────────────────────
  function getPlaceholder(type) {
    switch (type) {
      case 'integer': return '整数を入力';
      case 'float': return '小数を入力';
      case 'directory': return 'フォルダパスを入力';
      default: return '値を入力';
    }
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
