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

use std::collections::{BTreeMap, VecDeque};
use serde::{Deserialize, Serialize};
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

/// Complete snapshots are also replayable when an operation's init has fallen
/// out of console history. Job scope and operation identity remain distinct.
pub fn progress_update(
    phase: crate::platform::progress::ProgressPhase,
    identity: &crate::platform::progress::ProgressIdentity,
    snapshot: &crate::platform::progress::ProgressSnapshot,
) -> Value {
    use crate::platform::progress::ProgressPhase;
    event(
        match phase {
            ProgressPhase::Init => "progressbar.init",
            ProgressPhase::Step => "progressbar.step",
            ProgressPhase::Clear => "progressbar.clear",
        },
        json!({
            "topic": identity.topic,
            "scope": identity.scope,
            "operation": identity.operation,
            "sequence": snapshot.sequence,
            "current": if snapshot.total == 0 { snapshot.current } else { snapshot.current.min(snapshot.total) },
            "total": snapshot.total,
            "percent": snapshot.percent(),
            "message": snapshot.message,
        }),
    )
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

/// Latest progress lives independently of bounded console history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProgressReplay {
    #[serde(default)]
    active: BTreeMap<String, Value>,
    #[serde(default)]
    closed_operations: VecDeque<String>,
    #[serde(default)]
    closed_scopes: VecDeque<String>,
}

const RECENT_PROGRESS_COMPLETIONS: usize = 512;

fn progress_key(value: &Value) -> String {
    let data = &value["data"];
    serde_json::to_string(&[
        value["target_console"].as_str().unwrap_or("stdout"),
        data["scope"].as_str().unwrap_or(""),
        data["topic"].as_str().unwrap_or("default"),
        data["operation"].as_str().unwrap_or(""),
    ]).expect("string tuple is JSON serializable")
}

fn remember_completion(recent: &mut VecDeque<String>, key: String) {
    if recent.contains(&key) { return; }
    recent.push_back(key);
    if recent.len() > RECENT_PROGRESS_COMPLETIONS { recent.pop_front(); }
}

impl ProgressReplay {
    pub fn snapshots(&self) -> impl Iterator<Item = &Value> {
        self.active.values()
    }

    /// Stale packets cannot rewind or revive a finished job/attempt.
    /// The return value tells persistent adapters whether anything changed.
    pub fn observe(&mut self, value: Value) -> bool {
        let kind = value["type"].as_str().unwrap_or("");
        let data = &value["data"];
        let scope = data["scope"].as_str().unwrap_or("");
        match kind {
            "progressbar.init" | "progressbar.step" => {
                if !scope.is_empty() && self.closed_scopes.iter().any(|closed| closed == scope) {
                    return false;
                }
                let key = progress_key(&value);
                if self.closed_operations.contains(&key) { return false; }
                if let Some(previous) = self.active.get(&key)
                    && let (Some(old), Some(new)) = (
                        previous["data"]["sequence"].as_u64(), data["sequence"].as_u64(),
                    )
                    && new <= old
                { return false; }
                self.active.insert(key, value);
                true
            }
            "progressbar.clear" => {
                let console = value["target_console"].as_str();
                if data["topic"].as_str().is_none() && !scope.is_empty() {
                    self.clear_scope(scope, console);
                    return true;
                }
                let key = progress_key(&value);
                if let Some(previous) = self.active.get(&key)
                    && let (Some(old), Some(new)) = (
                        previous["data"]["sequence"].as_u64(), data["sequence"].as_u64(),
                    )
                    && new < old
                { return false; }
                self.active.remove(&key);
                if data["operation"].as_str().is_some_and(|id| !id.is_empty()) {
                    remember_completion(&mut self.closed_operations, key);
                }
                true
            }
            "queue_start" => {
                let job = queue_job_id(data);
                if job.is_empty() { return false; }
                let was_closed = self.closed_scopes.iter().any(|scope| scope == job);
                let had_active = self.active.values().any(|value| value["data"]["scope"].as_str() == Some(job));
                if had_active { self.clear_scope(job, None); }
                self.closed_scopes.retain(|scope| scope != job);
                had_active || was_closed
            }
            "queue_complete" | "queue_failed" | "queue_retry" | "queue_partial"
            | "queue.partial" | "queue_cancelled" | "queue.cancelled" => {
                let job = queue_job_id(data);
                if job.is_empty() { return false; }
                self.clear_scope(job, None);
                true
            }
            _ => false,
        }
    }

    fn clear_scope(&mut self, scope: &str, console: Option<&str>) {
        let recent = &mut self.closed_operations;
        self.active.retain(|key, value| {
            let matches = value["data"]["scope"].as_str() == Some(scope)
                && console.is_none_or(|console| {
                    value["target_console"].as_str().unwrap_or("stdout") == console
                });
            if matches && value["data"]["operation"].as_str().is_some_and(|id| !id.is_empty()) {
                remember_completion(recent, key.clone());
            }
            !matches
        });
        remember_completion(&mut self.closed_scopes, scope.to_owned());
    }
}

fn queue_job_id(data: &Value) -> &str {
    data.as_str().or_else(|| data["job_id"].as_str()).unwrap_or("")
}

pub fn history_replayable(message_type: &str) -> bool {
    matches!(message_type, "echo" | "error" | "log" | "progress")
}

pub fn message_scope(value: &Value) -> Option<&str> {
    value["scope"].as_str().or_else(|| value["data"]["scope"].as_str())
}

pub fn history_target_console(value: &Value) -> Option<&str> {
    value["target_console"].as_str().or_else(|| {
        matches!(value["type"].as_str(), Some("echo" | "error" | "log")).then_some("stdout")
    })
}

pub fn history_body(value: &Value) -> Option<&str> {
    match value["type"].as_str()? {
        "echo" => value["body"].as_str(),
        "error" => value["data"].as_str().or_else(|| value["message"].as_str()),
        "log" => value["message"].as_str().or_else(|| value["text"].as_str()),
        "progress" => value["message"].as_str(),
        _ => None,
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn step(job: &str, operation: &str, current: u64, sequence: u64) -> Value {
        event("progressbar.step", json!({
            "scope": job, "topic": "download", "operation": operation,
            "current": current, "total": 10, "percent": current * 10,
            "sequence": sequence,
        }))
    }

    #[test]
    fn replay_keeps_parallel_operations_and_survives_persistence() {
        let mut replay = ProgressReplay::default();
        replay.observe(step("job", "first", 2, 1));
        replay.observe(step("job", "second", 3, 1));
        let saved = serde_json::to_string(&replay).unwrap();
        let restored: ProgressReplay = serde_json::from_str(&saved).unwrap();
        let positions: Vec<_> = restored.snapshots().map(|value| value["data"]["current"].as_u64().unwrap()).collect();
        assert_eq!(positions, vec![2, 3]);
    }

    #[test]
    fn old_steps_cannot_rewind_or_resurrect_finished_operations() {
        let mut replay = ProgressReplay::default();
        replay.observe(step("job", "first", 5, 3));
        assert!(!replay.observe(step("job", "first", 1, 1)));
        assert_eq!(replay.snapshots().next().unwrap()["data"]["current"], 5);
        replay.observe(event("progressbar.clear", json!({
            "scope": "job", "topic": "download", "operation": "first", "sequence": 4,
        })));
        assert!(!replay.observe(step("job", "first", 7, 5)));
        assert_eq!(replay.snapshots().count(), 0);
    }

    #[test]
    fn retry_starts_a_new_attempt_without_accepting_the_old_attempt() {
        let mut replay = ProgressReplay::default();
        replay.observe(step("job", "old", 2, 1));
        replay.observe(queue_retry("job", 1, 3, 10, 10, "network"));
        assert!(!replay.observe(step("job", "old", 3, 2)));
        replay.observe(queue_start("job"));
        assert!(replay.observe(step("job", "new", 1, 1)));
        assert!(!replay.observe(step("job", "old", 3, 2)));
        assert_eq!(replay.snapshots().next().unwrap()["data"]["operation"], "new");
    }
    #[test]
    fn reclaimed_attempt_retires_abandoned_operations_but_not_other_jobs() {
        let mut replay = ProgressReplay::default();
        replay.observe(step("job", "abandoned", 2, 1));
        replay.observe(step("peer", "parallel", 5, 1));
        assert!(replay.observe(queue_start("job")));
        assert!(replay.observe(step("job", "resumed", 3, 1)));
        assert!(!replay.observe(step("job", "abandoned", 4, 2)));
        let restored: ProgressReplay = serde_json::from_str(&serde_json::to_string(&replay).unwrap()).unwrap();
        let operations: Vec<_> = restored.snapshots().map(|value| value["data"]["operation"].as_str().unwrap()).collect();
        assert_eq!(operations, vec!["resumed", "parallel"]);
    }

    #[test]
    fn cancellation_removes_durable_progress_and_rejects_late_updates() {
        let mut replay = ProgressReplay::default();
        replay.observe(step("job", "running", 2, 1));
        replay.observe(queue_cancelled("job"));
        let mut restored: ProgressReplay = serde_json::from_str(&serde_json::to_string(&replay).unwrap()).unwrap();
        assert_eq!(restored.snapshots().count(), 0);
        assert!(!restored.observe(step("job", "running", 3, 2)));
    }

}

