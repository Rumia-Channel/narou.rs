# narou.rb Porting Status

## Completed
- Project structure (src/db/, src/downloader/, src/converter/, src/web/)
- Cargo.toml with all dependencies
- error.rs: NarouError enum + Result type
- db/novel_record.rs: NovelRecord struct (serde, chrono DateTime)
- db/inventory.rs: Inventory (LRU cache, atomic write, Windows retry)
- db/index_store.rs: IndexStore (SHA256 fingerprint, by_toc_url/by_title)
- db/mod.rs: Database (singleton, CRUD, sorting, tag index)
- downloader/mod.rs: Downloader (full download pipeline, target resolve, TOC fetch, subtitle parse, section download, file save, DB update)
- downloader/site_setting.rs: SiteSetting (YAML, \k<name> interpolation, multi_match, capture group interpolation, novel_type detection)
- downloader/novel_info.rs: NovelInfo (web metadata fetch via novel_info_url, date parsing)
- downloader/html.rs: HTML→Aozora conversion
- downloader/rate_limit.rs: RateLimiter
- converter/settings.rs: IniData, NovelSettings (44 items), replace.txt
- converter/converter_base.rs: ConverterBase (text transformation pipeline)
- converter/mod.rs: NovelConverter (section conversion, SHA256 cache, Aozora output, convert_novel_by_id)
- web/mod.rs: Axum API (list, count, version, tags, notepad)
- main.rs: CLI - all subcommands fully implemented (web, download, update, convert, list, tag, freeze, remove)
- Multi-page TOC support (next_toc/next_url/toc_page_max)
- TOC file save/load (toc.yaml format)
- TocFile + DownloadResult structs
- Compiles with `cargo check` + `cargo clippy` (no errors)

## Not Yet Implemented

### Downloader
- Kakuyomu eval handler (JSON preprocessing - needs different approach in Rust)
- Narou API batch fetch (narou_api_url for bulk metadata during update)
- Differential detection (update_body_check - content hash comparison for existing sections)
- Illustration download (illust_current_url / illust_grep_pattern)
- confirm_over18 handling (R18 age confirmation page)
- Digest/merge processing (user confirmation when episodes are deleted)

### Converter
- 4-layer setting merge: force.* > setting.ini > default.* > ORIGINAL_SETTINGS (INI only done)
- converter.rb DSL (user-defined dynamic converter loading)
- AozoraEpub3/kindlegen external tool invocation (EPUB/MOBI generation)
- Device abstraction (Kindle/Kobo/iBooks device-specific processing)
- Template engine (askama imported but no templates created)

### Web/API
- WebSocket PushServer (real-time browser notifications, console output forwarding)
- PersistentQueue (crash-recovery persistent job queue)
- StreamingLogger ($stdout → WebSocket bridge)
- StreamingInput (CLI confirm/choose → browser modal)
- Remaining API endpoints (~50+ routes from appserver.rb: download/update/convert APIs, settings APIs, etc.)

### Other
- NovelInfo local cache
- Auto-update scheduler
- Eventable event system

## Next Steps Priority
1. AozoraEpub3/kindlegen external tool invocation (convert command output)
2. 4-layer setting merge completion
3. Remaining web API endpoints (download/update/convert as API)
4. Kakuyomu eval handler
5. Differential detection for update efficiency
