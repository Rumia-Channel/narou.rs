# Web auto-update scheduler status (2026-04-19)

- Rust Web auto-update no longer runs child `update` phases directly from the scheduler task. The scheduler now enqueues an `auto_update` job into `.narou/queue.yaml` via `JobType::AutoUpdate`, so it is serialized with normal Web queue work and appears in queue notifications.
- `update.auto-schedule.enable` / `update.auto-schedule` changes saved from the Web settings API now restart the scheduler immediately, matching Ruby's `Scheduler.stop` -> `Scheduler.start` behavior without requiring a Web server restart.
- Duplicate scheduled auto-update jobs are suppressed if an `AutoUpdate` job is already pending or running.
- `execute_auto_update` still performs the Ruby-compatible phase order: `update --gl narou` -> update `modified` tagged novels -> update non-narou-api novels. It now refreshes the Web process in-memory database after each child `update` phase, fixing stale `modified` tag collection after `--gl narou`.
- `COMMANDS.md` web section was updated to reflect queue-backed auto_update and scheduler restart behavior.
