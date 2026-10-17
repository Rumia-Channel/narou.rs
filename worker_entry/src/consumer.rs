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

use std::borrow::Cow;
use std::time::Duration;

use narou_rs::application::{
    JobKind,
    decode_legacy_envelope, JobClaim, JobId, JobLedgerStatus, JobPlan, JobQueue, JobRequest,
    JobTarget, LegacyEnvelopeOutcome, WorkerJobEnvelope, WORKER_JOB_ENVELOPE_VERSION,
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
/// (reject / 再キュー追加 / cron reaper の再投入・終端確定) キュー表示を
/// 更新させる。
pub(crate) async fn notify_queue_changed(push: &PushHubClient) {
    push.broadcast_best_effort(&[notification_queue()]).await;
}

/// native `should_reload_table_after_job` (`src/web/worker.rs`) の Worker 版。
///
/// `webui.table.reload-timing` が `queue` のときだけ、台帳の active 行
/// (pending / retryable / running) が 0 = キューが掃けたときに table.reload /
/// tag.updateCanvas を送る。判定材料は primary の台帳スナップショット (D1 の
/// 実データ) で、フロントが持つ表示状態は見ない。設定を読めない・既定値なら
/// native と同じ `every` に倒す (毎回更新)。
///
/// 呼び出し時点で対象行は既に終端 (または retryable へ遷移済み) なので、
/// 残り active が 0 なら「この終端でキューが掃けた」ことを意味する。
pub(crate) async fn table_reload_after_job(runtime: &WorkerRuntime) -> bool {
    let timing = runtime
        .write_services()
        .settings
        .get("webui.table.reload-timing")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned));
    if timing.as_deref() != Some("queue") {
        return true;
    }
    match runtime.ledger.active_job_count().await {
        Ok(active) => active == 0,
        Err(error) => {
            // 表示用の判定なので、台帳が読めないときは native の既定へ倒す。
            console_log!("table reload timing check failed: {error}");
            true
        }
    }
}

/// running 行を中断するときに `webui/queue_actions.rs` が `last_error` へ
/// 書く理由 (`ledger::cancel_running_job` の tombstone)。
///
/// この値で終わる terminal 行は「実行中にユーザーがキャンセルされた」ことを
/// 意味する — native の子プロセス kill → `JobOutcome::Cancelled` と同じ
/// 終了状態。終端書き込みが execution token 失効で失敗したときに、この
/// 文字列で判定して `queue_cancelled` を送出する ([`settle_user_cancel`])。
pub(crate) const USER_CANCEL_REASON: &str = "cancelled by user via Web UI";

/// Process a queue batch. Builds a fresh runtime and Downloader per
/// invocation so no mutable state is shared across event handlers.
pub async fn process_batch(
    message_batch: MessageBatch<serde_json::Value>,
    env: &Env,
) -> Result<()> {
    let runtime = WorkerRuntime::build(env).await.map_err(worker_error)?;
    let push = PushHubClient::new(env, runtime.subrequests.clone());
    let mut downloader = None;
    let source_convert = runtime.is_conversion_queue(&message_batch.queue());
    for message in message_batch.messages().map_err(worker_error)? {
        process_envelope(&runtime, &mut downloader, &push, message.body(), &message, source_convert).await?;
        if let Err(error) = runtime.wake_waiting_jobs().await {
            console_log!("admission wake failed; cron will recover it: {error}");
        }
    }
    Ok(())
}

/// 台帳へ reject を書き込んだあと、キュー表示の更新を通知する
/// (best-effort — 失敗しても処理は止めない)。
async fn notify_rejected(push: &PushHubClient, message_id: &str, reason: &str) {
    push.broadcast_best_effort(&[
        echo(&messages::jobs::enqueue_rejected(message_id, reason), "stdout"),
        notification_queue(),
    ])
    .await;
}

async fn process_envelope(
    runtime: &WorkerRuntime,
    downloader: &mut Option<Downloader>,
    push: &PushHubClient,
    value: &serde_json::Value,
    message: &Message<serde_json::Value>,
    source_convert: bool,
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
            // Even legacy v2 payloads execute the authoritative ledger plan.
            let plan = match runtime.ledger.get(&envelope.job_id).await? {
                Some(view) if !view.status.is_terminal() => view.job,
                Some(_) => {
                    message.ack();
                    return Ok(());
                }
                None => {
                    let reason = format!("ledger has no row for queued job {}", envelope.job_id);
                    console_log!("rejecting envelope {}: {reason}", message.id());
                    runtime.ledger.record_rejected(&message.id(), &reason).await?;
                    notify_rejected(push, &message.id(), &reason).await;
                    message.ack();
                    return Ok(());
                }
            };
            process_discrete(runtime, downloader, push, envelope.job_id, &plan, message, source_convert).await
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
                    process_discrete(runtime, downloader, push, queued.job_id, &queued.job, message, source_convert).await
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
    downloader: &mut Option<Downloader>,
    push: &PushHubClient,
    job_id: JobId,
    job: &JobPlan,
    message: &Message<serde_json::Value>,
    source_convert: bool,
) -> Result<()> {
    if (job.kind == JobKind::Convert) != source_convert {
        runtime.send_existing_job(&job_id, job.kind.as_str()).await?;
        message.ack();
        return Ok(());
    }
    if job.kind != JobKind::Convert && downloader.is_none() {
        *downloader = Some(runtime.new_downloader().await?);
    }
    let target = match &job.target {
        JobTarget::Id(id) => Cow::Owned(id.0.to_string()),
        JobTarget::Ncode(value) | JobTarget::Url(value) => Cow::Borrowed(value.as_str()),
        JobTarget::All => Cow::Borrowed("*"),
    };
    let (site_key, admission_error) = if matches!(job.kind, JobKind::Download | JobKind::Update | JobKind::AuthorCheck) {
        match downloader.as_ref().expect("acquisition downloader").site_key_for_target(&target).await {
            Ok(key) => (Some(key), None),
            Err(error) => (None, Some(error)),
        }
    } else { (None, None) };
    let normalized_target = runtime.normalized_job_target(&target);
    let claim = runtime.ledger.claim(&job_id, &normalized_target, site_key.as_deref()).await?;
    let (execution_token, checkpoint) = match claim {
        JobClaim::Claimed {
            execution_token,
            checkpoint,
        } => (execution_token, checkpoint),
        JobClaim::AlreadyTerminal | JobClaim::Unknown => {
            message.ack();
            return Ok(());
        }
        JobClaim::Busy => {
            // Contention is durable pending work, not a provider/business retry.
            // A duplicate live claim/backoff remains protected by its own lease.
            runtime.ledger.park_for_admission(&job_id, &normalized_target, site_key.as_deref()).await?;
            message.ack();
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

    let outcome = if let Some(error) = admission_error {
        JobOutcome::from_error(error)
    } else if job.kind == JobKind::Convert {
        // Convert は保存済みデータだけを見るので Downloader を使わない。
        // コンソール行は PushHubSink を内側で install/drain する。
        crate::convert::execute_convert(runtime, job, push).await
    } else {
        execute_job(
            downloader.as_mut().expect("acquisition downloader"),
            job,
            &job_id,
            checkpoint.as_ref(),
            runtime,
            &execution_token,
            push,
        )
        .await
    };
    match outcome {
        JobOutcome::Succeeded => {
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Succeeded,
                    None,
                )
                .await
            {
                // 実行中に行が tombstone されていれば queue_cancelled で閉じる
                // (`queue_complete` は送らない)。
                settle_user_cancel(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            broadcast_terminal_events(
                push,
                &job_id,
                &[event("queue_complete", json!(job_id.as_str()))],
                table_reload_after_job(runtime).await,
            )
            .await;
            message.ack();
        }
        JobOutcome::Skipped { reason } => {
            // native の単発 frozen 経路 (子プロセス mistook=1 → queue_partial)
            // と同じ終端: 実行結果は既に echo 済み、台帳は Partial で閉じる。
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Partial,
                    Some(&reason),
                )
                .await
            {
                settle_user_cancel(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            broadcast_terminal_events(
                push,
                &job_id,
                &[event(
                    "queue_partial",
                    json!({ "job_id": job_id.as_str(), "reason": reason }),
                )],
                table_reload_after_job(runtime).await,
            )
            .await;
            message.ack();
        }
        JobOutcome::Partial {
            reason,
            checkpoint,
        } => {
            console_log!("job {job_id} yielded for continuation: {reason}");
            if let Err(error) = runtime
                .ledger
                .yield_for_continuation(&job_id, &execution_token, &checkpoint)
                .await
            {
                // 中断されたジョブを再投入しない (行は terminal 済み)。
                settle_user_cancel(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
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
                table_reload_after_job(runtime).await,
            )
            .await;
            // Retire the old attempt before dispatch can start its continuation.
            runtime.enqueue_plan(job.clone()).await?;
            message.ack();
        }
        JobOutcome::Blocked { reason } => {
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Blocked,
                    Some(&reason),
                )
                .await
            {
                // 実行中にキャンセルされたなら queue_failed は出さない。
                settle_user_cancel(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            console_log!("job {job_id} blocked: {reason}");
            broadcast_failure(runtime, push, &job_id, &reason).await;
            message.ack();
        }
        JobOutcome::Permanent { reason } => {
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    &job_id,
                    &execution_token,
                    JobLedgerStatus::Permanent,
                    Some(&reason),
                )
                .await
            {
                // 実行中にキャンセルされたなら queue_failed は出さない。
                settle_user_cancel(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            console_log!("job {job_id} failed permanently: {reason}");
            broadcast_failure(runtime, push, &job_id, &reason).await;
            message.ack();
        }
        JobOutcome::Retryable { reason } => {
            match settle_retryable(runtime, &job_id, &execution_token, &reason).await {
                Err(error) => {
                    // `record_attempt` / 二次 `mark_terminal` が token 失効で
                    // 失敗 = 実行中にキャンセルされた。queue_retry /
                    // queue_failed は送らず queue_cancelled で閉じる。
                    settle_user_cancel(runtime, push, &job_id, &execution_token, message, error).await?;
                    return Ok(());
                }
                Ok(RetryDisposition::Rescheduled {
                    attempts,
                    max_retries,
                    backoff_secs,
                }) => {
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
                        table_reload_after_job(runtime).await,
                    )
                    .await;
                    message.retry_with_options(&options);
                }
                Ok(RetryDisposition::Exhausted) => {
                    broadcast_failure(runtime, push, &job_id, &reason).await;
                    message.ack();
                }
            }
        }
    }
    Ok(())
}

/// 終端書き込み (`mark_terminal` / `yield_for_continuation` /
/// `record_attempt`) が execution token の失効で失敗したときに呼ぶ。
///
/// running 行は `webui/queue_actions.rs` の cancel 系が
/// `ledger::cancel_running_job` で tombstone する (terminal status +
/// [`USER_CANCEL_REASON`])。同じ実行トークンの物理処理が戻ってから排他を解放し、
/// native の killed child と同じ `queue_cancelled` を送って ack する。
/// それ以外の失敗 (別実行への lease 移譲・D1 エラー等) は元のエラーを返す。
///
/// `Wasm` は途中で殺せないので、キャンセルされたジョブは「次の終端書き込み
/// が 0 行に終わる」ことで初めて中断を知る (native の SIGKILL と同じ
/// 観測位置)。
async fn settle_user_cancel(
    runtime: &WorkerRuntime,
    push: &PushHubClient,
    job_id: &JobId,
    execution_token: &str,
    message: &Message<serde_json::Value>,
    error: NarouError,
) -> std::result::Result<(), NarouError> {
    let cancelled = matches!(
        runtime.ledger.get(job_id).await,
        Ok(Some(view))
            if view.status.is_terminal() && view.last_error.as_deref() == Some(USER_CANCEL_REASON)
    );
    if !cancelled || !runtime.ledger.finish_cancelled(job_id, execution_token).await? {
        return Err(error);
    }
    console_log!("job {job_id} was cancelled by the user while running");
    broadcast_terminal_events(
        push,
        job_id,
        &[narou_rs::application::push_events::queue_cancelled(job_id.as_str())],
        table_reload_after_job(runtime).await,
    )
    .await;
    message.ack();
    Ok(())
}

/// `queue_failed` + コンソール echo — Blocked / Permanent / リトライ枯渇・
/// reaper による配信喪失確定の共通イベント列 (native `JobOutcome::Failed` 相当)。
///
/// `reason` は台帳へ書き込んだ last_error 全文だが、UI には native の
/// `failure_reason` (`src/web/worker.rs`) と同じ意味論 — **最初の非空行**
/// — だけを出す。`detail` は native 同様 `webui.debug-mode` が ON のとき
/// だけ `queue_failed.data.detail` に全文を載せる。
pub(crate) async fn broadcast_failure(
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
        table_reload_after_job(runtime).await,
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
pub(crate) async fn webui_debug_mode(runtime: &WorkerRuntime) -> bool {
    runtime
        .services
        .settings
        .get("webui.debug-mode")
        .await
        .ok()
        .flatten()
        .and_then(|value| narou_rs::setting_core::setting_bool_value(&value))
        .unwrap_or(false)
}

/// `queue.max-retries` の設定値をクランプする Cloudflare 側の上限。
/// `wrangler.toml` の `[[queues.consumers]] max_retries = 3` はデプロイ時の
/// 値で、Cloudflare は explicit な `retry_with_options` も同じ回数までしか
/// 再配信しない。設定値をこれより大きくしても 3 回目で DLQ に落ちて台帳が
/// `retryable` のまま残るため、ledger 側もここで同じ上限に丸める。cron
/// reaper (`scheduler::reap_stuck_jobs`) もこの上限で枯渇判定を揃える。
/// 上限を上げたい場合は wrangler の `max_retries` を先に上げ、この定数も
/// 合わせること。
pub(crate) const CF_CONSUMER_MAX_RETRIES: u32 = 3;

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
    let mut policy = RetryPolicy::resolve(
        values.first().and_then(Option::as_ref),
        values.get(1).and_then(Option::as_ref),
    );
    // `queue.max-retries` を CF の consumer `max_retries` (デプロイ値 3)
    // を超えて設定しても、Cloudflare 側がそれ以上再配信しないため
    // DLQ 行きになるだけ。ここで台帳側の上限へ丸めておく
    // (`CF_CONSUMER_MAX_RETRIES` を参照)。
    if policy.max_retries > CF_CONSUMER_MAX_RETRIES {
        console_log!(
            "queue.max-retries {} exceeds the deployed consumer max_retries {}; clamping",
            policy.max_retries,
            CF_CONSUMER_MAX_RETRIES
        );
        policy.max_retries = CF_CONSUMER_MAX_RETRIES;
    }
    // 台帳の `attempts` は失敗ごとに後置インクリメントされる。今回の失敗
    // までに完了した再キュー数 (= native `retry_count`) は記録前の
    // `attempts` で、native `retry_count < max_retries` と同じ上限判定。
    // 記録前に読むのは、今回の試行に対応するバックオフを record_attempt と
    // 同時に lease_until へ書くため (バックオフ中の二重クレーム防止)。
    let completed_retries = match runtime.ledger.get(job_id).await? {
        Some(view) => view.attempts,
        None => {
            return Err(NarouError::Platform(format!(
                "execution claim for job {job_id} is no longer valid"
            )));
        }
    };
    let rescheduled = policy.can_retry(completed_retries);
    let resume_after = if rescheduled {
        Duration::from_secs(
            u64::try_from(policy.backoff_secs(completed_retries).max(0)).unwrap_or(0),
        )
    } else {
        Duration::ZERO
    };
    let attempts = runtime
        .ledger
        .record_attempt(job_id, execution_token, reason, resume_after)
        .await?;
    if !rescheduled {
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
        let backoff_secs = policy.backoff_secs(completed_retries);
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

