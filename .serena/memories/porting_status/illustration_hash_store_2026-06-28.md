# 2026-06-28: illustration hash store

- Follow-up to `mem:porting_status/illustration_dedup_web_state_2026-06-28`.
- Added `src/illustration_store.rs` as the shared persistent illustration mapping layer. It stores `.illustration_cache.yaml` in each novel archive directory, outside `挿絵/`, with mappings for normalized source URL, mitemin image ID, and SHA-256 content hash.
- Remote illustration files are now stored as `挿絵/<sha256>.<ext>` instead of mitemin URL basename or section-index-count. mitemin sources use their unique `iNNNN` ID to avoid re-download; non-mitemin sources deduplicate by content hash after download.
- Existing legacy files such as `挿絵/i422674.jpg` or `挿絵/<section>-<count>.jpg` are read, hashed, and copied to the hash filename when encountered; the legacy file is not deleted automatically.
- `src/converter/mod.rs` uses the store for HTML `<img>` localization and bumped the section conversion context marker to `illustration-localization:v3`.
- `src/downloader/mod.rs::download_illustration` also uses the same store for `illust_grep_pattern` prefetches.
- Verification: `cargo test illustration_store --lib`, `cargo test localize_section --lib`, `cargo test convert_novel_keeps_localized_illustration_annotation --lib`, and `cargo check` passed.
