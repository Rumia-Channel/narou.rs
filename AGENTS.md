# narou.rs — Rust Port of narou.rb

## Overview
narou.rb（Ruby製の日本のWeb小説管理・電子書籍変換ソフトウェア）の互換実装をRustで作るプロジェクト。CLI・Web UI・変換出力など、外部から観測できる挙動の narou.rb 互換を目指す。

## 実装状況
`COMMANDS.md` が narou.rb 全24コマンドと Rust 拡張 (`db`/`illust`/`login`) 計27コマンドのオプション・挙動・実装状況を管理するマスタードキュメントである。最新の実装状況はそこを参照すること。

| 完了度 | コマンド数 | 内訳 |
|:------:|:---------:|------|
| ✅ 完了 | 23 | init, list, tag, freeze, remove, setting, diff, send, mail, backup, clean, illust, help, version, log, folder, browser, alias, inspect, csv, trace, db, login |
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
- pest 文法 (`src/downloader/preprocess.pest`) は以下の構文に対応: `guard`/`let`/`set`/`if`/`else`/`for`/`emit`/`insert_at_match`, 文字列補間 `${...}` (式を書ける), 正規表現 JSON 抽出 `extract_json(/.../)`, 追加取得 `request("...")` / `fetch_json("...")` と結果参照 `fetched["<url>"]`, メソッドチェイン `.map`/`.flat_map`/`.flatten`/`.compact`/`.join`/`.gsub`/`.replace`/`.is_array`/`.empty`/`.size`/`.first`/`.last`/`.reverse`, マッチ単位の置換 `.gsub(/re/) { |m| ... }` (`m` は `[全体, グループ1, ...]`), 添字アクセス `arr[0]`/`hash["key"]` (添字は式), 整数リテラルと `+`/`-` (数値文字列は自動変換、それ以外は null), 論理演算 `&&`/`||`/`!`/`==`/`!=`。実行時に step budget / 文字列サイズ上限 / 配列要素数上限による防御あり。
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
- `~/.narousetting/global_setting.yaml` は**ライブラリ状態ではない**ため、storage-backend が `sqlite` のときもファイルのまま維持する。`SQLITE_MANAGED_NAMES` から `global_setting` を外してあり、SQLite への取込・退避 (`*.imported-*`) は行わない。
- 理由: narou.rb はこのファイルしか読まないため、退避すると narou.rb 側で `aozoraepub3dir` 等が消える。また SQLite 側の実体は「そのライブラリの `.narou/db.sqlite`」なので、別ライブラリ (YAML モード) からは設定が見えなくなる。
- 旧ビルドが `app_state(scope='global', key='global_setting')` に残した行は、ファイルが無いときに初回読み出しでファイルへ書き戻し、その行を削除する (一度きりの復旧)。`tests/global_settings_storage.rs` がモード往復と復旧を固定している。
- ローカル側 (`local_setting` / `freeze` / `alias` / `tag_colors` / `latest_convert` / `login_cookie`) は SQLite モード時も管理対象のまま (YAML モードでは従来どおりファイル)。`narou-compat` の挙動も変更なし。

## 設定データの I/O 境界
- `local_setting` / `global_setting` の本番コードからの読み書きは `src/db/settings.rs`（`load` / `save` / `update` / `value` 等）を共通入口とする。CLI・Web・converter・downloader・logger・init・self-update から設定 YAML を直接 `fs::read_to_string` / `fs::write` で操作しない。
- 共通入口の下では既存 `Inventory` が保存方式（SQLite `app_state` と legacy YAML）を選択する。`native::application::NativeSettingsStore` も同じ共通入口に委譲する。Worker 側は従来の `SettingsStore` port / D1 adapter を利用する。
- SQLite migration / compat 判定等のストレージ実装内部、`webnovel/*.yaml` のようなユーザー編集可能なサイト定義、`setting.ini` 等の小説固有入力は別用途なのでこの禁止の対象外とする。設定保存時は必要に応じて `update` で同時更新による上書きを防ぐ。
- 保存元と読み出し先の不一致を防ぐため、SQLite 有効時に `setting` で保存した `default.*` / `force.*` が converter に反映されることを回帰テストで確認する。

## ログインが必要なサイト (フォールバック方式)
- 本体 (`narou_rs`) はログイン処理そのものを持たない。担うのは (1) ログインが必要かの判定、(2) どの小説の取得にログインが必要かの区別、(3) ログイン済み Cookie の更新の 3 点だけ。
- 通常は Cookie を送らない。取得に失敗したときだけ、保存済みのログイン Cookie を付けて 1 回再試行する。`404`（小説が消えた）またはサイト定義の `login_pattern` に一致するログイン壁が対象で、それ以外のエラーは従来どおり失敗させる。
- 再試行で取得できた小説はレコードの `requires_login`（SQLite `novels.requires_login` / `*.yaml` の `requires_login: true`）を立て、次回から最初のリクエストで Cookie を送る。オプション無しの小説は Cookie を一切送らないため、ログイン不要な小説の挙動は従来と変わらない。
- 再試行しても取得できない場合は従来どおり 404 判定（`frozen` / `404` タグ + `freeze.yaml`）へ進む。`requires_login` が立っている小説は「Cookie 付きで取得 → 失敗なら凍結」の順になる。
- サイト固有の値は `webnovel/*.yaml` に置く。追加キーは `login_url`（ログイン用 bin が開く URL、`\k<domain>` 補間あり）と `login_pattern`（HTTP 200 で返るログイン壁を検出する正規表現）。本体にサイト名・ドメイン固有の分岐は置かない。
- Cookie は `Inventory` の `login_cookie`（SQLite `app_state` / `.narou/login_cookie.yaml`）にホスト単位で保存する。応答の `Set-Cookie` は、既に保存済みのホストに限り `src/native/http.rs` が書き戻してセッションを維持する（保存していないホストには新規エントリを作らない）。
- ログイン実行は別 bin `narou_rs_login`（`src/bin/login.rs`）が担当する。Chromium 系ブラウザを `--remote-debugging-port` 付きで起動し、DevTools protocol (`Storage.getCookies`) で Cookie を取得する（`ws://` のみなのでブラウザ自動化依存を追加しない）。2 段階認証や CAPTCHA は実ブラウザ操作なのでそのまま通る。ブラウザが無い環境向けに `--cookie "<Cookie 文字列>"` の貼り付け保存、`--list` / `--clear` も用意する。
- **別端末・サーバーへの持ち込み**: `narou_rs_login` はブラウザのある端末で動かし、`--export <file>` でポータブルな書き出しファイル (YAML) を作る。`--passphrase` 指定時は Argon2id → XChaCha20-Poly1305 で暗号化される。ライブラリ外では書き出しが既定の出力になる。取り込み側は `narou login import <file>`（CLI）または Web UI 設定の「ログイン」タブで受け付ける。
- **暗号化保存**: 保存値は `.narou/login.key`（または `NAROU_RS_LOGIN_KEY`）の鍵で `enc:v1:<nonce>:<payload>` として暗号化され、ホスト名を AEAD の associated data に束ねるため別ホストへの流用はできない。旧形式の平文値は読み取り可能で、次回保存時に暗号化される。
- **セッション ID と小説の対応**: 保存した資格情報には UUID を振り (`LoginCredential.id`)、小説レコードは「どのセッションで成功したか」を `login_session` に持つ (SQLite `novels.login_session` / `*.yaml` の `login_session:`)。`requires_login` は「Cookie が要る」、`login_session` は「どれを使うか」を表す。フラグ付きの小説は次回以降、その ID の資格情報を最初のリクエストから送るので、一覧を毎回総当たりしない。ID が無い旧データはストア読み込み時に採番して書き戻す (小説側が覚える値なので不変)。採用した資格情報が消えていた場合は先頭にフォールバックし、次の成功で ID を書き直す。
- **複数ログインと試行順**: 1 ホストにつき資格情報を順序付きリスト（`LoginCredential`）として保存し、一覧の順に試す。ログインが必要なページは取得に成功した時点で、部分的な一覧は「欠けが解消した／話数が増えた」時点で試行を終える。採用した資格情報はその後の本文取得にも使う。保存形式は JSON 配列で、旧形式（host → Cookie 文字列）は 1 件として読み、次の書き込み時に移行する。`Set-Cookie` の書き戻しは、その応答で実際に送った資格情報だけを更新する（同じサイトの別アカウントのセッションを壊さないため）。
- **CLI / Web**: `narou login list`（値は伏せて表示）/`set`（置き換え）/`add`（末尾に追加）/`order <host> 2,1,3`（並べ替え）/`clear --index N`（1 件削除）/`import` / `export` を備える。Web UI の設定ページ「ログイン」タブも同じ操作（追加・置き換え・1 件削除・サイト削除・上下ボタンでの並べ替え・取り込み）ができる。API は `GET/DELETE /api/login`、`POST /api/login/set|add|order|import`、`DELETE /api/login/{host}`、`DELETE /api/login/{host}/{index}`。
- **書き出し形式**: `narou_login_export.yaml` は version 2（`credentials:` に順序付きリスト）。version 1（`cookies:` の host → Cookie 文字列）も読み込める。
- **Cookie の取得範囲**: サイト自身・親ドメイン・兄弟サブドメインを含むドメイン群の Cookie を保存する。Pixiv のように `.pixiv.net` にセッションを置くサイトでは、`www.pixiv.net` だけを見ると取り落とす。親ドメインの Cookie はサブドメイン宛のリクエストにも `CookieStore::load` がマージして送り、`Set-Cookie` も同じキーへ書き戻す。
- **ブラウザプロファイル**: `narou_rs_login` はサイトごとの固定プロファイル（`%TEMP%/narou-rs-login/<domain>`、`--profile` で変更可）を使い回すため、次回以降もログイン状態が残る。Cookie 取得後は対象 URL に一度アクセスし、サイト定義の `error_message` / `login_pattern` に一致すればログインできていない可能性を警告する。
- 配布物: `narou_rs_login` もリリース zip に同梱する（`scripts/package-release.ps1` の `-LoginBinaryPath`、`.github/workflows/release.yml` の helper build / sign / package、`cargo local-build` のすべてに対応済み）。Windows では他のサブ実行ファイルと同じく署名対象に含める。`scripts/package-release.ps1` は `-Platform win` のとき本体・updater・backup・login の Authenticode 署名を梱包前に検証し、未署名なら失敗する（`-SkipSignatureCheck` は署名できないローカル確認専用で、リリース CI からは指定しない）。

## Git 運用ルール
- 通常の修正・軽微な機能追加・ドキュメント更新は `develop` 上で行う。作業開始前に現在ブランチと作業ツリーを確認し、`main` 上で直接作業しない。
- 作業開始時に対象ブランチが `origin` より遅れている場合は、`git pull` で最新へ追従してから作業を始める。
- 大幅な変更、新機能、複数ファイルにまたがる設計変更、長時間かかる検証を伴う作業は、`develop` から機能単位のブランチを作成して進める。
- 機能ブランチ名は内容が分かる短い英数字・ハイフン形式にする。例: `fix-web-concurrency`, `feature-series-url`。
- 機能ブランチでは適切な動作テストを済ませてから `develop` に統合する。統合後も `develop` 上で必要なテストを再実行する。
- `main` への統合は、ユーザーが明示的に依頼した場合、またはリリース作業として明確に合意された場合だけ行う。`develop` は削除せず残す。
- `main` へ統合する前に、`develop` が clean であること、必要なテストが通っていること、バージョン更新や README 更新などリリースに必要な差分が揃っていることを確認する。
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
- 有効な AozoraEpub3 パスを設定した場合は、Ruby 版と同じく `chuki_tag.txt` のカスタム注記追記/置換、`AozoraEpub3.ini` のコピー、`template/OPS/css_custom/vertical_font.css` の行高反映コピーを行う。

## Build & Run
```powershell
cargo build              # Build (edition 2024)
cargo run -- convert 2  # カクヨム小説を変換（CWD: sample/novel/）
cargo run -- convert 1  # なろう小説を変換
cargo check              # Type-check
```

**重要**: `cargo run` は `.narou/` を持つ初期化済みライブラリをCWDとして実行する必要がある（例: `sample/novel/`、gitignore 済みのローカル用ディレクトリ）。

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
      mod.rs                       - SiteSetting struct, accessor methods, compile, load_all, tests
      interpolate.rs               - \k<name> テンプレートエンジン
      info_extraction.rs           - resolve_info_pattern, multi_match, get_novel_type_from_string
      loader.rs                    - load_all_from_dirs, load_settings_from_dir, merge_site_setting
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
- P4a コンテンツミラー (novel_sections/novel_outputs) — convert時に書込み、Web DL時EPUBはDB優先
- P4b バージョン履歴 (novel_versions/_sections/_diffs) + `narou diff --history|--show|--restore|--merge-from`。update時自動snapshotはconvertフック経由
- P4c オブジェクト格納: `objects`/`object_chunks` (BLOB + brotli + crc32) に小説データ・生成物を格納。native は FS ミラー + 読みフォールバック、worker は D1 のみ
- 後方互換: 旧ライブラリからの自動取込と `narou db export-yaml` によるロールバックを保証。`narou setting narou-compat=true` で `.narou/*.yaml` を維持する前方互換モードあり (既定 OFF)。`export-yaml --in-place` は実位置へ書き戻して YAML モードへ復帰する

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
- `worker_entry/` は `workers-rs 0.8.5` の `fetch` / `scheduled` / `queue` eventを公開する。`composition.rs` は D1 novel/freeze/settings/tag-color adapters と D1 `ObjectStore`(`objects`/`object_chunks` BLOB+brotli+crc32 テーブル) を構成し、Worker固有型をcoreへ逆流させない。
- `WorkerHttpClient` は Fetch APIを既存 `HttpClient` traitへ接続し、request/response body上限を強制する。`D1ObjectStore` は logical-key prefix、paged LIST、bounded small read/write、streaming AssetStore を D1 上に実装する。
- `D1NovelRepository` はprepared statements/migrationsでtyped filter/sort、keyset `scan_ids`、atomic sequence allocation、batch mutationをSQL化する。settings、freeze、tag colorsもD1 state/tableへ接続する。
- `/health/live`、`/health/ready`、認証付きread-only `/api/novels`/`/api/novels/:id`を公開する。`NAROU_ADMIN_TOKEN`はconstant-time比較。未知queue envelopeはledgerへ記録して安全にackする (retryしない)。Queue実ジョブ実行 (D1 ledger・bounded retry・checkpoint resume) は Phase 8 で実装済み。
- Worker production readinessはD1 `DB` bindingと `NAROU_ADMIN_TOKEN` secretを要求する。秘密値はリポジトリへ置かない。

### 最近の追加 (2026-05〜09)
- **update の並列ダウンロード** (E): `update.max-parallel-domains` 設定（既定 4）で対象小説をサイトドメイン別にグルーピングし、ドメインごとにワーカースレッドを割り当てて並列ダウンロード。同一ドメイン内は常に直列を維持するため対サイト礼儀は崩れない。1 で従来の逐次動作、フォース指定・ウェブモード・ドメインが1種類のときは自動的に逐次にフォールバック
- **ジョブ自動リトライ** (B): queue worker に exponential backoff 付き自動リトライ（`queue.retry-backoff` 既定 `1m,5m,15m`）を実装
- **挿絵メンテナンスコマンド** (A): `narou illust <sub>`（`orphan` / `migrate` / `fix-ext` / `rebuild`）を新設し、`.illustration_cache.yaml` 運用の保守ヘルパーを CLI から呼び出せるようにした。削除・改名・移行系はいずれも既定 dry-run
- **ブックマークレット登録フローの same-origin 復活** (H): Web UI から拡張ブックマークレットを登録する際の手続きを同一オリジン経由に戻し、外部リダイレクトを排除
- **reverse proxy Host 許可リスト拡張** (I-3): `server-add-accepted-hosts` 設定で許可 Host を後から追加できるようにし、リバースプロキシ越しのアクセス制御を強化
- **list / update の型付きソート** (I-1): ソートキーを型付きで実装し、`new_arrivals_date` / `general_lastup` など拡張キーをバリデーション込みで受理
- **self-update の Unix デタッチ** (D): Linux/macOS で self-update 中も本体が生存できるよう、updater を `setsid` で切り離して起動
- **self-update の variant 選択** (2026-09): GPL版(Lite組込み)/通常版 の選択は global 設定 `self-update.variant` (`gpl` / `standard`) が唯一の保存先で、Web UI の環境設定 Global タブのセレクトと `narou setting self-update.variant=...` の両方から設定でき、`narou setting` の一覧にも表示される。0.4.0 以下からの更新時は未設定なら Web UI が選択モーダルを表示して保存し、設定済みならモーダルを出さず保存値で更新する。更新は 保存値 → ビルド variant の順で解決し (リクエストの明示指定が最優先)、GPL版は `narou_rs_*-GPL.zip` を取得する。未設定へ戻すと実行中のビルドと同じ variant になる
- **ruby タグ除去** (I-4): サブタイトルとファイル名からルビ注記（`《…》` 形式）を除去し、Ruby版と表示を揃える
- **小説単位の queue lane 跨ぎ exclusion** (C): 同じ小説が primary / secondary lane の両方で同時に走らないよう、novel 単位の排他を queue worker に追加

### 変換互換性
- **なろう**: narou.rb参照データと完全互換確認済み
- **カクヨム (ID=1177354055617350769)**: **完全互換達成** — 行数完全一致 (25,273/25,273)、行単位 diff 0件。`cargo test` の `tests/convert_parity.rs` で byte-for-byte fixture テスト通過
- ※米印変換、全角数字、ルビ、auto_join_line、各種文字変換も完全一致

### AozoraEpub3_Lite 組み込みエンジン (lite feature, 2026-09)
- pin: `aozora_epub3_lite` = `cd67ddb` (v0.1.4)。更新時は `Cargo.toml` の `rev` を書き換えて `cargo update -p aozora_epub3_lite`。
- 組み立ては Lite CLI (`main.rs::convert_input`) と同じ公開 API を使う。独自実装 (挿絵の連番化・外字フォント収集・UUID 生成) は持たない。
  - `config_for(aozoraepub3dir)` = `AozoraConfig::load_from_dirs([dir], <dir>/AozoraEpub3.ini)`。Java 版と同じ注記表・外字フォント・INI を読む。INI が無ければ `preset/AozoraEpub3.ini` 相当のフラグ。
  - `build_book(input_txt, options)`: `collect_assets` → `decorate_image_tags` → `rewrite_image_source` → `remove_missing_image_sources` → `remove_image_sources` (自動表紙) → `reflow_image_sections` → `build_metadata` (`urn:uuid:` は Java と同じ `java_name_uuid`) → `build_title_page_markup` → `append_gaiji_assets`。
  - 挿絵は `EpubBuild::resolve` が書き出し時に 1 枚ずつ読み、`image::process` (余白除去・リサイズ・回転) をかける。寸法だけ事前に読む。
- narou カスタム注記 (`preset/custom_chuki_tag.txt`, 21 行) は `include_str!` で常に重ねる。`init` がインストール先 `chuki_tag.txt` に書き込む内容と同一なので、同梱資産だけで動く wasm / 未設定時でも `ここから柱` / 前書き / 後書き / 一字下げ 等が効く。
- `preset/AozoraEpub3.ini` も `include_str!` し、`aozoraepub3dir` が無いときの既定にする (`IniSettings::parse` → `AozoraConfig::from_ini`)。Java 版は常にこの INI (init がインストール先へコピーしたもの) を読むため、外部 AozoraEpub3 が無い環境でも `TitlePage` / `CoverPage` / `SpaceHyphenation` / `DakutenType` などのフラグが一致する。実測: 資産なしでも Java と 419/423 バイト一致。
- 同梱 `replace.txt` は読み込まない (削除済み)。Java は narou.rb 構成では `replace.txt` を持たない (配布物は `replace_sample.txt`) ため、読み込むと `－`→`―` など不要な文字置換が入り Java とずれる。
- 残差: 外字フォント (`gaiji/dakuten/*.ttf`) は `aozoraepub3dir` が無いと格納できない。`AozoraConfig::gaiji_fonts` がパス指定のため、同梱資産 (バイト列) からは渡せない。Java は濁点外字に `<span class="glyph u30fc-u309a">` を出すが Lite は素の文字になる (この差は外字を使う小説でのみ発生)。
- **フォント (濁点フォント / 本文フォント)**: 外部ツールには `DakutenFontGuard` が `aozoraepub3dir/template/OPS/fonts/DMincho.ttf` と `css_custom/vertical_font.css` を流し込んで効かせる。組み込み (Lite) にはその経路が無いので、narou は同じ内容を `EpubBuildOptions::extra_assets` の `style/vertical_font.css` + `fonts/DMincho.ttf` として渡し、crate 側が本文 (`item/xhtml/*.xhtml`) からその CSS をリンクする (組み込み CSS より後ろに置くので同じ詳細度ならこちらが勝つ)。設定 `convert.epub-font` (`auto` / `always`) で選び、`always` は本文全体を `DakutenAokinMincho` で組む — U+3000 (全角スペース) を描けない Reader (超縦書き など) 向けの回避策で、`auto` は従来どおり濁点注記のある小説だけ。
- **注意**: `aozoraepub3dir` を設定すると CLI は外部ツール (jar / Lite exe) を優先する (narou.rb と同じ)。組み込みエンジンを強制する設定は持たない。
- 実データ検証 (2026-09-15, v0.1.3): `WebNovel` の n0421du (401 セクション) で Java 版と **422/423 ファイルがバイト完全一致**、挿絵入りでも **425/426 がバイト完全一致**（単ページ画像化・連番・表紙処理を含む）。残差は `dcterms:modified` のみ（Java はローカル時刻に `Z`、Lite は UTC。Lite 側の意図的な非再現）。
- 検証手順は `docs/aozora_lite_evaluation_2026-08-23.md` の「更新 (2026-09-15)」節。

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
- ログインしていないと R18 作品は一覧から**黙って除かれる** (404 にならないので再試行も走らない)。R18 を含むシリーズは `narou_rs_login` で先に Cookie を保存しておくこと。
- 分岐の注意: `illustType` はイラストで 0 になり、DSL では数値 0 が偽になる。作品ページ判定は `illustTitle` の有無で行う。
- **アクセス間隔**: サイト定義の `min_interval` (秒) がそのサイトへのリクエスト間隔の下限になる (全体設定 `download.interval` より優先)。`RateLimitScope` がサイト定義の値を持ち、native limiter は `max(download.interval, min_interval)` で待つ。Pixiv は短時間の連続アクセスで 429 を返すため `min_interval: 5` を置いている (運用要件は最低 2 秒・できれば 5 秒)。
- **Pixiv の欠けの見分け方**: 匿名でも `/ajax/novel/series_content/{id}` の `page.seriesContents` には全話の id と話数が入っており、中身を伏せられている話だけ `series.viewableType` が 0 以外になる (ログイン時は全部 0)。見える話だけを持つ `thumbnails.novel` との差が「空のデータ + 実データ」の実データ側なので、これを欠けの判定に使う (0 件 / 1 話目から始まらない、は viewableType が無い応答向けの保険)。漫画シリーズの `/ajax/series/{id}` は匿名だと R-18 の id 自体を落とすため、そちらは 1 ページ目の最新 order と `series.total` で判定する。
- **一覧が欠けたまま成功する応答**: サイト定義に `login_partial_pattern` を置き、DSL が `login_partial::1` を emit すると「未ログインで一部の作品が落ちている」とみなす。保存済み Cookie で 1 回だけ再取得し、**欠けが解消したときだけ**採用する (解消しなければ匿名の結果をそのまま使うので 404 化しない)。Pixiv の漫画シリーズは未ログインだと R-18 が落ちて 16/21 件しか返らないため、1 ページ目の最新 order と `series.total` を比べてこの目印を出す。小説シリーズは本文一覧のページが未ログインで 0 件になるので、そこでも目印を出す (目次は複数ページに分かれるため、検知は `parse_subtitles_multipage` が全ページを見て行い、話数が増えたときだけ Cookie 付きの結果を採用する)。
- **うごイラ**: `illustType == 2` の作品はフレーム集約 zip と各フレームの表示時間 (`/ajax/illust/{id}/ugoira_meta`) で配られる。DSL が `zip の URL + "?ugoira=" + 遅延ms のカンマ区切り` を `<img src>` として出し、`src/illustration_animation.rs` が APNG に組み立てる (ブリッジの Pillow 実装と同じ出力形式)。フレームは JPEG/PNG をデコードするので `image` クレート (jpeg, png のみ) を使う。両クレートは `illustration-animation` feature (`dep:image` + `dep:zip`) にまとめてあり、native では常時 ON、Worker/wasm では入れない (wasm には zip 8 の圧縮コーデックが載らない) ため `assemble_animation` は未対応エラーを返し、呼び出し側は落としたアーカイブをそのまま保持する。全フレームを RGBA で揃える (半透明を含む作品があるため、サイズより忠実さを優先)。単一フレームなら通常の PNG、遅延やアーカイブが無ければ呼び出し側の通常経路 (静止画) にフォールバックする。実機確認: `a69642452` (19 フレーム, 1077x690, 21.3MB の APNG、Chromium がフレーム 0 を正しく描画)。
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

### コマンド実装状況 (詳細は `COMMANDS.md`)
- ✅ 完了 (23): init, list, tag, freeze, remove, setting, diff, send, mail, backup, clean, illust, help, version, log, folder, browser, alias, inspect, csv, trace, db, login
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
