# モジュール分割リファクタリング (2026-04-11)

## 概要
10個の大きなファイル（最大1394行）を責任ごとに66ファイルに分割。全ファイル200行以下を目標としたが、pipelineオーケストレーター（downloader/mod.rs: 677行, converter/settings.rs: 616行）はこれ以上分割すると不自然なためそのまま。

## 分割結果

### main.rs (804行) → cli.rs + commands/
- `cli.rs` — Cli struct + Commands enum (clap定義)
- `commands/mod.rs` — pub mod + resolve_target_to_id helper
- `commands/init.rs` — narou init (352行)
- `commands/download.rs`, `update.rs`, `convert.rs`, `web.rs`, `manage.rs`

### db/mod.rs (234行) → database.rs + paths.rs
- `database.rs` — Database struct (CRUD, sort, tag index)
- `paths.rs` — novel_dir_for_record, create_subdirectory_name
- `mod.rs` — DATABASE static, init_database, with_database/mut (singleton accessor)

### downloader/mod.rs (1394行) → 8ファイル
- `types.rs` — SectionElement, SectionFile, TocObject, DownloadResult 等のデータ型
- `fetch.rs` — HttpFetcher struct (curl → reqwest → wget の3-tier fallback)
- `toc.rs` — fetch_toc, parse_subtitles, parse_subtitles_multipage
- `section.rs` — download_section, parse_section_html, section cache
- `persistence.rs` — save_section_file, save_raw_file, save_toc_file
- `narou_api.rs` — narou_api_batch_update
- `util.rs` — build_section_url, pretreatment_source, sanitize_filename 等
- `mod.rs` — Downloader struct (orchestrator), target resolution, illustration download

### downloader/preprocess.rs (838行) → preprocess/ dir
- `preprocess/ast.rs` — Stmt, Expr, StrPart, Accessor 等 (AST型定義)
- `preprocess/parser.rs` — PreprocessParser (pest grammar), parse_preprocess, build_*
- `preprocess/interpreter.rs` — Ctx, eval_expr, eval_stmt, eval_method
- `preprocess/mod.rs` — PreprocessPipeline struct, run_preprocess

### downloader/site_setting.rs (709行) → site_setting/ dir
- `site_setting/mod.rs` — SiteSetting struct, accessor methods, compile, load_all
- `site_setting/interpolate.rs` — \k<name> テンプレートエンジン
- `site_setting/info_extraction.rs` — resolve_info_pattern, multi_match
- `site_setting/loader.rs` — load_all_from_dirs, load_settings_from_dir, merge_site_setting
- `site_setting/serde_helpers.rs` — deserialize_yes_no_bool

### converter/mod.rs (534行) → 3ファイル
- `render.rs` — render_novel_text (novel.txt.erb相当), ConvertedSection
- `output.rs` — create_output_text_path/filename, extract_domain/ncode_like
- `mod.rs` — NovelConverter struct, convert_novel pipeline, cache

### converter/settings.rs (757行) → ini.rs + settings.rs
- `ini.rs` — IniData / IniValue (INI parser/serializer)
- `settings.rs` — NovelSettings (44 items, INI overlay, replace.txt) 残り616行

### converter/converter_base.rs (900行) → converter_base/ dir
- `converter_base/mod.rs` — ConverterBase struct, TextType, convert pipeline orchestrator
- `converter_base/character_conversion.rs` — 半角/全角変換, 数字→漢数字, TCY
- `converter_base/indentation.rs` — auto_indent, half_indent_bracket, insert_separate_space
- `converter_base/stash_rebuild.rs` — illust/URL/kome stash & rebuild
- `converter_base/ruby.rs` — narou_ruby, find_ruby_base
- `converter_base/text_normalization.rs` — rstrip, ellipsis, page_break, dust_char 等

### converter/user_converter.rs (322行) → user_converter/ dir
- `user_converter/mod.rs` — UserConverter struct, load, apply_before/after, signature
- `user_converter/setting_override.rs` — apply_setting_override (26-arm match)

### web/mod.rs (823行) → 8ファイル
- `state.rs` — ApiResponse, IdPath, ListParams 等 (DTO structs)
- `novels.rs` — index, novels_count, api_list, get/remove/freeze/unfreeze
- `tags.rs` — add_tag, remove_tag, update_tags
- `batch.rs` — batch_tag/untag/freeze/unfreeze/remove
- `jobs.rs` — api_download/update/convert, queue_status/clear
- `novel_settings.rs` — get_settings, save_settings, list_devices
- `misc.rs` — version_current, tag_list, notepad_read/save, recent_logs
- `mod.rs` — AppState, create_router

## 検証
- `cargo check`: コンパイル成功 (警告のみ、全て既存)
- `cargo test`: 全9テスト通過 (7 unit + 2 integration)

## 設計方針
- 各ファイルは単一責任 (single responsibility)
- パイプラインオーケストレーター (Downloader, ConverterBase, NovelConverter) は mod.rs に残し、具体的な処理をサブモジュールに委譲
- 外部公開API (import path) は re-export で維持: `crate::downloader::Downloader`, `crate::converter::NovelConverter` 等
- HttpFetcher を Downloader から分離し、fetch.rs に独立structとして定義
