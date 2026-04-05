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

## Completed (round 2)
- **4-layer setting merge**: `NovelSettings::load_for_novel()` - force.* (local_setting.yaml) > setting.ini (per-novel section) > default.* (global INI) > ORIGINAL_SETTINGS
- **Device abstraction**: `converter/device.rs` - Device enum (Text, Epub, Mobi, Kobo), OutputManager with AozoraEpub3/kindlegen external tool invocation
- **NovelInfo local cache**: `downloader/info_cache.rs` - disk-backed cache with YAML persistence, max 500 entries
- **Narou API batch fetch**: `Downloader::narou_api_batch_update()` - bulk metadata via syosetu API, 50 items per request
- **Differential detection**: `Downloader::section_needs_update()` + `compute_section_hash()` - SHA256 content hash comparison
- **confirm_over18 handling**: `Downloader::handle_over18()` - regex-based R18 page detection
- **Illustration download**: `Downloader::download_illustration()` - illust_grep_pattern based image extraction and saving
- **Kakuyomu eval handler**: `SiteSetting::eval_kakuyomu()` - JSON preprocessing from __NUXT__ script tag
- **PersistentQueue**: `queue.rs` - crash-recovery persistent job queue with YAML persistence, push/pop/complete/fail
- **WebSocket PushServer**: `web/push.rs` - broadcast channel, WS handler, client management, event broadcasting
- **StreamingLogger**: `web/push.rs::StreamingLogger` - log buffering, broadcast to WS clients
- **30+ new Web API endpoints**: get/remove/freeze/unfreeze novels, tag management (add/remove/batch), download/update/convert APIs, settings CRUD, device list, queue status, recent logs
- **CORS support**: tower-http CorsLayer
- **`is_narou` field**: Added to NovelRecord for Narou API identification

## New files created
- `src/converter/device.rs` - Device enum + OutputManager
- `src/downloader/info_cache.rs` - NovelInfoCache
- `src/queue.rs` - PersistentQueue + QueueJob + JobType

## Remaining (minor)
- converter.rb DSL (user-defined dynamic converter loading)
- AozoraEpub3/kindlegen actual invocation testing (tool detection works)
- Device-specific epub options refinement
- WebSocket message framing (currently just text)
- StreamingInput (CLI confirm → browser modal bridge)
- Eventable event system
- Auto-update scheduler with cron-like triggers

## Next Steps Priority
1. Test with real webnovel YAML files
2. converter.rb DSL if needed
3. Refine AozoraEpub3 command-line options
4. StreamingInput implementation
5. Auto-update scheduler
1. AozoraEpub3/kindlegen external tool invocation (convert command output)
2. 4-layer setting merge completion
3. Remaining web API endpoints (download/update/convert as API)
4. Kakuyomu eval handler
5. Differential detection for update efficiency
