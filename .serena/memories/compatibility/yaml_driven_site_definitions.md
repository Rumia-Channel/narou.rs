# YAML-driven site definition compatibility

- narou.rb compatibility requires site-specific fetching, preprocessing, and extraction behavior to be controlled primarily by `webnovel/*.yaml`.
- Runtime definitions merge by site name: user YAML overrides keys only when its version is at least the bundled version. Older copies are ignored at runtime; the management API can still show their raw text. Updating YAML (with the current version) must remain enough to follow site changes without rebuilding Rust.
- Rust internals may differ from Ruby, and Ruby execution is not required, but the meaning of YAML constructs must be represented by a configurable Rust-side model.

## Preprocessing DSL (since 2026-04-11)

Kakuyomu preprocessing is fully YAML-driven via a pest-based DSL:

- `src/downloader/preprocess.pest` — grammar definition (Ruby-like safe subset)
- `src/downloader/preprocess/{ast.rs,parser.rs,interpreter.rs}` — AST + pest parser + interpreter
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

### Required JSON and credential scope (2026-10-18, unreleased)
- `fetch_json_required(url)` propagates HTTP, invalid-JSON and literal-null failures. An explicit missing-status list such as `[404]` admits only those HTTP statuses as null; optional `request` / `fetch_json` still settle failures as null. Required constraints win for the same URL, and failure status/original error survive repeated cached reads. A dependency404 is not whole-novel deletion.
- `has_cookie` reports a configured Cookie header allowed for the input URL, including consent cookies; it is not evidence of login or permission.
- Extra JSON results have a 256-entry FIFO per novel and credential/header context. Each settled entry owns the parsed value, original decoded JSON text and any shared failure. Body reuse preserves original whitespace/key order instead of serializing Value. Eviction/reset drop all three together; forms remain call-local. No raw-data persistence is added on Worker.
- Missing-section preservation is a generic opt-in `preserve_missing_sections:true`, used by Pixiv YAML1.5. Nonempty readable lists may retain stable old chapter bodies; ambiguous matches and all-inaccessible results are canceled. Masking/permission checks, public-animation handling and pagination are YAML, not domain branches in Rust.

### Kakuyomu specific
- `kakuyomu.jp.yaml` has a `preprocess:` section that handles JSON extraction, Apollo `__ref` resolution, TOC formatting, and metadata emission
- The DSL is functionally equivalent to the old Ruby `code: eval:` but safe and interpretable by Rust
- Users can modify the DSL in `webnovel/kakuyomu.jp.yaml` to follow site structure changes

### For future sites
- Add a `preprocess: |-` section to any `webnovel/*.yaml` file
- The DSL supports the same operations needed for most JSON-embedded sites
- If the DSL is insufficient, extend `preprocess.pest` and the AST/parser/interpreter under `preprocess/` with site-independent operations

## Hameln tag extraction (2026-08-02)

- `webnovel/syosetu.org.yaml` version 1.8 defines both detail-table and TOC tag patterns.
- The detail-table pattern extracts both normal `タグ` and `必須タグ` rows without crossing into `掲載開始`.
- The TOC fallback stops at the first `<br>` or `<hr>`, preventing short-story synopsis/body HTML from becoming tags.
- Site-specific behavior remains entirely in YAML; Rust changes are regression fixtures only.
