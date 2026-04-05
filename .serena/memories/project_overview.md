# narou.rs - Rust port of narou.rb

## Purpose
narou.rb (Ruby) のサーバー実行部分がメモリを大量に消費するため、Rustに移植する。
narou.rbは日本のWeb小説（なろう、ハーメルン、 Kakuyomu等）の管理・電子書籍変換ソフトウェア。

## Priority
1. Database layer (done)
2. Downloader (partially done)
3. Converter (partially done)
4. HTTP API with Axum (partially done)

## Tech Stack
- **Language**: Rust (edition 2024)
- **Web framework**: Axum 0.8
- **Async runtime**: Tokio (full features)
- **Serialization**: serde + serde_yaml + serde_json
- **HTTP client**: reqwest (blocking, cookies, gzip/brotli/deflate)
- **CLI**: clap 4
- **Date/time**: chrono
- **Regex**: regex
- **Hashing**: sha2 + hex
- **Error handling**: thiserror
- **Template**: askama
- **Logging**: tracing + tracing-subscriber
- **Sync**: parking_lot, dashmap
- **Browser open**: open

## Project Structure
```
src/
  lib.rs              - module definitions
  main.rs             - CLI entry point (clap subcommands)
  error.rs            - NarouError enum + Result type
  db/
    mod.rs            - Database (singleton, CRUD, sorting, tag index)
    novel_record.rs   - NovelRecord struct (serde)
    inventory.rs      - Inventory (LRU cache, atomic write, Windows retry)
    index_store.rs    - IndexStore (SHA256 fingerprint, by_toc_url/by_title)
  downloader/
    mod.rs            - Downloader (target resolve, TOC fetch, subtitle parse, section download)
    site_setting.rs   - SiteSetting (YAML load, \k<name> interpolation, compiled regex)
    html.rs           - HTML→Aozora conversion (br, p, ruby, b, i, s, img, em→傍点)
    rate_limit.rs     - RateLimiter (global state, step boundary wait)
  converter/
    mod.rs            - NovelConverter (section conversion pipeline, SHA256 cache)
    converter_base.rs - ConverterBase (text transformation pipeline)
    settings.rs       - IniData, NovelSettings (44 items, replace.txt parser)
  web/
    mod.rs            - Axum API server (routes, DataTables support)
```

## Key Design Decisions
- Database YAML format compatible with narou.rb's `database.yaml` / `database_index.yaml`
- Site settings YAML (`webnovel/*.yaml`) format compatible (including `\k<name>` variable interpolation)
- Aozora bunko format text output support (`［＃改ページ］`, `｜《》` ruby notation, etc.)
- Windows-compatible (path separators, atomic write retry on EACCES)
- Singleton Database pattern with static Mutex + helper functions

## Reference Sources (Ruby, read-only)
- `sample/narou/` - Ruby source code for reference
- Key files: database.rb, inventory.rb, downloader.rb, converterbase.rb, novelconverter.rb, appserver.rb, sitesetting.rb, html.rb, novelsetting.rb
