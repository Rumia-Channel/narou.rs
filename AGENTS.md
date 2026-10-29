# narou.rs — Rust Port of narou.rb

## Overview
narou.rb（Ruby製の日本のWeb小説管理・電子書籍変換ソフトウェア）の互換実装をRustで作るプロジェクト。CLI・Web UI・変換出力など、外部から観測できる挙動の narou.rb 互換を目指す。

## 実装状況
`COMMANDS.md` が narou.rb 全24コマンドと Rust 拡張 (`db`/`illust`/`login`/`author`) 計28コマンドのオプション・挙動・実装状況を管理するマスタードキュメントである。最新の実装状況はそこを参照すること。

| 完了度 | コマンド数 | 内訳 |
|:------:|:---------:|------|
| ✅ 完了 | 24 | init, list, tag, freeze, remove, setting, diff, send, mail, backup, clean, illust, author, help, version, log, folder, browser, alias, inspect, csv, trace, db, login |
| 🟡 部分 | 4 | download, update, convert, web |
| ❌ 未実装 | 0 | — (全コマンド実装済み) |

## Porting Policy
- このプログラムは本家 narou.rb を Rust へ移行するための互換実装である。Ruby 版ソースは `sample/narou`（gitignore 済みのローカルコピー、upstream: whiteleaf7/narou）に置いて参照する。`sample/` はリポジトリ管理外のため、手元に無い場合は upstream を clone して当てる。
- 内部ライブラリ、データ構造、処理系統、実装アルゴリズムは Ruby 版と同一である必要はない。Rust 側で保守しやすく、安全で、検証しやすい構成を優先してよい。
- 互換性の主対象は外部から観測できる挙動である。特に CLI/API の引数・戻り値・エラー挙動、`webnovel/*.yaml` や `converter.yaml` などの YAML 構文理解、`.narou/` 配下のデータ読み書き、最終的なファイル出力を narou.rb と徹底的に合わせる。
- Ruby 実装は仕様の参照元として扱う。処理手順をそのまま写すことよりも、同じ入力から同じ外部挙動・同じ出力を得ることを優先する。
- 互換性調査では Ruby 版の内部手順を読むが、それは外部仕様を抽出するためである。Rust 実装では、外部挙動・データ互換・出力互換を壊さない限り、Ruby の逐語的移植よりも堅牢性、保守性、検証容易性、性能、安全性が高い設計を選ぶ。

## 互換性の要件レベル
- 外部から観測できる挙動の互換性は**妥協せず完璧に**追求する。これには以下が含まれる:
  - **設定ファイルの位置**: `.narou/local_setting.yaml`、`~/.narousetting/global_setting.yaml` など、Ruby 版と同一パスに配置する。
  - **設定ファイルの読み書き互換**: Rust が書いた YAML を Ruby が読め、Ruby が書いた YAML を Rust が読めること。`---` ヘッダの有無など形式の差は許容されるが、意味論（キー名・値の型・構造）は一致させる。
  - **全設定項目の読み書き**: Rust 側に未実装の機能（send、mail、device 変更自動調整等）の設定項目であっても、`narou setting` コマンドで読み取り・設定・削除が可能であること。`default.*`、`force.*`、`default_args.*` 系の動的変数名もすべて受け付けること。
  - **CLI の引数・戻り値・エラーメッセージ・終了コード**: Ruby 版と同一であること。
  - **`webnovel/*.yaml` や `.narou/` 配下のデータ構造**: Ruby 版が読める形式を維持すること。
  - **最終的な変換出力ファイル**: narou.rb の出力と同一であること。
- 「内部実装は異なってよい」方針は変更しない。上記の外部互換性を満たす限り、Rust 側のアルゴリズム・データ構造・処理順序は自由に選んでよい。Ruby 版に既知の脆さや古い都合がある場合は、同じ外部結果になることをテストやドキュメントで確認した上で、Rust 側ではより良い内部設計を採用する。

## YAML-Driven Site Definition Compatibility
- サイト別の取得・前処理・抽出ルールは narou.rb と同じく `webnovel/*.yaml` を主たる仕様として扱う。ユーザーが初期化フォルダ内の `webnovel/*.yaml` を編集・差し替えた場合、その内容で挙動を変えられることが互換性の重要要件である。
- Rust 側にサイト固有ロジックを直接ハードコードする実装は、最終的な互換方針としては不可。特に `code: eval:` や前処理相当の記述を YAML から切り離して Rust 関数へ固定すると、narou.rb の「YAML を更新すればサイト追従できる」という性質を壊す。
- **サイト対応の実装順序**: (1) 既存の YAML 項目・`preprocess:` DSL で表現する、(2) 表現力が不足する場合はサイト非依存の汎用機能として DSL の文法・AST・インタプリタを拡張する、(3) 追加した DSL を `webnovel/*.yaml` から利用する。この順序を必須とし、DSL 拡張を省略してサイト専用 Rust 関数を追加してはならない。
- Rust の production code に置いてよいのは、HTTP 取得、URL 解決、HTML エンティティ復元、共通抽出、DSL 実行基盤、実行制限など全サイトで再利用できる仕組みだけとする。サイト名・ドメイン名・サイト固有 CSS selector / 正規表現 / JSON path を条件にした分岐や専用関数は置かず、それらの値と処理手順は YAML / DSL 側に記述する。
- 特定サイト名や実データを使う回帰テスト・fixture は許可するが、テスト対象の production code はサイト非依存でなければならない。DSL 拡張が安全性・互換性上どうしても不可能で暫定 Rust 処理が必要な場合は、実装前に理由と YAML へ戻す条件を明示し、ユーザーの了承を得る。
- 2026-05 時点: ハードコードされた `kakuyomu_preprocess` は完全に除去され、`webnovel/kakuyomu.jp.yaml` の `preprocess:` DSL ブロックへ移行済み。pest 文法ベースの安全な DSL パーサー (`src/downloader/preprocess.pest`) + インタプリタ (`src/downloader/preprocess/interpreter.rs`) により、YAML 記述だけでカクヨム JSON → 中間テキストの展開が可能である。ユーザー側 YAML の `preprocess:` を編集するだけで前処理ロジックを差し替えられる。
- pest 文法 (`src/downloader/preprocess.pest`) は以下の構文に対応: `guard`/`let`/`set`/`if`/`else`/`for`/`emit`/`insert_at_match`, 文字列補間 `${...}` (式を書ける)、オブジェクトはキー昇順 (`.keys` の並びも昇順), 正規表現 JSON 抽出 `extract_json(/.../)`, 追加取得 `request("...")` / `fetch_json("...")` と結果参照 `fetched["<url>"]`, メソッドチェイン `.map`/`.flat_map`/`.flatten`/`.compact`/`.join`/`.gsub`/`.replace`/`.keys` (オブジェクトのキー)/`.is_array`/`.empty`/`.size`/`.first`/`.last`/`.reverse`, マッチ単位の置換 `.gsub(/re/) { |m| ... }` (`m` は `[全体, グループ1, ...]`), 添字アクセス `arr[0]`/`hash["key"]` (添字は式), 整数リテラルと `+`/`-` (数値文字列は自動変換、それ以外は null), 論理演算 `&&`/`||`/`!`/`==`/`!=`。実行時に step budget / 文字列サイズ上限 / 配列要素数上限による防御あり。
- 注意: 真偽判定は Ruby 寄りで、**数値 0・空文字・空配列・null は偽**。`0` を取りうる数値フィールドを分岐に使わない (Pixiv の `illustType` が 0 になる例がある)。
- `.gsub` は第 1 引数に正規表現リテラル (`/re/`) も取れる。置換文字列では `$1` / `${name}` が展開される。チェインの `.field` / `[...]` は書いた順に評価される (`.first.name` が `.name` → `.first` の順に化けない)。
- **追加取得 (job キュー)**: `request(url)` / `fetch_json(url)` はその場では取得せず要求として記録し、実行側 (`util::pretreatment_source_with_jobs`) がサイトの `FetchPolicy` 経由で取得してから定義を再実行する。再実行は元の本文からやり直し、新しい要求が無くなるまで最大 4 ラウンド。結果は `fetched["<url>"]` で見え、失敗は null。ジョブの同一性は URL で、結果は `Downloader::preprocess_jobs` が 1 小説分保持する (同じ挿絵を多数の話が参照しても取得は 1 回)。完了したジョブはキューに残さず結果だけを保持するので Worker でも長命な状態を持たない。
- 注意: `preprocess` は TOC・本文・小説情報の全フェッチに同じスクリプトが走る。ページ種別は JSON の形で判定する。
- 新しいサイト対応やサイト構造変更対応では、まず YAML 表現で解決できるかを検討する。やむを得ず Rust に暫定処理を置く場合は、暫定であること、対応する YAML 意味論、将来 YAML 駆動へ戻す作業を `AGENTS.md` または Serena メモに明記する。
- Arcadia (`webnovel/www.mai-net.net.yaml`) に `encoding: UTF-8` は置かない。narou.rb の同梱 Arcadia 定義には無く、Rust 側は UTF-8 を既定として扱えばよい。Arcadia の本文取得不具合の実原因は `href` の `&amp;` を未デコードのまま section URL に使っていたことであり、`build_section_url()` 側で HTML エンティティを復元する。

## COMMANDS.md 同期ルール
- `COMMANDS.md` は narou.rb 全24コマンドのオプション・挙動と Rust 側実装状況を管理するマスタードキュメントである。
- **コマンドの新規実装・オプション追加・フラグ追加・挙動変更を行うたびに、必ず `COMMANDS.md` の該当箇所をリアルタイムに更新する。**
- 更新内容: Rust 列の ✅/🟡/❌ マーク、実装状況サマリの完了度、不足動作リストの削除・追加。
- 実装が完了したコマンドは「部分」→「完了」に昇格させる。
- 全24コマンドが narou.rb と完全互換になるまで、この同期作業を継続する。
- Serena メモリにも常に最新の実装状況を反映する。
- **完了判定の注意**: `COMMANDS.md` の ✅ 完了は、Rust 側に該当処理や help 表示が存在するだけでは付けない。必ず Ruby 版 `sample/narou/lib/command/*.rb`（ローカルコピー）と、CLI オプション、help 文、Examples、設定項目、終了コード、エラー文、未実装の周辺動作を細かく突き合わせ、外部から観測できる挙動が一致していることを確認してから完了にする。
- 特に `help` は未実装コマンド分も narou.rb から移植する方針のため、Rust 側の実装済みコマンド集合と比較して完了判定しない。`narou <command> -h` の詳細文、Options、Configuration、Variable List、Examples を Ruby 版の各 command ファイルと比較して判断する。
- 既に ✅ と書かれているコマンドでも、同じ節に「未実装」「不足動作」が残っている場合や Ruby 版 help/挙動との差分がある場合は、実態に合わせて 🟡 部分へ戻す。完了度は楽観的に維持せず、互換性確認の粒度を優先する。

## コミット時のコード整形禁止ルール
- git diff に現れる変更は、機能的な意味を持つものだけにすること。
- コードの見た目だけを変える無意味な変更を禁止する。具体的には以下:
  - 既存の一行を複数行に改行+インデントし直すだけの変更
  - 既存の複数行を一行にまとめ直すだけの変更
  - `use` / `import` の順番を入れ替えるだけの変更
- これらの整形変更は、機能変更に付随して不可避な場合（例: 引数追加で行長が変わる）のみ許容する。

## グローバル設定の保存先 (2026-09 修正)
- `~/.narousetting/global_setting.yaml` は**ライブラリ状態ではない**ため、storage-backend が `sqlite` のときもファイルのまま維持する。管理状態の一覧 (`src/db/state_inventory.rs`、旧 `SQLITE_MANAGED_NAMES` / `MANAGED_LOCAL` / `STATE_FILES`) では `global_setting` が `FileOnly` で、SQLite への取込・退避 (`*.imported-*`) は行わない。
- 理由: narou.rb はこのファイルしか読まないため、退避すると narou.rb 側で `aozoraepub3dir` 等が消える。また SQLite 側の実体は「そのライブラリの `.narou/db.sqlite`」なので、別ライブラリ (YAML モード) からは設定が見えなくなる。
- 旧ビルドが `app_state(scope='global', key='global_setting')` に残した行は、ファイルが無いときに初回読み出しでファイルへ書き戻し、その行を削除する (一度きりの復旧)。`tests/global_settings_storage.rs` がモード往復と復旧を固定している。
- ローカル側 (`local_setting` / `freeze` / `alias` / `tag_colors` / `latest_convert` / `login_cookie`) は SQLite モード時も管理対象のまま (YAML モードでは従来どおりファイル)。`narou-compat` の挙動も変更なし。

## 設定データの I/O 境界
- `local_setting` / `global_setting` の本番コードからの読み書きは `src/db/settings.rs`（`load` / `save` / `update` / `value` 等）を共通入口とする。CLI・Web・converter・downloader・logger・init・self-update から設定 YAML を直接 `fs::read_to_string` / `fs::write` で操作しない。
- 共通入口の下では既存 `Inventory` が保存方式（SQLite `app_state` と legacy YAML）を選択する。`native::application::NativeSettingsStore` も同じ共通入口に委譲する。Worker 側は従来の `SettingsStore` port / D1 adapter を利用する。
- SQLite migration / compat 判定等のストレージ実装内部、`webnovel/*.yaml` のようなユーザー編集可能なサイト定義、`setting.ini` 等の小説固有入力は別用途なのでこの禁止の対象外とする。設定保存時は必要に応じて `update` で同時更新による上書きを防ぐ。
- 保存元と読み出し先の不一致を防ぐため、SQLite 有効時に `setting` で保存した `default.*` / `force.*` が converter に反映されることを回帰テストで確認する。

## ログ出力の境界 (2026-10 修正)
- ログファイルを書く `src/logger.rs` は **lib 側 (`narou_rs::logger`) だけにコンパイルする**。`main.rs` に `mod logger;` を戻すと `LoggerState` が 2 つになり、`logger::init()` が bin 側しか初期化しないため、lib 側の logger を通る行 — `MessageSink` (`progress::console_sink`) や Downloader のレポート行 — がログに残らなくなる (issue #36)。bin は `use narou_rs::logger;` で lib のロガーを共有する。
- コンソールへ出す行は **必ず logger 経由**にする。`safe_println` / `safe_stderr_println` は `STDOUT_LOCK` を取って素の stdout / stderr へ書くためログに残らない (`Downloader::report_line` の sink 未設定時フォールバック専用)。並列実行時の行混線は `ConsoleSink` が取る `STDOUT_LOCK` が防ぐので、ログへ残すために直書きへ戻す必要はない。
- narou.rb は `$stdout` への write をすべて `append_log` する (`lib/narou_logger.rb`)。「コンソールには出るがログに残らない行」を作らないこと。
- `list` / `diff` / `help` / `log` は `logger::without_logging` で意図的にログへ残さない。
- 回帰テスト: `tests/cli_logging.rs` (実バイナリで sink 経由行と bin 側行が同じログに入ること)。

## ログインが必要なサイト (フォールバック方式)
- 本体 (`narou_rs`) はログイン処理そのものを持たない。担うのは (1) ログインが必要かの判定、(2) どの小説の取得にログインが必要かの区別、(3) ログイン済み Cookie の更新の 3 点だけ。
- 通常は Cookie を送らない。取得に失敗したときだけ、保存済みのログイン Cookie を付けて 1 回再試行する。`404`（小説が消えた）またはサイト定義の `login_pattern` に一致するログイン壁が対象で、それ以外のエラーは従来どおり失敗させる。
- 再試行で取得できた小説はレコードの `requires_login`（SQLite `novels.requires_login` / `*.yaml` の `requires_login: true`）を立て、次回から最初のリクエストで Cookie を送る。オプション無しの小説は Cookie を一切送らないため、ログイン不要な小説の挙動は従来と変わらない。
- 再試行しても取得できない場合は従来どおり 404 判定（`frozen` / `404` タグ + `freeze.yaml`）へ進む。`requires_login` が立っている小説は「Cookie 付きで取得 → 失敗なら凍結」の順になる。
- サイト固有の値は `webnovel/*.yaml` に置く。追加キーは `login_url`（ログイン用 bin が開く URL、`\k<domain>` 補間あり）と `login_pattern`（HTTP 200 で返るログイン壁を検出する正規表現）。本体にサイト名・ドメイン固有の分岐は置かない。
- Cookie は `Inventory` の `login_cookie`（SQLite `app_state` / `.narou/login_cookie.yaml`）に **サイト単位** で保存する。1 サイトの値は名前つきログインの配列で、各ログインが複数ホストの Cookie を持つ（`LoginGroup { id, site, label, cookies: [{ host, cookie }] }`）。応答の `Set-Cookie` は、その応答で実際に送ったログインのホストに限り `src/native/http.rs` が書き戻してセッションを維持する（保存していないホストには新規エントリを作らない）。
- ログイン実行は別 bin `narou_rs_login`（`src/bin/login.rs`）が担当する。Chromium 系ブラウザを `--remote-debugging-port` 付きで起動し、DevTools protocol (`Storage.getCookies`) で Cookie を取得する（`ws://` のみなのでブラウザ自動化依存を追加しない）。2 段階認証や CAPTCHA は実ブラウザ操作なのでそのまま通る。ブラウザが無い環境向けに `--cookie "<Cookie 文字列>"` の貼り付け保存、`--list` / `--clear` も用意する。
- **別端末・サーバーへの持ち込み**: `narou_rs_login` はブラウザのある端末で動かし、`--export <file>` でポータブルな書き出しファイル (YAML) を作る。`--passphrase` 指定時は Argon2id → XChaCha20-Poly1305 で暗号化される。ライブラリ外では書き出しが既定の出力になる。取り込み側は `narou login import <file>`（CLI）または Web UI 設定の「ログイン」タブで受け付ける。
- **暗号化保存**: 保存値は `.narou/login.key`（または `NAROU_RS_LOGIN_KEY`）の鍵で `enc:v1:<nonce>:<payload>` として暗号化され、在庫のキー（サイト名）を AEAD の associated data に束ねるため別サイトへの流用はできない。旧形式の平文値は読み取り可能で、次回保存時に暗号化される（ホスト名で束ねられた旧暗号文もそのまま読める）。鍵は base64 で、32 バイトはそのまま、16 バイト以上（例: `openssl rand -base64 24`）は SHA-256 で 32 バイトへ伸長して使う。Worker も同じ値・同じ実装（`narou_rs::login::parse_key_base64`）を使うので、`NAROU_RS_LOGIN_KEY` に同じ文字列を入れれば native と資格情報を共有できる。
- **作者の追跡**: 追跡する作者は小説とは別のストア (`src/author.rs`、inventory `author` / SQLite `app_state` / レガシー `.narou/author.yaml`) に **作者ページ URL (ユニークなキー) → サイト名** だけを持つ。名前・追加日時・最終確認などの帳簿は持たない (表示や判定には不要で、サイト名も URL から引ける)。CLI は `narou author add <URL> | list | remove <URL|番号> | check`。`narou update` は対象を省略した一括更新の後 (hotentry の後) にだけ全作者を確認し、対象を指定した個別更新では確認しない。**まだ管理下に無い作品だけ**を通常の download 経路で追加する (`update.disabled-author-tracking` で停止可)。作品の見つけ方はサイト定義側: `author_url` が作者ページを認識し、`author_api_url` (任意) があればその URL を取得、無ければ作者ページ本体を取得して `author_novel_pattern` を当てる。`novel_url` を capture しない定義では `author_work_url` に capture を流して作品 URL を作る (`\k<lower:ncode>` で小文字化。なろう API は大文字の ncode を返すため)。一覧がページ分けされるサイト (ハーメルン: `page=2, 3…`) は `author_next_pattern` の `author_next` capture を次ページとして辿る。終了条件は「次ページが無い」「既に訪れた URL に戻った」だけで、ページ数の上限は設けない (訪問済み集合が往復ページャを止める)。作者ページを扱う定義が複数ある場合は全部から集める (なろう と R18 版)。作者ページの HTML を直接読むより API を優先するのは、壊れにくくページ送りも不要なため。Pixiv (`www.pixiv.net`) は正規表現だけでは作品を切り分けられないため、`author_api_url` (`/ajax/user/{id}/profile/all`) を `preprocess:` の DSL に通し、`author_novel::<作品URL>` (単体小説 + 小説シリーズ + 漫画シリーズ + 単体イラスト・漫画) と `author_series::<シリーズID>` / `author_comic_series::<シリーズID>` を emit する (漫画シリーズは `/ajax/series/<id>`、単体イラストは `/artworks/<id>` の形)。作者の作品一覧にはシリーズに属する話/ページも混ざるので、narou 側は `author_series` ごとに `author_series_episodes_url` (`/ajax/novel/series/{id}/content_titles`) を、`author_comic_series` ごとに `author_comic_series_pages_url` (`/ajax/series/{id}?p=N&lang=ja`、12 件ずつなので空ページまで辿る) を読み、その分を作品一覧から除く (取得できなかったシリーズは除かずに残す)。`narou author check --dry-run` は追加予定の URL を並べるだけで DL しない (数千作品の作者でも確認できる)。Pixiv は匿名の `profile/all` が「完全な一覧」に見えても R-18 作品を返さないため、`author_login_merge: yes` で匿名分と保存済みログイン分を統合する。
- **作者追跡の保存境界**: `application::authors::{AuthorStore, AuthorService}` と `author::{authors_from_value, authors_to_value}` が共通契約。native は `NativeAuthorStore` → `Inventory::update_yaml`、Worker は `D1AuthorStore` → primary D1 の `app_state('inv','author')` を全 raw snapshot CAS で更新する。作者の追加・解除が同時に走っても別の登録を消さない。小説レコードの `author` 表示名は追跡登録ではない。
- **Workers の継続追跡**: 一括更新 (`/api/update` の update-all と対象なしの汎用 Update ジョブ) と定期更新の最終ページから、登録作者ごとの `author_check` ジョブを投入する。個別更新では投入しない。小説 0 件でも確認し、`update.disabled-author-tracking` は自動確認だけを止める。実効 YAML・Cookie・待機ポリシーは既存 `DownloadTargetExpander::author_works` を共有し、`NovelRepository::contains_toc_urls` で管理済み作品を除外して通常 Download ジョブへ流す。D1 は primary の URL 索引を 100 URL ずつ検索し、作品レコード全件を読み込まない。実行時と一覧取得後に追跡登録を再確認し、解除済みの古い確認ジョブはスキップする。すでに投入済みの通常 Download ジョブは作者解除では取り消さない。
- **作者の CLI / Web**: CLI は Native のローカルライブラリだけを操作する (`narou author add|list|remove|check`)。Workers 向けの CLI・Native CLI から Workers へのリモート接続機能は提供しない。番号は 1 始まりで 0 を拒否する。Native・Workers 共通の設定「作者」タブと `GET/POST/DELETE /api/authors`、`POST /api/authors/check` を提供し、Workers の作者操作はこの Web UI と API を使う。解除は追跡だけを消して作品を保持し、単発の作者 URL ダウンロードを継続登録へ変えない。
- **セッション ID と小説の対応**: 保存したログインには UUID を振り (`LoginGroup.id`)、小説レコードは「どのセッションで成功したか」を `login_session` に持つ (SQLite `novels.login_session` / `*.yaml` の `login_session:`)。`requires_login` は「Cookie が要る」、`login_session` は「どれを使うか」を表す。フラグ付きの小説は次回以降、その ID の資格情報を最初のリクエストから送るので、一覧を毎回総当たりしない。ID が無い旧データはストア読み込み時に採番して書き戻す (小説側が覚える値なので不変)。採用した資格情報が消えていた場合は先頭にフォールバックし、次の成功で ID を書き直す。
- **複数ログインと試行順**: 保存は 1 サイトにつき **順序つきのログインリスト**（`LoginGroup`）で、並び順がそのまま試行順になる。ダウンロード時は保存済みを順に試し、ログイン壁は成功した時点で、部分一覧は「欠けが消えた／話数が増えた」時点で打ち切る。採用したログインはその後の本文取得にも使う。送るヘッダは `merged_cookie()` が各ホストの Cookie を 1 本に畳んで作り、名前が衝突したときは具体的なホストを優先する。`Set-Cookie` の書き戻しは「その応答で実際に送ったログイン」だけを更新する（同じサイトの別アカウントのセッションを壊さないため）。
- **CLI / Web**: `narou login list` はサイトごとに番号・名前・短縮 ID・ホスト数を表示する。`import <file> --name <名前>`（そのファイルが持ち込むログインに名前を付ける）、`rename <site> <番号> <名前>`（空文字で名前を消す）、`order <site> 2,1,3`（1 始まりの番号で試行順を指定。全件指定が必要）、`clear [<site>] [--index N]`（サイト内の 1 件、またはサイトごと削除）、`clear`（全削除）を備える。Cookie の直接登録は廃止し、取り込みとブラウザ取得 (`narou_rs_login`) だけが登録経路。Web UI の設定ページ「ログイン」タブも同じ操作（取り込み・名前変更・上下ボタンでの並べ替え・1 件削除・サイト削除・全削除）ができる。API は `GET /api/login`、`POST /api/login/import|rename|order`、`DELETE /api/login`、`DELETE /api/login/{site}`、`DELETE /api/login/{site}/{index}`。
- **書き出し形式**: `narou_login_export.yaml` は version 3（`sites:` にサイト → ログインの並び）。version 2（`credentials:` にホストごとの 1 本、同じ位置が同じアカウント）と version 1（`cookies:` の host→cookie マップ）も読める。サイト定義が読めない環境では、まとめ先が分からない旧データはホスト単位のまま残す。
- **取り込み単位で名前をつける**: `narou login import <file> --name 本垢`（Web UI は取り込み欄の名前入力。空欄なら選択したファイル名から拡張子を除いた名前）で、そのファイルが持ち込むログインに名前が付く。ホストごとに分かれて書かれた旧形式（版 1 / 版 2）は読み込み時にホストごとに並べ直して 1 ログインへ畳むので、**1 ファイル = 1 セッション = 1 名前**になる（1 ファイルに複数セッションがあるときだけ `名前 1`, `名前 2` と番号が付く）。取得時に付けたければ `narou_rs_login --name 本垢`。
- **過去に分解されたデータの修復**: 保存済みのログインが「ホストが重ならず Cookie 名も衝突しない」組み合わせなら、読み込み時に 1 ログインへ畳んで書き戻す（別アカウントは Cookie 名が衝突するので分かれたまま残る）。
- **Cookie の取得範囲**: 取得側はサイトのドメインファミリー（サイト自身・親ドメイン・兄弟サブドメイン）を **1 つのログインにまとめて** 保存する。Pixiv のように `.pixiv.net` にセッションを置くサイトで `www.pixiv.net` だけを見るとセッションを取り落とす。まとめ先のサイト名はサイト定義のドメインで決め、定義が無ければホストをそのまま使う（親ドメインのキーは配下の定義があればそこへ寄せる）。`Set-Cookie` の書き戻しも同じログインへ行う。
- **ブラウザプロファイル**: `narou_rs_login` は**毎回まっさらな一時プロファイル**（`%TEMP%/narou-rs-login-<pid>`）でブラウザを開く。前回のログインを持ち越すと、別アカウントで入るためにログアウト→ログインを挟むことになり、その途中でセッションが切れやすい。Cookie を保存・書き出ししたあとプロファイルは削除する（取得に失敗したときは残すので、原因を見たり再試行に使える）。使い回したい場合だけ `--profile <DIR>` を指定する（ブラウザの保存ログインや拡張を使いたいとき）。取得後は対象 URL を 1 回取得し、サイト定義の `error_message` / `login_pattern` に当たる場合は「ログインできていない」と警告する（Python ブリッジの `/dashboard` リダイレクト判定に相当）。
- 配布物: `narou_rs_login` もリリース zip に同梱する（`scripts/package-release.ps1` の `-LoginBinaryPath`、`.github/workflows/release.yml` の helper build / sign / package、`cargo local-build` のすべてに対応済み）。Windows では他のサブ実行ファイルと同じく署名対象に含める。`scripts/package-release.ps1` は `-Platform win` のとき本体・updater・backup・login の Authenticode 署名を梱包前に検証し、未署名なら失敗する（`-SkipSignatureCheck` は署名できないローカル確認専用で、リリース CI からは指定しない）。

## セキュリティ境界 (2026-10 強化)
- **Native Web UI の loopback 認証**: `127.0.0.1` / `localhost` / `::1` bind かつ Basic 認証なしのとき、`narou web` は起動ごとの一度きりチケットを URL に付けてブラウザへ渡し、`/` で HttpOnly / SameSite=Strict / 30 日有効の `narou_ui_token` Cookie に交換する。`/api/*`・`/ws`・`/novels/{id}/download` はこの Cookie または `x-narou-internal-token`（`.narou/server.pid` の制御トークン）を要求する。静的なページと `/assets/*` は公開のまま。トークン本体は `.narou/ui_token`（0600）に永続化し、再起動・セルフアップデート後もブラウザのセッションを維持する。`local_ui_auth_required()` は固定フラグではなく現在の `ServerSecurity` から毎回判定する。**reverse proxy モードでも loopback ならトークンを要求する**（2026-10 修正）：同じ端末の別プロセスは `Host` / `Origin` を自由に書けるため、これらは認証の代替にならない。チケットは使う origin（Host）ごとに 1 回だけ有効で、reverse proxy 経由の公開ホストと直接アクセスの双方を開ける。詳細は `WEBUI.md` §8.1.1。
- **セルフアップデート**: Web API から更新 URL を指定する経路 (`asset_url`) は廃止した。取得先は GitHub Releases API の `browser_download_url` のみで、Release asset の `sha256` digest とサイズをダウンロード後に照合し、欠落・不一致はインストールせず中止する。`.narou/ui_token` と `.narou/login.key` はライブラリバックアップから除外する。
- **zip 展開のパス判定** (2026-10-28): `zip` クレートの `ZipFile::enclosed_name()` は Windows で予約デバイス名 (CON, NUL, ...) を判定するために `stem.split_at(3)` を使い、CP437 で復号した名前がバイト境界をまたぐとそこで **panic する** (`zip` 9.1.2 / 9.1.3 とも未修正)。ZIP は入手したファイルなので、展開処理が panic すると更新が途中で本体を壊した状態で止まる。判定は `src/bin/updater.rs` の `safe_entry_path` で行う (`stem.split_at(3)` を使わず、`..` の脱出・絶対パス・ドライブ接頭辞・予約デバイス名の拒否規則は `zip` 側と同じ)。`enclosed_name()` は他のコードでは使わない。
- **reverse proxy**: `server-reverse-proxy.enable=true` でも Host の許可リストは維持する。プロキシ配下の公開ホストは `server-add-accepted-hosts` に、WebSocket は `server-ws-add-accepted-domains` に明示する。外部 bind + Basic 認証は平文 HTTP なので、LAN 外公開時は TLS 終端 proxy を使う（起動時に警告）。全レスポンスに `X-Frame-Options: DENY` / `frame-ancestors 'none'` / `nosniff` / `no-referrer` を付ける。
- **ログイン資格情報**: 書き出し (`login export` / `narou_rs_login --export`) は 8 文字未満のパスフレーズを拒否し、ファイルは Unix 0600・シンボリックリンク非追従で書く (取り込みは旧ファイル互換のため任意長を受け付ける)。ライブラリの鍵 `.narou/login.key` は **作成時から 0600**（`OpenOptionsExt::mode`）で、作成後に chmod する方式の窓を作らない (2026-10 修正)。`narou_rs_login` のブラウザは `--remote-debugging-port=0` で起動し、`DevToolsActivePort` と `/json/version` の ws URL が起動したプロセスの port/host/path と一致することを確認する。SQLite の `db.sqlite` と `-wal` / `-shm` は Unix で 0600 にする。
- **お気に入りタグ** (issue #46, 2026-10): Web UI の「タグの編集」画面でお気に入りタグを登録し、検索なしで選べるようにする。共有モデルは `src/application/favorite_tags.rs` (native / Worker 共通の保存形式と検証)、native のストアは `src/native/favorite_tags.rs`、Worker の D1 ストアは `worker_entry/src/favorite_tags.rs`、API は `GET/POST /api/favorite_tags` (`action`: `add` / `remove` / `clear` / `reorder`)。保存先は在庫の `favorite_tags` キー (Native は `.narou/favorite_tags.yaml` / SQLite・Worker は `app_state('inv','favorite_tags')`) で登録順を保持する。タグ色とは独立した設定で、どちらも互いを変更しない。
  - **並べ替え** (issue #46, 2026-10-28): お気に入り一覧はドラッグ＆ドロップで並べ替えられる。`reorder` は `order` に載ったタグだけを元の位置同士で並べ替え、載っていないタグは位置を保つ (未知のタグは無視)。
- **タグ一覧の並び順** (issue #46, 2026-10-28): メインメニューの「タグ」の並び順は `webui.tag-sort-order` (既定 `favorites_first`) で決める。値は `favorites_first` / `alphabetical` / `count` / `registered`。並び替えの共通実装は `src/application/webui/tag_order.rs` (`TagSortOrder` / `sort_tags`)、native / Worker 共通。「登録順」の連番表は `src/application/tag_sequence.rs` (`TagSequence` / `library_tags` / `registered_ranks`) が持ち、native と Worker が同じ規則を使う。設定ページは `setting_info.rs` の `webui` タブに選択項目として出る。
- **タグの登録順** (issue #46, 2026-10-29): 「登録順」は各タグが付いた小説のうち ID が最小のものを登録順としており、古い小説へ後からタグを付けたときや ID の小さいほうへ後からタグを追加したときには「タグがライブラリへ入った順」と食い違っていた。タグごとにグローバルな連番を振り、在庫の `tag_sequence` キー (Native は `.narou/tag_sequence.yaml` / SQLite・Worker は `app_state('inv','tag_sequence')`) へ保存する。native のストアは `src/native/tag_sequence.rs`、Worker の D1 ストアは `worker_entry/src/tag_sequence.rs` (スナップショット CAS)。登録順の更新は記録が確定してから行う: native は `src/db/database.rs` の `update_records` / `save`、Worker は `worker_entry/src/d1_repository.rs` の `apply_batch`。既存ライブラリには連番が無いので、最初の書き込みで最小小説 ID → タグ名の順にバックフィルし、以後の新しいタグは登録順の末尾に足す。内部タグ (`vs_<ID>`) は連番表に含めない。
- **Worker / relay**: `NAROU_RELAY_ENDPOINT` は https 必須（ループバックのみ http 可）。スキーム省略の暗黙 `http://` 補完はしない（`platform::relay::normalize_relay_base`、違反は fail closed でリレー無効）。deploy CI の wrangler は固定バージョン (`NAROU_WRANGLER_VERSION`、既定 `4.151.3`) を使い、`NAROU_ADMIN_TOKEN` は CI で 32 文字以上を要求する。
- **上流コンテンツ**: うごイラのフレーム ZIP は宣言サイズと読み出し上限（16 MiB/フレーム）およびヘッダ寸法で復号前に検証する。EPUB に取り込む非リモート `<img src>` は novel ディレクトリ内の相対パスだけを許可する。
- **外部応答の Brotli 展開**: reqwest の `brotli` feature は有効にしない (CVE-2026-111497 は Google Brotli の C 実装 1.0.3〜1.2.0 が対象で C 版はリンクしていないが、pure Rust 実装も修正前と同じデコーダ経路を持つため、外部サイトの応答を展開する経路を落とす)。HTTP 取得は gzip / deflate / identity を使う。保存オブジェクト (`objects` / `section_bodies` など) の brotli 圧縮・伸長は自前データのみなので継続する (内部経路は `brotli` 9.1.0 / `brotli-decompressor` 6.2.0 を使う。これは CVE-2026-111497 対応版で、該当しうる入力に正しいエラーコードを返す。壊れた brotli ペイロードが panic ではなくエラーになることは `src/platform/object_store.rs` の単体テストで固定する)。外部取得リレーも `curl --compressed` を使わず、`Accept-Encoding: gzip, deflate` を明示して gzip / deflate を Rust (`flate2`) で展開し、`br` は fail closed する (`relay/src/transport/curl.rs`)。2026-10。

## 非表示・並列DL・転送 (v0.5.0)

- **非表示設定**: 在庫 `hidden` に `novels` / `tags` / `authors` / `sites` を持つ (`HiddenState`)。Native は `.narou/hidden.yaml` / SQLite `app_state`、Worker は D1 `app_state`。一覧は `NovelFilter.exclude_*` で除外し、`include_hidden` が true のときだけ非表示も表示する。タグ非表示はそのタグの作品も隠す。更新・変換などの内部列挙は非表示作品も対象。CLI は `narou hidden` / `list --show-hidden`、Web UI は「非表示も表示」・右クリック・非表示設定モーダル。
- **並列DLの系列**: `SiteSetting.site_family`（YAML）を第一優先し、未指定時は `platform::site_family` がドメインから自動判定する。なろう系は `syosetu.com`、Pixiv は `pixiv.net` を同梱定義に明記。Native `update` の domain groups、Worker の D1 admission `site_key`、Native Web のキュー worker が同じ系列で直列化する。異なる系列は並列。
- **小説データ転送**: 形式と検証は wasm-safe な `src/transfer`（manifest + `novels/<i>/...`、決定論的 ZIP、パス/サイズ/zip bomb/SHA-256 検証、`plan_import`）。Native は `src/native/transfer`、CLI は `narou export/import`、Native Web は `/api/transfer/*`、Worker は ObjectStore 経由の同 API、ブラウザは `transfer_wasm`。競合は `toc_url`、既定 Skip / `--overwrite` で置換。上書きはアーカイブにあるファイルだけを置換し、既存の余分なファイルを削らない。**ZIP の中身 (manifest スキーマ・検証ルール・衝突規則・同梱対象・挿絵の扱い) は `docs/transfer-archive-format.md` にまとめる** (形式を変えるときはここを先に更新する)。
- **バーチャル小説**: `vs_<ID>` の通常 `novels` 行として登録し、`ncode=vs_<ID>` / タグ `vs_<ID>` を持つ。メンバー順は在庫 `virtual_novel_members`、ダウンロード・更新の対象外。表示一覧ではメンバー行を除外する。`convert vs_<ID>` はメンバーの toc/本文を一時ディレクトリで連結し、各メンバータイトルを章見出しとして 1 EPUB に結合する。CLI は `narou virtual add/append/list/order/remove`、Web API は `/api/virtual_novels` と `POST /api/virtual_novels/{id}/members`。`append` はメンバーを末尾へ追加し、仮想小説のタグ・話数・最新話掲載日を再計算する (native `native::virtual_novel::add_members` / Worker `worker_entry/src/virtual_novels.rs` の `add_members`、検証は core `virtual_novel::VirtualNovelMembers::append`)。Web UI はバーチャル小説を 1 件だけ選択していれば ncode を求めずその作品を対象にし、そうでなければ一覧から選ぶダイアログを出す (`selectedVirtualNovel`)。
- 設計の全体像は `docs/v0.5.0-architecture.md`。

## 機能追加・修正時の動作テスト

- **新機能を追加した場合、または既存の挙動を修正した場合は、必ず動作テストを徹底的に行う。** コンパイルが通っただけ、小さな変更だから、時間がないから、CI が回るから、などの理由でテストを省略してはならない。
- CLI / Web UI / Worker / Native / wasm のうち影響する経路すべてについて、実際にその経路を動かして確認する。UI 変更はブラウザでの操作確認、Worker 変更は workerd 契約テスト、変換変更は生成物の確認まで行う。
- 追加した不具合修正には、修正前に失敗し修正後に成功する回帰テストを追加する。既存テストが通ることだけで済ませない。
- テストを実行できなかった場合は、その事実と理由を明示し、完了扱いにしない。

## バージョン更新時の必須作業

- **番号のどの桁を上げるかで必須作業が変わる。** 0.x.0（マイナーバージョン）の更新では依存更新が必須だが、0.0.x（パッチ版。0.5.3 → 0.5.4 など）では必須ではない。
- **0.x.0（マイナーバージョン）の更新ごとに、必ず次の3つを行う。**
  1. `cargo clippy --workspace --all-targets --all-features -- -D warnings` とビルドで警告を確認し、警告を解消する。依存更新で新しく出た警告も対象。
  2. `cargo update` でワークスペースの依存を更新する。`about-probe` も `cargo update --manifest-path about-probe/Cargo.toml` で更新し、両方の `Cargo.lock` を同じコミットに含める。
  3. 依存更新後に `cargo test --workspace` / clippy / Worker wasm / ライセンス再生成 (`cargo about`) を実行し、結果を確認する。更新で壊れた API はこの時点で直す。
- **0.0.x（パッチ版）では、依存更新 (`cargo update`) とライセンス再生成 (`cargo about`) は必須ではない。** 既存依存のままリリースしてよい。依存を変更した場合に限り、`Cargo.lock` / `about-probe/Cargo.lock` と `Third-Party-License*.md` を同じコミットで更新する。
- パッチ版でも `cargo clippy --workspace --all-targets --all-features -- -D warnings` と `cargo test --workspace` による確認は行う（依存更新を伴わないなら clippy の警告対応は不要だが、既存の警告がないことを確認する）。
- 依存更新はリリースとは独立した単位でよい。依存更新だけのコミットをリリースのコミットに混在させない。

## Git 運用ルール
- 通常の修正・軽微な機能追加・ドキュメント更新は `develop` 上で行う。作業開始前に現在ブランチと作業ツリーを確認し、`main` 上で直接作業しない。
- 作業開始時に対象ブランチが `origin` より遅れている場合は、`git pull` で最新へ追従してから作業を始める。
- 大幅な変更、新機能、複数ファイルにまたがる設計変更、長時間かかる検証を伴う作業は、`develop` から機能単位のブランチを作成して進める。
- 機能ブランチ名は内容が分かる短い英数字・ハイフン形式にする。例: `fix-web-concurrency`, `feature-series-url`。
- 機能ブランチでは適切な動作テストを済ませてから `develop` に統合する。統合後も `develop` 上で必要なテストを再実行する。
- `main` への統合は、ユーザーが明示的に依頼した場合、またはリリース作業として明確に合意された場合だけ行う。`develop` は削除せず残す。
- `main` へ統合する前に、`develop` が clean であること、必要なテストが通っていること、バージョン更新や `VERSION.md` の変更履歴などリリースに必要な差分が揃っていることを確認する。
- **変更履歴は `VERSION.md` に置く**。`README.md` には変更履歴の本文を書かず、`VERSION.md` への参照だけを置く。リリース時は `VERSION.md` の先頭へ新しいバージョンの節を追加し、`README.md` は触らない (バージョン更新そのものは `Cargo.toml` / `worker_entry/Cargo.toml` / `Cargo.lock` / `about-probe/Cargo.lock` / `Third-Party-License*.md` に行う)。
- **版数の基準は GitHub の tag / Release とする**。ローカルの `Cargo.toml` や `VERSION.md` に未リリースの次版が書かれていても、それを公開済みとしてさらに版数を進めない。ユーザーから「バージョンを上げて」と指示されたら、GitHub の最新 tag / Release の次の版をリリースする（例: ローカルが 0.4.18 でも GitHub の最新が v0.4.17 なら v0.4.18 でリリースする）。公開前に追加する機能・修正・ツアーは同じ未リリース版へまとめる。ユーザーが版数を指定した場合はその指定を優先する（2026-10-17: 公開済み v0.4.12 に対する次版は v0.4.13）。
- タグ作成はユーザーがバージョン番号を明示した場合だけ行う。`main` へ統合した後、`main` 上でのみタグ作成を許可する。タグは `main` のリリースコミットを指すようにし、作成後に push する。
- 実装が一区切りついたら、機能単位で git commit する。無関係な変更をひとつの commit に混ぜず、レビューやロールバックがしやすい粒度に分ける。
- commit 前には `git diff` / `git status` を確認し、ユーザー由来または別作業由来の変更を混ぜない。意図しない整形差分、改行だけの変更、import 並び替えだけの変更を含めない。
- commit メッセージは英語の短い命令形または要約形にする。例: `Fix web download concurrency`, `Document release setup steps`。
- push は原則として作業単位の commit 後に行う。ユーザーが「push しないで」と明示した場合は commit までに留め、push しない。
- `develop` で作業した commit は `origin/develop` に push する。機能ブランチで作業した場合は、そのブランチを push し、`develop` 統合後に `origin/develop` も push する。
- `main` 統合後は `origin/main` を push する。リリースタグを作成した場合はタグも push する。
- **バージョン更新時は必ず `cargo check` を実行し、ビルドが通ることを確認してから commit・push・タグ作成を行う。** `Cargo.toml` のバージョン更新と `cargo check` による `Cargo.lock` 更新は同じ commit に含める。
- `git reset --hard`、`git checkout --`、強制 push、履歴改変 rebase は、ユーザーが明示的に依頼した場合以外は行わない。
- ブランチ削除はユーザーが明示的に依頼した場合だけ行う。特に `develop` は残す。

## GitHub issue の調査・返信ルール
- issue は**本文だけでなく最新の全コメント・追記まで読む**。追加された再現条件、旧版／新版・既存データ／新規データの違い、質問・懸念を整理し、それらを落とさず調査・検証・返信へ反映する。
- 返信は報告された OS・実行形態・保存方式・利用機能を基準にし、**その環境での理解・対処に必要な情報だけを詳しく書く**。Native / Workers の区別が書かれていなければ Native として扱う。Windows の Native 版の報告に、関係のない Workers の実装・配備、Wasm、D1、別環境向けの CI 結果などを持ち込まない。共通コードを使っていることだけを理由に説明範囲を広げない。
- 返信前に、同じリポジトリの過去の返信を実際に読み、文体・説明の粒度・利用者への配慮を参考にする。基準例は [issue #35 の返信](https://github.com/Rumia-Channel/narou.rs/issues/35#issuecomment-6052141382) と [issue #36 の返信](https://github.com/Rumia-Channel/narou.rs/issues/36#issuecomment-6093885631)。関連する過去 issue があればその返信も確認し、見出しだけの模倣や機械的なコピペはしない。
- 利用者への返信は丁寧な日本語で書き、報告へのお礼と対応状況を冒頭で伝える。「修正しました」、コミットリンク、テスト結果の羅列だけで済ませず、報告者が症状と対応内容を理解して試せる説明にする。
- 原則として「原因」「修正内容」「確認したこと」「お試しいただく際の注意」などの見出しで整理する。必要な内容を過不足なく書き、形式を埋めるための空疎な節や無関係な内部実装の説明は増やさない。
- 原因は**報告された現象との因果関係**を説明する。「既存作品だけ反映されない」「以前は動いた」などの差がなぜ生じるか、仕様変更なのか不具合なのかを具体的に答える。報告者の追加質問を技術説明の中に埋もれさせず、質問ごとに明確に回答する。
- 確認したことには再現条件、修正前後の違い、実際に動かした CLI／Web UI、確認した出力・保存状態、回帰テストの範囲を記す。検証用 fixture での結果と報告者の実環境を区別し、未確認の条件・推測・検証限界を隠さない。未実行の検証や未確認の原因を断定しない。
- データ消失・設定の残存・復旧が関係する場合は、既存データへの影響と、必要な復旧・再実行の具体的な手順を説明する。該当すれば dry-run、バックアップ、不可逆な操作の注意も示し、実証していない手順を案内しない。
- 修正コミットと、利用者が試すべきバージョンを明示する。`develop` で修正済みでも公開済みとは書かず、未リリースならその旨を伝える。再発時は、この issue へ連絡してほしいことと、切り分けに必要なコマンド・ログ・設定などを具体的に案内し、Cookie・鍵・認証情報は求めない。
- 投稿直前に、本文・全追記への回答漏れ、根拠と説明の一致、バージョン・公開状況・リンク、利用者が次に行うことを確認する。短い完了報告で打ち切らず、過去の丁寧な返信と同等に、調査結果と利用者向けの案内を揃えてから送信する。

## サブエージェント運用ルール
- サブエージェントを使うのは、広範囲の監査、複数の独立トラックへ分解できる実装、並列化メリットが明確な作業に限る。
- **1ファイル編集や、ごく少数ファイルで完結する軽微修正では、サブエージェントを呼ばずメインエージェントが直接処理すること。**
- サブエージェントを使う必要がある場合は、作業内容に適したモデルを選択してよい。

## CSS 変数ルール
- WEB UI の CSS で色・サイズ・間隔等を指定する際は、ハードコード値ではなく必ず `var(--xxx)` 形式の CSS 変数を使うこと。
- 変数は `base.css` の `:root` や各テーマで定義されたものを参照する（例: `var(--navbar-bg)`, `var(--text-color)`, `var(--container-padding)`）。
- 新しいページや要素を追加する場合もこのルールに従い、テーマ切り替えに対応した記述にすること。

## CSS 単位ルール
- WEB UI の CSS でサイズ・間隔・余白・フォントサイズ等を指定する際は、`px` のような画面解像度に依存する絶対単位を使わず、`em`・`rem`・`%`・`vw`・`vh` などの相対単位のみを使うこと。
- これにより、異なる解像度・DPI・フォント設定でも UI が適切にスケールする。
- `@media` クエリのブレークポイントには `em` を使う（例: `@media (max-width: 48em)`）。

## Dependency Policy
- `Cargo.toml` は原則として直接編集しない。
- 依存クレートの追加・更新は `cargo add`、`cargo update` など Cargo のコマンド経由で行い、その時点で取得できる最新の互換バージョンを使う。
- 例外的に `Cargo.toml` の手編集が必要な場合は、先に理由を明確化し、変更後に `cargo check` などで検証する。

## Init / Local Data Compatibility
- `narou init` は narou.rb の `Command::Init` / `Narou.init` / `Inventory` を参照して実装する。
- 新規初期化では `.narou/`、`小説データ/`、ユーザー編集用の `webnovel/` を作成し、同梱 `webnovel/*.yaml` を初期コピーする。
- `.narou/` 配下の `local_setting.yaml`、`database.yaml`、`database_index.yaml`、`alias.yaml`、`freeze.yaml`、`tag_colors.yaml`、`latest_convert.yaml`、`queue.yaml`、`notepad.txt` は narou.rb の Inventory 互換ファイルとして扱う。
- `local_setting.yaml` は Ruby 版と同じく任意設定の置き場であり、初期化時に大量のデフォルト値を書き込まない。既定値は各読み取り処理側で narou.rb に合わせて解釈する。
- 端末上で `narou init` を実行した場合は、Ruby 版と同じく AozoraEpub3 の場所と行の高さを対話式に質問する。非対話環境では入力待ちせず、既存設定がなければスキップする。
- `narou init -p/--path` は指定先に `AozoraEpub3.jar` がある場合だけ `~/.narousetting/global_setting.yaml` に保存する。`-p :keep` は既存の有効な `aozoraepub3dir` を再利用する。
- `narou init -l/--line-height` は AozoraEpub3 設定が保存される場合だけ `line-height` として保存し、未指定時は Ruby 版の非対話デフォルトに合わせて `1.8` を使う。
- 有効な AozoraEpub3 パスを設定した場合は、Ruby 版と同じく `chuki_tag.txt` のカスタム注記追記/置換、`AozoraEpub3.ini` のコピー、行高反映済みカスタム CSS のコピーを行う。カスタム CSS の置き場所は AozoraEpub3 のテンプレート構成で変わる: 元祖レイアウト (`template/OPS/css/` がある) は `template/OPS/css_custom/vertical_font.css`、1.1.1 以降の改造版 / 電書協レイアウト (`template/item/style/` がある) は `template/item/style_custom/font.css`（中身は `preset/denshokyo_font.css`）。判定は `converter::dakuten_font::AozoraTemplateKind` が唯一の定義で、`init`・濁点フォント切替・変換時の不足補完・`lite` エンジンへの同梱 CSS がこれに従う。OPS パス固定に戻すと二分アキ (`［＃二分アキ］`)・行高・前書き/後書きの装飾が EPUB に反映されない (issue #42)。

## Build & Run
```powershell
cargo build              # Build (edition 2024)
cargo run -- convert 2  # カクヨム小説を変換（CWD: sample/novel/）
cargo run -- convert 1  # なろう小説を変換
cargo check              # Type-check
```

**重要**: `cargo run` は `.narou/` を持つ初期化済みライブラリをCWDとして実行する必要がある（例: `sample/novel/`、gitignore 済みのローカル用ディレクトリ）。

## 検証用ライブラリ
- 動作確認は **`C:\Users\rumia\Documents\WebNovel`** で行う。実データ入りの narou ライブラリ（なろう / カクヨム / ノクターンの 9 作品、`webnovel/*.yaml` も同梱版を配置済み）で、ユーザーが「中のデータの削除も含めて完全に自由にしていい」と許可している。検証用に小説を追加・削除してよい。
- `%TEMP%` に検証用ライブラリを作らない（過去に 100 個・685MB の残骸を出した）。テストや検証が一時ディレクトリを作るときは `tempfile::TempDir` を使い、panic しても残さない（手動削除は panic で飛ぶ）。

## Edition 2024 注意事項
- `{}`フォーマット直後に文字列を書くとprefix扱いされるためスペースが必要
- 特に `regex::Regex::new(r"...").unwrap()` の直後に `.` で始まる式を書くとコンパイルエラーになる
- セミコロンで終わらせるか変数に代入すること

## Project Structure
```
src/
  main.rs                          - CLI entry point (thin dispatcher)
  cli.rs                           - clap定義 (Cli struct + Commands enum, 引数前処理)
  error.rs                         - NarouError enum + Result type
  queue.rs                         - PersistentQueue (YAMLベース永続化ジョブキュー)
  epub_lite.rs                     - AozoraEpub3_Lite 組み込み EPUB 生成 (feature "lite", ストリーミング書き出し)
  illustration_animation.rs        - うごイラ等アニメ挿絵の APNG 組み立て (feature "illustration-animation")
  illustration_store.rs            - 挿絵キャッシュ index + IllustrationStorageService (AssetStore)
  assets/aozora_lite/              - 同梱 chuki テーブル (GPL-3.0-only, Lite由来)
  lib.rs                           - クレートルート (pub mod定義)
  application/                     - Web/Worker 共通の use-case 層 (jobs/novel_actions/settings/scheduler/self_update 等)
  login/                           - ログイン Cookie 保存・暗号化・export/import (crypto/mod/transfer)
  platform/
    mod.rs                         - プラットフォーム抽象層 (traits re-export, 設計: docs/platform-abstraction.md)
    http.rs                        - HttpClient trait + HttpRequest/HttpResponse (coreはreqwest/curlを直接呼ばない)
    clock.rs                       - Clock trait + SystemClock
    rate_limiter.rs                - RateLimiter trait + RateLimitScope
    object_store.rs                - ObjectStore trait + ObjectKey/ObjectMetadata
    repository.rs                  - NovelRepository trait + NovelId/NovelQuery
    mocks.rs                       - MockHttpClient / MemoryObjectStore / MemoryNovelRepository / FakeRateLimiter
  native/
    sqlite/                        - SQLite管理基盤 (repository/state/bulk/content/versions/object_store/migrations)
    mod.rs                         - native 実装 (core から参照しない)
    http.rs                        - NativeHttpClient (3-tier: curl crate → reqwest → wget fallback, spawn_blocking 隔離)
  commands/
    mod.rs                         - pub mod + resolve_target_to_id, resolve_alias_target
    init.rs                        - narou init (ディレクトリ作成, AozoraEpub3設定)
    download.rs                    - narou download
    update.rs                      - narou update
    convert.rs                     - narou convert
    web.rs                         - narou web (Axumサーバー起動)
    list.rs/manage.rs              - narou list (manage.rs に tag/freeze/remove も同居)
    tag.rs, freeze.rs, remove.rs   - (manage.rs 内に統合)
    setting.rs                     - narou setting
    diff.rs, send.rs, mail.rs      - diff / send / mail
    backup.rs, clean.rs            - backup / clean
    help.rs, version.rs            - help / version
    log.rs, trace.rs               - log / trace
    alias.rs, folder.rs, browser.rs - alias / folder / browser
    inspect.rs, csv.rs             - inspect / csv
    db.rs                          - narou db (verify / export-yaml / vacuum)
    login.rs                       - narou login (list/import/export/set/add/order/clear)
    web_tray.rs                    - Windows タスクトレイ
  db/
    mod.rs                         - シングルトン (DATABASE static, init_database, with_database/mut)
    database.rs                    - Database struct (CRUD, sort, tag index)
    novel_record.rs                - NovelRecord struct (45フィールド, nilable bool対応)
    inventory.rs                   - Inventory (LRU cache, atomic write, Windows retry)
    index_store.rs                 - IndexStore (SHA256 fingerprint)
    paths.rs                       - novel_dir_for_record, create_subdirectory_name
    ruby_time.rs                   - Ruby互換日時フォーマット
  downloader/
    mod.rs                         - Downloader struct (DL pipeline orchestrator, Arc<dyn HttpClient> + Arc<dyn RateLimiter> 注入)
    types.rs                       - SectionElement, SectionFile, TocObject, DownloadResult 等
    http_policy.rs                 - プラットフォーム中立 HTTP ポリシー (decode/status mapping/redirect/fetch_text/fetch_bytes)
    toc.rs                         - fetch_toc, parse_subtitles, parse_subtitles_multipage
    section.rs                     - download_section, parse_section_html, section cache
    persistence.rs                 - save_section_file, save_raw_file, save_toc_file, ensure_default_files
    narou_api.rs                   - narou_api_batch_update (なろうAPI一括更新)
    util.rs                        - build_section_url, pretreatment_source, sanitize_filename 等
    site_setting/
      mod.rs                       - SiteSetting struct, accessor methods, compile, load_all, merge_user_definition_yaml, tests
      interpolate.rs               - \k<name> テンプレートエンジン
      info_extraction.rs           - resolve_info_pattern, multi_match, get_novel_type_from_string
      loader.rs                    - load_all_from_dirs, load_settings_from_dir
      serde_helpers.rs             - deserialize_yes_no_bool
    preprocess/
      mod.rs                       - PreprocessPipeline struct, run_preprocess
      ast.rs                       - Stmt, Expr, StrPart, Accessor 等 (AST型定義)
      parser.rs                    - PreprocessParser (pest grammar), parse_preprocess, build_*
      interpreter.rs               - Ctx, eval_expr, eval_stmt, eval_method
      preprocess.pest              - pest grammar file
    novel_info.rs                  - NovelInfo (from_toc_source / from_novel_info_source)
    html.rs                        - to_aozora (HTML→青空文庫形式変換)
    info_cache.rs                  - 小説情報キャッシュ
    rate_limit.rs                  - RateLimiter
    security.rs                    - URL検証、SSRF防止
  converter/
    mod.rs                         - NovelConverter struct, convert_novel pipeline, cache (1246行)
    render.rs                      - render_novel_text (novel.txt.erb相当), ConvertedSection
    output.rs                      - create_output_text_path/filename, extract_domain/ncode_like
    ini.rs                         - IniData / IniValue (INI parser/serializer)
    settings.rs                    - NovelSettings (44 items, INI overlay, replace.txt)
    device.rs                      - OutputManager (端末別出力: epub, mobi, kindle等)
    dakuten_font.rs                - 濁点フォント処理
    inspector.rs                   - 調査ログ生成 (Inspector)
    converter_base/
      mod.rs                       - ConverterBase struct, TextType, convert pipeline orchestrator (298行)
      character_conversion.rs      - 半角/全角変換, 数字→漢数字, TCY
      indentation.rs               - auto_indent, half_indent_bracket, insert_separate_space
      stash_rebuild.rs             - illust/URL/kome stash & rebuild
      ruby.rs                      - narou_ruby, find_ruby_base (ルビ注記処理)
      text_normalization.rs        - rstrip, ellipsis, page_break, dust_char, blank_line 等
    user_converter/
      mod.rs                       - UserConverter struct, load, apply_before/after, signature
      setting_override.rs          - apply_setting_override (converter.yaml設定オーバーライド)
  web/
    mod.rs                         - AppState, create_router (70+ エンドポイント, request_guard/basic_auth middleware)
    state.rs                       - ApiResponse, IdPath, ListParams 等 (DTO structs)
    novels.rs                      - index, novels_count, api_list, get/remove/freeze/unfreeze
    tags.rs                        - add_tag, remove_tag, update_tags, edit_tag
    batch.rs                       - batch_tag/untag/freeze/unfreeze/remove
    jobs.rs                        - api_download/update/convert, queue_status/clear, send/mail/backup
    novel_settings.rs              - get_settings, save_settings, list_devices
    misc.rs                        - version_current/latest, tag_list, notepad_read/save, recent_logs
    push.rs                        - PushServer, WebSocket, StreamingLogger
    worker.rs                      - バックグラウンドジョブ実行 (子プロセス管理)
    scheduler.rs                   - 自動更新スケジューラ (enqueue auto_update job)
    frontend.rs                    - Web UI 静的ページ配信 (/settings, /help, /about, etc.)
    global_settings.rs             - グローバル設定 API
    sort_state.rs                  - 一覧ソート状態保存
    tag_colors.rs                  - タグ色管理
    update.rs                      - セルフアップデート API
    login.rs                       - ログイン Cookie 管理 API (/api/login)
    feature_tour.rs                - 機能ツアー API (/api/feature_tour)
    library_backup.rs              - ライブラリ一括バックアップ API (/api/library_backup)
    assets/                        - 静的アセット (CSS, JS)
sample/  (gitignore 済みのローカル用ディレクトリ)
  novel/                           - テスト用CWD (.narou/ + webnovel/*.yaml)
  narou/                           - Ruby参照ソース (whiteleaf7/narou のローカルコピー)
  1177354055617350769 .../         - カクヨム参照データ (narou.rb出力, 25,273行)
```

## Reference Files (Ruby, 読取専用, `sample/narou/` のローカルコピー)
- `sample/narou/lib/converterbase.rb` — テキスト変換エンジン (1503行) — **最も重要な参照**
- `sample/narou/lib/novelconverter.rb` — コンバーター全体オーケストレータ (1209行)
- `sample/narou/lib/html.rb` — HTML→青空変換 (124行) — Rustの `html.rs` はこれに準拠
- `sample/narou/template/novel.txt.erb` — 最終テキスト組み立てERBテンプレート (93行)
- `sample/narou/lib/novelsetting.rb` — 設定定義
- `sample/narou/lib/command/*.rb` — 各コマンド実装 (help/CLI挙動の参照元)

## Current Status (2026-09)

### SQLite 管理基盤移行 (P0〜P4c 完了 / P5 一部、詳細: docs/sqlite-storage-migration-plan.md)
- P1 `src/native/sqlite/` エンジン + dual-run テスト、P2 メタデータ全面移行 (database/freeze/alias/tag_colors/local_setting/queue/notepad/latest_convert → `.narou/db.sqlite`)、レガシー自動import(元ファイルは *.imported-* 退避)、`narou db verify|export-yaml|vacuum`。`global_setting` は 2026-09 修正で移行対象から外れファイル管理のまま (前節「グローバル設定の保存先」参照)
- P3 デュアルモード化: **既定は従来どおり YAML 管理**。`.narou/storage-backend` マーカー(`sqlite`)または Web UI ツアーの選択で Lite(SQLite) へ切替。`NAROU_RS_LEGACY_YAML=1` は強制レガシー。API: `GET/POST /api/storage/mode`
- **設定ページからの切替**: 設定 → WEB UI タブの「データ管理方式」から現在のモード表示・SQLite への移行・YAML への復帰ができる (`GET/POST /api/storage/mode`、実装は `src/web/storage.rs`)。YAML へ戻すときは `narou db export-yaml --in-place` 相当 (実位置への書き出し + マーカー切替) を先に実行するので、切替で作品データは失われない。`NAROU_RS_LEGACY_YAML=1` のときは固定 (API が `locked_by_env` を返す)。
- **アップデート版 (Lite / 通常)**: `self-update.variant` は設定ページの Global タブで選べる (`gpl` = AozoraEpub3_Lite 組込み / `standard` = 外部 AozoraEpub3)。更新時の版選択モーダルと同じ設定を書き換える。
- P4a コンテンツミラー (novel_sections/novel_outputs) — convert時に書込み、Web DL時EPUBはDB優先
- P4b バージョン履歴 (novel_versions/_sections/_diffs) + `narou diff --history|--show|--restore|--merge-from`。update時自動snapshotはconvertフック経由
- P4c オブジェクト格納: `objects`/`object_chunks` (BLOB + brotli + crc32) に小説データ・生成物を格納。native は FS ミラー + 読みフォールバックを維持。Worker は管理・目次を D1 に残し、本文・履歴は独立した S3 移行に対応する（後節参照）
- 後方互換: 旧ライブラリからの自動取込と `narou db export-yaml` によるロールバックを保証。`narou setting narou-compat=true` で `.narou/*.yaml` を維持する前方互換モードあり (既定 OFF)。`export-yaml --in-place` は実位置へ書き戻して YAML モードへ復帰する
- **索引は SQLite モードでも全件データから作り直す (2026-09 修正)**: `Database::refresh` / `update_records` はリレーショナル側のレコードから `IndexStore` を再構築する。`get_by_toc_url` / `find_by_title` はこの索引しか見ないため、退避済みの `database_index.yaml` をそのまま使うと登録済み URL の再ダウンロードが重複レコードになっていた (issue #30)。ファイル出力は従来どおり (`narou-compat` 時のみ書き出し、通常の SQLite モードでは書かない)
- **凍結状態も保存先は 1 つ (2026-10 修正、issue #35)**: YAML 管理は `.narou/freeze.yaml`、SQLite 管理は `app_state('inv','freeze')` で、どちらも `Inventory::update_yaml` を通す (`narou freeze` / `download --freeze` / Web UI の freeze 操作)。以前は `freeze` がファイルを直接書いており、SQLite 側の状態が更新されないうえ、次回起動のレガシー取込がその断片で在庫を丸ごと置換して他の凍結を失わせ、`freeze.yaml.imported-*` を増やし続けていた。取込は**和集合**になり (`state::merge_freeze_payload`)、SQLite では内容が `frozen_novels` へ投影される (`repository::sync_frozen_novels`、レコード取込後に `StateDb::resync_frozen_novels` で再投影) ため、状態検索式 (`status_search_expression.sql`) と `narou list` が一致する。既に失われた凍結は `narou db repair-freeze [--dry-run]` が `freeze.yaml.imported-*` の和集合から復旧する (解除はしない)。回帰テストは `tests/freeze_sqlite_state.rs`

### プラットフォーム抽象化 (Phase 1-8 実装完了、2026-08)
- **設計資料**: `docs/platform-abstraction.md` — Cloudflare Workers 対応のための全面プラットフォーム抽象化。Phase 4のsmall object / large asset境界、logical key、native mapping、remaining native FS、Phase 7のD1/Worker read-only adapter、Phase 8のQueue/crawler/scheduler境界も記録。
- **Phase 1 完了**: `src/platform/` に traits（HttpClient / Clock / RateLimiter / ObjectStore / NovelRepository）+ テスト用 mock（MockHttpClient / MemoryObjectStore / MemoryNovelRepository / FakeRateLimiter / SystemClock）を導入。
- `NarouError::Http` は `reqwest::Error` の直接 `#[from]` をやめ String 化。`Platform(String)` variant 追加。core から reqwest 型が error 経由で漏れるのを防止。
- **Phase 2 完了**: downloader を trait 利用へ全面移行。
  - `HttpClient::send` / `RateLimiter::acquire` は boxed future の async trait（`Send`）。`HttpRequest` は所有型 + `RedirectMode`（Follow/Manual）、`HttpResponse` は bytes のみ。
  - `src/downloader/fetch.rs`（HttpFetcher）は削除。transport は `src/native/http.rs` の `NativeHttpClient`（curl→reqwest→wget tier fallback、`tokio::task::spawn_blocking` で隔離）。
  - デコード・ステータス→ドメインエラー変換・リダイレクト解決は `src/downloader/http_policy.rs`（プラットフォーム中立）に集約。
  - Downloader は `Arc<dyn HttpClient>` + `Arc<dyn RateLimiter>` + `Arc<dyn NovelRepository>` を保持。`with_platform(http, rate_limiter, novels)` で注入し、`with_user_agent` は native 実装を組み立てる。
  - `cmd_download` / `cmd_update` は async 化。update の domain 別並列 worker は `tokio::spawn` に移行。
  - なろう系サイトの wait-steps 既定 10 は `RateLimitScope::narou` フラグで維持。
- **Phase 3 完了**: `NovelRepository` を async 化し、typed `NovelFilter` / `SearchTerm` / `NovelSortKey` / `NovelQuery` / `NovelMutation`、keyset `scan_ids`、atomic `allocate_id`、一回保存の `apply_batch` を実装。
  - `src/native/novel_repository.rs` は共有 `db::DATABASE` を使う stateless adapter。async 経路は `spawn_blocking`、CLI は `_sync` 経路で実行し、YAML を二重ロードしない。
  - Downloader / narou API / CLI / Web の NovelRecord 操作を repository 経由へ移行。Web 一覧は `count` + paginated `query`、一括処理は `scan_ids`。
  - Native YAML round-trip、unknown fields / raw_title / nilable bool / 日時、Memory repository、並列 ID reservation、Downloader injection をテストで固定。
- **Phase 4 完了**:
  - `ObjectStore` は `PlatformFuture` async API（`stat` / bounded `read_small`・`write_small` / `delete` / cursor付き `list_page`）、`AssetStore` は bounded chunk streamとcopy/move semanticsを提供。
  - `ObjectKey` は `/`区切りlogical UTF-8 key。`NovelObjectKeys` / `GeneratedAssetKey` がキー生成を集約し、OS `Path`をcore identityにしない。
  - `NativeObjectStore` は既存の `小説データ/` layoutへ写像し、atomic write、archive-root / symlink / reparse-point escape対策、legacy section filename fallbackをnative adapter内で維持。
  - downloaderのTOC/section/raw/setting/replace/cache persistenceは`PersistenceService`経由。codecはpureで、`Clock`を注入可能。旧Path APIは`src/native/legacy_persistence.rs`へ隔離。
  - illustrationはmetadata indexと`IllustrationStorageService`を分離し、blob write成功後にcache indexを更新。既存`.illustration_cache.yaml`形式とnative migration/orphan CLI互換を維持。
  - converterのdirect `curl::Easy`を除去し、illustration localizationの`ConverterCapabilities`へ`HttpClient` / `RateLimiter` / `ObjectStore` / `AssetStore` / index / logical prefix / 必要時の`NovelRecord` resolverを注入可能にした。zero-argument native constructorsは`src/native/converter.rs`へ隔離し、pure converter pipelineへplatform traitを逆流させない。
  - `src/native/converter.rs` / `src/native/downloader.rs` にzero-argument native constructorsを隔離し、coreからNativeHttpClient / NativeObjectStore / NativeNovelRepositoryを直接参照しない。
- **対象の形ごとの取得先**: `toc_url` は文字列のほか `by_target:` (ターゲット URL に一致する `match:` 正規表現 → `url:` テンプレートのリスト) を取れる。Pixiv のように 1 ドメインで複数の対象形 (小説 / 小説シリーズ / イラスト / 漫画シリーズ) を扱うサイトは、これで形ごとに別 API を指せる。`novel_info_url: \k<toc_url>` も対象ごとに解決されるので、形ごとの API URL を二重管理しなくてよい。
- DSL は取得元 URL を `${url}` で参照できる (ページ番号の繰り上げなどに使う)。
- **サイト取得の出口は 1 つ**: `http_policy::FetchPolicy` が Cookie とサイト定義の `headers:` を持ち、`fetch_bytes`/`fetch_text`/`resolve_final_url(_with_body)` がそれを受け取る。挿絵の取得も同じ policy を通る (以前は cookie 無し・ヘッダ無しで素の GET だった)。サイト定義の `headers:` は `\k<...>` 補間され、名前・値が不正なヘッダは policy 構築時に落とす。
  - `MemoryObjectStore` async/paged/chunked fake、PersistenceService fixed-clock、NativeObjectStore layout/existing-data compatibility testsを追加。
- **Phase 5 remaining native boundary**: Inventory/settings、site definition loader、downloader info cache、Web固有FS、converter/settings/ini/inspector/user-converter/section-convert-cache、converter/deviceのsubprocess/tempdirはnative-only capabilityとして残る。content blobをLISTでmetadata DB化しない。

### Phase 6-8 Worker backend (2026-08)
- `narou_rs` の `worker-runtime` feature は `application`、`platform`、portable `db`/`converter::ini` のみを公開する。CLI、Web、native HTTP、filesystem、process、settings adapters は `native-runtime` gate の内側に置く。
- `worker_entry/` は `workers-rs 0.8.5` の `fetch` / `scheduled` / `queue` eventを公開する。`composition.rs` は D1 の管理・設定・目次アダプタと、本文の圧縮 S3 保存 + D1 参照を担う `ContentStore` を構成する。Worker 固有型を core へ逆流させない。
- `WorkerHttpClient` は Fetch API を既存 `HttpClient` trait へ接続する。**403 を返した GET は `connect()`（生ソケット）で取り直す**（`platform::http1`）。ソケット側で成功したときだけ採用し、接続拒否・タイムアウト・解析失敗は元の403を保つ。全アドレスが Cloudflare 公開レンジ内のサイト（例: ハーメルン）には、別の出口 IP を持つ外部リレー（Rust 版 `relay/` crate の `narou_relay`）を使う。`platform::relay` の既存プロトコルを維持し、Worker からのリレー呼び出しは**通常の fetch サブリクエスト**で送る（Worker の custom domain とリレーが同一 Cloudflare ゾーンにある場合、生ソケットではエッジに届かずリレーのログにも残らないことがある）。設定は Worker secret `NAROU_RELAY_ENDPOINT` / `NAROU_RELAY_KEY`。両方が揃ったときだけ有効で、接続先はオリジンに正規化する。リレー自体の配備は本体CIと独立して管理し、Worker配備はリレーの再配備を待たない。
- `D1ObjectStore` は logical-key prefix、paged LIST、bounded small read/write、streaming AssetStore を D1 上に実装する。
- Worker の User-Agent は native と同じ **`ua_generator` のランダム Firefox UA**（`download::resolve_user_agent` に分岐は無い）。`worker-runtime` でも `dep:ua_generator` を有効化し、`ua_generator` は `default-features = false`（`fetch` は使わない）でビルドする。リレー経由の取得も同じ UA を転送するため、Worker だけ固定 UA にしてサイト判定が変わらないようにする。
- `D1NovelRepository` はprepared statements/migrationsでtyped filter/sort、keyset `scan_ids`、atomic sequence allocation、batch mutationをSQL化する。settings、freeze、tag colorsもD1 state/tableへ接続する。
- `/health/live`、`/health/ready` と認証付き API (`/api/novels*`、`/api/login*`、`/api/sites*`、`/api/jobs*`、`/api/global_setting`、`/api/admin/object-migration`) に加え、Web UI の表示・操作系ルート（`/api/list`、`/api/sort_state`、`/api/webui/config`、`/api/tag_list`、`/api/queue/*`、`/api/feature_tour/*`、`/api/freeze`、`/api/novels/{freeze,unfreeze,remove}`、`/api/edit_tag`、`/api/tag/change_color`、`/api/download`、`/api/convert`、`/api/update*`、`/api/story`、`/api/diff_*`、`/api/notepad/*`、`/api/history`、`/api/taginfo.json`、`/api/version/*.json`）を公開する。実現不能な操作は 501 + 機械可読コード。JSON は core の `application::settings_view` / `web_payloads` / `version_compare` を native と共有する。`NAROU_ADMIN_TOKEN`はconstant-time比較。未知queue envelopeはledgerへ記録して安全にackする (retryしない)。Queue実ジョブ実行 (D1 ledger・bounded retry・checkpoint resume) は Phase 8 で実装済み。
- Worker production readinessはD1 `DB` bindingと `NAROU_ADMIN_TOKEN` secretを要求する。秘密値はリポジトリへ置かない。

### Cloudflare Workers 移行 (2026-09-26 決定、詳細: docs/cloudflare-workers-migration-plan.md)
- **ゴール**: Web UI ごと Workers へ移行する。native は CLI と、Workers で代替できない重量処理・ローカル操作のために残す。
- **重量処理**: Worker 内の AozoraEpub3_Lite (in-process) で完結。外部プロセス前提の機能 (AozoraEpub3 jar / kindlegen / SMTP / 端末送信 / セルフアップデート / `folder` / `browser` 等) は `blocked` / `501` で明示的に拒否し、CF Containers は使わない。
- **保存 (2026-10-16 変更)**: **管理情報・目次・本文への参照は D1、本文・旧本文・挿絵は S3 互換ストレージ**。本文は `SectionFile` YAML（抽出 HTML・前後書き・話情報）を話単位で共通 Brotli codec に通し、`content/<ランダム版ID>.bin` に保存する。新版の保存成功後だけ D1 `content_objects` の参照を公開し、サイズ・CRC32・SHA-256 を読み出し時に照合する。`ContentStore` は既存の `ObjectStore` / `AssetStore` を実装し、native の保存方式は変更しない。切替は `app_state('inv','content_backend')` の `d1`（旧方式）→ `migrating`（新規書込み S3、参照なしだけ旧 D1 読み）→ `s3`（参照のみ）の一方向。`asset_backend` は引き続き挿絵専用。**実装・binding・設定キーは `S3_*` / `s3_*` で統一し、ベンダ名を識別子に使わない**。
- **本文の既定は S3**: 本文ストア初期化時、`content_backend` が未設定なら旧 D1 本文・履歴が無い場合は `s3`、残る場合は `migrating` を選び、新規本文を S3 に保存する。明示的な `d1` は非必須環境だけで維持し、`NAROU_REQUIRE_S3=true` では同じ規則で進める。S3 構成成功後、旧本文の存在判定と marker 保存を primary の単一 SQL で行い、既存 `migrating` / `s3` を戻さない。一括コピー・照合・削除は引き続き管理 API で行う。`plan` / `status` は初期化せず、未設定を `d1` と報告する読み取り専用である。
- **S3 必須配備**: develop/production の CI は `NAROU_REQUIRE_S3=true` を渡す。挿絵は旧 `asset_backend=d1` や未設定でも S3 を選び、D1 の旧挿絵を移行・削除・代替参照しない。管理・目次・設定は D1、本文・履歴は独立した `content_backend` に従う。保存済み marker は canonical JSON と既存 raw TEXT の `d1`/`s3` を受け付け、不正値・読み取り失敗は拒否する。S3 の設定/通信失敗でも D1 へ戻さない。認証済み `/api/storage/mode?probe=s3` の実ストア選択と bounded LIST 成功を CI で必須確認し、一般 smoke の省略を成功扱いしない。**例外は前段の Cloudflare Access が答えた 403 だけ** (2026-10-06 決定): custom domain が Access の内側にあり CI に service token が無い場合、CI からは検証しようがないため `::warning::` 付きのスキップとし、ステップサマリも `skipped: blocked before the Worker …; not verified` と表示する (成功扱いにしない)。401 / 503 / 到達不可などの他の失敗は従来どおりジョブを失敗させる。403 を例外にできる根拠は「Worker はこの経路で 403 を返さない」こと (S3 LIST 失敗は `worker_entry/src/storage_probe.rs` が 503、認証失敗は 401)。プローブは urllib 既定の UA が Cloudflare のボット判定に掛かって 403 になっていたため、アプリ本体と同じブラウザ風の `User-Agent` (`ci/deploy_worker.py` の `PROBE_USER_AGENT`) を必ず送る。警告には前段の手掛かり (`cf-mitigated` など) を付けて切り分けられるようにする。配備ジョブの env には `NAROU_REQUIRE_S3: "true"` も置き、smoke (`contract.mjs`) 自身にも同じ `?probe=s3` 検査をさせる — Python プローブが前段の 403 でスキップした場合でも必須確認が残る (2026-10-06 時点の実測: UA 修正後に `S3 backend + LIST: ok`)。
- **既存DNSレコードとの競合**: wrangler の `externally managed DNS records`（Cloudflare code100117）では、`ci/deploy_worker.py` が `NAROU_DOMAIN_MODE=route` で組み直し、同じホスト名へ `[[routes]] pattern="<domain>/*"` + `custom_domain=false` で載せ直す。DNSは削除せず、Accessもホスト名単位のまま維持する。zoneの自動判定が効かない場合は `NAROU_ZONE_NAME` を指定する。特定ホスティング先の所有権判定や配備先切り替え変数は置かない。
- **EPUB は保存しない**: AozoraEpub3_Lite で、Web UI の DL 要求時に保存済みデータからストリーミング生成する。**Workers では raw HTML などのキャッシュを一切保存しない**（native は従来どおりで `.narou/` 互換に影響なし）。
- **挿絵の取得はダウンロード時 (2026-10-08 修正)**: 挿絵の実体はダウンロード時に取得して S3 へ書く (Worker の変換は native と違い挿絵をローカライズしない)。取得対象はサイト定義の `illust_grep_pattern`、無ければ **narou.rb `HTML#initialize` の既定 `<img>` パターン** (`src/downloader/mod.rs` の `DEFAULT_ILLUST_GREP_PATTERN`)。なろう / R18なろう / カクヨム / Arcadia はこの経路で挿絵を持つ。既定パターンでは narou.rb `Illustration#scanner` と同じく URL でない注記 (相対パス) は取得せず、挿絵 URL は protocol-relative (`//host/...`) を含めて本文のスキームで解決し、mitemin は取得用 URL (`viewimagebig` → `viewimage`) へ寄せる。挿絵が 1 枚も無い小説には空の `.illustration_cache.yaml` を作らない。
- **索引と実体の突き合わせ (自己修復, 2026-10-08 修正)**: `.illustration_cache.yaml` はメタデータなので実体の存在を保証しない (ストアの切替・手動削除・別環境での取得で「索引にあるのに実体が無い」状態になる)。索引が空でないときだけ `<prefix>/挿絵` を 1 回 (最大 1000 件 × 10 ページ) 一覧し、索引が知っているファイル名が無ければ **取り直す** (`PersistenceService::list_illustration_names` → `Downloader::download_illustration`)。一覧に失敗したときは従来どおり索引だけで判断する。debug-mode では `挿絵: 保存済み N 件 / 索引 M 件 (<prefix>)` と `挿絵: 索引にはありますが実体が無いので取り直します: 挿絵/<file> (<url>)` を出すので、S3 と D1 のどちらに実体があるかを切り分けられる。一覧が空を返したときは索引のファイルを `stat` で 1 枚ずつ確認する (`verify_indexed_illustrations`、上限 64 件) ので、LIST が prefix の解釈差で空を返す環境でも取り直しの誤判定をしない。
- **挿絵のキーは 1 箇所で作る (2026-10-08 修正)**: 挿絵の論理キーは `platform::illustration_key(prefix, filename)` に集約し、**保存側 (`IllustrationStorageService`) と読み出し側 (`NovelObjectKeys::illustration` / `download.epub`) が同じ関数を通る**。片側だけ sanitize するとキーがずれて「保存したのに見つからない」になるため。`download.epub` はストアの一覧に載っていない参照も `keys.illustration()` + `stat` で直接確認してから EPUB を組む (一覧が効かない環境でも実体があれば入る)。**挿絵が有効で、本文に参照があり、1 枚も解決できない場合だけ** `WARN: EPUB: 挿絵を 1 枚も解決できませんでした (prefix=…)` と「一次ストア (D1) に N 件残っている / どこにも無い」を出す (`report_missing_illustrations`)。参照なし・挿絵無効は欠落ではない。
- **本文の移行・容量回収**: 認証付き `GET/POST /api/admin/content-migration` の `plan` → `copy` → `verify` → `cutover` → `purge` → `gc` を使う。`plan` は読み取りだけ、各操作は既定 20／最大 100 件で再開可能。現行本文と `本文/cache/<版>/*.yaml`・旧 `.cache` 形式の履歴が対象。進捗は `app_state('inv','migrate_content')`、管理操作は期限付き lease で排他する。移行開始後の旧本文の書き戻し・無許可削除は D1 トリガーでも拒否し、同時取得の新しい参照をコピーで上書きしない。照合後の切替が済むまで旧 D1 本文を消さない。切替後の S3 欠落・破損を旧 D1 へのフォールバックで隠さない。不要な版は `content_garbage` で削除を再開でき、未完了 PUT の予約は 1 時間保護する。期限切れの予約で参照を公開せず、GC との競合で欠落した版を公開しない。
- **CLI 手順**: `BASE_URL` / `NAROU_ADMIN_TOKEN` を環境変数に置き、`node scripts/migrate-worker-content.mjs` で計画、`--apply` で旧 D1 本文の削除まで実行する。配備だけで自動移行はしない。`purge` 後に marker だけを `d1` に戻したり、参照に非対応の旧バイナリへ戻す運用は不可。本文を D1 の列へ展開する旧計画は撤回。検証は `worker_entry/tests/content_storage.mjs`（実 Worker / ローカル D1 / S3 HTTP fixture）と `test_content_storage_sql.py`。
- **目次を本文として移さない**: サブディレクトリありの `NovelObjectKeys::new("site", "本文", true)` は `novels/site/本文/本文/toc.yaml` を作る。親が `本文` でも `toc.yaml` / `converter.yaml` / `.illustration_cache.yaml` は本文ではなく D1 メタデータ。`ContentStore::is_content_key` と migration `0012_content_metadata_boundary.sql` のビュー・書き込みガードで同じ除外を行い、S3 へのコピー・旧本文の purge の対象にしない。`content_storage.mjs` はこの構成で切替・purge・EPUB・再起動を確認する。
- **Worker の更新失敗で現行本文を失わない**: `PersistenceService::cache_section` は portable ビルドでは `AssetStore::copy`、native では従来の `move_or_copy` を使う。履歴保存が成功した後でも新本文の PUT は失敗し得るため、Worker で先に現行参照を消してはならない。改題で論理キーが変わった話は、新本文と新 TOC の保存が済んだ後だけ旧キーを削除する（budget 再開済みの話も対象）。`worker_entry/tests/content_storage.mjs` は COPY 成功後の PUT 503 と、旧本文の EPUB 配信・改題後の旧キー削除を実 Worker で確認する。

### Worker のジョブ実行・進捗配信・コンソール (2026-09)
- **実行系**: `worker_entry/src/executor.rs` が queue から受けたジョブを実行し、`consumer.rs` が ack/retry を、`ledger.rs` が試行回数と実行トークンを担う。再試行の解釈 (`queue.max-retries` / `queue.retry-backoff`) は core の `application::retry_policy` が唯一の実装で、native の queue worker と Worker が同じ値を使う。
- **進捗配信 (PushHub)**: Durable Object `PUSH_HUB` (`worker_entry/src/push_hub.rs`) が WebSocket を保持し、worker からは `POST /api/push/events` の HTTP でイベントを渡す (DO を直接 fetch すると "Cannot reconstruct" になるため HTTP 経由)。Web UI は `/websocket` に接続する。
- **イベント payload**: ジョブイベントの生成は core の `application::push_events` が唯一の実装で、native の Web UI 経路と Worker が同一の JSON を出す。
- **コンソール行**: ジョブのコンソール出力は core の `application::messages` が唯一の生成元。Worker の `MessageContext` は future の poll ごとに `PushHubSink` と詳細ログ設定を束縛し、並列ジョブ間で sink・操作 ID・表示先を共有しない。行を積むたびに送信タスク (`spawn_local`) を起動し、順序・取りこぼし・最小送信間隔は `FlushQueue` が管理する。ジョブ終端より先に `drain()` で残りを送る。
- **コンソールの分離 (`concurrency`)**: Worker の既定は有効で、明示的に `concurrency=false` を保存したときだけ 1 コンソールへまとめる。convert は native と同じ `stdout2`、DL/update は `stdout`。`MessageContext` がジョブごとの表示先を持ち、共通の `Stream::target_console_with` で解決する。native のレーン制御・Worker の queue 並列実行は配送アダプタに残し、`concurrency.*` の native コンソール整形は Worker では無効。
- **詳細ログ (`webui.debug-mode`)**: 詳細ログの唯一の入口は core の `application::debug`。ON のときだけ `[debug] …` 行を sink (= Web コンソールの `echo`) へ流し、OFF では何もしない。設定の真偽値解釈は `enabled_in_settings` に集約する。native の CLI は `debug::apply_settings`、Worker のジョブは poll ごとに束縛する `MessageContext` の設定を使い、並列ジョブ間で有効状態を上書きしない。ジョブ外の EPUB 生成は PushHub へ直接送る (`push_debug_line`)。Web UI (`main.js`) は ON のときだけ Web コンソール行をブラウザの開発者コンソールへも転送する。挿絵は「保存されなかった」「本文の参照が URL のまま」「EPUB の入力に見つからない」をこの詳細ログと欠落警告で追う。
- **共有ヘルパ**: Web UI の入力検証・上限値・ソート状態・HTML ヘルパは core の `application::webui` が唯一の定義で、native の `src/web/**` と `worker_entry/src/webui/**` はそれを再エクスポートして使う。
- **ローカル検証**: `worker_entry` で `npx wrangler dev --config wrangler.toml` を起動し、`POST /api/download` → `POST /api/convert` を流せば DL から変換まで通る。fetch は手元のマシンから出るため、本番で 403 になるサイトもここでは取得できる (実測: なろう / カクヨム / syosetu.org)。ジョブのコンソール行は `ws://127.0.0.1:8787/ws` に `Authorization: Bearer <NAROU_ADMIN_TOKEN>` で接続すると `echo` イベント (`{"type":"echo","target_console":"stdout","body":"…"}`) として見える。ブラウザは `/login` で管理トークンを Cookie (`narou_api_token`、HttpOnly / SameSite=Lax) に入れれば Bearer 無しで UI 全体を操作できる (fetch / EPUB リンク / WebSocket すべて Cookie が付く)。`.dev.vars` に `NAROU_AUTH_REQUIRED=false` を足すのは、Cookie を経由せず素通しさせたい場合だけ (本番は Access を境界にするときだけ false)。
- **注意**: ソースを変更すると `wrangler dev` は資産ディレクトリ `public/` の削除に失敗して (Windows の EBUSY) 無言で停止する。変更後は起動し直すこと。
- **ホットリロード (native)**: `narou web` は設定変更を再起動なしで反映する。`server-*`（Host 許可リスト / WS Origin / reverse proxy / Basic 認証）は `src/web/server_security.rs` の `ServerSecurity` を `RwLock` で持ち、保存 API が `SettingsEffect::ServerSecurityChanged` で即時再適用、外部からの変更（CLI・手編集）は `commands/web.rs` の 30 秒 watcher が拾う。watcher は `update.auto-schedule(.timezone)` でスケジューラを再起動し、`webui.*` を `webui.config.reload` で再配信、`logging*` でロガーを再 init する。**CLI (`narou setting`) からの `server-*` の変更は watcher を待たずに即座に反映する**（issue #46, 2026-10-28）：`cmd_setting` が `SERVER_SECURITY_NAMES` (`application::settings`) に含まれる値が変わったとき `commands::web::notify_running_server_to_reload_security` が内部 API (`POST /api/server_security/reload`) を呼ぶ。サーバは待ち受けポートとホストを PID ファイル (`.narou/server.pid`) に書くので、`--port` で起動した場合も CLI が対象を見つけられる。PID ファイルは narou ルート基準で解決する（カレントディレクトリ基準だと別の場所から実行した CLI が見つける）。回帰テストは `tests/cli_server_security_reload.rs`。`webnovel/*.yaml` は `EFFECTIVE_SITE_SETTINGS`（差し替え可能・fingerprint で再コンパイル抑制）を sites API の保存後に更新＋30 秒ポーリングで外部編集を拾い、`site_timezone` は毎呼び出しで現スナップショットを参照する。再起動が要るのは `server-port` / `server-bind`（リスナー再バインド）、`server-basic-auth.require-for-external-bind`（起動時ガード）、`no-color`（tracing subscriber）、`concurrency` のレーン数、`.narou/storage-backend` の切替（DB 層の差し替えで、切替時に明示的に再起動）だけ。
- **ホットリロード**: Worker は再起動・再デプロイを前提にしない。サイト定義 / 設定 / `asset_backend` / section hash / 凍結 ID / タグ色 / secret はすべて isolate 内キャッシュ (`isolate_cache::TtlMap`) で、TTL (既定 30 秒) か書き込み時の無効化で反映する。**設定変更・secret ローテーション・`webnovel/*.yaml` の差し替えに再デプロイは不要** (反映は最大 30 秒、書き込んだ isolate では即時)。キャッシュを足すときは同じ規則に従うこと。
- **D1 read replication**: `deploy_worker.py` が `ci/enable_read_replication.py` で有効化する (REST API のみ、`wrangler d1` に該当コマンド無し)。Cloudflare の仕様上 **Sessions API を使わない限り全クエリは primary に行く** ため、UI の読み取りは `build_ui` の `first-unconstrained` セッション、ジョブ実行系は primary 固定という分担にしている。
- **デプロイ**: `worker_entry/ci/deploy_worker.py` が `NAROU_DEPLOY_TARGET=develop|production` で D1 / Queue を用意し `wrangler.ci.toml` を描画してデプロイし、契約テストを流す。`CLOUDFLARE_API_TOKEN` が必要 (無い環境では手元からデプロイできない)。

### 最近の追加 (2026-05〜09)
- **update の並列ダウンロード** (E): `update.max-parallel-domains` 設定（既定 4）で対象小説をサイトドメイン別にグルーピングし、ドメインごとに並列ダウンロード。同一ドメイン内は常に直列を維持するため対サイト礼儀は崩れない。1・フォース指定・ドメインが1種類のときは逐次にフォールバック。Web 更新も並列経路を使い、操作 ID ごとの進捗を配信する。
- **ジョブ自動リトライ** (B): queue worker に exponential backoff 付き自動リトライ（`queue.retry-backoff` 既定 `1m,5m,15m`）を実装
- **挿絵メンテナンスコマンド** (A): `narou illust <sub>`（`orphan` / `migrate` / `fix-ext` / `rebuild`）を新設し、`.illustration_cache.yaml` 運用の保守ヘルパーを CLI から呼び出せるようにした。削除・改名・移行系はいずれも既定 dry-run
- **ブックマークレット登録フローの same-origin 復活** (H): Web UI から拡張ブックマークレットを登録する際の手続きを同一オリジン経由に戻し、外部リダイレクトを排除
- **reverse proxy Host 許可リスト拡張** (I-3): `server-add-accepted-hosts` 設定で許可 Host を後から追加できるようにし、リバースプロキシ越しのアクセス制御を強化
- **list / update の型付きソート** (I-1): ソートキーを型付きで実装し、`new_arrivals_date` / `general_lastup` など拡張キーをバリデーション込みで受理
- **self-update の Unix デタッチ** (D): Linux/macOS で self-update 中も本体が生存できるよう、updater を `setsid` で切り離して起動
- **self-update の variant 選択** (2026-09): GPL版(Lite組込み)/通常版 の選択は global 設定 `self-update.variant` (`gpl` / `standard`) が唯一の保存先で、Web UI の環境設定 Global タブのセレクトと `narou setting self-update.variant=...` の両方から設定でき、`narou setting` の一覧にも表示される。0.4.0 以下からの更新時は未設定なら Web UI が選択モーダルを表示して保存し、設定済みならモーダルを出さず保存値で更新する。更新は 保存値 → ビルド variant の順で解決し (リクエストの明示指定が最優先)、GPL版は `narou_rs_*-GPL.zip` を取得する。未設定へ戻すと実行中のビルドと同じ variant になる
- **ruby タグ除去** (I-4): サブタイトルとファイル名からルビ注記（`《…》` 形式）を除去し、Ruby版と表示を揃える
- **小説単位の queue lane 跨ぎ exclusion** (C): 同じ小説が primary / secondary lane の両方で同時に走らないよう、novel 単位の排他を queue worker に追加。判定は**実行中の子プロセスが今まさに触っている小説**（`.narou/lock.yaml`、`NovelLockGuard`。convert / download / update が取得し、終了で消える。30 分を超えた残骸は無視）で行うので、全作品更新の最中でも別の小説の変換は並行できる（当初は「対象一覧」で判定していたため、全作品更新の間はどの変換も待機になっていた）

### 変換互換性
- **なろう**: narou.rb参照データと完全互換確認済み
- **カクヨム (ID=1177354055617350769)**: **完全互換達成** — 行数完全一致 (25,273/25,273)、行単位 diff 0件。`tests/convert_parity.rs` のローカル参照 fixture で byte-for-byte 確認済み（通常の `cargo test` では ignored。参照データを用意して明示実行する手順は `tests/README.md`）
- ※米印変換、全角数字、ルビ、auto_join_line、各種文字変換も完全一致
- **変換の future は `Send`** (2026-10 修正): 調査ログ用に `NovelConverter` が保持する `Inspector` は `Arc<parking_lot::Mutex<Inspector>>` で共有する。`Rc<RefCell<_>>` のままだと `ConvertService::convert_and_store` / `convert_only` の future が非 `Send` になり、並列実行へ渡すと実行時 panic する (clippy `future_not_send` で検出)。非 `Send` の future があるときは `clippy::future_not_send` を ignore するのではなく、型を `Send` にして解消する。

### AozoraEpub3_Lite 組み込みエンジン (lite feature, 2026-09)
- pin: `aozora_epub3_lite` = `381c0b17f91946e22d41e5246d3977c706abe82e` (v0.1.6)。この revision は wasm の JS 時計対応に加え、I/O 非依存の逐次画像収集 `AssetPlan` を公開する。更新時は Cargo 経由で git revision を指定し、`cargo update -p aozora_epub3_lite` と native / Worker の実出力を検証する。
- 呼び出し側は書き出しを **1 エントリずつ**進められる (`EpubBook::stream_writer(sink)` → `next_entry()` / `write_current(bytes)` / `finish()`)。Worker の `download.epub` はこれと `worker::Response::from_stream` を組み合わせ、完成した EPUB を保持せずチャンクを流す (`ChunkSink` は `narou_rs::epub_lite` が提供)。画像計画と処理は `EpubPreparation` / `EpubBuild` で native と共有し、`EpubBuild::image` の保存元対応から 1 枚ずつ取得する (列挙は 512 枚 / 1 枚 16 MiB で 413)。
- 組み立ては Lite CLI (`main.rs::convert_input`) と同じ公開 API を使う。独自実装 (挿絵の連番化・外字フォント収集・UUID 生成) は持たない。
  - `config_for(aozoraepub3dir)` = `AozoraConfig::load_from_dirs([dir], <dir>/AozoraEpub3.ini)`。Java 版と同じ注記表・外字フォント・INI を読む。INI が無ければ `preset/AozoraEpub3.ini.default` 相当のフラグ。
  - `build_book(input_txt, options)` は `EpubPreparation` で入力を解釈し、native の `collect_assets` と Worker の逐次 `AssetPlan` の結果を同じ `finish` へ渡す。画像参照・連番・欠落除去・自動表紙・画像ページ配置・メタデータ・外字資産の組み立てを二重実装しない (`urn:uuid:` は Java と同じ `java_name_uuid`)。
  - 挿絵は `EpubBuild::resolve` が書き出し時に 1 枚ずつ読み、`image::process` (余白除去・リサイズ・回転) をかける。寸法だけ事前に読む。
- narou カスタム注記 (`preset/custom_chuki_tag.txt.default`, 21 行) は `include_str!` で常に重ねる。`init` がインストール先 `chuki_tag.txt` に書き込む内容と同一なので、同梱資産だけで動く wasm / 未設定時でも `ここから柱` / 前書き / 後書き / 一字下げ 等が効く。
- `preset/AozoraEpub3.ini.default` も `include_str!` し、`aozoraepub3dir` が無いときの既定にする (`IniSettings::parse` → `AozoraConfig::from_ini`)。Java 版は常にこの INI (init がインストール先へコピーしたもの) を読むため、外部 AozoraEpub3 が無い環境でも `TitlePage` / `CoverPage` / `SpaceHyphenation` / `DakutenType` などのフラグが一致する。実測: 資産なしでも Java と 419/423 バイト一致。
- **利用者設定と同梱の既定値は別ファイル (issue #45 の続き, 2026-10)**: `preset/` の設定は「利用者が作る `AozoraEpub3.ini` / `custom_chuki_tag.txt`」と「配布物が持つ `AozoraEpub3.ini.default` / `custom_chuki_tag.txt.default`」に分けた。**利用者設定は配布物に同梱しない** (`scripts/package-release.ps1` が `preset/` から除外する / `.github/workflows/build-linux.yml` も `.default` だけを配置する) ので、どのバージョンのセルフアップデートでも書き換わらない。`src/bin/updater.rs` も `preset/AozoraEpub3.ini` と `preset/custom_chuki_tag.txt` は既存があれば上書きしない。実行時の解決順は「利用者設定 → 同梱の既定値 → 埋め込みの既定値」で、共通定義は `src/preset.rs` に置く。
- 同梱 `replace.txt` は読み込まない (削除済み)。Java は narou.rb 構成では `replace.txt` を持たない (配布物は `replace_sample.txt`) ため、読み込むと `－`→`―` など不要な文字置換が入り Java とずれる。
- 残差: 外字フォント (`gaiji/dakuten/*.ttf`) は `aozoraepub3dir` が無いと格納できない。`AozoraConfig::gaiji_fonts` がパス指定のため、同梱資産 (バイト列) からは渡せない。Java は濁点外字に `<span class="glyph u30fc-u309a">` を出すが Lite は素の文字になる (この差は外字を使う小説でのみ発生)。
- **フォント (濁点フォント / 本文フォント)**: 外部ツールには `DakutenFontGuard` が `aozoraepub3dir/template/OPS/fonts/DMincho.ttf` と `css_custom/vertical_font.css` を流し込んで効かせる。組み込み (Lite) にはその経路が無いので、narou は同じ内容を `EpubBuildOptions::extra_assets` の `style/vertical_font.css` + `fonts/DMincho.ttf` として渡し、crate 側が本文 (`item/xhtml/*.xhtml`) からその CSS をリンクする (組み込み CSS より後ろに置くので同じ詳細度ならこちらが勝つ)。設定 `convert.epub-font` (`auto` / `always`) で選び、`always` は本文全体を `DakutenAokinMincho` で組む — U+3000 (全角スペース) を描けない Reader (超縦書き など) 向けの回避策で、`auto` は従来どおり濁点注記のある小説だけ。
- **注意**: `aozoraepub3dir` を設定すると CLI は外部ツール (jar / Lite exe) を優先する (narou.rb と同じ)。エンジンは設定 `convert.epub-engine` (値: `auto` / `lite` / `external`) で選ぶ。解決順は環境変数 → ローカル設定 → グローバル設定で、ローカルの `lite` はグローバルの `aozoraepub3dir` より優先する (issue #42 追記)。未設定 = `auto` は従来どおり外部があれば外部・無ければ組み込み、`lite` は組み込みを強制 (組み込み入りでビルドされていなければエラー)、`external` は外部を強制する。設定ページの「一般」タブからも切り替えられ、Web UI の変換は子プロセスで走るためその設定がそのまま効く。1 回だけ試すときは環境変数 `NAROU_RS_EPUB_ENGINE=lite|external` (`builtin` / `java` も可) で上書きできる。
- 実データ検証 (2026-09-15, v0.1.3): `WebNovel` の n0421du (401 セクション) で Java 版と **422/423 ファイルがバイト完全一致**、挿絵入りでも **425/426 がバイト完全一致**（単ページ画像化・連番・表紙処理を含む）。残差は `dcterms:modified` のみ（Java はローカル時刻に `Z`、Lite は UTC。Lite 側の意図的な非再現）。
- 検証手順は `docs/aozora_lite_evaluation_2026-08-23.md` の「更新 (2026-09-15)」節。

### Worker の HTTP 検証・資格情報・設定 (2026-09)

- **URL 検証の分担**: 構文判定 (scheme / host / port / アドレスリテラルの公開判定) は
  `src/platform/url_policy.rs` にあり、`HttpClient::validate_url` の**既定実装**がこれを使う。
  native は上書きして DNS 解決先まで確認する (`src/native/http.rs`)。wasm には解決手段が無いため
  worker は既定実装のまま (= 構文とリテラルのみ) で、到達可否は Cloudflare の egress に委ねる。
  同期文脈 (挿絵 URL の足切り / preprocess DSL の `fetch`) は `is_safe_public_url_syntax` を使い、
  ホスト名の解決先は実際の取得時に transport が確認する。
- **資格情報**: `D1CookieStore` (`worker_entry/src/d1_cookie_store.rs`) が native と同じ
  `app_state(scope='inv', key='login_cookie')` を読み書きする (at-rest 暗号化、鍵は secret
  `NAROU_RS_LOGIN_KEY`)。`Set-Cookie` の書き戻しは `WorkerHttpClient`、管理は
  `GET /api/login` / `POST /api/login/set` / `DELETE /api/login/{host}`。
  保存形式の互換は `src/platform/cookie_store.rs` の固定ベクタテストで担保する。
- **wasm の乱数**: `getrandom` の `wasm_js` を**ターゲット限定の依存**
  (`[target.'cfg(target_arch = "wasm32")'.dependencies]`) で有効にしている。native の依存グラフに
  `wasm-bindgen` を入れないための措置なので、`getrandom` の扱いを変えるときは `cargo tree` で確認する。
- **設定**: `SnapshotDownloaderSettings` (`src/downloader/settings.rs`) に `app_state` の
  `local` (`update.strong` / `guard-spoiler` / `auto-add-tags` / `download.use-subdirectory`) と
  `global` (`over18`)、`inv` の section hash cache を起動時に読んで渡す。同期 API と非同期 D1 の
  都合で書き戻しは no-op。`over18` 未設定は `None` のままにして年齢認証 `Blocked` 経路を保つ。

### 挿絵バケットの設定 (2026-09)

- 2 モード: (a) `[vars]` に値を置く（既定）、(b) Cloudflare Secrets Store に値を置き、CI には
  `NAROU_SECRETS_STORE_ID` と `NAROU_S3_*_SECRET_NAME`（名前だけ）を渡す。(b) は
  `ci/render_config.py` が `[[secrets_store_secrets]]` を差し込み、prefix を `narou/<target>` に導出する。
- Worker 側の解決順は Secrets Store (`<変数名>_STORE`) → `env.var` → `env.secret`。
- 検証はローカルで `python ci/render_config.py` を両モード実行し `tomllib` で読み戻す。

### Worker の大きなオブジェクト (2026-09)

- `GET /api/novels/{id}/illustrations/{name}` は、S3 構成なら presigned URL へ 302 して Worker を
  中継させない（`s3_sigv4::presign_get` + botocore 突き合わせテスト）。D1 構成ではそのまま返す。
- 移行は `plan`（件数と容量を数えるだけ）→ `copy` → `verify` の順。`asset_backend` の切替は最後。

### Worker のフロント配信・認証・ストレージ (2026-09)

- **静的アセット**: `worker_entry/build_assets.mjs` が `src/web/assets` を `public/` へ焼き込む
  (`/assets/...` の配置 + ページはルート直下 + `?v=<内容ハッシュ>` + `__NAROU_RS_WEBUI_BUILD__`)。
  `wrangler*.toml` の `[assets]` が配信し、`html_handling = "auto-trailing-slash"` で `/settings` を解決する。
  `[build]` は `node build_assets.mjs && worker-build --release`。手動で `wrangler dev` する場合は
  アセットを作り忘れないこと（`tests/run.mjs` は内部で作る）。
- **認証**: health は無認証で `authentication_required` / `authentication_configured` を返す。
  失敗は `{error:{code}}` 形式で、401 = `authentication_required`、トークン未設定 = 500 +
  `authentication_not_configured`。`NAROU_AUTH_REQUIRED=false` はローカル開発用の抜け道。
- **S3**: 資格情報は `<変数名>_STORE` バインディング (Secrets Store) 優先、無ければ secret。
  メタデータは `GET` + `Range: bytes=0-0` で読む（`HEAD` が 403 になる互換ストレージがある）。
- **キュー**: メッセージは `{version, job_id}` だけ。計画は D1 台帳が唯一の権威。
- **サイト定義**: 実効定義は isolate 内に 30 秒キャッシュ（書き込みで即時無効化）。

### Worker の CI デプロイ (2026-09)

- `.github/workflows/platform.yml` の `worker-deploy-{develop,production}`。きっかけは
  `develop` push / タグ push（+ 手動 dispatch）。staging 環境は持たない。GitHub Environments は
  用途で `Cloudflare`（Workers の secret と vars）と `CodeSining`（release の署名）の 2 つだけを使い、
  target は `NAROU_DEPLOY_TARGET` で切り替える（一覧は
  `docs/cloudflare-workers-migration-plan.md` §2.2.8）。
- 実体は `worker_entry/ci/deploy_worker.py`（provision → render → migrate → secret 投入 → deploy → smoke）。
  D1/Queue は環境名から導出して冪等に作るので、初回デプロイでも手作業が要らない。
- 資格情報が無いリポジトリではデプロイだけ理由付きで省略する（テストは走る）。

### サイト定義の差し替え経路 (2026-09)

- `src/application/site_definitions.rs` が唯一の入り口（`SiteDefinitions` + `SiteDefinitionStore` port）。
  bundle（配布物の `webnovel/*.yaml`）が種とフォールバック、ユーザー定義が同名で上書きする。
  名前はファイル名そのもの（`ncode.syosetu.com.yaml`）。
- 置き場は保存方式で選ぶ: native YAML モード = `webnovel/` フォルダ、native SQLite モード = オブジェクト
  ストア（切り替え時に一度だけ取り込み、以後ファイルを読まない）、Worker = D1。
- API は native / Worker 共通: `GET /api/sites`、`GET /api/sites/{name}`（本文つき）、
  `PUT /api/sites/{name}`、`DELETE /api/sites/{name}`。`put` は保存前にコンパイル検証する。
- 起動時に `native::site_definitions::install_effective_site_settings()` を呼び、同期コードは
  `downloader::site_setting::effective_site_settings()` を使う（未設定ならファイルから読む）。
  この起動時のスナップショット (`install_runtime_snapshot`) は fetch policy 用なので、下の
  `effective_runtime()` 側 (version gate 付き) を使う。
- 実効定義は 2 経路がある。管理 API の表示・保存は `SiteDefinitions::effective()`
  （ユーザー本文をそのまま返す。native `/api/sites` と同一）、fetch policy / Worker の
  ダウンローダ (`bundled_sites::load_site_settings`) は `SiteDefinitions::effective_runtime()`
  （`downloader::site_setting::merge_user_definition_yaml` の version gate を掛ける）。
  Worker のログイン Cookie 保存キーの畳み (`D1CookieStore::key_site_settings`) も後者を使う。
- オブジェクトストアの一覧上限は `platform::prefix_upper_bound` を使う（`{prefix}0` は CJK や英字名を
  落とすので使わない）。

### 挿絵の D1→S3 移行 (2026-09)

- `POST /api/admin/object-migration` (`copy` / `verify` / `status`) で挿絵だけを S3 へ写す。
  `asset_backend=s3` に切り替える前に実行し、進捗は `app_state('inv','migrate_illustrations')` に残す。
- コアは `narou_rs::platform::store_migration::migrate_page`（`MemoryObjectStore` でテスト済み）。
  Worker 側 (`worker_entry/src/object_migration.rs`) は D1/S3 と `app_state` を挿すだけにする。

### 保存先の振り分け (SplitStore, 2026-09)

- `src/platform/split_store.rs`: 挿絵（うごイラの APNG 含む）のバイナリだけ S3 互換ストアへ、
  それ以外は D1 などの構造化ストアへ流す。判定は「末尾 2 セグメントが `挿絵/<画像拡張子>`」で、
  小説ディレクトリ名が `挿絵` のケースを巻き込まない。切り替えは `app_state('inv','asset_backend')`
  (`d1` | `s3`)。旧 `object_backend` は撤去済み。
- ストアをまたぐ `copy` / `move_or_copy` は失敗させ、呼び出し側の想定違いを早期に検出する。

### native の S3 ストレージ (Wasabi など, 2026-09)

- 保存先の実装は core の `src/platform/s3_store.rs` 1 つで、native と Worker が同じコードを使う
  (Worker 側の `worker_entry/src/s3_store.rs` は環境から接続情報を読んで組み立てるだけ)。
  path-style URL、SigV4 署名と presigned GET、stat は `GET` + `Range`。1 オブジェクトの上限は
  64 MiB、`read_small`/`write_small` は 16 MiB (multipart は未実装で、上限超過は黙って
  切り捨てず失敗させる)。
- S3 LIST エラーは資格情報を含み得る XML 本文の先頭を表示しない。エラーコードは固定の許可リスト、任意の SignatureProvided / AWSAccessKeyId / StringToSignBytes はメモリ内比較のカテゴリだけを出す。欠落・重複・不正形式は原因を断定しない。署名失敗時だけ公開 AWS fixture の自己テストを初回実行・キャッシュする。実効 host/port は URL から userinfo/path/query を除去し、非標準 region は伏せる。native 資格情報の設定元は既存の解決時にラベルだけ捕捉し、優先順位・値の正規化を変えない。
- **署名は URL から導出する** (2026-10): `S3Store::send` は渡された URL から
  `path`/`query` を切り出して署名する (呼び出し側が canonical 値を別途渡すと
  endpoint にパス成分がある構成で乖離し得た)。`SignatureDoesNotMatch` のエラーには
  サーバーが返す `StringToSign` は実改行・文字列 `\n` / `\r\n`・XML 改行参照を正規化し、
  64 桁 hex digest とアルゴリズム・日時・スコープを分けて比較する。不正な応答は断定しない。
  digest 一致だけで secret 不正と判断せず、全文一致なら有効な鍵ペア・設定元・鍵導出・送信した
  Authorization の確認を促す（診断表示の修正自体は 403 の解決ではない）。presigned GET の
  パスは生キーから組み立てる (`object_path()` の encode 済み値を渡すと二重
  エンコードになる)。endpoint に `?`/`#` を含む値は拒否する。
- native の設定は `local_setting` の `s3.endpoint` / `s3.bucket` / `s3.region` / `s3.prefix` /
  `s3.access-key-id` / `s3.secret-access-key`。`narou setting` から読み書きでき、環境変数 (`S3_*`)
  は各項目のローカル設定が空／未設定の場合に使う（ローカル設定が優先）。項目ごとの解決なので、
  ローカルに片方の鍵だけ残ると環境変数のもう片方と組み合わさり得る。値は公開ログへ出さない。
- 挿絵を S3 に置くかは `s3.asset-backend` (`local` | `s3`、環境変数 `NAROU_RS_ASSET_BACKEND`)。
  **Worker 側の切替は従来どおり `app_state('inv','asset_backend')`** で、native は設定ファイル側に
  置く (native から `app_state` を書く口が無いため)。`s3` を選んで接続情報が欠けていれば
  **fail-closed** で失敗する (黙ってローカル保存へ落とすとディスクを食い潰す)。
- native の保存先は `src/native/object_store.rs` の `NativeStores` が組む。`SplitStore` により
  挿絵のバイナリだけが S3 へ流れ、本文・メタデータ・`.illustration_cache.yaml` は従来どおり
  ローカル (YAML / SQLite ミラー) に残る。downloader と Web のアプリサービスはこのペアを使う。
- **EPUB 生成はファイルパスを要求する**ため (`epub_lite` も外部 AozoraEpub3 もディレクトリを読む)、
  生成直前にだけ S3 から取り出し、スコープを抜けた時点で削除する
  (`native::illustrations::Materialized`)。Web のオンデマンド EPUB と CLI 変換の Lite 経路の
  両方に接続済み。
- **既存ライブラリの移行**: `narou illust s3-push` が件数と容量を数え (既定 dry-run)、`-f` で
  ローカルから S3 へ写す。`narou illust s3-verify` がバイト単位で突き合わせる。どちらも
  ライブラリ全体が対象で、`s3.asset-backend=s3` と接続情報が必要
  (`platform::store_migration::migrate_page` を共有)。
- **挿絵のハッシュ dedup** (S3 保存なら既定 ON、2026-10): `S3Location::with_illustration_dedup` を
  付けると、`挿絵/<sha256-hex>.<ext>` を作品を跨いで `illustrations/<base64url(sha256)>.<ext>` の
  グローバルプールへ寄せる (同じ画像の二重保存を防ぐ)。小説別の配置は `挿絵/` ではなく pool 側が
  正位置になり、`read`/`stat`/`read_stream` はプールに無ければ旧 `挿絵/` 配置へフォールバックする
  (移行前の実オブジェクトをそのまま読める)。`delete` は旧配置だけを指し、pool は消さない
  (他作品と共有されるため)。dedup ON のとき新規挿絵のファイル名は常に `<sha256>.<ext>`
  (mitemin 名も pool に出る。index の `iNNN→hex` 対応で参照は辿れる)。旧来の hex でない名前は
  小説ごとの `挿絵/` 配置のまま残る。
  条件は **S3 を実際に使っていること** (`s3.asset-backend=s3`) だけで、storage-backend の
  YAML / SQLite は問わない。Worker も `illustration_dedup: true` で、全作品・漫画の挿絵が
  同じプールに入る。native は `native::s3::illustrations_in_s3()`、Worker は常に真
  (`illustration_store::dedup_active()`)。
- `narou illust s3-dedup`: 旧 `挿絵/` 配置の S3 オブジェクトを pool へ `copy` し、pool にある
  ことが確認できたものだけ旧配置を `delete` する (既定 dry-run、`-f` で実行。S3 保存なら
  YAML / SQLite を問わず使える)。hex 名でない挿絵は小説ごとの配置を保つため対象外。
  `s3-push` は dedup ON なら新規 hex 挿絵がそのまま pool に出るので、`s3-dedup` は
  「dedup を ON にする前に書かれた分」の後処理。Worker は同じ処理を
  `POST /api/admin/illustration-dedup` (`action: status|plan|copy|reset`) でページ単位に実行できる。
- **SORAHOST は `concurrency=true`**: DL/update と convert/send を別レーンで並行に流す
  (小説単位の排他は `.narou/lock.yaml`)。実測ピークは 2 ジョブ同時で約 120MB (debug) +
  常駐 31MB で、256MB でも通常運用は収まるが、うごイラ組み立て (~56MB) や大きい挿絵入り
  EPUB のスパイクを吸収するなら **384MB** を推奨 (ピーク約 300〜320MB の見込み)。
- **SORAHOST の既定は容量優先**: `sorahost/start.sh` が初回に `convert.section-cache=false`
  (話ごとの変換キャッシュ無し)・`economy=nosave_diff,nosave_raw` (更新ごとの差分スナップショットと
  raw HTML を保存しない)・`convert.no-epub=true` (EPUB を保存しない)・`convert.keep-txt=false`
  (txt を残さない) を入れる。どちらも Web UI の EPUB ダウンロードは SQLite の変換済みテキスト
  (`novel_outputs`) から都度生成するので影響しない。
- **実ファイルを書かない構成** (`sqlite.mirror-files`、既定 true。環境変数
  `NAROU_RS_MIRROR_FILES=0` でも切れる): SQLite モードの `小説データ/` ミラーを
  書かない。保存先は `objects`/`object_chunks` だけになる。読み出しは DB と
  `objects` で完結し、変換が要求するファイル (`toc.yaml` / `本文/*.yaml` /
  `setting.ini` / `replace.txt`) は `NativeStore::materialize_novel_files` が
  スコープの間だけ実体化し、抜けると自分が書いた分だけ消す
  (`MaterializedNovelFiles`)。実ファイル前提の補助 (`narou illust` の
  orphan/rebuild/fix-ext、`narou clean`) は対象が無いため実質何もしない。
  `narou backup` / `narou db export-yaml --in-place` は従来どおり動く。
- **生データ (raw HTML) を保存しない構成**:
  - `DownloaderSettings::save_raw_html()` が唯一の判定。native は `economy` に `nosave_raw` が
    あれば false、**Worker は常に false** (`WorkerDownloaderSettings` / `SnapshotDownloaderSettings`)。
    raw は取得時にメモリ上にある HTML を使って挿絵を走査するので、保存を切っても新規話の挿絵は
    従来どおりローカライズされる。保存済み raw を読むのは `narou illust orphan` /
    `rebuild` / mitemin 移行だけで、いずれも raw が無ければ「参照が減る」方向に倒れる
    (キャッシュ由来の到達可能性は変わらない)。
  - 挿絵の削除を伴う `illust orphan -f` を raw 無しで使うと、キャッシュに載っていない
    挿絵は孤児と判定される点に注意 (キャッシュは保存時に必ず書かれる)。
- **変換済みテキストを保存しない構成** (`convert.keep-txt`): false のとき変換済みテキストを
  どこにも残さない — `novel.txt` 固定名ミラー・`novel_outputs` 行・オブジェクトの
  `<prefix>/novel.txt` すべて対象で、変換後の txt ファイルも削除する。EPUB は
  ダウンロードのたびに保存済み本文から組み立て直す (native の `generate_epub_on_demand` は
  `ConvertService::convert_only`、Worker の `download.epub` も同じ)。以前に残った
  `novel_outputs` 行・`novel.txt` オブジェクトは、false 状態で走った変換が掃除する。
  **native は既定 true、Worker は既定 false** (D1 を食わないため)。`executor` の
  「変換が必要か」判定も保存しない構成では常に false になる。
  環境変数 `NAROU_RS_KEEP_TXT=0` / `=1`、D1 の `convert.keep-txt` で切り替える。
- **話ごとの変換キャッシュは容量の厳しい環境で切れる**: `convert.section-cache`
  (local 設定、既定 true、環境変数 `NAROU_RS_SECTION_CACHE=0` で無効)。無効時は読み書きせず、flush 時に既存の
  `section_convert_cache/<id>.yaml` を削除する (再有効化しても壊れない)。既定では
  brotli 圧縮 + 1 行ヘッダ (`narou-section-cache:v1 <encoding>`) で保存し、ヘッダの無い
  旧形式 (生 YAML) は読み込み時に dirty 扱いにして次の flush で圧縮し直す。
  Worker (`worker-runtime`) はこのキャッシュ自体を持たない (該当コードが
  `native-runtime` 限定で no-op)。
- HTTP 層に `HttpMethod::{Put, Delete}` と `HttpRequest::trusted_endpoint` を追加した。後者は
  保存先が利用者自身の設定値であることを示し、公開アドレス判定を掛けず (ローカル MinIO を許可)、
  応答ヘッダを全部返す (`ETag` / `Content-Range` が保存先の契約)。

### 外部リレーの独立運用

- **SORAHOST と GCP (Cloud Run) へのリレー配備は非推奨 (2026-10-21) で、Node 版リレーは削除した。** 旧配備スクリプト (`scripts/relay/bootstrap.mjs` / `deploy-sorahost.mjs` / `relay.js` / `pins.json` / `worker/`) と `docs/cloud-run-relay.md` / `docs/sorahost-relay.md` も削除済み。リレーは VPS (`relay.md` / `docs/vps-relay.md`) または自宅回線 (`docs/home-relay.md`) で運用する。再導入しない。
- 本体 CI は Native / GPL / Wasm / Worker / ライセンス等の通常の検証後に Worker を配備する。SORAHOST 専用ジョブ・`SORAHOST` 変数・`run_workers` 入力・リレー配備への `needs` は撤去済みで、再導入しない。
- 外部取得リレーはワークスペース crate **`relay/`** の Rust バイナリ **`narou_relay`**。`GET|POST /proxy`、`X-Proxy-Token`、JSON/base64 応答 (`setCookie` を含む) は旧 Node 版と同一の契約。本体の `platform::relay` と `WorkerHttpClient::with_relay` は維持する。ビルドは `cargo build --release -p narou_relay`、実行は `./target/release/narou_relay` (Windows は `target\release\narou_relay.exe`)。
- **リレーは配備先ごとに分岐させない。** ホスト差は `relay/src/config.rs` が吸収する: `PORT` → `SERVER_PORT` (既定 3000)、`NAROU_RELAY_HOST` (既定は `K_SERVICE` の有無で `0.0.0.0` / `127.0.0.1`)、`.env` (`NAROU_RELAY_ENV_FILE` → 実行時 CWD の `./.env` → バイナリのディレクトリ → `$HOME`。既存の環境変数を優先)、`NAROU_RELAY_FETCH` (取得順。既定 `curl,wget`)、`NAROU_RELAY_CURL` / `NAROU_RELAY_WGET` (既定は PATH の curl / wget)。サーバー実装にホスト固有の分岐を足さない。
- 取得トランスポートは **システムの curl / wget** のみで、Node / Web fetch による代替経路は持たない。`NAROU_RELAY_FETCH` は `curl` / `wget` のカンマ区切りで、使えない wget は起動時に取得順から外れる。未知の名前・1 つも使えない構成は起動エラー。curl は `--resolve`、wget (HTTPS のみ) は検証済み IP へ接続する使い捨て CONNECT プロキシで接続先を固定し、リダイレクトは追わない (3xx は `location` として返す)。1 リクエスト = 上流 1 回、全体 75 秒の期限とクライアント切断で子プロセスを中断する。
- 応答は `{status, contentType, location?, setCookie, via, body(base64)}`、エラーは `{"error": "..."}`。`/health` は認証不要で `{"ok":true,"fetch":"<実際の取得順>"}` を返し、起動ログは `narou relay listening on <host>:<port> (fetch: curl:path, wget:path)`。
- サーバーは認証を本文読み取りより先に行い、未認証・`/proxy` 以外・不正メソッドは本文を読まずに 4xx で閉じる。同時接続数上限 (128) と完了タスクの回収で、未認証クライアントによるソケット/タスク保持・`JoinSet` の無制限増加を防ぐ。wget 経路は CONNECT トンネルの確立を確認してから応答を採用する。`NAROU_RELAY_CACERT` 未設定時は起動時に警告する。
- リレーの環境変数: `NAROU_RELAY_KEY` (必須)、`NAROU_RELAY_ALLOWED_HOSTS`、`PORT` / `SERVER_PORT`、`NAROU_RELAY_HOST`、`NAROU_RELAY_FETCH`、`NAROU_RELAY_CURL` / `NAROU_RELAY_WGET`、`NAROU_RELAY_CACERT`、`NAROU_RELAY_ENV_FILE`。鍵・値はログへ出さない。
- Worker とリレーに同じ `NAROU_RELAY_KEY`、Worker に `NAROU_RELAY_ENDPOINT` を設定する。GitHub Environment `CloudFlare` の Secret `RELAY_DOMAIN` (例 `relay.example.com`) を置いた場合は、`worker_entry/ci/deploy_worker.py` が `NAROU_RELAY_ENDPOINT` より優先して `https://<RELAY_DOMAIN>` を Worker secret に入れる (名前付きトンネル等で URL を固定するとき用。`.github/workflows/platform.yml` の develop / production 両ジョブが `RELAY_DOMAIN` を渡す)。旧 `SORAHOST_PROXY_*` の別名は受け付けない。公開運用では HTTPS を使い、リレーの既定待受 `127.0.0.1:3000` はリバースプロキシまたはトンネルで公開する。
- 本体 CI はホストのプロビジョニングやリレーの配備を行わない。配置後の確認は `python3 scripts/relay/verify_auth.py`（fail closed: 到達不能・想定外ステータスは exit 1、公開先は https 必須、3xx 非追跡）、リレーの契約・ポリシーの回帰は `cargo test -p narou_relay` (ワークスペース CI は `cargo test --workspace`) を使う。詳細は `docs/platform-abstraction.md`。
- **Cloudflare 保護サイト（ハーメルン等）はデータセンター回線のリレーでは取得できない (2026-10-20 実測)。** GitHub Actions (Azure のデータセンター) から `https://syosetu.org/` は curl (HTTP/2)・wget・Node HTTP/1.1・Node HTTP/2 のすべてで 403 `cf-mitigated: challenge`、対照の `ncode.syosetu.com` は 302。同じリレーを自宅回線で動かすと curl と Node HTTP/2 は 200 になる (当時のリレーは Node 版で、Node の HTTP クライアントも候補だった。現在の Rust 版の経路は curl / wget のみ)。クライアント変更 (curl→wget 等) では解決しないため、この用途に VPS リレーは使えない。自宅回線リレーは `scripts/relay/home-relay.ps1` (リレー + cloudflared クイックトンネル) と `docs/home-relay.md` の手順で動かし、設定は `scripts/relay/.env` (gitignore 済み、`.env.example` を複製) に置く。Worker の `NAROU_RELAY_ENDPOINT` を表示されたトンネル URL、`NAROU_RELAY_KEY` を `.env` と同一にする。Pixiv などデータセンター回線でも取得できるサイトは VPS のリレー (`relay.md` / `docs/vps-relay.md`) で取得できる。ただし共有 NAT でも wget が通るホストがある (2026-10-20: 格安 IPv6-only VPS の AS2514 共有 IPv4 から curl 403 / wget + ブラウザ UA + サイトヘッダ 200)。そのため `NAROU_RELAY_FETCH` (取得順、既定 `curl,wget`) と wget トランスポートを持つ。wget は `--resolve` 相当が無いので、検証済み IP へ接続する使い捨て CONNECT プロキシを 127.0.0.1 に立て、TLS の SNI・証明書検証は wget に任せる。HTTP 対象は wget では扱わない。レスポンスの `via` は `curl` / `wget`。`/health` は実際の取得順を `fetch` で返す。
- **リレーへ渡す JSON のヘッダ名は大文字小文字の綴りを保持する (2026-10-21 修正)。** HTTP/1.1 の wget / curl は与えられた綴りをそのまま送るため、`platform::relay::RelayRequest` で小文字へ正規化すると Cloudflare が `Sec-Fetch-*` 等をブラウザ相当と見なさず 403 `cf-mitigated: challenge` を返す。同名は大文字小文字を区別せず束ね、最初に現れた綴りを JSON のキーにする (値を配列にして順序を保つ既存規則は維持)。ハーメルン (`syosetu.org`) の VPS リレーで実測: 直接 wget + サイトヘッダは 200、小文字化した JSON 経由は 403、修正後の JSON は 200 `via=wget`。回帰は `src/platform/relay.rs` のユニットテスト。
- **CA バンドルは侵害されたルート CA を拒否する (2026-10-21、eMudhra 中間 CA 侵害 2026-10-20 対応)。** eMudhra の 8 つの中間 CA は秘密鍵が復元可能だったため、失効情報を扱わないクライアントは `AAA Certificate Services` / `emSign ECC Root CA - G3` 等へ連鎖する偽造証明書を受け入れる。リレーは TLS を curl / wget に委ねるため、`NAROU_RELAY_CACERT` を渡す前に `relay/src/ca.rs` が SHA-256 指紋デナイリストで侵害ルートを検査し、含まれていれば `narou relay startup failed: CA バンドルを検証できません: ...` で起動を中止する (fail closed)。未指定なら各クライアントの既定ストアを使い、起動時に検査が無効である旨の警告を出す。VPS は `ca-certificates` 更新 + `NAROU_RELAY_CACERT=/etc/ssl/certs/ca-certificates.crt`、Native は OS ストア依存のため OS 更新が必要。Cargo.lock の Rust TLS 依存も更新: `rustls-webpki` 0.103.16（失効した中間 CA の SPKI を拒否する）、`rustls` 0.23.46。`webpki-roots` / `webpki-root-certs` は本リポジトリの依存に無い（native は native-tls/OS ストア）。回帰は `cargo test -p narou_relay` の CA 検証・起動テスト。
- 既存のサービス・Secret・保存済み資格情報はコード変更だけで削除しない。新しい設定を用意する前に、旧設定を勝手に消したり本番の取得を無効化したりしない。旧ホストの手動 native 配置資料は `sorahost/README.md` に残すが、CI とは接続しない。

### Worker の契約テスト (2026-09)

- `worker_entry/tests/contract.mjs` … HTTP 契約（health / 認証 fail-closed / 一覧・ジョブ API /
  queue 経由の Convert 終端）。`BASE_URL` と `NAROU_ADMIN_TOKEN` を env で渡す。
- `worker_entry/tests/run.mjs` … ビルド → ローカル D1 へ migration → `wrangler dev` → 契約テスト。
  `.dev.vars`（無ければ `NAROU_ADMIN_TOKEN` を書き込む）と `wrangler.test.toml`（`wrangler.toml` から
  `[build]` を外した生成物）は gitignore 済み。
- CI: `.github/workflows/platform.yml` の `worker-contract`（push で実行）。デプロイは手動トリガーの
  `worker-deploy`（environment ゲート）で、必要な secret/vars はワークフローのコメント参照。

### Worker 内の変換 (2026-09)

- `ConvertService` (`src/application/convert.rs`) が変換テキストを組む唯一のポータブル経路。
  ObjectStore 上の `toc.yaml` / `本文/*.yaml` / `setting.ini` / `replace.txt` / `converter.yaml` と
  `SettingsStore` の `default.*` / `force.*` を入力に `NovelConverter::build` → `convert_novel` を実行し、
  `<prefix>/novel.txt` へ書く（native の `convert_novel_by_id` と同じ固定名ミラー）。
- `converter/**` は `native-runtime` ゲートで fs/プロセス経路を切り離してある。Worker 側では
  セクション変換キャッシュ・挿絵ローカライズ・device 出力を持たない（毎回変換する）。native 専用の
  補助が未使用になるため、`worker-runtime` 構成に限り `converter` モジュールの dead_code を許可する。
- 新しい converter の機能を足すときは、`#[cfg(feature = "native-runtime")]` を付けた関数から
  fs/プロセスを触るようにし、純関数 (`NovelSettings::from_sources` / `parse_replace_patterns` /
  `UserConverter::from_yaml` など) を worker から使う。

### Pixiv 対応 (webnovel/www.pixiv.net.yaml, 2026-09)
- 4 種の対象に対応: 小説 (`/novel/show.php?id=N`) / 小説シリーズ (`/novel/series/S`) / イラスト・漫画 (`/artworks/A`) / 漫画シリーズ (`/user/U/series/S`)。ncode は種別ごとに接頭辞を付ける (`n` 小説, `s` 小説シリーズ, `a` イラスト・漫画, `c` 漫画シリーズ)。作品ページの HTML は Next.js の SPA シェルで本文を含まないため、`/ajax/*` の JSON API だけを使う。サイト固有の Rust 処理は無く、すべて YAML + `preprocess:` DSL で表現している。
- 取得元: シリーズ詳細 `/ajax/novel/series/{id}` (作品情報 + 目次 1 ページ目への誘導)、シリーズ目次 `/ajax/novel/series_content/{id}?limit=30&last_order=N&order_by=asc` (30 話ずつ、続きがあれば `next_toc` で辿る)、本文 `/ajax/novel/{id}`。
- ncode は URL の数値だけだと作品種別をまたいで衝突するため、`ncode:` キーでページから `n` + 数値 (小説) / `s` + 数値 (シリーズ) を組み立てる。`ncode` はサイト定義の新キーで、URL に種別プレフィックスが無いサイト向けの汎用機能。
- 目次は `body.thumbnails.novel` から作る (`page.seriesContents` と同じ順序で話数 `seriesContentOrder` と掲載日を持つため)。ページが 30 件で埋まっているときだけ `next::` 行を出し、`next_toc`/`next_url` がそれを拾う。
- 本文記法: `[[rb:base>ruby]]` → `<ruby>`、`[[jumpuri:text>url]]` → `<a>`、`[chapter:X]` → 見出し行、`[newpage]` → `［＃改ページ］`、`[jump:N]` → `（Nページ目へ）`。
- 挿絵: `[pixivimage:ID]` / `[pixivimage:ID-N]` は ID から画像 URL を引く追加 API (`/ajax/illust/{id}/pages`) が必要なので、DSL が `fetch_json(...)` で要求し、実行側が取得して再実行した結果を `<img src>` に置き換える。`[uploadedimage:ID]` は同じ応答の `textEmbeddedImages` から解決する (追加取得なし)。解決できなかった参照は `<!--...-->` の目印だけ残し、取得失敗で本文を落とさない。実 URL は `illust_grep_pattern` が拾って `挿絵/` にローカライズする。
- ログインが必要な作品 (R-18 / ログイン限定) は HTTP 200 のまま `content` が欠ける。DSL が `login_required::1` を出し、`login_pattern` と `error_message` の両方に一致させて、保存済み Cookie での 1 回再試行 → 駄目なら 404 判定に乗せる。
- イラスト・漫画 (`/artworks/A`) は narou の「小説」として登録する: `a{A}`、短編 (1 話)、本文はページ画像 (`/ajax/illust/{id}/pages` の各ページを `<img src>` にしたもの)。文字数は 0。ブリッジ (`narou_bridge`) の `dl_art` と同じ表現。
- 漫画シリーズ (`/user/U/series/S`) は `c{S}` の連載として登録し、各話 = シリーズ内の作品。一覧 API `/ajax/series/{id}?p=N&lang=ja` は 12 件ずつ・`order` の **降順** で返るため `.reverse` して昇順にし、`order 1` に到達するまで `next::` で次ページを要求する。各話の題名・作者・掲載日は作品ページ (`/ajax/illust/{workId}`) から取る (一覧には ID と順序しか無い)。公開話数は `illustSeries[0].total`。
- **タグ**: 漫画シリーズのシリーズ情報 (`illustSeries[0]`) にはタグが無い (トップページにタグが設定されず、各話だけにタグがあるケース)。各話の作品 API `/ajax/illust/{workId}` の `tags.tags` を preprocess が `tag::` 行として出し、narou 側が全目次ページ分を重複除去して初出順にレコードのタグへ統合する (2026-10-21 修正、YAML version 1.6)。小説シリーズは `series_content` の `entry.tags` から同じ仕組みで収集する。既存レコードも次回更新時にタグを追記する (本文に変更が無くても `auto-add-tags` が有効なら統合)。
- ログインしていないと R18 作品は一覧から**黙って除かれる** (404 にならないので再試行も走らない)。R18 を含むシリーズは `narou_rs_login` で先に Cookie を保存しておくこと。
- 分岐の注意: `illustType` はイラストで 0 になり、DSL では数値 0 が偽になる。作品ページ判定は `illustTitle` の有無で行う。
- **サイト定義の `version`**: `webnovel/*.yaml` は `name` で照合し、**ユーザー側コピーの version が同梱版以上 (`>=`)** のときだけ、そのファイルのキーを同梱版へ上書きマージする (キー単位のマージなので、ユーザーが書いていないキーは同梱版が残る。唯一の実装は `downloader::site_setting::merge_user_definition_yaml` — native の `loader.rs` と core の `SiteDefinitions::effective_runtime()` (Worker の fetch policy / Cookie のサイト畳みも) が共有する。管理 API 表示用の `SiteDefinitions::effective()` は gate を掛けない)。挙動を実測すると: 2.3 のユーザー定義は同梱 2.4 に**無視され**、2.4 のユーザー定義は**優先される**。したがって **同梱定義の既存キーを変更したら version を必ず上げる** (上げないと初期化時にコピーされた古い値が優先される)。新しいキーの追加だけなら version を上げなくても届くが、まとめて反映させる意味でも変更のたびに +0.1 を目安に上げる。
- **表示用 URL (`original_url`)**: 取得に使う `toc_url` と利用者が指定した URL が違う場合、後者をレコードの追加フィールド `original_url` に控える (`NovelRecord::set_original_url` / `display_url`。`narou.rb` は未知キーとして無視する)。Pixiv は `toc_url` が API (`/ajax/...`) になるため、これが無いと Web UI のリンクが API を開いてしまう。通常サイトは両者同じなので記録しない (`target != toc_url` のときだけ)。Web UI は API の `display_url` を優先し、無ければ `toc_url` にフォールバックする。既存レコードは再ダウンロード時に埋まる。
- **アクセス間隔**: サイト定義の `min_interval` (秒) がそのサイトへのリクエスト間隔の下限になる (全体設定 `download.interval` より優先)。`RateLimitScope` がサイト定義の値を持ち、native limiter は `max(download.interval, min_interval)` で待つ。Pixiv は短時間の連続アクセスで 429 を返すため `min_interval: 5` を置いている (運用要件は最低 2 秒・できれば 5 秒)。
- **Pixiv の欠けの見分け方**: 匿名でも `/ajax/novel/series_content/{id}` の `page.seriesContents` には全話の id と話数が入っており、中身を伏せられている話だけ `series.viewableType` が 0 以外になる (ログイン時は全部 0)。見える話だけを持つ `thumbnails.novel` との差が「空のデータ + 実データ」の実データ側なので、これを欠けの判定に使う (0 件 / 1 話目から始まらない、は viewableType が無い応答向けの保険)。漫画シリーズの `/ajax/series/{id}` は匿名だと R-18 の id 自体を落とすため、そちらは 1 ページ目の最新 order と `series.total` で判定する。
- **一覧が欠けたまま成功する応答**: サイト定義に `login_partial_pattern` を置き、DSL が `login_partial::1` を emit すると「未ログインで一部の作品が落ちている」とみなす。保存済み Cookie で 1 回だけ再取得し、**欠けが解消したときだけ**採用する (解消しなければ匿名の結果をそのまま使うので 404 化しない)。Pixiv の漫画シリーズは未ログインだと R-18 が落ちて 16/21 件しか返らないため、1 ページ目の最新 order と `series.total` を比べてこの目印を出す。小説シリーズは本文一覧のページが未ログインで 0 件になるので、そこでも目印を出す (目次は複数ページに分かれるため、検知は `parse_subtitles_multipage` が全ページを見て行い、話数が増えたときだけ Cookie 付きの結果を採用する)。
- **うごイラ**: `illustType == 2` の作品はフレーム集約 zip と各フレームの表示時間 (`/ajax/illust/{id}/ugoira_meta`) で配られる。DSL が `zip の URL + "?ugoira=" + 遅延ms のカンマ区切り` を `<img src>` として出し、`src/illustration_animation.rs` が APNG に組み立てる (ブリッジの Pillow 実装と同じ出力形式)。フレームは JPEG/PNG をデコードするので `image` クレート (jpeg, png のみ) を使う。両クレートは `illustration-animation` feature (`dep:image` + `dep:zip`) にまとめ、native と Worker (`worker-runtime` 経由) の両方で有効。`zip` は wasm でも動くよう `default-features = false` + `deflate-flate2-zlib-rs` (純 Rust の flate2) に絞ってある (既定 feature は bzip2/zstd/lzma 等の C 実装系を取り込むため wasm32-unknown-unknown に載らない)。feature を外したビルドでは `assemble_animation` が未対応エラーを返し、呼び出し側は落としたアーカイブをそのまま保持する。全フレームを RGBA で揃える (半透明を含む作品があるため、サイズより忠実さを優先)。単一フレームなら通常の PNG、遅延やアーカイブが無ければ呼び出し側の通常経路 (静止画) にフォールバックする。実機確認: `a69642452` (19 フレーム, 1077x690, 21.3MB の APNG、Chromium がフレーム 0 を正しく描画)。
- 画像ホスト用ヘッダ: `i.pximg.net` は `Referer` 無しだと 403 を返すため、サイト定義の `headers:` キーで `Referer: \k<top_url>/` を宣言する (値は `\k<...>` 補間される)。
- 実機確認 (2026-09-23): 単体作品 (短編, `n26352975`/`n29204764`)、シリーズ 6 話 (`s16299140`)、シリーズ 42 話 (`s16305923`, 目次 2 ページ)、シリーズの 1 話 (`n29205030`, 前書き/改ページ/章見出し/ルビ)、挿絵付き作品 (`n29198933`, `[pixivimage:]` → `挿絵/` へローカライズ) で DL・変換・再更新 (差分なし) を確認。ログイン限定作品は Cookie 無しで 404 判定になることも確認。
- 追加確認 (2026-09-23): イラスト `a141939696` (1 枚) / 漫画 25 ページ `a143868144` (**挿絵 25 枚**) / 漫画シリーズ `c311834` (5 話・挿絵 16 枚) / 漫画シリーズ `c205917` (16 話を 2 ページに跨って取得・R18 の 5 作品は非ログインのため一覧から除外) で DL・変換・再更新 (差分なし) を確認。

### ダウンロード互換性
- なろう (n8858hb, 24セクション) DL完走確認済み
- カクヨム (ID=2, 294セクション) DL完走確認済み
- syosetu.org（ハーメルン）: UAランダム化、HTTP/1.1/Cookie/圧縮/curl fallback による403回避対応済み。R18 分離ドメイン h.syosetu.org も同一サイトとして対応（412369=56セクション、405366=15セクションで h あり/なし双方向の DL・重複防止を実機確認済み）
- ハーメルン R18 (h.syosetu.org) は Cloudflare の managed challenge 配下にあり、ブラウザが必ず送る `Sec-Fetch-*` が無いリクエストは `403` + `Cf-Mitigated: challenge` で弾かれる。`webnovel/syosetu.org.yaml` の `headers:` で実ブラウザ相当の `Sec-Fetch-*` / `Upgrade-Insecure-Requests` / `Accept` / `Accept-Language` を明示して回避する（2026-09 対応）。
- 同じホストで reqwest は 403、libcurl は 200 を返す（TLS/HTTP クライアント差）。そのため `send_manual`（リダイレクトを自前で辿るモード）も curl ティアを先に試すようにした。以前は reqwest 固定で、curl は 4xx/5xx 時のリダイレクト探索にしか使っておらず、challenge 下のサイトで本文ごと 403 になっていた。
- Arcadia: `href` の `&amp;` デコード修正により本文取得修正済み

### YAML駆動サイト定義
- 2026-05: 完了。`kakuyomu_preprocess` ハードコードは除去され、YAML の `preprocess:` DSL ブロック + pest 文法 + セーフインタプリタで駆動される。
- 新サイト追加やサイト構造変更は `webnovel/*.yaml` の編集だけで対応可能。

### Web UI
- 全APIエンドポイント実装済み (70+)
- Pure JS/CSS frontend (JP/EN切替、テーマ切替、レスポンシブ対応)
- WebSocket プッシュ通知 (ジョブ進捗、ログストリーミング)
- 自動更新スケジューラ (queue-backed, scheduler restart without server restart)
- キュー並列実行 (concurrency 有効時: primary lane DL/update + secondary lane convert/send)
- Basic認証、Host/Origin検証、CSRF対策、reverse proxy モード
- Windows タスクトレイ常駐 (`--hide-console`)
- **ブラウザ標準の `prompt` / `confirm` / `alert` は使わない**。`src/web/assets/js/ui/dialog.js` の `confirmDialog` / `alertDialog` / `promptDialog` / `formDialog` / `selectDialog` (すべて Promise) を使う。markup は初回表示時に JS が組み立てるので、`index.html` と `settings.html` のどちらからでも import できる。新しい JS モジュールを足したら `src/web/frontend.rs` の asset 配信表 (match arm) にも必ず追加する (Worker は `worker_entry/build_assets.mjs` が `src/web/assets` を丸ごと写すため追加作業は不要)。

### コマンド実装状況 (詳細は `COMMANDS.md`)
- ✅ 完了 (24): init, list, tag, freeze, remove, setting, diff, send, mail, backup, clean, illust, author, help, version, log, folder, browser, alias, inspect, csv, trace, db, login
- 🟡 部分 (4): download, update, convert, web
- ❌ 未実装 (0): 全コマンド何らかの実装あり

## 未解決の既知課題

### 2026-04: WEB UI の自動更新ボタンが出ない件
- 現象: v0.1.32 で `latest_version != current_version` にも関わらず `update_available: false`
- 該当コード: `src/web/misc.rs::version_latest`
- 仮説: 不可視文字混入、キャッシュ不整合、v0.1.32 固有のコードバグ
- 再現環境は喪失 (ユーザー側アップデート済み、2026-04-26)
- `841bec5` で `NAROU_RS_RELEASE_BUILD` フラグ焼き込み済み
- 対応指針: `version_latest` 防御的書き直し、JS 側フォールバック判定、生バイト列検査
- **2026-09 対応済み**: `version_latest` を書き直し、`version_core`（数字と `.` のみ抽出）+ `version_is_newer`（数値タプル比較、パース不能時は不一致フォールバック）で不可視文字・表記揺れを吸収するようにした

### YAML駆動サイト定義
- 2026-05: 完了。`kakuyomu_preprocess` ハードコードは除去され、YAML の `preprocess:` DSL ブロック + pest 文法 + セーフインタプリタで駆動される。
- 新サイト追加やサイト構造変更は `webnovel/*.yaml` の編集だけで対応可能。

## Converter Pipeline (Ruby準拠)

### `convert(text, text_type)` 全体フロー:
1. `rstrip_all_lines` — 全行の行末空白削除
2. user_converter `apply_before`
3. `before_hook`:
   - body/textfile: `convert_page_break` (閾値以上の連続空行→`［＃改頁］`)
   - non-story + pack_blank_line: `\n\n` → `\n`, 先頭3改行を2に制限
4. `convert_for_all_data` — 一括前処理:
   - hankakukana_to_zenkakukana
   - auto_join_in_brackets
   - auto_join_line (if enabled) — `、\n　` のみ結合
   - erase_comments_block
   - replace_illust_tag → `［＃挿絵＝N］`
   - replace_url → `［＃URL=N］`
   - replace_narou_tag — `【改ページ】` を削除
   - convert_numbers — subtitle/chapter/story は全角変換のみ
   - exception_reconvert_kanji_to_num, convert_kanji_num_with_unit, rebuild_kanji_num
   - insert_separate_space
   - stash_kome(`※`→`※※`), convert_double_angle_quotation_to_gaiji, convert_novel_rule, convert_head_half_spaces
   - convert_fraction_and_date, modify_kana_ni_to_kanji_ni, convert_prolonged_sound_mark_to_dash
5. `convert_main_loop` — 行単位処理 + 後処理:
   - zenkaku_rstrip, request_insert_blank, process_author_comment
   - insert_blank_before_line_and_behind_to_special_chapter
   - insert_blank_line_to_border_symbol (■等の前後に空行+4字下げ)
   - outputs(line) → join
   - rebuild_force_indent_chapter
   - rebuild_illust, rebuild_url, rebuild_hankaku_num_comma
   - rebuild_kome_to_gaiji (`※※` → `※［＃米印、1-2-8］`)
   - half_indent_bracket, auto_indent (E000 sentinel marker → `\u{3000}`)
   - narou_ruby, convert_horizontal_ellipsis, convert_double_angle_quotation_to_gaiji_post
   - delete_dust_char
6. user_converter `apply_after`
7. `replace_by_replace_txt` — replace.txt ユーザー定義置換

### `novel.txt.erb` テンプレート構造 (Rustの `render_novel_text` に実装済み):
```
Title\n
Author\n
cover_chuki\n
［＃区切り線］\n
(if story non-empty) あらすじ：\n{story}\n\n
掲載ページ:\n<a href="{toc_url}">{toc_url}</a>\n
［＃区切り線］\n
For each section:
  ［＃改ページ］\n
  (if chapter non-empty)
    ［＃ページの左右中央］\n
    ［＃ここから柱］{title}［＃ここで柱終わり］\n
    ［＃３字下げ］［＃大見出し］{chapter}［＃大見出し終わり］\n
    ［＃改ページ］\n
  (if subchapter non-empty)
    ［＃１字下げ］［＃１段階大きな文字］{subchapter}［＃大きな文字終わり］\n
  \n
  {indent}［＃中見出し］{subtitle}［＃中見出し終わり］\n
  \n\n
  {body}
  (if postscript) ...
(if enable_display_end_of_book) \n［＃ここから地付き］［＃小書き］（本を読み終わりました）［＃小書き終わり］［＃ここで地付き終わり］\n
```

## 技術スタック
- **Language**: Rust (edition 2024)
- **Web framework**: Axum 0.8
- **Async runtime**: Tokio (full features)
- **Serialization**: serde + serde_yaml + serde_json
- **HTTP client**: reqwest (blocking + async, cookies, gzip/brotli/deflate, native-tls via `native-tls-vendored`) + curl crate。`default-features = false` で rustls を避けている (Windows で aws-lc-rs の NASM 依存を踏まないため)
- **CLI**: clap 4
- **Date/time**: chrono + chrono-tz
- **Regex**: regex
- **Hashing**: sha2 + hex
- **Error handling**: thiserror
- **Logging**: tracing + tracing-subscriber
- **Sync**: parking_lot, dashmap, tokio::sync
- **Browser open**: open
- **WebSocket**: tokio-tungstenite
- **HTTP client (low-level)**: curl crate
- **Random UA**: ua_generator
- **管理DB**: SQLite (`rusqlite` bundled, optional dep / native-runtime)。**既定は従来の YAML 管理**で、`.narou/storage-backend` マーカー (`sqlite`) または Web UI ツアーでの選択で `.narou/db.sqlite` 管理へ切り替えるオプトイン方式。`NAROU_RS_LEGACY_YAML=1` は SQLite を完全に無効化する
- **EPUB エンジン (オプション)**: `aozora_epub3_lite` (git 依存, rev pin) — cargo feature `lite` で有効化。`worker-runtime` は自動的に `lite` を含む。`lite` ビルドは GPL-3.0-only (assets/aozora_lite/LICENSE.md)、無しは従来どおり BSD-2-Clause + 外部 AozoraEpub3 プロセス。
- **サードパーティライセンス**: `cargo-about` で 2 種類生成する。GPL 側は `about.toml` + `--workspace` → `Third-Party-License.md`（`aozora_epub3_lite` と `narou_worker` に限り GPL-3.0-only を crate 単位で許可）。非 GPL 側は `about-non-gpl.toml` + `about-probe/`（`lite` 無しの `narou_rs` に依存する切り離し manifest）→ `Third-Party-License-non-GPL.md` で、GPL を一切許可しないゲートを兼ねる。`worker_entry` が `worker-runtime` 経由で `lite` を常時有効化するため、workspace 直下の走査は必ず GPL 側になる。生成コマンドは `about.hbs` の冒頭に記載。CI (`platform.yml` の `license` job) が両方を再生成して差分ゼロを検証するため、依存を変更したらノーティスも再生成して同時にコミットすること（生成器は `cargo-about` 0.9.2 に固定）。第 3 節の直接依存テーブルは手書きなので、`scripts/check-license-table.py` が `Cargo.toml` と突き合わせる（同じく CI で実行）。依存の追加・削除・要求バージョン変更時は `about.hbs` のテーブルも直すこと。
