// コンソール出力の HTML を安全に取り込む共有処理。
//
// ライブ出力 (appendConsole) と履歴の再描画 (replaceConsoleHistory) が
// 同じ経路を使うことで、履歴側だけ生のタグが露出する問題を避ける。
// 許可するのは <span> の色・太字と <ruby> の基底テキストだけで、
// その他の要素はテキストへ落とす。

export function sanitizeConsoleSpanStyle(styleText) {
  var declarations = String(styleText || '')
    .split(';')
    .map(function(part) { return part.trim(); })
    .filter(Boolean);
  if (!declarations.length || declarations.length > 2) return '';

  var color = '';
  var fontWeight = '';
  for (var i = 0; i < declarations.length; i++) {
    var separator = declarations[i].indexOf(':');
    if (separator <= 0) return '';
    var name = declarations[i].slice(0, separator).trim().toLowerCase();
    var value = declarations[i].slice(separator + 1).trim();
    if (name === 'color') {
      if (!/^[#(),.%\sa-zA-Z0-9-]+$/.test(value)) return '';
      color = value;
    } else if (name === 'font-weight') {
      if (value.toLowerCase() !== 'bold') return '';
      fontWeight = 'bold';
    } else {
      return '';
    }
  }
  if (!color) return '';

  var parts = [];
  if (fontWeight) parts.push('font-weight:bold');
  parts.push('color:' + color);
  return parts.join(';');
}

export function sanitizeConsoleNodes(source, target) {
  var children = source.childNodes || [];
  for (var i = 0; i < children.length; i++) {
    var child = children[i];
    if (child.nodeType === Node.TEXT_NODE) {
      target.appendChild(document.createTextNode(child.textContent || ''));
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && child.tagName === 'RUBY') {
      appendConsoleRubyBaseText(child, target);
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && (child.tagName === 'RT' || child.tagName === 'RP')) {
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && child.tagName === 'RB') {
      sanitizeConsoleNodes(child, target);
      continue;
    }
    if (child.nodeType === Node.ELEMENT_NODE && child.tagName === 'SPAN') {
      var style = sanitizeConsoleSpanStyle(child.getAttribute('style') || '');
      if (!style) {
        target.appendChild(document.createTextNode(child.textContent || ''));
        continue;
      }
      var span = document.createElement('span');
      span.setAttribute('style', style);
      sanitizeConsoleNodes(child, span);
      target.appendChild(span);
      continue;
    }
    target.appendChild(document.createTextNode(child.textContent || ''));
  }
}

export function appendConsoleRubyBaseText(source, target) {
  var rbElements = source.querySelectorAll('rb');
  if (rbElements.length > 0) {
    for (var i = 0; i < rbElements.length; i++) {
      target.appendChild(document.createTextNode(rbElements[i].textContent || ''));
    }
    return;
  }

  var children = source.childNodes || [];
  for (var i = 0; i < children.length; i++) {
    var child = children[i];
    if (child.nodeType === Node.TEXT_NODE) {
      target.appendChild(document.createTextNode(child.textContent || ''));
    } else if (child.nodeType === Node.ELEMENT_NODE && child.tagName !== 'RT' && child.tagName !== 'RP') {
      sanitizeConsoleNodes(child, target);
    }
  }
}

export function appendSanitizedConsoleHtml(target, html) {
  var template = document.createElement('template');
  template.innerHTML = html;
  sanitizeConsoleNodes(template.content, target);
}

// 1 行を描画する。タグを含む行はサニタイズして埋め、`<hr>` は罫線にする。
export function renderConsoleLine(target, line) {
  if (line === '<hr>' || /^―{30,}$/.test(line)) {
    var hr = document.createElement('hr');
    hr.className = 'console-line console-hr';
    target.appendChild(hr);
    return;
  }
  var div = document.createElement('div');
  div.className = 'console-line';
  // 許可する HTML は span と ruby だけ。それ以外はテキストとして扱う。
  if (/<(?:span|ruby|rb|rt|rp)[\s>]/i.test(line)) {
    appendSanitizedConsoleHtml(div, line);
  } else {
    div.textContent = line;
  }
  target.appendChild(div);
}
