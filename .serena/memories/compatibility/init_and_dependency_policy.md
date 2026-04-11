# narou.rb Compatibility Policy: init, webnovel YAML, dependencies

- This project is a Rust compatibility port of upstream narou.rb under `sample/narou`.
- Rust internals, libraries, data structures, and algorithms may differ from Ruby, but externally visible behavior must match narou.rb as much as possible.
- Compatibility targets include CLI/API arguments and behavior, YAML syntax handling (`webnovel/*.yaml`, `converter.yaml`, etc.), `.narou/` inventory file reads/writes, output directory structure, and generated text files.
- User-initialized `webnovel/*.yaml` under the working narou root must be preferred over bundled YAML, allowing users to freely modify site definitions like narou.rb. Bundled `webnovel/*.yaml` is a fallback/source for initial copies.
- `narou init` should follow narou.rb reference behavior:
  - create `.narou/` and `小説データ/` for a new root;
  - create/copy a user-editable `webnovel/` directory;
  - keep `.narou/local_setting.yaml` and other inventory YAML empty unless user settings exist, because narou.rb generally applies defaults at read sites;
  - when `narou init` is run from an interactive terminal, ask for the AozoraEpub3 directory and line height like narou.rb; in non-interactive environments, do not block for input and skip when no existing setting is available;
  - create Ruby-compatible inventory files such as `database.yaml`, `database_index.yaml`, `alias.yaml`, `freeze.yaml`, `tag_colors.yaml`, `latest_convert.yaml`, `queue.yaml`, and `notepad.txt` when missing;
  - only save global AozoraEpub3 settings when the configured directory contains `AozoraEpub3.jar`; `-p :keep` should reuse an existing valid global path; line-height defaults to 1.8 only when AozoraEpub3 settings are actually saved;
  - when a valid AozoraEpub3 path is configured, rewrite the same AozoraEpub3 support files as narou.rb: append/replace custom `chuki_tag.txt`, copy `AozoraEpub3.ini`, and render/copy `template/OPS/css_custom/vertical_font.css` with the configured line height.
- Do not edit `Cargo.toml` directly for dependency additions/updates. Use Cargo commands such as `cargo add` or `cargo update` to obtain current compatible crate versions. Direct manual edits require an explicit reason and validation with `cargo check` or stronger tests.
