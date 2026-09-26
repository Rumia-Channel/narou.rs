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
    classify_failure, messages, ExecutionPhase, JobFailureClass, JobId, JobKind, JobPlan,
    JobTarget, WorkerExecutionCheckpoint,
};
use narou_rs::downloader::{
    DownloadExecutionOptions, DownloadResult, Downloader, UpdateStatus,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelId, NovelRepository};

use crate::budget::WorkerBudget;
use crate::composition::WorkerRuntime;
use crate::push_hub::{HubProgress, PushHubClient, PushHubSink, echo};


/// Bounded wall-clock budget per job. It is checked only before starting the
/// next section, so a section's fetch and persistence remain atomic from the
/// worker's point of view.
pub const JOB_TIME_BUDGET: Duration = Duration::from_secs(60 * 10);

/// Typed outcome of executing one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Succeeded,
    /// Terminal skip: the job was eligible but its target was disallowed
    /// (frozen novel without `--force`). Ledgers as `Partial` because that
    /// is the native shape — a single-target frozen job exits the child
    /// process with mistook=1, which the web worker reports as partial
    /// rather than failed.
    Skipped { reason: String },
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

fn resume_section_for_checkpoint(
    job_id: &JobId,
    novel_id: Option<narou_rs::platform::NovelId>,
    checkpoint: Option<&WorkerExecutionCheckpoint>,
) -> std::result::Result<Option<usize>, String> {
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    if checkpoint.job_id != *job_id {
        return Err(format!(
            "execution checkpoint belongs to job {}, not {}",
            checkpoint.job_id, job_id
        ));
    }
    if checkpoint.novel_id.is_some() && checkpoint.novel_id != novel_id {
        return Err("execution checkpoint belongs to a different novel".to_string());
    }
    match checkpoint.phase {
        ExecutionPhase::Planned | ExecutionPhase::Finished => Ok(None),
        ExecutionPhase::Fetching | ExecutionPhase::Persisting => {
            let Some(index) = checkpoint.next_section_index else {
                return Err("execution checkpoint has no section cursor".to_string());
            };
            usize::try_from(index).map(Some).map_err(|_| {
                format!("execution checkpoint section index {index} is not representable")
            })
        }
    }
}

/// Execute one job and classify the outcome.
///
/// `downloader` is owned by the caller for the duration of a queue batch —
/// `runtime` supplies the platform capabilities the lifecycle hooks need
/// (freeze check, post-update record writes, checkpointing ledger).
pub async fn execute_job(
    downloader: &mut Downloader,
    job: &JobPlan,
    job_id: &JobId,
    checkpoint: Option<&WorkerExecutionCheckpoint>,
    runtime: &WorkerRuntime,
    execution_token: &str,
    push: &PushHubClient,
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
    let resume_from_section =
        match resume_section_for_checkpoint(job_id, novel_id, checkpoint) {
            Ok(index) => index,
            Err(reason) => return JobOutcome::Permanent { reason },
        };

    // 最後の防波堤: 投入側 (job_actions / スケジューラ) での除外が漏れても、
    // `--force` 無しの Download/Update は凍結中の小説を触らない。
    // native の `commands::{update,download}` の凍結拒否と同じ規則で、単発
    // ジョブは native と同じ通知行を出して終了する。
    if !force && matches!(job.kind, JobKind::Download | JobKind::Update) {
        match resolve_novel_id(runtime.novels.as_ref(), &job.target).await {
            Some(id) if runtime.services.novel_actions.is_frozen(id).await => {
                let title = runtime
                    .novels
                    .get(id)
                    .await
                    .ok()
                    .flatten()
                    .map(|record| record.title)
                    .unwrap_or_default();
                let line = match job.kind {
                    JobKind::Update => messages::update::frozen_id(id.0, title),
                    _ => messages::download::frozen_abort(title),
                };
                console_log!("job {job_id} skipped: {line}");
                push.broadcast_best_effort(&[echo(&line, "stdout")]).await;
                // native の単発 frozen 経路は子プロセスを mistook=1 で終了
                // させ、Web worker がそれを「部分完了」(queue_partial) として
                // 記録するので、ここでも terminal の Partial に写す
                // (queue_failed にはしない)。
                return JobOutcome::Skipped { reason: line };
            }
            _ => {}
        }
    }

    // 進捗バーを PushHub 経由で UI へ (native の WebProgress 相当)。
    // topic は job kind、scope は job id — consumer が終端で同じ scope の
    // progressbar.clear を送って消す。
    downloader.set_progress(Box::new(HubProgress::new(
        push.clone(),
        job.kind.as_str(),
        job_id.as_str(),
    )));
    // native のコンソール行 = PushHub の echo イベント。Downloader 内の
    // report_line/report_warn と、sink を引き回せない既定 sink 経路の
    // 両方へ同じバッファを指す sink をインストールする
    // (`PushHubSink::install` が既定 sink 登録まで担う — convert ジョブも同じ)。
    let push_sink = PushHubSink::install(push.clone());
    downloader.set_message_sink(push_sink.clone());
    let mut budget = WorkerBudget::new(JOB_TIME_BUDGET, &runtime.subrequests)
        .with_checkpoints(
            runtime.ledger.clone(),
            job_id.clone(),
            execution_token.to_string(),
            novel_id,
        );
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
            push.broadcast_best_effort(&[echo(
                &format!("再開チェックポイントが不正です ({reason})。先頭から実行し直します"),
                "stdout",
            )])
            .await;
            let mut restart_budget = WorkerBudget::new(JOB_TIME_BUDGET, &runtime.subrequests);
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
    // バッファに積まれた行をジョブの区切りでまとめて送信する。
    push_sink.drain().await;
    match result {
        Err(NarouError::DownloadBudgetExpired {
            next_section_index,
        }) => {
            let limit = budget.exceeded_reason().unwrap_or("wall-clock");
            JobOutcome::Partial {
                reason: format!(
                    "section-boundary {limit} budget expired before section {next_section_index} ({} subrequests used)",
                    runtime.subrequests.used()
                ),
                checkpoint: WorkerExecutionCheckpoint::planned(job_id.clone(), novel_id)
                    .advance(ExecutionPhase::Fetching, Some(next_section_index as u64)),
            }
        }
        result => classify_result(result, job, job_id, runtime).await,
    }
}

/// Resolve a job target to a stored novel id (native `get_data_by_target`
/// parity for the freeze check): `Id` is direct, `Ncode`/`Url` resolve
/// through the repository, `All` is unreachable (rejected by
/// [`unsupported_reason`]). Resolution failures behave like native —
/// "no record" means "not frozen", so the download proceeds and reports
/// its own not-found outcome.
pub(crate) async fn resolve_novel_id(novels: &dyn NovelRepository, target: &JobTarget) -> Option<NovelId> {
    match target {
        JobTarget::Id(id) => Some(*id),
        JobTarget::Ncode(ncode) => novels
            .find_by_ncode(ncode)
            .await
            .ok()
            .flatten()
            .map(|record| NovelId(record.id)),
        JobTarget::Url(url) => novels
            .find_by_toc_url(url)
            .await
            .ok()
            .flatten()
            .map(|record| NovelId(record.id)),
        JobTarget::All => None,
    }
}

async fn classify_result(
    result: Result<DownloadResult>,
    job: &JobPlan,
    job_id: &JobId,
    runtime: &WorkerRuntime,
) -> JobOutcome {
    match result {
        Ok(result) => match result.status {
            UpdateStatus::Ok | UpdateStatus::None => {
                // native `commands::update` parity: a checked update —
                // changed or not — clears `modified`, stamps
                // `last_check_date`, and syncs the `end` tag.
                if job.kind == JobKind::Update {
                    if let Err(error) = runtime
                        .services
                        .novel_actions
                        .record_update_check(
                            NovelId(result.id),
                            runtime.clock.now_utc(),
                        )
                        .await
                    {
                        console_log!(
                            "job {job_id}: post-update bookkeeping failed for novel {}: {error}",
                            result.id
                        );
                    }
                }
                JobOutcome::Succeeded
            }
            UpdateStatus::Canceled => JobOutcome::Blocked {
                reason: format!(
                    "job {} canceled: an interactive decision (auth/adult/digest) has no terminal on the worker",
                    job.kind.as_str()
                ),
            },
            UpdateStatus::Failed => {
                // The only `Failed` producer is the existing-novel TOC 404
                // (the downloader already echoed "小説が削除されているか
                // 非公開な可能性があります"). Native freezes the novel with
                // `frozen` + `404` tags so daily auto-update stops retrying
                // it — do the same through the shared service.
                if let Err(error) = runtime
                    .services
                    .novel_actions
                    .mark_not_found_and_freeze(NovelId(result.id))
                    .await
                {
                    console_log!(
                        "job {job_id}: auto-freeze after 404 failed for novel {}: {error}",
                        result.id
                    );
                }
                JobOutcome::Permanent {
                    reason: "download failed: novel was not found or the update was refused"
                        .to_string(),
                }
            }
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
#[cfg(test)]
mod tests {
    use super::resume_section_for_checkpoint;
    use narou_rs::application::{
        ExecutionPhase, JobId, WorkerExecutionCheckpoint,
    };
    use narou_rs::platform::NovelId;

    #[test]
    fn checkpoint_claim_data_resolves_to_downloader_resume_index() {
        let job_id = JobId("job-1".into());
        let checkpoint = WorkerExecutionCheckpoint::planned(
            job_id.clone(),
            Some(NovelId(42)),
        )
        .advance(ExecutionPhase::Fetching, Some(3));
        assert_eq!(
            resume_section_for_checkpoint(&job_id, Some(NovelId(42)), Some(&checkpoint))
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            resume_section_for_checkpoint(
                &JobId("other-job".into()),
                Some(NovelId(42)),
                Some(&checkpoint),
            )
            .unwrap_err(),
            "execution checkpoint belongs to job job-1, not other-job"
        );
    }
}
