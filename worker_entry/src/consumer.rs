//! Queue consumers.
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
//!
//! # Roles
//!
//! - The **writer** consumes the conversion queue (version-2 execution) and
//!   the legacy acquisition ingress. The ingress is *drain only*: it gives the
//!   row its identity and lets the lane dispatcher assign it. The writer
//!   never executes an acquisition.
//! - An **acquisition lane** consumes exactly one acquisition queue. It
//!   executes the version-3 assignment named by the envelope, and only when
//!   the envelope names the lane this script owns.

use std::borrow::Cow;
use std::time::Duration;

use narou_rs::application::acquisition_lanes::{AcquisitionEnvelope, ENVELOPE_VERSION};
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
/// 文字列で判定して `queue_cancelled` を送出する ([`settle_lost_execution`])。
pub(crate) const USER_CANCEL_REASON: &str = "cancelled by user via Web UI";

/// Delay of the rare capacity retry of an assigned acquisition.
///
/// The reservation is retained by the capacity park, so the same envelope can
/// be delivered again without losing the job.
const CAPACITY_RETRY_DELAY_SECS: u32 = 5;

/// A lightweight ledger for the paths that must not build the full runtime.
fn bare_ledger(env: &Env) -> Result<crate::ledger::D1JobLedger> {
    Ok(crate::ledger::D1JobLedger::new(
        crate::db_handle::DbHandle::primary(std::sync::Arc::new(env.d1("DB").map_err(worker_error)?)),
        std::sync::Arc::new(narou_rs::platform::SystemClock),
    ))
}

/// Process a writer queue batch.
///
/// The conversion queue keeps the version-2 execution protocol. The legacy
/// acquisition ingress is drained into the lane dispatcher: the writer never
/// runs an acquisition itself.
pub async fn process_batch(
    message_batch: MessageBatch<serde_json::Value>,
    env: &Env,
) -> Result<()> {
    let queue_name = message_batch.queue();
    let source_convert =
        crate::composition::convert_queue_name(env).as_deref() == Some(queue_name.as_str());
    let memory_permit = if source_convert {
        let Some(memory_permit) = crate::memory_budget::try_heavy() else {
            // No runtime/site compilation, downloader, or site lease while waiting.
            // Park in D1 and ack: physical queue retries must not spend business
            // attempts (nor fall into the DLQ merely because the isolate is busy).
            let ledger = bare_ledger(env)?;
            for message in message_batch.messages().map_err(worker_error)? {
                defer_capacity(&ledger, &message).await?;
                message.ack();
            }
            return Ok(());
        };
        Some(memory_permit)
    } else {
        None
    };
    let runtime = WorkerRuntime::build(env).await.map_err(worker_error)?;
    let push = runtime.push();
    for message in message_batch.messages().map_err(worker_error)? {
        // A batch never accumulates section caches from previous novels.
        let mut downloader = None;
        if source_convert {
            process_envelope(&runtime, &mut downloader, push, message.body(), &message, true).await?;
        } else {
            drain_ingress(&runtime, push, message.body(), &message).await?;
        }
    }
    drop(memory_permit);
    if let Err(error) = runtime.wake_waiting_jobs().await {
        console_log!("admission wake failed; cron will recover it: {error}");
    }
    if source_convert && let Err(error) = runtime.dispatch_acquisition().await {
        console_log!("acquisition dispatch after conversion failed: {error}");
    }
    Ok(())
}

/// Process one acquisition lane batch.
///
/// The lane owns exactly one assigned job. The heavy permit is dropped after
/// physical work drains, before releasing or reclassifying its reservation.
pub async fn process_acquisition_batch(
    message_batch: MessageBatch<serde_json::Value>,
    env: &Env,
    lane: u8,
) -> Result<()> {
    for message in message_batch.messages().map_err(worker_error)? {
        let value = message.body();
        let version = value.get("version").and_then(serde_json::Value::as_u64).unwrap_or(0);
        if version != u64::from(ENVELOPE_VERSION) {
            // Legacy envelopes are ingress notifications: they are drained into
            // the dispatcher, never executed in a lane.
            let runtime = WorkerRuntime::build(env).await.map_err(worker_error)?;
            drain_ingress(&runtime, runtime.push(), &value, &message).await?;
            continue;
        }
        let envelope: AcquisitionEnvelope = match serde_json::from_value(value.clone()) {
            Ok(envelope) => envelope,
            Err(error) => {
                // Malformed payloads are poison: they would never parse again
                // on redelivery, so durably block + ack instead of spinning.
                let reason = format!("malformed v3 envelope: {error}");
                console_log!("rejecting envelope {}: {reason}", message.id());
                let ledger = bare_ledger(env)?;
                ledger.record_rejected(&message.id(), &reason).await?;
                message.ack();
                continue;
            }
        };
        if !envelope.is_valid() || envelope.lane != lane {
            // A delivery for another lane never executes, and it never frees
            // the current owner of that lane.
            console_log!(
                "ignoring acquisition envelope for lane {} on lane {lane}",
                envelope.lane
            );
            message.ack();
            continue;
        }
        // The heavy permit belongs to this job only.
        let Some(memory_permit) = crate::memory_budget::try_heavy() else {
            let ledger = bare_ledger(env)?;
            ledger.park_acquisition_capacity(&envelope).await?;
            // The assignment is retained, so the same envelope may be
            // delivered again without losing the job.
            let options = QueueRetryOptionsBuilder::new()
                .with_delay_seconds(CAPACITY_RETRY_DELAY_SECS)
                .build();
            message.retry_with_options(&options);
            continue;
        };
        let runtime = WorkerRuntime::build(env).await.map_err(worker_error)?;
        let completion = execute_assigned(&runtime, runtime.push(), &envelope, &message).await;
        drop(memory_permit);
        match completion? {
            AcquisitionCompletion::Retain => {}
            AcquisitionCompletion::Release => {
                runtime.ledger.release_acquisition_lane(&envelope).await?;
            }
            AcquisitionCompletion::Reclassify(fresh) => {
                runtime.ledger.reclassify_acquisition(
                    &envelope,
                    &fresh.normalized_target,
                    &fresh.site_key,
                ).await?;
                message.ack();
            }
        }
        // Ownership and capacity must both be free before the next dispatch.
        if let Err(error) = runtime.wake_waiting_jobs().await {
            console_log!("conversion admission wake failed: {error}");
        }
        if let Err(error) = runtime.dispatch_acquisition().await {
            console_log!("acquisition dispatch failed: {error}");
        }
    }
    Ok(())
}

async fn defer_capacity(ledger: &crate::ledger::D1JobLedger, message: &Message<serde_json::Value>) -> Result<()> {
    let value = message.body();
    let id = match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(version) if version == u64::from(WORKER_JOB_ENVELOPE_VERSION) => {
            match serde_json::from_value::<WorkerJobEnvelope>(value.clone()) {
                Ok(envelope) if narou_rs::application::envelope_bytes(&envelope)
                    .is_ok_and(|bytes| bytes <= narou_rs::application::job_limits::MAX_ENVELOPE_BYTES) => Some(envelope.job_id),
                Ok(_) => {
                    ledger.record_rejected(&message.id(), "envelope exceeds queue payload limit").await?;
                    None
                }
                Err(error) => {
                    ledger.record_rejected(&message.id(), &format!("malformed v2 envelope: {error}")).await?;
                    None
                }
            }
        }
        Some(1) => {
            let request = serde_json::from_value::<JobRequest>(value.get("job").cloned().unwrap_or_default());
            match request.map(|request| decode_legacy_envelope(&request)) {
                Ok(LegacyEnvelopeOutcome::Plan(plan)) => Some(ledger.enqueue(plan).await?.job_id),
                Ok(LegacyEnvelopeOutcome::Reject { reason }) => {
                    ledger.record_rejected(&message.id(), &reason).await?;
                    None
                }
                Err(error) => {
                    ledger.record_rejected(&message.id(), &format!("malformed v1 envelope: {error}")).await?;
                    None
                }
            }
        }
        _ => {
            ledger.record_rejected(&message.id(), "unsupported envelope version").await?;
            None
        }
    };
    if let Some(id) = id {
        if ledger.get(&id).await?.is_some() {
            ledger.park_for_capacity(&id).await?;
        } else {
            ledger.record_rejected(&message.id(), &format!("ledger has no row for queued job {id}")).await?;
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

/// Drain one legacy ingress envelope into the lane dispatcher.
///
/// The old acquisition queue is an ingress, not an execution path: the row is
/// given its identity and the dispatcher assigns it to a free lane. Nothing
/// is executed here, so the writer never holds a heavy permit for it.
async fn drain_ingress(
    runtime: &WorkerRuntime,
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
            // The ledger row is the authority for the plan; the envelope
            // only names the job.
            match runtime.ledger.get(&envelope.job_id).await? {
                Some(view) if !view.status.is_terminal() => {
                    route_acquisition_row(runtime, &envelope.job_id, &view.job).await?;
                }
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
            }
            message.ack();
        }
        v if v == u64::from(ENVELOPE_VERSION) => {
            // An assigned delivery that reached the legacy ingress: the lane
            // reservation owns the job, so the message is acknowledged
            // without touching the row.
            console_log!("ignoring an assigned acquisition delivery on the legacy ingress");
            message.ack();
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
                    let queued = runtime.ledger.enqueue(job.clone()).await?;
                    notify_queue_changed(push).await;
                    route_acquisition_row(runtime, &queued.job_id, &queued.job).await?;
                    message.ack();
                }
                LegacyEnvelopeOutcome::Reject { reason } => {
                    console_log!("rejecting legacy envelope {}: {reason}", message.id());
                    runtime.ledger.record_rejected(&message.id(), &reason).await?;
                    notify_rejected(push, &message.id(), &reason).await;
                    message.ack();
                }
            }
        }
        other => {
            let reason = format!("unsupported envelope version {other}");
            console_log!("rejecting envelope {}: {reason}", message.id());
            runtime.ledger.record_rejected(&message.id(), &reason).await?;
            notify_rejected(push, &message.id(), &reason).await;
            message.ack();
        }
    }
    runtime.dispatch_acquisition().await?;
    Ok(())
}

/// Route one durable row: conversion keeps its queue, acquisition gets an
/// identity and waits for a lane.
async fn route_acquisition_row(
    runtime: &WorkerRuntime,
    job_id: &JobId,
    job: &JobPlan,
) -> Result<()> {
    if job.kind == narou_rs::application::JobKind::Convert {
        runtime.send_existing_job(job_id, job.kind.as_str()).await?;
        return Ok(());
    }
    let settings = match runtime.effective_site_settings().await {
        Ok(settings) => settings,
        Err(error) => {
            // 定義が読めなければ identity は確定できない。行は identity なし
            // のまま残り、次の取り込み処理が解決する。
            console_log!("site definitions are unavailable: {error}");
            return Ok(());
        }
    };
    if let Err(error) = runtime
        .prepare_acquisition_identity(job_id, job, &settings)
        .await
    {
        // 対象が解決できない行は identity なしで残る。ディスパッチャの取り込み
        // 処理が、実行時と同じ恒久エラーとして終端する。
        console_log!("job {job_id} has no effective site identity yet: {error}");
    }
    Ok(())
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

async fn load_update_record(
    runtime: &WorkerRuntime,
    target: &JobTarget,
) -> Result<Option<narou_rs::db::NovelRecord>> {
    let id = match target {
        JobTarget::Id(id) => Some(*id),
        JobTarget::Ncode(ncode) => runtime.novels.find_by_ncode(ncode).await?.map(|record| record.id.into()),
        JobTarget::Url(url) => runtime.novels.find_by_toc_url(url).await?.map(|record| record.id.into()),
        JobTarget::All => None,
    };
    let Some(id) = id else { return Ok(None); };
    let content = narou_rs::application::NovelContentService::new(runtime.novels.clone(), runtime.write_objects());
    let mut scope = content.read_scope("update-admission");
    let record = content.read_record(&mut scope, id).await.map_err(|error| NarouError::Platform(error.to_string()));
    let released = scope.release_all().await;
    let record = record?;
    released?;
    Ok(record)
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
    let (update_record, record_error) = if job.kind == JobKind::Update {
        match load_update_record(runtime, &job.target).await {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        }
    } else { (None, None) };
    let virtual_update = update_record.as_ref().is_some_and(narou_rs::virtual_novel::is_virtual_record);
    let mut prepared_writer = None;
    let mut setup_error = record_error;
    if setup_error.is_none() && job.kind != JobKind::Convert && !virtual_update {
        let prepared = async {
            let checkpoint = runtime.ledger.execution_checkpoint(&job_id).await?;
            runtime.new_downloader(&job.target, checkpoint.as_ref().and_then(|checkpoint| checkpoint.write_origin.as_ref())).await
        }.await;
        match prepared {
            Ok((value, writer)) => { *downloader = Some(value); prepared_writer = writer; }
            Err(error) => setup_error = Some(error),
        }
    }
    let target = match &job.target {
        JobTarget::Id(id) => Cow::Owned(id.0.to_string()),
        JobTarget::Ncode(value) | JobTarget::Url(value) => Cow::Borrowed(value.as_str()),
        JobTarget::All => Cow::Borrowed("*"),
    };
    let (site_key, admission_error) = if let Some(error) = setup_error {
        (None, Some(error))
    } else if !virtual_update && matches!(job.kind, JobKind::Download | JobKind::Update | JobKind::AuthorCheck) {
        match downloader.as_ref().expect("acquisition downloader").site_key_for_target(&target, update_record.as_ref()).await {
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
        JobClaim::AlreadyTerminal => {
            advance_terminal_creation(runtime, &job_id).await?;
            message.ack();
            return Ok(());
        }
        JobClaim::Unknown => {
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
    // Move the actual claim token into one immutable owner. Every writer and
    // reservation in this invocation retains this same Arc, never a new token.
    let job_owner = std::sync::Arc::new(narou_rs::platform::JobExecutionOwner::new(job_id.as_str(), execution_token));
    let execution_token = job_owner.execution_token();

    // 開始通知: キュー表示の更新だけを送る。native はここで echo 行を出さず
    // CLI 子プロセスの出力が流れるだけなので、worker でも合成の開始行は
    // 送らない (queue_start は UI の refresh トリガとしてだけ有用)。
    push.broadcast_best_effort(&[
        event("queue_start", json!(job_id.as_str())),
        notification_queue(),
    ])
    .await;

    let outcome = if virtual_update {
        // Old queued plans and the generic jobs API use the same successful
        // no-op as native update: no site lookup, pacing or upstream request.
        JobOutcome::Succeeded
    } else if let Some(error) = admission_error {
        JobOutcome::from_error(error)
    } else if job.kind == JobKind::Convert {
        // Convert は保存済みデータだけを見るので Downloader を使わない。
        // コンソール行は PushHubSink を内側で install/drain する。
        crate::convert::execute_convert(runtime, job, &job_id, std::sync::Arc::clone(&job_owner), checkpoint.as_ref(), push).await
    } else {
        execute_job(
            downloader.as_mut().expect("acquisition downloader"),
            job,
            crate::executor::JobExecution {
                job_id: &job_id, checkpoint: checkpoint.as_ref(),
                owner: std::sync::Arc::clone(&job_owner), update_record: update_record.as_ref(),
                prepared_writer,
            },
            runtime,
            push,
        )
        .await
    };
    settle_outcome(
        runtime,
        push,
        &job_id,
        job,
        execution_token,
        outcome,
        message,
        RetryDispatch::Provider,
    )
    .await
}

/// The preflight verdict of one assigned job.
enum Preflight {
    /// The stored identity still matches the effective YAML.
    Ready,
    /// The effective YAML changed: the row must be re-identified before it
    /// can run under the current family.
    Stale(crate::acquisition::AcquisitionIdentity),
    /// The target lost its definition: the job cannot run at all.
    Unresolvable {
        status: JobLedgerStatus,
        reason: String,
    },
}

/// Reservation changes are deferred until the isolate's heavy permit is free.
enum AcquisitionCompletion {
    Retain,
    Release,
    Reclassify(crate::acquisition::AcquisitionIdentity),
}

/// Execute one assigned acquisition.
///
/// The assignment owns the job: the claim validates the lane, the dispatch
/// token and the lease, and the stored site identity is authoritative.
async fn execute_assigned(
    runtime: &WorkerRuntime,
    push: &PushHubClient,
    envelope: &AcquisitionEnvelope,
    message: &Message<serde_json::Value>,
) -> Result<AcquisitionCompletion> {
    // The effective YAML may have changed after the assignment was made. The
    // stored identity is then stale and the job must not run under the old
    // family: it is re-identified and the dispatcher assigns it again.
    let preflight = match runtime.ledger.get_view(&envelope.job_id).await? {
        Some(view) => match crate::acquisition::resolve_identity(runtime, &view.job.target).await {
            Ok(fresh) => {
                let stale = view.site_key.as_deref() != Some(fresh.site_key.as_str())
                    || view.normalized_target.as_deref() != Some(fresh.normalized_target.as_str());
                if stale {
                    Preflight::Stale(fresh)
                } else {
                    Preflight::Ready
                }
            }
            Err(error) => {
                let status = match narou_rs::application::classify_failure(&error) {
                    narou_rs::application::JobFailureClass::Retryable => {
                        // A transient failure keeps the assignment: the
                        // delivery is retried by the queue.
                        let options = QueueRetryOptionsBuilder::new()
                            .with_delay_seconds(CAPACITY_RETRY_DELAY_SECS)
                            .build();
                        message.retry_with_options(&options);
                        return Ok(AcquisitionCompletion::Retain);
                    }
                    narou_rs::application::JobFailureClass::Permanent => JobLedgerStatus::Permanent,
                    narou_rs::application::JobFailureClass::Blocked => JobLedgerStatus::Blocked,
                };
                Preflight::Unresolvable {
                    status,
                    reason: error.to_string(),
                }
            }
        },
        // Without a row the claim reports the unknown job.
        None => Preflight::Ready,
    };
    if let Preflight::Stale(fresh) = preflight {
        console_log!(
            "acquisition job {} changed identity; re-assigning it",
            envelope.job_id
        );
        // The outer consumer drops its permit before the fenced batch updates
        // the identity and releases the reservation.
        return Ok(AcquisitionCompletion::Reclassify(fresh));
    }
    let unresolvable = match preflight {
        Preflight::Unresolvable { status, reason } => Some((status, reason)),
        _ => None,
    };
    let claim = runtime.ledger.claim_acquisition(envelope).await?;
    let (execution_token, checkpoint) = match claim {
        JobClaim::Claimed {
            execution_token,
            checkpoint,
        } => (execution_token, checkpoint),
        JobClaim::AlreadyTerminal => {
            // Exact terminal reservations can be released; stale or live
            // owners remain protected by the token/status guards in SQL.
            message.ack();
            return Ok(AcquisitionCompletion::Release);
        }
        JobClaim::Unknown => {
            let reason = format!("ledger has no row for queued job {}", envelope.job_id);
            console_log!("rejecting envelope {}: {reason}", message.id());
            runtime.ledger.record_rejected(&message.id(), &reason).await?;
            notify_rejected(push, &message.id(), &reason).await;
            message.ack();
            return Ok(AcquisitionCompletion::Retain);
        }
        JobClaim::Busy => {
            // The admission was lost to another owner. The job keeps its
            // identity and returns to the dispatchable pool.
            message.ack();
            return Ok(AcquisitionCompletion::Release);
        }
    };
    // One immutable owner for this invocation: the writer guard, the
    // reservation and every terminal write keep this same execution token.
    let job_owner = std::sync::Arc::new(
        narou_rs::platform::JobExecutionOwner::new(envelope.job_id.as_str(), execution_token),
    );
    let execution_token = job_owner.execution_token();
    let Some(view) = runtime.ledger.get(&envelope.job_id).await? else {
        message.ack();
        return Ok(AcquisitionCompletion::Retain);
    };
    let job = view.job;
    if let Some((status, reason)) = unresolvable {
        // The target lost its definition: the job cannot run under any
        // family. The claim already owns the row, so the durable error is
        // written with the same token guard as any other terminal state.
        console_log!("acquisition job {} cannot be resolved: {reason}", envelope.job_id);
        let outcome = match status {
            JobLedgerStatus::Permanent => JobOutcome::Permanent { reason },
            _ => JobOutcome::Blocked { reason },
        };
        settle_outcome(
            runtime,
            push,
            &envelope.job_id,
            &job,
            execution_token,
            outcome,
            message,
            RetryDispatch::Lane,
        )
        .await?;
        return Ok(AcquisitionCompletion::Release);
    }
    let (update_record, record_error) = if job.kind == JobKind::Update {
        match load_update_record(runtime, &job.target).await {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        }
    } else { (None, None) };
    let virtual_update = update_record.as_ref().is_some_and(narou_rs::virtual_novel::is_virtual_record);
    let outcome = if virtual_update {
        JobOutcome::Succeeded
    } else if let Some(error) = record_error {
        JobOutcome::from_error(error)
    } else {
        // Construction errors are execution failures too; do not bypass the
        // bounded attempt settlement after a claim has been acquired.
        // The claimed checkpoint is authoritative for the writer origin: a
        // lane continuation resumes its original namespace epoch instead of
        // capturing a new one.
        match runtime
            .new_downloader(&job.target, checkpoint.as_ref().and_then(|checkpoint| checkpoint.write_origin.as_ref()))
            .await
        {
            Ok((mut downloader, prepared_writer)) => execute_job(
                &mut downloader,
                &job,
                crate::executor::JobExecution {
                    job_id: &envelope.job_id, checkpoint: checkpoint.as_ref(),
                    owner: std::sync::Arc::clone(&job_owner), update_record: update_record.as_ref(),
                    prepared_writer,
                },
                runtime,
                push,
            ).await,
            Err(error) => JobOutcome::from_error(error),
        }
    };
    settle_outcome(
        runtime,
        push,
        &envelope.job_id,
        &job,
        execution_token,
        outcome,
        message,
        RetryDispatch::Lane,
    )
    .await?;
    // A failed settlement retains the reservation for crash recovery.
    Ok(AcquisitionCompletion::Release)
}

/// How a retryable outcome is delivered again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetryDispatch {
    /// The queue owns redelivery: ask the provider for a delayed retry.
    Provider,
    /// The lane dispatcher owns redelivery: the backoff is persisted and the
    /// dispatcher sends a fresh assignment once the lane is free.
    Lane,
}

/// Write the durable outcome of one execution and settle the message.
async fn settle_outcome(
    runtime: &WorkerRuntime,
    push: &PushHubClient,
    job_id: &JobId,
    job: &JobPlan,
    execution_token: &str,
    outcome: JobOutcome,
    message: &Message<serde_json::Value>,
    retry: RetryDispatch,
) -> Result<()> {
    match outcome {
        JobOutcome::Succeeded => {
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    job_id,
                    execution_token,
                    JobLedgerStatus::Succeeded,
                    None,
                )
                .await
            {
                // Distinguish cancellation from a superseding execution
                // without changing the successor's row or reservations.
                settle_lost_execution(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            advance_terminal_creation(runtime, &job_id).await?;
            broadcast_terminal_events(
                push,
                job_id,
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
                    job_id,
                    execution_token,
                    JobLedgerStatus::Partial,
                    Some(&reason),
                )
                .await
            {
                settle_lost_execution(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            advance_terminal_creation(runtime, &job_id).await?;
            broadcast_terminal_events(
                push,
                job_id,
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
                .yield_for_continuation(job_id, execution_token, &checkpoint)
                .await
            {
                // 中断されたジョブを再投入しない (行は terminal 済み)。
                settle_lost_execution(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            broadcast_terminal_events(
                push,
                job_id,
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
            // Preserve the yielded row's checkpoint, attempts, and cancellation
            // tombstone even if the queue was cleared during notification.
            if retry == RetryDispatch::Provider {
                runtime.send_existing_job(job_id, job.kind.as_str()).await?;
            }
            message.ack();
        }
        JobOutcome::Blocked { reason } => {
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    job_id,
                    execution_token,
                    JobLedgerStatus::Blocked,
                    Some(&reason),
                )
                .await
            {
                // 実行中にキャンセルされたなら queue_failed は出さない。
                settle_lost_execution(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            advance_terminal_creation(runtime, &job_id).await?;
            console_log!("job {job_id} blocked: {reason}");
            broadcast_failure(runtime, push, job_id, &reason).await;
            message.ack();
        }
        JobOutcome::Permanent { reason } => {
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    job_id,
                    execution_token,
                    JobLedgerStatus::Permanent,
                    Some(&reason),
                )
                .await
            {
                // 実行中にキャンセルされたなら queue_failed は出さない。
                settle_lost_execution(runtime, push, &job_id, &execution_token, message, error).await?;
                return Ok(());
            }
            advance_terminal_creation(runtime, &job_id).await?;
            console_log!("job {job_id} failed permanently: {reason}");
            broadcast_failure(runtime, push, job_id, &reason).await;
            message.ack();
        }
        JobOutcome::Retryable { reason } => {
            match settle_retryable(runtime, job_id, execution_token, &reason).await {
                Err(error) => {
                    // Confirm cancellation or lost authority in D1. A stale
                    // physical execution must not retry/fail its successor.
                    settle_lost_execution(runtime, push, &job_id, &execution_token, message, error).await?;
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
                    broadcast_terminal_events(
                        push,
                        job_id,
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
                    match retry {
                        RetryDispatch::Provider => {
                            let options = QueueRetryOptionsBuilder::new()
                                .with_delay_seconds(delay)
                                .build();
                            message.retry_with_options(&options);
                        }
                        RetryDispatch::Lane => {
                            // The persisted backoff owns the schedule: the
                            // dispatcher sends a fresh assignment once it
                            // elapsed and the lane is free again.
                            message.ack();
                        }
                    }
                }
                Ok(RetryDisposition::Exhausted) => {
                    advance_terminal_creation(runtime, job_id).await?;
                    broadcast_failure(runtime, push, job_id, &reason).await;
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
/// 別実行へ移った場合は行・予約を触らず古い配信だけ ack する。D1 の検証失敗や
/// 現在も所有している実行の失敗はエラーとして返す。
///
/// `Wasm` 自体を外側から kill せず、取得は話・未取得画像の境界で台帳を
/// 再確認して中断する。最後の処理との競合も終端書き込みの token/status
/// guard で検出し、物理処理が戻るまでは所有権を解放しない。
async fn settle_lost_execution(
    runtime: &WorkerRuntime,
    push: &PushHubClient,
    job_id: &JobId,
    execution_token: &str,
    message: &Message<serde_json::Value>,
    error: NarouError,
) -> std::result::Result<(), NarouError> {
    let cancelled = matches!(
        runtime.ledger.get(job_id).await?,
        Some(view)
            if view.status.is_terminal() && view.last_error.as_deref() == Some(USER_CANCEL_REASON)
    );
    if !cancelled || !runtime.ledger.finish_cancelled(job_id, execution_token).await? {
        return match runtime.ledger.ensure_execution_active(job_id, execution_token).await {
            Err(NarouError::SuspendDownload(_)) => {
                // A primary read proves this token is no longer current.
                // Do not clear checkpoints, release creation IDs, or emit
                // terminal events belonging to another physical executor.
                message.ack();
                Ok(())
            }
            Err(check_error) => Err(check_error),
            Ok(()) => Err(error),
        };
    }
    advance_terminal_creation(runtime, job_id).await?;
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

/// The terminal CAS comes first: a superseded physical executor must never
/// free the reservation still owned by another continuation of the same job.
/// Keep the exact checkpoint until release succeeds so cron can finish after
/// a lost response or isolate crash. `false` means durable cancellation still
/// awaits physical stop or closed cleanup, never namespace/numeric release.
pub(crate) async fn advance_terminal_creation(runtime: &WorkerRuntime, job_id: &JobId) -> Result<bool> {
    let Some((raw, eligible)) = runtime.ledger.terminal_creation_checkpoint(job_id).await? else { return Ok(true); };
    if !eligible { return Ok(false); }
    let checkpoint: narou_rs::application::WorkerExecutionCheckpoint = serde_json::from_str(&raw)
        .map_err(|error| NarouError::Platform(format!("terminal creation checkpoint for {job_id} is corrupt: {error}")))?;
    if checkpoint.job_id != *job_id || (checkpoint.write_origin.is_none()
        && (checkpoint.phase != narou_rs::application::ExecutionPhase::Planned || checkpoint.next_section_index.is_some())) {
        return Err(NarouError::Platform(format!("terminal creation checkpoint ownership for {job_id} is invalid")));
    }
    let (id, generation) = checkpoint.creation_reservation.ok_or_else(|| NarouError::Platform(format!("terminal creation checkpoint for {job_id} has no reservation")))?;
    // release_creation は未公開の空 namespace を durable に `remove-v1:` owner
    // で閉じて `CreationCleanupPending` を返す。drain は cron の
    // `recover_pending_removals` だけに頼らず、ここでも同じ bounded sweep を
    // 回す — native `recover_removals` が release と removal drain を交互に
    // 回すのと同じ形。閉じたままでは後続の取得が `novel_busy` で止まる。
    let mut cursor: Option<String> = None;
    for _ in 0..8 {
        match runtime.novels.release_id(narou_rs::platform::NovelIdReservation::new(id, generation)).await {
            Ok(()) => break,
            Err(NarouError::CreationCleanupPending) => {},
            Err(error) => return Err(error),
        }
        let next = runtime.write_services().novel_actions.recover_removals(cursor.as_deref(), 4, 16).await
            .map_err(|error| NarouError::Platform(format!("creation removal drain: {error}")))?;
        // `None` カーソル = ページ走査が末尾に到達。owner が drain し切れて
        // いなければ次のループが先頭からもう1パス回り、per-owner ファイル
        // 予算で残った分を同じ呼び出しで続ける。
        cursor = next;
    }
    match runtime.novels.release_id(narou_rs::platform::NovelIdReservation::new(id, generation)).await {
        Ok(()) => {},
        Err(NarouError::CreationCleanupPending) => return Ok(false),
        Err(error) => return Err(error),
    }
    runtime.ledger.clear_terminal_creation_checkpoint(job_id, &raw).await?;
    Ok(true)
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
///
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
