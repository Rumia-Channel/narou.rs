//! Worker queue consumer execution (Phase 8).
//!
//! Executes one discrete [`JobPlan`] with the shared Downloader and reduces
//! the outcome to the ledger vocabulary:
//!
//! - success / no-update-needed → `Succeeded`
//! - interactive decision required (auth/adult/digest) → `Blocked`
//! - novel gone / invalid → `Permanent`
//! - transient transport failure → `Retryable` (bounded by the consumer)
//! - bounded time guard tripped → `Partial` (the shared Downloader cannot
//!   yet yield at section boundaries, so a partial result is recorded
//!   explicitly instead of running unbounded)
//!
//! No blanket retry-to-success and no `catch_unwind`: a panic propagates to
//! the queue runtime, which redelivers / dead-letters the message.

use std::time::Duration;

use futures::future::Either;
use narou_rs::application::{
    classify_failure, ExecutionPhase, JobFailureClass, JobId, JobPlan, JobTarget,
    WorkerExecutionCheckpoint,
};
use narou_rs::downloader::{DownloadResult, Downloader, UpdateStatus};
use narou_rs::error::Result;

/// Bounded wall-clock guard per job. The shared Downloader has no section
/// boundary yield yet, so a job that outlives this budget is recorded as an
/// explicit partial outcome instead of running unbounded.
pub const JOB_TIME_BUDGET: Duration = Duration::from_secs(60 * 10);

/// Typed outcome of executing one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Succeeded,
    /// Soft-budget checkpoint shape: the shared Downloader cannot yield at
    /// safe section boundaries yet, so the bounded time guard recorded an
    /// explicit partial outcome. Continuation is idempotent re-execution
    /// (sections are persisted as they are fetched and overwritten on rerun).
    Partial {
        reason: String,
        checkpoint: WorkerExecutionCheckpoint,
    },
    Blocked { reason: String },
    Permanent { reason: String },
    Retryable { reason: String },
}

/// Whether a job plan can be executed by this consumer at all.
pub fn unsupported_reason(job: &JobPlan) -> Option<String> {
    if !job.kind.is_worker_executable() {
        return Some(format!(
            "unsupported job kind {:?} on the worker (no subprocess support)",
            job.kind
        ));
    }
    if job.target == JobTarget::All {
        return Some(
            "auto-update must be planned into discrete per-novel jobs".to_string(),
        );
    }
    None
}

/// Execute one job and classify the outcome.
///
/// `downloader` is owned by the caller for the duration of a queue batch —
/// one consumer invocation never shares a mutable Downloader across
/// concurrent executions.
pub async fn execute_job(downloader: &mut Downloader, job: &JobPlan, job_id: &JobId) -> JobOutcome {
    if let Some(reason) = unsupported_reason(job) {
        return JobOutcome::Blocked { reason };
    }

    let force = job
        .options
        .iter()
        .any(|option| option == "--force" || option == "-f");
    let target = job.target.as_str();

    let download = Box::pin(downloader.download_novel_with_force(&target, force));
    let guard = Box::pin(worker::Delay::from(JOB_TIME_BUDGET));
    match futures::future::select(download, guard).await {
        Either::Left((result, _guard)) => classify_result(result, job),
        Either::Right(((), _download)) => {
            let novel_id = match job.target {
                JobTarget::Id(id) => Some(id),
                _ => None,
            };
            JobOutcome::Partial {
                reason: format!(
                    "bounded time guard tripped after {}s; shared downloader has no section-boundary yield yet",
                    JOB_TIME_BUDGET.as_secs()
                ),
                checkpoint: WorkerExecutionCheckpoint::planned(job_id.clone(), novel_id)
                    .advance(ExecutionPhase::Fetching, None),
            }
        }
    }
}

fn classify_result(result: Result<DownloadResult>, job: &JobPlan) -> JobOutcome {
    match result {
        Ok(result) => match result.status {
            UpdateStatus::Ok | UpdateStatus::None => JobOutcome::Succeeded,
            UpdateStatus::Canceled => JobOutcome::Blocked {
                reason: format!(
                    "job {} canceled: an interactive decision (auth/adult/digest) has no terminal on the worker",
                    job.kind.as_str()
                ),
            },
            UpdateStatus::Failed => JobOutcome::Permanent {
                reason: "download failed: novel was not found or the update was refused".to_string(),
            },
        },
        Err(error) => match classify_failure(&error) {
            JobFailureClass::Retryable => JobOutcome::Retryable {
                reason: error.to_string(),
            },
            JobFailureClass::Permanent => JobOutcome::Permanent {
                reason: error.to_string(),
            },
            JobFailureClass::Blocked => JobOutcome::Blocked {
                reason: error.to_string(),
            },
        },
    }
}
