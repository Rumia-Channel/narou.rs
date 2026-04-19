# Site datetime timezone handling (2026-04-19)

Problem: site/API date strings without timezone (for example Narou `2026-04-19 12:00:00` or `2026年04月19日 12時00分`) were parsed as UTC via `NaiveDateTime::and_utc()`. Because Narou and similar Japanese sites publish JST wall-clock times, Web UI displayed them 9 hours late (12:00 JST became 21:00 JST in a JST browser after storing 12:00 UTC).

Fix:
- Added `chrono-tz` with `case-insensitive` feature.
- In-memory Rust values remain `DateTime<Utc>`, but `database.yaml` serialization now uses narou.rb-compatible JST YAML timestamps (`YYYY-MM-DD HH:MM:SS.nnnnnnnnn +09:00`). Legacy Rust epoch-second values are still accepted on read.
- Added `timezone: Asia/Tokyo` support to `SiteSetting`; bundled Japanese site YAMLs (`ncode.syosetu.com`, `novel18.syosetu.com`, `syosetu.org`, `www.akatsuki-novels.com`, `www.mai-net.net`) declare it.
- Added local setting `time-zone` (default `Asia/Tokyo`) as fallback for site YAMLs without `timezone`.
- `parse_datetime_with_timezone()` handles IANA names via `chrono-tz` and fixed offsets like `+09:00`; RFC3339 / explicit `%z` inputs still keep their explicit offset.
- Narou API datetime parsing in both `src/commands/update.rs` and `src/downloader/narou_api.rs` now treats naive API values as `Asia/Tokyo`.
- Strong update date-only comparisons format YMD in the selected site timezone, avoiding UTC date rollover mistakes.

Verification:
- `cargo check`
- `cargo test ruby_time --lib`
- `cargo test timezone_less_site_datetime --lib`
- `cargo test parse_narou_date_accepts_japanese_datetime_with_weekday --lib`
- `cargo test narou_api_datetime_is_interpreted_as_jst --bin narou_rs`
- `cargo test bundled_japanese_site_definitions_set_jst_timezone --lib`
- `cargo test ensure_default_local_settings_writes_expected_defaults --bin narou_rs`
- `cargo test -- --test-threads=1`
- `ccc index`

Docs: `COMMANDS.md` updated for `time-zone` and timezone-aware update behavior.