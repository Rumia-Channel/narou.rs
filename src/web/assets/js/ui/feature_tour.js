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

  El.featureTourBody.replaceChildren(fragment);
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
