# WEBUI.md — Narou.rs WEB UI 互換性トラッキング

narou.rb WEB UI と Rust版 WEB UI の要素・動作・レイアウトの互換性を追跡する。
配色やアイコンはモダン/オリジナルで可、要素と動作とレイアウトは COMMANDS.md 並みの厳しさで管理。

---

## 1. ページ一覧

| # | パス | 説明 | Rust | 状態 |
|---|------|------|------|------|
| 1 | `/` | メインページ (小説リスト) | `index.html` | ✅ |
| 2 | `/settings` | 環境設定ページ | `settings.html` + `settings.js` | ✅ |
| 3 | `/novels/:id/setting` | 個別小説設定 | `settings.js` 内で動的切替 | ✅ |
| 4 | `/help` | ヘルプページ | `window.open` で外部/内部 | ✅ |
| 5 | `/about` | バージョン情報 | `#about-modal` モーダル | ✅ |
| 6 | `/notepad` | メモ帳 (別ページ) | `notepad.html` | ✅ |
| 7 | `/novels/:id/author_comments` | 前書き/後書き | `author_comments.html` + API | ✅ |
| 8 | `/novels/:id/download` | ebook ダウンロード | `novels.rs` download_ebook | ✅ |
| 9 | `/_rebooting` | 再起動中表示 | `rebooting.html` | ✅ |
| 10 | `/edit_menu` | 編集メニュー | なし | ❌ |

---

## 2. メインページ要素

### 2.1 ナビバー

#### 2.1.1 ブランド

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| ブランドロゴ/テキスト | "Narou.rb MOD" + ロゴ画像 | "Narou.rs WEB UI" テキスト | ✅ |
| ブランドリンク | `/` | `/` | ✅ |

#### 2.1.2 表示(View)メニュー (左ドロップダウン1)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-view-frozen` | ❄ 凍結中を表示 (チェック) | ✅ (localStorage永続化) |
| 2 | `#action-view-nonfrozen` | 📖 凍結中以外を表示 (チェック) | ✅ |
| — | divider | — | ✅ |
| 3 | `#action-view-wide` | 📐 小説リストの幅を広げる (トグル) | ✅ |
| — | divider | — | ✅ |
| 4 | `#action-view-setting-newtab` | 🔗 設定を別タブで開く (チェック) | ✅ |
| — | divider | — | ✅ |
| 5 | `#action-view-buttons-top` | ⬆ ボタンを上に表示 (チェック) | ✅ |
| 6 | `#action-view-buttons-footer` | ⬇ ボタンをフッターに表示 (チェック) | ✅ |
| — | divider | — | ✅ |
| 7 | `#action-view-col-visibility` | 🔲 列の表示/非表示... | ✅ (列可視性モーダル) |

#### 2.1.3 選択(Select)メニュー (左ドロップダウン2)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-select-all` | ✅ 全て選択 (Ctrl+A) | ✅ |
| 2 | `#action-select-all-visible` | 📋 表示中を全て選択 (Shift+A) | ✅ |
| 3 | `#action-deselect-all` | ⬜ 全て解除 (Ctrl+Shift+A) | ✅ |
| — | divider | — | ✅ |
| 4 | `#action-select-mode-single` | 🔘 シングル選択 [S] | ✅ (チェック表示) |
| 5 | `#action-select-mode-rect` | ⬛ 範囲選択 [R] | ✅ |
| 6 | `#action-select-mode-hybrid` | 🔀 ハイブリッド選択 [H] | ✅ |

#### 2.1.4 タグ(Tag)メニュー (左ドロップダウン3)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-tag-edit` | 🏷 タグ編集 [T] | ✅ (タグ編集モーダル起動) |
| — | divider | — | ✅ |
| 2–N | 動的タグリスト | 既存タグ一覧 (クリックでフィルタ) | ✅ (API: `/api/tag_list`) |

#### 2.1.5 ツールメニュー (左ドロップダウン4)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-tool-dnd-window` | D&Dウィンドウを開く | 🟡 (HTML要素あり、別ウィンドウ未実装) |
| — | divider | — | ✅ |
| 2 | `#action-tool-csv-download` | CSV形式でリストをダウンロード | ✅ |
| 3 | `#action-tool-csv-import` | CSVファイルからインポート | ✅ (ファイルピッカー+API呼出) |
| — | divider | — | ✅ |
| 4 | `#action-tool-notepad` | メモ帳（別ページ） | ✅ (`/notepad` へ遷移) |
| 5 | `#action-tool-notepad-popup` | メモ帳（ポップアップ） | ✅ |

#### 2.1.6 オプションメニュー (右ドロップダウン ⚙)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-option-settings` | 🔧 環境設定... | ✅ |
| — | divider | — | ✅ |
| 2 | `#action-option-help` | ❓ ヘルプ... | ✅ |
| 3 | `#action-option-about` | ℹ️ Narou.rs について | ✅ (バージョン表示モーダル) |
| — | divider | — | ✅ |
| 4 | — | Language切替 (日本語 ↔ English) | ✅ (Rust独自) |
| — | divider | — | ✅ |
| 5 | — | テーマ選択 (Cerulean/Darkly/Readable/Slate/Superhero/United) | ✅ (セレクトボックス、localStorage永続化) |
| — | divider | — | ✅ |
| 6 | `#action-option-server-reboot` | 🔄 サーバを再起動 | ✅ (確認ダイアログ付き) |
| 7 | `#action-option-shutdown` | ⏻ サーバをシャットダウン | ✅ (確認ダイアログ付き) |

#### 2.1.7 キュー表示 (右ナビバー)

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| アイコン | `.glyphicon-inbox` | 📥 (Unicode) | ✅ |
| サイズバッジ | `.queue__sizes` (default + convert分割) | `#queue-count` 単一 | 🟡 (分割なし) |
| クリックでモーダル表示 | キューマネージャー | キューマネージャーモーダル | ✅ |
| ツールチップ | "クリックでキュー一覧を表示" | "クリックでキュー一覧を表示" | ✅ |
| アクティブ状態 (色変化) | `.queue.active` | `queue-size-active` | ✅ |

#### 2.1.8 フィルター入力 (右ナビバー)

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 検索アイコン | `#myFilter-search-icon` (.glyphicon-search) | `#filter-search-icon` (🔍) | ✅ |
| テキスト入力 | `#myFilter` | `#filter-input` | ✅ |
| クリアボタン | `#myFilter-clear` (.glyphicon-remove-circle) | `#filter-clear` (×) | ✅ |
| placeholder | "Filter" | "Filter" | ✅ |
| タグフィルタ構文 | `tag:xxx` | `tag:xxx` | ✅ |

---

### 2.2 コンソール

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| コンテナ | `#console-container` | `#console-container` | ✅ |
| 表示エリア | `#console.console` (dark bg) | `#console.console` | ✅ |
| キュー中断ボタン | `.queue-cancel` | `#console-cancel` (❌) | ✅ |
| 全履歴取得ボタン | `.console-history` | `#console-history` (☁) | ✅ |
| ゴミ箱ボタン | `.console-trash` | `#console-trash` (🗑) | ✅ |
| 拡大/縮小ボタン | `.console-expand` (full/small切替) | `#console-expand` (⤢/⤣) | ✅ |
| デュアルコンソール | 並行モード時に左右分割 | なし | ❌ |

---

### 2.3 コントロールパネル

#### 2.3.1 ボタン一覧

| # | ボタン | サブメニュー | Rust | 状態 |
|---|--------|-------------|------|------|
| 1 | **Download** (primary/青) | ドロップダウン: 強制再DL | ✅ (モーダル入力+D&D+強制再DLサブメニュー) | ✅ |
| 2 | **Update** (success/緑) | ドロップダウン: GL確認/タグ指定/表示中/凍結済み | ✅ (全4サブメニュー) | ✅ |
| 3 | **な** (success/緑) | — | ✅ | ✅ |
| 4 | **他** (success/緑) | — | ✅ | ✅ |
| 5 | **🔄** (success/緑) | — | ✅ (modifiedタグ付き更新) | ✅ |
| 6 | **Send** (warning/橙) | — | ✅ | ✅ |
| 7 | **Freeze** (info/水色) | ドロップダウン: 凍結/解除 | ✅ | ✅ |
| 8 | **Remove** (danger/赤) | — | ✅ (確認ダイアログ付き) | ✅ |
| 9 | **Convert** (default/白) | — | ✅ | ✅ |
| 10 | **Other** (default/白) | ドロップダウン: 差分/調査/フォルダ/バックアップ/設定焼付/メール | ✅ (全6サブメニュー) | ✅ |
| 11 | **Eject** (default/白, 隠し) | ドロップダウン | なし | ❌ |

#### 2.3.2 enable-selected 制御

| 要素 | Rust | 状態 |
|------|------|------|
| Send, Freeze, Remove, Convert, Other | `enable-selected` クラスでdisabled制御 | ✅ |
| ドロップダウンサブメニューの `enable-selected` リンク | `disabled` クラス切替 | ✅ |

#### 2.3.3 フッターナビバー (ボタン複製)

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| フッター固定表示 | 表示メニューで切替 | `#footer-navbar` | ✅ |
| ボタン複製 | メインコントロールパネルのクローン | cloneNode + click委譲 | ✅ |

---

### 2.4 小説リストテーブル

#### 2.4.1 カラム一覧

| # | カラム | 説明 | Rust | 状態 |
|---|--------|------|------|------|
| 1 | ID | 数値ID (凍結時 ＊ID) | ✅ | ✅ |
| 2 | 更新日 | 更新日 (時間バッジ: 1h/6h/24h/3d/1w + 新着●マーク) | ✅ (バッジ+新着マーク) | ✅ |
| 3 | 最新話掲載日 | general_lastup (時間バッジ付き、新着ヒント色) | ✅ (バッジ+hint-new-arrival) | ✅ |
| 4 | 更新チェック日 | last_check_date | ✅ | ✅ |
| 5 | タイトル | タイトル表示 | ✅ | ✅ |
| 6 | 作者名 | クリックでフィルタ | ✅ (.filterable) | ✅ |
| 7 | 掲載 | サイト名、クリックでフィルタ | ✅ (.filterable) | ✅ |
| 8 | 種別 | 短編/連載 | ✅ | ✅ |
| 9 | タグ | 色付きバッジ (7色対応) | ✅ | ✅ |
| 10 | 話数 | `N話` 形式 | ✅ | ✅ |
| 11 | 文字数 | 万字/千字 表示 (unitizeNumeric) | ✅ | ✅ |
| 12 | 状態 | 連載中/完結 | ✅ | ✅ |
| 13 | リンク | ToC URL (🔗アイコン) | ✅ | ✅ |
| 14 | 個別 | ⋯ メニューボタン (→コンテキストメニュー) | ✅ | ✅ |
| 15 | あらすじ | ℹボタンでポップオーバー表示 | ✅ (API: `/api/story`) | ✅ |

#### 2.4.2 行の状態表示

| 状態 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 選択行ハイライト | 黄色背景 | `.selected` 黄色背景 | ✅ |
| 凍結行 | 青色テキスト + ＊マーク | `.frozen` クラス + ＊マーク | ✅ |
| 新着マーク | マゼンタ ● | `.status-new-dot` ● | ✅ |
| 更新時間バッジ | 1h(赤)/6h(緑)/24h(青)/3d(灰)/1w(水色) | `.gl-badge.gl-1h/6h/24h/3d/1w` | ✅ |
| 新着ヒント (GL > last_update) | 背景色変化 | `.hint-new-arrival` | ✅ |
| 奇数/偶数行色 | CSS striping | CSS変数で指定 | ✅ |

#### 2.4.3 テーブル機能

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| ソート (ヘッダークリック) | DataTables server-side | JS クライアントサイドソート | ✅ |
| ソートインジケータ | ▲▼ アイコン | `.active-sort` + `.sort-asc` | ✅ |
| 列の表示/非表示切替 | DataTables ColVis | 列可視性モーダル (#colvis-modal) | ✅ |
| ページネーション | DataTables paging | なし (全件表示) | ❌ |
| 列ドラッグ並べ替え | — | なし | ❌ |

---

### 2.5 コンテキストメニュー (右クリック)

**Rust版: ✅ 実装済み — 全16項目 (14項目 + divider)**

| # | ラベル | 動作 | Rust |
|---|--------|------|------|
| 1 | 小説の変換設定 | `/novels/:id/setting` を開く | ✅ |
| 2 | 差分を表示 | diff モーダル表示 | ✅ |
| 3 | タグを編集 | タグ編集モーダル | ✅ |
| — | divider | — | ✅ |
| 4 | 凍結/凍結解除 | freeze toggle (動的ラベル) | ✅ |
| 5 | 更新 | update API | ✅ |
| 6 | 凍結済みでも更新 | update_force API | ✅ |
| 7 | 送信 | send API | ✅ |
| — | divider | — | ✅ |
| 8 | 削除 | remove (確認ダイアログ付き) | ✅ |
| 9 | 変換 | convert API | ✅ |
| 10 | 調査状況ログを表示 | inspect API | ✅ |
| — | divider | — | ✅ |
| 11 | 保存フォルダを開く | folder API | ✅ |
| 12 | バックアップを作成 | backup API | ✅ |
| 13 | 再ダウンロード | download_force API | ✅ |
| 14 | メールで送信 | mail API | ✅ |
| 15 | 作者コメント表示 | author_comments ページ表示 | ✅ |

---

### 2.6 範囲選択メニュー

Ruby版: `#rect-select-menu` — 範囲選択モードでドラッグ後に表示
**Rust版: ❌ 未実装 (選択モード切替のみ、ドラッグ選択動作は未実装)**

---

### 2.7 タグ色選択メニュー

**Rust版: ✅ 実装済み**

`#select-color-menu` — タグを右クリックで色選択コンテキストメニュー表示。
7色: Green, Yellow, Blue, Magenta, Cyan, Red, White
API: POST `/api/tag/change_color` → `tag_colors.yaml` に永続化

---

### 2.8 アラート・通知

| 種類 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| フェードアウト通知 | `.fadeout-alert` (fixed, z-1000) | `#notification-container` + `.notification-fadeout` | ✅ |
| 初回アクセスウェルカム | `.alert-info` + ヘルプリンク | なし | ❌ |
| パフォーマンスモード警告 | `#performance-info.alert-info.hide` | なし | ❌ |
| 全表示モード警告 | `#show-all-warning.alert-warning.hide` | なし | ❌ |

---

## 3. モーダルウィンドウ

### 3.1 キューマネージャーモーダル

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| モーダル表示 | `#queue-manager-modal` | `#queue-modal` | ✅ |
| 実行中タスク表示 | タスク文字列表示 | `#queue-running-list` | ✅ |
| 待機タスクリスト | ドラッグ並替 | `#queue-pending-list` (タスク詳細表示+個別削除) | 🟡 (ドラッグ並替なし) |
| キュー消去ボタン | あり | `#queue-clear-button` | ✅ |
| ドラッグ&ドロップ並替 | あり | なし | ❌ |
| 個別タスク取消 | あり | POST `/api/remove_pending_task` + 🗑ボタン | ✅ |

### 3.2 タグ編集モーダル

**Rust版: ✅ 実装済み (`#tag-edit-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 既存タグ表示 | 色付きバッジ | タグバッジ (×削除ボタン付き) | ✅ |
| タグ追加 | テキスト入力 | `#new-tag-input` + 追加ボタン | ✅ |
| タグ削除 | ×ボタン | `.tag-remove` ×ボタン | ✅ |
| 複数小説一括適用 | あり | あり (selectedIds / single ID) | ✅ |
| Enter で追加 | あり | あり | ✅ |

### 3.3 Aboutモーダル

**Rust版: ✅ 実装済み (`#about-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| バージョン表示 | あり | `#about-version` (APIから取得) | ✅ |
| 最新バージョンチェック | `/api/version/latest.json` | なし | ❌ |
| ライセンス情報 | あり | 簡易テキスト | 🟡 |

### 3.4 差分表示モーダル

**Rust版: ✅ 実装済み (`#diff-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 差分リスト取得 | あり | POST `/api/diff_list` | ✅ |
| 差分コマンド実行 | あり | POST `/api/diff` | ✅ |
| タイトル表示 | あり | `<h5>` タイトル | ✅ |
| 差分内容表示 | あり | `<pre>` preformatted | ✅ |
| 差分キャッシュ削除ボタン | あり | POST `/api/diff_clean` (エントリ毎の🗑ボタン) | ✅ |

### 3.5 確認ダイアログ

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 汎用確認モーダル | bootbox.js カスタム | `#confirm-modal` (HTML) + `confirm()` (JS) | ✅ |
| サーバー主導モーダル | `ping.modal` WebSocket | なし | ❌ |

### 3.6 メモ帳モーダル

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| モーダル表示 | ポップアップ版 | `#notepad-modal` | ✅ |
| テキスト編集 | あり | `#notepad` textarea | ✅ |
| 保存 | POST `/api/notepad/save` | `#save-notepad-button` | ✅ |
| 保存通知 | あり | showNotification() | ✅ |
| WebSocket 同期 | `notepad.change` イベント | なし | ❌ |
| 別ページ版 | `/notepad` (別ページ) | `notepad.html` | ✅ |

### 3.7 ダウンロードモーダル

**Rust版: ✅ 実装済み (`#download-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| URL/Nコード入力 | あり | `#download-input` textarea | ✅ |
| 複数入力 (スペース/改行区切り) | あり | あり | ✅ |
| D&D リンクドロップ | あり | `#download-link-drop-here` | ✅ |
| メール送信チェックボックス | あり | `#download-mail` checkbox | ✅ |
| ダウンロードボタン | あり | `#download-submit` | ✅ |
| キャンセルボタン | あり | `#download-cancel` | ✅ |

### 3.8 列可視性モーダル

**Rust版: ✅ 実装済み (`#colvis-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 全列のチェックボックス | DataTables ColVis | 13列のチェックリスト | ✅ |
| 全て表示/全て隠す/リセット | — | ✅ (3ボタン) | ✅ |
| localStorage永続化 | — | `narou-rs-webui-hidden-cols` | ✅ |

---

## 4. キーボードショートカット

**Rust版: ✅ 全12キー実装済み (shortcuts.js)**

| キー | 動作 | Rust |
|------|------|------|
| `Ctrl+A` | 表示されている小説を選択 | ✅ |
| `Shift+A` | 全ての小説を選択 | ✅ |
| `Ctrl+Shift+A` | 選択を全て解除 | ✅ |
| `ESC` | モーダル/コンテキストメニュー閉じ → 選択解除 | ✅ |
| `F5` | テーブルリフレッシュ | ✅ |
| `W` | 小説リストの幅を広げる切替 | ✅ |
| `F` | 凍結中を表示 | ✅ |
| `Shift+F` | 凍結中以外を表示 | ✅ |
| `S` | シングル選択モード | ✅ |
| `R` | 範囲選択モード | ✅ |
| `H` | ハイブリッド選択モード | ✅ |
| `T` | タグ編集 (選択時のみ) | ✅ |

---

## 5. テーマシステム

### 5.1 利用可能テーマ

**Rust版: ✅ 6テーマ全て実装 (theme.css, CSS変数)**

| テーマ | Rust | 状態 |
|--------|------|------|
| **Cerulean** (デフォルト) | `[data-theme=""]` | ✅ |
| **Darkly** | `[data-theme="Darkly"]` | ✅ |
| **Readable** | `[data-theme="Readable"]` | ✅ |
| **Slate** | `[data-theme="Slate"]` | ✅ |
| **Superhero** | `[data-theme="Superhero"]` | ✅ |
| **United** | `[data-theme="United"]` | ✅ |

### 5.2 テーマ切替

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| テーマ選択UI | ⚙メニューにテーマリスト | `#theme-select` セレクトボックス | ✅ |
| テーマ永続化 | `webui.theme` 設定値 | localStorage `narou-rs-webui-theme` | ✅ |
| サーバー側テーマ反映 | 設定値で初期テーマ決定 | `/api/webui/config` → `config.theme` | ✅ |

---

## 6. API エンドポイント

### 6.1 小説データ

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/list` | GET | ✅ |
| `/api/novels/count` | GET | ✅ |
| `/api/novels/all_ids` | GET | ✅ |
| `/api/novels/{id}` | GET | ✅ |
| `/api/novels/{id}` | DELETE | ✅ |
| `/api/webui/config` | GET | ✅ |

### 6.2 ダウンロード・更新・変換

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/download` | POST | ✅ (targets + force + mail) |
| `/api/update` | POST | ✅ (targets + force + --gl/--tag) |
| `/api/convert` | POST | ✅ |
| `/api/send` | POST | ✅ |
| `/api/mail` | POST | ✅ |
| `/api/backup` | POST | ✅ |
| `/api/inspect` | POST | ✅ |
| `/api/folder` | POST | ✅ |
| `/api/setting_burn` | POST | ✅ |
| `/api/diff_list` | POST | ✅ |
| `/api/diff` | POST | ✅ (差分コマンド実行) |
| `/api/diff_clean` | POST | ✅ (差分キャッシュ削除) |

### 6.3 凍結・削除 (バッチ)

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/novels/freeze` | POST | ✅ (BatchIdsBody) |
| `/api/novels/unfreeze` | POST | ✅ |
| `/api/novels/remove` | POST | ✅ |
| `/api/novels/{id}/freeze` | POST | ✅ (個別) |
| `/api/novels/{id}/unfreeze` | POST | ✅ |

### 6.4 タグ

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/tag_list` | GET | ✅ (tags + colors) |
| `/api/tag/change_color` | POST | ✅ |
| `/api/novels/{id}/tag` | POST | ✅ (単一タグ追加) |
| `/api/novels/{id}/tag` | DELETE | ✅ (単一タグ削除) |
| `/api/novels/{id}/tags` | POST | ✅ (複数タグ追加) |
| `/api/novels/{id}/tags` | PUT | ✅ (タグ置換) |
| `/api/novels/{id}/tags/remove` | POST | ✅ (複数タグ削除) |
| `/api/novels/tag` | POST | ✅ (バッチタグ追加) |
| `/api/novels/tag` | DELETE | ✅ (バッチタグ削除) |

### 6.5 キュー

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/queue/status` | GET | ✅ |
| `/api/queue/clear` | POST | ✅ |
| `/api/queue/cancel` | POST | ✅ |
| `/api/get_pending_tasks` | GET | ✅ (待機タスク詳細) |
| `/api/remove_pending_task` | POST | ✅ (タスク個別削除) |

### 6.6 設定

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/settings/{id}` | GET | ✅ (個別小説設定) |
| `/api/settings/{id}` | POST | ✅ |
| `/api/devices` | GET | ✅ |
| `/api/global_setting` | GET | ✅ |
| `/api/global_setting` | POST | ✅ |

### 6.7 ユーティリティ

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/csv/download` | GET | ✅ |
| `/api/csv/import` | POST | ✅ |
| `/api/notepad/read` | GET | ✅ |
| `/api/notepad/save` | POST | ✅ |
| `/api/version/current.json` | GET | ✅ |
| `/api/log/recent` | GET | ✅ |
| `/api/history` | GET | ✅ (コンソール全履歴) |
| `/api/clear_history` | POST | ✅ (履歴消去) |
| `/api/sort_state` | GET | ✅ (ソート状態取得) |
| `/api/sort_state` | POST | ✅ (ソート状態保存) |
| `/api/story` | GET | ✅ (あらすじ取得) |

### 6.8 システム

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/shutdown` | POST | ✅ |
| `/api/reboot` | POST | ✅ |

### 6.9 未実装 API (Ruby版にあるが Rust版未実装)

| エンドポイント | 説明 |
|-------------|------|
| `/api/version/latest.json` | 最新バージョンチェック |
| `/api/backup_bookmark` | 栞バックアップ |
| `/api/eject` | 端末取出し |
| `/api/validate_url_regexp_list` | URL正規表現一覧 |
| `/api/reorder_pending_tasks` | タスク並替 |
| `/api/cancel_running_task` | 実行中タスク取消 |

---

## 7. WebSocket

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| WebSocket接続 | port + 1 | port + 1 (config.ws_port) | ✅ |
| `echo` (コンソール出力) | S→C | ✅ (appendConsole) | ✅ |
| `log` / `console` | S→C | ✅ (appendConsole) | ✅ |
| `table.reload` / `refresh` / `list_updated` | S→C | ✅ (refreshList+refreshTags) | ✅ |
| `tag.updateCanvas` | S→C | ✅ (refreshTags) | ✅ |
| `status` / `queue` / `notification.queue` | S→C | ✅ (refreshQueue) | ✅ |
| 再接続 | 5秒リトライ | ✅ (5s setTimeout) | ✅ |
| `progressbar.init/step/clear` | S→C | ✅ (PushServer + main.js) | ✅ |
| `ping.modal` (サーバー主導モーダル) | S→C | なし | ❌ |
| `notepad.change` (メモ帳同期) | S→C | なし | ❌ |
| `device.ejectable` | S→C | なし | ❌ |

---

## 8. 設定ページ (`/settings`)

**Rust版: ✅ 実装済み (settings.js + settings.html)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| グローバル設定表示 | あり | GET `/api/global_setting` | ✅ |
| グローバル設定保存 | あり | POST `/api/global_setting` | ✅ |
| 個別小説設定ページ | `/novels/:id/setting` | GET/POST `/api/settings/{id}` | ✅ |
| デバイス一覧 | あり | GET `/api/devices` | ✅ |

---

## 9. JP/EN 言語切替

**Rust版独自: ✅ 実装済み (i18n.js)**

| 機能 | 状態 |
|------|------|
| `data-i18n` 属性による翻訳 | ✅ |
| localStorage 永続化 (`narou-rs-webui-language`) | ✅ |
| ナビバーメニュー切替ボタン | ✅ |
| 動的に全テキスト切替 | ✅ |

---

## 10. レスポンシブ対応

| 機能 | Rust版 | 状態 |
|------|--------|------|
| ハンバーガーメニュー (モバイル) | `#navbar-toggle-btn` | ✅ |
| 相対単位ベース (em, rem, %) | CSS変数 + responsive.css | ✅ |
| テーブル横スクロール | `overflow-x: auto` | ✅ |
| コンテキストメニュー位置補正 | viewport 端で補正 | ✅ |

---

## 11. localStorage 永続化

| キー | 内容 | 状態 |
|------|------|------|
| `narou-rs-webui-theme` | テーマ名 | ✅ |
| `narou-rs-webui-language` | ja / en | ✅ |
| `narou-rs-webui-view-frozen` | 凍結表示 | ✅ |
| `narou-rs-webui-view-nonfrozen` | 非凍結表示 | ✅ |
| `narou-rs-webui-wide-mode` | ワイドモード | ✅ |
| `narou-rs-webui-setting-new-tab` | 設定新タブ | ✅ |
| `narou-rs-webui-buttons-top` | ボタン上部 | ✅ |
| `narou-rs-webui-buttons-footer` | ボタンフッター | ✅ |
| `narou-rs-webui-select-mode` | 選択モード | ✅ |
| `narou-rs-webui-hidden-cols` | 非表示列 | ✅ |

---

## 12. 実装サマリ

**ページ**: 9/10 ✅ (メイン, 設定, ヘルプ, About, 個別設定, メモ帳, 作者コメント, ebook DL, 再起動)
**ナビバー要素**: 全メニュー ✅ (表示/選択/タグ/ツール/オプション)
**コントロールパネル**: 10/11 ボタン ✅ (Eject以外)
**コンテキストメニュー**: 15/15 項目 ✅ (作者コメント表示含む)
**モーダル**: 8/8 ✅ (タグ編集, About, 差分, 確認, メモ帳, ダウンロード, キュー, 列可視性)
**キーボードショートカット**: 12/12 ✅
**テーマ**: 6/6 ✅
**API**: 51 実装済み / 6 未実装 (eject, version/latest, backup_bookmark, validate_url, reorder, cancel_running)
**WebSocket**: 基本イベント ✅, 進捗バー ✅, モーダル/メモ帳同期 ❌
**設定ページ**: ✅
**言語切替**: ✅ (Rust独自)
**レスポンシブ**: ✅
**i18n 監査**: ✅ (JOB_TYPE_LABELS を Ruby版と完全一致に修正済み)
