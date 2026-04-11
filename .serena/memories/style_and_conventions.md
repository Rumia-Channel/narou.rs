# Style and Conventions

## Code Style
- **No comments** unless explicitly asked
- Do not edit `Cargo.toml` directly for dependency changes. Use Cargo commands such as `cargo add` and `cargo update` so dependency versions come from the current registry metadata. If a direct manifest edit is unavoidable, document why and verify with `cargo check`.
- Use `thiserror` for error types with `#[from]` for automatic conversions
- `pub type Result<T> = std::result::Result<T, NarouError>` pattern
- Use `parking_lot::Mutex` over `std::sync::Mutex`
- Use `dashmap::DashMap` for concurrent maps
- Database singleton: `static DATABASE: OnceLock<Mutex<Database>>`
- Helper functions: `with_database()`, `with_database_mut()`

## Naming
- snake_case for functions, methods, variables
- PascalCase for types, structs, enums
- SCREAMING_SNAKE_CASE for constants
- Module names: short, descriptive (db, web, converter, downloader)

## Patterns
- Builder pattern for complex structs (not yet widely used)
- LRU cache with protection for certain keys (inventory)
- Atomic write: write to tmp file → rename (with Windows EACCES retry)
- SHA256 fingerprinting for change detection (index store, converter cache)
- stash/rebuild pattern in converter (placeholder substitution)

## NovelSettings 44 items
enable_yokogaki, enable_inspect, enable_convert_num_to_kanji, enable_kanji_num_with_units, kanji_num_with_units_lower_digit_zero, enable_alphabet_force_zenkaku, disable_alphabet_word_to_zenkaku, enable_half_indent_bracket, enable_auto_indent, enable_force_indent, enable_auto_join_in_brackets, enable_auto_join_line, enable_enchant_midashi, enable_author_comments, enable_erase_introduction, enable_erase_postscript, enable_ruby, enable_illust, enable_transform_fraction, enable_transform_date, date_format, enable_convert_horizontal_ellipsis, enable_convert_page_break, to_page_break_threshold, enable_dakuten_font, enable_display_end_of_book, enable_add_date_to_title, title_date_format, title_date_align, title_date_target, enable_ruby_youon_to_big, enable_pack_blank_line, enable_kana_ni_to_kanji_ni, enable_insert_word_separator, enable_insert_char_separator, enable_strip_decoration_tag, enable_add_end_to_title, enable_prolonged_sound_mark_to_dash, cut_old_subtitles, slice_size, author_comment_style, novel_author, novel_title, output_filename

Setting priority: force.* > setting.ini > default.* > ORIGINAL_SETTINGS
