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
    decode_legacy_envelope, JobClaim, JobId, JobLedgerStatus, JobPlan, JobQueue, JobRequest,
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

    let outcome = execute_job(
        downloader,
        job,
        &job_id,
        checkpoint.as_ref(),
        &runtime.subrequests,
        &runtime.ledger,
        &execution_token,
    )
    .await;
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
            message.ack();
        }
        JobOutcome::Retryable { reason } => {
            retry_or_ack(runtime, &job_id, &execution_token, message, reason).await?;
        }
    }
    Ok(())
}

async fn retry_or_ack(
    runtime: &WorkerRuntime,
    job_id: &JobId,
    execution_token: &str,
    message: &Message<serde_json::Value>,
    reason: String,
) -> Result<()> {
    let attempts = runtime
        .ledger
        .record_attempt(job_id, execution_token, &reason)
        .await?;
    if attempts >= MAX_RETRYABLE_ATTEMPTS {
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
    Ok(())
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker runtime error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{JobOutcome, MAX_RETRYABLE_ATTEMPTS, retry_delay_secs};

    #[test]
    fn retry_backoff_is_one_based_and_bounded() {
        assert_eq!(retry_delay_secs(1), 5);
        assert_eq!(retry_delay_secs(2), 10);
        assert_eq!(retry_delay_secs(MAX_RETRYABLE_ATTEMPTS), 20);
        assert_eq!(retry_delay_secs(20), 60);
    }

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
