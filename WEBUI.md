# WEBUI.md — narou.rb WEB UI 完全仕様 vs narou.rs 実装状況

> narou.rb (Ruby版) の WEB UI の全ページ・モーダル・要素・レイアウト・API を網羅的に記録し、
> narou.rs (Rust版) の現在の実装と比較する。

凡例: ✅ 実装済み / 🟡 部分実装 / ❌ 未実装

---

## 1. ページ一覧

| # | パス | 説明 | Rust |
|---|------|------|------|
| 1 | `/` | メインページ (小説リスト・コンソール・コントロール) | 🟡 |
| 2 | `/settings` | 環境設定ページ (グローバル/ローカル設定) | ❌ |
| 3 | `/help` | ヘルプページ | ❌ |
| 4 | `/about` | About ダイアログ (部分テンプレート) | ❌ |
| 5 | `/novels/:id/setting` | 個別小説の変換設定 (GET/POST) | ❌ |
| 6 | `/novels/:id/author_comments` | 前書き/後書きビューア | ❌ |
| 7 | `/novels/:id/download` | EPUB/端末ファイルのDL | ❌ |
| 8 | `/notepad` | メモ帳 (別ページ版) | ❌ |
| 9 | `/edit_menu` | 個別メニューエディター | ❌ |
| 10 | `/_rebooting` | 再起動中表示 | ❌ |

---

## 2. メインページ (`/`) — 全要素詳細

### 2.1 ナビバー (固定上部)

**構造**: `nav#header-navbar.navbar.navbar-default.navbar-fixed-top`

#### 2.1.1 ブランド

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| ブランド文字 | "Narou.rb M⚡D WEB UI" (bootsnap indicator付き) | "Narou.rs WEB UI" | ✅ |
| ハンバーガーメニュー (モバイル) | `.navbar-toggle` × 3 icon-bar | `#navbar-toggle-btn` × 3 icon-bar | ✅ |

#### 2.1.2 表示メニュー (左ドロップダウン1)

| # | ID | ラベル | ショートカット | Rust |
|---|-----|--------|---------------|------|
| 1 | `#action-view-all` | 全ての項目を表示 | — | 🟡 (「全ての小説を表示」) |
| 2 | `#action-view-setting` | 表示する項目を設定 | — | ❌ |
| — | divider | — | — | — |
| 3 | `#action-view-novel-list-wide` | ✓ 小説リストの幅を広げる | W | 🟡 (ショートカットなし) |
| — | divider | — | — | — |
| 4 | `#action-view-nonfrozen` | ✓ 凍結中以外を表示 | Shift+F | 🟡 (ショートカットなし) |
| 5 | `#action-view-frozen` | ✓ 凍結中を表示 | F | 🟡 (ショートカットなし) |
| — | divider | — | — | — |
| 6 | `#action-view-toggle-setting-page-open-new-tab` | ✓ 変換設定ページは新規タブで開く | — | ❌ |
| — | divider | — | — | — |
| 7 | `#action-view-toggle-buttons-show-page-top` | ✓ ボタンをページ上部に表示 | — | ❌ |
| 8 | `#action-view-toggle-buttons-fix-footer` | ✓ ボタンを画面下部に表示 | — | ❌ |
| — | divider | — | — | — |
| 9 | `#action-view-link-to-edit-menu` | 個別メニューを編集... | — | ❌ |
| 10 | `#action-view-select-menu-style` | 個別メニューの表示スタイルを選択 | — | ❌ |
| — | divider | — | — | — |
| 11 | `#action-view-reset` | 表示設定を全てリセット | — | ❌ |

#### 2.1.3 選択メニュー (左ドロップダウン2)

| # | ID | ラベル | ショートカット | Rust |
|---|-----|--------|---------------|------|
| 1 | `#action-select-view` | 表示されている小説を選択 | Ctrl+A | ✅ |
| 2 | `#action-select-all` | 全ての小説を選択 | Shift+A | ✅ |
| 3 | `#action-select-clear` | 選択を全て解除 | ESC | ✅ |
| — | divider | — | — | ❌ |
| 4 | `#action-select-mode-single` | ✓ シングル選択モード | S | ❌ |
| 5 | `#action-select-mode-rect` | ✓ 範囲選択モード | R | ❌ |
| 6 | `#action-select-mode-hybrid` | ✓ ハイブリッド選択モード | H | ❌ |

#### 2.1.4 タグメニュー (左ドロップダウン3)

| # | ID | ラベル | ショートカット | Rust |
|---|-----|--------|---------------|------|
| 1 | `#tag-list-canvas` | タグ一覧 (色付きバッジ、クリックでフィルタ) | — | 🟡 (canvas有、色未対応) |
| — | divider | — | — | ✅ |
| 2 | `#action-tag-edit.enable-selected` | 選択した小説のタグを編集 | T | 🟡 (ショートカットなし) |

#### 2.1.5 ツールメニュー (左ドロップダウン4)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-tool-open-dnd-window` | D&Dウィンドウを開く | ❌ |
| — | divider | — | — |
| 2 | `#action-tool-csv-download` | CSV形式でリストをダウンロード | ✅ |
| 3 | `#action-tool-csv-import` | CSVファイルからインポート | ❌ |
| — | divider | — | — |
| 4 | `#action-tool-notepad` | メモ帳（別ページ） | 🟡 (モーダル版のみ) |
| 5 | `#action-tool-notepad-window` | メモ帳（ポップアップ） | ❌ |

#### 2.1.6 オプションメニュー (右ドロップダウン ⚙)

| # | ID | ラベル | Rust |
|---|-----|--------|------|
| 1 | `#action-option-settings` | 🔧 環境設定... (`/settings` リンク) | ❌ |
| — | divider | — | — |
| 2 | `#action-option-help` | ❓ ヘルプ... (`/help` リンク) | ❌ |
| 3 | `#action-option-about` | ℹ Narou.rb について | 🟡 (モーダルなし) |
| — | divider | — | — |
| 4 | — | テーマ選択 (Cerulean/Darkly/Readable/Slate/Superhero/United) | ❌ |
| — | divider | — | — |
| 5 | `#action-option-server-reboot` | 🔄 サーバを再起動 | ❌ |
| 6 | `#action-option-server-shutdown` | ⏻ サーバをシャットダウン | ✅ |

**Rust独自**:
- Language切替 (`日本語 ↔ English`) — Ruby版にはない

#### 2.1.7 キュー表示 (右ナビバー)

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| アイコン | `.glyphicon-inbox` | 📥 (Unicode) | ✅ |
| サイズバッジ | `.queue__sizes` (default + convert分割) | `#queue-count` 単一 | 🟡 |
| クリックでモーダル表示 | キューマネージャー (ドラッグ並替・取消) | 簡易キュー状態モーダル | 🟡 |
| ツールチップ | "クリックでキュー一覧を表示" | "キュー状態" | 🟡 |
| アクティブ状態 (赤色) | `.queue.active` + `.queue-plus` | 未対応 | ❌ |

#### 2.1.8 フィルター入力 (右ナビバー)

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 検索アイコン | `#myFilter-search-icon` (.glyphicon-search) | なし | ❌ |
| テキスト入力 | `#myFilter` (border-radius: 18px) | `#filter-input` | ✅ |
| クリアボタン | `#myFilter-clear` (.glyphicon-remove-circle) | `#filter-clear` (×) | ✅ |
| placeholder | "Filter" | "Filter" | ✅ |

---

### 2.2 コンソール

| 要素 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| コンテナ | `#console-container` | `#console-container` | ✅ |
| 表示エリア | `#console.console` (dark bg, 150px高) | `#console.console` | ✅ |
| キュー中断ボタン | `.queue-cancel` (.glyphicon-remove-circle) | なし | ❌ |
| 全履歴取得ボタン | `.console-history` (.glyphicon-cloud-download) | なし | ❌ |
| ゴミ箱ボタン | `.console-trash` (.glyphicon-trash) | `#console-trash` (🗑) | ✅ |
| 拡大/縮小ボタン | `.console-expand` (full/small切替) | `#console-expand` (⤢) | ✅ |
| デュアルコンソール | 並行モード時に左右分割 | なし | ❌ |

---

### 2.3 コントロールパネル

#### 2.3.1 ボタン一覧

| # | ボタン | サブメニュー | Ruby版 | Rust | 状態 |
|---|--------|-------------|--------|------|------|
| 1 | **Download** (primary/青) | ドロップダウン | "新規ダウンロード" + "強制再DL" | ボタンのみ | 🟡 |
| 2 | **Update** (success/緑) | ドロップダウン | "最新話掲載日確認" / "タグ指定更新" / "表示中を更新" / "凍結済みも更新" | "表示中更新" / "凍結済み更新" | 🟡 |
| 3 | **な** (success/緑) | — | なろうAPI確認 | あり | ✅ |
| 4 | **他** (success/緑) | — | その他の最新話確認 | あり | ✅ |
| 5 | **🔄** (success/緑) | — | modifiedタグ付き小説を更新 | なし | ❌ |
| 6 | **Send** (warning/橙) | ドロップダウン (条件付き) | "Send" + hotentry/栞バックアップ | ボタンのみ | 🟡 |
| 7 | **Freeze** (info/水色) | ドロップダウン | "凍結" / "凍結解除" | ドロップダウン | ✅ |
| 8 | **Remove** (danger/赤) | — | "選択した小説を削除" | あり | ✅ |
| 9 | **Convert** (default/白) | — | "選択した小説を変換" | あり | ✅ |
| 10 | **Other** (default/白) | ドロップダウン | "差分"/"調査"/"フォルダ"/"バックアップ"/"設定焼付"/"メール送信" | "差分"/"フォルダ"/"バックアップ"/"メール" | 🟡 |
| 11 | **Eject** (default/白, 隠し) | ドロップダウン | "端末取出し"/"今すぐ取出し" | なし | ❌ |

**Ruby版 Otherサブメニュー vs Rust版**:

| # | ラベル | Rust |
|---|--------|------|
| 1 | 選択した小説の最新の差分を表示 | ✅ |
| 2 | 選択した小説の調査状況ログを表示 | ❌ |
| 3 | 選択した小説の保存フォルダを開く | ✅ |
| 4 | 選択した小説のバックアップを作成 | ✅ |
| 5 | 選択した小説の設定の未設定項目に共通設定を焼付ける | ❌ |
| 6 | 選択した小説をメールで送信 | ✅ |

**Ruby版 Downloadサブメニュー**:

| # | ラベル | Rust |
|---|--------|------|
| 1 | 選択した小説を強制再ダウンロード | ❌ |

**Ruby版 Updateサブメニュー**:

| # | ラベル | Rust |
|---|--------|------|
| 1 | 最新話掲載日を確認 | ❌ |
| 2 | タグを指定して更新 | ❌ |
| 3 | 表示されている小説を更新 | ✅ |
| 4 | 凍結済みでも更新 | ✅ |

#### 2.3.2 enable-selected 制御

Ruby版では `.enable-selected` クラスのボタンは選択が0の時 `disabled` になる。
- Ruby: Freeze / Remove / Convert / Other / Eject / タグ編集
- Rust: Remove / Convert / Other のみ (Freezeは `enable-selected` 未付与)

---

### 2.4 小説リストテーブル

#### 2.4.1 カラム一覧

| # | Ruby版カラム | 説明 | Rust | 状態 |
|---|-------------|------|------|------|
| 1 | ID | 数値ID | あり | ✅ |
| 2 | 最終更新日 | 更新日 (色付き時間バッジ: 1h/6h/24h/3d/1w) | あり (バッジなし) | 🟡 |
| 3 | 最新話掲載日 | general_lastup | あり | ✅ |
| 4 | タイトル | クリックで設定ページ or ToCリンク | あり (クリック未対応) | 🟡 |
| 5 | 作者 | クリックでフィルタ | あり (クリック未対応) | 🟡 |
| 6 | サイト名 | クリックでフィルタ | あり (クリック未対応) | 🟡 |
| 7 | 掲載URL | ToC URL リンク | なし | ❌ |
| 8 | 話数 | 合計チャプター数 | なし | ❌ |
| 9 | 平均文字数 | 話あたりの平均文字数 | なし | ❌ |
| 10 | 状態 | 連載/完結/短編 等 | あり (簡易) | 🟡 |
| 11 | タグ | 色付きバッジ、クリックでフィルタ | あり (色未対応) | 🟡 |
| 12 | メニュー | ☰ コンテキストメニューボタン (モバイル用) | なし | ❌ |
| 13 | ダウンロード | ダウンロードボタン (モバイル用) | なし | ❌ |
| 14 | フォルダ | フォルダ開くボタン | なし | ❌ |
| 15 | 更新ボタン | 更新ボタン | なし | ❌ |

#### 2.4.2 行の状態表示

| 状態 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 選択行ハイライト | 黄色 box-shadow + ✓マーク | 黄色背景 | 🟡 |
| 凍結行 | 青色テキスト (#6caddd) + ＊マーク | frozen クラスのみ | 🟡 |
| 新着マーク | マゼンタ ● `.hint-new-arrival` | なし | ❌ |
| 更新時間バッジ | 1h(赤)/6h(緑)/24h(青)/3d(灰青)/1w(水色) | なし | ❌ |
| 奇数/偶数行色 | #f8f3e5 / #fffcef | CSS変数で指定 | ✅ |

#### 2.4.3 DataTables 機能 (Ruby版)

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| サーバーサイドソート | DataTables server-side | JS ソート (data-sort) | 🟡 |
| ページネーション | DataTables paging | なし | ❌ |
| 列の表示/非表示切替 | DataTables ColVis | なし | ❌ |
| 列ドラッグ並べ替え | — | なし | ❌ |

---

### 2.5 コンテキストメニュー (右クリック)

Ruby版は行を右クリックすると15項目のコンテキストメニューを表示する。
**Rust版: ❌ 未実装 (コンテキストメニュー機能なし)**

| # | ラベル | 動作 |
|---|--------|------|
| 1 | 変換設定 | `/novels/:id/setting` を開く |
| 2 | 差分を表示 | diff API呼び出し |
| 3 | タグを編集 | タグ編集モーダル |
| 4 | 凍結/凍結解除 | freeze toggle |
| 5 | 更新 | update API呼び出し |
| 6 | 強制更新 | update_force API呼び出し |
| 7 | 送信 | send API呼び出し |
| 8 | 削除 | remove (確認ダイアログ付き) |
| 9 | 変換 | convert API呼び出し |
| 10 | 調査 | inspect API呼び出し |
| 11 | フォルダを開く | folder API呼び出し |
| 12 | バックアップ | backup API呼び出し |
| 13 | 強制再ダウンロード | download_force API呼び出し |
| 14 | メールで送信 | mail API呼び出し |
| 15 | 前書き/後書き | `/novels/:id/author_comments` を開く |

---

### 2.6 範囲選択メニュー

Ruby版: `#rect-select-menu` — 範囲選択モードでドラッグ後に表示
**Rust版: ❌ 未実装**

| # | ID | ラベル |
|---|-----|--------|
| 1 | `#rect-select-menu-select` | 選択 |
| 2 | `#rect-select-menu-clear` | 解除 |
| 3 | `#rect-select-menu-reverse` | 反転 |
| 4 | `#rect-select-menu-cancel` | キャンセル |

---

### 2.7 タグ色選択メニュー

Ruby版: `#select-color-menu` — タグの色変更用コンテキストメニュー
**Rust版: ❌ 未実装**

| # | ID | 色 |
|---|-----|-----|
| 1 | `#select-color-menu-green` | Green |
| 2 | `#select-color-menu-yellow` | Yellow |
| 3 | `#select-color-menu-blue` | Blue |
| 4 | `#select-color-menu-magenta` | Magenta |
| 5 | `#select-color-menu-cyan` | Cyan |
| 6 | `#select-color-menu-red` | Red |
| 7 | `#select-color-menu-white` | White |

---

### 2.8 アラート・通知

| 種類 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| 初回アクセスウェルカム | `.alert-info` + ヘルプリンク | なし | ❌ |
| パフォーマンスモード警告 | `#performance-info.alert-info.hide` | なし | ❌ |
| 全表示モード警告 | `#show-all-warning.alert-warning.hide` | なし | ❌ |
| フェードアウトアラート | `.fadeout-alert` (fixed, z-1000) | なし | ❌ |

---

## 3. モーダルウィンドウ

### 3.1 キューマネージャーモーダル

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| モーダル表示 | `#queue-manager-modal` (100vw-20px, max 760px) | `#queue-modal` (簡易) | 🟡 |
| 実行中タスク表示 | タスク詳細 + 進捗 | なし | ❌ |
| 待機タスクリスト | ドラッグ&ドロップ並替 | なし | ❌ |
| タスクキャンセル | 個別取消ボタン | なし | ❌ |
| 未完了タスク復元 | 復元プロンプト (flexレイアウト) | なし | ❌ |
| ヒントテキスト | `.queue-manager__hint` (グレー) | なし | ❌ |
| ステータス表示 | pending/completed/failed | pending/completed/failed | ✅ |

### 3.2 タグ編集モーダル

**Rust版: ❌ 未実装 (タグ編集モーダルなし)**

Ruby版の機能:
- 既存タグ表示 (色付きバッジ)
- タグ追加 (テキスト入力 `#new-tag`, 300px幅)
- タグ削除 (×ボタン)
- タグ色変更 (色選択コンテキストメニュー)
- 選択した複数小説に一括適用

### 3.3 Aboutモーダル

**Rust版: ❌ 未実装**

Ruby版: `/about` パーシャルをモーダル表示
- バージョン情報
- 最新バージョンチェック (`/api/version/latest.json`)
- ライセンス情報

### 3.4 差分表示モーダル

**Rust版: ❌ 未実装**

Ruby版の機能:
- `.diff-list-container` でコミット一覧表示
- 各コミットをクリックで差分内容表示
- 日付・タイトル・サイズ情報

### 3.5 確認ダイアログ

**Rust版: ❌ 未実装 (browser confirm() のみ)**

Ruby版: bootbox.js ベースのカスタム確認ダイアログ
- `ping.modal` WebSocketイベントでサーバーからモーダル表示
- `modal.confirm` / `modal.choose` でサーバー応答を返す

### 3.6 メモ帳モーダル

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| モーダル表示 | あり (ポップアップ版) | `#notepad-modal` | ✅ |
| テキスト編集 | あり | `#notepad` textarea | ✅ |
| 保存 | POST `/api/notepad/save` | `#save-notepad-button` | ✅ |
| WebSocket 同期 | `notepad.change` イベント | なし | ❌ |
| 別ページ版 | `/notepad` (別ページ) | なし | ❌ |

---

## 4. キーボードショートカット

**Rust版: ❌ 全て未実装**

| キー | 動作 |
|------|------|
| `Ctrl+A` | 表示されている小説を選択 |
| `Shift+A` | 全ての小説を選択 (選択反転) |
| `Ctrl+Shift+A` | 選択を全て解除 |
| `ESC` | 選択解除 / フィルタクリア |
| `F5` | テーブルリフレッシュ |
| `W` | 小説リストの幅を広げる切替 |
| `F` | 凍結中を表示 |
| `Shift+F` | 凍結中以外を表示 |
| `S` | シングル選択モード |
| `R` | 範囲選択モード |
| `H` | ハイブリッド選択モード |
| `T` | タグ編集 |

---

## 5. テーマシステム

### 5.1 利用可能テーマ

| テーマ | ナビバー色 | キュー色 | Rust |
|--------|-----------|---------|------|
| **Cerulean** (デフォルト) | #2FA4E7 (青) | #2FA4E7 | ✅ (CSS変数) |
| **Darkly** | #375a7f (紺) | #375a7f | ❌ |
| **Readable** | #fff (白) | #4582ec | ❌ |
| **Slate** | #3A3F44 (暗灰) | — | ❌ |
| **Superhero** | #4E5D6C (鉄紺) | — | ❌ |
| **United** | #DD4814 (橙赤) | #DD4814 | ❌ |

### 5.2 テーマ切替

| 機能 | Ruby版 | Rust版 | 状態 |
|------|--------|--------|------|
| テーマ選択UI | ⚙メニューにテーマリスト | なし | ❌ |
| テーマ永続化 | `webui.theme` 設定値 | CSS変数でCerulean固定 | ❌ |
| ダークモード | Darkly/Slate/Superhero | dark variant in theme.css | 🟡 |

---

## 6. API エンドポイント

### 6.1 小説データ

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/list` | GET | 小説リスト取得 | ✅ |
| `/api/novels/count` | GET | 小説数取得 | ✅ |
| `/api/novels/all_ids` | GET | 全ID取得 | ❌ |
| `/api/sort_state` | GET | ソート状態取得 | ❌ |

### 6.2 ダウンロード・更新

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/download` | POST | 新規ダウンロード (URL) | ✅ |
| `/api/download_force` | POST | 強制再ダウンロード | ❌ |
| `/api/download_request` | POST | DLリクエスト (D&D用) | ❌ |
| `/api/update` | POST | 選択小説を更新 | ✅ |
| `/api/update_by_tag` | POST | タグ指定更新 | ❌ |
| `/api/update_general_lastup` | POST | 最新話掲載日確認 | ✅ |

### 6.3 変換・処理

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/convert` | POST | 変換 | ✅ |
| `/api/mail` | POST | メール送信 | ❌ |
| `/api/send` | POST | 端末送信 | ❌ |
| `/api/backup` | POST | バックアップ | ❌ |
| `/api/inspect` | POST | 調査 | ❌ |
| `/api/diff` | POST | 差分表示 | ❌ |
| `/api/diff_list` | GET | 差分リスト取得 | ❌ |
| `/api/diff_clean` | POST | 差分キャッシュ削除 | ❌ |
| `/api/folder` | POST | フォルダを開く | ❌ |
| `/api/backup_bookmark` | POST | 栞バックアップ | ❌ |

### 6.4 凍結・削除

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/freeze` | POST | 凍結 (toggle) | ✅ |
| `/api/freeze_on` | POST | 凍結 | ✅ |
| `/api/freeze_off` | POST | 凍結解除 | ✅ |
| `/api/remove` | POST | 削除 (データのみ) | ✅ |
| `/api/remove_with_file` | POST | 削除 (ファイルも) | ❌ |

### 6.5 タグ

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/tag_list` | GET | タグ一覧 | ✅ |
| `/api/taginfo.json` | GET | タグ情報 (色付き) | ❌ |
| `/api/edit_tag` | POST | タグ編集 | ❌ |
| `/api/change_tag_color` | POST | タグ色変更 | ❌ |

### 6.6 キュー

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/get_queue_size` | GET | キューサイズ | ✅ |
| `/api/get_pending_tasks` | GET | 待機タスク | ❌ |
| `/api/reorder_pending_tasks` | POST | タスク並替 | ❌ |
| `/api/remove_pending_task` | POST | タスク削除 | ❌ |
| `/api/restore_pending_tasks` | POST | タスク復元 | ❌ |
| `/api/cancel_running_task` | POST | 実行中タスク取消 | ❌ |
| `/api/cancel` | POST | キャンセル | ❌ |

### 6.7 ユーティリティ

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/api/history` | GET | コンソール履歴 | ❌ |
| `/api/clear_history` | POST | 履歴消去 | ❌ |
| `/api/story` | GET | あらすじ取得 | ❌ |
| `/api/csv/download` | GET | CSV エクスポート | ❌ |
| `/api/csv/import` | POST | CSV インポート | ❌ |
| `/api/notepad/read` | GET | メモ帳読取 | ✅ |
| `/api/notepad/save` | POST | メモ帳保存 | ✅ |
| `/api/version/current.json` | GET | 現在バージョン | ✅ |
| `/api/version/latest.json` | GET | 最新バージョン | ❌ |
| `/api/validate_url_regexp_list` | GET | URL正規表現一覧 | ❌ |

### 6.8 システム

| エンドポイント | メソッド | 説明 | Rust |
|-------------|--------|------|------|
| `/shutdown` | POST | シャットダウン | ✅ |
| `/reboot` | POST | 再起動 | ❌ |
| `/update_system` | POST | システム更新 | ❌ |
| `/check_already_update_system` | GET | 更新チェック | ❌ |
| `/api/eject` | POST | 端末取出し | ❌ |

---

## 7. WebSocket イベント

| イベント | 方向 | 説明 | Rust |
|---------|------|------|------|
| `echo` | S→C | コンソール出力 | ✅ |
| `table.reload` | S→C | テーブル再読込 | ❌ |
| `tag.updateCanvas` | S→C | タグキャンバス更新 | ❌ |
| `device.ejectable` | S→C | 端末取出し可能通知 | ❌ |
| `notification.queue` | S→C | キュー通知 (実行開始/完了) | ❌ |
| `notepad.change` | S→C | メモ帳変更同期 | ❌ |
| `error` | S→C | エラー通知 | ❌ |
| `ping.modal` | S→C | サーバーからモーダル表示 | ❌ |
| `modal.confirm` | C→S | 確認応答 | ❌ |
| `modal.choose` | C→S | 選択応答 | ❌ |
| `hide.modal` | S→C | モーダル非表示 | ❌ |

---

## 8. 設定ページ (`/settings`)

**Rust版: ❌ 全体未実装**

### 8.1 構成

| セクション | 説明 |
|-----------|------|
| グローバル設定 | `~/.narousetting/global_setting.yaml` の編集 |
| ローカル設定 | `.narou/local_setting.yaml` の編集 |

### 8.2 設定項目 (settingmessages.rb より)

各設定には以下の属性がある:
- ラベル (日本語名)
- 入力タイプ (text / boolean toggle / select)
- ヘルプテキスト (説明文)
- デフォルト値

主な設定カテゴリ:
- `convert.*` — 変換関連 (auto_join_line, enable_enchant_midashi, etc.)
- `device` — 出力端末 (kindle, kobo, etc.)
- `update.*` — 更新関連 (auto-schedule, interval)
- `server-*` — サーバ関連 (port, bind, basic-auth)
- `webui.*` — WebUI関連 (theme, performance-mode, table.reload-timing)
- `default.*` / `force.*` — デフォルト/強制設定
- `default_args.*` — デフォルト引数

### 8.3 UI部品

| 部品 | 説明 |
|------|------|
| テキスト入力 | 一般的な設定値 (float right, 150px幅) |
| トグルスイッチ | Boolean設定 (iOS/Android/Candy風) |
| セレクト | 選択肢 (device, theme等) |
| ヘルプ補助テキスト | `.help-extra-messages` (グレー、左ボーダー) |
| replace.txt テーブル | `#replace-txt-table` (250px input) |

---

## 9. 個別小説設定ページ (`/novels/:id/setting`)

**Rust版: ❌ 全体未実装**

Ruby版の機能:
- 44項目の変換設定 (NovelSettings)
- INI形式のオーバーレイ設定
- replace.txt ユーザー定義置換
- 保存→フラッシュメッセージ→リダイレクト

---

## 10. その他のページ

### 10.1 ヘルプページ (`/help`)
**Rust版: ❌ 未実装**
- 操作説明 (70%幅 / モバイル100%幅)
- スタイル付き見出し (赤ボーダー + 回転四角装飾)
- スクリーンショット付き

### 10.2 前書き/後書きビューア (`/novels/:id/author_comments`)
**Rust版: ❌ 未実装**
- 各話の前書き・後書きを表示
- ログボックス (270px高)

### 10.3 メモ帳ページ (`/notepad`)
**Rust版: ❌ 未実装** (モーダル版のみ)
- 別ページとしてのメモ帳UI

### 10.4 個別メニューエディター (`/edit_menu`)
**Rust版: ❌ 未実装**
- コンテキストメニューのカスタマイズ

### 10.5 再起動画面 (`/_rebooting`)
**Rust版: ❌ 未実装**
- 再起動中のローディング表示

---

## 11. CSS/スタイル要件

### 11.1 テーブルスタイル

| スタイル | Ruby版 | Rust版 | 状態 |
|---------|--------|--------|------|
| ヘッダー背景 | #605555 (濃茶灰) | CSS変数 | ✅ |
| 奇数行 | #f8f3e5 (クリーム) | CSS変数 | ✅ |
| 偶数行 | #fffcef (薄黄) | CSS変数 | ✅ |
| 選択行 | 黄色 box-shadow + ✓ | 黄色背景 | 🟡 |
| 凍結行 | 青文字 #6caddd + ＊ | frozen クラス | 🟡 |
| 新着マーク | マゼンタ ● | なし | ❌ |
| 更新時間バッジ | 1h/6h/24h/3d/1w 色分け | なし | ❌ |

### 11.2 コンソールスタイル

| スタイル | Ruby版 | Rust版 | 状態 |
|---------|--------|--------|------|
| 背景色 | #333 | CSS変数 | ✅ |
| テキスト色 | white | CSS変数 | ✅ |
| フォントサイズ | 13px (desktop) / 11px (mobile) | 相対単位 | ✅ |
| 高さ | 150px (desktop) / 100px (mobile) | 相対単位 | ✅ |
| 角丸 | 4px | 相対単位 | ✅ |
| プログレスバー | 80%幅 (desktop) / 100% (mobile) | なし | ❌ |

### 11.3 レスポンシブ対応

| ブレークポイント | Ruby版 | Rust版 | 状態 |
|----------------|--------|--------|------|
| ≤767px (モバイル) | ナビバー折畳、テーブル簡素化 | 48em / 30em | 🟡 |
| 768–1199px (タブレット) | 中間レイアウト | — | ❌ |
| ≥1200px (デスクトップ) | フルレイアウト | デフォルト | ✅ |

### 11.4 タッチデバイス最適化

Ruby版: `body.touch-device` クラスで大きめのタッチターゲット
**Rust版: ❌ 未実装**

---

## 12. JavaScript 機能

### 12.1 LocalStorage 管理

| キー | 説明 | Rust |
|------|------|------|
| `console` | コンソール履歴 | ❌ |
| `table_column_visible` | 列表示/非表示状態 | ❌ |
| `view_mode` | 表示モード (all/nonfrozen/frozen) | ❌ |
| `select_mode` | 選択モード (single/rect/hybrid) | ❌ |
| `novel_list_wide` | リスト幅拡大フラグ | ❌ |
| `buttons_placement` | ボタン配置 (top/footer) | ❌ |
| `setting_open_new_tab` | 設定を新タブで開くか | ❌ |
| `lang` | 言語 (ja/en) | ✅ |

### 12.2 通知システム

Ruby版: bootbox.js ベースのリッチ通知
- `.fadeout-alert` — 画面右上にフェードアウト通知
- `notification.queue` WebSocketイベントで実行完了通知
**Rust版: ❌ 未実装**

### 12.3 Drag & Drop

Ruby版:
- `#link-drop-here` — URL D&D でダウンロード開始
- `#csv-drop-here` — CSV D&D でインポート
**Rust版: ❌ 未実装**

---

## 13. 実装状況サマリ

### ページ単位

| カテゴリ | 合計 | ✅ | 🟡 | ❌ |
|---------|------|-----|-----|-----|
| ページ | 10 | 0 | 1 | 9 |
| メインページ要素 | ~60 | ~12 | ~15 | ~33 |
| モーダル | 6 | 1 | 1 | 4 |
| API エンドポイント | ~45 | ~15 | 0 | ~30 |
| WebSocket イベント | 11 | 1 | 0 | 10 |
| キーボードショートカット | 12 | 0 | 0 | 12 |
| テーマ | 6 | 1 | 0 | 5 |

### 優先実装順序 (推奨)

1. **キーボードショートカット** — 簡単に追加可能、UX向上大
2. **テーブルカラム拡充** — 掲載URL・話数・更新時間バッジ
3. **コンテキストメニュー** — 右クリック操作、UX向上大
4. **タグ編集モーダル** — 色付きタグ・色変更
5. **設定ページ** — `/settings` の全機能
6. **View/Updateサブメニュー補完** — 不足メニュー項目
7. **キューマネージャー強化** — ドラッグ並替・タスク詳細
8. **テーマ切替** — 6テーマ対応
9. **差分表示** — diff API + モーダル
10. **個別小説設定** — `/novels/:id/setting`
11. **About/Help/その他ページ** — 静的ページ群
