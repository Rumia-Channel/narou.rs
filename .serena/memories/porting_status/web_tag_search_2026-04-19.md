# Web UI Tag Editing/Search Parity (2026-04-19)

- Rust Web UI tag editor now shows completion candidates from the existing tag list (`/api/tag_list?format=json`) while typing in `#new-tag-input`.
- Candidate matching is intentionally limited to existing tag text matching: exact, prefix, and substring after NFKC + kana normalization. It does not embed kanji reading inference or dictionary mappings.
- Tag input accepts multiple whitespace-separated tags and posts them as one `tags` array to the existing tag add API.
- Novel list clicks now build structured filter tokens:
  - tag labels: `tag:<name>`
  - author names: `author:<name>`
  - site names: `sitename:<name>`
- Modifier behavior follows narou.rb-style tag search semantics and applies to tag/author/site filters:
  - normal click: AND token append
  - Ctrl click: OR merge into the latest same field/sign token with `|`
  - Shift click: exclusion AND token append
  - Shift+Ctrl click: exclusion OR merge
- Client filter parser was adjusted to split `|` values while respecting quoted field values.