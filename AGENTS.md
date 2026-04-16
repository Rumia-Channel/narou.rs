# narou.rs — Rust Port of narou.rb

## Overview
narou.rb（Ruby製の日本のWeb小説管理・電子書籍変換ソフトウェア）のサーバー実行部分をRustに移植するプロジェクト。なろう・カクヨム等のサイトからのDL・変換が動作し、narou.rbの出力フォーマットと完全互換性を持つことを目指す。

## Porting Policy
- このプログラムは `sample/narou` にある本家 narou.rb を Rust へ移行するための互換実装である。
- 内部ライブラリ、データ構造、処理系統、実装アルゴリズムは Ruby 版と同一である必要はない。Rust 側で保守しやすく、安全で、検証しやすい構成を優先してよい。
- 互換性の主対象は外部から観測できる挙動である。特に CLI/API の引数・戻り値・エラー挙動、`webnovel/*.yaml` や `converter.yaml` などの YAML 構文理解、`.narou/` 配下のデータ読み書き、最終的なファイル出力を narou.rb と徹底的に合わせる。
- Ruby 実装は仕様の参照元として扱う。処理手順をそのまま写すことよりも、同じ入力から同じ外部挙動・同じ出力を得ることを優先する。
- 互換性調査では Ruby 版の内部手順を読むが、それは外部仕様を抽出するためである。Rust 実装では、外部挙動・データ互換・出力互換を壊さない限り、Ruby の逐語的移植よりも堅牢性、保守性、検証容易性、性能、安全性が高い設計を選ぶ。

## YAML-Driven Site Definition Compatibility
- サイト別の取得・前処理・抽出ルールは narou.rb と同じく `webnovel/*.yaml` を主たる仕様として扱う。ユーザーが初期化フォルダ内の `webnovel/*.yaml` を編集・差し替えた場合、その内容で挙動を変えられることが互換性の重要要件である。
- Rust 側にサイト固有ロジックを直接ハードコードする実装は、最終的な互換方針としては不可。特に `code: eval:` や前処理相当の記述を YAML から切り離して Rust 関数へ固定すると、narou.rb の「YAML を更新すればサイト追従できる」という性質を壊す。
- 2026-04 時点の注意: `src/downloader/mod.rs` の `kakuyomu_preprocess` は、カクヨム JSON を `title::...` や `Episode;...` の中間テキストへ展開する Rust 側の暫定ハードコードであり、YAML 駆動互換としては未完成である。`tableOfContentsV2` 対応も応急的な互換パッチであって、正しい最終設計ではない。
- 引き継ぎ時の優先課題: `webnovel/kakuyomu.jp.yaml` にある `code: eval:` 相当を、Ruby 実行そのものに限定せず、Rust 内で安全に解釈できる YAML 定義の前処理モデルへ移すこと。最低限、同梱 YAML とユーザー側 YAML の記述変更だけでカクヨムの JSON 展開ロジックを差し替えられる状態にする。
- 新しいサイト対応やサイト構造変更対応では、まず YAML 表現で解決できるかを検討する。やむを得ず Rust に暫定処理を置く場合は、暫定であること、対応する YAML 意味論、将来 YAML 駆動へ戻す作業を `AGENTS.md` または Serena メモに明記する。
- 2026-04 時点の注意: Arcadia (`webnovel/www.mai-net.net.yaml`) に `encoding: UTF-8` は置かない。narou.rb の同梱 Arcadia 定義には無く、Rust 側は UTF-8 を既定として扱えばよい。Arcadia の本文取得不具合の実原因は `href` の `&amp;` を未デコードのまま section URL に使っていたことであり、`build_section_url()` 側で HTML エンティティを復元する。

## 互換性の要件レベル
- 外部から観測できる挙動の互換性は**妥協せず完璧に**追求する。これには以下が含まれる:
  - **設定ファイルの位置**: `.narou/local_setting.yaml`、`~/.narousetting/global_setting.yaml` など、Ruby 版と同一パスに配置する。
  - **設定ファイルの読み書き互換**: Rust が書いた YAML を Ruby が読め、Ruby が書いた YAML を Rust が読めること。`---` ヘッダの有無など形式の差は許容されるが、意味論（キー名・値の型・構造）は一致させる。
  - **全設定項目の読み書き**: Rust 側に未実装の機能（send、mail、device 変更自動調整等）の設定項目であっても、`narou setting` コマンドで読み取り・設定・削除が可能であること。`default.*`、`force.*`、`default_args.*` 系の動的変数名もすべて受け付けること。
  - **CLI の引数・戻り値・エラーメッセージ・終了コード**: Ruby 版と同一であること。
  - **`webnovel/*.yaml` や `.narou/` 配下のデータ構造**: Ruby 版が読める形式を維持すること。
  - **最終的な変換出力ファイル**: narou.rb の出力と同一であること。
- 「内部実装は異なってよい」方針は変更しない。上記の外部互換性を満たす限り、Rust 側のアルゴリズム・データ構造・処理順序は自由に選んでよい。Ruby 版に既知の脆さや古い都合がある場合は、同じ外部結果になることをテストやドキュメントで確認した上で、Rust 側ではより良い内部設計を採用する。

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
- 実装が一区切りついたら、**機能単位で git commit と GitHub push を行うこと。**
- 無関係な変更をひとつの commit に混ぜず、レビューやロールバックがしやすい粒度に分けること。

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
  main.rs                          - CLI entry point (thin dispatcher, ~70行)
  cli.rs                           - clap定義 (Cli struct + Commands enum)
  error.rs                         - NarouError enum + Result type
  commands/
    mod.rs                         - pub mod + resolve_target_to_id helper
    init.rs                        - narou init (ディレクトリ作成, AozoraEpub3設定)
    download.rs                    - narou download
    update.rs                      - narou update
    convert.rs                     - narou convert
    web.rs                         - narou web (Axumサーバー起動)
    manage.rs                      - narou list / tag / freeze / remove
  db/
    mod.rs                         - シングルトン (DATABASE static, init_database, with_database/mut)
    database.rs                    - Database struct (CRUD, sort, tag index)
    novel_record.rs                - NovelRecord struct
    inventory.rs                   - Inventory (LRU cache, atomic write, Windows retry)
    index_store.rs                 - IndexStore (SHA256 fingerprint)
    paths.rs                       - novel_dir_for_record, create_subdirectory_name
  downloader/
    mod.rs                         - Downloader struct (DL pipeline orchestrator)
    types.rs                       - SectionElement, SectionFile, TocObject, DownloadResult 等
    fetch.rs                       - HttpFetcher (3-tier: curl crate → reqwest → wget)
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
    novel_info.rs                  - NovelInfo (from_toc_source / from_novel_info_source)
    html.rs                        - to_aozora (HTML→青空文庫形式変換)
    info_cache.rs                  - 小説情報キャッシュ
    rate_limit.rs                  - RateLimiter
    preprocess.pest                - pest grammar file
  converter/
    mod.rs                         - NovelConverter struct, convert_novel pipeline, cache
    render.rs                      - render_novel_text (novel.txt.erb相当), ConvertedSection
    output.rs                      - create_output_text_path/filename, extract_domain/ncode_like
    ini.rs                         - IniData / IniValue (INI parser/serializer)
    settings.rs                    - NovelSettings (44 items, INI overlay, replace.txt)
    device.rs                      - OutputManager (端末別出力)
    converter_base/
      mod.rs                       - ConverterBase struct, TextType, convert pipeline orchestrator
      character_conversion.rs      - 半角/全角変換, 数字→漢数字, TCY
      indentation.rs               - auto_indent, half_indent_bracket, insert_separate_space
      stash_rebuild.rs             - illust/URL/kome stash & rebuild
      ruby.rs                      - narou_ruby, find_ruby_base (ルビ注記処理)
      text_normalization.rs        - rstrip, ellipsis, page_break, dust_char, blank_line 等
    user_converter/
      mod.rs                       - UserConverter struct, load, apply_before/after, signature
      setting_override.rs          - apply_setting_override (converter.yaml設定オーバーライド)
  web/
    mod.rs                         - AppState, create_router (Axumルーター定義)
    state.rs                       - ApiResponse, IdPath, ListParams 等 (DTO structs)
    novels.rs                      - index, novels_count, api_list, get/remove/freeze/unfreeze
    tags.rs                        - add_tag, remove_tag, update_tags
    batch.rs                       - batch_tag/untag/freeze/unfreeze/remove
    jobs.rs                        - api_download/update/convert, queue_status/clear
    novel_settings.rs              - get_settings, save_settings, list_devices
    misc.rs                        - version_current, tag_list, notepad_read/save, recent_logs
    push.rs                        - PushServer, WebSocket, StreamingLogger
  queue.rs                         - PersistentQueue (SQLite-backed job queue)
  lib.rs                           - library root
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

## Current Status

### 完了
- DL動作確認: なろう(n8858hb, 24セクション), カクヨム(ID=2, 294セクション)
- Convert動作確認: なろう版はnarou.rb参照データと完全互換
- カクヨム版: 構造（大見出し/柱/改ページ/中見出し/end-of-book）は完全一致
- ※米印変換、全角数字も完全一致
- syosetu.org（ハーメルン）向けの403回避対応を追加: デフォルトUAを `ua_generator::ua::spoof_firefox_ua()` のランダム生成にし、reqwest/curl双方でwget相当のHTTP/1.1、Accept系ヘッダー、Cookie、圧縮対応を使う。
- syosetu.orgのタイトル/作者取得を修正: `novel_info_url` の取得も `Downloader` のUA/header/curl fallback経路を通す。`https://syosetu.org/?mode=ss_detail&nid=232822` の現行HTML断片からタイトル・作者・連載種別を抽出するテストを追加済み。

### 2026-04-11 syosetu.org DL対応メモ
- reqwestでUAだけを変えてもsyosetu.orgは403になるケースがある。wget/curlで通っていた差分は、UA以外にHTTP/1.1、Accept/Accept-Language/Accept-Encoding/Accept-Charset/Connection、Cookie保持、圧縮展開が揃っている点。
- `Downloader::with_user_agent(None)` と `--user-agent random` は `ua_generator` のFirefox系ランダムUAを使う。Chrome系UAはCloudflare challengeに寄るケースがあったため、現時点ではFirefox系を既定にしている。
- reqwest clientは `.cookie_store(true)`, `.http1_only()`, `.gzip(true)`, `.brotli(true)`, `.deflate(true)` を有効化している。デフォルトヘッダーの `Accept-Encoding` は `gzip, deflate, br`。
- 403時は `curl.exe`（非Windowsは `curl`）へfallbackする。curl側は `--http1.1 --compressed` と同じAccept系ヘッダーを指定する。ローカルのcurlがbrotli非対応の場合、`Accept-Encoding` は `gzip, deflate` に落とす。
- 一度curl fallbackが成功したら `prefer_curl = true` にし、以後のTOC/本文/novel_info取得を先にcurlで試す。reqwestで毎話403→curl retryになる遅延を避けるため。
- syosetu.orgの本文リンク `1.html` などは、YAMLの `href` テンプレートだけに頼ると `https://syosetu.org/novel//k<ncode>/1.html` のように壊れる。`build_section_url(setting, toc_url, href)` で目次URL基準の相対URLとして解決する。
- 注意: `https://syosetu.org/novel/232822/` の251話フルDL完走は未検証。情報ページ取得・タイトル/作者抽出・対象テスト・既存テストは確認済み。

### 未解決 (第16ラウンドの残課題)
**カクヨム版の行数が合わない**: 参照25,273行 vs 出力25,766行 (+493行)

#### 根本原因（ほぼ特定済み）
**`auto_indent` が `\n` を誤って `\u{3000}` に変換している:**
- `auto_indent` の regex `(?m)^([^{ignore_chars}])` が、body先頭の `\n` にマッチ
- `\n` が ignore_chars に含まれていないため、`\n` → `\u{3000}\n` に変換
- 各セクションのbodyに `\u{3000}\n` が1つ余分に付与され、全体で+493行

#### 修正方針（3つのアプローチ）
1. **A**: regexに `\n` を追加: `(?m)^([^{ignore_chars}\n])` — 空行にマッチしなくなる
2. **B**: auto_indent前に `data.strip_prefix('\n')` で先頭改行を除去
3. **C**: Rubyのline-by-line処理（`@write_fp.puts`）を模倣してauto_indent前にテキスト行処理

**Ruby版でなぜ起きないのか**: Rubyの `convert_main` は `@read_fp.each_with_index` で行ごとに処理し、`@write_fp.puts(line)` で出力する。空行は `puts("")` → `\n` として出力されるが、auto_indent実行時のテキスト構造がRust版と微妙に異なる可能性がある。詳細な調査が必要。

#### 確認済みの修正（第16ラウンド）
1. `auto_join_line` をRuby準拠に修正: `([^、])、\n　([^「『...])` のみ結合（旧: `。` 結合で全パラグラフをマージ）
2. `br_to_aozora` にHTML改行除去を追加: `text.gsub(/[\r\n]+/, "")` を先に実行（Ruby準拠）
3. `p_to_aozora` に `\n?</p>` 対応を追加（Ruby準拠）
4. template で body の先頭 `\n` を `trim_start_matches('\n')` で除去

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
   - convert_rome_numeric, alphabet_to_zenkaku, force_indent_special_chapter
   - convert_numbers — subtitle/chapter/story は全角変換のみ
   - exception_reconvert_kanji_to_num, convert_kanji_num_with_unit, rebuild_kanji_num
   - insert_separate_space
   - convert_special_characters: stash_kome(`※`→`※※`), convert_double_angle_quotation_to_gaiji, convert_novel_rule, convert_head_half_spaces
   - convert_fraction_and_date, modify_kana_ni_to_kanji_ni, convert_prolonged_sound_mark_to_dash
5. `convert_main_loop` — 行単位処理 + 後処理:
   - zenkaku_rstrip, request_insert_blank, process_author_comment
   - insert_blank_before_line_and_behind_to_special_chapter
   - insert_blank_line_to_border_symbol (■等の前後に空行+4字下げ)
   - outputs(line) → join
   - rebuild_force_indent_chapter
   - rebuild_illust, rebuild_url, rebuild_hankaku_num_comma
   - rebuild_kome_to_gaiji (`※※` → `※［＃米印、1-2-8］`)
   - half_indent_bracket, auto_indent ← **ここにバグあり**
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
  \n\n  ← 注: これが2空行(3改行)で参照と一致
  {body}
  (if postscript) ...
(if enable_display_end_of_book) \n［＃ここから地付き］［＃小書き］（本を読み終わりました）［＃小書き終わり］［＃ここで地付き終わり］\n
```

## 全修正済みバグ一覧 (31件)
1. DOTALL regex: body/introduction/postscriptパターンに`dot_matches_new_line(true)`
2. save_raw_file: 抽出bodyではなくraw HTMLを保存
3. HTML変換の順序: `to_aozora()`をconvertパイプラインの最初で実行
4. num_to_kanji OOB: `.min(9)`でクリップ
5. updateのtoc_url: DB recordのtoc_urlを優先
6. \k\<top_url\>再帰: interpolate()内でtop_urlを先に解決
7. Multiple URL patterns: `compiled_url: Vec<Regex>`に変更
8. multi_match DOTALL: `RegexBuilder::dot_matches_new_line(true)`を使用
9. download_time未設定: DL時に設定、update時に旧値保持
10. introduction/postscript省略: Option→Stringに変更し常にシリアライズ
11. fix_yaml_block_scalar: `|-`/`|`両方に対応
12. 青空注記プレフィックス: 全コードで `Ｃ`→`＃` に統一
13. 区切り線表記: `Ｐ区切線`→`＃区切り線`
14. URL形式: `<URL>`→`<a href="URL">URL</a>`
15. 字下げ表記: `三字`→`３字`（全角数字）
16. カクヨムDL対応: kakuyomu_preprocess, multi_line(true), href interpolation等
17. stash_kome: `※`→`※※`、rebuild_kome_to_gaiji: `※※`→`※［＃米印、1-2-8］`
18. convert_numbers: subtitle/chapter/story は hankaku_num_to_zenkaku のみ
19. before_hook: pack_blank_line, convert_page_break を追加
20. SectionFile使用: load_sections_from_dir が SectionFile 全体を返す
21. to_aozora 呼び出し: data_type != "text" の場合に element テキストに適用
22. auto_join_line: `。` 結合→`、` 結合のみ（Ruby準拠）
23. br_to_aozora: HTML中改行を先に全除去（Ruby準拠）
24. p_to_aozora: `\n?</p>` 対応（Ruby準拠）
25. body先頭 `\n` をテンプレート挿入時にstrip
26. デフォルトUA: 固定UAではなく `ua_generator` のFirefox系ランダムUAを使用。`--user-agent random` も同じ扱い。
27. syosetu.org 403対策: reqwestのデフォルトヘッダー、Cookie store、HTTP/1.1、gzip/brotli/deflateを有効化。
28. syosetu.org fallback: reqwest 403時にwget相当ヘッダー付きのcurl fallbackを追加し、成功後は `prefer_curl` でcurl優先に切り替える。
29. Brotli対応: reqwestはbrotli展開を有効化。curl fallbackは `curl -V` でbrotli対応を検出し、非対応なら `Accept-Encoding: gzip, deflate` に落とす。
30. syosetu.org本文URL: `1.html` などの相対hrefを目次URL基準で解決し、壊れた `/novel//k<ncode>/...` URLを作らない。
31. syosetu.orgタイトル/作者: `novel_info_url` 取得もDownloaderのUA/header/curl fallbackを通し、情報ページからタイトル・作者・連載種別を抽出するテストを追加。
