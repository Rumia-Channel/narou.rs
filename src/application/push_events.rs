//! WebSocket イベントのペイロード組み立て (native / Worker で共有)。
//!
//! `src/web/push.rs` (native `PushServer`) と `worker_entry/src/push_hub.rs`
//! (Worker `PushHub`) が送る `{ "type": ..., ... }` 形式の JSON をここで
//! 1 箇所に定義する。フロント (`src/web/assets/js/main.js` の
//! `handleWsMessage`) が読むキーは `type` / `data` / `body` /
//! `target_console` と `data` 内の `topic` / `percent` / `current` /
//! `total` / `scope` / `job_id` / `reason` / `detail` — キー名・値の型・
//! 省略条件は native が正とする形に固定し、テストで凍結する。
//!
//! ここでは値を作るだけで、送信・履歴への積み方・コンソール振り分けは
//! 各ランタイム側の責務 (`publish_json` / `PushHubClient::broadcast`)。

use serde_json::{json, Value};

/// `PushServer::broadcast` / `broadcast_event` 相当の制御イベント。
/// `data` は native では常に文字列 (`&str`) だが、Worker 側の queue_* 系が
/// 構造化 data を使うため `Into<Value>` を受ける。
pub fn event(event_type: &str, data: impl Into<Value>) -> Value {
    let data = data.into();
    json!({
        "type": event_type,
        "data": data,
    })
}

/// `broadcast_echo` 相当のコンソール行イベント。
/// `main.js` は `msg.body` と `msg.target_console` を読む。
pub fn echo(body: &str, target_console: &str) -> Value {
    json!({
        "type": "echo",
        "body": body,
        "target_console": target_console,
    })
}

/// `broadcast_progress` 相当 (`type: "progress"`)。
pub fn progress(current: usize, total: usize, message: &str) -> Value {
    json!({
        "type": "progress",
        "current": current,
        "total": total,
        "message": message,
    })
}

/// `broadcast_log` 相当 (`type: "log"`)。
pub fn log(level: &str, message: &str) -> Value {
    json!({
        "type": "log",
        "level": level,
        "message": message,
    })
}

/// `broadcast_error` 相当 (`broadcast("error", message)`)。
pub fn error(message: &str) -> Value {
    event("error", message)
}

/// `broadcast_progressbar_init_to` 相当。
pub fn progressbar_init(topic: &str, target_console: &str) -> Value {
    json!({
        "type": "progressbar.init",
        "data": { "topic": topic },
        "target_console": target_console,
    })
}

/// `broadcast_progressbar_step_to` 相当。
pub fn progressbar_step(percent: f64, topic: &str, target_console: &str) -> Value {
    json!({
        "type": "progressbar.step",
        "data": { "percent": percent, "topic": topic },
        "target_console": target_console,
    })
}

/// `broadcast_progressbar_clear_to` 相当。
pub fn progressbar_clear(topic: &str, target_console: &str) -> Value {
    json!({
        "type": "progressbar.clear",
        "data": { "topic": topic },
        "target_console": target_console,
    })
}

/// `WebProgress` (`src/progress.rs`) / Worker `HubProgress` が送る
/// `progressbar.init`。`data.scope` は job 単位の消去
/// (`progressbar_scope_clear`) と対応する。
pub fn progressbar_init_scoped(topic: &str, scope: &str) -> Value {
    json!({
        "type": "progressbar.init",
        "data": { "topic": topic, "scope": scope },
    })
}

/// `WebProgress` / `HubProgress` の `progressbar.step`。
/// こちらは data に `current` / `total` も載る (トップレベルの
/// `target_console` は native では stdout 中継時に付けられる)。
pub fn progressbar_step_scoped(
    current: u64,
    total: u64,
    percent: f64,
    topic: &str,
    scope: &str,
) -> Value {
    json!({
        "type": "progressbar.step",
        "data": {
            "current": current,
            "total": total,
            "percent": percent,
            "topic": topic,
            "scope": scope,
        },
    })
}

/// `WebProgress` / `HubProgress` の `progressbar.clear` (topic+scope 指定)。
pub fn progressbar_clear_scoped(topic: &str, scope: &str) -> Value {
    json!({
        "type": "progressbar.clear",
        "data": { "topic": topic, "scope": scope },
    })
}

/// native `clear_progress_for_job` (`src/web/worker.rs`) 相当の
/// スコープ単位クリア。`data.scope` が job id、`target_console` が表示先。
pub fn progressbar_scope_clear(scope: &str, target_console: &str) -> Value {
    json!({
        "type": "progressbar.clear",
        "data": { "scope": scope },
        "target_console": target_console,
    })
}

/// `notification.queue` — キュー表示の再読込トリガ。
pub fn notification_queue() -> Value {
    event("notification.queue", "")
}

/// `table.reload` — 一覧の再読込トリガ。
pub fn table_reload() -> Value {
    event("table.reload", "")
}

/// `tag.updateCanvas` — タグ表示の再読込トリガ。
pub fn tag_update_canvas() -> Value {
    event("tag.updateCanvas", "")
}

/// ジョブ開始イベント。`data` は job id 文字列。
pub fn queue_start(job_id: &str) -> Value {
    event("queue_start", job_id)
}

/// ジョブ完了イベント。`data` は job id 文字列。
pub fn queue_complete(job_id: &str) -> Value {
    event("queue_complete", job_id)
}

/// ジョブ取消イベント。`data` は `{ "job_id": ... }`。
pub fn queue_cancelled(job_id: &str) -> Value {
    event("queue_cancelled", json!({ "job_id": job_id }))
}

/// 部分完了イベント。`data` は `{ "job_id": ... }` に、native は
/// `exit_code`、Worker は `reason` を載せる (両方 `None` なら `job_id`
/// のみ — 省略条件は呼び出し側の引数で決まる)。
pub fn queue_partial(job_id: &str, exit_code: Option<i32>, reason: Option<&str>) -> Value {
    let mut data = json!({ "job_id": job_id });
    if let Some(code) = exit_code {
        data["exit_code"] = json!(code);
    }
    if let Some(reason) = reason {
        data["reason"] = json!(reason);
    }
    event("queue_partial", data)
}

/// リトライ予定イベント。`data` のキー・型は native のジョブループ
/// (`src/web/worker.rs`) が送るものと一致させる。
pub fn queue_retry(
    job_id: &str,
    retry_count: u32,
    max_retries: u32,
    backoff_secs: i64,
    available_at: i64,
    reason: &str,
) -> Value {
    event(
        "queue_retry",
        json!({
            "job_id": job_id,
            "retry_count": retry_count,
            "max_retries": max_retries,
            "backoff_secs": backoff_secs,
            "available_at": available_at,
            "reason": reason,
        }),
    )
}

/// ジョブ失敗イベント。`detail` は `webui.debug-mode` が ON のときだけ
/// 呼び出し側が `Some` を渡す (省略時はキー自体を出さない)。
pub fn queue_failed(job_id: &str, reason: &str, detail: Option<&str>) -> Value {
    let mut data = json!({
        "job_id": job_id,
        "reason": reason,
    });
    if let Some(detail) = detail {
        data["detail"] = Value::String(detail.to_string());
    }
    event("queue_failed", data)
}

/// `library_backup.done` — native のライブラリバックアップ完了通知。
/// `main.js` は `msg.data.path` を読むので `data` はオブジェクトで送る
/// (`src/web/library_backup.rs` が emit、Worker 側に同等機能は無い)。
pub fn library_backup_done(path: &str) -> Value {
    event("library_backup.done", json!({ "path": path }))
}

/// `library_backup.failed` — 同失敗通知。`main.js` は `msg.data.message`
/// を読む。
pub fn library_backup_failed(message: &str) -> Value {
    event("library_backup.failed", json!({ "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 各イベントの wire 形状を凍結する。ここが native
    // (`src/web/push.rs` / `src/progress.rs` / `src/web/worker.rs`) と
    // Worker (`worker_entry/src/push_hub.rs` / `consumer.rs`) の共通
    // ソースなので、同じ入力から両者が同じ JSON を得ることの証明になる。
    // `main.js` が読むキー (`body` / `target_console` / `data.*`) は
    // リテラル比較で漏れなく固定される。

    #[test]
    fn event_with_str_data_matches_broadcast_event() {
        assert_eq!(
            event("table.reload", ""),
            json!({ "type": "table.reload", "data": "" })
        );
        assert_eq!(
            event("custom", "payload"),
            json!({ "type": "custom", "data": "payload" })
        );
    }

    #[test]
    fn event_with_structured_data_matches_worker_shape() {
        assert_eq!(
            event("queue_start", json!("job-1")),
            json!({ "type": "queue_start", "data": "job-1" })
        );
        assert_eq!(
            queue_start("job-1"),
            json!({ "type": "queue_start", "data": "job-1" })
        );
    }

    #[test]
    fn library_backup_events_carry_object_data() {
        // main.js は `msg.data.path` / `msg.data.message` を読むので
        // `data` は JSON 文字列ではなくオブジェクトでなければならない。
        assert_eq!(
            library_backup_done("C:\\backups\\narou-backup-1.zip"),
            json!({
                "type": "library_backup.done",
                "data": { "path": "C:\\backups\\narou-backup-1.zip" }
            })
        );
        assert_eq!(
            library_backup_failed("disk full"),
            json!({
                "type": "library_backup.failed",
                "data": { "message": "disk full" }
            })
        );
    }

    #[test]
    fn echo_event_shape() {
        assert_eq!(
            echo("line", "stdout2"),
            json!({ "type": "echo", "body": "line", "target_console": "stdout2" })
        );
    }

    #[test]
    fn progress_and_log_event_shapes() {
        assert_eq!(
            progress(2, 5, "working"),
            json!({ "type": "progress", "current": 2, "total": 5, "message": "working" })
        );
        assert_eq!(
            log("warn", "message"),
            json!({ "type": "log", "level": "warn", "message": "message" })
        );
        assert_eq!(
            error("boom"),
            json!({ "type": "error", "data": "boom" })
        );
    }

    #[test]
    fn progressbar_events_match_push_server_shape() {
        // PushServer::broadcast_progressbar_*_to が出す形。
        assert_eq!(
            progressbar_init("convert", "stdout"),
            json!({
                "type": "progressbar.init",
                "data": { "topic": "convert" },
                "target_console": "stdout",
            })
        );
        assert_eq!(
            progressbar_step(50.0, "convert", "stdout2"),
            json!({
                "type": "progressbar.step",
                "data": { "percent": 50.0, "topic": "convert" },
                "target_console": "stdout2",
            })
        );
        assert_eq!(
            progressbar_clear("convert", "stdout"),
            json!({
                "type": "progressbar.clear",
                "data": { "topic": "convert" },
                "target_console": "stdout",
            })
        );
    }

    #[test]
    fn scoped_progressbar_events_match_web_progress_shape() {
        // WebProgress / HubProgress が出す形 (scope 付き、target_console 無し)。
        assert_eq!(
            progressbar_init_scoped("convert", "job-1"),
            json!({
                "type": "progressbar.init",
                "data": { "topic": "convert", "scope": "job-1" },
            })
        );
        assert_eq!(
            progressbar_step_scoped(1, 4, 25.0, "download", "job-1"),
            json!({
                "type": "progressbar.step",
                "data": {
                    "current": 1,
                    "total": 4,
                    "percent": 25.0,
                    "topic": "download",
                    "scope": "job-1",
                },
            })
        );
        assert_eq!(
            progressbar_clear_scoped("convert", "job-1"),
            json!({
                "type": "progressbar.clear",
                "data": { "topic": "convert", "scope": "job-1" },
            })
        );
    }

    #[test]
    fn progressbar_scope_clear_matches_clear_progress_for_job() {
        assert_eq!(
            progressbar_scope_clear("job-1", "stdout2"),
            json!({
                "type": "progressbar.clear",
                "data": { "scope": "job-1" },
                "target_console": "stdout2",
            })
        );
    }

    #[test]
    fn refresh_triggers_match_broadcast_event_shape() {
        assert_eq!(
            notification_queue(),
            json!({ "type": "notification.queue", "data": "" })
        );
        assert_eq!(
            table_reload(),
            json!({ "type": "table.reload", "data": "" })
        );
        assert_eq!(
            tag_update_canvas(),
            json!({ "type": "tag.updateCanvas", "data": "" })
        );
    }

    #[test]
    fn queue_complete_and_cancelled_match_native_shape() {
        assert_eq!(
            queue_complete("job-1"),
            json!({ "type": "queue_complete", "data": "job-1" })
        );
        assert_eq!(
            queue_cancelled("job-1"),
            json!({ "type": "queue_cancelled", "data": { "job_id": "job-1" } })
        );
    }

    #[test]
    fn queue_partial_matches_native_and_worker_shapes() {
        // native: job_id のみ / exit_code 付き。
        assert_eq!(
            queue_partial("job-1", None, None),
            json!({ "type": "queue_partial", "data": { "job_id": "job-1" } })
        );
        assert_eq!(
            queue_partial("job-1", Some(3), None),
            json!({
                "type": "queue_partial",
                "data": { "job_id": "job-1", "exit_code": 3 },
            })
        );
        // Worker: reason 付き。
        assert_eq!(
            queue_partial("job-1", None, Some("budget")),
            json!({
                "type": "queue_partial",
                "data": { "job_id": "job-1", "reason": "budget" },
            })
        );
    }

    #[test]
    fn queue_retry_matches_native_shape() {
        assert_eq!(
            queue_retry("job-1", 2, 3, 300, 1_700_000_000, "network error"),
            json!({
                "type": "queue_retry",
                "data": {
                    "job_id": "job-1",
                    "retry_count": 2,
                    "max_retries": 3,
                    "backoff_secs": 300,
                    "available_at": 1_700_000_000_i64,
                    "reason": "network error",
                },
            })
        );
    }

    #[test]
    fn queue_failed_omits_detail_unless_present() {
        assert_eq!(
            queue_failed("job-1", "boom", None),
            json!({
                "type": "queue_failed",
                "data": { "job_id": "job-1", "reason": "boom" },
            })
        );
        assert_eq!(
            queue_failed("job-1", "boom", Some("full detail")),
            json!({
                "type": "queue_failed",
                "data": {
                    "job_id": "job-1",
                    "reason": "boom",
                    "detail": "full detail",
                },
            })
        );
    }

    #[test]
    fn serialized_payload_carries_main_js_keys() {
        // main.js の handleWsMessage が参照するキーがシリアライズ済み
        // payload に残ること。
        let payload = echo("text", "stdout").to_string();
        assert!(payload.contains("\"body\":\"text\""));
        assert!(payload.contains("\"target_console\":\"stdout\""));
        let payload = progressbar_init_scoped("convert", "job-1").to_string();
        assert!(payload.contains("\"topic\":\"convert\""));
        assert!(payload.contains("\"scope\":\"job-1\""));
    }
}
