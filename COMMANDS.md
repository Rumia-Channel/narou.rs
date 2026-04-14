# narou.rs コマンド互換性ドキュメント

narou.rb 全24コマンドのオプション・挙動と、Rust 側の実装状況・要件を整理する。

---

## 互換性判定方針

- 互換性の対象は、CLI/API の引数・出力・終了コード・エラー、設定/YAML/.narou 配下データ構造、最終生成ファイルなど、外部から観測できる挙動である。
- Rust 側の内部ライブラリ、データ構造、処理順序、アルゴリズムは Ruby 版と同一である必要はない。外部挙動とデータ互換を維持できるなら、Ruby の逐語的移植よりも保守しやすく、安全で、検証しやすい実装を優先する。
- `COMMANDS.md` の ✅/🟡/❌ は Ruby 版の内部手順との一致ではなく、外部挙動の一致度で判定する。ただし外部挙動を把握するため、Ruby 版 `sample/narou/lib/command/*.rb` と周辺実装の細かい確認は必須とする。

---

## 目次

1. [互換性判定方針](#互換性判定方針)
2. [グローバルオプション](#グローバルオプション)
3. [コマンドショートカット](#コマンドショートカット)
4. [実装状況サマリ](#実装状況サマリ)
5. [各コマンドの詳細仕様](#各コマンドの詳細仕様)
6. [実装優先度](#実装優先度)

---

## グローバルオプション

全コマンドの前に処理される。`ARGV` から削除されてからコマンドに渡される。

| オプション | 説明 | Rust 実装 |
|-----------|------|:---------:|
| `--no-color` | カラー表示を無効にする（`global_setting.yaml` の `no-color` も反映） | ✅ |
| `--multiple` | 引数区切りに `,` も使えるようにする（`multiple-delimiter` 設定対応） | ✅ |
| `--time` | 実行時間を表示する | ✅ |
| `--backtrace` | エラー時に詳細バックトレースを表示 | ✅ |
| `--user-agent <UA>` | カスタム User-Agent | ✅ |

**補足**:
- `default_args.<command>` 設定でコマンドごとのデフォルト引数を `local_setting.yaml` に定義可能。CLIフラグがこれを上書きする。 ✅ 実装済
- `-v` / `--version` は `version` コマンドに変換される。`version --more` も受け付ける。 ✅
- `-h` / `--help` は clap ヘルプを表示。 ✅
- 引数なしは `help` コマンドにフォールバック。 ✅

---

## コマンドショートカット

narou.rb はコマンド名の先頭1文字または2文字でコマンドを一意に特定できる。 🟡 部分実装

**注意**: ショートカット解決テーブル自体は Ruby版と同じ順序で構築しているが、Rust 側 `Commands` enum に未実装コマンド（`version`, `diff`, `send` 等）が存在しないため、それらへ解決された後に clap の未認識サブコマンドエラーになる。全24コマンドが enum と実処理に揃うまでは完了扱いにしない。

| 1文字 | 2文字 | コマンド |
|:-----:|:-----:|---------|
| `d` | `do` | download |
| `u` | `up` | update |
| `l` | `li` | list |
| `c` | `co` | convert |
| `di` | | diff |
| `se` | | setting |
| `al` | | alias |
| `in` | | inspect |
| `se` | | send (settingと衝突。`se`→settingが優先) |
| `fo` | | folder |
| `br` | | browser |
| `r` | `re` | remove |
| `f` | `fr` | freeze |
| `t` | `ta` | tag |
| `w` | `we` | web |
| `ma` | | mail |
| `ba` | | backup |
| `cs` | | csv |
| `cl` | | clean |
| `lo` | | log |
| `tr` | | trace |
| `h` | `he` | help |
| `v` | `ve` | version |
| | | init (`i`はinspectが優先) |

---

## 実装状況サマリ

| コマンド | narou.rb | Rust 完了度 | 備考 |
|---------|:--------:|:-----------:|------|
| `init` | ✅ | ✅ 完了 | AozoraEpub3 設定含め完全 |
| `download` | ✅ | 🟡 部分 | `--mail` を追加。メール設定の自動作成と送信は実装済みだが、全互換確認は継続中 |
| `update` | ✅ | 🟡 部分 | Ruby版ターゲット解決、freeze.yaml参照、完結タグ同期、`--gl`主要挙動、`update.strong` 相当の同日本文比較、digest選択肢、差分cache退避、hotentryのcopy/send/mailまでは実装済み。周辺出力/イベント細部が残る |
| `convert` | ✅ | 🟡 部分 | `--device`, `--no-epub`, `--output` 等不足 |
| `list` | ✅ | 🟡 部分 | `--latest`, `--reverse`, `--url`, `--filter` 等不足 |
| `tag` | ✅ | 🟡 部分 | `--color`, `--clear`, `--list` 不足 |
| `freeze` | ✅ | 🟡 部分 | 全オプションは実装済み。freeze.yaml と `frozen` タグの同期は実装済みだが、ターゲット解決のRuby完全互換は未確認 |
| `remove` | ✅ | 🟡 部分 | `--yes`, `--with-file` 不足 |
| `web` | ✅ | 🟡 部分 | APIのみ。HTML UIなし |
| `setting` | ✅ | 🟡 部分 | 基本読み書きは実装済み。ただし default/force/default_args 系と全設定網羅に不足 |
| `diff` | ✅ | ❌ 未実装 | |
| `send` | ✅ | ❌ 未実装 | USB 経由端末送信 |
| `mail` | ✅ | 🟡 部分 | `mail_setting.yaml` 読込と SMTP 送信の基盤を追加。Pony/mail 設定の完全互換は要確認だが、hotentry 自動メールは実装済み |
| `backup` | ✅ | ✅ 完了 | `narou backup`/複数 target、`backup/` 除外、180バイト切り詰めまで対応 |
| `clean` | ✅ | ❌ 未実装 | |
| `help` | ✅ | 🟡 部分 | トップレベルは概ね実装済み。各コマンド -h は Ruby版詳細ヘルプとの差分あり |
| `version` | ✅ | ✅ 完了 | `-v`/`--version` と `--more` を実装。出力順序、help 文言、AozoraEpub3 探索、失敗時メッセージを Ruby 版に揃えた |
| `log` | ✅ | ✅ 完了 | `--num`, `--tail`, `--source-convert`, `<path>` を実装。最新ログ選択、`.narou/local_setting.yaml` の `log.*` 既定値、`*_convert` フィルタも対応 |
| `folder` | ✅ | ❌ 未実装 | |
| `browser` | ✅ | ❌ 未実装 | |
| `alias` | ✅ | ❌ 未実装 | |
| `inspect` | ✅ | ❌ 未実装 | |
| `csv` | ✅ | ❌ 未実装 | |
| `trace` | ✅ | ✅ 完了 | `trace_dump.txt` を表示。panic 時に保存されたバックトレースを読む |

---

## 各コマンドの詳細仕様

### 1. `init` — ✅ 完了

> 現在のフォルダを小説用に初期化します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--path` | `-p` | string | — | AozoraEpub3 フォルダ指定。`:keep` で既存再利用 |
| `--line-height` | `-l` | float | 1.8 | 行の高さ (em) |

**Rust 実装**: `src/commands/init.rs` (396行)。完全実装済み。

---

### 2. `download` — 🟡 部分

> 指定した小説をダウンロードします

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--force` | `-f` | flag | false | 全話強制再DL | ✅ |
| `--no-convert` | `-n` | flag | false | DLのみ、変換スキップ | ✅ |
| `--freeze` | `-z` | flag | false | DL後に凍結 | ✅ |
| `--remove` | `-r` | flag | false | DL後に削除(変換+送信のみ) | ✅ |
| `--mail` | `-m` | flag | false | DL後にメール送信 | ✅ |
| targets | | Vec\<String\> | — | URL/Nコード/ID/タイトル | ✅ |

**実装済み動作**:
- `-f`/`--force`: 全話強制再ダウンロード (`Downloader::download_novel_with_force`)
- `-n`/`--no-convert`: 変換スキップ
- `-z`/`--freeze`: DL完了後に自動凍結 (`--freeze` は `--remove` より優先)
- `-r`/`--remove`: DL+変換後にDBから削除（ファイルは残る）
- 引数なしでインタラクティブモード (stdin から URL 入力、TTY時のみ)
- 凍結チェック: 凍結済み小説はスキップ
- ダウンロード済みチェック: 既存小説はスキップ（`--force`で上書き）
- タグ展開: `tag:NAME` → 該当IDに展開、`^tag:NAME` → 補集合
- `tagname_to_ids`: ID優先、未登録はタグ名として展開
- `mistook_count` 追跡 → 終了コード反映
- 複数ターゲット間の水平線セパレータ
- 有効ターゲット検証: Nコード or URL(サイト設定マッチ)
- Nコード指定時は `https://ncode.syosetu.com/<ncode>/` からURLキャプチャを作り、サイト定義の `\k<ncode>` を展開してDLする

**不足動作**:
- `--mail`: 変換済み電子書籍を `mail_setting.yaml` に従って送信
- 再ダウンロード確認プロンプト (Ruby: `Narou::Input.confirm("再ダウンロードしますか")`)

---

### 3. `update` — 🟡 部分

> 小説を更新します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--no-convert` | `-n` | flag | false | 変換スキップ | ✅ |
| `--convert-only-new-arrival` | `-a` | flag | false | 新着がある場合のみ変換 | ✅ |
| `--gl [OPT]` | — | string | — | general_lastup 更新。OPT: なし=全, `narou`, `other` | ✅ |
| `--force` | `-f` | flag | false | 凍結小説も更新 | ✅ |
| `--sort-by KEY` | `-s` | string | — | 更新順ソート | ✅ |
| `--ignore-all` | `-i` | flag | false | 引数なし時の全更新を無効化 | ✅ |
| ids | | Vec\<String\> | — | ID/URL/Nコード/タイトル/別名/tag:NAME/タグ名 | ✅ |

**実装済み動作**:
- `-n`/`--no-convert`: 変換スキップ
- `-a`/`--convert-only-new-arrival`: 新着がある場合のみ変換（設定 `update.convert-only-new-arrival` にも対応）
- `--gl [OPT]`: なろう API バッチで `general_lastup` を更新し、変更のあった小説に `modified` タグを付与。OPT省略=全、`narou`=なろうAPI対応のみ、`other`=非なろうのみ
- `-f`/`--force`: 凍結小説も更新
- `-s`/`--sort-by KEY`: 更新順ソート（設定 `update.sort-by` にも対応）。有効キー: `id`, `last_update`, `title`, `author`, `new_arrivals_date`, `general_lastup`
- `-i`/`--ignore-all`: 引数なし時の全更新を無効化
- 標準入力からのターゲット読み取りに対応。Ruby版同様 `narou tag ... | narou u` や `narou l -t "foo bar" | narou u` のようなパイプ入力を解決
- ターゲット解決: Ruby版 `tagname_to_ids`/`Downloader.get_data_by_target` 相当に合わせ、ID、URL、Nコード、タイトル、`.narou/alias.yaml` 別名、通常タグ名、`tag:NAME`、`^tag:NAME` を解決
- 既存小説更新時は Ruby版同様に DB の `toc_url` から `ncode` などのURLキャプチャを復元し、DB上の `sitename` を保存先決定で優先する
- あらすじ比較は `<br>`/`<br/>`/`<br />` と改行・行末空白を正規化し、実質同一なら更新扱いにしない
- 凍結チェック: Ruby版と同じ `.narou/freeze.yaml` を参照（既存Rustデータ移行用に `frozen` タグも補助的に認識）
- `modified` タグ管理: 更新成功時に自動削除、`--gl` で変更検出時に自動付与
- `end` タグ管理: 更新・`--gl other` で完結状態に合わせて `end` タグを同期
- `_convert_failure` フラグ: 変換失敗時に記録、次回更新で再変換を試行
- `update.interval` 設定対応（最低2.5秒、YAMLの数値/文字列を許容）
- `update.strong` 設定対応。同日更新時は保存済み `本文/*.yaml` の本文要素と取得本文をハッシュ比較し、実質同一なら更新扱いにしない
- `update.convert-only-new-arrival` 設定対応（YAMLの真偽値/文字列/数値を許容）
- `last_check_date` 追跡
- `download.choices-of-digest-options` 設定対応。Ruby版と同じ 1-8 のダイジェスト化選択肢を処理し、キャンセル・凍結・バックアップ・あらすじ表示・ブラウザ起動・保存フォルダ起動・変換を実行
- ダイジェスト化キャンセル時は `UpdateStatus::Canceled` を返し、`update` / `download` コマンド側でRuby版相当のキャンセル表示と終了コード加算を行う
- 差分更新時は Ruby版同様 `本文/cache/<timestamp>/` に旧sectionを退避し、差分が無い場合は空cacheディレクトリを削除
- `SuspendDownload` 発生時は通常失敗ではなくバッチ全体の中断として扱うように修正
- `auto-add-tags` 設定対応。site YAML の `tags` パターンから取得したタグをDBタグへ自動追加
- `hotentry` / `hotentry.auto-mail` 設定のうち、hotentry の新着話収集・統合テキスト生成・device に応じた変換・`copy-to`・端末送信までは実装済み
- ソートキーバリデーション（不正キーでエラー+終了コード127）
- `setting update.sort-by` の select 値を Ruby版 `Narou::UPDATE_SORT_KEYS` と同期済み
- 小説間インターバル（Ruby版 `Interval` クラス互換）
- 全件更新時の凍結スキップ、個別指定時の凍結メッセージ
- 終了コード: エラー数（最大127）、中断時126
- `--all` は Ruby版に存在しないRust独自オプションだったため削除

**完了扱いにしない理由 / 不足動作**:
- `mail hotentry` 連携（hotentry.auto-mail 含む）
- `confirm_over18?` の global_setting 永続化は未実装で、現状は都度確認のみ
- Ruby版の section hash cache 永続化との完全な外部互換は未確認
- Ruby版の詳細表示・hotentry後処理・割り込み時Worker cancelなど、周辺出力/イベント処理の細部は追加突合が必要

---

### 4. `convert` — 🟡 部分

> 小説を変換します。管理小説以外にテキストファイルも変換可能

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--output FILE` | `-o` | string | — | 出力ファイル名指定 | ❌ |
| `--make-zip` | — | flag | false | i文庫 ZIP 作成 | ❌ |
| `--enc ENCODING` | `-e` | string | UTF-8 | テキストファイル文字コード | ❌ |
| `--no-epub` | — | flag | false | EPUB 生成スキップ | ❌ |
| `--no-mobi` | — | flag | false | MOBI 生成スキップ | ❌ |
| `--no-strip` | — | flag | false | MOBI ストリップスキップ | ❌ |
| `--no-zip` | — | flag | false | ZIP 作成スキップ | ❌ |
| `--no-open` | — | flag | false | 出力フォルダを開かない | ❌ |
| `--inspect` | `-i` | flag | false | 小説状態調査ログ表示 | ❌ |
| `--verbose` | `-v` | flag | false | AozoraEpub3/kindlegen 標準出力表示 | ❌ |
| `--ignore-default` | — | flag | false | default.* 設定を無視 | ❌ |
| `--ignore-force` | — | flag | false | force.* 設定を無視 | ❌ |
| targets | | Vec\<String\> | — | ID/タイトル/ファイルパス | ✅ |

**不足動作**:
- テキストファイルの直接変換 (DBにないファイルパス指定)
- `convert.copy-to` への自動コピー
- `convert.multi-device` による複数端末同時変換
- EPUB→MOBI 変換パイプライン (AozoraEpub3 + kindlegen)
- i文庫 ZIP 作成
- 変換後の端末送信
- `dc:subject` へのタグ埋め込み
- ThreadPool による並列変換

**注**: EPUB/MOBI 生成は AozoraEpub3.jar と kindlegen への依存がある。Rust 側のテキスト変換 (`novel.txt` 生成) は完了しているが、AozoraEpub3 の呼び出しパイプラインは別途必要。

---

### 5. `list` — 🟡 部分

> 現在管理している小説の一覧を表示します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--latest` | `-l` | flag | false | 最新更新順でソート | ❌ |
| `--gl` | — | flag | false | general_lastup でソート | ❌ |
| `--reverse` | `-r` | flag | false | 逆順ソート | ❌ |
| `--url` | `-u` | flag | false | URL 表示 | ❌ |
| `--kind` | `-k` | flag | false | 小説種別表示 (短編/連載) | ❌ |
| `--site` | `-s` | flag | false | サイト名表示 | ❌ |
| `--author` | `-a` | flag | false | 作者名表示 | ❌ |
| `--filter VAL` | `-f` | string | — | フィルタ: `series`/`ss`/`frozen`/`nonfrozen` | ❌ |
| `--grep VAL` | `-g` | string | — | テキスト検索。`-` prefix で NOT | ❌ |
| `--tag [TAGS]` | `-t` | string | — | タグ表示/フィルタ | ✅ (部分) |
| `--echo` | `-e` | flag | false | パイプ時も人間可読出力 | ❌ |
| `--frozen` | — | flag | false | 凍結済みのみ | ✅ |
| limit | | int | — | 表示数上限 | ❌ |

**不足動作**:
- パイプ接続時はスペース区切りID一覧を出力 (他コマンドへのチェーン用)
- 6時間以内更新の小説をハイライト
- 列のカスタマイズ (`--url`, `--kind`, `--site`, `--author`)

---

### 6. `setting` — 🟡 部分

> 各コマンドの設定を変更します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--list` | `-l` | flag | false | 現在の設定一覧表示 |
| `--all` | `-a` | flag | false | 全設定可能変数を表示 |
| `--burn` | — | flag | false | 共通設定を小説別 setting.ini に焼き込み |

**使用形式**:
```
narou setting name=value   # 設定
narou setting name=        # 削除
narou setting name         # 読み取り
```

**設定スコープ**:
- `local_setting.yaml` (プロジェクトローカル)
- `~/.narousetting/global_setting.yaml` (グローバル)
- `default.*` = 未設定小説のデフォルト値
- `force.*` = 全小説の強制上書き値

**主要 local_setting 項目**:

| 設定名 | 型 | 説明 |
|-------|-----|------|
| `device` | select | 対象端末 (kindle/kobo/等) |
| `hotentry` | boolean | hotentry 自動生成 |
| `concurrency` | boolean | 並列DL+変換 |
| `logging` | boolean | ログ保存 |
| `update.interval` | float | 小説間ウェイト (秒、最小2.5) |
| `update.strong` | boolean | 同日更新時の内容チェック |
| `update.convert-only-new-arrival` | boolean | 新着時のみ変換 |
| `update.sort-by` | select | 更新順ソートキー |
| `update.auto-schedule.enable` | boolean | 自動更新スケジューラ有効 |
| `update.auto-schedule` | string | スケジュール時刻 (HHMM, カンマ区切り) |
| `convert.copy-to` | directory | 変換ファイルのコピー先 |
| `convert.copy-zip-to` | directory | ZIP ファイルのコピー先 |
| `convert.copy-to-grouping` | multiple | コピー先のグルーピング |
| `convert.no-open` | boolean | 変換後にフォルダを開かない |
| `convert.inspect` | boolean | 常に調査ログ表示 |
| `convert.multi-device` | multiple | 複数端末同時変換 |
| `convert.filename-to-ncode` | boolean | 出力ファイル名にNコード使用 |
| `convert.make-zip` | boolean | i文庫 ZIP 作成 |
| `download.interval` | float | 話間DL ウェイト (秒) |
| `download.wait-steps` | integer | N話ごとに長待機 |
| `download.use-subdirectory` | boolean | サブディレクトリ使用 |
| `send.without-freeze` | boolean | 送信時に凍結除外 |
| `economy` | multiple | 省容量設定 (cleanup_temp/send_delete/nosave_diff/nosave_raw) |
| `guard-spoiler` | boolean | DL時の話名非表示 |
| `auto-add-tags` | boolean | サイトタグ自動追加 |
| `user-agent` | string | カスタム User-Agent |
| `webui.theme` | select | WebUI テーマ |

**主要 global_setting 項目**:

| 設定名 | 型 | 説明 |
|-------|-----|------|
| `aozoraepub3dir` | directory | AozoraEpub3 の場所 |
| `line-height` | float | 行の高さ |
| `difftool` | string | 外部 diff ツールパス |
| `difftool.arg` | string | diff ツール引数 (%OLD, %NEW) |
| `no-color` | boolean | カラー表示無効 |
| `server-port` | integer | Web サーバポート (+1 は WebSocket) |
| `server-bind` | string | Web サーババインドアドレス |
| `server-basic-auth.*` | bool/str | Basic 認証設定 |
| `over18` | boolean | 18+ フラグ |

**実装済み (Rust)**:
- `local_setting.yaml` / `global_setting.yaml` の読み書き (`Inventory`)
- 設定値のバリデーション (型チェック、選択肢チェック)
- `--list` 現在値一覧表示
- `--all` 全変数表示
- `--burn` による setting.ini への焼き込み
- `name` 読み取り、`name=value` 設定、`name=` 削除
- 不明変数名のエラー、古い変数の掃除削除
- エラー数を終了コードとして返す
- `apply_force_and_default_settings` で変換時の `force.*/default.*` をフラットキーから正しく解決

**完了扱いにしない理由 / 不足動作**:
- `narou setting default.*=...` / `force.*=...` / `default_args.*=...` の設定・読み取り・削除が、コマンド側のスコープ判定と完全には接続されていない。`cast_value` には一部対応があるが、Ruby版互換の外部挙動として要修正。
- Ruby版 `SETTING_VARIABLES` との全項目突合が未完。`device` の選択肢や大文字小文字、隠し項目、型、select/multiple 値に差分が残る可能性がある。
- Ruby版の `device` 変更時の関連設定自動変更（`modify_settings_when_device_changed` / device hook）が未実装。
- `setting -h` は Ruby版の `Local Variable List` / `Global Variable List` を help 内に表示するが、Rust版 help は省略している。

---

### 7. `freeze` — 🟡 部分

> 小説の凍結設定を行います

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--list` | `-l` | flag | false | 凍結小説一覧 | ✅ |
| `--on` | — | flag | false | 強制凍結 | ✅ |
| `--off` | — | flag | false | 強制解除 | ✅ |
| targets | | Vec\<String\> | — | 対象小説 | ✅ |

**実装済み動作**:
- デフォルトはトグル動作 (凍結→解除、未凍結→凍結)
- `--on` で強制凍結
- `--off` で強制解除
- `--list` / `-l` で凍結済み一覧表示 (`list --frozen` に委譲)
- 解除時に `404` タグも削除
- 引数なし時はヘルプ表示
- タイトル付きメッセージ: `タイトル を凍結しました` / `タイトル の凍結を解除しました`

**完了扱いにしない理由 / 不足動作**:
- Ruby版は `.narou/freeze.yaml` を凍結状態の保存先にするが、Rust版は `NovelRecord.tags` の `frozen` タグで管理しており、ファイル互換が崩れている。
- Ruby版は `tagname_to_ids` と `Downloader.get_data_by_target` 経由で ID/Nコード/URL/タイトル/別名/タグ名を解決する。Rust版は主に ID/タイトルのみで、タグ展開や別名解決が未完。
- `--list` も Ruby版は `List.execute!("--filter", "frozen")` 相当で freeze 状態を見るが、Rust版は tag ベースの `list --frozen` に依存している。

---

### 8. `tag` — 🟡 部分

> 各小説にタグを設定及び閲覧が出来ます

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--add TAGS` | `-a` | string | — | タグ追加 (スペース区切り) | ✅ |
| `--delete TAGS` | `-d` | string | — | タグ削除 | ✅ (--remove) |
| `--color COL` | `-c` | string | auto | タグ色設定 | ❌ |
| `--clear` | — | flag | false | 全タグクリア | ❌ |
| `--list` | `-l` | flag | false | タグ一覧表示 | ❌ |
| targets | | Vec\<String\> | — | 対象小説 | ✅ |

**不足動作**:
- 引数なし = タグ一覧表示
- タグ指定のみ(モードなし) = タグ検索 (`list --tag` に委譲)
- 禁止文字: `:;"'><$@&^\\\|%/`` と `hotentry`
- 色の自動ローテーション (green/yellow/blue/magenta/cyan/red/white)
- 特殊タグ: `end` (完結), `404` (削除済み)

---

### 9. `remove` — 🟡 部分

> 小説を削除します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--yes` | `-y` | flag | false | 確認スキップ | ❌ |
| `--with-file` | `-w` | flag | false | ファイルも削除 | ❌ (常時削除) |
| `--all-ss` | — | flag | false | 全短編小説を対象 | ❌ |
| targets | | Vec\<String\> | — | 対象小説 | ✅ |

**不足動作**:
- デフォルトはDB indexのみ削除 (ファイル残す)
- `--with-file` でファイル含め完全削除
- インタラクティブ確認プロンプト
- 凍結中は削除不可
- `--all-ss` で `novel_type == 2` を一括選択
- 変換中ロックチェック

---

### 10. `diff` — ❌ 未実装

> 更新された小説の差分を表示します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--number NUM` | `-n` | int | 1 | 差分番号 (1=最新) |
| `--list` | `-l` | flag | false | 差分一覧表示 |
| `--clean` | `-c` | flag | false | 指定小説の差分全削除 |
| `--all-clean` | — | flag | false | 凍結以外の全差分削除 |
| `--no-tool` | — | flag | false | 外部 diff ツールを使わない |
| `-N` (数値) | — | int | — | `-n N` の短縮形 |
| target | | string | — | 小説指定 (省略時=最終更新) |

**実装要件**:
- raw/ ディレクトリ内の過去セクションの差分管理
- セクション YAML から一時テキスト生成 → 差分表示
- 外部 diff ツール統合 (`difftool` / `difftool.arg` 設定)
- 内蔵差分ビューア (カラー付き)
- 差分バージョン指定: `2023.02.21@01.39.46` 形式

---

### 11. `send` — ❌ 未実装

> 変換したEPUB/MOBIを電子書籍端末に送信します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--without-freeze` | `-w` | flag | false | 凍結小説を除外 |
| `--force` | `-f` | flag | false | タイムスタンプ無視で強制送信 |
| `--backup-bookmark` | `-b` | flag | false | ブックマークバックアップ (KindlePW) |
| `--restore-bookmark` | `-r` | flag | false | ブックマーク復元 |
| device | | string | — | 端末名 (kindle/kobo/等)。省略時=設定値 |
| targets | | Vec\<string\> | — | 対象。省略時=全小説 |

**実装要件**:
- USB マスストレージ経由で端末の documents/ へコピー
- `last_mail_date` による差分送信
- `convert.copy-to` 設定との連動
- 端末別のファイル配置規則
- hotentry 対応

---

### 12. `mail` — 🟡 部分

> 変換したEPUB/MOBIをメールで送信します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--force` | `-f` | flag | false | 全非凍結小説を強制送信 |
| targets | | Vec\<string\> | — | 対象。`hotentry` 指定可 |

**実装要件**:
- SMTP 設定 (`mail_setting.yaml`)
- Send-to-Kindle 対応
- `last_mail_date` による差分送信
- hotentry 自動メール (`hotentry.auto-mail` 設定)

---

### 13. `web` — 🟡 部分

> WEBアプリケーション用サーバを起動します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--port PORT` | `-p` | int | 3000 | サーバポート | ✅ |
| `--no-browser` | `-n` | flag | false | ブラウザ自動起動抑制 | ✅ |

**不足動作**:
- WebSocket ポート = HTTP ポート + 1
- 初回起動時のファイアウォール警告
- 自動更新スケジューラの起動
- Basic 認証 (`server-basic-auth` 設定)
- `server-bind` 設定対応
- HTML フロントエンド (Ruby 版は HAML テンプレート)
- `webui.theme` 設定
- `webui.performance-mode` 設定

---

### 14. `backup` — ✅ 完了

> 小説のバックアップを作成します

オプションなし。

**Rust 実装**: `src/commands/backup.rs` で Ruby 版同様に複数 target を順に処理し、`backup/` 直下へ ZIP を保存する。バックアップ名はタイトルを Ruby 版同様に整形して 180 バイトで切り詰める。

---

### 15. `clean` — ❌ 未実装

> ゴミファイルを削除します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--force` | `-f` | flag | false | 実際に削除 |
| `--dry-run` | `-n` | flag | false | 表示のみ |
| `--all` | `-a` | flag | false | 全小説を対象 |
| target | | string | — | 小説指定 (省略時=最終変換) |

**実装要件**:
- orphaned な `raw_data/*.txt` と `section_save/*.yaml` の検出
- TOC の subtitles に含まれないファイルを特定
- 凍結小説は `--all` でもスキップ
- `--force` なしの場合はリストのみ

---

### 16. `help` — 🟡 部分

> このヘルプを表示します

**実装** (`src/commands/help.rs`):
- 未初期化時: `narou init` を促すメッセージ（`.narou/` ディレクトリ存在チェック）
- 初期化済み: 全24コマンド一覧 + oneline_help（narou.rb と同一順序・同一テキスト）
- グローバルオプション表示（`--no-color`, `--multiple`, `--time`, `--backtrace`）
- ショートカット説明（`d`, `fr` 等の例示付き）
- `NO_COLOR` 環境変数対応（ANSIエスケープコード条件付き出力）
- 引数なし（`narou`）およびショートカット（`h`, `he`）からのフォールバック対応

**完了扱いにしない理由 / 不足動作**:
- `help` は未実装コマンド分も narou.rb から移植する方針だが、`narou <command> -h` の詳細文・Examples・Configuration・Variable List が Ruby版 `sample/narou/lib/command/*.rb` と完全一致していない。
- `setting -h` の `Local Variable List` / `Global Variable List` が省略されている。
- `list`, `folder`, `browser`, `inspect`, `clean`, `diff`, `send`, `mail`, `alias`, `csv` などで banner、説明文、Examples、Options の省略・改変・追加がある。
- `tag -h` は Ruby版にない `--list` を表示しており、Ruby版 help 互換として要整理。

---

### 17. `version` — ✅ 完了

> バージョンを表示します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--more` | `-m` | flag | false | Java/AozoraEpub3 バージョンも表示 |

**Rust 実装**: `src/commands/version.rs` で `narou -v` / `narou version` に対応。`--more` も受け付ける。help 文言と `--more` の出力は Ruby 版に合わせた。

---

### 18. `log` — ✅ 完了

> 保存したログを表示します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--num NUM` | `-n` | int | 20 | 表示行数 |
| `--tail` | `-t` | flag | false | ストリーミング (`tail -f` 相当) |
| `--source-convert` | `-c` | flag | false | 変換ログを表示 |
| `<path>` | | string | — | ログファイルパス直接指定可 |

**Rust 実装**: `src/commands/log.rs` で `narou log` / `-n` / `-t` / `-c` / `<path>` に対応。最新ログは `log/*.txt` を更新日時順で選択し、`.narou/local_setting.yaml` の `log.num` / `log.tail` / `log.source-convert` も既定値として反映。`-c` は Ruby版同様 `*_convert` ログだけを対象にする。

---

### 19. `folder` — ❌ 未実装

> 小説の保存フォルダを開きます

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--no-open` | `-n` | flag | false | パス表示のみ |
| target | | string | — | 小説指定 |

**実装要件**:
- ファイルマネージャで小説ディレクトリを開く (`open::that` または同等)
- パスを常に stdout に出力

---

### 20. `browser` — ❌ 未実装

> 小説の掲載ページをブラウザで開きます

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--vote` | `-v` | flag | false | 感想ページを開く (なろうのみ) |
| target | | string | — | 小説指定 |

**実装要件**:
- デフォルトブラウザで `toc_url` を開く
- `--vote` は最新話の感想ページ (`#my_novelpoint`) を開く

---

### 21. `alias` — ❌ 未実装

> 小説のIDに紐付けた別名を作成します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--list` | `-l` | flag | false | 現在の別名一覧 |
| assignment | | string | — | `name=target` で設定、`name=` で削除 |

**実装要件**:
- `alias.yaml` の読み書き
- 別名 → ID 解決をターゲット解決パイプラインに統合
- 英数字+アンダースコアのみ。禁止語: `hotentry`

---

### 22. `inspect` — ❌ 未実装

> 小説状態の調査状況ログを表示します

オプションなし。target 省略時 = 最終変換小説。

**実装要件**:
- `NovelSetting` + `Inspector` メッセージの読み取り・表示
- `convert.inspect=true` で常時表示

---

### 23. `csv` — ❌ 未実装

> 小説リストをCSV形式で出力したりインポートしたりします

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--output FILE` | `-o` | string | stdout | CSV 保存先 |
| `--import FILE` | `-i` | string | — | CSV からインポート |

**実装要件**:
- エクスポート列: id, title, author, sitename, url, novel_type, tags, frozen, last_update, general_lastup
- インポート: `url` 列が必須。各行を `download` に渡す

---

### 24. `trace` — ✅ 完了

> 直前のバックトレースを表示します

オプションなし。デバッグ用。

**Rust 実装**: `src/commands/trace.rs` で `trace_dump.txt` をそのまま表示。保存先は Ruby版同様、`.narou` が見つかる場合はルート直下、なければ CWD 直下。

---

## 実装優先度

### P0: コアパイプラインの完成
既存9コマンドの互換性を完成させる。

| タスク | コマンド | 影響 |
|-------|---------|------|
| download `--force`, `--no-convert`, `--freeze` | download | DL フラグ互換 |
| update の残互換実装 | update | Ruby版ターゲット解決・`--gl`主要挙動・`update.strong`・digest選択肢・差分用 cache 退避・hotentry の copy/send/mail までは実装済み。周辺出力/イベント細部が残る |
| convert `--device`, `--no-open`, `--output` | convert | 変換パイプライン完成 |
| list `--latest`, `--reverse`, `--filter`, `--url`, `--author`, `--site` | list | 一覧表示の実用性 |
| tag `--color`, `--clear`, `--list` | tag | タグ管理の完成 |
| remove `--yes`, `--with-file` | remove | 削除の安全性 |
| freeze `--list`, `--on` | freeze | 凍結管理の完成 |

### P1: 設定管理基盤
多くのコマンドが `local_setting` / `global_setting` に依存する。

| タスク | 説明 |
|-------|------|
| `setting` コマンド | 設定の読み書き・一覧・バリデーション |
| `default.*` / `force.*` 解決 | 設定カスケードの実装 |
| 設定値型バリデーション | boolean/integer/float/string/directory/select/multiple |

### P2: ユーティリティコマンド
比較的独立して実装可能。

| コマンド | 難易度 | 依存 |
|---------|:------:|------|
| `help` | 低 | なし |
| `version` | 低 | なし |
| `folder` | 低 | なし |
| `browser` | 低 | なし |
| `alias` | 低 | `alias.yaml` |
| `clean` | 低 | TOC 読み込み |
| `csv` | 中 | DL パイプライン |
| `inspect` | 中 | Inspector メッセージ |
| `log` | 中 | ログシステム |

### P3: 端末連携
外部ツール依存。

| コマンド | 難易度 | 依存 |
|---------|:------:|------|
| `send` | 高 | USB マスストレージ、端末別規則 |
| `mail` | 高 | SMTP 設定 |
| `diff` | 中 | 外部 diff ツール、raw データ管理 |

### P4: グローバル機能

| 機能 | 説明 |
|------|------|
| `--no-color` | 全コマンド対応 |
| `--multiple` | 引数区切り `,` 対応 |
| `--time` | 実行時間計測 |
| `--backtrace` | 詳細エラー表示 |
| コマンドショートカット | 1-2文字省略名 |
| `default_args.*` | コマンド別デフォルト引数 |

---

## 設定型定義参照

設定値の型は `setting.rb` で定義されている。Rust 側でも同等の型定義が必要。

| 型 | 説明 | 例 |
|-----|------|-----|
| `:boolean` | 真偽値 | `true` / `false` |
| `:integer` | 整数 | `10` |
| `:float` | 浮動小数点 | `2.5` |
| `:string` | 文字列 | `"Kindle PaperWhite"` |
| `:directory` | ディレクトリパス (存在チェック) | `"/path/to/dir"` |
| `:select` | 選択肢から一つ | `device`: `kindle`, `kobo`, 等 |
| `:multiple` | 選択肢から複数 (カンマ区切り) | `economy`: `cleanup_temp,send_delete` |
