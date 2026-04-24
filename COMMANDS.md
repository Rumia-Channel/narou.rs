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

narou.rb はコマンド名の先頭1文字または2文字でコマンドを一意に特定できる。 ✅ 完了

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
| `download` | ✅ | 🟡 部分 | `--mail` と保存フォルダ欠落時の再DL確認まで実装。`mail` 系の最終 end-to-end 確認待ちで保留 |
| `update` | ✅ | 🟡 部分 | Ruby版ターゲット解決、freeze.yaml参照、完結タグ同期、`--gl`主要挙動、`update.strong` 相当の同日本文比較、section hash cache 永続化、digest選択肢、差分cache退避、Ctrl+C 中断、hotentryのcopy/send/mailまでは実装済み。hotentry 周辺の細部が残る |
| `convert` | ✅ | 🟡 部分 | `--output` / `--enc` / テキストファイル入力 / `--inspect` / `convert.inspect` / `--no-open` / `--no-epub` / `--no-mobi` / `--no-strip` / `--make-zip` / `--no-zip` / `--verbose` / `device` 設定反映 / `convert.multi-device` / `convert.copy-to` / `convert.copy-zip-to` / `convert.copy-to-grouping` / `--ignore-default` / `--ignore-force` / `dc:subject` 埋め込み / `調査ログ.txt` 生成、`enable_erase_introduction` / `enable_erase_postscript` 反映、Ruby式の auto-indent 判定、保存済み/未保存の挿絵ローカル注記化と保存INFOまでは実装。実機 send 最終確認が残る |
| `list` | ✅ | ✅ 完了 | `limit`, `--latest`, `--gl`, `--reverse`, `--url`, `--kind`, `--site`, `--author`, `--filter`, `--grep`, `--tag`, `--echo` と pipe 時ID出力まで実装 |
| `tag` | ✅ | ✅ 完了 | `--add`, `--delete`, `--color`, `--clear`、引数なしタグ一覧、タグ検索、`tag_colors.yaml` 自動色ローテーションまで実装 |
| `freeze` | ✅ | ✅ 完了 | `--list` / `--on` / `--off`、freeze.yaml 同期、URL/Nコード/alias/tag 解決まで実装 |
| `remove` | ✅ | ✅ 完了 | `--yes`, `--with-file`, `--all-ss`、確認、freeze/lock チェックを実装 |
| `web` | ✅ | 🟡 部分 | API / queue worker / auto-scheduler に加え、pure JS / pure CSS の分割 frontend、JP/EN 切替、theme/performance/reload 設定反映までは実装済み。frontend は全件取得+client-side 描画のため narou.rb 細部 parity は継続中 |
| `setting` | ✅ | ✅ 完了 | 基本読み書き、`--burn`、dynamic `default/force/default_args`、hidden select 値検証、`setting -a` の全変数一覧まで Ruby 互換に揃えた |
| `diff` | ✅ | ✅ 完了 | 外部 diff ツール、raw データ管理 |
| `send` | ✅ | ✅ 完了 | Kindle/Kobo/Reader 送信、`--without-freeze`、栞 backup/restore、hotentry を実装 |
| `mail` | ✅ | 🟡 部分 | `mail_setting.yaml` bootstrap / 不完全設定 path 表示 / spinner / hotentry / `last_mail_date` 差分送信、Pony寄りの SMTP/TLS オプション受理までは実装。実SMTP の最終確認が残る |
| `backup` | ✅ | ✅ 完了 | `narou backup`/複数 target、`backup/` 除外、180バイト切り詰めまで対応 |
| `clean` | ✅ | ✅ 完了 | `latest_convert` 既定値、`--all`、`--force`/`--dry-run`、freeze スキップ、`raw/*.txt|*.html` と `本文/*.yaml` の orphan 判定を実装 |
| `help` | ✅ | ✅ 完了 | トップレベル help、初回未初期化 help、各コマンド `-h` の詳細文・Examples・convert Configuration・setting Variable List まで同期 |
| `version` | ✅ | ✅ 完了 | `-v`/`--version` と `--more` を実装。出力順序、help 文言、AozoraEpub3 探索、失敗時メッセージを Ruby 版に揃えた |
| `log` | ✅ | ✅ 完了 | `--num`, `--tail`, `--source-convert`, `<path>` を実装。最新ログ選択、`.narou/local_setting.yaml` の `log.*` 既定値、`*_convert` フィルタも対応 |
| `folder` | ✅ | ✅ 完了 | `--no-open`、引数省略時 help、alias/tag 解決を実装 |
| `browser` | ✅ | ✅ 完了 | `--vote` で最新話感想ページ生成、引数省略時 help、alias/tag 解決を実装 |
| `alias` | ✅ | ✅ 完了 | `alias.yaml` 読み書き、`--list`、`name=` 解除、`hotentry` 禁止語、共通ターゲット解決への統合を実装 |
| `inspect` | ✅ | ✅ 完了 | `調査ログ.txt` 表示、変換時ログ生成、`convert.inspect`、summary/full display、括弧/kana/前後書き系、挿絵保存成功/失敗、textfile の `enable_enchant_midashi` 推奨 INFO まで実装 |
| `csv` | ✅ | ✅ 完了 | CSV export/import、`-o` / `-i`、`url` ヘッダー必須、download 経由 import を実装 |
| `trace` | ✅ | ✅ 完了 | `trace_dump.txt` を表示。panic 時に保存されたバックトレースを読む |

---

## 各コマンドの詳細仕様

### 1. `init` — ✅ 完了

> 現在のフォルダを小説用に初期化します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--path` | `-p` | string | — | AozoraEpub3 フォルダ指定。`:keep` で既存再利用 |
| `--line-height` | `-l` | float | 1.8 | 行の高さ (em) |

**Rust 実装**: `src/commands/init.rs`。Ruby版同様、初期化時は `.narou/`・`小説データ/`・ユーザー編集用 `webnovel/` を作成し、`webnovel/*.yaml` を初回コピーする。`.narou/local_setting.yaml` や `queue.yaml` などの inventory ファイルは init 時に eager 生成せず、各機能が必要になった時点で作る。AozoraEpub3 / mail 用の preset は repo 直下 `preset/` に同梱し、`init` / `mail` が `sample/narou/preset` に依存せず自己完結で動くようにした。2026-04 の FS hardening で `--path` / `:keep` は絶対パスのみ受け付け、UNC・drive-relative・`\\?\` 形式を拒否した上で canonicalize 後も `AozoraEpub3.jar` の親ディレクトリ一致を再確認する。

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
- `-m`/`--mail`: 変換後に `mail` コマンドと同じ helper で送信。設定未作成時は `mail_setting.yaml` を生成し、注意メッセージも Ruby版に寄せた
- 引数なしでインタラクティブモード (stdin から URL 入力、TTY時のみ)
- 凍結チェック: Ruby版と同じ `.narou/freeze.yaml` を優先し、移行互換として `frozen` タグも補助的に認識した上で凍結済み小説をスキップ
- ダウンロード済みチェック: 既存小説はスキップ（`--force`で上書き）
- DB に記録があるのに保存フォルダが消えていた場合は、Ruby版同様に DB インデックスを削除して `再ダウンロードしますか (y/n)?` を確認する。非TTYでは yes 扱いで再DLへ進む
- タグ展開: `tag:NAME` → 該当IDに展開、`^tag:NAME` → 補集合
- `tagname_to_ids`: ID優先、未登録はタグ名として展開
- `mistook_count` 追跡 → 終了コード反映
- 複数ターゲット間の水平線セパレータ
- 有効ターゲット検証: Nコード or URL(サイト設定マッチ)
- Nコード指定時は `https://ncode.syosetu.com/<ncode>/` からURLキャプチャを作り、サイト定義の `\k<ncode>` を展開してDLする

**完了扱いにしない理由 / 不足動作**:
- `--mail` の最終的な実SMTP end-to-end 検証は `mail` コマンド側の未了項目と共通

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
- サイト/API由来のタイムゾーン表記なし日時は `webnovel/*.yaml` の `timezone`、未指定時は local `time-zone`（既定 `Asia/Tokyo`）で解釈する。DB の YAML には narou.rb 互換の `YYYY-MM-DD HH:MM:SS.nnnnnnnnn +09:00` 形式で保存し、既存 Rust 版の epoch 秒も読み込める。なろう API の `YYYY-MM-DD HH:MM:SS` は JST 扱い
- あらすじ比較は `<br>`/`<br/>`/`<br />` と改行・行末空白を正規化し、実質同一なら更新扱いにしない
- 既存小説で実変更が無い場合は `last_update` を保持し、更新確認だけで Web UI の `new-update` 表示が再点灯しないようにする
- 凍結チェック: Ruby版と同じ `.narou/freeze.yaml` を参照（既存Rustデータ移行用に `frozen` タグも補助的に認識）
- `modified` タグ管理: 更新成功時に自動削除、`--gl` で変更検出時に自動付与
- `end` タグ管理: 更新・`--gl other` で完結状態に合わせて `end` タグを同期
- `_convert_failure` フラグ: 変換失敗時に記録、次回更新で再変換を試行
- `update.interval` 設定対応（最低2.5秒、YAMLの数値/文字列を許容）
- `update.strong` 設定対応。同日更新時は保存済み `本文/*.yaml` の本文要素と取得本文をハッシュ比較し、実質同一なら更新扱いにしない
- Ruby版同様 `.narou/section_hash_cache.yaml` を永続化し、strong update 時の既存 section digest を再利用する
- `update.convert-only-new-arrival` 設定対応（YAMLの真偽値/文字列/数値を許容）
- `last_check_date` 追跡
- `download.choices-of-digest-options` 設定対応。Ruby版と同じ 1-8 のダイジェスト化選択肢を処理し、キャンセル・凍結・バックアップ・あらすじ表示・ブラウザ起動・保存フォルダ起動・変換を実行
- ダイジェスト化キャンセル時は `UpdateStatus::Canceled` を返し、`update` / `download` コマンド側でRuby版相当のキャンセル表示と終了コード加算を行う
- 差分更新時は Ruby版同様 `本文/cache/<timestamp>/` に旧sectionを退避し、差分が無い場合は空cacheディレクトリを削除
- `SuspendDownload` 発生時は通常失敗ではなくバッチ全体の中断として扱うように修正
- `auto-add-tags` 設定対応。site YAML の `tags` パターンから取得したタグをDBタグへ自動追加
- `hotentry` / `hotentry.auto-mail` 設定のうち、hotentry の新着話収集・統合テキスト生成・device に応じた変換・`copy-to`・端末送信・mail までは実装済み
- `confirm_over18?` 相当として、R18 サイトで承諾した場合は global `over18: true` を保存し、以後は再確認しない。`over18: false` を明示設定した場合は Ruby版同様に再確認せず中止し、ハーメルンは site YAML の R18 マーカー検出でも同じ分岐を通す
- ソートキーバリデーション（不正キーでエラー+終了コード127）
- `setting update.sort-by` の select 値を Ruby版 `Narou::UPDATE_SORT_KEYS` と同期済み
- 小説間インターバル（Ruby版 `Interval` クラス互換）
- 全件更新時の凍結スキップ、個別指定時の凍結メッセージ
- 終了コード: エラー数（最大127）、中断時126
- Ctrl+C 割り込み時はフラグを検知して `アップデートを中断しました` を表示し、終了コード126で終了
- `--all` は Ruby版に存在しないRust独自オプションだったため削除

**完了扱いにしない理由 / 不足動作**:
- Ruby版の詳細表示・hotentry後処理など、周辺出力/イベント処理の細部は追加突合が必要

---

### 4. `convert` — 🟡 部分

> 小説を変換します。管理小説以外にテキストファイルも変換可能

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--output FILE` | `-o` | string | — | 出力ファイル名指定 | ✅ |
| `--make-zip` | — | flag | false | i文庫 ZIP 作成 | ✅ |
| `--enc ENCODING` | `-e` | string | UTF-8 | テキストファイル文字コード | ✅ |
| `--no-epub` | — | flag | false | EPUB 生成スキップ | ✅ |
| `--no-mobi` | — | flag | false | MOBI 生成スキップ | ✅ |
| `--no-strip` | — | flag | false | MOBI ストリップスキップ | ✅ |
| `--no-zip` | — | flag | false | ZIP 作成スキップ | ✅ |
| `--no-open` | — | flag | false | 出力フォルダを開かない | ✅ |
| `--inspect` | `-i` | flag | false | 小説状態調査ログ表示 | ✅ |
| `--verbose` | `-v` | flag | false | AozoraEpub3/kindlegen 標準出力表示 | ✅ |
| `--ignore-default` | — | flag | false | default.* 設定を無視 | ✅ |
| `--ignore-force` | — | flag | false | force.* 設定を無視 | ✅ |
| targets | | Vec\<String\> | — | ID/タイトル/ファイルパス | ✅ |

**不足動作**:
- 変換後の端末送信の実機最終検証

**Rust 実装メモ**:
- `-o/--output` を direct convert に接続し、フォルダ部分を無視して保存先小説フォルダ配下へ出力する。複数 target 時は Ruby版同様 `basename (n).ext` を付ける
- `-i/--inspect` を clap / `main.rs` / `commands::convert` に接続し、`local_setting.yaml` の `convert.inspect=true` も Ruby版同様に direct convert の既定値として注入する
- `--no-open` と `convert.no-open=true` を direct convert に反映し、既定では最初に生成した出力ファイルの保存フォルダを開く
- `device` 設定が `text` 以外なら direct convert でも `OutputManager` 経由で ebook を生成し、`--no-epub` と `convert.no-epub=true` があれば Ruby版同様 txt のみに戻す
- `device=kindle` かつ `--no-mobi` / `convert.no-mobi=true` の場合は kindlegen を呼ばず EPUB 出力へ切り替える。`sample\\novel` で `device: kindle` を一時設定して `convert --no-mobi 3` の `.epub` 出力を確認済み
- kindlegen の探索は AozoraEpub3 同梱 / PATH / Windows の Kindle Previewer 3 同梱版を順に見るようにし、Ruby版同様 exit code 2 のみをエラー扱いに寄せた。`sample\\novel` で `device: kindle` の通常変換から `.mobi` 出力まで確認済み
- `--no-strip` を `OutputManager` まで通し、通常の Kindle 変換では kindlegen 後に SRCS セクションを除去するようにした。`sample\\novel` で stripped `.mobi` が 1,676,009 bytes、`--no-strip` 時は 4,176,001 bytes となりサイズ差を確認済み
- `--make-zip` / `convert.make-zip=true` と `device=ibunko` を direct convert に接続し、i文庫 ZIP を生成できるようにした。ZIP には本文 `.txt`・`挿絵/*`・`cover.*` を同梱し、`sample\\novel` で ZIP/EPUB 併産を確認済み
- `--no-zip` / `convert.no-zip=true` も direct convert に反映し、`sample\\novel` で `--make-zip --no-zip 3` 実行時は EPUB のみ残ることを確認済み
- `--verbose` 未指定時は AozoraEpub3 / kindlegen の標準出力を抑止し、指定時のみ透過表示する。`sample\\novel` で `device: epub` の通常変換では AozoraEpub3 行が出ず、`--verbose` 付きでは `Detected encoding = UTF-8` / `変換開始` が表示されることを確認済み
- `convert.add-dc-subject-to-epub=true` 時は DB タグから `convert.dc-subject-exclude-tags` を除いた値を EPUB 内 `standard.opf` の `<dc:subject>` へ埋め込む。除外設定未作成時は Ruby版同様 `404,end` を自動保存し、`sample\\novel` で `alpha` タグだけが埋め込まれ `end` は除外されることを確認済み
- direct `convert` 後の端末送信は従来の compat 経路と同じ `send_file_to_device` helper へ揃えた。`sample\\novel` の `device: epub` 経路では副作用なく通ることを確認したが、Kindle/Kobo/Reader 実機での最終検証までは未了
- `convert.copy-to` / `convert.copy_to` と `convert.copy-to-grouping` の device/site グルーピングも direct convert へ接続し、`sample\\novel` で `device=epub` + `copy-to-grouping=device,site` のコピー先生成を確認済み
- `convert.copy-zip-to` も direct convert へ接続し、`sample\\novel` で `--make-zip 3` 実行時に ZIP のコピー出力を確認済み
- `convert.multi-device` があれば `device` より優先して複数端末へ順に変換する。Ruby版同様 `kindle` を先頭へ寄せ、無効な端末名は警告し、`sample\\novel` で `convert.multi-device: epub,ibunko` により EPUB + ZIP 出力を確認済み
- `--ignore-default` / `--ignore-force` を `NovelSettings::load_for_novel_with_options` に渡し、`default.*` / `force.*` の適用を個別に無効化できるようにした
- DB 管理小説だけでなくファイルパス指定の textfile 変換も `commands::convert` に接続し、`--enc` による UTF-8 / Shift_JIS / EUC-JP 系のデコードと `enable_enchant_midashi` 推奨 INFO を追加した
- 変換後に `調査ログ.txt` を常に保存し、`enable_inspect` が有効なときは行末読点状況とカギ括弧内改行状況を記録する
- `--inspect` 指定時は full display、未指定時は Ruby版同様に summary だけを出す
- `enable_erase_introduction` / `enable_erase_postscript` を section 変換に反映し、`enable_auto_indent` は Ruby版 `Inspector#inspect_indent` 相当の比率判定でのみ有効化する
- 保存済み `挿絵/<section-index>-<count>.<ext>` があれば HTML `<img>` を対応する青空文庫注記へローカルパスで復元する。`sample\\novel` の n8858hb section 16 で `［＃挿絵（挿絵/16-0.jpg）入る］` を確認済み
- Windows の `\\?\\C:\\...\\AozoraEpub3.jar` 形式パスは Java classpath にそのまま渡すと失敗するため、Ruby版同様に jar の basename を current_dir 基準で渡すよう修正した。`sample\\novel` で `device=epub` 実変換と `--no-epub` 抑止を確認済み

**注**: EPUB/MOBI 生成は AozoraEpub3.jar と kindlegen への依存がある。Rust 側のテキスト変換 (`novel.txt` 生成) は完了しているが、AozoraEpub3 の呼び出しパイプラインは別途必要。

---

### 5. `list` — ✅ 完了

> 現在管理している小説の一覧を表示します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--latest` | `-l` | flag | false | 最新更新順でソート | ✅ |
| `--gl` | — | flag | false | 更新日ではなく最新話掲載日を使用 | ✅ |
| `--reverse` | `-r` | flag | false | 逆順ソート | ✅ |
| `--url` | `-u` | flag | false | URL 表示 | ✅ |
| `--kind` | `-k` | flag | false | 小説種別表示 (短編/連載) | ✅ |
| `--site` | `-s` | flag | false | サイト名表示 | ✅ |
| `--author` | `-a` | flag | false | 作者名表示 | ✅ |
| `--filter VAL` | `-f` | string | — | フィルタ: `series`/`ss`/`frozen`/`nonfrozen` | ✅ |
| `--grep VAL` | `-g` | string | — | テキスト検索。`-` prefix で NOT | ✅ |
| `--tag [TAGS]` | `-t` | string | — | タグ表示/フィルタ | ✅ |
| `--echo` | `-e` | flag | false | パイプ時も人間可読出力 | ✅ |
| limit | | int | — | 表示数上限 | ✅ |

**Rust 実装**:
- `limit` positional、`--latest` / `--gl` / `--reverse`、列追加オプション (`--url` / `--kind` / `--site` / `--author`) を実装
- `--filter` は `series` / `ss` / `frozen` / `nonfrozen` の複数指定と Ruby版相当の不正値エラー (終了コード127) に対応
- hidden 互換オプション `--frozen` も受理し、`--filter frozen` 相当として扱う
- `--grep` は AND / `-word` の NOT 検索に対応
- `--tag` は無引数でタグ列表示、引数付きで全指定タグを含む小説に絞り込む
- TTY では人間可読一覧、pipe / redirect 時は ID のみ、`--echo` 時はヘッダ付き一覧を出力する
- `.narou/freeze.yaml` に基づく凍結表示、6時間以内更新の色分け、`tag_colors.yaml` によるタグ色付けを実装

---

### 6. `setting` — ✅ 完了

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
| `device` | select | 対象端末 (kindle/kobo/epub/ibunko/reader/ibooks) |
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
| `time-zone` | string | YAML に timezone がないサイト日時の既定タイムゾーン |
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
| `over18` | boolean | 18+ フラグ（未設定時は初回のみ確認、`false` 明示時はR18取得を中止） |

**実装済み (Rust)**:
- `local_setting.yaml` / `global_setting.yaml` の読み書き (`Inventory`)
- 設定値のバリデーション (型チェック、選択肢チェック、boolean の true/false 厳密化)
- `default.*` / `force.*` / `default_args.*` は既知の original setting / command 名だけ受理し、未知名を拒否
- `default_args.trace` / `default_args.console` を含む Ruby 由来の command 名を受理
- `webui.table.reload-timing` / `webui.theme` など hidden select 項目でも選択肢チェックを実施
- `--list` 現在値一覧表示
- `--all` 全変数表示
- `setting -a` では hidden 項目に加え、`default.*` / `force.*` / `default_args.*` を型・説明付きで Local Variable List に列挙
- `--burn` による setting.ini への焼き込み
- `--burn` の確認プロンプトと tag ターゲット展開
- `name` 読み取り、`name=value` 設定、`name=` 削除
- 不明変数名のエラー、古い変数の掃除削除
- エラー数を終了コードとして返す
- `apply_force_and_default_settings` で変換時の `force.*/default.*` をフラットキーから正しく解決
- `device` 変更時の関連設定の自動反映（Ruby の `RELATED_VARIABLES` に合わせて `default.enable_half_indent_bracket` を変更）
- `time-zone` をローカル設定として追加。`webnovel/*.yaml` の `timezone` がないサイトで、タイムゾーン表記なしの日時を内部時刻へ変換する際の既定値として使う。DB YAML への日時保存は narou.rb 互換の JST timestamp 形式を維持する

---

### 7. `freeze` — ✅ 完了

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
- `.narou/freeze.yaml` を保存先に使い、`frozen` タグとも同期
- `tagname_to_ids` と `Downloader.get_data_by_target` 相当で ID/Nコード/URL/タイトル/別名/タグ名を解決

---

### 8. `tag` — ✅ 完了

> 各小説にタグを設定及び閲覧が出来ます

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--add TAGS` | `-a` | string | — | タグ追加 (スペース区切り) | ✅ |
| `--delete TAGS` | `-d` | string | — | タグ削除 | ✅ |
| `--color COL` | `-c` | string | auto | タグ色設定 | ✅ |
| `--clear` | — | flag | false | 全タグクリア | ✅ |
| targets | | Vec\<String\> | — | 対象小説 | ✅ |

**Rust 実装**:
- 引数なしでタグ一覧表示、タグ名のみ指定時は Ruby版同様 `list --tag` 相当の検索へ委譲
- `--add` / `--delete` はスペース区切り複数タグを処理し、`--clear` は対象小説のタグを全削除する
- `--color` は `green/yellow/blue/magenta/cyan/red/white` を受け付け、無効色は Ruby版同様に警告して無視する
- `tag_colors.yaml` の保存順を保持しつつ、自動色ローテーション (green→yellow→blue→magenta→cyan→red→white) を実装
- 追加タグの禁止文字 `:;"'><$@&^\\\|%/\`` と禁止語 `hotentry` を Ruby版相当に検証する
- 編集後は `現在のタグは ... です` を表示し、タグ名/`tag:NAME`/ID/URL/Nコード/タイトル/alias のターゲット解決に対応

---

### 9. `remove` — ✅ 完了

> 小説を削除します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--yes` | `-y` | flag | false | 確認スキップ | ✅ |
| `--with-file` | `-w` | flag | false | ファイルも削除 | ✅ |
| `--all-ss` | — | flag | false | 全短編小説を対象 | ✅ |
| targets | | Vec\<String\> | — | 対象小説 | ✅ |

**Rust 実装**:
- デフォルトは Ruby版同様 DB index のみ削除し、保存フォルダは残したまま `toc.yaml` だけ削除する
- `--with-file` で小説保存フォルダを完全削除する
- `--yes` 未指定時は Ruby版 `Input.confirm` 相当で削除確認を出す
- `--all-ss` で `novel_type == 2` の短編を全選択する。短編が存在しない場合は `短編小説がひとつもありません` を表示する
- tag 展開、alias/タイトル/URL/Nコード解決、freeze.yaml ベースの凍結判定、`.narou/lock.yaml` が存在する場合の変換中チェックを実装

---

### 10. `diff` — ✅ 完了

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

**実装状況**:
- `-n/--number` と `-N` 短縮形、`-l/--list`、`-c/--clean`、`--all-clean`、`--no-tool` を実装済み
- 差分バージョン指定 `YYYY.MM.DD@HH.MM.SS` / `;` 区切りに対応
- セクション YAML からの一時テキスト生成、外部 diff ツール統合、内蔵差分ビューアを実装済み
- 既定対象は最新更新の小説で、差分キャッシュは `本文/cache/<version>/` に配置する

---

### 11. `send` — ✅ 完了

> 変換したEPUB/MOBIを電子書籍端末に送信します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--without-freeze` | `-w` | flag | false | 凍結小説を除外 |
| `--force` | `-f` | flag | false | タイムスタンプ無視で強制送信 |
| `--backup-bookmark` | `-b` | flag | false | ブックマークバックアップ (KindlePW) |
| `--restore-bookmark` | `-r` | flag | false | ブックマーク復元 |
| device | | string | — | 端末名 (kindle/kobo/等)。省略時=設定値 |
| targets | | Vec\<string\> | — | 対象。省略時=全小説 |

**Rust 実装**:
- `src/commands/send.rs` で Ruby版 `send.rb` の外部挙動を実装
- 先頭引数の device 指定、`narou setting device=<device>` からの既定端末選択、device 未指定時の Ruby準拠エラー文を実装
- `--without-freeze` / `--force` / `--backup-bookmark` / `--restore-bookmark` を実装
- `send.without-freeze` / `send.backup-bookmark` 設定を反映
- target 省略時は全小説を対象にし、`hotentry=true` なら `hotentry` も自動送信対象に追加
- tag 展開、alias/タイトル/URL/Nコード解決、hotentry 送信、先頭ファイルのタイムスタンプ比較によるスキップを実装
- Kindle/Kobo/Reader の USB documents 転送に対応
- Kindle の `.sdr/*.azw3{f,r}` 栞ファイルのバックアップ/復元を Ruby の `misc/bookmark/<device>/...` 配置に合わせて実装

---

### 12. `mail` — 🟡 部分

> 変換したEPUB/MOBIをメールで送信します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--force` | `-f` | flag | false | 全ての小説を強制送信 |
| targets | | Vec\<string\> | — | 対象。`hotentry` 指定可 |

**Rust 実装**:
- `mail_setting.yaml` の読み込み、未作成時の preset コピー、初回作成時の `last_mail_date` 初期化を実装
- 設定不完全時は Ruby版同様に設定ファイルのフルパス付きでエラーを表示
- target 省略時は `.narou/freeze.yaml` を優先しつつ `frozen` タグも補助的に認識して凍結以外の全小説を対象にし、`last_mail_date` と `new_arrivals_date` を比較して未送信分だけ送る
- `hotentry` 特別扱い、tag 展開、alias / タイトル / URL / Nコード解決に対応
- 送信中は `メールを送信しています...` の進捗表示を行い、成功時に `last_mail_date` を更新する
- `smtp` 経路では preset に含まれる `via_options.domain` を EHLO 名へ反映し、`authentication` は `:plain` / `:login` / `:xoauth2` を受理する
- `smtp` の TLS 解釈は Ruby/Pony 寄りに拡張しつつ、narou.rs では既定で TLS 必須にした。`ssl` / `tls` / `enable_starttls` / `enable_starttls_auto` は継続受理し、`allow_insecure: true` / `mail.smtp.allow_insecure` と `tls_skip_verify: true` / `mail.smtp.tls_skip_verify` を明示した場合だけ平文SMTP・opportunistic STARTTLS・証明書検証無効化を許可する
- message 生成では `reply_to` / `cc` / `bcc` を受理し、複数宛先は YAML sequence またはカンマ区切り文字列で解釈する

**完了扱いにしない理由 / 不足動作**:
- Ruby版 Mailer/Pony 設定のうち `smtp` 以外の経路や周辺オプションは未確認
- 実SMTP を使った sender-side / arrival-side の end-to-end 検証がまだ不足

---

### 13. `web` — 🟡 部分

> WEBアプリケーション用サーバを起動します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--port PORT` | `-p` | int | 未指定時は global `server-port` / 初回はランダム保存 | サーバポート | ✅ |
| `--no-browser` | `-n` | flag | false | ブラウザ自動起動抑制 | ✅ |
| `--hide-console` |  | flag | false | Windows でコンソールを出さずタスクトレイ常駐 | ✅ |

**実装済み動作**:
- `--port` 未指定時は Ruby版同様 global `server-port` を使い、未設定ならランダムポートを採番して保存する
- global `server-bind` を読み、未設定時は `127.0.0.1` に bind する（`localhost` は `127.0.0.1` 扱い）
- WebSocket サーバは HTTP と別に `server-port + 1` で起動する
- 初回起動時は Ruby版同様にファイアウォール許可と停止方法の案内を表示し、`server_setting.yaml` に起動済みフラグを保存する
- global `server-basic-auth.*` が有効な場合は HTTP/WS の両ルータで Basic 認証を要求する
- hidden global `server-basic-auth.require-for-external-bind` は narou.rs 独自の外部公開ガード。既定値 `true` の間は `0.0.0.0` / 公開bindで Basic 認証未設定の起動を拒否し、`false` にするとこのガードだけ解除する（Web UI には表示しない）
- hidden global `server-reverse-proxy.enable` は narou.rs 独自の reverse proxy モード。既定値 `false` で、`true` にすると nginx 等の前段 proxy が付ける外側の Host / Origin を受け入れ、same-origin の `/ws` 接続を使う（Web UI には表示しない）
- API の凍結/解凍操作と一覧上の `frozen` 判定は CLI と同じ `.narou/freeze.yaml` を優先し、`frozen` タグは補助的に扱う
- queue worker が `.narou/queue.yaml` 永続キューを読み書きし、download / update / auto_update / convert / send / backup / mail の queued job を別プロセスまたは worker 内処理で実行する。Ruby版同様 `pending` / `running` を分けて保持し、legacy `cmd` / `args` / `meta` / `status` / `created_at` / `started_at` を維持したまま復元できる。`concurrency` 有効時は外部通信あり(download/update/auto_update)とその他(convert/send/backup/mail)を別 lane で並列実行し、無効時は全 job を投入順に逐次実行する
- Web 経由の convert job は `--no-open` で非対話化し、API 指定 device は worker 専用 override で child process に渡す
- `queue_clear` は deadlock しないように永続キュー保存順を修正済み
- local `update.auto-schedule.enable` / `update.auto-schedule` が有効なら、Ruby版同様に時刻指定で自動アップデートを Web queue に投入する。設定保存時は Ruby版同様に scheduler を stop/start し、サーバ再起動なしで変更を反映する
- 自動アップデートは `--gl narou` → `modified` タグ対象 → その他小説の順に child `update` を実行し、child stdout/stderr と Web 用構造化進捗を Web UI コンソールへ中継する。実行中 phase の child PID は通常 job と同じ中止処理へ登録する。各 phase 後に Web サーバ側 DB を再読み込みして `modified` タグ検出漏れを防ぐ。`server_setting.current_sort` は Ruby互換に `column` 数値/数値文字列の両方を受理し、対応する `--sort-by` へ引き継ぐ
- Web UI からのサーバ再起動では replacement process に `--no-browser` を付与し、hidden 起動中は `--hide-console` も維持したまま再起動待機ページから同じタブで元ページへ戻る
- Windows の `narou web --hide-console` は GUI subsystem で起動し、通常 CLI 実行時は親コンソールへ再接続、hidden 実行時はタスクトレイの右クリックメニューから `終了` / `再起動` を呼べる。Web worker / auto-update / 即時 API 実行が起動する child process も hidden 状態を引き継ぎ、空のコンソールを開かない
- Web 設定画面は Ruby版同様、`tab` がある設定を `invisible` 指定でも表示する。`webui.theme` / `webui.table.reload-timing` / `webui.debug-mode` / `server-bind` / `server-basic-auth.*` / `server-ws-add-accepted-domains` / `over18` も設定画面に出る
- `webui.theme` / `webui.table.reload-timing` / `webui.performance-mode` / `webui.debug-mode` 保存時は、開いている Web UI に設定再読み込みイベントを送り、テーマメニューの変更も `webui.theme` へ保存する
- `webui.debug-mode` が ON のときは、Web worker が失敗 child process の直近 stdout/stderr を要約して `queue_failed` イベントに載せ、Web UI 通知とコンソールに詳細エラーを出す。OFF のときは従来どおり簡潔な失敗通知だけにする
- Web UI のタグ編集は既存タグ一覧から入力中タグ名に一致する候補を表示し、タグ名クリック検索は `tag:`、作者名クリック検索は `author:`、掲載サイトクリック検索は `sitename:` を生成する。通常クリックは AND、Ctrl クリックは同一フィールド内 OR、Shift クリックは除外 AND、Shift+Ctrl クリックは除外 OR として検索文字列を更新する
- Web UI の単体/一括削除 API は削除成功時に console history へ削除ログを出力する。`concurrency` 有効時は非外部通信として `#console-stdout2`、無効時は `#console` を使う
- Web サーバ起動時は Ruby版 `fill_general_all_no_in_database` 相当に、`general_all_no` 未設定レコードの `toc.yaml` を読んで話数をDBへ補完する
- `/` では pure JS / pure CSS の分割 asset frontend を配信し、navbar / console / control panel / list + sidebar の構成で一覧操作できる
- UI は日本語既定で、JP/EN トグルによる切替と `localStorage` 永続化に対応する
- `webui.theme` / `webui.performance-mode` / `webui.table.reload-timing` を `/api/webui/config` と worker 側設定参照経由で反映し、theme 初期値、performance auto/on/off 判定、table reload の every/queue 挙動へ接続する
- レスポンシブ CSS を分離し、スマートフォン幅でも一覧・キュー・メモ帳を同じ asset 構成で表示できる
- 一覧 API の `frozen` 取得は DB 再入ロックによる deadlock を避けるよう修正済み
- favicon は data URL で埋め込み、追加 route なしでブラウザ 404 を出さない

**不足動作**:
- narou.rb の HAML/UI と完全一致するレベルの細かな見た目・配置・文言差分の洗い込み
- Web UI の各操作・表示を Ruby 版 view/helper と突き合わせた最終 parity 確認

---

### 14. `backup` — ✅ 完了

> 小説のバックアップを作成します

オプションなし。

**Rust 実装**: `src/commands/backup.rs` で Ruby 版同様に複数 target を順に処理し、`backup/` 直下へ ZIP を保存する。バックアップ名はタイトルを Ruby 版同様に整形して 180 バイトで切り詰める。

---

### 15. `clean` — ✅ 完了

> ゴミファイルを削除します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--force` | `-f` | flag | false | 実際に削除 |
| `--dry-run` | `-n` | flag | false | 表示のみ |
| `--all` | `-a` | flag | false | 全小説を対象 |
| target | | string | — | 小説指定 (省略時=最終変換) |

**Rust 実装**: `src/commands/clean.rs` で Ruby版 `clean.rb` の流れを実装。`--force` / `--dry-run` / `--all` に対応し、target 省略時は `.narou/latest_convert.yaml` の `id` を使って直前変換小説を検査する。`--all` は凍結済み小説をスキップし、TOC の `subtitles` から `"<index> <file_subtitle>"` を組み立てて orphan 判定する。

**互換メモ**:
- Ruby版は `raw/*.txt` を対象にするが、Rust 側の保存形式は `raw/*.html` のため両方を orphan 対象に含めた
- target 解決は ID / URL / Nコード / タイトル / alias / tag 展開を共通パイプラインで処理
- `sample\\novel` で `clean`, `clean <alias>`, `clean -f`, `clean --all`, `clean --all -f` を実行し、作為的 orphan の検出と削除を確認済み

---

### 16. `help` — ✅ 完了

> このヘルプを表示します

**実装** (`src/commands/help.rs`):
- 未初期化時: `narou init` を促すメッセージ（`.narou/` ディレクトリ存在チェック）
- 初期化済み: 全24コマンド一覧 + oneline_help（narou.rb と同一順序・同一テキスト）
- グローバルオプション表示（`--no-color`, `--multiple`, `--time`, `--backtrace`）
- ショートカット説明（`d`, `fr` 等の例示付き）
- `NO_COLOR` 環境変数対応（ANSIエスケープコード条件付き出力）
- 引数なし（`narou`）およびショートカット（`h`, `he`）からのフォールバック対応

**Rust 実装**:
- 未初期化時 / 初期化済み時のトップレベル help を Ruby版相当に表示
- 全24コマンドの oneline help、グローバルオプション、ショートカット説明を同一順序で表示
- `narou <command> -h` の banner、説明文、Examples、Options を Ruby版各 command に合わせて整備
- `convert` の `Configuration:` 節、`setting -h` の Variable List、`update --gl` の詳細説明表も表示

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

### 19. `folder` — ✅ 完了

> 小説の保存フォルダを開きます

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--no-open` | `-n` | flag | false | パス表示のみ |
| target | | string | — | 小説指定 |

**Rust 実装**: `src/commands/folder.rs` で Ruby版 `folder.rb` を実装。引数省略時は help を表示し、target 指定時は小説保存ディレクトリを開く。`--no-open` 指定時は開かずにパスだけを表示する。

**互換メモ**:
- target 解決は ID / URL / Nコード / タイトル / alias / tag 展開を共通パイプラインで処理
- Windows は `explorer`、macOS は `open`、Linux は `xdg-open` を使って既定のファイルマネージャを起動
- `sample\\novel` で `folder --no-open testalias` と引数省略時 help を確認済み

---

### 20. `browser` — ✅ 完了

> 小説の掲載ページをブラウザで開きます

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--vote` | `-v` | flag | false | 感想ページを開く (なろうのみ) |
| target | | string | — | 小説指定 |

**Rust 実装**: `src/commands/browser.rs` で Ruby版 `browser.rb` を実装。引数省略時は help を表示し、target 指定時は `toc_url` を既定ブラウザで開く。`--vote` 指定時は `toc.yaml` の最終 `subtitle.index` を読み、`<toc_url><index>/#my_novelpoint` を開く。

**互換メモ**:
- target 解決は ID / URL / Nコード / タイトル / alias / tag 展開を共通パイプラインで処理
- `sample\\novel` で引数省略時 help を確認し、`--vote` の URL 組み立ては unit test で検証済み

---

### 21. `alias` — ✅ 完了

> 小説のIDに紐付けた別名を作成します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--list` | `-l` | flag | false | 現在の別名一覧 |
| assignment | | string | — | `name=target` で設定、`name=` で削除 |

**Rust 実装**: `src/commands/alias.rs` で Ruby版 `alias.rb` を実装。`.narou/alias.yaml` の読み書き、`--list`、`name=target` による設定、`name=` による解除、`hotentry` 禁止語、半角英数字+`_` 制約に対応した。

**互換メモ**:
- list 表示は `alias=title` 形式
- 別名解決は `src/commands/mod.rs` / `src/commands/download.rs` の共通ターゲット解決へ統合済みで、`folder`, `browser`, `clean`, `convert`, `update`, `setting` などの既存 target 解決でも使える
- `sample\\novel` で `alias testalias=1`, `alias --list`, `alias testalias=` を確認済み

---

### 22. `inspect` — ✅ 完了

> 小説状態の調査状況ログを表示します

オプションなし。target 省略時 = 最終変換小説。

**Rust 実装**:
- `src/commands/inspect.rs` で `調査ログ.txt` の読み取り表示を実装。target 省略時は `.narou/latest_convert.yaml` の `id` を使い、複数 target は Ruby版同様に区切り線付きで順に表示する。ログが存在しない場合は `調査ログがまだ無いようです` を表示する
- `src/converter/inspector.rs` を追加し、変換時に `調査ログ.txt` を保存するようにした
- `convert.inspect=true` と `convert --inspect` で full display、通常 convert では Ruby版同様 summary 表示にした
- `auto_join_in_brackets` の警告/エラー、`modify_kana_ni_to_kanji_ni` INFO、`enable_erase_introduction` / `enable_erase_postscript` INFO も inspection に反映した
- `illustration.rb` 相当として、HTML挿絵のローカル保存、保存成功 INFO、未対応画像形式 / 例外 ERROR も convert 中の inspection に反映した
- textfile 変換時も `enable_enchant_midashi` が false なら Ruby版同様の推奨 INFO を記録し、`sample\\novel` で UTF-8 / Shift_JIS の実変換と `調査ログ.txt` 保存を確認済み

---

### 23. `csv` — ✅ 完了

> 小説リストをCSV形式で出力したりインポートしたりします

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--output FILE` | `-o` | string | stdout | CSV 保存先 |
| `--import FILE` | `-i` | string | — | CSV からインポート |

**Rust 実装**: `src/commands/csv.rs` で CSV export/import を実装。export は `id,title,author,sitename,url,novel_type,tags,frozen,last_update,general_lastup` を UTF-8 で出力し、`-o/--output` 指定時はファイル保存、未指定時は stdout へ出力する。import は `url` ヘッダー必須で、各行の URL を Ruby版同様 `download` 処理へ渡し、各 URL ごとに区切り線を出力する。

**互換メモ**:
- malformed CSV と `url` ヘッダー欠落はエラー終了
- `sample\\novel` で export / file output / import / malformed を確認済み

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
| update の残互換実装 | update | Ruby版ターゲット解決・`--gl`主要挙動・`update.strong`・section hash cache 永続化・digest選択肢・差分用 cache 退避・Ctrl+C 中断・hotentry の copy/send/mail までは実装済み。hotentry 周辺の細部が残る |
| convert send | convert | `--no-strip` まで実装済み。残りは実機 send 最終確認 |
| download の残互換実装 | download | command 固有の欠落はほぼ解消。残りは `mail` と共有の実SMTP end-to-end 確認 |

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
