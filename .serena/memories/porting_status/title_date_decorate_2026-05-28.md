# Title Date Decoration Compatibility (2026-05-28)

- Rust `convert` now applies the Ruby `NovelConverter#decorate_title` equivalent to the first rendered book title line used by AozoraEpub3/EPUB cover generation.
- Covered settings: `enable_add_date_to_title`, `title_date_format`, `title_date_align`, `title_date_target`, and `enable_add_end_to_title`.
- Covered Ruby extended title-date symbols: `$t`, `$s`, `$ns`, `$nt`, `$ntag`.
- Covered date targets: `general_lastup`, `last_update`, `new_arrivals_date`, and `convert` fallback to current time.
- The same render path is used by `download`, `update`, and `convert`, so the fix covers all three command flows.
- Investigation found the default setting loader was already passing these values into `NovelSettings`; the missing compatibility was in final title rendering.
- Regression tests were added in `src/converter/render.rs` and `src/converter/settings.rs`.
