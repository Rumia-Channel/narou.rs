# YAML-driven site definition compatibility

- narou.rb compatibility requires site-specific fetching, preprocessing, and extraction behavior to be controlled primarily by `webnovel/*.yaml`.
- The initialized narou root's `webnovel/*.yaml` must override bundled defaults. Users should be able to edit those YAML files to follow site changes without rebuilding Rust code.
- Rust internals may differ from Ruby, and Ruby execution is not required, but the meaning of YAML constructs must be represented by a configurable Rust-side model.

## Preprocessing DSL (since 2026-04-11)

Kakuyomu preprocessing is fully YAML-driven via a pest-based DSL:

- `src/downloader/preprocess.pest` — grammar definition (Ruby-like safe subset)
- `src/downloader/preprocess.rs` — AST + pest parser + interpreter
- Site YAML files use a `preprocess: |-` field containing the DSL script
- `SiteSetting` compiles the DSL in `compile()`, cached as `PreprocessPipeline`
- `pretreatment_source()` executes the pipeline when present
- Old `kakuyomu_preprocess()` hardcoded function and `eval_kakuyomu()` method are removed

### DSL features
- Statements: `guard`, `let`, `set`, `if`/`else`/`end`, `for`/`in`/`end`, `emit`, `insert_at_match`
- Expressions: string interpolation (`${expr}`), accessor chains (`.field`, `["key"]`), method chains (`.map`, `.flat_map`, `.flatten`, `.compact`, `.join`, `.gsub`, `.replace`, `.is_array`, `.empty`)
- `extract_json(/regex/flags)` — extract and parse JSON from HTML
- Boolean operators: `&&`, `||`, `==`, `!=`, `!`
- Comments: `# ...`

### Kakuyomu specific
- `kakuyomu.jp.yaml` has a `preprocess:` section that handles JSON extraction, Apollo `__ref` resolution, TOC formatting, and metadata emission
- The DSL is functionally equivalent to the old Ruby `code: eval:` but safe and interpretable by Rust
- Users can modify the DSL in `webnovel/kakuyomu.jp.yaml` to follow site structure changes

### For future sites
- Add a `preprocess: |-` section to any `webnovel/*.yaml` file
- The DSL supports the same operations needed for most JSON-embedded sites
- If the DSL is insufficient, extend `preprocess.pest` and `preprocess.rs` with new operations
