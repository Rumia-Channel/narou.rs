# narou.rb Porting Status

## Completed
- Project structure (src/db/, src/downloader/, src/converter/, src/web/)
- Cargo.toml with all dependencies
- error.rs: NarouError enum
- db/novel_record.rs: NovelRecord struct (serde, chrono DateTime)
- db/inventory.rs: Inventory (LRU cache, atomic write, Windows retry)
- db/index_store.rs: IndexStore (SHA256 fingerprint, by_toc_url/by_title)
- db/mod.rs: Database (singleton, CRUD, sorting, tag index)
- downloader/mod.rs: Downloader (target resolve, TOC fetch, subtitle parse, section download)
- downloader/site_setting.rs: SiteSetting (YAML, \k<name> interpolation, regex)
- downloader/html.rs: HTML→Aozora conversion
- downloader/rate_limit.rs: RateLimiter
- converter/settings.rs: IniData, NovelSettings (44 items), replace.txt
- converter/converter_base.rs: ConverterBase (text transformation pipeline)
- converter/mod.rs: NovelConverter (section conversion, SHA256 cache, Aozora output)
- web/mod.rs: Axum API (list, count, version, tags, notepad)
- main.rs: CLI (web, download, update, convert, list, tag, freeze, remove)
- Compiles with `cargo check`

## Not Yet Implemented
- Kakuyomu eval handler (JSON preprocessing)
- PersistentQueue (crash recovery job queue)
- Illustration download
- Device abstraction (Kindle/Kobo/iBooks)
- WebSocket PushServer
- 4-layer setting merge (force.* > setting.ini > default.* > ORIGINAL_SETTINGS) - INI only
- converter.rb DSL (user-defined converters)
- Template engine (askama imported, no templates)
- AozoraEpub3/kindlegen external tool invocation

## Next Steps Priority
1. Complete download command (full pipeline: resolve→fetch_toc→parse→download sections→save files→update DB)
2. Implement update command (differential download + DB update)
3. Implement convert command (AozoraEpub3 integration)
4. Implement tag/freeze/remove commands
5. Add more web API endpoints
