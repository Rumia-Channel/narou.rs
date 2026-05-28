# narou.rs — Rust Port of narou.rb

## Overview
narou.rb（Ruby製の日本のWeb小説管理・電子書籍変換ソフトウェア）のサーバー実行部分をRustに移植するプロジェクト。なろう・カクヨム等のサイトからのDL・変換が動作し、narou.rbの出力フォーマットと完全互換性を持つことを目指す。

## 実装状況
`COMMANDS.md` が narou.rb 全24コマンドのオプション・挙動とRust側実装状況を管理するマスタードキュメントである。最新の実装状況はそこを参照すること。

| 完了度 | コマンド数 | 内訳 |
|:------:|:---------:|------|
| ✅ 完了 | 18 | init, list, tag, freeze, remove, setting, diff, send, backup, clean, help, version, log, folder, browser, alias, inspect, csv, trace |
| 🟡 部分 | 4 | download, update, convert, web |
| 🟡 部分 | 1 | mail |
| ❌ 未実装 | 1 | — (全コマンド実装済み) |

## Porting Policy
- このプログラムは `sample/narou` にある本家 narou.rb を Rust へ移行するための互換実装である。
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
- 2026-05 時点: ハードコードされた `kakuyomu_preprocess` は完全に除去され、`webnovel/kakuyomu.jp.yaml` の `preprocess:` DSL ブロックへ移行済み。pest 文法ベースの安全な DSL パーサー (`src/downloader/preprocess.pest`) + インタプリタ (`src/downloader/preprocess/interpreter.rs`) により、YAML 記述だけでカクヨム JSON → 中間テキストの展開が可能である。ユーザー側 YAML の `preprocess:` を編集するだけで前処理ロジックを差し替えられる。
- pest 文法 (`src/downloader/preprocess.pest`) は以下の構文に対応: `guard`/`let`/`set`/`if`/`else`/`for`/`emit`/`insert_at_match`, 文字列補間 `${...}`, 正規表現 JSON 抽出 `extract_json(/.../)`, メソッドチェイン `.map`/`.flat_map`/`.flatten`/`.compact`/`.join`/`.gsub`/`.replace`/`.is_array`/`.empty`, 論理演算 `&&`/`||`/`!`/`==`/`!=`。実行時に step budget / 文字列サイズ上限 / 配列要素数上限による防御あり。
- 新しいサイト対応やサイト構造変更対応では、まず YAML 表現で解決できるかを検討する。やむを得ず Rust に暫定処理を置く場合は、暫定であること、対応する YAML 意味論、将来 YAML 駆動へ戻す作業を `AGENTS.md` または Serena メモに明記する。
- Arcadia (`webnovel/www.mai-net.net.yaml`) に `encoding: UTF-8` は置かない。narou.rb の同梱 Arcadia 定義には無く、Rust 側は UTF-8 を既定として扱えばよい。Arcadia の本文取得不具合の実原因は `href` の `&amp;` を未デコードのまま section URL に使っていたことであり、`build_section_url()` 側で HTML エンティティを復元する。

## COMMANDS.md 同期ルール
- `COMMANDS.md` は narou.rb 全24コマンドのオプション・挙動と Rust 側実装状況を管理するマスタードキュメントである。
- **コマンドの新規実装・オプション追加・フラグ追加・挙動変更を行うたびに、必ず `COMMANDS.md` の該当箇所をリアルタイムに更新する。**
- 更新内容: Rust 列の ✅/🟡/❌ マーク、実装状況サマリの完了度、不足動作リストの削除・追加。
- 実装が完了したコマンドは「部分」→「完了」に昇格させる。
- 全24コマンドが narou.rb と完全互換になるまで、この同期作業を継続する。
- Serena メモリにも常に最新の実装状況を反映する。
- **完了判定の注意**: `COMMANDS.md` の ✅ 完了は、Rust 側に該当処理や help 表示が存在するだけでは付けない。必ず Ruby 版 `sample/narou/lib/command/*.rb` と、CLI オプション、help 文、Examples、設定項目、終了コード、エラー文、未実装の周辺動作を細かく突き合わせ、外部から観測できる挙動が一致していることを確認してから完了にする。
- 特に `help` は未実装コマンド分も narou.rb から移植する方針のため、Rust 側の実装済みコマンド集合と比較して完了判定しない。`narou <command> -h` の詳細文、Options、Configuration、Variable List、Examples を Ruby 版の各 command ファイルと比較して判断する。
- 既に ✅ と書かれているコマンドでも、同じ節に「未実装」「不足動作」が残っている場合や Ruby 版 help/挙動との差分がある場合は、実態に合わせて 🟡 部分へ戻す。完了度は楽観的に維持せず、互換性確認の粒度を優先する。

## コミット時のコード整形禁止ルール
- git diff に現れる変更は、機能的な意味を持つものだけにすること。
- コードの見た目だけを変える無意味な変更を禁止する。具体的には以下:
  - 既存の一行を複数行に改行+インデントし直すだけの変更
  - 既存の複数行を一行にまとめ直すだけの変更
  - `use` / `import` の順番を入れ替えるだけの変更
- これらの整形変更は、機能変更に付随して不可避な場合（例: 引数追加で行長が変わる）のみ許容する。

## Git 運用ルール
- 通常の修正・軽微な機能追加・ドキュメント更新は `develop` 上で行う。作業開始前に現在ブランチと作業ツリーを確認し、`main` 上で直接作業しない。
- 作業開始時に対象ブランチが `origin` より遅れている場合は、`git pull` で最新へ追従してから作業を始める。
- 大幅な変更、新機能、複数ファイルにまたがる設計変更、長時間かかる検証を伴う作業は、`develop` から機能単位のブランチを作成して進める。
- 機能ブランチ名は内容が分かる短い英数字・ハイフン形式にする。例: `fix-web-concurrency`, `feature-series-url`。
- 機能ブランチでは適切な動作テストを済ませてから `develop` に統合する。統合後も `develop` 上で必要なテストを再実行する。
- `main` への統合は、ユーザーが明示的に依頼した場合、またはリリース作業として明確に合意された場合だけ行う。`develop` は削除せず残す。
- `main` へ統合する前に、`develop` が clean であること、必要なテストが通っていること、バージョン更新や README 更新などリリースに必要な差分が揃っていることを確認する。
- タグ作成はユーザーがバージョン番号を明示した場合だけ行う。タグは `main` のリリースコミットを指すようにし、作成後に push する。
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

**重要**: `cargo run` は `sample/novel/` をCWDとして実行する必要がある（`.narou/` ディレクトリが必要なため）。

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
  lib.rs                           - クレートルート (pub mod定義)
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
    mod.rs                         - Downloader struct (DL pipeline orchestrator, 2497行)
    types.rs                       - SectionElement, SectionFile, TocObject, DownloadResult 等
    fetch.rs                       - HttpFetcher (3-tier: curl crate → reqwest → wget fallback)
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
    assets/                        - 静的アセット (CSS, JS)
sample/
  novel/                           - テスト用CWD (.narou/ + webnovel/*.yaml)
  narou/                           - Ruby参照ソース (git submodule的な位置, .gitignore)
  1177354055617350769 .../         - カクヨム参照データ (narou.rb出力, 25,273行)
```

## Reference Files (Ruby, 読取専用)
- `sample/narou/lib/converterbase.rb` — テキスト変換エンジン (1503行) — **最も重要な参照**
- `sample/narou/lib/novelconverter.rb` — コンバーター全体オーケストレータ (1209行)
- `sample/narou/lib/html.rb` — HTML→青空変換 (124行) — Rustの `html.rs` はこれに準拠
- `sample/narou/template/novel.txt.erb` — 最終テキスト組み立てERBテンプレート (93行)
- `sample/narou/lib/novelsetting.rb` — 設定定義
- `sample/narou/lib/command/*.rb` — 各コマンド実装 (help/CLI挙動の参照元)

## Current Status (2026-05)

### 変換互換性
- **なろう**: narou.rb参照データと完全互換確認済み
- **カクヨム (ID=1177354055617350769)**: **完全互換達成** — 行数完全一致 (25,273/25,273)、行単位 diff 0件。`cargo test` の `tests/convert_parity.rs` で byte-for-byte fixture テスト通過
- ※米印変換、全角数字、ルビ、auto_join_line、各種文字変換も完全一致

### ダウンロード互換性
- なろう (n8858hb, 24セクション) DL完走確認済み
- カクヨム (ID=2, 294セクション) DL完走確認済み
- syosetu.org（ハーメルン）: UAランダム化、HTTP/1.1/Cookie/圧縮/curl fallback による403回避対応済み。フルDL未検証
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
- ✅ 完了 (18): init, list, tag, freeze, remove, setting, diff, send, backup, clean, help, version, log, folder, browser, alias, inspect, csv, trace
- 🟡 部分 (5): download, update, convert, web, mail
- ❌ 未実装 (0): 全コマンド何らかの実装あり

## 未解決の既知課題

### 2026-04: WEB UI の自動更新ボタンが出ない件
- 現象: v0.1.32 で `latest_version != current_version` にも関わらず `update_available: false`
- 該当コード: `src/web/misc.rs::version_latest`
- 仮説: 不可視文字混入、キャッシュ不整合、v0.1.32 固有のコードバグ
- 再現環境は喪失 (ユーザー側アップデート済み、2026-04-26)
- `841bec5` で `NAROU_RS_RELEASE_BUILD` フラグ焼き込み済み
- 対応指針: `version_latest` 防御的書き直し、JS 側フォールバック判定、生バイト列検査

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
- **HTTP client**: reqwest (blocking, cookies, gzip/brotli/deflate) + curl crate
- **CLI**: clap 4
- **Date/time**: chrono + chrono-tz
- **Regex**: regex
- **Hashing**: sha2 + hex
- **Error handling**: thiserror
- **Template**: askama
- **Logging**: tracing + tracing-subscriber
- **Sync**: parking_lot, dashmap, tokio::sync
- **Browser open**: open
- **WebSocket**: tokio-tungstenite
- **HTTP client (low-level)**: curl crate
- **Random UA**: ua_generator
