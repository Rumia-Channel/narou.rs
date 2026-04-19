# Web auto-update scheduler status (2026-04-19)

- Rust Web auto-update no longer runs child `update` phases directly from the scheduler task. The scheduler now enqueues an `auto_update` job into `.narou/queue.yaml` via `JobType::AutoUpdate`, so it is serialized with normal Web queue work and appears in queue notifications.
- `update.auto-schedule.enable` / `update.auto-schedule` changes saved from the Web settings API now restart the scheduler immediately, matching Ruby's `Scheduler.stop` -> `Scheduler.start` behavior without requiring a Web server restart.
- Duplicate scheduled auto-update jobs are suppressed if an `AutoUpdate` job is already pending or running.
- `execute_auto_update` performs the Ruby-compatible phase order: `update --gl narou` -> update `modified` tagged novels -> update non-narou-api novels. It refreshes the Web process in-memory database after each child `update` phase, fixing stale `modified` tag collection after `--gl narou`.
- Auto-update child `update` phases now run with stdout/stderr piped and `NAROU_RS_WEB_MODE=1`. Plain output is relayed to the Web UI console via `PushServer::broadcast_echo`, and `__NAROU_WS__:` structured progress lines are forwarded with `broadcast_raw`. This fixes the regression where scheduled auto-update logs/progress appeared only in the server CLI console.
- Auto-update registers the currently running child `update` PID in `running_child_pids` for the job id, so the existing Web UI cancel endpoints can stop the active phase just like normal queued jobs.
- `COMMANDS.md` web section was updated to reflect queue-backed auto_update, scheduler restart behavior, phase DB refresh, Web console output relay, and cancellation PID registration.
- 2026-04-19追加: Web queue の `concurrency` 有効時 lane は外部通信あり(download/update/auto_update)を primary、その他(convert/send/backup/mail)を secondary に分離する。外部通信ありの出力は `#console`、その他は `#console-stdout2` へ送る。`concurrency` 無効時は `WorkerLane::All` で従来通り投入順に逐次実行する。
