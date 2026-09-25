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
| 5 | `/about` | バージョン情報 | `about.html` + `#about-modal` | ✅ |
| 6 | `/notepad` | メモ帳 (別ページ) | `notepad.html` | ✅ |
| 7 | `/novels/:id/author_comments` | 前書き/後書き | `author_comments.html` + API | ✅ |
| 8 | `/novels/:id/download` | ebook ダウンロード | `novels.rs` download_ebook | ✅ |
| 9 | `/_rebooting` | 再起動中表示 | `rebooting.html` | ✅ |
| 10 | `/bookmarklet` | ブックマークレット案内 | `bookmarklet.html` | ✅ |
| 11 | `/edit_menu` | 編集メニュー | `edit_menu.html` | ✅ |

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
| 1 | `#action-view-all` | 全ての項目を表示 | ✅ (凍結/非凍結ともに表示) |
| 2 | `#action-view-setting` | 表示する項目を設定 | ✅ (列可視性モーダル `#colvis-modal`) |
| — | divider | — | ✅ |
| 3 | `#action-view-novel-list-wide` | 小説リストの幅を広げる [W] (チェック) | ✅ |
| — | divider | — | ✅ |
| 4 | `#action-view-nonfrozen` | 凍結中以外を表示 [Shift+F] (チェック) | ✅ |
| 5 | `#action-view-frozen` | 凍結中を表示 [F] (チェック) | ✅ (localStorage永続化) |
| — | divider | — | ✅ |
| 6 | `#action-view-toggle-setting-new-tab` | 変換設定ページは新規タブで開く (チェック) | ✅ |
| — | divider | — | ✅ |
| 7 | `#action-view-toggle-buttons-top` | ボタンをページ上部に表示 (チェック) | ✅ |
| 8 | `#action-view-toggle-buttons-footer` | ボタンを画面下部に表示 (チェック) | ✅ |
| — | divider | — | ✅ |
| 9 | `#action-view-link-to-edit-menu` | 個別メニューを編集... | ✅ (`/edit_menu` を別タブで開く) |
| 10 | `#action-view-select-menu-style` | 個別メニューの表示スタイルを選択 | ✅ (表示スタイルモーダル) |
| — | divider | — | ✅ |
| 11 | `#action-view-feature-tour` | 新機能ツアーを表示 | ✅ (feature_tour モーダルを全エントリで開く) |
| — | divider | — | ✅ |
| 12 | `#action-view-reset` | 表示設定を全てリセット | ✅ (localStorage の表示系設定を初期値へ) |

#### 2.1.3 選択(Select)メニュー (左ドロップダウン2)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-select-view` | 表示されている小説を選択 (Ctrl+A) | ✅ |
| 2 | `#action-select-all` | 全ての小説を選択 (Shift+A) | ✅ |
| 3 | `#action-select-clear` | 選択を全て解除 (ESC) | ✅ |
| — | divider | — | ✅ |
| 4 | `#action-select-mode-single` | シングル選択モード [S] (チェック) | ✅ |
| 5 | `#action-select-mode-multi` | 範囲選択モード [R] (チェック) | ✅ |
| 6 | `#action-select-mode-hybrid` | ハイブリッド選択モード [H] (チェック) | ✅ |

#### 2.1.4 タグ(Tag)メニュー (左ドロップダウン3)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#tag-list-canvas` | 既存タグ一覧 (クリックでフィルタ) | ✅ (API: `/api/tag_list`) |
| — | divider | — | ✅ |
| 2 | `#action-tag-edit` | 選択した小説のタグを編集 [T] | ✅ (タグ編集モーダル起動) |

#### 2.1.5 ツールメニュー (左ドロップダウン4)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-tool-dnd-window` | D&Dウィンドウを開く | ✅ (`/widget/drag_and_drop` 別ウィンドウ) |
| — | divider | — | ✅ |
| 2 | `#action-tool-csv-download` | CSV形式でリストをダウンロード | ✅ |
| 3 | `#action-tool-csv-import` | CSVファイルからインポート | ✅ (ファイルピッカー+API呼出) |
| — | divider | — | ✅ |
| 4 | `#action-tool-notepad` | メモ帳（別ページ） | ✅ (`/notepad` へ遷移) |
| 5 | `#action-tool-notepad-popup` | メモ帳（ポップアップ） | ✅ |

#### 2.1.6 オプションメニュー (右ドロップダウン ⚙)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-option-settings` | 環境設定... | ✅ |
| — | divider | — | ✅ |
| 2 | `#action-option-help` | ヘルプ... | ✅ |
| 3 | `#action-option-about` | Narou.rs について | ✅ (バージョン表示モーダル) |
| — | divider | — | ✅ |
| 4 | `#action-lang-toggle` | Language切替 (日本語 ↔ English) | ✅ (Rust独自) |
| — | divider | — | ✅ |
| 5 | `#theme-select` | テーマ選択 (Cerulean/Darkly/Readable/Slate/Superhero/United) | ✅ (セレクトボックス、localStorage永続化) |
| — | divider | — | ✅ |
| 6 | `#action-option-server-reboot` | サーバを再起動 | ✅ (確認ダイアログ付き) |
| 7 | `#action-option-shutdown` | サーバをシャットダウン | ✅ (確認ダイアログ付き) |

#### 2.1.7 キュー表示 (右ナビバー)

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| アイコン | `.glyphicon-inbox` | material icon `move_to_inbox` | ✅ |
| サイズバッジ | `.queue__sizes` (default + convert分割) | `#queue-sizes` (`#queue-count` + 区切り + `#queue-count-convert`)。concurrency 有効時だけ convert 側を表示 | ✅ |
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
| URL / Nコード / 数値ID 検索 | — | 貼り付けた URL (`https://ncode.syosetu.com/n0421du/`)、Nコード (`n0421du`)、URL 末尾の数値 ID (`16818093093698047858`)、レコード ID (`3`) | ✅ |

一致規則はサーバ側の検索 (`/api/list` の `search[value]`) と同じで、URL は
レコードの `toc_url` に対する部分一致（末尾 `/` は無視）、Nコードは
`ncode` の部分一致または `toc_url` 最終セグメントとの完全一致、数値 ID は
レコード ID との完全一致。`-`/`^`/`!` の否定、`|` の OR、`tag:` / `author:` /
`site:` / `title:` / `status:` / `id:` のフィールド指定も同じ入力欄で使える。

---

### 2.2 コンソール

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| コンテナ | `#console-container` | `#console-container` | ✅ |
| 表示エリア | `#console.console` (dark bg) | `#console.console` | ✅ |
| キュー中断ボタン | `.queue-cancel` | `#console-cancel` | ✅ |
| 全履歴取得ボタン | `.console-history` | `#console-history` (☁) | ✅ |
| ゴミ箱ボタン | `.console-trash` | `#console-trash` (🗑) | ✅ |
| 拡大/縮小ボタン | `.console-expand` (full/small切替) | `#console-expand` (⤢/⤣) | ✅ |
| サブプロセス出力ストリーミング | `StreamingLogger`で$stdoutをキャプチャ→echo WS | `Stdio::piped()`+BufRead→echo WS | ✅ |
| デュアルコンソール | `concurrency`設定時に`$stdout2`で左右分割 | 外部通信あり(download/update/auto_update)は`#console`、その他(convert/send/backup/mail)は`#console-stdout2` | ✅ |

---

### 2.3 コントロールパネル

| # | ボタン | サブメニュー | Rust | 状態 |
|---|--------|-------------|------|------|
| 1 | **Download** (primary/青) | ドロップダウン: 選択した小説を強制再ダウンロード | ✅ (`#btn-download` モーダル入力 + 強制再DLサブメニュー) | ✅ |
| 2 | **Update** (success/緑) | ドロップダウン: 最新話掲載日を確認/タグを指定して更新/表示されている小説を更新/凍結済みでも更新 | ✅ (`#btn-update` + 全4サブメニュー) | ✅ |
| 3 | **な** (success/緑) | — | ✅ (`#btn-gl-narou`) | ✅ |
| 4 | **他** (success/緑) | — | ✅ (`#btn-gl-other`) | ✅ |
| 5 | **🔄** (success/緑) | — | ✅ (`#btn-gl-modified`、modifiedタグ付き更新) | ✅ |
| 6 | **Send** (warning/橙) | ドロップダウン: 端末の栞データをバックアップ | ✅ (`#btn-send` + backup_bookmark) | ✅ |
| 7 | **Freeze** (info/水色) | ドロップダウン: 選択した小説を凍結/凍結を解除 | ✅ | ✅ |
| 8 | **Remove** (danger/赤) | — | ✅ (`#btn-remove`、確認ダイアログ付き) | ✅ |
| 9 | **Convert** (default/白) | — | ✅ (`#btn-convert`) | ✅ |
| 10 | **Other** (default/白) | ドロップダウン: 差分表示/調査ログ/保存フォルダ/バックアップ/設定焼付/メール送信 | ✅ (全6サブメニュー) | ✅ |
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

`/api/list` の日時系 (`last_update`, `new_arrivals_date`, `general_lastup`, `last_check_date`) は narou.rb と同じく epoch integer を返し、描画側で表示文字列へ変換する。テーブルは全 19 列で、列可視性モーダル (`#colvis-modal`) はそのうちタイトル列以外の 18 列を切り替え対象にする。

| # | カラム | 説明 | Rust | 状態 |
|---|--------|------|------|------|
| 1 | ID | 数値ID (凍結時 ＊ID) | ✅ | ✅ |
| 2 | 更新日 | 日付/時刻の中央寄せ表示 + `新着` ラベル | ✅ (`date-cell` + `new-arrivals`) | ✅ |
| 3 | 最新話掲載日 | general_lastup (日付/時刻 + 時間バッジ、新着ヒント) | ✅ (`date-cell.hint-new-arrival` + `gl-badge`) | ✅ |
| 4 | 更新チェック日 | last_check_date | ✅ | ✅ |
| 5 | タイトル | タイトル表示 | ✅ | ✅ |
| 6 | 作者名 | クリックでフィルタ | ✅ (.filterable) | ✅ |
| 7 | 掲載 | サイト名、クリックでフィルタ | ✅ (.filterable) | ✅ |
| 8 | 種別 | 短編/連載 | ✅ | ✅ |
| 9 | タグ | 色付きバッジ (7色対応)、クリックでフィルタ | ✅ (tag:xxxフィルタ) | ✅ |
| 10 | 話数 | `N話` 形式 | ✅ | ✅ |
| 11 | 文字数 | 万字/千字 表示 (unitizeNumeric) | ✅ | ✅ |
| 12 | 平均文字数 | 文字数÷話数 (narou.rb 相当) | ✅ | ✅ |
| 13 | 状態 | 連載中/完結/中断 | ✅ | ✅ |
| 14 | リンク | ToC URL (🔗相当アイコン) | ✅ | ✅ |
| 15 | ＤＬ | `/novels/:id/download` への ebook ダウンロードリンク | ✅ | ✅ |
| 16 | 保存先 | 保存フォルダを開くボタン (folder API) | ✅ | ✅ |
| 17 | 更新 | 凍結済みでも更新ボタン (update_force) | ✅ | ✅ |
| 18 | あらすじ | ℹ相当ボタンでポップオーバー表示 | ✅ (API: `/api/story`) | ✅ |
| 19 | 個別 | ⋯ メニューボタン (→コンテキストメニュー) | ✅ | ✅ |

#### 2.4.2 行の状態表示

| 状態 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 選択行ハイライト | 黄色背景 | `.selected` 黄色背景 | ✅ |
| 凍結行 | 青色テキスト + ＊マーク | `.frozen` クラス + ＊マーク | ✅ |
| 新着/更新表示 | 6時間以内は `新着`(マゼンタ) / `更新`(緑) | `.new-arrivals` / `.new-update` | ✅ |
| 更新時間バッジ | 1h(赤)/6h(緑)/24h(青)/3d(灰)/1w(水色) | `.gl-badge.gl-1h/6h/24h/3d/1w` (general_lastup列) | ✅ |
| 新着ヒント (GL > last_update) | マゼンタの ● マーカー | `.hint-new-arrival` | ✅ |
| 奇数/偶数行色 | CSS striping | CSS変数で指定 | ✅ |

#### 2.4.3 テーブル機能

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| ソート (ヘッダークリック) | DataTables server-side | JS クライアントサイドソート（`/api/list` の server-side params は現状未使用） | ✅ |
| ソートインジケータ | ▲▼ アイコン | `.active-sort` + `.sort-asc` | ✅ |
| 列の表示/非表示切替 | DataTables ColVis | 列可視性モーダル (#colvis-modal) | ✅ |
| ページネーション | DataTables paging + length selector | JS クライアントサイド paging + `件分表示` selector | ✅ |
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
**Rust版: ✅ 実装済み (ドラッグ選択/矩形選択/ハイブリッド選択)**

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
| 待機タスクリスト | ドラッグ並替 | `#queue-pending-list` (詳細+個別削除+上下並替+ドラッグ) | ✅ |
| キュー消去ボタン | あり | `#queue-clear-button` | ✅ |
| 再読み込みボタン | あり | `#queue-reload-button` | ✅ |
| ドラッグ&ドロップ並替 | あり | あり (上下ボタン + ドラッグ) | ✅ |
| 個別タスク取消 | あり | POST `/api/remove_pending_task` + 🗑ボタン | ✅ |
| 実行中タスク中止 | あり | POST `/api/cancel_running_task` + ⏹ボタン | ✅ |

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
| 最新バージョンチェック | `/api/version/latest.json` | あり | ✅ |
| ワンクリック自動アップデート | なし | `#about-update` → `POST /api/update/start` (同梱 `narou_rs_updater(.exe)` がファイル置換+再起動) | ✅ |
| ブックマークレット案内 | あり | `/bookmarklet` | ✅ |
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
| 表示切替可能な列のチェックボックス | DataTables ColVis | 18列のチェックリスト (タイトル列は常時表示で対象外) | ✅ |
| 全て表示/全て隠す/リセット | — | ✅ (3ボタン) | ✅ |
| localStorage永続化 | — | `narou-rs-webui-hidden-cols` | ✅ |

### 3.9 タグ指定アップデートモーダル

**Rust版: ✅ 実装済み (`#update-by-tag-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| タグ一覧取得 | `/api/taginfo.json` | POST `/api/taginfo.json` | ✅ |
| 包含タグチェックボックス | `data-tagname` | `data-tagname` | ✅ |
| 除外タグチェックボックス | `data-exclusion-tagname` | `data-exclusion-tagname` | ✅ |
| タグ色表示 | `tag-label` with background-color | `tag-label` with style | ✅ |
| タグ件数表示 | `TAG(COUNT)` 形式 | `TAG(COUNT)` 形式 | ✅ |
| 更新実行 | POST `/api/update_by_tag` | POST `/api/update_by_tag` | ✅ |

### 3.10 GL確認モーダル

**Rust版: ✅ 実装済み (`#gl-update-modal`)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 説明テキスト表示 | bootbox.dialog | `#gl-update-modal` | ✅ |
| なろう小説チェックボックス | `narou` (デフォルトON) | `#gl-check-narou` | ✅ |
| その他チェックボックス | `other` (デフォルトOFF) | `#gl-check-other` | ✅ |
| modified更新チェックボックス | `updateModified` (デフォルトOFF) | `#gl-check-modified` | ✅ |
| localStorage永続化 | `update_general_lastup_checked` | 同左 | ✅ |
| 確認実行 | POST `/api/update_general_lastup` | 同左 | ✅ |
| 公式サイトリンク | `http://dev.syosetu.com/man/api/` | 同左 | ✅ |

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
| テーマ永続化 | `webui.theme` 設定値 | localStorage 優先、未保存時は `webui.theme` を初期値に使用 | ✅ |
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
| `/api/backup_bookmark` | POST | ✅ (栞バックアップ) |
| `/api/inspect` | POST | ✅ |
| `/api/folder` | POST | ✅ |
| `/api/setting_burn` | POST | ✅ |
| `/api/diff_list` | POST | ✅ |
| `/api/diff` | POST | ✅ (差分コマンド実行) |
| `/api/diff_clean` | POST | ✅ (差分キャッシュ削除) |
| `/api/update_by_tag` | POST | ✅ (タグ指定更新) |
| `/api/update_general_lastup` | POST | ✅ (GL確認更新) |
| `/api/cancel` | POST | ✅ (実行中タスクkillのみ、キュー消去なし) |
| `/api/download_force` | POST | ✅ (強制再DL) |

### 6.2b ログイン

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/login` | GET | ✅ (サイト別・試行順つき一覧。値はマスク) |
| `/api/login` | DELETE | ✅ (全削除) |
| `/api/login/import` | POST | ✅ (書き出しファイル取り込み、replace 対応) |
| `/api/login/set` | POST | ✅ (サイトの資格情報を置き換え) |
| `/api/login/add` | POST | ✅ (サイトの資格情報を末尾に追加) |
| `/api/login/order` | POST | ✅ (試行順の並替) |
| `/api/login/{host}` | DELETE | ✅ (サイト単位で削除) |
| `/api/login/{host}/{index}` | DELETE | ✅ (1 件だけ削除) |

### 6.3 凍結・削除 (バッチ)

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/novels/freeze` | POST | ✅ (BatchIdsBody) |
| `/api/novels/unfreeze` | POST | ✅ |
| `/api/freeze` | POST | ✅ (トグル) |
| `/api/freeze_on` | POST | ✅ |
| `/api/freeze_off` | POST | ✅ |
| `/api/novels/remove` | POST | ✅ (with_file: falseがデフォルト、削除ログをconsoleへ出力) |
| `/api/remove` | POST | ✅ (with_file パラメータ対応、削除ログをconsoleへ出力) |
| `/api/remove_with_file` | POST | ✅ (常にファイル削除、削除ログをconsoleへ出力) |
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
| `/api/edit_tag` | POST | ✅ (三状態バルク編集: 0=削除, 1=維持, 2=追加) |

### 6.5 キュー

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/queue/status` | GET | ✅ |
| `/api/queue/clear` | POST | ✅ |
| `/api/queue/cancel` | POST | ✅ (プロセスkill + pending消去) |
| `/api/cancel_running_task` | POST | ✅ (特定タスク取消) |
| `/api/get_pending_tasks` | GET | ✅ (待機タスク詳細) |
| `/api/remove_pending_task` | POST | ✅ (タスク個別削除) |
| `/api/reorder_pending_tasks` | POST | ✅ (タスク並替) |
| `/api/get_queue_size` | GET | ✅ (キューサイズ取得) |

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
| `/api/taginfo.json` | POST | ✅ (タグ情報+HTML) |
| `/api/validate_url_regexp_list` | GET | ✅ (URL正規表現一覧) |
| `/api/diff_history` | GET | ✅ (小説本文のバージョン履歴。SQLite 管理時) |
| `/api/diff_show` | GET | ✅ (保存済み差分の表示) |
| `/api/diff_restore` | POST | ✅ (指定バージョンへ復元) |
| `/api/diff_merge` | POST | ✅ (指定バージョンを作業セットへマージ) |
| `/api/novels/{id}/author_comments` | GET | ✅ (前書き/後書き取得) |

### 6.8 タスク復元

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/restore_pending_tasks` | POST | ✅ (保留タスク数報告) |
| `/api/defer_restore_pending_tasks` | POST | ✅ (保留タスク消去) |
| `/api/confirm_running_tasks` | POST | ✅ (再実行/延期判定) |

### 6.8b 新機能ツアー / ストレージ

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/feature_tour/pending` | GET | ✅ (未表示ツアー項目) |
| `/api/feature_tour/all` | GET | ✅ (現在バージョンまでの全項目) |
| `/api/feature_tour/seen` | POST | ✅ (表示済み version を記録) |
| `/api/feature_tour/config` | POST | ✅ (自動表示の有効/無効) |
| `/api/storage/mode` | GET/POST | ✅ (管理バックエンド yaml/sqlite の取得・切替。native-runtime のみ) |

### 6.9 システム

| エンドポイント | メソッド | Rust |
|-------------|--------|------|
| `/api/version/current.json` | GET | ✅ (現在のバージョン) |
| `/api/version/latest.json` | GET | ✅ (最新版と更新可否) |
| `/api/downloadable.gif` | GET | 🟡 透明 GIF を返すが、対象の登録状態に応じた画像の切替は未実装 |
| `/api/shutdown` | POST | ✅ |
| `/api/reboot` | POST | ✅ |
| `/api/update/start` | POST | ✅ (Rust拡張: 同梱 updater による自動アップデート) |
| `/api/library_backup` | GET/POST | ✅ (ライブラリ全体バックアップの状態確認/作成) |

### 6.10 未実装 / 廃止した API

| エンドポイント | 説明 |
|-------------|------|
| `/api/eject` | 端末取出し (未実装。実機検証必要) |
| `/api/download4ssl` | **削除済み** (旧ブックマークレットのクロスオリジン POST 用。CSRF ガード導入により使用不可となり、`/?register=<url>` same-origin 方式へ移行) |
| `/api/download_request` | **削除済み** (同上。旧 D&D ウィジェット互換の名残。`/?register=<url>` 経由で代替可能) |

### 6.11 ブックマークレット (same-origin 登録フロー)

旧ブックマークレットはクロスオリジンで `POST /api/download_request` を叩く方式だったが、`request_guard_middleware` の Origin チェックにより常に 403 となっていた (BUG-17)。

Rust版では以下のように CSRF ガードと両立する same-origin 方式を採用:

1. **`/bookmarklet` ページのブックマークレット** は現在表示中の URL を取得し、`window.open(<webui-origin> + '/?register=' + encodeURIComponent(location.href))` で WEB UI を別ウィンドウで開く
2. WEB UI 側 (`main.js` の `handleBookmarkletRegisterParam`) が `?register=` パラメータを検出
3. 確認ダイアログ (`#register-modal`) を表示し、対象 URL を明示
4. 承認時に既存の same-origin エンドポイント `POST /api/download` を JS から呼ぶ (`postJson('/api/download', { targets: [url] })`)
5. ブラウザは `Origin: <webui-origin>` を付与するため、`request_guard_middleware` の Origin チェックを通過する
6. 承認後に `history.replaceState` で URL から `register` パラメータを除去 (リロード時の再プロンプト防止)

| 要素 | 役割 |
|------|------|
| `/bookmarklet` | ブックマークレット取得ページ。`src/web/assets/bookmarklet.html` |
| `#register-modal` | 確認ダイアログ。URL表示 + キャンセル/登録ボタン |
| `handleRegisterParam(url)` (actions.js) | URL を検証してモーダルに表示 (http(s) のみ, 2048 文字以内) |
| `handleBookmarkletRegisterParam()` (main.js) | 起動時に `?register=` を検出して `handleRegisterParam` へ引き渡し |
| `POST /api/download` | 既存の same-origin DL キュー投入エンドポイントを流用 |

---

## 7. WebSocket

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| WebSocket接続 | port + 1 | port + 1 (config.ws_port) | ✅ |
| `echo` (コンソール出力) | S→C | ✅ (appendConsole, HTML span色付き対応) | ✅ |
| `log` / `console` | S→C | ✅ (appendConsole) | ✅ |
| `table.reload` / `refresh` / `list_updated` | S→C | ✅ (refreshList+refreshTags) | ✅ |
| `tag.updateCanvas` | S→C | ✅ (refreshTags) | ✅ |
| `status` / `queue` / `notification.queue` | S→C | ✅ (refreshQueue) | ✅ |
| `queue_start` / `queue_complete` / `queue_failed` / `queue_retry` | S→C | ✅ (`queue_retry` は失敗 job が `queue.max-retries` / `queue.retry-backoff` に従い `available_at` 付きで `active_pending` へ自動再投入されたタイミングで送出) | ✅ |
| `error` | S→C | ✅ (appendConsole) | ✅ |
| 再接続 | 5秒リトライ | ✅ (5s setTimeout) | ✅ |
| 接続時履歴送信 | 直近60件を接続時にプッシュ | ✅ (history_on_connect) | ✅ |
| 接続時コンソールクリア | 接続時にconsole.clear | ✅ (ws.onopen → console.clear) | ✅ |
| `shutdown` | S→C | ✅ (メッセージ表示) | ✅ |
| `reboot` | S→C | ✅ (/_rebooting へリダイレクト) | ✅ |
| `console.clear` | S→C | ✅ (コンソール内容クリア) | ✅ |
| TermColorLight色付き出力 | `<span>` HTML色付き | ✅ (termcolor.rs + innerHTML) | ✅ |
| `progressbar.init/step/clear` | S→C | ✅ (WebProgress + main.js、`stdout` / `stdout2` 各コンソール下端に表示して完了時に消去) | ✅ |
| `queue_partial` / `queue_cancelled` | — (Rust拡張) | ✅ (部分成功/取消の通知でキュー表示を更新) | ✅ |
| `ping.modal` (サーバー主導モーダル) | S→C | なし | ❌ |
| `notepad.change` (メモ帳同期) | S→C | なし | ❌ |
| `device.ejectable` | S→C | なし | ❌ |

---

## 8. 設定ページ (`/settings`)

**Rust版: ✅ 実装済み (settings.js + settings.html、保存した local/detail 設定の runtime 反映に加え、Ruby版相当の選択肢ラベル・help HTML・未設定時の主要既定値表示にも対応)**

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| グローバル設定表示 | あり | GET `/api/global_setting` | ✅ |
| グローバル設定保存 | あり | POST `/api/global_setting` | ✅ |
| 個別小説設定ページ | `/novels/:id/setting` | GET/POST `/api/settings/{id}` | ✅ |
| デバイス一覧 | あり | GET `/api/devices` | ✅ |

### 8.1 リバースプロキシ / 追加ホスト

外部公開や別ドメインからの `Host` ヘッダを許可するために、以下の設定キーを併用します。`s` は `setting` サブコマンドの短縮です（`web` は `w` / `we` で呼べます）。

- `server-bind` — LAN 公開時は `0.0.0.0` か LAN IP を指定（既定 `127.0.0.1`）。
- `server-reverse-proxy.enable` — nginx 等の前段 proxy が付与する外側 Host / Origin を許可（既定 `false`）。`true` の間は固定許可リストではなく外側 Host の構文妥当性だけで判定する。
- `server-basic-auth.require-for-external-bind` — `0.0.0.0` など外部 bind 時に Basic 認証必須にする narou.rs 独自ガード（既定 `true`）。
- `server-add-accepted-hosts` — HTTP `Host` ヘッダに追加で許可するホストのリスト（カンマ区切り）。`*.example.com` のような安全なワイルドカードに対応。unsafe なパターン（`*` 単独、`*.com`、末尾ワイルドなど）は警告ログを出して無視。既定の許可集合は bind host + loopback + 自ホスト名のまま据え置き、追加ホストだけを opt-in で広げる。

Web UI 設定画面に出る関連項目: `server-bind` / `server-basic-auth.*` / `server-ws-add-accepted-domains` / `server-add-accepted-hosts` / `over18`。`server-reverse-proxy.enable` と `server-basic-auth.require-for-external-bind` は hidden のため CLI からのみ変更します。

### 8.2 セルフアップデートの variant

Global タブの `self-update.variant` セレクトで、アップデート時に取得するリリースを選びます。

| 選択肢 | 保存値 | 取得するファイル |
|--------|--------|------------------|
| GPL版（AozoraEpub3_Lite 組込み） | `gpl` | `narou_rs_<platform>_<arch>-GPL.zip` (例: `narou_rs_win_x64-GPL.zip`) |
| 通常版（外部 AozoraEpub3） | `standard` | `narou_rs_<platform>_<arch>.zip` (例: `narou_rs_win_x64.zip`) |
| 未設定 | （設定を削除） | 実行中のビルドと同じ variant |

- CLI からも同じ設定を扱えます: `narou_rs setting self-update.variant=gpl` / `narou_rs setting self-update.variant`（読み取り）/ `narou_rs setting self-update.variant=`（削除）。
- 0.4.0 以下から更新するときの variant 選択モーダルは、この設定が未設定のときだけ表示されます。設定済みならモーダルは出ず、保存値の variant を取得します。
- 更新 API（`POST /api/update/start`）に `variant` を付けた場合は、その指定が最優先され、以後の更新のために保存されます。

### 8.3 ログインタブ

「ログイン」タブ (settings.js `renderLoginTab`) は `narou_rs_login` が書き出したファイルや、ブラウザからコピーした Cookie 文字列を取り込む受け口です。設定保存フォームとは独立して動き、Cookie 値は暗号化して保存されます。

| 機能 | 実装 | 状態 |
|------|------|------|
| 保存済みサイトの一覧 | GET `/api/login` → `#login-hosts`。値はマスク表示、暗号化状態を併記 | ✅ |
| 試行順の変更 | 各行の ↑↓ ボタン → POST `/api/login/order` | ✅ |
| 1 件ずつ/サイト単位/全削除 | DELETE `/api/login/{host}/{index}`、`/api/login/{host}`、DELETE `/api/login` | ✅ |
| 書き出しファイル取り込み | ファイル選択またはテキスト貼付 + パスフレーズ → POST `/api/login/import`。「取り込みに含まれないサイトを削除」は `--replace` 相当 | ✅ |
| Cookie 直接登録 | サイト + Cookie 文字列 (+任意ラベル) → POST `/api/login/set` (置き換え) / `/api/login/add` (追加) | ✅ |

### 8.4 新機能ツアー / ストレージ選択

| 機能 | 実装 | 状態 |
|------|------|------|
| 未読分のツアー自動表示 | 起動時 GET `/api/feature_tour/pending` → `#feature-tour-modal` | ✅ (0.2.3〜) |
| 全ツアー再表示 | 表示メニュー「新機能ツアーを表示」→ GET `/api/feature_tour/all` | ✅ |
| 既読記録 | POST `/api/feature_tour/seen` → ローカル設定の `webui.feature-tour.seen-version` | ✅ |
| 自動表示の無効化 | モーダル内「以降新機能ツアーは表示しない」→ POST `/api/feature_tour/config` (`webui.feature-tour.disabled`) | ✅ |
| SQLite 移行プロンプト | 0.4.0 ツアー内で GET/POST `/api/storage/mode`（YAML→SQLite 選択、`.narou/storage-backend` マーカー） | ✅ (native-runtime のみ) |

ライブラリの管理方式は既定で YAML のままです。SQLite はこのツアー (または `.narou/storage-backend`) で明示的に選んだ場合のみ有効で、CLI では `narou_rs db verify` / `export-yaml` / `vacuum` が保守用に使えます。

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
| `narou-rs-webui-setting-new-tab` | 変換設定ページを新規タブで開く | ✅ |
| `narou-rs-webui-buttons-top` | ボタン上部 | ✅ |
| `narou-rs-webui-buttons-footer` | ボタンフッター | ✅ |
| `narou-rs-webui-select-mode` | 選択モード | ✅ |
| `narou-rs-webui-hidden-cols` | 非表示列 | ✅ |
| `narou-rs-webui-page-length` | 1 ページの表示件数 | ✅ |
| `narou-rs-webui-list-state` | ソート列・方向・現在ページ (TTL 6時間) | ✅ |
| `narou_settings_active_tab` | 設定ページのアクティブタブ | ✅ (settings.js 独自のキー) |

---

## 12. 実装サマリ

**ページ**: 11/11 ✅ (メイン, 設定, ヘルプ, About, 個別設定, メモ帳, 作者コメント, ebook DL, 再起動, ブックマークレット, 編集メニュー)
**ナビバー要素**: 全メニュー ✅ (表示/選択/タグ/ツール/オプション。表示メニューに新機能ツアー・表示リセット・個別メニュー編集を含む)
**コントロールパネル**: 実装済み 10 ボタン群 (Download/Update/な/他/🔄/Send/Freeze/Remove/Convert/Other)。Eject は未実装
**コンテキストメニュー**: 15 項目 + divider ✅ (作者コメント表示含む。`/edit_menu` で項目編集可)
**モーダル**: タグ編集, About, 差分, 確認, メモ帳, ダウンロード, キュー, 列可視性, タグ指定アップデート, GL確認, 新機能ツアー等 ✅
**キーボードショートカット**: 12 キー ✅ (shortcuts.js)
**テーマ**: 6 テーマ ✅ (theme.css の CSS 変数で切替。フォールバック値など一部の hardcoded 色・px 値は残る)
**API**: 主要エンドポイントは実装済み (§6 参照)。未実装/廃止は §6.10 に列挙 (eject, download4ssl, download_request 等)
**WebSocket**: 基本イベント ✅, echo出力ストリーミング ✅, TermColorLight色付き出力 ✅, 進捗バー ✅, DB自動更新+table.reload+tag.updateCanvas ✅, 履歴on-connect ✅, console.clear ✅, shutdown/reboot ✅, 起動時バージョン表示+未完了タスク警告 ✅, モーダル/メモ帳同期 ❌
**設定ページ**: ✅ (`webui.performance-mode` / `webui.table.reload-timing` に加え、`download.interval` / `download.wait-steps` / `user-agent` / `guard-spoiler` / 各種 length-limit を runtime 反映。Ruby版相当の `select_summaries` 表示と help HTML 表示にも対応。`queue.max-retries` / `queue.retry-backoff` も detail タブに表示され、夜間更新で一時的なネットワーク失敗があった場合に指数バックオフで自動リトライする。Global タブに `self-update.variant`、ログインタブに Cookie 取り込み UI あり)
**言語切替**: ✅ (Rust独自)
**レスポンシブ**: ✅ (狭い画面では列可視性の既定表示を絞る)
**i18n 監査**: ✅ (JOB_TYPE_LABELS を Ruby版と完全一致に修正済み)
