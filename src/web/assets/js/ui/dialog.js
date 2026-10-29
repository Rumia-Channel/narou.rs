/**
 * アプリ内ダイアログ。
 *
 * ブラウザ標準の alert / confirm / prompt は見た目がページと揃わず、選択肢の
 * 一覧も出せないため、Web UI ではこのモジュールのダイアログを使う。
 * index.html と settings.html の両方から使えるよう、markup は初回の表示時に
 * 組み立てて body の末尾へ置く (HTML 側に変更を要求しない)。
 *
 *   const ok = await confirmDialog({ title: '削除', message: '…しますか？', danger: true });
 *   const name = await promptDialog({ title: '名前', message: '…', value: current });
 *   const picked = await selectDialog({ title: '選択', options: [{ value, label, detail }] });
 *   const values = await formDialog({ title: '登録', fields: [{ name, label, required }] });
 *   await alertDialog({ title: '通知', message: '…' });
 *
 * formDialog のフィールドに `suggestions` を渡すと、入力欄の下に候補リストが
 * 出る (ブラウザ標準の datalist ではなく自前のリスト)。↓ / ↑ で候補を動かし、
 * Enter またはクリックで入力欄へ入れる。
 *
 * いずれもキャンセル (× / キャンセル / Escape / 背景クリック) は null を返す
 * (confirm は false、alert は解決のみ)。
 */

const INVALID = Symbol('invalid');
const OVERLAY_ID = 'app-dialog';

let rootEl = null;
let titleEl = null;
let messageEl = null;
let fieldsEl = null;
let optionsEl = null;
let cancelEl = null;
let okEl = null;
let active = null;
let restoreFocusTo = null;
let optionSequence = 0;
let fieldSequence = 0;

/** 候補リストに出す最大件数。 */
const SUGGESTION_LIMIT = 20;

function createElement(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text != null) element.textContent = text;
  return element;
}

function ensureDialog() {
  if (rootEl) return;
  const root = createElement('div', 'modal-overlay hide');
  root.id = OVERLAY_ID;
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-modal', 'true');

  const dialog = createElement('div', 'modal-dialog modal-dialog-sm');
  const content = createElement('div', 'modal-content');

  const header = createElement('div', 'modal-header');
  titleEl = createElement('h4', 'modal-title');
  titleEl.id = 'app-dialog-title';
  const closeButton = createElement('button', 'modal-close', '×');
  closeButton.id = 'app-dialog-close';
  closeButton.type = 'button';
  closeButton.setAttribute('aria-label', '閉じる');
  header.append(titleEl, closeButton);

  const body = createElement('div', 'modal-body');
  messageEl = createElement('p', 'app-dialog-message');
  messageEl.id = 'app-dialog-message';
  fieldsEl = createElement('div', 'app-dialog-fields');
  fieldsEl.id = 'app-dialog-fields';
  optionsEl = createElement('div', 'app-dialog-options');
  optionsEl.id = 'app-dialog-options';
  body.append(messageEl, fieldsEl, optionsEl);

  const footer = createElement('div', 'modal-footer');
  cancelEl = createElement('button', 'btn btn-default');
  cancelEl.id = 'app-dialog-cancel';
  cancelEl.type = 'button';
  okEl = createElement('button', 'btn btn-primary');
  okEl.id = 'app-dialog-ok';
  okEl.type = 'button';
  footer.append(cancelEl, okEl);

  content.append(header, body, footer);
  dialog.appendChild(content);
  root.appendChild(dialog);
  document.body.appendChild(root);

  closeButton.addEventListener('click', () => closeActive(active ? active.cancelValue : null));
  cancelEl.addEventListener('click', () => closeActive(active ? active.cancelValue : null));
  okEl.addEventListener('click', submitActive);
  root.addEventListener('click', (event) => {
    if (event.target === root) closeActive(active ? active.cancelValue : null);
  });

  rootEl = root;
}

function closeActive(value) {
  if (!active) return;
  const current = active;
  active = null;
  document.removeEventListener('keydown', handleKeyDown, true);
  current.root.classList.add('hide');
  const previous = restoreFocusTo;
  restoreFocusTo = null;
  if (previous && typeof previous.focus === 'function') {
    try {
      previous.focus();
    } catch (_) {
      // 失われた要素へのフォーカス復帰失敗は無視する。
    }
  }
  current.resolve(value);
}

function submitActive() {
  if (!active) return;
  const value = active.read();
  if (value === INVALID) return;
  closeActive(value);
}

function handleKeyDown(event) {
  if (!active) return;
  const suggestions = event.target ? event.target.appDialogSuggestions : null;
  if (event.key === 'Escape') {
    // 候補リストが開いているときは、入力欄側でリストを閉じる (ダイアログは閉じない)。
    if (suggestions && suggestions.open) return;
    event.preventDefault();
    event.stopPropagation();
    closeActive(active.cancelValue);
    return;
  }
  if (event.key !== 'Enter') return;
  const target = event.target;
  if (target && (target.tagName === 'TEXTAREA' || target.tagName === 'BUTTON')) return;
  // ↓ / ↑ で候補を選んでいれば、入力欄側でその候補を反映する。
  if (suggestions && suggestions.activeIndex >= 0) return;
  event.preventDefault();
  submitActive();
}

function openDialog(options) {
  ensureDialog();
  if (active) closeActive(active.cancelValue);

  const {
    title = '確認',
    message = '',
    okLabel = 'OK',
    cancelLabel = 'キャンセル',
    danger = false,
    showCancel = true,
    cancelValue = null,
    build = null,
    read = () => true,
    focus = null,
  } = options;

  restoreFocusTo = document.activeElement;
  titleEl.textContent = title;
  messageEl.textContent = message;
  messageEl.classList.toggle('hide', !message);
  fieldsEl.replaceChildren();
  optionsEl.replaceChildren();
  fieldsEl.classList.add('hide');
  optionsEl.classList.add('hide');
  okEl.textContent = okLabel;
  okEl.className = danger ? 'btn btn-danger' : 'btn btn-primary';
  cancelEl.textContent = cancelLabel;
  cancelEl.classList.toggle('hide', !showCancel);

  const built = build ? build({ fields: fieldsEl, options: optionsEl }) || {} : {};

  rootEl.classList.remove('hide');
  return new Promise((resolve) => {
    active = {
      resolve,
      cancelValue,
      read,
      root: rootEl,
    };
    document.addEventListener('keydown', handleKeyDown, true);
    const focusTarget = focus || built.focus || okEl;
    if (focusTarget && typeof focusTarget.focus === 'function') {
      try {
        focusTarget.focus();
      } catch (_) {
        // フォーカスできない要素でもダイアログは開いたままにする。
      }
    }
  });
}

function buildFields(container, fields) {
  container.classList.remove('hide');
  const inputs = [];
  for (const field of fields) {
    const wrapper = createElement('div', 'app-dialog-field');
    const inputId = `app-dialog-field-${++fieldSequence}`;
    if (field.label) {
      const label = createElement('label', 'app-dialog-field-label', field.label);
      label.setAttribute('for', inputId);
      wrapper.appendChild(label);
    }
    const input = createElement('input', 'app-dialog-input');
    input.type = 'text';
    input.id = inputId;
    input.value = field.value || '';
    if (field.placeholder) input.placeholder = field.placeholder;
    input.addEventListener('input', () => input.classList.remove('app-dialog-input-invalid'));
    wrapper.appendChild(input);
    if (Array.isArray(field.suggestions) && field.suggestions.length > 0) {
      wrapper.appendChild(createSuggestionList(field, input));
    }
    container.appendChild(wrapper);
    inputs.push({ field, input });
  }
  return inputs;
}

/**
 * 入力欄の候補リスト。
 *
 * ブラウザ標準の `datalist` は候補がブラウザ既定の見た目になり、矢印キーでの
 * 操作もブラウザ依存だったため、同じスタイルの一覧を自前で作る。↓ / ↑ で候補を
 * 動かし、Enter またはクリックで入力欄へ入れる。
 */
function createSuggestionList(field, input) {
  const list = createElement('div', 'app-dialog-suggestions hide');
  const state = { items: [], activeIndex: -1, open: false };
  input.appDialogSuggestions = state;

  const close = () => {
    state.open = false;
    state.activeIndex = -1;
    state.items = [];
    list.replaceChildren();
    list.classList.add('hide');
  };

  const apply = (value) => {
    input.value = value;
    close();
  };

  const highlight = (index) => {
    const buttons = list.children;
    for (let i = 0; i < buttons.length; i += 1) {
      buttons[i].classList.toggle('active', i === index);
    }
    state.activeIndex = index;
    if (index >= 0 && buttons[index] && typeof buttons[index].scrollIntoView === 'function') {
      buttons[index].scrollIntoView({ block: 'nearest' });
    }
  };

  const render = () => {
    const query = input.value.trim().toLowerCase();
    const matches = [];
    for (const suggestion of field.suggestions) {
      const text = String(suggestion);
      if (!query || text.toLowerCase().startsWith(query)) matches.push(text);
      if (matches.length >= SUGGESTION_LIMIT) break;
    }
    state.items = matches;
    state.activeIndex = -1;
    list.replaceChildren();
    if (!matches.length) {
      state.open = false;
      list.classList.add('hide');
      return;
    }
    for (const text of matches) {
      const button = createElement('button', 'app-dialog-suggestion', text);
      button.type = 'button';
      button.addEventListener('click', () => apply(text));
      list.appendChild(button);
    }
    state.open = true;
    list.classList.remove('hide');
  };

  input.addEventListener('input', render);
  input.addEventListener('focus', render);
  input.addEventListener('blur', close);
  // 候補を押した瞬間にフォーカスが外れてもクリックが失われないよう、
  // リスト側の mousedown で既定のフォーカス移動を止める。
  list.addEventListener('mousedown', (event) => event.preventDefault());
  input.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown') {
      if (!state.items.length) render();
      if (!state.items.length) return;
      event.preventDefault();
      highlight(Math.min(state.activeIndex + 1, state.items.length - 1));
      return;
    }
    if (event.key === 'ArrowUp') {
      if (!state.items.length) return;
      event.preventDefault();
      highlight(Math.max(state.activeIndex - 1, 0));
      return;
    }
    if (event.key === 'Enter') {
      if (state.activeIndex < 0) return;
      event.preventDefault();
      event.stopPropagation();
      apply(state.items[state.activeIndex]);
      return;
    }
    if (event.key === 'Escape') {
      if (!state.open) return;
      event.preventDefault();
      event.stopPropagation();
      close();
      return;
    }
    if (event.key === 'Tab') {
      if (state.activeIndex >= 0) {
        apply(state.items[state.activeIndex]);
      } else {
        close();
      }
    }
  });
  return list;
}

function readFields(inputs) {
  const values = {};
  for (const entry of inputs) {
    const value = entry.input.value.trim();
    if (entry.field.required && !value) {
      entry.input.classList.add('app-dialog-input-invalid');
      entry.input.focus();
      return INVALID;
    }
    values[entry.field.name] = value;
  }
  return values;
}

/**
 * はい / いいえ の確認。OK で true、キャンセルで false。
 */
export function confirmDialog(options = {}) {
  const { title = '確認', message = '', okLabel = 'OK', cancelLabel = 'キャンセル', danger = false } = options;
  return openDialog({
    title,
    message,
    okLabel,
    cancelLabel,
    danger,
    cancelValue: false,
    read: () => true,
  });
}

/**
 * 通知。OK を押すまで待つ。
 */
export function alertDialog(options = {}) {
  const { title = '通知', message = '', okLabel = 'OK' } = options;
  return openDialog({
    title,
    message,
    okLabel,
    showCancel: false,
    cancelValue: null,
    read: () => true,
  });
}

/**
 * 1 行入力。空文字を許可しない場合は required (既定) を指定する。
 */
export function promptDialog(options = {}) {
  const {
    title = '入力',
    message = '',
    label = '',
    value = '',
    placeholder = '',
    okLabel = 'OK',
    cancelLabel = 'キャンセル',
    required = true,
  } = options;
  let inputs = [];
  return openDialog({
    title,
    message,
    okLabel,
    cancelLabel,
    cancelValue: null,
    build: ({ fields }) => {
      inputs = buildFields(fields, [
        { name: 'value', label, value, placeholder, required },
      ]);
      return { focus: inputs[0] ? inputs[0].input : null };
    },
    read: () => {
      const values = readFields(inputs);
      return values === INVALID ? INVALID : values.value;
    },
  });
}

/**
 * 複数項目の入力フォーム。
 *
 * fields: [{ name, label, value, placeholder, required }]
 * 戻り値: { [name]: string } / キャンセル時 null
 */
export function formDialog(options = {}) {
  const {
    title = '入力',
    message = '',
    fields = [],
    okLabel = 'OK',
    cancelLabel = 'キャンセル',
  } = options;
  let inputs = [];
  return openDialog({
    title,
    message,
    okLabel,
    cancelLabel,
    cancelValue: null,
    build: ({ fields: container }) => {
      inputs = buildFields(container, fields);
      return { focus: inputs[0] ? inputs[0].input : null };
    },
    read: () => readFields(inputs),
  });
}

/**
 * 一覧からの選択。options: [{ value, label, detail }]
 * 戻り値: 選ばれた value / キャンセル時 null
 */
export function selectDialog(options = {}) {
  const {
    title = '選択',
    message = '',
    options: choices = [],
    value = '',
    okLabel = 'OK',
    cancelLabel = 'キャンセル',
  } = options;
  let container = null;
  return openDialog({
    title,
    message,
    okLabel,
    cancelLabel,
    cancelValue: null,
    build: ({ options: optionsContainer }) => {
      container = optionsContainer;
      optionsContainer.classList.remove('hide');
      const name = `app-dialog-choice-${++optionSequence}`;
      for (const choice of choices) {
        const label = createElement('label', 'app-dialog-option');
        const input = createElement('input');
        input.type = 'radio';
        input.name = name;
        input.value = String(choice.value);
        input.checked = String(choice.value) === String(value);
        const body = createElement('span', 'app-dialog-option-body');
        body.appendChild(createElement('span', 'app-dialog-option-label', choice.label));
        if (choice.detail) {
          body.appendChild(createElement('span', 'app-dialog-option-detail', choice.detail));
        }
        label.append(input, body);
        optionsContainer.appendChild(label);
      }
      const checked = optionsContainer.querySelector('input[type="radio"]:checked');
      return { focus: checked || optionsContainer.querySelector('input[type="radio"]') };
    },
    read: () => {
      const checked = container ? container.querySelector('input[type="radio"]:checked') : null;
      return checked ? checked.value : null;
    },
  });
}
