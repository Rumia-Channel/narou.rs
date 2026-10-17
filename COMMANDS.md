# narou.rs コマンド互換性ドキュメント

narou.rb 全24コマンドのオプション・挙動と、Rust 側の実装状況・要件を整理する。Rust 拡張の `db` (SQLite 管理 DB 保守)・`illust` (挿絵メンテナンス)・`login` (ログイン情報管理)・`author` (作者追跡) を含む 28 コマンドを網羅する。

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
- 【Rust 拡張】0.4.0 未満から 0.4.0 以上へアップデートした後の初回起動時に、ライブラリ全体バックアップ (ライブラリルート全体 = `小説データ/` + `.narou/` + `webnovel/` + その他ユーザー作成ファイル → exe と同じフォルダの `backup/narou-backup-<timestamp>.zip`) を対話プロンプトで提案する。実際の zip 作成は同梱サブ実行ファイル `narou_rs_backup` が行い、保存先はプロンプトで変更できる。保存先がライブラリ内にある場合は対象から除外する。前回起動バージョンは `.narou/last-run-version` に記録。`web`/`help`/`version`・非対話実行・`NAROU_ENV=test` では出さない。容量不足時は警告して既定を「いいえ」にする。Web UI でも `GET/POST /api/library_backup` で同じ提案・作成を行える。

---

## コマンドショートカット

narou.rb はコマンド名の先頭1文字または2文字でコマンドを一意に特定できる。優先度は upstream の `Command::COMMAND_LIST` の並び順で決まり、**先に並ぶコマンドが優先**される。 ✅ 完了

| コマンド | 1文字 | 2文字 | 備考 |
|---------|:-----:|:-----:|------|
| download | `d` | `do` | |
| update | `u` | `up` | |
| list | `l` | `li` | |
| convert | `c` | `co` | `clean` / `csv` より優先 |
| diff | | `di` | |
| setting | `s` | `se` | `send` と衝突するため `s` / `se` は setting |
| alias | `a` | `al` | |
| inspect | `i` | `in` | `init` と衝突するため `i` は inspect |
| send | | | ショートカット無し（`s` / `se` は setting） |
| folder | `f` | `fo` | `freeze` と衝突するため `f` は folder |
| browser | `b` | `br` | `backup` と衝突するため `b` は browser |
| remove | `r` | `re` | |
| freeze | | `fr` | 1文字 `f` は folder |
| tag | `t` | `ta` | |
| web | `w` | `we` | |
| mail | `m` | `ma` | |
| backup | | `ba` | 1文字 `b` は browser |
| csv | | `cs` | |
| clean | | `cl` | |
| log | | `lo` | |
| trace | | `tr` | |
| help | `h` | `he` | |
| version | `v` | `ve` | |
| init | | | ショートカット無し（`i` / `in` は inspect） |
| db（Rust 拡張） | | `db` | narou.rb コマンドのショートカットを奪わない |
| illust（Rust 拡張） | | `il` | 同上 |
| login（Rust 拡張） | | | ショートカット無し。`lo` は `log` に解決されるため、フル名でのみ呼べる |

**注意**: v0.4.0 では `COMMAND_NAMES` の先頭に Rust 拡張の `db` が入っていたため `narou d` が `db` に解決されていた。現在は upstream と同じ並び（Rust 拡張は末尾）で、`d` は `download`。`src/cli.rs` のテストで並び順と主要な解決結果を固定している。

---

## 実装状況サマリ

| コマンド | narou.rb | Rust 完了度 | 備考 |
|---------|:--------:|:-----------:|------|
| `init` | ✅ | ✅ 完了 | AozoraEpub3 設定含め完全 |
| `download` | ✅ | 🟡 部分 | `--mail` と保存フォルダ欠落時の再DL確認まで実装。Pixiv (小説/小説シリーズ/イラスト・漫画/漫画シリーズ)、サイト定義の `headers:`/`min_interval:`、`narou login` のログイン Cookie フォールバックまで対応。`mail` 系の end-to-end は `tests/mail_e2e.rs` で完了済み |
| `update` | ✅ | 🟡 部分 | Ruby版ターゲット解決、freeze.yaml参照、完結タグ同期、`--gl`主要挙動、`update.strong` 相当の同日本文比較、section hash cache 永続化、digest選択肢、差分cache退避、Ctrl+C 中断、ドメイン別並列DL (`update.max-parallel-domains`)、hotentryのcopy/send/mailまでは実装済み。hotentry 周辺の細部が残る |
| `convert` | ✅ | 🟡 部分 | `--output` / `--enc` / テキストファイル入力 / `--inspect` / `convert.inspect` / `--no-open` / `--no-epub` / `--no-mobi` / `--no-strip` / `--make-zip` / `--no-zip` / `--verbose` / `device` 設定反映 / `convert.multi-device` / `convert.copy-to` / `convert.copy-zip-to` / `convert.copy-to-grouping` / `--ignore-default` / `--ignore-force` / `dc:subject` 埋め込み / `調査ログ.txt` 生成、`enable_erase_introduction` / `enable_erase_postscript`、表紙タイトルの `title_date` / 完結装飾反映、Ruby式の auto-indent 判定、保存済み/未保存の挿絵ローカル注記化と保存INFO、`lite` feature の組み込み EPUB エンジン (AozoraEpub3_Lite) フォールバックまでは実装。実機 send 最終確認が残る |
| `list` | ✅ | ✅ 完了 | `limit`, `--latest`, `--gl`, `--reverse`, `--url`, `--kind`, `--site`, `--author`, `--filter`, `--grep`, `--tag`, `--sort-by`, `--echo` と pipe 時ID出力まで実装 |
| `tag` | ✅ | ✅ 完了 | `--add`, `--delete`, `--color`, `--clear`、引数なしタグ一覧、タグ検索、`tag_colors.yaml` 自動色ローテーションと `webui.new-tag-color` 既定色まで実装 |
| `freeze` | ✅ | ✅ 完了 | `--list` / `--on` / `--off`、freeze.yaml 同期、URL/Nコード/alias/tag 解決まで実装 |
| `remove` | ✅ | ✅ 完了 | `--yes`, `--with-file`, `--all-ss`、確認、freeze/lock チェックを実装 |
| `web` | ✅ | 🟡 部分 | API / queue worker (自動リトライ含む) / auto-scheduler に加え、pure JS / pure CSS の分割 frontend、JP/EN 切替、theme/performance/reload 設定反映、「ログイン」設定タブ、`self-update.variant` によるセルフアップデート variant 選択、バージョン別機能ツアー (`/api/feature_tour/*`)、SQLite 移行の opt-in (`/api/storage/mode`) までは実装済み。frontend は全件取得+client-side 描画のため narou.rb 細部 parity は継続中 |
| `setting` | ✅ | ✅ 完了 | 基本読み書き、`--burn`、dynamic `default/force/default_args`、hidden select 値検証、`setting -a` の全変数一覧まで Ruby 互換に揃えた。`self-update.variant` / `narou-compat` など Rust 拡張変数も管理する |
| `diff` | ✅ | ✅ 完了 | 外部 diff ツール、raw データ管理 |
| `send` | ✅ | ✅ 完了 | Kindle/Kobo/Reader 送信、`--without-freeze`、栞 backup/restore、hotentry を実装 |
| `mail` | ✅ | ✅ 完了 | `mail_setting.yaml` bootstrap / 不完全設定 path 表示 / spinner / hotentry / `last_mail_date` 差分送信、Pony寄りの SMTP/TLS オプション受理、添付ファイル名の正規表現置換まで実装。`smtp` 経路は `tests/mail_e2e.rs` の end-to-end テストで sender 側・受信側ヘッダまで自動確認済み |
| `backup` | ✅ | ✅ 完了 | `narou backup`/複数 target、`backup/` 除外、180バイト切り詰めまで対応 |
| `clean` | ✅ | ✅ 完了 | `latest_convert` 既定値、`--all`、`--force`/`--dry-run`、freeze スキップ、`raw/*.txt|*.html` と `本文/*.yaml` の orphan 判定を実装 |
| `illust` | — (Rust 拡張) | ✅ 完了 | `.illustration_cache.yaml` 運用のための `narou illust <sub>`。`orphan`/`migrate`/`fix-ext`/`rebuild` に加え、挿絵を S3 互換ストレージへ写す `s3-push` / 突き合わせる `s3-verify` を実装。削除/改名/移行 (および `s3-push`) は既定 dry-run (`-f` で実行) |
| `author` | — (Rust 拡張) | ✅ 完了 | 追跡する作者を登録すると (`narou author add <作者ページURL>`)、`narou update` の後段で新しい作品を自動追加する。`add`/`list`/`remove`/`check`。作者ページの認識と作品一覧の取得はサイト定義 (`author_url` / `author_api_url` / `author_novel_pattern` / `author_work_url`) が担う。Pixiv は `preprocess:` の DSL が `author_novel::` / `author_series::` / `author_comic_series::` を emit し、シリーズに属する話/ページは `author_series_episodes_url` / `author_comic_series_pages_url` で除く (作品単位 = 単体小説 + 小説シリーズ + 漫画シリーズ + 単体イラスト・漫画)。`check --dry-run` は追加予定の URL だけを表示 |
| `login` | — (Rust 拡張) | ✅ 完了 | ブラウザ端末で `narou_rs_login` が取得したログイン Cookie の受け入れ側。`list`/`import`/`export`/`rename`/`order`/`clear` を実装。**サイトごとに複数の「名前つきログイン」を試行順つきで保持**し、1 ログインが複数ホストの Cookie を持つ (ブラウザのセッションがホストをまたぐため)。保存値は `.narou/login.key` (または `NAROU_RS_LOGIN_KEY`) の鍵で `enc:v1:` 暗号化され、書き出しファイルは `--passphrase` で Argon2id→XChaCha20-Poly1305 暗号化。Web UI 設定の「ログイン」タブと `GET /api/login`、`POST /api/login/import\|rename\|order`、`DELETE /api/login/{site}`、`DELETE /api/login/{site}/{index}` も対応 |
| `db` | — (Rust 拡張) | ✅ 完了 | SQLite 管理 DB の保守。`verify` / `export-yaml [--out\|--in-place]` / `vacuum` / `repair-freeze [--dry-run]`。既定は YAML 管理で、SQLite は Web UI 初回ツアーまたは `.narou/storage-backend` マーカーによる opt-in。`repair-freeze` は旧ビルドが残した `freeze.yaml.imported-*` から失われた凍結状態を復旧する |
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

**Rust 実装**: `src/commands/init.rs`。Ruby版同様、初期化時は `.narou/`・`小説データ/`・ユーザー編集用 `webnovel/` を作成し、`webnovel/*.yaml` を初回コピーする。`.narou/local_setting.yaml` や `queue.yaml` などの inventory ファイルは init 時に eager 生成せず、各機能が必要になった時点で作る。AozoraEpub3 / mail 用の preset は repo 直下 `preset/` に同梱し、`init` / `mail` が `sample/narou/preset` に依存せず自己完結で動くようにした。2026-04 の FS hardening で `--path` は `./AozoraEpub3` のような相対パスも一度 absolute/canonicalize へ展開してから検証・保存しつつ、UNC・drive-relative・`\\?\` 形式は引き続き拒否する。`:keep` は既存挙動のまま有効な保存済みパスだけを再利用する。

---

### 2. `download` — 🟡 部分

> 指定した小説をダウンロードします

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--force` | `-f` | flag | false | 全話強制再DL | ✅ |
| `--no-convert` | `-n` | flag | false | DLのみ、変換スキップ | ✅ |
| `--freeze` | `-z` | flag | false | DL後に凍結 | ✅ |
| `--remove` | `-r` | flag | false | DL・変換・送信後にDBのインデックスから削除（変換済みファイルは残る） | ✅ |
| `--mail` | `-m` | flag | false | DL後にメール送信 | ✅ |
| targets | | Vec\<String\> | — | URL/Nコード/ID/タイトル | ✅ |

**実装済み動作**:
- `-f`/`--force`: 全話強制再ダウンロード (`Downloader::download_novel_with_force`)
- `-n`/`--no-convert`: 変換スキップ
- `-z`/`--freeze`: DL完了後に自動凍結 (`--freeze` は `--remove` より優先)
- `-r`/`--remove`: DL+変換後にDBから削除（ファイルは残る）
- `-m`/`--mail`: 変換後に `mail` コマンドと同じ helper で送信。設定未作成時は `mail_setting.yaml` を生成し、注意メッセージも Ruby版に寄せた
- DL後の自動変換は `update` と同じ共通変換経路を使い、`convert.multi-device` / `convert.copy-to` / `convert.copy-to-grouping` を反映する
- 引数なしでインタラクティブモード (stdin から URL 入力、TTY時のみ)
- 凍結チェック: Ruby版と同じ `.narou/freeze.yaml` を優先し、移行互換として `frozen` タグも補助的に認識して凍結済み小説をスキップ
- ダウンロード済みチェック: 既存小説はスキップ（`--force`で上書き）
- DB に記録があって保存フォルダが無い場合も、ObjectStore の `toc.yaml` があれば登録済みとして扱う（SQLite のミラー無効運用）。保存済み TOC も無い場合だけ Ruby版同様に DB インデックスを削除し、再ダウンロード確認へ進む
- 新規 DL の 404・取得エラー・年齢制限キャンセル等で作品が未登録なら、その ID の予約を解放して次の取得へ再利用する。同じ一括 DL 内でも失敗だけで ID を進めない。使用中・登録済みの ID は再利用せず、既存作品や過去の欠番の番号は変更しない。Native YAML / SQLite と Worker D1 で同じ予約・確定・解放の契約を使う
- タグ展開: `tag:NAME` → 該当IDに展開、`^tag:NAME` → 補集合
- `tagname_to_ids`: ID優先、未登録はタグ名として展開
- `mistook_count` 追跡 → 終了コード反映
- 複数ターゲット間の水平線セパレータ
- 有効ターゲット検証: Nコード or URL（サイト設定の小説 URL / シリーズ URL / 作者・ユーザー URL に一致）
- Nコード指定時は `https://ncode.syosetu.com/<ncode>/` からURLキャプチャを作り、サイト定義の `\k<ncode>` を展開してDLする
- `webnovel/*.yaml` の `series_url` / `series_item_url` に一致するシリーズ URL は、共通の `application::download_targets::DownloadTargetExpander` で個別小説 URL に展開してから通常の download 処理に渡す。小説家になろう、R18 なろう、カクヨムのシリーズ/コレクション URL に対応。CLI の対話入力もシリーズ URL を受理する
- Worker の `/api/download`（`/api/download_force` を含む）と `/api/jobs` は、全対象のシリーズ・作者 URL 展開が成功してから作品ごとのジョブを登録し、元の `--force` 等を各作品へ引き継ぐ。取得失敗・空シリーズ一覧はエラー。正常に読めた空の作者一覧は追加なし。保存済みジョブ・旧 envelope の実行境界も同じ展開器を使う
- 作者・ユーザー URL の単発登録と CLI `author` の継続追跡は別機能。`Downloader::author_works` / `AuthorListing` が作品列挙・複数ログインの試行・ページ送り・シリーズ所属作品の除外を共有し、サイト固有の規則は YAML / preprocess DSL に置く。小説・シリーズの URL 判定を優先するため、Pixiv のシリーズを作者一覧として展開しない
- R18 なろうの作者はプロフィールの最新作だけでなく、`/mypage/novellist/xid/<作者ID>/` の全作品一覧を `author_api_url` から取得し、DSL / `author_next_pattern` で次ページを辿る。単発の一括取得と継続追跡で同じ列挙経路を使う
- 新規取得したセクションは解析済みの本文・前書き・後書きに加え、既定では raw HTML も `illust_grep_pattern` の検索対象にしてサイト固有マークアップ内の挿絵を先取り保存する。取得ページが空の本文枠と UI 画像だけの場合は、サイト定義の `illust_from_raw: false` で解析済み本文・前後書きだけに限定できる（アルファポリスで使用）。未更新セクションは従来どおり保存済み section のみを検索する
- サイト定義に `illust_grep_pattern` が無いサイト (なろう / R18なろう / カクヨム / Arcadia) は narou.rb `HTML#initialize` の既定 `<img>` パターン (`downloader::DEFAULT_ILLUST_GREP_PATTERN`) で挿絵を先取り保存する。既定パターンでは URL でない注記 (相対パス) は対象外にし、挿絵 URL の protocol-relative (`//host/...`) は本文のスキームで解決し、mitemin は取得用 URL (`viewimagebig` → `viewimage`) へ寄せる。挿絵が 1 枚も無い小説には空の `.illustration_cache.yaml` を作らない (2026-10-08 修正)
- 索引 (`.illustration_cache.yaml`) が知っている挿絵でも、`<prefix>/挿絵` の一覧に実体が無ければ取り直す (自己修復)。索引が空でないときだけ 1 小説 1 回一覧するので、通常の更新に余分な往復は付かない (2026-10-08 修正)
- ハーメルンのタグ抽出は `webnovel/syosetu.org.yaml` の詳細表・目次ページ両方のパターンで行い、短編ページのあらすじ・本文をタグとして取り込まない
- Pixiv (`webnovel/www.pixiv.net.yaml`) は小説 / 小説シリーズ / イラスト・漫画 (`/artworks/A`) / 漫画シリーズ (`/user/U/series/S`) の 4 種を扱い、ncode は `n`/`s`/`a`/`c` + ID。イラストは 1 話・本文がページ画像のみ、漫画シリーズは各作品を 1 話とする連載。取得先 API は `toc_url` の `by_target` (ターゲット URL ごとのテンプレート) で切り替える
- Pixiv (`webnovel/www.pixiv.net.yaml`) は `/ajax/*` の JSON を `preprocess:` DSL で中間テキスト化して取得する。単体作品 URL (`/novel/show.php?id=N`) は短編 (novel_type 2) として 1 話、シリーズ URL (`/novel/series/S`) は目次 30 話ずつの複数ページ取得で全話を登録する。シリーズの 1 話 URL はその話だけを単体作品として登録する
- Pixiv 小説シリーズのタグは、シリーズ自身と全話の目次メタデータから収集する。後続ページや後半の話だけに付いたタグも初出順に重複除去し、`auto-add-tags=true` なら本文を再取得しない更新でも追加する。既存の手動タグは保持する。サイト固有の抽出は YAML (`version: 1.4`)、ページ間の収集・統合は共通 downloader が担う。
- Pixiv の ncode はサイト定義の `ncode:` キーで `n` + 数値 (小説) / `s` + 数値 (小説シリーズ) / `a` + 数値 (イラスト・漫画) / `c` + 数値 (漫画シリーズ) を組み立てる。URL の数値だけでは作品種別をまたいで衝突するため
- なろう式の ncode 判定 (`n\d+[a-z]+`) に当たらない ncode は、タイトル一致が無いときに ncode 一致で解決する (`n29204764` / `s16299140` などを `update` / `convert` の対象に指定できる)
- Pixiv の挿絵 (`[pixivimage:]` / `[uploadedimage:]`) は DSL の `fetch_json` で画像 URL を解決し (`[uploadedimage:]` は同じ応答から解決)、`illust_grep_pattern` が `挿絵/` へローカライズする。解決できなかった参照は `<!--...-->` の目印だけ残す
- Pixiv のアニメーション挿絵（うごイラ）はフレーム ZIP として取得され、`illustration-animation` feature（native と Worker の両方で有効）で APNG に組み立てて `挿絵/` へ保存する。`zip` は wasm でも動くよう純 Rust の deflate バックエンドに絞ってある
- アルファポリス (`webnovel/www.alphapolis.co.jp.yaml`, version 1.4) は小説トップ・各話 URL を作品トップへ正規化し、埋め込み JSON の配列順で無料公開・レンタル済みの話を取得する。未レンタル話は部分一覧として検出し、保存 Cookie で閲覧可能な話が増えれば採用、制限が残れば取得範囲を警告する。章名は最初に閲覧できる話へ付け、全話が閲覧不可の章は出力しない。題名、作者、あらすじ、タグ、更新日時、文字数、完結状態を抽出し、本文はセッション Cookie と CSRF 付きフォーム POST で取得する。R18 は `over18` の同意に従い、電子書籍特典や CAPTCHA は本文として保存しない。サイト固有の処理はすべて YAML / DSL に置く
- カクヨムネクスト (`webnovel/kakuyomu.jp.yaml`, version 2.6) は目次の `EmptyEpisode` を `login_partial_pattern` で検出し、保存済みログインを順に試す。制限が残れば取得範囲を警告して無料分だけを取得し、後から閲覧権のある Cookie を保存して更新すると残りも取得できる。採用したセッションは本文取得と次回更新でも使い、通常の公開作品には Cookie を送らない。本文 URL が作品トップ等へ戻され、本文枠が無い場合も DSL で失敗させ、空本文をキャッシュ・保存しない。サイト固有の判定は YAML / DSL に置く
- 部分一覧に保存済みの話が欠ける場合は、話の削除やダイジェスト化とは判定せず `Canceled` で更新を見送り、目次・本文を保持する（force 時も同じ）。閲覧できる話が 0 件の部分一覧も空の小説として登録しない。この保存境界と警告は全サイト共通の downloader が担う
- サイト定義の `min_interval:` でサイトごとの最低リクエスト間隔 (秒) の下限を宣言でき、該当サイトのレートリミットスコープにだけ適用される
- サイト定義の `headers:` キーで任意のリクエストヘッダを宣言できる (Pixiv は画像ホスト用に `Referer`、ハーメルン R18 は Cloudflare challenge 回避用に `Sec-Fetch-*` を指定)。値は `\k<...>` 補間され、危険な名前・値は無視される
- サイト定義の `cookie_hosts:` は、サイトの `domain` とは別に Cookie を送ってよい信頼済みホスト名の配列（各ホストのサブドメインも含む）。R18 なろうは作者用 `xmypage.syosetu.com` を指定する。未指定の第三者ホストには Cookie を送らない
- サイト定義の `cookie_session: true` は取得ポリシー内だけの匿名 Cookie セッションを有効にする。応答の Cookie を同一 origin・Path・Secure・有効期限の範囲で次の送信に使い、保存済みログインへ書き込まない。既定は無効
- リダイレクトを自前で辿るモード (`resolve_final_url`) も curl ティアを先に試す。CDN challenge 下のホストでは reqwest が 403 でも libcurl が 200 を返すことがあるため
- `preprocess:` DSL は `request(url)` / `fetch_json(url)` で追加取得を要求できる。実行側がサイトの取得ポリシー経由で取得し、定義を再実行する (最大 4 ラウンド)。結果は `fetched["<url>"]`、失敗は null。ジョブは URL 単位で、結果は 1 小説分だけ保持する
- `preprocess:` DSL の `extract(/.../)` は最初の capture を文字列として返す。`post_form(url, [[name, value], ...], [[header, value], ...])` は同一 origin へのフォーム POST を要求し、再実行時に本文文字列を返す（未取得は null）。第3引数は省略可能で、Cookie / Authorization 等の注入は拒否する。フォーム結果は1回の前処理だけ保持し、同じ URL でも本文・ヘッダが違う要求を区別する。取得失敗はエラーとなり、`fail "理由"` でも前処理を失敗させられる。失敗時に途中の書き換えを公開しない
- 保存済みログイン Cookie のフォールバックを実装。TOC 取得がログイン壁 (404 やサイト定義の `login_pattern`) / 部分一覧 (`login_partial_pattern`) に当たったとき `narou login` の資格情報を試行順に試し、成功した資格情報の ID を小説レコードの `login_session` に保存して次回から最初のリクエストで使う。取得側の `narou_rs_login` は親ドメインの Cookie も拾い、サイト全体で使える資格情報として保存する。`update` でも同じ経路を通る
- Workers は年齢認証を対話で確認できない。成人向け作品の取得を許可する場合は Web UI の「環境設定 → Global」で `over18=true` を保存する。`false` は明示的な拒否であり、設定変更で既に `blocked` になったジョブを自動再開はしない
- 話ごとの進捗行・章/節の見出し・ログイン再試行・「小説が削除されているか非公開な可能性があります」などのレポート行は、コンソールと同時に `logging` のログファイルへも出力する。narou.rb は `$stdout` への write をすべて `append_log` するため、ログに残らない行があってはならない。並列ドメインワーカー間の行混線は sink 内の排他 (`STDOUT_LOCK`) で防ぐ。原因は `src/logger.rs` が bin と lib の両方にコンパイルされて `LoggerState` が 2 つ存在し、`logger::init()` が bin 側しか初期化していなかったこと。`MessageSink` (`progress::console_sink`) 経由の行は未初期化の lib 側ロガーへ書かれていた。bin は lib のロガーを共有する (2026-10 修正、issue #36)

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
- `--gl [OPT]`: なろう API バッチで `general_lastup` を更新し、API が title を返さない/返せない小説は Ruby版同様 individual TOC 取得へフォールバックする。`modified` タグは Ruby版に合わせて `novelupdated_at` が手元の `last_check_date` / `last_update` より新しい時だけ付与し、修正が無い時は外す。`novelupdated_at` 未取得サイトは従来どおり `general_lastup` 差分ベースを維持する。OPT省略=全、`narou`=なろうAPI対応のみ、`other`=非なろうのみ
- `-f`/`--force`: 凍結小説も更新
- `-s`/`--sort-by KEY`: 更新順ソート（設定 `update.sort-by` にも対応）。有効キーは `db::sort_keys` (`src/db/sort.rs`) の `id`, `last_update`, `general_lastup`, `last_check_date`, `title`, `author`, `sitename`, `novel_type`, `tags`, `general_all_no`, `length`, `status`, `toc_url`, `new_arrivals_date`
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
- `update.interval` 設定対応（最低2.5秒、YAMLの数値/文字列を許容）。直近の更新開始時刻をサイトドメイン単位で保持し、同一ドメインへの連続更新時だけ待機する。Web UI などの直列更新でも異なるドメイン間では待機時間を引き継がず、ドメイン不明の小説は共通の安全側バケットとして扱う
- `update.strong` 設定対応。同日更新時は保存済み `本文/*.yaml` の本文要素と取得本文をハッシュ比較し、実質同一なら更新扱いにしない
- Ruby版同様 `.narou/section_hash_cache.yaml` を永続化し、strong update 時の既存 section digest を再利用する
- カクヨムは各話の `publishedAt` を初回掲載日時、`editedAt`（無い場合は `publishedAt`）を `subupdate` として扱い、本文改稿のみの通常 Update でも対象話を更新する
- `update.convert-only-new-arrival` 設定対応（YAMLの真偽値/文字列/数値を許容）
- `last_check_date` 追跡
- `download.choices-of-digest-options` 設定対応。Ruby版と同じ 1-8 のダイジェスト化選択肢を処理し、キャンセル・凍結・バックアップ・あらすじ表示・ブラウザ起動・保存フォルダ起動・変換を実行
- ダイジェスト化キャンセル時は `UpdateStatus::Canceled` を返し、`update` / `download` コマンド側でRuby版相当のキャンセル表示と終了コード加算を行う
- 差分更新時は Ruby版同様 `本文/cache/<timestamp>/` に旧sectionを退避し、差分が無い場合は空cacheディレクトリを削除
- download/update 共通の差分退避は旧目次のファイル名を参照する。各話の題名変更でも SQLite の旧本文を `本文/cache/` へ保存し、新題名で更新した後の差分なし再更新・章題変更も処理できる（issue #32）。保存先を `diff` が参照する Ruby版互換の `cache/` に統一
- `SuspendDownload` 発生時は通常失敗ではなくバッチ全体の中断として扱うように修正
- `auto-add-tags` 設定対応。site YAML の `tags` パターンから取得したタグをDBタグへ自動追加
- `hotentry` / `hotentry.auto-mail` 設定のうち、hotentry の新着話収集・統合テキスト生成・device に応じた変換・`copy-to`・端末送信・mail までは実装済み
- `confirm_over18?` 相当として、R18 サイトで承諾した場合は global `over18: true` を保存し、以後は再確認しない。`over18: false` を明示設定した場合は Ruby版同様に再確認せず中止し、ハーメルンは site YAML の R18 マーカー検出でも同じ分岐を通す
- ソートキーバリデーション（不正キーでエラー+終了コード127）
- `setting update.sort-by` の select 値を Ruby版 `Narou::UPDATE_SORT_KEYS` と同期済み
- 小説間インターバル（Ruby版 `Interval` クラス互換）
- `update.max-parallel-domains` 設定対応（既定4）。対象小説をサイトドメイン別にグルーピングし、ドメインごとにワーカーを割り当てて並列にダウンロードする。同一ドメイン内は常に直列のまま処理されるため対サイト礼儀は崩れない。Web UI でも操作ごとの進捗を分離して並列処理する。1で従来通りの逐次動作、フォース指定・ドメインが1種類しかない時は自動的に逐次処理にフォールバック
- `update.disabled-author-tracking` で、更新終了後の追跡作者からの新作品追加を停止できる。CLI と native Web UI の共通設定一覧から読み書き可能。Worker は永続的作者追跡の実行経路を持たないため、この設定を無効表示し API 保存も拒否する
- 全件更新時の凍結スキップ、個別指定時の凍結メッセージ
- 終了コード: エラー数（最大127）、中断時126
- Ctrl+C 割り込み時はフラグを検知して `アップデートを中断しました` を表示し、終了コード126で終了
- `--all` は Ruby版に存在しないRust独自オプションだったため削除
- ログイン Cookie のフォールバックは `download` と共通の downloader 経路を通るため、保存済みの `login` 資格情報によるログイン壁/部分一覧の再取得は update でも動く
- 失敗した小説は downloader 側が「小説が削除されているか非公開な可能性があります」を出し、コマンド側は行を出さない (Ruby版と同じ)。この行がログに残るので、`n 件のエラーが発生しました` の内訳をログから追える (2026-10 修正、issue #36)

**完了扱いにしない理由 / 不足動作**:
- Ruby版の詳細表示・hotentry後処理など、周辺出力/イベント処理の細部は追加突合が必要

---

### 3.x `db` — 追加サブコマンド (narou.rs 独自, Ruby版対応外)

SQLite 管理データベースの保守。**0.4.0 既定は YAML 管理のまま**で、Web UI 初回ツアー(または `.narou/storage-backend` マーカーファイル=`sqlite`)で選択したときのみ有効化される。Web API は `GET/POST /api/storage/mode`。

| サブコマンド | 内容 |
|---|---|
| `verify` | `PRAGMA integrity_check` + 全 payload の CRC-32 検査。取込済み `freeze.yaml.imported-*` があれば `repair-freeze` を案内する |
| `export-yaml [--out DIR]` | 管理状態に加え、本文・個別設定・ユーザーサイト定義・挿絵をレガシー配置で書き出す。元の管理方式は変更しない |
| `export-yaml --in-place` | 同じ内容を実位置へ復元してから `storage-backend` を `yaml` に戻す。読み取り・CRC・保存・S3 取得が失敗した場合は SQLite のまま維持。共有 global_setting は通常のファイル管理を維持 |
| `vacuum` | VACUUM で容量回収 |
| `repair-freeze [--dry-run]` | 旧ビルドが残した `freeze.yaml.imported-*` を全て合算し、保存済みの凍結状態に無い ID を凍結へ戻す（和集合のみで、解除はしない）。`--dry-run` は復旧予定の表示のみ。取り込んだファイルは削除しない |

**復帰失敗時のデータ保全**: インプレース書き出しの管理ファイルは、保存前に SQLite へ SHA-256 指紋を記録し、原子的に置換する。`narou-compat=false` では、失敗後に残った未編集の書き出しファイルを次回起動で再取り込みせず、その後に正常保存した SQLite の状態・作品レコードを維持する。ユーザーが編集して指紋が変わった旧形式ファイルの取り込みと、`narou-compat=true` のファイル優先は維持する。
既存の書き出しファイルと次に公開する内容の指紋を最大 2 件保持し、再試行が保存直前で停止しても前の残骸を再取り込みしない。指紋更新と公開は同じファイルロック内で行う。`--out` が実管理ディレクトリ `.narou` を指す場合も同じ保護を適用する。

**凍結状態の保存先**: YAML 管理では `.narou/freeze.yaml`、SQLite 管理では `app_state('inv','freeze')` で、どちらも Inventory の共通入口 (`Inventory::update_yaml`) を通す。SQLite 側では同じ内容が `frozen_novels` へ投影され、状態の検索 (`status` 検索式) が `narou list` と一致する。`freeze.yaml` の取込は和集合で行うため、残っていた断片ファイルが他の凍結を解除することはない (issue #35)。

**前方互換モード**: `narou setting narou-compat=true` で `.narou/*.yaml` (database.yaml, freeze.yaml, alias.yaml, tag_colors.yaml, login_cookie.yaml, latest_convert.yaml, local_setting.yaml, queue.yaml, notepad.txt) をファイルとして維持し、narou.rb がそのまま読める状態を保つ。ファイルが正で SQLite はミラー。OFF(既定) ではファイルを `*.imported-*` へ退避し SQLite のみで管理する。`~/.narousetting/global_setting.yaml` はライブラリ状態ではないため narou-compat/SQLite の有無にかかわらず常にファイルのまま維持され、SQLite への取込・退避は行わない (旧ビルドが `app_state` の global/global_setting に残した行は、ファイルが無いとき初回読み出しでファイルへ書き戻してその行を削除する)

---

### 3.x `author` — ✅ 完了 (narou.rs 独自, Ruby版対応外)

作者ページを登録しておくと、**まだ管理していない作品**を通常のダウンロード経路で追加する。Native は `narou update` の後段、Workers は更新 API と定期実行から継続して確認する。小説がまだ 0 件のライブラリも対象になる。

| Native CLI | 内容 |
|---|---|
| `add <URL>` | 作者ページを登録。`author_url` に一致するサイト定義が必要 |
| `list` | 登録済みの作者（サイト / ページ URL） |
| `remove <URL\|番号>` | `list` の 1 始まりの番号か URL で追跡解除。0 は拒否し、取得済み作品は削除しない |
| `check [--dry-run]` | 全作者を確認して未管理の作品を取得する。`--dry-run` は追加予定の URL だけを表示し、取得しない |

**保存形式**: 小説とは別の inventory `author`（Native は SQLite `app_state` / `.narou/author.yaml`、Workers は D1 の `app_state(scope='inv', key='author')`）に、**作者ページ URL（ユニーク）→ サイト名** だけを持つ。名前や日時などの帳簿は持たない。登録・解除は Native の Inventory 原子更新、Workers の全スナップショット CAS で保存し、別の登録操作を上書きしない。

**サイト定義**: `author_url`（作者ページを認識する正規表現）/ `author_api_url`（作品一覧の取得先の雛形。任意）/ `author_novel_pattern`（作品を抜く正規表現。`novel_url` か、`author_work_url` 用の capture）/ `author_work_url`（capture から作品 URL を作る雛形）/ `author_next_pattern`（ページ分けされた一覧の次ページ。ハーメルン・アルファポリス等で使用。終了は次ページが無いか訪問済みに戻ったとき）。

**共通取得**: `author add/check` も現在の実効サイト定義を使う。作品一覧の取得・認証・シリーズ所属作品の除外は CLI / Worker の単発 URL 展開と共通。offset 型 API は `author_page_url` / `author_page_size` / `author_page_total_pattern` / `author_page_max_start` でページ送りする。なろう API の公式開始位置上限を越える場合や総件数に届かない場合は、途中までの一覧を成功扱いしない。

アルファポリスの作者一覧は作品タイトル欄からサイト内の小説だけを列挙する。漫画、外部登録、プロフィール内リンクは除外し、現在ページ直後のページリンクを辿る。単発取得と継続追跡で同じ定義を使う。

**Workers の継続追跡**: `/api/update` と Update ジョブの投入、および定期更新の最終ページから、作者ごとの `author_check` ジョブを投入する。実効サイト定義・Cookie・待機ポリシーは単発取得と共通で、登録済み作品は TOC URL の索引で除外する。`update.disabled-author-tracking=true` で自動確認を止められるが、作者タブの「今すぐ確認」は実行できる。解除済み作者の古い確認ジョブはスキップし、一覧取得後も登録を再確認する。すでに通常の Download ジョブとして投入された作品は、作者の解除だけではキャンセルしない。

**CLI の対象**: `narou author` は Native のローカルライブラリ専用。Workers の作者管理は Web UI から行い、Native CLI から Workers へ接続する機能は提供しない。

```sh
narou author add https://mypage.syosetu.com/2842627/
narou author list
narou author check --dry-run
narou author remove 1
```

**Web UI / API**: Native・Workers 共通で「環境設定 → 作者」から登録・一覧確認・追跡解除・今すぐ確認ができる。このタブは即時保存で、「設定を保存」は不要。`GET /api/authors` は `{success, authors:[{site,url}]}`、`POST /api/authors` と `DELETE /api/authors` は本文 `{url}`、`POST /api/authors/check` は `{dry_run}` を受け付ける。

**単発取得との区別**: 通常のダウンロード欄や `download` に作者 URL を入れる操作は、その時点の作品一覧の取得だけであり、追跡登録は増やさない。継続追跡には `author add` または作者タブでの登録が必要。

---

### 3.y `login` — ✅ 完了 (narou.rs 独自, Ruby版対応外)
ブラウザのある端末とダウンロード実行ホストが別であることを前提にしたログイン情報管理コマンド。取得側は別実行ファイル `narou_rs_login` が担当し、本コマンドは受け入れ・書き出し・一覧・登録・追加・並べ替え・削除を行う。

| サブコマンド | 内容 |
|---|---|
| `list` | サイトごとにログインを番号・名前・短縮 ID・ホスト数つきで表示 (Cookie 値は `name=…` に伏せる) |
| `import <file> [--passphrase P] [--replace] [--name N]` | `narou_rs_login --export` の書き出しファイル (YAML) を取り込む。`--name` でそのファイルが持ち込むログインに名前を付ける (`本垢` など)。`--replace` で取り込みに含まれないサイトを削除 |
| `export <file> [--passphrase P] [--clear-text]` | 保存済み情報を書き出しファイルへ出力。`--passphrase` 指定時は Argon2id→XChaCha20-Poly1305 で暗号化 |
| `rename <site> <番号> <名前>` | ログインに名前を付ける (`""` で名前を消す)。番号は `list` の 1 始まり |
| `order <site> 2,1,3` | 現在の位置 (1 始まり) を新しい試行順に並べ替える。件数・重複は検証 |
| `clear [site] [--index N]` | 1 サイト分 / `--index` でそのサイトの 1 件だけ / 引数なしですべて削除 |

Cookie の直接登録 (`set`/`add`) は廃止した。登録経路は `narou_rs_login` のブラウザ取得と書き出しファイルの取り込みだけで、利用者は名前と順序を管理する。

**取得用 bin**: `narou_rs_login` は小説・作者・トップページの URL、サイト名、親ドメインも受理し、YAML の `login_url` を開く。アルファポリスは `narou_rs_login alphapolis.co.jp --name 本垢` で取得できる。対象作品 URL を渡した場合の保存後確認も共通の取得・YAML 前処理を通し、HTTP エラーやログイン壁を成功表示しない。

カクヨムは `narou_rs_login kakuyomu.jp --name NEXT`、または対象作品 URL の指定で `/auth/login` を開く。NEXT の有料部分にはアカウントの閲覧権も必要で、ログイン済みというだけで取得可能とは判定しない。未ログイン・権利なしでは警告付きで無料分を取得し、保存済みの話を欠く部分一覧では更新を見送って既存データを保持する。

**保存形式**: `login_cookie` inventory (SQLite `app_state` / `.narou/login_cookie.yaml`) に、**1 サイト = 順序つきログイン配列** (`LoginGroup { id, site, label, cookies: [{ host, cookie }], added_at }` の JSON 配列) を `enc:v1:<nonce>:<payload>` として暗号化保存。並び順がそのまま試行順になる。鍵は `.narou/login.key` (初回作成、Unix では 0600) または `NAROU_RS_LOGIN_KEY` (base64)。在庫のキー (サイト名) を AEAD の associated data に束ねるため別サイトへの流用は不可。旧形式の平文値はそのまま読め (ホスト名で束ねた旧暗号文も可)、次回保存時にサイト単位の新形式へ移行する。

**1 ログイン = 複数ホスト (取り込んだ 1 ファイル = 1 ログイン)**: ブラウザのセッションは `pixiv.net` と `www.pixiv.net` のように複数ホストにまたがるため、取得側はサイトのドメインファミリーを 1 つのログインにまとめて保存する。送信時は `merged_cookie()` が各ホストの Cookie を 1 本の `Cookie:` ヘッダに畳み、名前が衝突したときは具体的なホストを優先する。まとめ先のサイト名はサイト定義 (`webnovel/*.yaml`) のドメインで決め、定義が無ければホストを使う (親ドメインのキーは配下の定義があればそこへ寄せる)。

ドメインファミリーの収集・サイト名への畳み・親ホストの検索は Public Suffix List を使い、`co.jp` のような複数ラベルの suffix を別サイトの共有範囲と誤認しない。アルファポリスの `alphapolis.co.jp` / `www.alphapolis.co.jp` は 1 サイトへまとめ、無関係な `*.co.jp` や suffix 自体は同じログインへ入れない。

旧形式 (版 1 の host→cookie、版 2 のホストごとの一覧) は読み取り時に**ホストごとに並べ直してから 1 ログインへ畳み直す**ので、1 ファイル = 1 セッションのまま入る (版 2 の同じ位置は同じアカウント)。過去にホストごとに分解されたまま保存されたデータも、読み込み時に「ホストが重ならず Cookie 名も衝突しない」ログイン同士を 1 つに畳んで書き戻す。

**セッション ID**: 各ログインに UUID を振り（保存値に含める）、小説レコードは `requires_login` に加えて `login_session`（成功したログインの ID）を持つ。フラグ付きの小説は次回以降その ID のログインを最初のリクエストから送るため、一覧の総当たりをしない。ID の無い旧データはストア読み込み時に採番・保存される。

**試行順の使われ方**: ダウンロード時、ログイン壁 (404 / `login_pattern`) か部分一覧 (`login_partial_pattern`) のときに保存済みを**順に試す**。ログイン壁は成功した時点で、部分一覧は「欠けが消えた／取得話数が増えた」時点で打ち切る。採用したログインはその後の本文取得にも使う。`Set-Cookie` の書き戻しは、その応答で実際に送ったログインだけを更新する (別アカウントのセッションを壊さない)。

ログイン画面を返す初回 GET の再利用も同じ再試行処理へ渡す。HTTP 200 のままログイン壁が残る資格情報は採用せず、資格情報が無い／全件失敗なら題名なし・0 話の小説を作らない。アルファポリスの未レンタル話は YAML が `login_partial_pattern` で示し、保存 Cookie を試した後も制限が残れば無料公開・レンタル済みの話だけを取得する。後からのレンタルは通常の更新で追加でき、保存済みの話が欠ける部分一覧では既存データを保持する。

**データ管理方式 (YAML / SQLite)**: 設定ページの WEB UI タブに「データ管理方式」があり、現在のモード表示・SQLite 管理への移行・YAML 管理への復帰ができる。API は `GET/POST /api/storage/mode` (実装 `src/web/storage.rs`、`narou db export-yaml --in-place` と同じ書き出しを先に行う)。`NAROU_RS_LEGACY_YAML=1` のときは固定され、API は `locked_by_env: true` を返す。

**書き出し形式**: `version`/`exported_at`/`library`/`encrypted`/`kdf`/`salt`/`payload`/`sites` を持つ YAML エンベロープ (version 3)。`narou_rs_login --export <file>` が生成し、ライブラリ外ではそれが既定の出力になる。version 2 (`credentials:` にホストごとの 1 本) と version 1 (`cookies:` のホスト→Cookie マップ) も読み取り可能。

**ヘルプ**: トップレベル一覧と `narou login -h` は実装と同期している。`-h` の `<sub>` 一覧は `list`/`import`/`export`/`rename`/`order`/`clear` を、Examples は `rename`・`order`・`clear --index` を含む主要形を、オプション一覧は `--passphrase`/`--replace`/`--clear-text`/`--name`/`--index` を表示する。
**Web UI**: 設定ページ「ログイン」タブで一覧 (サイト → 名前つきログイン)・取り込み (名前欄つき。空欄ならファイル名を使用)・名前変更・並べ替え (上下ボタン)・1 件削除・サイト削除・全削除。取り込みはファイル選択 (FileReader) と貼り付けの両方に対応。API: `GET /api/login`、`POST /api/login/import`、`POST /api/login/rename`、`POST /api/login/order`、`DELETE /api/login`、`DELETE /api/login/{site}`、`DELETE /api/login/{site}/{index}`。

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
| targets | | Vec\<String\> | — | ID/タイトル/ファイルパス/標準入力 | ✅ |

**不足動作**:
- 変換後の端末送信の実機最終検証

**Rust 実装メモ**:
- HTML の `&nbsp;` は narou.rb と同じ通常スペースへ復元する（本文・前書き・後書き・あらすじの合成入力で回帰確認）。
- `sqlite.mirror-files=false` の一時取り出しは全ページを走査し、4,096 件を超えるオブジェクトも後片付けの対象にする。既存の実ファイルと挿絵は変更しない。
- CLI も `setting.ini` / `replace.txt` を取り出してから個別設定を読み、挿絵索引を実効 ObjectStore に保存する。ミラー有効時の復元ファイルは残し、無効時の取り出しファイルだけ最後の利用者が解放した時点で消す。挿絵を含む同期 I/O は既存 Tokio 文脈でも二重 runtime にしない。
- 個別 `setting.ini` は Native CLI / Web の変換子プロセスでも実効 ObjectStore から読み、SQLite 移行前の実ファイルが残っていても正準保存値を優先する。`novel_title` / `novel_author` を含む再編集・設定解除を反映し、読み出しのために既存ファイルを上書き・削除しない。YAML 管理とライブラリ外の読み出しは従来どおり（issue #37）。
- 挿絵の一時取り出しが途中で失敗した場合も、新規作成した完全／不完全ファイルを片付け、再試行時に不完全な画像を再利用しない。
- 半角カナは Ruby版 NKF 相当の対応表と濁点・半濁点合成で全角化し、ルビの読みも `ﾛｰﾙﾌﾟﾚｲﾝｸﾞｹﾞｰﾑ` → `ロールプレイングゲーム` と変換する。半角句読点にも対応し、全角英数字や互換文字は変更しない（issue #31）
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
- `convert.add-dc-subject-to-epub=true` 時は DB タグから `convert.dc-subject-exclude-tags` を除いた値を EPUB 内 `standard.opf` の `<dc:subject>` へ埋め込む。DB lookup のみで判定するため小説 ID `0` でも Ruby版同様に tags を拾える。除外設定未作成時は Ruby版同様 `404,end` を自動保存し、`sample\\novel` で `alpha` タグだけが埋め込まれ `end` は除外されることを確認済み
- direct `convert` 後の端末送信は従来の compat 経路と同じ `send_file_to_device` helper へ揃えた。`sample\\novel` の `device: epub` 経路では副作用なく通ることを確認したが、Kindle/Kobo/Reader 実機での最終検証までは未了
- `convert.copy-to` / `convert.copy_to` と `convert.copy-to-grouping` の device/site グルーピングも direct convert へ接続し、`sample\\novel` で `device=epub` + `copy-to-grouping=device,site` のコピー先生成を確認済み
- `convert.copy-zip-to` も direct convert へ接続し、`sample\\novel` で `--make-zip 3` 実行時に ZIP のコピー出力を確認済み
- `convert.multi-device` があれば `device` より優先して複数端末へ順に変換する。Ruby版同様 `kindle` を先頭へ寄せ、無効な端末名は警告し、`sample\\novel` で `convert.multi-device: epub,ibunko` により EPUB + ZIP 出力を確認済み
- `--ignore-default` / `--ignore-force` を `NovelSettings::load_for_novel_with_options` に渡し、`default.*` / `force.*` の適用を個別に無効化できるようにした
- DB 管理小説だけでなくファイルパス指定の textfile 変換も `commands::convert` に接続し、`--enc` による UTF-8 / Shift_JIS / EUC-JP 系のデコードと `enable_enchant_midashi` 推奨 INFO を追加した
- `report.txt` の互換監査で出た変換差分のうち、ローマ数字変換、分数/日付変換、明示設定時の漢数字+単位変換、`disable_alphabet_word_to_zenkaku`、root `replace.txt` の追加適用、Kindle 向け矢印/ZWS、iBooks 章見出し前 6 改行を実装済み。既存のカクヨム byte-for-byte fixture は維持している
- `narou list ... | narou convert` のようなパイプ入力に対応し、非TTYの標準入力から空白区切りの target を読み取って CLI 指定 target の末尾へ追加する
- 変換後に `調査ログ.txt` を常に保存し、`enable_inspect` が有効なときは行末読点状況とカギ括弧内改行状況を記録する
- `--inspect` 指定時は full display、未指定時は Ruby版同様に summary だけを出す
- `enable_erase_introduction` / `enable_erase_postscript` を section 変換に反映し、`enable_auto_indent` は Ruby版 `Inspector#inspect_indent` 相当の比率判定でのみ有効化する
- 表紙タイトル生成で Ruby版 `decorate_title` 相当を実装し、`enable_add_date_to_title` / `title_date_format` / `title_date_align` / `title_date_target` / `enable_add_end_to_title` を反映する。`$t` / `$s` / `$ns` / `$nt` / `$ntag` の拡張書式、`general_lastup` / `last_update` / `new_arrivals_date` / `convert` の日付対象、完結タグによる ` (完結)` 付与も対応
- Rust 拡張設定 `enable_strip_title_prefix` で、タイトル先頭に連続する `【…】` / `《…》` / `〈…〉` / `［…］` / `[...]` を除去可能（既定 `false`）。除去後の値を `database.yaml` / `toc.yaml` の `title` として一覧表示、作品内タイトル、出力ファイル名に統一し、取得元の値は `database.yaml` の追加フィールド `raw_title` に内部保持する。旧 Rust 版でも未知フィールドとして round-trip 保存される。小説フォルダ名は設定後に新規取得した作品だけ除去後タイトルで作成し、既存作品の `file_title` とフォルダ名は維持する。raw の告知だけが変化して除去後タイトルが同じ場合はタイトル更新として扱わない
- HTML `<img>` の挿絵は `.illustration_cache.yaml` に `source URL` / `mitemin ID` / `SHA-256 hash` の対応表を保存して再利用する。mitemin は Ruby版互換のため `挿絵/i618380.<ext>` のような ID 名を本文に出し、ID と hash の対応を保持する。mitemin 以外は同一バイト列なら `挿絵/<sha256>.<ext>` の hash 名で重複排除する。作品の挿絵 store 起動時は cache と `raw/*.html` の `<img src>` を使い、既存の hash 名や `section-index-count` 名の mitemin 画像を `iNNNN.<ext>` へ移行する。`n3352gq` で発生した `section-index-count` 名による重複保存・EPUB肥大化を修正し、古い section 変換 cache は `illustration-localization:v4` で無効化する。downloader の `illust_grep_pattern` 先取り保存も同じ store を使う。挿絵の実体はダウンロード時に取得するため、サイト定義に `illust_grep_pattern` が無いサイトは既定の `<img>` パターンで取得する (2026-10-08 修正)
- 前書き・後書き中の挿絵注記は Ruby版 `Helper.extract_illust_chuki` / `novel.txt.erb` と同様に作者コメント装飾の外へ分離する。AozoraEpub3 が単ページ画像として回転・再圧縮できるようになり、`n1980en`（592話・挿絵注記115件）の同一画像入力では Rust 版 EPUB 55,656,277 bytes、Ruby版 55,655,378 bytes までサイズ差が縮小した
- HTML 由来の story / section では Ruby版同様に `()` の暗黙ルビ推測を行わず、HTML `<ruby>` は `to_aozora` 経由の明示ルビとして保持する。`text` / `text/plain` / textfile では従来どおり `()` 暗黙ルビを処理する
- Windows の `\\?\\C:\\...\\AozoraEpub3.jar` 形式パスは Java classpath にそのまま渡すと失敗するため、Ruby版同様に jar の basename を current_dir 基準で渡すよう修正した。`sample\\novel` で `device=epub` 実変換と `--no-epub` 抑止を確認済み
- Windows で `〜` / `～` / `−` / `‼` / `⁇` / `⁈` / `⁉` / variation selector や CP932/Windows-31J 未定義文字 (`♠` / `♡` / `♢` / `♣` / `𠮷` など) を含み、Java/AozoraEpub3 側で出力名がずれやすい小説パスは、AozoraEpub3 に本文・表紙・`挿絵/` を安全な一時ファイル名で渡し、生成後に本来の Unicode ファイル名へ戻す。`C:\Users\rumia\Documents\Narou` の n5853lh で EPUB 生成を確認済み
- AozoraEpub3-JDK21 (1.6.x) は出力ファイルが `-dst` の **実パス** 配下かを検査するため、`-dst` が junction / シンボリックリンク / 8.3 短縮名 を含む形だと、実際には同じ場所でも `java.io.IOException: 出力パスが許可されたディレクトリ外です` で失敗する (検査は出力ファイルが未生成の時点で字句的に正規化したパスと比較されるため)。narou は `-dst` と入力パスを実パスへ解決してから渡す (一時ワークスペース使用時・通常出力時とも)。一時ワークスペースが `%TEMP%` 配下にある場合も同じ理由で失敗していた
- `lite` feature 有効時は、外部 AozoraEpub3 が見つからない場合に組み込み `epub_lite` エンジンへフォールバックする (設定 `convert.epub-engine` = `auto`/`lite`/`external` で選ぶ。設定ページの「一般」タブからも切替可。1 回だけなら環境変数 `NAROU_RS_EPUB_ENGINE=lite`/`=external` で上書き)（外部ツール (jar / `AozoraEpub3_Lite.exe`) が見つかればそちらを優先する。narou.rb と同じ挙動）。`device=epub` / `kobo` / `reader` / `ibooks` / `mobi` の中間 EPUB 生成を外部ツールなしで行える。**Java 版と同じ資産と組み立て**: `aozoraepub3dir` があれば `chuki_*.txt` (narou カスタム注記込み)・`gaiji/*.ttf`・`AozoraEpub3.ini` を読み、無ければ同梱の `preset/AozoraEpub3.ini` と `preset/custom_chuki_tag.txt` で同じフラグを再現する（外字フォントだけは同梱できないので入らない）。Lite の公開パイプライン (`collect_assets` / `decorate_image_tags` / `reflow_image_sections` / `build_title_page_markup` / `append_gaiji_assets` / `build_metadata`) を CLI と同じ順で使う。挿絵は書き出し時に 1 枚ずつ読み、Java と同じ前処理 (余白除去・リサイズ・回転) をかける。実データ検証: `C:\Users\rumia\Documents\WebNovel` の n0421du (401セクション) で Java 版 `AozoraEpub3.jar` の出力と **422/423 ファイルがバイト完全一致**（`aozoraepub3dir` 未設定でも **419/423**）、挿絵入りでも **425/426 がバイト完全一致**。残差は `dcterms:modified` のみ (Java はローカル時刻に `Z`、Lite は UTC) 濁点フォント (`vertical_font_with_dakuten.css` + `DMincho.ttf`) は外部ツールと同じ内容を組み込みエンジンにも渡す。設定 `convert.epub-font` = `auto`(濁点注記のある小説だけ)/`always`(本文全体を DMincho、`U+3000` を描けない Reader 向け) で選ぶ。
- 挿絵の自動回転は `convert.rotate-image` (`auto`/`0`/`1`/`2`) で制御する。`auto` (既定) は AozoraEpub3.ini の `RotateImage` に従い、`0` は回転しない、`1`/`2` は横長挿絵を +90°/−90°。組み込みエンジン・外部 `AozoraEpub3_Lite.exe` (`-i` で差し替えた一時 INI を渡す。インストール先の INI は書き換えない)・Worker の `download.epub` に効く。Java 版 `AozoraEpub3.jar` は INI パス指定を持たず、インストール先 INI を書き換える方針ではないため本設定は効かない (jar で同じ挙動にするには AozoraEpub3.ini 側を変更する)
- Worker のストリーミング EPUB も共通設定から横書き・挿絵の有無・濁点フォント・行高を反映し、CSS / フォント資産を実際に埋め込む。自動表紙は本文順の適切な挿絵を最大 1 枚保持して作り、書き出しで再利用する。変換 API の端末指定は EPUB のみ受理し、非対応端末と個別 `enable_inspect` は明示的に拒否する。

**注**: EPUB/MOBI 生成は AozoraEpub3 (Java 版 `AozoraEpub3.jar`、または Rust 製代替 [AozoraEpub3_Lite](https://github.com/Rumia-Channel/AozoraEpub3_Lite)) と kindlegen への依存がある。`aozoraepub3dir` 設定は jar を優先し、無ければ `AozoraEpub3_Lite.exe` / `AozoraEpub3.exe` バイナリを受理する (`canonicalize_aozoraepub3_tool_path`)。詳細は `docs/aozora_lite_evaluation_2026-08-23.md`。

---

### 5. `list` — ✅ 完了

> 現在管理している小説の一覧を表示します

| オプション | 短縮 | 型 | デフォルト | 説明 | Rust |
|-----------|------|-----|-----------|------|:----:|
| `--latest` | `-l` | flag | false | 最終更新日の新しい順（降順）でソート | ✅ |
| `--gl` | — | flag | false | 更新日ではなく最新話掲載日を使用 | ✅ |
| `--reverse` | `-r` | flag | false | 表示順を反転（`--latest` と併用すると昇順） | ✅ |
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
- `--latest` は `last_update` 降順（`--gl` 併用時は `general_lastup` 降順）、`--reverse` 併用時は昇順。Ruby版 `Database#sort_by` の既定動作に合わせる（Issue #21）。
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
- `default.*` / `force.*` は `convert` の小説・テキスト入力の両経路で共通設定ストアから取得し、`--ignore-default` / `--ignore-force` で個別に無効化できる（Issue #22）。
- `global_setting` は SQLite 管理の対象外とし、storage-backend が `sqlite` でも `~/.narousetting/global_setting.yaml` を維持する (narou.rb 互換・ライブラリ間で共有するため)。旧ビルドが DB へ取り込んで退避した分は、ファイルが無ければ初回読み出しで復旧する
- local / global 設定の入出力は `src/db/settings.rs` → `Inventory` に統一する。SQLite 利用時は `app_state`、legacy YAML 利用時は従来の設定ファイルに保存し、双方で同じキー・型を扱う。
- Web の設定フォームは未設定・明示 false・明示 true を区別し、変更項目と表示時の期待値だけを送る。保存先ごとに CAS で更新し、競合は HTTP 409。未送信項目・未知キーは保持し、未変更保存は書き込まない。
- native の `s3.*` / `sqlite.mirror-files` 変更は長命な Web ストアとの差を避けるため再起動が必要。保存 API と外部変更 watcher が再起動へ接続し、失敗時は保存済み／手動再起動が必要という状態を返す。

**主要 local_setting 項目**:

| 設定名 | 型 | 説明 |
|-------|-----|------|
| `device` | select | 対象端末 (kindle/kobo/epub/ibunko/reader/ibooks) |
| `hotentry` | boolean | hotentry 自動生成 |
| `concurrency` | boolean | 並列DL+変換 |
| `logging` | boolean | ログ保存 (コンソール出力を `log/` にも追記) |
| `update.interval` | float | 同一サイトドメインの小説間ウェイト (秒、最小2.5) |
| `update.strong` | boolean | 同日更新時の内容チェック |
| `update.disabled-author-tracking` | boolean | native の更新後の作者追跡を停止 (Worker では無効) |
| `update.convert-only-new-arrival` | boolean | 新着時のみ変換 |
| `update.sort-by` | select | 更新順ソートキー |
| `update.max-parallel-domains` | integer | ドメイン別並列DLのワーカー数 (既定4、1で逐次) |
| `update.auto-schedule.enable` | boolean | 自動更新スケジューラ有効 |
| `update.auto-schedule` | string | スケジュール時刻 (HHMM, カンマ区切り) |
| `update.auto-schedule.timezone` | string | 自動アップデートの HHMM を評価する IANA タイムゾーン (Worker 既定 `Asia/Tokyo`) |
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
| `webui.new-tag-color` | select | 新規タグの既定色。`default`/未設定時は自動色ローテーション |
| `queue.max-retries` | integer | 失敗 job を `available_at` 付きで自動再投入する最大回数。`0` でリトライ無効。既定 `3` |
| `queue.retry-backoff` | string | リトライ時の待機秒数をカンマ区切りで指定（`s`/`m`/`h` 単位可、例: `1m,5m,15m`）。要素数を超えて失敗したときは最後の値を再利用。既定 `1m,5m,15m` |
| `narou-compat` | boolean | (Rust 拡張) SQLite 管理時も `.narou/*.yaml` をファイルとして維持し narou.rb との前方互換を保つ。OFF (既定) で完全 SQLite 移行 |

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
| `self-update.variant` | select | セルフアップデートで取得するリリース variant（`gpl` = GPL版（AozoraEpub3_Lite 組込み・GPL-3.0） / `standard` = 通常版（外部 AozoraEpub3・BSD））。未設定なら実行中のビルドと同じ variant |

**実装済み (Rust)**:
- `local_setting.yaml` / `global_setting.yaml` の読み書き (`Inventory`)
- 設定値のバリデーション (型チェック、選択肢チェック、boolean の true/false 厳密化)
- `default.*` / `force.*` / `default_args.*` は既知の original setting / command 名だけ受理し、未知名を拒否
- `default_args.trace` / `default_args.console` を含む Ruby 由来の command 名を受理
- `webui.table.reload-timing` / `webui.theme` / `webui.new-tag-color` など hidden select 項目でも選択肢チェックを実施
- Windows の WEB UI で directory 設定を保存する際、`fs::canonicalize` が返す `\\?\UNC\` を通常の `\\server\share` 形式へ戻し、`convert.copy-to` などの UNC パスを保持
- `--list` 現在値一覧表示
- `--all` 全変数表示
- `setting -a` では hidden 項目に加え、`default.*` / `force.*` / `default_args.*` を型・説明付きで Local Variable List に列挙
- `--burn` は個別 `setting.ini` を ObjectStore 経由で読み書きし、SQLite とファイルの設定を分岐させない
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
- `tag_colors.yaml` の保存順を保持しつつ、`webui.new-tag-color` 未設定または `default` 時は自動色ローテーション (green→yellow→blue→magenta→cyan→red→white)、色名指定時は新規タグへ固定色を割り当てる
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
- SQLite 管理時は小説データが `objects` テーブルにも入っているため、削除時はストレージ側のオブジェクトも同時に消す（`--with-file` は小説プレフィックス配下を全削除、オプションなしは `toc.yaml` のみ）。残すと次回の DL が「本文は取得済み」と判断してファイルを書き戻さず、変換が `section file not found` で失敗する
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
| `--history` | — | flag | false | (P4b) SQLite版バージョン履歴一覧 |
| `--show ID` | — | int | — | (P4b) 指定バージョンの保存済み差分表示 |
| `--restore ID` | — | int | — | (P4b) 指定バージョンへ復元 (copy-forward・新ヘッド記録) |
| `--merge-from ID` | — | int | — | (P4b) 指定バージョンを作業セットへマージ |
| `--merge-sections LIST` | — | str | — | (P4b) `--merge-from` の対象話制限 (カンマ区切り) |

> P4b オプションは SQLite バックエンド有効時のみ動作する。履歴は convert 実行時に自動スナップショットされ、保持数は将来 `diff.history-limit` で制御予定。`--restore` / `--merge-from` はリレーショナル作業セットに加えて ObjectStore の TOC・本文を更新し、古い話のキーを削除する。ファイル出力はミラー設定に従う。`--clean` は差分キャッシュに加えて SQLite バージョン履歴も全削除する。
| target | | string | — | 小説指定 (省略時=最終更新) |

**実装状況**:
- `-n/--number` と `-N` 短縮形、`-l/--list`、`-c/--clean`、`--all-clean`、`--no-tool` を実装済み
- 差分バージョン指定 `YYYY.MM.DD@HH.MM.SS` / `;` 区切りに対応
- セクション YAML からの一時テキスト生成、外部 diff ツール統合、内蔵差分ビューアを実装済み
- 既定対象は最新更新の小説で、差分キャッシュは `本文/cache/<version>/` に配置する
- Web API: `GET /api/diff_history?id=N`（バージョン一覧）、`GET /api/diff_show?id=N&version=V`（保存済み差分）、`POST /api/diff_restore` / `POST /api/diff_merge`（`{target, version, sections?}`）を追加。`api_diff` 応答に `history` フィールドを追加（既存キー不変）

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

### 12. `mail` — ✅ 完了

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
- 添付ファイル名は `mail_setting.yaml` の `attachment_filename_pattern` / `attachment_filename_replacement`、または local setting の `mail.attachment-filename-pattern` / `mail.attachment-filename-replacement` で正規表現置換できる。`mail_setting.yaml` 側を優先し、未設定時は従来どおり変換済みファイル名をそのまま使う
- `tests/mail_e2e.rs` でローカル SMTP listener を使った end-to-end 検証を行う
  - `smtp` 経路で送受信双方が期待どおり動くことを確認 (From / To / Subject / body / Content-Type / 添付ファイル名)
  - `Content-Disposition` の `filename*0*=` / `filename*1*=` 分割を含む RFC 2231 / RFC 5987 エンコード下でも元ファイル名が保持されること、および添付ファイル名の正規表現置換が反映されることを検証
  - CC / 複数宛先の分割、`last_mail_date` 差分送信、既定の安全既定 (平文 SMTP は `allow_insecure: true` 明示 opt-in が必須) も網羅

**補注 (完了一覧の「不足動作」ではなく実装上の事実)**:
- narou.rs の `mail` 実装は `via: smtp` のみ対応で、Ruby版 Mailer/Pony が持つ sendmail 等の代替経路は未対応。`mail_setting.yaml` の `via` に smtp 以外を指定すると明示的にエラーを返す
- 実 SMTP end-to-end は `tests/mail_e2e.rs` で自動検証済み。TLS ハンドシェイク (STARTTLS / Wrapper / 証明書検証) は再現に自己署名証明書が必要なため、平文経路のみ自動テストでカバーしている

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
- global `server-add-accepted-hosts` は HTTP の `Host` ヘッダに追加で許可するホストのリスト（カンマ区切り）。`*.example.com` 形式の安全なワイルドカードに対応し、unsafe なパターン（`*` 単独、`*.com`、末尾ワイルドなど）は警告ログを出して無視。既定の許可集合（bind host + loopback + 自ホスト名）はそのまま残り、追加ホストだけを opt-in で広げる
- hidden global `server-max-targets-per-request` は WEB UI が 1 リクエストで送れる小説 ID の最大数。既定値 `100000`、未設定または 0 以下は既定にフォールバック（Web UI には表示しない）。蔵書数が極端に多い環境で `narou setting server-max-targets-per-request=200000` のように上書きできる（`server-*` は変数名から自動で global スコープに振り分けられる）
- API の凍結/解凍操作と一覧上の `frozen` 判定は CLI と同じ `.narou/freeze.yaml` を優先し、`frozen` タグは補助的に扱う
- queue worker が `.narou/queue.yaml` 永続キューを読み書きし、download / update / auto_update / convert / send / backup / mail の queued job を別プロセスまたは worker 内処理で実行する。Ruby版同様 `pending` / `running` を分けて保持し、legacy `cmd` / `args` / `meta` / `status` / `created_at` / `started_at` を維持したまま復元できる。`concurrency` 有効時は外部通信あり(download/update/auto_update)とその他(convert/send/backup/mail)を別 lane で並列実行し、無効時は全 job を投入順に逐次実行する
- 一時的なネットワーク失敗で夜間更新全体が止まらないよう、queue worker は失敗した job を `JobOutcome::Failed` 時に判定し、`retry_count < max_retries` かつ恒久失敗 (detail に "not found" / "invalid argument" / "no such file" / "permanent failure" / "永久失敗" / "恒久失敗" を含む) でなければ `available_at` 付きで `active_pending` へ自動再投入する。`available_at` 経過後の job だけが `pop` 系で取り出されるためスリープを挟まない。Web UI には `queue_retry` イベントを、追加試行なしで `failed` へ落ちた場合は従来どおり `queue_failed` イベントを通知する
- Web UI のキュー詳細モーダルは `queue_start` / `queue_complete` / `queue_failed` / `queue_retry` / `notification.queue` 受信時に開いていれば再取得し、WebSocket が届かない場合も定期更新時に開いている詳細だけ再取得する
- リトライ挙動は `narou setting` の `queue.max-retries`（既定 3、`0` で無効）と `queue.retry-backoff`（既定 `1m,5m,15m`、カンマ区切り、`s`/`m`/`h` 単位可、要素数を超えて失敗したときは最後の値を再利用）で調整できる。`QueueJob` の `available_at` フィールドは `#[serde(default, skip_serializing_if = "Option::is_none")]` 付きで読み書きされ、リトライ機能導入前の旧 `queue.yaml` もそのまま再ロード可能
- `queue.yaml` 保存時は Ruby版に寄せ、先頭 `---` を出さず、job id は UUIDv4 形式、`created_at` / `started_at` / `updated_at` は秒精度の ISO8601 で出力する
- idle 中の queue worker は同一プロセス内の queue 更新通知で起床し、外部プロセスが `queue.yaml` を更新した場合だけ低頻度フォールバックで検出する。空キュー時に `.narou/queue.yaml` を 500ms ごとに読み続けない
- Web 経由の convert job は `--no-open` で非対話化し、API 指定 device は worker 専用 override で child process に渡す
- 一覧の作品リンクは URL を HTML 属性としてエスケープし、引用符が `href` / `title` の外へ出ないようにする。
- 個別メニューの「変換」は、その作品 ID だけを `/api/convert` に送る。一覧で未選択でも動作し、要求送信の失敗は通知する（issue #33）。SQLite + Lite の本文のみ／端末出力の両経路は `convert.keep-txt` を接続ロック取得前に読み、CLI の Tokio 文脈からの挿絵取り出しは別スレッドで実行して EPUB 生成まで完了する。
- 実行中ジョブの中止 API が HTTP 200 の `error` 応答を返した場合も、Web UI は失敗として通知する。
- 一覧更新が重なった場合は最新の要求だけを反映し、古い応答で削除済み作品・凍結状態・選択状態が巻き戻らないようにする。
- 並列取得の進捗は操作 ID と更新番号で分離し、完了・再試行・再接続でも他の進行中操作を上書き／消去しない。Worker の出力・詳細ログもジョブごとの文脈で配信する。
- 目次の総ページ数が不明な場合は、探索上限を分母にせず不定進捗と確認済みページ数を表示する。総数のないページ送りは次ページが無くなるか既訪問 URL に戻るまで辿り、50 ページで打ち切らない。本文などの確定した進捗は完了件数を作品名より先に表示し、取得中の話番号と区別する。
- native は毎回の実行開始を通知する。再試行や Worker の lease 回収で開始通知を受けたときは、同じジョブの旧操作だけを破棄して新しい進捗を受理する。Worker の継続実行は前回の終了通知を送ってから再投入し、中断 API 自身も進捗を消去するため、終了通知を送れない orphan のバーを再接続で復活させない。
- `download.interval` / `download.wait-steps` は実際の HTTP 送信ごとに、サイト定義の `min_interval` とともに適用する。リダイレクトと通信手段の切替も待機を省略せず、Worker では待機中も設定スナップショットの有効期限までに再確認する。
- グローバル設定は変更項目だけを初期値照合付きで保存する。整数表記になる浮動小数値も設定メタデータで照合し、実際に競合した変更は拒否する。個別 INI の更新はファイル・SQLite・D1 の原子的更新を通し、同時変更された別項目と未知項目を保持する。
- コンソール履歴の取得応答は表示世代で照合し、履歴削除や新しい行より古い応答で表示を巻き戻さない。コンソール操作と真偽値設定はキーボード操作に対応する。狭い画面の操作バーは折り返し、メニューを操作ボタンの下へ表示する。
- Web UI のログコンソールは下端のハンドルをドラッグ、またはフォーカスして上下キーで高さを調整できる。左右のペインは同じ高さを保ち、調整値はブラウザ単位で保存する。「表示 → コンソールサイズをリセット」で初期の高さへ戻し、「表示設定を全てリセット」にも含める。サイズ変更・リセットではログ、進捗表示、過去の行を読んでいる位置を消さない。
- FHD を超える画面ではブラウザの既定文字サイズを尊重しつつ UI を段階的に拡大し、メイン領域・ヘッダー幅とコンソールの初期高さをビューポートへ追従させる。幅広表示時にコンソールと操作バーだけを旧上限へ制限しない。狭い画面の初期高さ・スクロールと、読み物・ダイアログの可読幅は維持する。
- 非対応操作は共通 capability で制限し、Worker の一覧内「保存先を開く」ボタンも無効にする。SQLite のオンデマンド EPUB は変換済みテキストの保存 codec を復号して読み、保存異常をキャッシュ欠落として隠さない。
- Worker の EPUB 挿絵欠落警告は、挿絵が有効で本文に参照があり、実体を 1 枚も解決できない場合だけ表示する。参照のない作品や挿絵無効時には警告しない。
- `queue_clear` は deadlock しないように永続キュー保存順を修正済み
- local `update.auto-schedule.enable` / `update.auto-schedule` が有効なら、Ruby版同様に時刻指定で自動アップデートを Web queue に投入する。設定保存時は Ruby版同様に scheduler を stop/start し、サーバ再起動なしで変更を反映する
- 自動アップデートは `--gl narou` → `modified` タグ対象 → その他小説の順に child `update` を実行し、child stdout/stderr と Web 用構造化進捗を Web UI コンソールへ中継する。実行中 phase の child PID は通常 job と同じ中止処理へ登録する。各 phase 後に Web サーバ側 DB を再読み込みして `modified` タグ検出漏れを防ぐ。`server_setting.current_sort` は Ruby互換に `column` 数値/数値文字列の両方を受理し、対応する `--sort-by` へ引き継ぐ。`last_check_date` も Ruby版同様に自動アップデート/modified 更新の sort key として使える。Web UI からの「全更新」も Ruby版同様に明示的な update-all 扱いとなり、開始メッセージと実際の update 対象順の両方で現在の一覧ソート順を使う。手動の `最新話掲載日確認 + modified 更新` も Ruby版同様に 1 つの `update_general_lastup` job 内で `--gl` と `tag:modified` を直列実行し、queue 詳細ラベルは `update_general_lastup` / `update_by_tag` の legacy cmd をそのまま表示する。Web UI の選択更新/convert/削除は current sort の snapshot を request に添えて server 側でも並べ直し、選択順ではなく現在の一覧ソート順で処理する。Web UI の modified / update_by_tag 系更新は、CLI `update --sort-by` が対応している列 (`id` / `last_update` / `title` / `author` / `general_lastup` / `last_check_date`) では現在の一覧ソート順を引き継ぐ
- Web queue の restore/clear API は Ruby互換に、running 中の仕事を消さずに pending/復元待ちだけを消去し、復元フラグは restore 成功後にだけ下ろす。`reorder_pending_tasks` は失敗時に success=false を返し、`taginfo.json` は選択 ID ごとのタグ出現数 (`count`) と全体件数 (`total_count`) を返す selection-aware backend になっている
- Web UI の一覧検索は既存のタイトル・作者・サイト・タグ・状態に加え、URL、N コード、小説 ID（整数）にも対応する。貼り付けた `https://` / `http://` URL は `https:` のフィールド指定として誤認せず全文を検索する。一覧は `/api/list?all=true` を取得してクライアント側で絞り込むため、判定 (`src/web/assets/js/ui/render.js`) はサーバ側 `search[value]` と同一規則（`toc_url` の部分一致（末尾 `/` は無視）、`ncode` の部分一致または `toc_url` 最終セグメントとの一致、レコード ID の一致）を持たせ、`/api/list` は `ncode` を返す。コンソール拡大時は右側にまだ出力がなくても左右両ペインの高さを揃える。
- 一覧の「リンク」は HTTP(S) の `original_url` だけを優先し、ID・Nコード・別名などが旧データに保存されていても `toc_url` へフォールバックする。ID 指定の更新では正しいページ URL を上書きしない（issue #33、再ダウンロード不要）。
- Web UI からのサーバ再起動では replacement process に `--no-browser` を付与し、hidden 起動中は `--hide-console` も維持したまま再起動待機ページから同じタブで元ページへ戻る
- systemd の `.service` 配下で Web UI から本体更新するときは、通常の `setsid` 子プロセスではなく `systemd-run` の別 transient service に既存 updater を起動させる。本体の終了後に旧 updater が zip を適用したら、元の systemd service を `systemctl restart`（user service は `--user`）で再起動する。`Restart=always` / `KillMode=control-group` による updater 強制終了を回避する。引き渡し失敗時は API も success=false とし、本体を終了しない。
- ソース checkout の `target/<profile>/narou_rs` は local-build 版として識別し、自動更新を開始せず `git pull` + `cargo build --release` または別ディレクトリへの GitHub Release 版展開を案内する。Release 版 updater は適用失敗時もダウンロード済み `.tmp` と展開用 `update_extract.tmp` の削除を試み、失敗理由は `update.log` に残す
- Windows の `narou web --hide-console` は GUI subsystem で起動し、通常 CLI 実行時は親コンソールへ再接続、hidden 実行時はタスクトレイの右クリックメニューから `終了` / `再起動` を呼べる。Web worker / auto-update / 即時 API 実行が起動する child process も hidden 状態を引き継ぎ、空のコンソールを開かない
- 即時実行の `diff` / `folder` / `reboot` API は child command の失敗や replacement process 起動失敗を success=false として返し、false success を出さない
- Web 設定画面は Ruby版同様、`tab` がある設定を `invisible` 指定でも表示する。`webui.theme` / `webui.table.reload-timing` / `webui.new-tag-color` / `webui.debug-mode` / `server-bind` / `server-basic-auth.*` / `server-ws-add-accepted-domains` / `server-add-accepted-hosts` / `self-update.variant` / `over18` も設定画面に出る
- セルフアップデートで取得する variant（GPL版（AozoraEpub3_Lite 組込み） / 通常版（外部 AozoraEpub3））は Global タブの `self-update.variant` セレクト、または `narou setting self-update.variant=gpl|standard` で設定する。`narou setting` の一覧にも表示され、`narou setting self-update.variant=`（Web UI では「未設定」）で実行中のビルドと同じ variant に戻る。0.4.0 以下からの更新時に出る variant 選択モーダルは、この設定が既にあれば表示せず保存値で更新する
- `webui.theme` / `webui.table.reload-timing` / `webui.performance-mode` / `webui.new-tag-color` / `webui.debug-mode` 保存時は、開いている Web UI に設定再読み込みイベントを送り、テーマメニューの変更も `webui.theme` へ保存する
- `webui.debug-mode` が ON のときは、Web worker が失敗 child process の直近 stdout/stderr を要約して `queue_failed` イベントに載せ、Web UI 通知とコンソールに詳細エラーを出す。OFF のときは従来どおり簡潔な失敗通知だけにする
- `/novels/{id}/download` は生成済み ebook を全量メモリへ読み込まず、`tokio::fs::File` から 64KiB チャンクでストリーミングする。大きい EPUB でも `Content-Length` / `Content-Disposition` を付けたまま返す
- 一覧の検索文字列・現在ページ・ソート初期値はブラウザ `localStorage` に 6 時間の有効期限付きで保存し、リロード後に検索欄とページ位置を復元する
- Web UI のタグ編集は既存タグ一覧から入力中タグ名に一致する候補を表示し、タグ名クリック検索は `tag:`、作者名クリック検索は `author:`、掲載サイトクリック検索は `sitename:` を生成する。通常クリックは AND、Ctrl クリックは同一フィールド内 OR、Shift クリックは除外 AND、Shift+Ctrl クリックは除外 OR として検索文字列を更新する
- Web UI のタグ編集モーダルでは各タグの「色」ボタンまたは右クリックから既存の色選択メニューを開ける。変更成功後はタグ一覧・小説一覧・開いている編集モーダルを再取得し、色を即時反映する
- Web UI の update 系は、ID 選択・タグ条件・modified followup など入口の違いに関わらず、最終的に通常の `update ID...` 実行経路へ合流する。タグ条件は受付時に ID snapshot を確定し、条件自体は queue meta に残す
- Web UI の `tag` / `freeze` / `remove` 相当操作は、単体・一括とも共通 use-case と native の操作 port を経由し、target 解決、`tag_colors`、凍結状態、lock チェック等を CLI と同じ規則で扱う。global settings API も設定名スコープ判定・値キャスト・`device` の派生設定補正を CLI と同じ `setting_core` へ合流し、`SettingsService` が保存・競合・ACK を管理する。
- Web UI の単体/一括削除 API は削除成功時に console history へ削除ログを出力する。`concurrency` 有効時は非外部通信として `#console-stdout2`、無効時は `#console` を使う
- Web UI の CSV ダウンロードは CLI `csv` の stdout を返し、export 項目を CLI と一致させる
- Web サーバ起動時は Ruby版 `fill_general_all_no_in_database` 相当に、`general_all_no` 未設定レコードの `toc.yaml` を読んで話数をDBへ補完する
- `/` では pure JS / pure CSS の分割 asset frontend を配信し、navbar / console / control panel / list + sidebar の構成で一覧操作できる
- UI は日本語既定で、JP/EN トグルによる切替と `localStorage` 永続化に対応する
- `webui.theme` / `webui.performance-mode` / `webui.table.reload-timing` / `webui.new-tag-color` を設定画面と worker 側設定参照経由で反映し、theme 初期値、performance auto/on/off 判定、table reload の every/queue 挙動、新規タグ既定色へ接続する
- レスポンシブ CSS を分離し、スマートフォン幅でも一覧・キュー・メモ帳を同じ asset 構成で表示できる
- 一覧 API の `frozen` 取得は DB 再入ロックによる deadlock を避けるよう修正済み
- 一覧 API の `new_arrivals` 判定は `webnovel/*.yaml` の `timezone` に合わせたサイト現地時刻で行い、`domain` 未保存の既存データは `toc_url` のドメインからサイト定義を解決する
- favicon は data URL で埋め込み、追加 route なしでブラウザ 404 を出さない
- バージョン別の機能ツアーを `GET /api/feature_tour/pending` / `GET /api/feature_tour/all` / `POST /api/feature_tour/seen` / `POST /api/feature_tour/config` で提供し、アップデート後に新機能を Web UI で告知する。SQLite 管理への移行はツアー経由の opt-in で、`GET/POST /api/storage/mode` がモードの読み書きを担う（CLI 側の保守コマンドは `narou db`）
- 0.4.13 の機能ツアーはアルファポリスの小説・作者一覧対応とカクヨムネクスト対応を告知する。無料分の取得とレンタル・契約に応じたログイン Cookie の利用、閲覧制限による既存本文の保護を案内し、既読後は自動再表示しない。ツアー定義は Native / Worker 共通
- 設定画面のデータ管理方式は描画後・保存後の再描画時に取得し、固定環境では切替ボタンを隠す。Workers の保存前読み取りは primary を使い、replica／isolate キャッシュの古い値を書き戻さない。メモ帳は `object_id` 検査に加えて条件付き UPSERT で同時保存を排他し、負けた要求に最新本文を含む競合応答を返す。
- Worker の環境設定 API (`GET /api/global_setting`) は、編集時の `expected_settings` に使う値を primary からキャッシュなしで取得する。別 isolate の保存後に古い値を表示して初回保存から偽の 409 になることを防ぐ。保存側の CAS と本物の同時編集に対する 409 は維持し、通常の一覧・表示設定の読み取りキャッシュは変更しない
- native / Worker が共通 capability payload を返し、Worker で実行できない CSV・送信・mail・端末操作・調査ログ・キュー並べ替え等は UI から実行できないようにする。直接 API の拒否も通知し、成功として扱わない。
- Worker の `concurrency` はコンソール分離（既定 true）、`convert.keep-txt` は既定 false、`queue.max-retries` は配備上限 3 と表示・保存検証を一致させる。native 専用の `s3.*` は表示せず、ミラー・話キャッシュ・外部 EPUB エンジン等は無効化し、API からも変更を拒否する。
- Worker の取得は `NAROU_JOBS`（最大 4）、変換は `CONVERT_QUEUE`（最大 1）に実行枠を分ける。取得枠が埋まっても別作品の変換を開始でき、同じ作品の取得・変換は共通 D1 台帳で排他する。実効 YAML のサイト単位では取得を 1 件に制限し、別ホスト表記・作者確認も同じサイトへ揃える。
- Worker の排他待ちは待機として永続化して ACK し、失敗回数や provider retry を消費しない。完了・毎分 cron で同じ job ID を再投入し、旧レーンの envelope も正しい Queue へ転送する。作品の別 URL と ID は YAML 解決後の TOC URL で照合し、元の target と再開用 checkpoint は変更しない。
- Worker の wall-clock / subrequest budget は話の境界で安全に区切る継続処理で、Web コンソールに自動再開を案内する。保存済み本文・次の話の位置を維持して同じ job ID で続行し、失敗回数は増やさない。
- Worker の履歴取得・消去は PushHub の永続化履歴に接続し、再接続でも消去済みの行を復活させない。`webui.table.reload-timing` は実際の台帳の待機・実行数でテーブル更新を制御し、進捗・通知は抑止しない。
- 設定保存は `webui.config.reload` を配信する。UI のテーブル・タグ更新は debounce / single-flight で合流し、古い応答を反映しない。Worker のキュー詳細は cursor で全ページを取得してから表示を交換する（既定 200 件／最大 1000 件、全体の待機・実行数を別に返す）。
- 保存方式の読み取りが失敗した場合は不明／失敗として表示し、YAML と推定しない。Worker は D1 固定と表示し、切替操作を出さない。native の実際の方式切替だけが再起動を要求する。
- Worker の本文・旧本文は、話単位の圧縮 S3 オブジェクトと D1 参照へ移行できる。`GET/POST /api/admin/content-migration` が `plan` / `copy` / `verify` / `cutover` / `purge` / `gc` / `status` を提供し、`scripts/migrate-worker-content.mjs` で CLI から再開可能な移行を実行する（既定は計画のみ、`--apply` で旧 D1 本文の削除まで実行）。本文用 `content_backend` と挿絵用 `asset_backend` は独立し、native の保存方式は変えない。
- Worker は本文ストア初期化時、`content_backend` 未設定で旧本文が無ければ `s3`、旧本文・履歴があれば `migrating` を保存し、新しい本文を S3 に置く。明示的な `d1` は S3 必須でない環境だけで維持し、`NAROU_REQUIRE_S3=true` では同じ規則で移行開始する。S3 構成に成功してから状態を原子的に保存し、既存の `migrating` / `s3` を戻さない。一括コピー・照合・旧 D1 本文削除は引き続き管理 API で実行し、`plan` / `status` は読み取りだけである。
- S3 基本化によって先に `migrating` へ進んだ既存ライブラリでも、最初のコピーで移行対象の件数・圧縮前サイズを記録し、再開・切替後も維持する。`plan` の読み取り専用の契約は変えない。
- 本文移行の保守スクリプトを再実行すると、コピー・照合・切替・旧本文削除の完了済み段階を飛ばし、未完了の段階から継続する。検証途中のカーソルを先頭へ戻さず、検証済み件数を二重計数しない。
- Worker の更新は旧本文を履歴へコピーしてから新本文を保存し、PUT 失敗でも現行本文を維持する。改題された話の旧ファイル名は新本文と目次の保存後に削除する。native の履歴移動は従来どおり。
- Worker の本文分類はサブディレクトリ内の作品フォルダ名が `本文` でも、`toc.yaml` / `converter.yaml` / `.illustration_cache.yaml` を D1 に維持する。migration 0012 が既存 DB の移行対象ビューと書き込みガードも更新し、本文削除後の EPUB 生成・再起動を実 Worker で確認している。

**不足動作**:
- narou.rb の HAML/UI と完全一致するレベルの細かな見た目・配置・文言差分の洗い込み
- Web UI の各操作・表示を Ruby 版 view/helper と突き合わせた最終 parity 確認

---

### 14. `backup` — ✅ 完了

> 小説のバックアップを作成します

オプションなし。

**Rust 実装**: `src/commands/backup.rs` で Ruby 版同様に複数 target を順に処理し、`backup/` 直下へ ZIP を保存する。バックアップ名はタイトルを Ruby 版同様に整形して 180 バイトで切り詰める。

SQLite のミラー無効時も本文・設定・挿絵を保存先からスコープ付きで取り出して ZIP に入れる。フォルダが無いだけで登録を削除しない。

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
- SQLite のミラー無効時も TOC と保存済みキーから孤児を検出し、ObjectStore の不要な本文・raw を削除する。
- `clean` は `raw/` と `本文/` の直下だけを列挙する。SQLite ではキーの索引範囲を使い、履歴キャッシュ・挿絵の再帰走査やページごとの全件再走査を行わない。DB にだけ残る孤児と、取込後に追加された実ファイルの孤児の両方を対象にし、凍結・dry-run の扱いは維持する（issue #38）。
- target 解決は ID / URL / Nコード / タイトル / alias / tag 展開を共通パイプラインで処理
- `sample\\novel` で `clean`, `clean <alias>`, `clean -f`, `clean --all`, `clean --all -f` を実行し、作為的 orphan の検出と削除を確認済み

---

### 16. `illust` — ✅ 完了

> 挿絵ハッシュストアの運用補助 (orphan/migrate/fix-ext/rebuild/s3-push/s3-verify)

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--force` | `-f` | flag | false | 実際に変更する (削除/改名/移行) |
| `--all` | `-a` | flag | false | 全小説を対象にする |
| `<sub>` | — | enum | — | `orphan` / `migrate` / `fix-ext` / `rebuild` / `s3-push` / `s3-verify` / `s3-dedup` |
| target | | string | — | 小説指定 (省略時=最終変換) |

**サブコマンド**:
- `orphan` — `.illustration_cache.yaml` の `sources`/`mitemin_ids`/`hashes` と `raw/*.html` の `<img src>` の双方から到達不能な `挿絵/*` を列挙。既定 dry-run、`-f` で削除。
- `migrate` — レガシー名 (`<話数>-<連番>.ext` / URL basename) をハッシュ名へ一括移行し、ソースマップも更新。非 mitemin も対象。
- `fix-ext` — マジックバイト判定 (JPEG/PNG/GIF/WEBP/BMP) で拡張子を実体に合わせて改名。
- `rebuild` — `挿絵/` + `raw/*.html` から `.illustration_cache.yaml` を再構築し永続化。
- `s3-push` — 挿絵をローカルから S3 互換ストレージ (Wasabi など) へ写す。既定は件数と容量を数えるだけで、`-f` で実行。**ライブラリ全体が対象** (`<target>` は使わない) で、`s3.asset-backend=s3` と接続情報が必要。
- S3 LIST の署名エラー診断は、改行表現を正規化して canonical digest と StringToSign 全体を別々に比較する。digest 一致だけで secret の誤りと断定せず、診断改善を 403 の解消とは扱わない。 生の XML 本文は表示せず、許可したエラーコードと任意のサーバー echo の比較結果（一致・不一致・欠落・不正）、公開 AWS サンプルの実行時自己テスト、実効接続先 host/port・標準的な region・native 設定元ラベルを表示する。鍵 ID・署名・資格情報の値やハッシュは表示しない。環境変数ラベルは `.env` と起動時の既存環境を区別しない。 `StringToSignBytes` は各バイト 1〜2 桁の16進数として厳密比較し、LF / CRLF 等を正規化しない。
- `s3-verify` — ローカルの挿絵と S3 の内容をバイト単位で突き合わせる (書き込みなし)。同じくライブラリ全体が対象。
- `s3-dedup` — 旧 `挿絵/` 配置の S3 オブジェクトを dedup プール (`illustrations/<base64url(sha256)>.<ext>`) へ移し、残ったものを消す。**SQLite モード + S3 保存のときだけ有効**。既定は件数だけ数える dry-run、`-f` で実行。hex 名でない挿絵は小説ごとの配置を保つ。

**Rust 実装**: メンテナンスヘルパー (`find_orphan_illustrations`, `plan_legacy_illustration_migrations` / `apply_legacy_illustration_migrations`, `plan_extension_fixes` / `apply_extension_fixes`, `rebuild_illustration_cache`, `detect_image_extension`) を `src/illustration_store.rs` (crate 側) に集約。`src/commands/illust.rs` は CLI オプション解決と dry-run / `-f` の振り分けに専念し、将来 Web UI から同じ crate 関数を直接呼べる形を維持する。削除系・改名系・移行系はすべて既定 dry-run。`-f` 指定時も本文参照・cache 参照の双方から到達不能 / 移行計画を厳密判定してから実際に変更する (BUG-7/15 と整合)。対象小説の解決は clean と同じく ID / URL / Nコード / タイトル / alias / tag 展開の共通パイプラインを使い、`--all` は凍結済み小説をスキップする。

DB-only ライブラリの `orphan` / `rebuild` / `fix-ext` 等は保存先のデータを取り出して処理し、索引更新・削除・改名をストアへ反映する。壊れた挿絵索引は空として削除を進めず、失敗させる。S3 移行・検証の独立 I/O は最大 4 件、既知の小オブジェクトは見積り 8 MiB の窓で処理し、大きい／サイズ不明の対象は単独で処理する。

S3 の改名同期は HTTP 200 だけでコピー成功としない。完全な `CopyObjectResult` と必須メタデータを確認し、本文内エラー・空／破損／不完全な応答は失敗させる。コピー成功を確認できない場合は旧オブジェクトを削除せず、生の応答本文もエラーへ載せない。

**互換メモ**:
- ハッシュ名 (`<64-hex>.ext`) と mitemin ID 名 (`iNNNN.ext`) は canonical とみなし、cache の対応表に既に載っていれば自動的に "到達可能" として orphan 判定から除外する
- 移行 (`migrate`) 後の cache 更新は、同ルーチン内で `IllustrationStore::remember_hash_source` / `remember_mitemin` を直接呼んで反映する。`rebuild` は store を白紙から組み立てる
- `sample\\novel` で orphan/migrate/fix-ext/rebuild の dry-run を実行し、作為的 legacy ファイルと孤児ファイルの検出を確認

---

### 18. `help` — ✅ 完了

> このヘルプを表示します

**実装** (`src/commands/help.rs`):
- 未初期化時: `narou init` を促すメッセージ（`.narou/` ディレクトリ存在チェック）
- 初期化済み: 全26コマンド一覧 + oneline_help（narou.rb の24コマンドは元と同じ順序・文言で掲載。Rust 拡張の `illust` と `login` を追加。`db` は一覧に含まれない）
- グローバルオプション表示（`--no-color`, `--multiple`, `--time`, `--backtrace`）
- ショートカット説明（`d`, `fr` 等の例示付き）
- `NO_COLOR` 環境変数対応（ANSIエスケープコード条件付き出力）
- 引数なし（`narou`）およびショートカット（`h`, `he`）からのフォールバック対応

**Rust 実装**:
- 未初期化時 / 初期化済み時のトップレベル help を Ruby版相当に表示
- 全26コマンドの oneline help とグローバルオプション、ショートカット説明を表示（既存24コマンドは narou.rb と同じ順序）
- `narou <command> -h` の banner、説明文、Examples、Options を Ruby版各 command に合わせて整備
- `convert` の `Configuration:` 節、`setting -h` の Variable List、`update --gl` の詳細説明表も表示

---

### 19. `version` — ✅ 完了

> バージョンを表示します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--more` | `-m` | flag | false | Java/AozoraEpub3 バージョンも表示 |

**Rust 実装**: `src/commands/version.rs` で `narou -v` / `narou version` に対応。`--more` も受け付ける。help 文言と `--more` の出力は Ruby 版に合わせた。

---

### 20. `log` — ✅ 完了

> 保存したログを表示します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--num NUM` | `-n` | int | 20 | 表示行数 |
| `--tail` | `-t` | flag | false | ストリーミング (`tail -f` 相当) |
| `--source-convert` | `-c` | flag | false | 変換ログを表示 |
| `<path>` | | string | — | ログファイルパス直接指定可 |

**Rust 実装**: `src/commands/log.rs` で `narou log` / `-n` / `-t` / `-c` / `<path>` に対応。最新ログは `log/*.txt` を更新日時順で選択し、`.narou/local_setting.yaml` の `log.num` / `log.tail` / `log.source-convert` も既定値として反映。`-c` は Ruby版同様 `*_convert` ログだけを対象にする。

**ログに残る範囲**: コンソールへ出る行は `log/*.txt` にも追記される (narou.rb `lib/narou_logger.rb` の `append_log` 相当)。`MessageSink` (`progress::console_sink`) 経由の行も同じログへ入る。以前は `src/logger.rs` が bin と lib の両方にコンパイルされて `LoggerState` が 2 つ存在し、sink 経由の行が未初期化の lib 側ロガーへ書かれて記録されなかった (2026-10 修正、issue #36)。`list` / `diff` / `help` / `log` は `logger::without_logging` で意図的にログへ残さない。回帰テストは `tests/cli_logging.rs`。

---

### 21. `folder` — ✅ 完了

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

### 22. `browser` — ✅ 完了

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

### 23. `alias` — ✅ 完了

> 小説のIDに紐付けた別名を作成します

| オプション | 短縮 | 型 | デフォルト | 説明 |
|-----------|------|-----|-----------|------|
| `--list` | `-l` | flag | false | 現在の別名一覧 |
| assignment | | string | — | `name=target` で設定、`name=` で削除 |

**Rust 実装**: `src/commands/alias.rs` で Ruby版 `alias.rb` を実装。`.narou/alias.yaml` の読み書き、`--list`、`name=target` による設定、`name=` による解除、`hotentry` 禁止語、半角英数字+`_` 制約に対応した。

**互換メモ**:
- list 表示は `alias=title` 形式
- 別名解決は `src/commands/mod.rs` / `src/commands/download.rs` の共通ターゲット解決へ統合済みで、`folder`, `browser`, `clean`, `convert`, `update`, `setting` などの既存 target 解決でも ID/タイトルに加えて Nコード・URL をそのまま受け付ける
- `sample\\novel` で `alias testalias=1`, `alias --list`, `alias testalias=` を確認済み

---

### 24. `inspect` — ✅ 完了

> 小説状態の調査状況ログを表示します

オプションなし。target 省略時 = 最終変換小説。

**Rust 実装**:
- `src/commands/inspect.rs` で `調査ログ.txt` の読み取り表示を実装。target 省略時は `.narou/latest_convert.yaml` の `id` を使い、複数 target は Ruby版同様に区切り線付きで順に表示する。ログが存在しない場合は `調査ログがまだ無いようです` を表示する
- SQLite の `調査ログ.txt` は ObjectStore から読み、読み取り失敗を「ログなし」として隠さない。ミラー無効で実フォルダが無い `folder` 操作は利用不能と明示する。
- `src/converter/inspector.rs` を追加し、変換時に `調査ログ.txt` を保存するようにした
- `convert.inspect=true` と `convert --inspect` で full display、通常 convert では Ruby版同様 summary 表示にした
- `auto_join_in_brackets` の警告/エラー、`modify_kana_ni_to_kanji_ni` INFO、`enable_erase_introduction` / `enable_erase_postscript` INFO も inspection に反映した
- `illustration.rb` 相当として、HTML挿絵のローカル保存、保存成功 INFO、未対応画像形式 / 例外 ERROR も convert 中の inspection に反映した
- textfile 変換時も `enable_enchant_midashi` が false なら Ruby版同様の推奨 INFO を記録し、`sample\\novel` で UTF-8 / Shift_JIS の実変換と `調査ログ.txt` 保存を確認済み

---

### 25. `csv` — ✅ 完了

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

### 26. `trace` — ✅ 完了

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
| download の残互換実装 | download | command 固有の欠落はほぼ解消。`mail` 系の end-to-end は `tests/mail_e2e.rs` で別途完了済み |

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

### Worker の S3 ストア確認

`GET /api/storage/mode` は管理方式 `sqlite` に加え、`metadata_backend=d1`、
`illustration_backend=d1|s3`、本文用 `content_backend=d1|migrating|s3`、`content_migration_api`、`s3_required` を返す（既存の認証を適用）。
`?probe=s3` は選択された S3 に上限 1 件の LIST を行い、`s3_list=ok|failed|not_selected`
だけを返す。失敗は HTTP 503。キー、保存先設定、プロバイダーのエラー本文は返さない。
HTTP 200 でも正しい `ListBucketResult` 外側要素を持たない本文は LIST 成功としない。
CI の S3 必須配備では旧 D1 marker・未設定より挿絵の S3 指定を優先する。旧 D1 挿絵は移行・削除・代替参照しない。本文も初期化時に旧 D1 本文の有無に応じて `migrating` / `s3` へ進め、新規本文を S3 に保存する。管理・目次・設定は D1 に維持する。設定不備や S3 要求失敗で D1 へ戻さず、配備後の実 LIST 成功も要求する。
この確認は LIST 権限・接続の確認であり、挿絵の既存データ移行や GET/PUT の検証ではない。

S3 の存在確認は `GET` + `Range: bytes=0-0` を使う。GET の署名は必須ヘッダのみで、
キャッシュが変更・除去し得る `Range` は署名対象に含めない（送信自体は維持）。
`206` の `Content-Range` と、範囲指定が外れた `200` の `Content-Length` の両方からサイズを読む。
保存キー・索引・GET/PUT 権限の要件は変わらず、403 を成功扱いしたり D1 に逃がしたりしない。
Worker の未指定のテキストエラー応答には `text/plain; charset=utf-8` を付け、
日本語のキーが文字化けしないようにする（JSON エラーの Content-Type は維持）。
