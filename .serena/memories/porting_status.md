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
- downloader/html.rs: HTML→Aozora conversion
- downloader/rate_limit.rs: RateLimiter
- converter/settings.rs: IniData, NovelSettings (44 items), replace.txt
- converter/converter_base.rs: ConverterBase (text transformation pipeline)
- converter/mod.rs: NovelConverter (section conversion, SHA256 cache, Aozora output)
- web/mod.rs: Axum API (30+ endpoints)
- main.rs: CLI - all subcommands fully implemented
- Multi-page TOC, Kakuyomu eval, PersistentQueue, WebSocket PushServer, StreamingLogger

## narou.rbフォーマット互換性 (第6ラウンド完了)

### toc.yaml ✅
- `---` YAML frontmatter
- `story: |+` (block scalar, keep trailing newline) — DOTALL regex fix
- `author` populated from info page
- `download_time` on each subtitle (set during download, preserved on update)
- `novel_type`, `subupdate` present when applicable

### 本文/*.yaml (SectionFile) ✅
- `---` YAML frontmatter
- All metadata: index, href, chapter, subchapter, subtitle, file_subtitle, subdate, subupdate, download_time
- `element:` nesting: data_type, introduction, postscript, body
- `introduction: ''` and `postscript: ''` always present (String with #[serde(default)])

### setting.ini / replace.txt ✅
- Auto-generated on download (ensure_default_files)

### txt出力ヘッダ ✅ (軽微差異あり)
- Format: `タイトル\n著者名\n\n［Ｐ区切線］\nあらすじ：\n...\n掲載ページ:\n<URL>\n［Ｐ区切線］`
- Diff: narou.rb uses `［＃区切り線］` vs our `［Ｐ区切線］` (both valid aozora)
- Diff: narou.rb wraps URLs as `<a href="URL">URL</a>` vs our `<URL>`
- Diff: narou.rb uses `［＃３字下げ］` vs our `［Ｃ三字下げ］`

### 第6ラウンドで修正したバグ
8. multi_match DOTALL: story等の複数行パターンに`dot_matches_new_line(true)`が必要
9. download_time未設定: SubtitleInfo.download_timeがNone固定→DL時に設定、update時に旧値保持
10. introduction/postscript省略: Option→Stringに変更し常にシリアライズ
11. fix_yaml_block_scalar: `|-`/`|`両方に対応

## Remaining (minor)
- converter.rb DSL (user-defined dynamic converter loading)
- AozoraEpub3/kindlegen actual invocation testing
- カクヨム対応の実動テスト
- 他サイト対応の実動テスト
- txt出力の青空注記記号スタイル統一(＃ vs Ｐ)
