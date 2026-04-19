# update last_update / Web new-update marker fix (2026-04-19)

- Web UI `new-update` follows narou.rb frontend semantics: the last_update cell is marked when `last_update` is within the 6-hour annotation window, unless `new_arrivals_date` qualifies for `new-arrivals`.
- Root cause of non-Narou sites staying `new-update`: Rust `Downloader::download_novel` rewrote existing records' `last_update` to `Utc::now()` even when `UpdateStatus::None` (no actual update). `update` then also set `last_check_date`, so merely checking a non-Narou/R18 site refreshed `last_update` and made the Web UI show `new-update` again.
- Fix: compute `UpdateStatus` before DB merge and only replace existing `last_update` when status is `Ok` (new novel, force, changed sections, metadata/story changes, or deleted sections). `UpdateStatus::None` now preserves the existing `last_update`; `last_check_date` still records the check time in `commands::update`.
- Added unit coverage via `no_update_preserves_last_update_timestamp` alongside the forced-redownload status test.
