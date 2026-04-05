# Task Completion Checklist

When a coding task is completed:
1. Run `cargo check` to verify compilation
2. Run `cargo clippy` for lint checks
3. Run `cargo fmt --check` to verify formatting
4. If any issues, fix them before considering the task done
5. Do NOT commit unless explicitly asked

## Completed Rounds

### Round 1 - Project Structure
- Project structure, Cargo.toml, error.rs, db/ (all files), downloader/ (all files), converter/ (all files), web/mod.rs, main.rs
- Compilation success

### Round 2 - Feature Implementation
- 4-layer settings merge, Device abstraction, Narou API batch, differential detection, confirm_over18, illustration DL, Kakuyomu eval, PersistentQueue, WebSocket PushServer, StreamingLogger, 30+ API endpoints, CORS, NovelInfo cache, is_narou field, NovelSettings Serialize

### Round 3 - Converter DSL + Testing
- User-defined converter (YAML DSL), ConverterBase integration, NovelConverter integration, test environment setup, tokio+blocking fix

### Round 4 - Download Bug Fixes + Live Test
- YAML boolean deserializer (yes/no strings → bool)
- \k<> interpolation fix (unknown keys preserved instead of replaced with empty)
- \\k double backslash handling in YAML unquoted scalars
- URL capture extraction and propagation (ncode from URL → toc_url, novel_info_url)
- fetch_toc signature change (accept URL parameter)
- Live download test PASSED: n8858hb downloaded with 24 sections

## Fixed Bugs
1. YAML parse failure: confirm_over18/append_title_to_folder_name "yes"/"no" strings
2. \k<> interpolation: unknown keys replaced with empty string breaking URLs
3. \\k double backslash: YAML unquoted scalar \\k not matching regex \k
4. URL capture not propagated: ncode from URL match not flowing to toc_url/novel_info_url
