# narou.rs — Rust Port of narou.rb

## Overview
narou.rb（Ruby製の日本のWeb小説管理・電子書籍変換ソフトウェア）のサーバー実行部分をRustに移植するプロジェクト。なろう・カクヨム等のサイトからのDL・変換が動作し、narou.rbの出力フォーマットと完全互換性を持つことを目指す。

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
  main.rs                          - CLI (clap subcommands: download, update, convert, list, etc.)
  error.rs                         - NarouError enum + Result type
  db/
    mod.rs                         - Database (singleton, CRUD, sorting, tag index)
    novel_record.rs                - NovelRecord struct
    inventory.rs                   - Inventory (LRU cache, atomic write, Windows retry)
    index_store.rs                 - IndexStore (SHA256 fingerprint)
  downloader/
    mod.rs                         - Downloader (full DL pipeline, SectionFile/SectionElement structs)
    site_setting.rs                - SiteSetting (YAML, \k<name> interpolation, multi_match)
    html.rs                        - to_aozora (HTML→青空文庫形式変換)
    rate_limit.rs                  - RateLimiter
  converter/
    mod.rs                         - NovelConverter (convert_novel, render_novel_text, section cache)
    converter_base.rs              - ConverterBase (Ruby準拠のテキスト変換パイプライン)
    settings.rs                    - NovelSettings (44 items, replace.txt parser)
    user_converter.rs              - converter.yaml 対応 (宣言的ユーザー定義コンバータ)
  web/
    mod.rs                         - Axum API (30+ endpoints)
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

## 全修正済みバグ一覧 (25件)
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
