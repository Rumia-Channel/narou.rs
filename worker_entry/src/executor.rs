//! Worker queue consumer execution (Phase 8).
//!
//! Executes one discrete [`JobPlan`] with the shared Downloader and reduces
//! the outcome to the ledger vocabulary:
//!
//! - success / no-update-needed → `Succeeded`
//! - interactive decision required (auth/adult/digest) → `Blocked`
//! - novel gone / invalid → `Permanent`
//! - transient transport failure → `Retryable` (bounded by the consumer)
//! - section-boundary budget expiration → `Partial` with the next section
//!   cursor (the in-flight section is never cancelled)
//!
//! No blanket retry-to-success and no `catch_unwind`: a panic propagates to
//! the queue runtime, which redelivers / dead-letters the message.

use std::time::Duration;
use worker::console_log;

use narou_rs::application::{
    classify_failure, ExecutionPhase, JobFailureClass, JobId, JobPlan, JobTarget,
    WorkerExecutionCheckpoint,
};
use narou_rs::downloader::{
    DownloadExecutionOptions, DownloadResult, Downloader, SectionBudget, UpdateStatus,
};
use narou_rs::error::{NarouError, Result};

/// Bounded wall-clock budget per job. It is checked only before starting the
/// next section, so a section's fetch and persistence remain atomic from the
/// worker's point of view.
pub const JOB_TIME_BUDGET: Duration = Duration::from_secs(60 * 10);

struct DeadlineBudget {
    deadline_ms: f64,
}

impl DeadlineBudget {
    fn new(budget: Duration) -> Self {
        Self {
            deadline_ms: js_sys::Date::now() + budget.as_secs_f64() * 1_000.0,
        }
    }
}

impl SectionBudget for DeadlineBudget {
    fn should_yield(&mut self, _next_section_index: usize) -> bool {
        js_sys::Date::now() >= self.deadline_ms
    }
}

/// Typed outcome of executing one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Succeeded,
    /// Soft-budget checkpoint produced before the next section starts. The
    /// checkpoint is resumable because preceding sections are already
    /// persisted and the next section index is explicit.
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
pub async fn execute_job(
    downloader: &mut Downloader,
    job: &JobPlan,
    job_id: &JobId,
    checkpoint: Option<&WorkerExecutionCheckpoint>,
) -> JobOutcome {
    if let Some(reason) = unsupported_reason(job) {
        return JobOutcome::Blocked { reason };
    }

    let force = job
        .options
        .iter()
        .any(|option| option == "--force" || option == "-f");
    let target = job.target.as_str();
    let novel_id = match job.target {
        JobTarget::Id(id) => Some(id),
        _ => None,
    };
    let resume_from_section = match checkpoint {
        None => None,
        Some(checkpoint) if checkpoint.job_id != *job_id => {
            return JobOutcome::Permanent {
                reason: format!(
                    "execution checkpoint belongs to job {}, not {}",
                    checkpoint.job_id, job_id
                ),
            };
        }
        Some(checkpoint)
            if checkpoint.novel_id.is_some()
                && checkpoint.novel_id != novel_id =>
        {
            return JobOutcome::Permanent {
                reason: "execution checkpoint belongs to a different novel".to_string(),
            };
        }
        Some(checkpoint) => match checkpoint.phase {
            ExecutionPhase::Planned | ExecutionPhase::Finished => None,
            ExecutionPhase::Fetching | ExecutionPhase::Persisting => {
                let Some(index) = checkpoint.next_section_index else {
                    return JobOutcome::Permanent {
                        reason: "execution checkpoint has no section cursor".to_string(),
                    };
                };
                match usize::try_from(index) {
                    Ok(index) => Some(index),
                    Err(_) => {
                        return JobOutcome::Permanent {
                            reason: format!(
                                "execution checkpoint section index {index} is not representable"
                            ),
                        };
                    }
                }
            }
        },
    };

    let mut budget = DeadlineBudget::new(JOB_TIME_BUDGET);
    let result = downloader
        .download_novel_with_execution_options(
            &target,
            DownloadExecutionOptions {
                force,
                budget: Some(&mut budget),
                resume_from_section,
            },
        )
        .await;
    let result = match result {
        Err(NarouError::DownloadResumeCorrupt(reason)) if resume_from_section.is_some() => {
            console_log!(
                "job {job_id}: invalid resume checkpoint ({reason}); restarting from section 0"
            );
            let mut restart_budget = DeadlineBudget::new(JOB_TIME_BUDGET);
            downloader
                .download_novel_with_execution_options(
                    &target,
                    DownloadExecutionOptions {
                        force,
                        budget: Some(&mut restart_budget),
                        resume_from_section: None,
                    },
                )
                .await
        }
        other => other,
    };
    match result {
        Err(NarouError::DownloadBudgetExpired {
            next_section_index,
        }) => JobOutcome::Partial {
            reason: format!(
                "section-boundary budget expired after {}s before section {next_section_index}",
                JOB_TIME_BUDGET.as_secs()
            ),
            checkpoint: WorkerExecutionCheckpoint::planned(job_id.clone(), novel_id)
                .advance(ExecutionPhase::Fetching, Some(next_section_index as u64)),
        },
        result => classify_result(result, job),
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
