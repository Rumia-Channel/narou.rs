# Kakuyomu parity work (2026-04-09)

## Goal
Bring Rust converter output for Kakuyomu novel ID `1177354055617350769` to full narou.rb-compatible parity against:
`sample/1177354055617350769 「先輩の妹じゃありません！」/kakuyomu_jp_1177354055617350769.txt`

## Final state
- `cargo check` passes
- `cargo run -- convert 2` from `sample/novel` passes
- output line count matches reference exactly: `25273 / 25273`
- final line-by-line diff count: `0`

## Files changed during parity work
- `src/converter/converter_base.rs`
- `src/converter/mod.rs`
- `src/downloader/html.rs`

## Important fixes applied
1. `auto_join_line` fixed to Ruby behavior
- Broken replacement `"$!1、$!2"` corrected to `"$1、$2"`
- join condition kept to Ruby-style `、\n　` cases only

2. Number conversion normalized toward Ruby
- `convert_numbers_to_kanji` changed from broken positional logic to digit-by-digit kanji mapping
- `KANJI_DIGITS[0]` changed from `零` to `〇`
- `exception_reconvert_kanji_to_num` implemented for adjacent Latin/fullwidth `%`-style cases

3. Missing pipeline stages restored
- `insert_separate_space` implemented and wired into `convert_for_all_data`
- `alphabet_to_zenkaku` implemented and added
- `modify_kana_ni_to_kanji_ni` changed to closure-based replacement to avoid `$1二$2` misparse

4. Punctuation / notation fixes
- `convert_novel_rule` corrected so `。　` becomes `。` instead of dropping the period
- `convert_horizontal_ellipsis` made Ruby-like for runs of `・` / `。` / `、` / `．`
- sesame handling added for explicit ruby of the form `｜base《・...》`
- `em_to_sesame` annotation text corrected from `旁点` to `傍点`

5. Indentation / line-structure fixes
- `half_indent_bracket` now always runs for body/textfile
- when disabled, it still strips leading spaces before opening-bracket lines instead of skipping entirely
- body/introduction/postscript rendering trims trailing newlines in `src/converter/mod.rs`

6. Subtitle normalization
- `normalize_subtitle_markup` added in `src/converter/mod.rs`
- removes unwanted `［＃縦中横］` around small numeric markers in subtitles such as `幕間１（１）`

7. Final remaining ruby edge case
- The last diff was line 8834:
  - Rust output: `｜東雲さん《 ・ ・ ・ ・ 》`
  - reference: `※［＃縦線］東雲さん※［＃始め二重山括弧］ ・ ・ ・ ・ ※［＃終わり二重山括弧］`
- Root cause: explicit ruby with invalid spacing should not remain a normal ruby in narou.rb-compatible output
- Fix in `narou_ruby`:
  - detect explicit ruby `｜base《ruby》`
  - when ruby text starts with halfwidth space, or ends with 2+ halfwidth spaces, convert it to gaiji-style double-angle representation instead of preserving ruby
- This matches narou.rb spec behavior for invalid explicit-ruby spacing without regressing other spaced-ruby cases like `｜ストーカー《　木　戸　》`

## Investigation notes
- Broad auto-indent sentinel protection was tried and reverted because it caused regressions around border symbols and should not be reintroduced casually
- Blanket conversion of all spaced rubies to gaiji was also rejected because reference output still keeps cases like `｜ストーカー《　木　戸　》`
- The successful fix was a narrow invalid-explicit-ruby rule aligned with Ruby spec semantics

## Verification commands used
- `cargo check`
- `cargo run -- convert 2` (CWD: `sample/novel`)
- PowerShell line-by-line comparison between output and reference text
