//! Queue consumer (Phase 8).
//!
//! One worker invocation consumes one or more envelopes. Each envelope is
//! claimed against the D1 ledger, executed (Download/Update via the shared
//! Downloader — one fresh instance per invocation, never a mutable
//! Downloader shared across concurrent executions), and then:
//!
//! - durable terminal state (`succeeded` / `partial` / `blocked` /
//!   `permanent`) is written **before** `ack`
//! - `retryable` outcomes record an attempt and re-queue with a bounded
//!   backoff (`retry_with_options`); the ledger is never blindly retried to
//!   success (attempt cap → durable `permanent`)
//! - unsupported versions / malformed payloads are durably ledgered as
//!   `blocked` and acked — never silently dropped
//!
//! A panic or a ledger failure propagates to the queue runtime (redelivery /
//! DLQ); there is no `catch_unwind` and no silent acknowledgment.

use narou_rs::application::{
    JobKind,
    decode_legacy_envelope, JobClaim, JobId, JobLedgerStatus, JobPlan, JobQueue, JobRequest,
    LegacyEnvelopeOutcome, WorkerJobEnvelope, WORKER_JOB_ENVELOPE_VERSION,
};
use narou_rs::downloader::Downloader;
use narou_rs::error::{NarouError, Result};
use serde_json::json;
use worker::{console_log, Env, Message, MessageBatch, MessageExt, QueueRetryOptionsBuilder};

use crate::composition::WorkerRuntime;
use crate::executor::{execute_job, JobOutcome};
use narou_rs::application::messages;
use narou_rs::application::retry_policy::{self, RetryPolicy};

use crate::push_hub::{
    broadcast_terminal_events, echo, event, notification_queue, PushHubClient,
};

/// Queue consumer におけるイベント配信。ジョブの台帳が変わるたびに
/// (reject / 再キュー追加) キュー表示を更新させる。
async fn notify_queue_changed(push: &PushHubClient) {
    push.broadcast_best_effort(&[notification_queue()]).await;
}

/// Process a queue batch. Builds a fresh runtime and Downloader per
/// invocation so no mutable state is shared across event handlers.
pub async fn process_batch(
    message_batch: MessageBatch<serde_json::Value>,
    env: &Env,
) -> Result<()> {
    let runtime = WorkerRuntime::build(env).await.map_err(worker_error)?;
    let push = PushHubClient::new(env, runtime.subrequests.clone());
    let mut downloader = runtime.new_downloader().await?;
    for message in message_batch.messages().map_err(worker_error)? {
        process_envelope(&runtime, &mut downloader, &push, message.body(), &message).await?;
    }
    Ok(())
}

/// 台帳へ reject を書き込んだあと、キュー表示の更新を通知する
/// (best-effort — 失敗しても処理は止めない)。
async fn notify_rejected(push: &PushHubClient, message_id: &str, reason: &str) {
    push.broadcast_best_effort(&[
        echo(&format!("キュー投入を拒否しました ({message_id}): {reason}"), "stdout"),
        notification_queue(),
    ])
    .await;
}

async fn process_envelope(
    runtime: &WorkerRuntime,
    downloader: &mut Downloader,
    push: &PushHubClient,
    value: &serde_json::Value,
    message: &Message<serde_json::Value>,
) -> Result<()> {
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    match version {
        v if v == u64::from(WORKER_JOB_ENVELOPE_VERSION) => {
            let envelope: WorkerJobEnvelope = match serde_json::from_value(value.clone()) {
                Ok(envelope) => envelope,
                Err(error) => {
                    // Malformed payloads are poison: they would never parse
                    // again on redelivery, so durably block + ack instead of
                    // spinning. `Err` is reserved for ledger/runtime
                    // failures, which are retried.
                    let reason = format!("malformed v2 envelope: {error}");
                    console_log!("rejecting envelope {}: {reason}", message.id());
                    runtime.ledger.record_rejected(&message.id(), &reason).await?;
                    notify_rejected(push, &message.id(), &reason).await;
                    message.ack();
                    return Ok(());
                }
            };
            // Defense in depth: an envelope that somehow exceeds the payload
            // limit is durably blocked, never executed.
            let oversized = match narou_rs::application::envelope_bytes(&envelope) {
                Ok(size) => size > narou_rs::application::job_limits::MAX_ENVELOPE_BYTES,
                Err(_) => true,
            };
            if oversized {
                let reason = format!(
                    "envelope exceeds queue payload limit (max {} bytes)",
                    narou_rs::application::job_limits::MAX_ENVELOPE_BYTES
                );
                console_log!("rejecting envelope {}: {reason}", message.id());
                runtime.ledger.record_rejected(&message.id(), &reason).await?;
                notify_rejected(push, &message.id(), &reason).await;
                message.ack();
                return Ok(());
            }
            // 計画は台帳が持つ。旧形式 (メッセージに計画を積んだもの) だけ
            // そのまま使う。
            let plan = match envelope.job {
                Some(plan) => plan,
                None => match runtime.ledger.get(&envelope.job_id).await? {
                    Some(view) => view.job,
                    None => {
                        let reason =
                            format!("ledger has no row for queued job {}", envelope.job_id);
                        console_log!("rejecting envelope {}: {reason}", message.id());
                        runtime.ledger.record_rejected(&message.id(), &reason).await?;
                        notify_rejected(push, &message.id(), &reason).await;
                        message.ack();
                        return Ok(());
                    }
                },
            };
            process_discrete(runtime, downloader, push, envelope.job_id, &plan, message).await
        }
        1 => {
            // Legacy envelope: one `JobRequest` body. Only a request that
            // plans exactly one unambiguous job is promoted; everything else
            // is durably ledgered as blocked and acked.
            let legacy = value.get("job").cloned().unwrap_or(serde_json::Value::Null);
            let request: JobRequest = match serde_json::from_value(legacy) {
                Ok(request) => request,
                Err(error) => {
                    let reason = format!("malformed v1 envelope: {error}");
                    console_log!("rejecting envelope {}: {reason}", message.id());
                    runtime.ledger.record_rejected(&message.id(), &reason).await?;
                    notify_rejected(push, &message.id(), &reason).await;
                    message.ack();
                    return Ok(());
                }
            };
            match decode_legacy_envelope(&request) {
                LegacyEnvelopeOutcome::Plan(job) => {
                    let queued = runtime.ledger.enqueue(job).await?;
                    notify_queue_changed(push).await;
                    process_discrete(runtime, downloader, push, queued.job_id, &queued.job, message).await
                }
                LegacyEnvelopeOutcome::Reject { reason } => {
                    console_log!("rejecting legacy envelope {}: {reason}", message.id());
                    runtime.ledger.record_rejected(&message.id(), &reason).await?;
                    notify_rejected(push, &message.id(), &reason).await;
                    message.ack();
                    Ok(())
                }
            }
        }
        other => {
            let reason = format!("unsupported envelope version {other}");
            console_log!("rejecting envelope {}: {reason}", message.id());
            runtime.ledger.record_rejected(&message.id(), &reason).await?;
            notify_rejected(push, &message.id(), &reason).await;
            message.ack();
            Ok(())
        }
    }
}

async fn process_discrete(
    runtime: &WorkerRuntime,
    downloader: &mut Downloader,
    push: &PushHubClient,
    job_id: JobId,
    job: &JobPlan,
    message: &Message<serde_json::Value>,
) -> Result<()> {
    let claim = runtime.ledger.claim(&job_id).await?;
    let (execution_token, checkpoint) = match claim {
        JobClaim::Claimed {
            execution_token,
            checkpoint,
        } => (execution_token, checkpoint),
        JobClaim::AlreadyTerminal | JobClaim::Unknown => {
            message.ack();
            return Ok(());
        }
        JobClaim::Busy { retry_after } => {
            let delay = retry_after.as_secs().clamp(1, u64::from(u32::MAX)) as u32;
            let options = QueueRetryOptionsBuilder::new()
                .with_delay_seconds(delay)
                .build();
            console_log!(
                "job {job_id} is leased; retrying after {delay}s instead of acknowledging"
            );
            message.retry_with_options(&options);
            return Ok(());
        }
    };

    // 開始通知: キュー表示の更新だけを送る。native はここで echo 行を出さず
    // CLI 子プロセスの出力が流れるだけなので、worker でも合成の開始行は
    // 送らない (queue_start は UI の refresh トリガとしてだけ有用)。
    push.broadcast_best_effort(&[
        event("queue_start", json!(job_id.as_str())),
        notification_queue(),
    ])
    .await;

    let outcome = if job.kind == JobKind::Convert {
        // Convert は保存済みデータだけを見るので Downloader を使わない。
        // コンソール行は PushHubSink を内側で install/drain する。
        crate::convert::execute_convert(runtime, job, push).await
    } else {
        execute_job(
            downloader,
            job,
            &job_id,
            checkpoint.as_ref(),
            &runtime.subrequests,
            &runtime.ledger,
            &execution_token,
            push,
        )
        .await
    };
    match outcome {
        JobOutcome::Succeeded => {
            runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Succeeded,
                    None,
                )
                .await?;
            broadcast_terminal_events(
                push,
                &job_id,
                &[event("queue_complete", json!(job_id.as_str()))],
            )
            .await;
            message.ack();
        }
        JobOutcome::Partial {
            reason,
            checkpoint,
        } => {
            console_log!("job {job_id} yielded for continuation: {reason}");
            runtime
                .ledger
                .yield_for_continuation(&job_id, &execution_token, &checkpoint)
                .await?;
            runtime.enqueue_plan(job.clone()).await?;
            broadcast_terminal_events(
                push,
                &job_id,
                &[
                    echo(&reason, "stdout"),
                    event(
                        "queue_partial",
                        json!({ "job_id": job_id.as_str(), "reason": reason }),
                    ),
                ],
            )
            .await;
            message.ack();
        }
        JobOutcome::Blocked { reason } => {
            runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Blocked,
                    Some(&reason),
                )
                .await?;
            console_log!("job {job_id} blocked: {reason}");
            broadcast_failure(runtime, push, &job_id, &reason).await;
            message.ack();
        }
        JobOutcome::Permanent { reason } => {
            runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Permanent,
                    Some(&reason),
                )
                .await?;
            console_log!("job {job_id} failed permanently: {reason}");
            broadcast_failure(runtime, push, &job_id, &reason).await;
            message.ack();
        }
        JobOutcome::Retryable { reason } => {
            match settle_retryable(runtime, &job_id, &execution_token, &reason).await? {
                RetryDisposition::Rescheduled {
                    attempts,
                    max_retries,
                    backoff_secs,
                } => {
                    // Cloudflare Queues の遅延は秒整数なのでイベント値を
                    // u32 にクランプして渡す (スケジュール値は非負)。
                    let delay = backoff_secs.min(i64::from(u32::MAX)) as u32;
                    let options = QueueRetryOptionsBuilder::new()
                        .with_delay_seconds(delay)
                        .build();
                    broadcast_terminal_events(
                        push,
                        &job_id,
                        &[event(
                            "queue_retry",
                            json!({
                                "job_id": job_id.as_str(),
                                "retry_count": attempts,
                                "max_retries": max_retries,
                                "backoff_secs": backoff_secs,
                                "available_at": (js_sys::Date::now() / 1000.0) as i64
                                    + backoff_secs,
                                "reason": first_non_empty_line(&reason),
                            }),
                        )],
                    )
                    .await;
                    message.retry_with_options(&options);
                }
                RetryDisposition::Exhausted => {
                    broadcast_failure(runtime, push, &job_id, &reason).await;
                    message.ack();
                }
            }
        }
    }
    Ok(())
}

/// `queue_failed` + コンソール echo — Blocked / Permanent / リトライ枯渇の
/// 共通イベント列 (native `JobOutcome::Failed` 相当)。
///
/// `reason` は台帳へ書き込んだ last_error 全文だが、UI には native の
/// `failure_reason` (`src/web/worker.rs`) と同じ意味論 — **最初の非空行**
/// — だけを出す。`detail` は native 同様 `webui.debug-mode` が ON のとき
/// だけ `queue_failed.data.detail` に全文を載せる。
async fn broadcast_failure(
    runtime: &WorkerRuntime,
    push: &PushHubClient,
    job_id: &JobId,
    reason: &str,
) {
    let mut data = json!({
        "job_id": job_id.as_str(),
        "reason": first_non_empty_line(reason),
    });
    if webui_debug_mode(runtime).await {
        data["detail"] = serde_json::Value::String(reason.to_string());
    }
    broadcast_terminal_events(
        push,
        job_id,
        &[
            // native は CLI の stdout をそのまま流すので、失敗行は
            // `  Error: …` の形で出る (messages::error_line と同じ書式)。
            echo(
                &messages::error_line(first_non_empty_line(reason)),
                "stdout",
            ),
            event("queue_failed", data),
        ],
    )
    .await;
}

/// native `failure_reason` 相当: detail (= last_error) の最初の非空行を
/// 採用する。見つからなければ trim 済みの全文にフォールバック。
fn first_non_empty_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| text.trim())
}

/// `webui.debug-mode` (local スコープ)。読み取りに失敗しても表示用なので
/// false (= detail を載せない) に倒す。
async fn webui_debug_mode(runtime: &WorkerRuntime) -> bool {
    runtime
        .services
        .settings
        .get("webui.debug-mode")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// `Retryable` を台帳へ記録し、まだ試行枠が残っているかを返す。
/// メッセージの ack/retry は呼び出し側がイベント送信のあとに行う
/// (イベントを先に流さないと UI が再試行の前にキューを見逃す)。
enum RetryDisposition {
    Rescheduled {
        attempts: u32,
        max_retries: u32,
        backoff_secs: i64,
    },
    Exhausted,
}

async fn settle_retryable(
    runtime: &WorkerRuntime,
    job_id: &JobId,
    execution_token: &str,
    reason: &str,
) -> Result<RetryDisposition> {
    let attempts = runtime
        .ledger
        .record_attempt(job_id, execution_token, reason)
        .await?;
    // リトライ方針は native と同じ local 設定から組み立てる
    // (`queue.max-retries` / `queue.retry-backoff`)。読み取り失敗は既定値に
    // 倒す — native の設定読み取り失敗と同じ扱い。
    let values = runtime
        .services
        .settings
        .get_many(&[
            retry_policy::MAX_RETRIES_KEY,
            retry_policy::RETRY_BACKOFF_KEY,
        ])
        .await
        .unwrap_or_default();
    let policy = RetryPolicy::resolve(
        values.first().and_then(Option::as_ref),
        values.get(1).and_then(Option::as_ref),
    );
    // 台帳の `attempts` は失敗ごとに後置インクリメントされるので、今回の
    // 失敗までに完了した再キュー数 (= native `retry_count`) は
    // `attempts - 1`。native `retry_count < max_retries` と同じ上限判定。
    let retry_count = attempts.saturating_sub(1);
    if !policy.can_retry(retry_count) {
        let exhausted = format!("retries exhausted after {attempts} attempts: {reason}");
        runtime
            .ledger
            .mark_terminal(
                job_id,
                execution_token,
                JobLedgerStatus::Permanent,
                Some(&exhausted),
            )
            .await?;
        console_log!("job {job_id} permanently failed: {exhausted}");
        Ok(RetryDisposition::Exhausted)
    } else {
        let backoff_secs = policy.backoff_secs(retry_count);
        console_log!(
            "job {job_id} retryable (attempt {attempts}), re-queueing in {backoff_secs}s: {reason}"
        );
        Ok(RetryDisposition::Rescheduled {
            attempts,
            max_retries: policy.max_retries,
            backoff_secs,
        })
    }
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker runtime error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::JobOutcome;

    #[test]
    fn four_budget_yields_do_not_consume_failure_attempts() {
        use narou_rs::application::{
            ExecutionPhase, JobId, WorkerExecutionCheckpoint,
        };
        use narou_rs::platform::NovelId;

        let job_id = JobId("job-1".into());
        let mut checkpoint =
            WorkerExecutionCheckpoint::planned(job_id.clone(), Some(NovelId(42)));
        for next_section_index in 1..=4 {
            let outcome = JobOutcome::Partial {
                reason: format!("budget yield {next_section_index}"),
                checkpoint: checkpoint.advance(
                    ExecutionPhase::Fetching,
                    Some(next_section_index),
                ),
            };
            checkpoint = match outcome {
                JobOutcome::Partial { checkpoint, .. } => checkpoint,
                JobOutcome::Retryable { .. } => {
                    panic!("budget yields must not be retryable")
                }
                _ => unreachable!("budget yields must be partial"),
            };
        }
        assert_eq!(checkpoint.job_id, job_id);
        assert_eq!(checkpoint.next_section_index, Some(4));
        assert_eq!(checkpoint.phase, ExecutionPhase::Fetching);
    }
}
