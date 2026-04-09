# narou.rb Porting Status

## Completed
- Project structure (src/db/, src/downloader/, src/converter/, src/web/)
- Cargo.toml with all dependencies (edition 2024)
- error.rs: NarouError enum + Result type
- db/novel_record.rs: NovelRecord struct (serde, chrono DateTime)
- db/inventory.rs: Inventory (LRU cache, atomic write, Windows retry)
- db/index_store.rs: IndexStore (SHA256 fingerprint, by_toc_url/by_title)
- db/mod.rs: Database (singleton, CRUD, sorting, tag index)
- downloader/mod.rs: Downloader (full download pipeline)
- downloader/site_setting.rs: SiteSetting (YAML, \k<name> interpolation, multi_match with DOTALL)
- downloader/novel_info.rs: NovelInfo (web metadata fetch)
- downloader/html.rs: HTML→Aozora conversion (Ruby互換)
- downloader/rate_limit.rs: RateLimiter
- converter/settings.rs: IniData, NovelSettings (44 items), replace.txt
- converter/converter_base.rs: ConverterBase (Rubyパイプライン準拠)
- converter/mod.rs: NovelConverter (section conversion, SHA256 cache, Aozora output, novel.txt.erbテンプレート)
- web/mod.rs: Axum API (30+ endpoints)
- main.rs: CLI - all subcommands fully implemented
- Multi-page TOC, Kakuyomu eval, PersistentQueue, WebSocket PushServer, StreamingLogger
- **カクヨムDL動作確認** (ID=2, 294セクション)
- **なろうDL動作確認** (n8858hb, 24セクション)
- **なろうconvert動作確認** (青空文庫txt出力フォーマット互換)

## narou.rbフォーマット互換性

### なろう版 ✅ (第14ラウンド完了)
完全互換。全注記・構造がnarou.rb参照データと一致。

### カクヨム版 🔧 (第16ラウンド進行中)
構造（大見出し/柱/改ページ/中見出し/end-of-book）は完全一致。
※米印変換、全角数字も完全一致。
しかし**行数が合わない**（参照25,273行 vs 出力25,766行 = +493行）。

## 第16ラウンドで発見・修正した問題

### 修正済み
1. **auto_join_line 完全に間違っていた** — 旧実装は `。`で終わる行を次行と結合していた（全パラグラフを結合してしまう）。Rubyの正しい実装は `([^、])、\n　([^「『(（【<＜〈《≪・■…‥―　１-９一-九])` のみを結合（読点「、」で終わる行だけ）。
2. **br_to_aozora がHTML中の改行を除去していなかった** — Rubyの `br_to_aozora` は `text.gsub(/[\r\n]+/, "")` でHTMLソース中の全改行を先に除去してから `<br>` を `\n` に変換。旧実装はこれをやっておらず、`</p>` と次の `<p>` の間の改行が残り、to_aozora後に `\n\n\n` のような余分な空行が発生していた。
3. **p_to_aozora が `</p>` の前の `\n?` を考慮していなかった** — Rubyは `text.gsub(/\n?<\/p>/i, "\n")` で `\n?</p>` → `\n`。旧実装は `</p>` → `\n` のみ。
4. **debug用 cache clear と is_special_line_start を削除** — `convert_novel` 冒頭の `self.section_cache.clear()` はデバッグ用。`is_special_line_start` 関数は旧auto_join_line専用で未使用に。
5. **template で body の先頭 `\n` を strip** — to_aozora後のbody先頭に `\n` が残り、auto_indent が `\u{3000}` を付加して余分な空行になる問題を `trim_start_matches('\n')` で対処。

### 未解決（次の作業）
1. **出力が参照より493行多い** — 行レベルで1行ずつずれている。原因: 各セクションのbodyが参照より1行余分に出力されている可能性が高い。
2. **auto_indent が `\n` を `\u{3000}` に変換するバグ** — `auto_indent` の regex `(?m)^([^{ignore_chars}])` が、空行の先頭の `\n` にマッチして `\u{3000}\n` に変換してしまう。`\n` が ignore_chars に含まれていないため。Rubyの `auto_indent` も同じ動作のはずだが、参照データにはこの問題が起きていない。Rubyのline-by-line処理（`@write_fp.puts`）がこの問題を回避している可能性がある。
3. **cache clear を convert_novel から削除したが、convert_novel_by_id にある clear_cache メソッドは残している** — これは意図的なものか確認が必要。

## 次にやること（優先順位）

### P0: 行数差分の原因究明と修正
```
参照: 25,273行
出力: 25,766行 (+493行)
```
各セクションのbodyが1行ずつ多い可能性。以下の手順で調査:
1. `cargo run -- convert 2` 後のtxtと参照txtをdiff
2. diffの最初の差異を確認（Line 23あたりからずれている）
3. ズャクションごとにline offsetがどう変化するか確認
4. bodyの先頭に余分な `\n` が残っているか確認（`trim_start_matches('\n')` が効いているか）
5. `auto_indent` の `\n` → `\u{3000}` 問題が他にも影響しているか確認

### P0: auto_indent の `\n` マッチ問題の根本的修正
2つのアプローチ:
- A) `(?m)^([^{ignore_chars}\n])` にして `\n` をignore_charsに追加
- B) Rubyのline-by-line処理を模倣し、auto_indent前にテキストの先頭/末尾の `\n` を処理
- C) auto_indent前に `data = data.strip_prefix('\n')` で先頭 `\n` を除去（テンプレート側で既に `\n\n` を追加しているため）

### P1: 全体diffの詳細分析
1行ずつのずれが全体でどう伝播するか確認
章（chapter）境界、後書き（postscript）境界でも同様のずれが起きているか

### P2: なろう版convert回帰テスト
カクヨム対応でなろう版が壊れていないか確認: `cargo run -- convert 1`

## 参照データ
- カクヨム: `sample/1177354055617350769 「先輩の妹じゃありません！」/kakuyomu_jp_1177354055617350769.txt` (25,273行)
- 出力先: `sample/novel/小説データ/カクヨム/1177354055617350769 「先輩の妹じゃありません！」/output/「先輩の妹じゃありません！」.txt`

## 全修正済みバグ一覧 (第1〜16ラウンド)
1. DOTALL regex: body/introduction/postscriptパターンに`dot_matches_new_line(true)`が必要
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
12. 青空注記プレフィックス: 全コードで `Ｃ`(\u{FF23})→`＃`(\u{FF03}) に統一
13. 区切り線表記: `Ｐ区切線`→`＃区切り線`
14. URL形式: `<URL>`→`<a href="URL">URL</a>` に変更
15. 字下げ表記: `三字`→`３字`（全角数字）
16. カクヨムDL対応: kakuyomu_preprocess, multi_line(true), href interpolation, chapter ID抽出等
17. stash_kome: `※` → `※※`、rebuild_kome_to_gaiji: `※※` → `※［＃米印、1-2-8］`
18. convert_numbers: subtitle/chapter/story は hankaku_num_to_zenkaku のみ（漢字変換しない）
19. before_hook: pack_blank_line, convert_page_break を追加
20. SectionFile使用: load_sections_from_dir が SectionFile 全体を返すように変更
21. to_aozora 呼び出し: convert_novel 内で data_type != "text" の場合に element テキストに対して呼ぶ
22. auto_join_line: 全く間違った実装をRuby準拠に修正（`。`結合→`、`結合のみ）
23. br_to_aozora: HTML中の改行を先に全除去する処理を追加（Ruby準拠）
24. p_to_aozora: `</p>` 前の `\n?` を考慮（Ruby準拠）
25. body先頭の `\n` をテンプレート挿入時にstrip
