import { fetchJson, postJson } from '../core/http.js';
import { El } from '../core/state.js';

export function initFeatureTour() {
  El.featureTourClose?.addEventListener('click', closeFeatureTour);
  El.featureTourOk?.addEventListener('click', closeFeatureTour);
  El.featureTourDisableAuto?.addEventListener('change', saveDisableAutoTour);
}

export async function maybeShowPendingFeatureTour() {
  try {
    const data = await fetchJson('/api/feature_tour/pending');
    const entries = Array.isArray(data?.entries) ? data.entries : [];
    if (!entries.length) return;
    renderFeatureTour(entries, data);
    El.featureTourModal?.classList.remove('hide');
    const version = data.latest_pending_version || entries[entries.length - 1]?.version;
    if (version) {
      void postJson('/api/feature_tour/seen', { version }).catch(() => {});
    }
  } catch {
    // The tour is supplemental; failure must not block the main UI.
  }
}

export async function openFeatureTour() {
  try {
    const data = await fetchJson('/api/feature_tour/all');
    const entries = Array.isArray(data?.entries) ? data.entries : [];
    renderFeatureTour(entries, data, { manual: true });
    El.featureTourModal?.classList.remove('hide');
  } catch {
    renderFeatureTour([], {}, { manual: true });
    El.featureTourModal?.classList.remove('hide');
  }
}

function closeFeatureTour() {
  El.featureTourModal?.classList.add('hide');
}

function renderFeatureTour(entries, data = {}, options = {}) {
  if (!El.featureTourBody) return;
  if (El.featureTourDisableAuto) {
    El.featureTourDisableAuto.checked = Boolean(data.disabled);
  }

  const fragment = document.createDocumentFragment();
  const lead = document.createElement('p');
  lead.className = 'feature-tour-lead';
  lead.textContent = options.manual
    ? 'Narou.rs の主な追加・改善点です。'
    : '今回の Narou.rs で目立つ追加・改善点です。';
  fragment.appendChild(lead);

  if (entries.length) {
    const list = document.createElement('div');
    list.className = 'feature-tour-list';
    for (const entry of entries) {
      list.appendChild(renderEntry(entry));
    }
    fragment.appendChild(list);
  } else {
    const empty = document.createElement('p');
    empty.className = 'feature-tour-empty';
    empty.textContent = '表示できる新機能ツアーはありません。';
    fragment.appendChild(empty);
  }

  const migration = data.storage_migration;
  if (migration && migration.available && !options.manual) {
    fragment.appendChild(renderStorageMigrationPrompt());
  }

  El.featureTourBody.replaceChildren(fragment);
}

function renderStorageMigrationPrompt() {
  const box = document.createElement('div');
  box.className = 'feature-tour-storage';

  const title = document.createElement('p');
  title.className = 'feature-tour-storage-title';
  title.textContent = '管理方式の選択（0.4.0 移行）';
  box.appendChild(title);

  const desc = document.createElement('p');
  desc.className = 'feature-tour-storage-desc';
  desc.textContent =
    '0.4.0 から作品データの管理を SQLite へ移行できます。移行すると旧 YAML は自動退避され、いつでも export-yaml で戻せます。どちらの方式でも今までのデータがそのまま使えます。';
  box.appendChild(desc);

  const buttons = document.createElement('div');
  buttons.className = 'feature-tour-storage-actions';

  const sqliteButton = document.createElement('button');
  sqliteButton.type = 'button';
  sqliteButton.className = 'button primary';
  sqliteButton.textContent = 'Lite版（SQLite管理）へ移行';
  sqliteButton.addEventListener('click', () => chooseStorageMode('sqlite', sqliteButton));

  const yamlButton = document.createElement('button');
  yamlButton.type = 'button';
  yamlButton.className = 'button';
  yamlButton.textContent = '従来どおり YAML 管理';
  yamlButton.addEventListener('click', () => chooseStorageMode('yaml', yamlButton));

  buttons.appendChild(sqliteButton);
  buttons.appendChild(yamlButton);
  box.appendChild(buttons);
  return box;
}

async function chooseStorageMode(mode, button) {
  if (button.disabled) return;
  button.disabled = true;
  try {
    const result = await postJson('/api/storage/mode', { mode });
    window.alert(result.message || '設定しました');
    window.location.reload();
  } catch (error) {
    window.alert('設定に失敗しました: ' + error);
    button.disabled = false;
  }
}

async function saveDisableAutoTour() {
  const disabled = Boolean(El.featureTourDisableAuto?.checked);
  try {
    await postJson('/api/feature_tour/config', { disabled });
  } catch {
    if (El.featureTourDisableAuto) {
      El.featureTourDisableAuto.checked = !disabled;
    }
  }
}

function renderEntry(entry) {
  const article = document.createElement('article');
  article.className = 'feature-tour-entry';

  const header = document.createElement('div');
  header.className = 'feature-tour-entry-header';

  const title = document.createElement('h5');
  title.textContent = entry.title || '新機能';
  header.appendChild(title);

  const version = document.createElement('span');
  version.className = 'feature-tour-version';
  version.textContent = entry.version ? `v${entry.version}` : '';
  header.appendChild(version);
  article.appendChild(header);

  if (entry.body) {
    const body = document.createElement('p');
    body.className = 'feature-tour-body-text';
    body.textContent = entry.body;
    article.appendChild(body);
  }

  if (Array.isArray(entry.items) && entry.items.length) {
    const items = document.createElement('ul');
    items.className = 'feature-tour-items';
    for (const item of entry.items) {
      const li = document.createElement('li');
      li.textContent = item;
      items.appendChild(li);
    }
    article.appendChild(items);
  }

  return article;
}
