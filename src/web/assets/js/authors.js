import { fetchJson, postJson, assertApiSuccess } from './core/http.js';

let current = null;
let mutationPending = false;
let readGeneration = 0;

export function bindAuthorsPane(pane) {
  pane.innerHTML = '<div class="panel-settings"><div class="panel-heading">作者の継続追跡</div>' +
    '<div class="list-group-item"><p>作者 URL を保存し、更新時に未登録の新作を取得します。通常のダウンロード欄への作者 URL 入力は、その時点の作品の一括取得だけです。</p>' +
    '<form class="author-form"><label for="author-url">追跡する作者の URL</label>' +
    '<input id="author-url" class="setting-input" type="url" required autocomplete="url" placeholder="https://mypage.syosetu.com/2842627/">' +
    '<div class="author-actions"><button class="btn btn-primary" type="submit">作者を登録</button>' +
    '<button class="btn btn-default" type="button" data-action="check">今すぐ確認</button>' +
    '<button class="btn btn-default" type="button" data-action="reload">一覧を再読み込み</button></div></form>' +
    '<p class="setting-help">このタブの操作は即時保存されます。「設定を保存」は不要です。追跡対象から削除しても、取得済み作品は削除しません。</p>' +
    '<p class="setting-help author-status" role="status" aria-live="polite"></p></div>' +
    '<ul class="list-group author-list"></ul></div>';
  const form = pane.querySelector('form');
  const input = pane.querySelector('input');
  const status = pane.querySelector('.author-status');
  const list = pane.querySelector('.author-list');
  let loading = false;
  const view = { pane, load, controls, status };
  current = view;

  function controls() {
    const busy = mutationPending || loading;
    pane.setAttribute('aria-busy', String(busy));
    pane.querySelectorAll('button,input').forEach((element) => { element.disabled = busy; });
    if (!busy) form.querySelector('[type="submit"]').disabled = !input.value.trim();
  }
  function note(text, failed = false) {
    status.textContent = text;
    status.classList.toggle('author-error', failed);
  }
  function render(authors) {
    list.replaceChildren();
    if (!authors.length) {
      const row = document.createElement('li'); row.className = 'list-group-item';
      row.textContent = '追跡中の作者はいません。'; list.append(row); return;
    }
    for (const author of authors) {
      const row = document.createElement('li'); row.className = 'list-group-item author-row';
      const details = document.createElement('div');
      const site = document.createElement('div'); site.textContent = author.site || 'サイト不明';
      const link = document.createElement('a'); link.className = 'author-url'; link.textContent = author.url;
      try {
        const url = new URL(author.url);
        if (['http:', 'https:'].includes(url.protocol)) { link.href = url.href; link.target = '_blank'; link.rel = 'noopener noreferrer'; }
      } catch (_) {}
      details.append(site, link);
      const remove = document.createElement('button'); remove.type = 'button';
      remove.className = 'btn btn-danger'; remove.textContent = '追跡対象から削除';
      remove.setAttribute('aria-label', author.url + ' の追跡を解除');
      remove.addEventListener('click', () => mutate(async () => {
        const result = assertApiSuccess(await fetchJson('/api/authors', { method: 'DELETE', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ url: author.url }) }));
        if (typeof result.changed !== 'boolean') throw new Error('削除結果の形式が不正です');
        return result.changed ? '作者の追跡を解除しました。取得済み作品は保持しています。' : 'この作者は既に追跡対象から外れています。';
      }));
      row.append(details, remove); list.append(row);
    }
  }
  async function load() {
    const ticket = ++readGeneration;
    loading = true; controls(); note('作者一覧を読み込み中…');
    try {
      const data = assertApiSuccess(await fetchJson('/api/authors'));
      if (data.success !== true || !Array.isArray(data.authors) || data.authors.some((author) => typeof author.url !== 'string' || typeof author.site !== 'string')) throw new Error('作者一覧の形式が不正です');
      if (ticket !== readGeneration || current !== view) return false;
      render(data.authors); note('追跡中の作者: ' + data.authors.length + ' 件');
      return true;
    } catch (error) {
      if (ticket === readGeneration && current === view) note('一覧の取得に失敗しました: ' + error.message, true);
      return false;
    } finally {
      if (ticket === readGeneration && current === view) { loading = false; controls(); }
    }
  }
  async function mutate(operation) {
    if (mutationPending || loading) return;
    mutationPending = true; ++readGeneration; controls(); note('処理中…');
    try {
      const message = await operation();
      if (current && await current.load()) {
        current.status.textContent = message;
        current.status.classList.remove('author-error');
      }
    } catch (error) {
      if (current) { current.status.textContent = '操作に失敗しました: ' + error.message; current.status.classList.add('author-error'); }
    } finally {
      mutationPending = false;
      if (current) current.controls();
    }
  }
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    if (!form.reportValidity()) return;
    const url = input.value.trim();
    mutate(async () => {
      const result = assertApiSuccess(await postJson('/api/authors', { url }));
      if (typeof result.changed !== 'boolean') throw new Error('登録結果の形式が不正です');
      input.value = '';
      return result.changed ? '作者を登録しました。更新時から継続して確認します。' : 'この作者は既に登録されています。';
    });
  });
  input.addEventListener('input', controls);
  pane.querySelector('[data-action="reload"]').addEventListener('click', () => { if (!mutationPending) load(); });
  pane.querySelector('[data-action="check"]').addEventListener('click', () => mutate(async () => {
    const result = assertApiSuccess(await postJson('/api/authors/check', { dry_run: false }));
    if (!Number.isInteger(result.count)) throw new Error('確認結果の形式が不正です');
    return '作者の確認・取得を受け付けました（' + result.count + ' 件のジョブ）。進捗は小説一覧のコンソールで確認できます。';
  }));
  pane.addEventListener('authors:reload', () => { if (!mutationPending) load(); });
  load();
}
