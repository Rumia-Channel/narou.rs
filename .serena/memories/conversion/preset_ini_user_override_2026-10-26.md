# preset/ settings: user override vs bundled default (issue #45 follow-up, 2026-10-26)

- Problem: `preset/AozoraEpub3.ini` was shipped as *both* the bundled default and the
  file users edit to configure the built-in AozoraEpub3_Lite engine. The self-update
  replaces every extracted file, so one update silently reverted the user's edits
  (verified: the release zip contains `narou/preset/AozoraEpub3.ini` and
  `src/bin/updater.rs::copy_tree_overwrite` replaced it unconditionally).
- Fix: split the two roles.
  - Bundled default: `preset/AozoraEpub3.ini.default` and `preset/custom_chuki_tag.txt.default`.
  - User file: `preset/AozoraEpub3.ini` and `preset/custom_chuki_tag.txt`. **Not shipped**
    (`scripts/package-release.ps1` excludes them from `preset/`, `.github/workflows/build-linux.yml`
    copies only the `.default` files), and `src/bin/updater.rs` skips them when the
    destination already exists.
  - Runtime resolution order: user file → bundled default → embedded default.
    Shared definitions live in `src/preset.rs` (always compiled, no feature gate);
    `src/epub_lite.rs` and `src/commands/init.rs` both use it.
- Existing installs keep working: a user file that is already in `preset/` is never
  touched, so edits made against v0.5.2/v0.5.3 survive the update.
- Regression tests: `src/preset.rs` (resolution order), `src/epub_lite.rs`
  (user file wins over the bundled default), `src/bin/updater.rs` (update keeps the
  user file), `tests/convert_indent_settings.rs` (real binary honours both files).
