# syosetu.org fetch / UA / brotli / title-author fix (2026-04-11)

Implemented and verified after the syosetu.org 403 report for `download https://syosetu.org/novel/232822/`.

## Implemented
- Default downloader UA is now random via `ua_generator::ua::spoof_firefox_ua()`. `None`, empty UA, and `--user-agent random` all use this generator. Explicit non-empty `--user-agent` is still honored exactly.
- `Downloader::with_user_agent` builds reqwest with browser/wget-like defaults: `cookie_store(true)`, `http1_only()`, `gzip(true)`, `brotli(true)`, `deflate(true)`, timeout 30s, and default headers for Accept, Accept-Language, Accept-Encoding (`gzip, deflate, br`), Accept-Charset, Connection.
- Added curl fallback for syosetu.org-style 403s. `fetch_toc`, `download_section`, and novel info fetching use curl fallback. If fallback succeeds, `prefer_curl = true` so later requests try curl first and avoid slow reqwest 403 retries per episode.
- Curl fallback uses `curl.exe` on Windows and `curl` elsewhere with `--fail --silent --show-error --location --http1.1 --compressed`, the selected UA, Accept headers, and optional Cookie header.
- Curl brotli handling is conditional. `curl -V` is checked once via `OnceLock`; if local curl reports brotli/libbrotli, send `Accept-Encoding: gzip, deflate, br`, otherwise send `gzip, deflate`. On this Windows environment, curl did not report brotli support.
- Fixed syosetu.org relative section URLs. `build_section_url(setting, toc_url, href)` resolves `1.html` relative to the TOC URL, avoiding broken URLs like `https://syosetu.org/novel//k<ncode>/1.html`.
- Fixed missing title/author. `novel_info_url` previously used `NovelInfo::load` with bare reqwest and could return empty on 403. `Downloader::load_novel_info` now fetches the info page through the same UA/header/curl fallback path and then parses it.
- `NovelInfo` parsing was split into `from_novel_info_source` and `from_toc_source` so fetched HTML parsing can be tested independently.
- Added test `downloader::tests::syosetu_org_info_patterns_extract_title_and_author`, which verifies the current syosetu.org info-page HTML shape extracts title, author `鉄鋼怪人`, and novel_type `Some(1)` for novel 232822.

## Investigation notes
- UA alone was not enough for reqwest against syosetu.org; wget/curl compatibility depended on additional request traits: HTTP/1.1, Accept/Accept-Language/Accept-Encoding/Accept-Charset/Connection, cookie handling, and compression decoding.
- Chrome-like generated UA triggered Cloudflare challenge in testing, while Firefox-like UA worked with curl and wget-like headers. That is why the default random UA currently uses the Firefox generator.

## Verified
- `cargo check` passed.
- `cargo test` passed, including existing Kakuyomu parity tests and the new syosetu.org info extraction test.
- Direct curl-style fetch of `https://syosetu.org/?mode=ss_detail&nid=232822` returned current HTML containing タイトル, 作者, and 話数 rows.

## Not verified
- Full 251-episode download completion for `https://syosetu.org/novel/232822/` was not rerun to completion after the final title/author fix. Earlier full run was stopped due time after the fallback path was still being optimized.

## Key files
- `src/downloader/mod.rs`
- `src/downloader/novel_info.rs`
- `src/main.rs` for global `--user-agent` plumbing from the preceding UA work
- `Cargo.toml` includes `ua_generator`
