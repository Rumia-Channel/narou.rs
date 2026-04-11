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

## Porting Policy
- This project is a Rust compatibility port of the upstream narou.rb implementation under `sample/narou`.
- Rust internals do not need to match the Ruby libraries, data structures, processing pipeline, or algorithms.
- Compatibility is judged by externally visible behavior: CLI/API arguments and results, YAML syntax support (`webnovel/*.yaml`, `converter.yaml`, etc.), `.narou/` file reads/writes, and final generated output files.
- Treat the Ruby implementation as the reference specification, but prefer Rust designs that are maintainable, safe, and easy to verify when they preserve the same external behavior and output.

## Key Design Decisions
- Database YAML format compatible with narou.rb's `database.yaml` / `database_index.yaml`
- Site settings YAML (`webnovel/*.yaml`) format compatible (including `\k<name>` variable interpolation)
- Aozora bunko format text output support (`［＃改ページ］`, `｜《》` ruby notation, etc.)
- Windows-compatible (path separators, atomic write retry on EACCES)
- Singleton Database pattern with static Mutex + helper functions

## Edition Note
- Rust edition 2024: `\n` in println! requires `println!()` separate call or use `{key}` with space before text. String escapes like `\u{NNNN}` work normally but some tools may double-escape backslashes.

## CLI Subcommands (all implemented)
- `web [--port N] [--no-browser]` - Axum web server
- `download <url|ncode|id>...` - Full pipeline: resolve→TOC→metadata→sections→save→DB
- `update [--all | <id>...]` - Re-download all or specified novels
- `convert <id|title>...` - Convert saved novel to Aozora text
- `list [--tag T] [--frozen]` - List novels with optional filters
- `tag --add T | --remove T <targets>` - Add/remove tags
- `freeze <targets> [--off]` - Freeze/unfreeze novels
- `remove <targets>` - Remove novel (DB + files)

## Reference Sources (Ruby, read-only)
- `sample/narou/` - Ruby source code for reference
- Key files: database.rb, inventory.rb, downloader.rb, converterbase.rb, novelconverter.rb, appserver.rb, sitesetting.rb, html.rb, novelsetting.rb
