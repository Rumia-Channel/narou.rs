# Kakuyomu parity test note (2026-04-19)

- Full `cargo test` now passes the `tests/convert_parity.rs` Kakuyomu byte-for-byte fixture.
- The fixture texts under `sample/novel/小説データ/カクヨム/*/kakuyomu_jp_*.txt` are generated with EPUB-style device defaults, so `enable_half_indent_bracket` must be false in the parity test. `NovelSettings::default()` keeps Ruby's base default true, but the EPUB device preset disables it.
- `src/converter/converter_base/mod.rs` now inserts the private E000 marker after any synthetic leading newlines rather than before them, so lines protected from half-indent/auto-indent remain protected when `convert_main_loop` prefixes an extra blank line.
