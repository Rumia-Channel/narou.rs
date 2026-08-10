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
    decode_legacy_envelope, JobId, JobLedgerStatus, JobPlan, JobQueue, JobRequest,
    LegacyEnvelopeOutcome, WorkerJobEnvelope, WORKER_JOB_ENVELOPE_VERSION,
};
use narou_rs::downloader::Downloader;
use narou_rs::error::{NarouError, Result};
use worker::{console_log, Env, Message, MessageBatch, MessageExt, QueueRetryOptionsBuilder};

use crate::composition::WorkerRuntime;
use crate::executor::{execute_job, JobOutcome};

/// Bounded retry budget: after this many retryable attempts a job is
/// durably failed as `permanent`. There is no blanket retry-to-success.
pub const MAX_RETRYABLE_ATTEMPTS: u32 = 3;

/// Backoff for attempt `n` (1-based): 5s, 10s, 20s, capped at 60s.
fn retry_delay_secs(attempt: u32) -> u32 {
    5u32
        .saturating_mul(2u32.pow(attempt.saturating_sub(1)))
        .min(60)
}

/// Process a queue batch. Builds a fresh runtime and Downloader per
/// invocation so no mutable state is shared across event handlers.
pub async fn process_batch(
    message_batch: MessageBatch<serde_json::Value>,
    env: &Env,
) -> Result<()> {
    let runtime = WorkerRuntime::build(env).map_err(worker_error)?;
    let mut downloader = runtime.new_downloader()?;
    for message in message_batch.messages().map_err(worker_error)? {
        process_envelope(&runtime, &mut downloader, message.body(), &message).await?;
    }
    Ok(())
}

async fn process_envelope(
    runtime: &WorkerRuntime,
    downloader: &mut Downloader,
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
                message.ack();
                return Ok(());
            }
            process_discrete(runtime, downloader, envelope.job_id, &envelope.job, message).await
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
                    message.ack();
                    return Ok(());
                }
            };
            match decode_legacy_envelope(&request) {
                LegacyEnvelopeOutcome::Plan(job) => {
                    let queued = runtime.ledger.enqueue(job).await?;
                    process_discrete(runtime, downloader, queued.job_id, &queued.job, message)
                        .await
                }
                LegacyEnvelopeOutcome::Reject { reason } => {
                    console_log!("rejecting legacy envelope {}: {reason}", message.id());
                    runtime.ledger.record_rejected(&message.id(), &reason).await?;
                    message.ack();
                    Ok(())
                }
            }
        }
        other => {
            let reason = format!("unsupported envelope version {other}");
            console_log!("rejecting envelope {}: {reason}", message.id());
            runtime.ledger.record_rejected(&message.id(), &reason).await?;
            message.ack();
            Ok(())
        }
    }
}

async fn process_discrete(
    runtime: &WorkerRuntime,
    downloader: &mut Downloader,
    job_id: JobId,
    job: &JobPlan,
    message: &Message<serde_json::Value>,
) -> Result<()> {
    // Idempotent claim: a redelivery of an already-terminal job acks without
    // re-executing; an unknown id is durably ledgered as blocked.
    if !runtime.ledger.claim(&job_id).await? {
        message.ack();
        return Ok(());
    }

    let outcome = execute_job(downloader, job, &job_id).await;
    match outcome {
        JobOutcome::Succeeded => {
            runtime
                .ledger
                .mark_terminal(&job_id, JobLedgerStatus::Succeeded, None)
                .await?;
            message.ack();
        }
        JobOutcome::Partial {
            reason,
            checkpoint,
        } => {
            runtime
                .ledger
                .mark_terminal(&job_id, JobLedgerStatus::Partial, Some(&reason))
                .await?;
            console_log!(
                "job {job_id} finished partial (checkpoint {:?}): {reason}",
                checkpoint
            );
            message.ack();
        }
        JobOutcome::Blocked { reason } => {
            runtime
                .ledger
                .mark_terminal(&job_id, JobLedgerStatus::Blocked, Some(&reason))
                .await?;
            console_log!("job {job_id} blocked: {reason}");
            message.ack();
        }
        JobOutcome::Permanent { reason } => {
            runtime
                .ledger
                .mark_terminal(&job_id, JobLedgerStatus::Permanent, Some(&reason))
                .await?;
            console_log!("job {job_id} failed permanently: {reason}");
            message.ack();
        }
        JobOutcome::Retryable { reason } => {
            let attempts = runtime.ledger.record_attempt(&job_id, &reason).await?;
            if attempts >= MAX_RETRYABLE_ATTEMPTS {
                let exhausted = format!(
                    "retries exhausted after {attempts} attempts: {reason}"
                );
                runtime
                    .ledger
                    .mark_terminal(&job_id, JobLedgerStatus::Permanent, Some(&exhausted))
                    .await?;
                console_log!("job {job_id} permanently failed: {exhausted}");
                message.ack();
            } else {
                let delay = retry_delay_secs(attempts);
                console_log!(
                    "job {job_id} retryable (attempt {attempts}), re-queueing in {delay}s: {reason}"
                );
                let options = QueueRetryOptionsBuilder::new()
                    .with_delay_seconds(delay)
                    .build();
                message.retry_with_options(&options);
            }
        }
    }
    Ok(())
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker runtime error: {error}"))
}
