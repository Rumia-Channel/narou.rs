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
use narou_rs::platform::{NovelId, NovelObjectKeys, NovelRepository};

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

    // `webui.debug-mode` なら、このジョブの間は詳細ログ (挿絵の取り込み判断や
    // EPUB 組立の内訳など) を Web コンソールへ流す。ジョブごとに 1 回だけ
    // D1 の設定を読む (convert ジョブもこの経路を通る)。
    narou_rs::application::debug::set_enabled(crate::consumer::webui_debug_mode(runtime).await);

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
            Some(id) if runtime.write_services().novel_actions.is_frozen(id).await => {
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

    // native `update.interval`: 同じドメインの作品開始を一定時間空ける。
    // 凍結で読み飛ばす作品は native 同様に間隔を消費しない (frozen 判定の後)。
    if job.kind == JobKind::Update {
        let domain = update_domain(runtime, &job.target).await;
        if let Err(error) = runtime.pace_update_start(&domain).await {
            return JobOutcome::Retryable {
                reason: format!("update start pacing failed: {error}"),
            };
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
                &messages::jobs::resume_checkpoint_invalid(&reason),
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
    // 1 件分の結果行は native `commands::{download,update}` と同じ文言を
    // 共有実装 (`emit_download_result_lines`) で出す。エラー系も native と
    // 同じ行を使う (download は `  {error}`、update は `ID:{id} {title} の
    // 更新は失敗しました` + debug-mode 時の `  Error detail: {error}`)。
    // 予算切れは下の Partial 分岐が理由を説明するので除く。
    match &result {
        Ok(result) => {
            // native `commands::update` は `DownloadResult.title` が空のとき
            // キャンセル行のタイトルをレコードから引き直すので、同じ解決を
            // ここでも行う (Download では使われない)。
            let canceled_title = if job.kind == JobKind::Update
                && result.status == UpdateStatus::Canceled
                && result.title.is_empty()
            {
                runtime
                    .novels
                    .get(NovelId(result.id))
                    .await
                    .ok()
                    .flatten()
                    .map(|record| record.title)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            narou_rs::application::emit_download_result_lines(
                push_sink.as_ref(),
                job.kind,
                result,
                Some(&canceled_title),
            );
        }
        Err(error) if !matches!(error, NarouError::DownloadBudgetExpired { .. }) => {
            // ネイティブはエラーも stdout に出す (`error()` -> `$stdout.error`)。
            // Web UI は target_console が stdout 以外だと 2 番目のコンソールへ
            // 流すため、ここも stdout に揃える。
            use narou_rs::application::messages::MessageSink as _;
            // `SuspendDownload` は対話/中断 (native は update/download 全体を
            // 中断して専用行を出す)。Worker ではジョブ単位なので、kind ごとに
            // native の中断行を出してから終端分類へ進む。
            let line = match (job.kind, error) {
                (_, NarouError::SuspendDownload(_)) if job.kind == JobKind::Update => {
                    messages::update::update_interrupted().to_string()
                }
                (_, NarouError::SuspendDownload(_)) => {
                    messages::download::download_interrupted().to_string()
                }
                (JobKind::Update, _) => {
                    let title = match novel_id {
                        Some(id) => runtime
                            .novels
                            .get(id)
                            .await
                            .ok()
                            .flatten()
                            .map(|record| record.title)
                            .unwrap_or_default(),
                        None => job.target.as_str(),
                    };
                    messages::update::update_failed(
                        novel_id.map(|id| id.0).unwrap_or_default(),
                        title,
                    )
                }
                (_, _) => messages::indented_error(error),
            };
            push_sink.emit(messages::Stream::Stdout, &line);
            if job.kind == JobKind::Update && crate::consumer::webui_debug_mode(runtime).await {
                push_sink.emit(
                    messages::Stream::Stdout,
                    &messages::update::error_detail(error),
                );
            }
        }
        _ => {}
    }
    // ダウンロードが更新した section hash cache を D1 へ書き戻す
    // (強更新の判定ヒント。失敗は再 DL だけに効くのでログだけ残す)。
    if let Err(error) = runtime.persist_pending_section_hash_cache().await {
        console_log!("job {job_id}: section hash cache write-back failed: {error}");
    }
    // バッファに積まれた行の送信は全ての emit (上の結果行と
    // `classify_result` 内の変換案内/失敗行) が終わったジョブの区切りで
    // まとめて行う。先に drain すると変換系の行が取り残される。
    let outcome = match result {
        Err(NarouError::DownloadBudgetExpired {
            next_section_index,
        }) => {
            let limit = budget.exceeded_reason().unwrap_or("wall-clock");
            JobOutcome::Partial {
                reason: messages::jobs::budget_expired_partial(
                    limit,
                    next_section_index,
                    runtime.subrequests.used(),
                ),
                checkpoint: WorkerExecutionCheckpoint::planned(job_id.clone(), novel_id)
                    .advance(ExecutionPhase::Fetching, Some(next_section_index as u64)),
            }
        }
        result => classify_result(result, job, job_id, runtime, push_sink.as_ref()).await,
    };
    // バッファに積まれた行をジョブの区切りでまとめて送信する。
    push_sink.drain().await;
    // ジョブの sink を isolate から外す (残すと以後の emit_default が
    // 死んだバッファに書き込む)。
    narou_rs::application::messages::take_default_sink();
    outcome
}

/// native `commands::update` の `UNKNOWN_DOMAIN_KEY` と同じ見出し語。ドメインを
/// 持たないレコード (取得直後など) は 1 つのバケットにまとめて直列化する。
const UNKNOWN_DOMAIN_KEY: &str = "__unknown__";

/// `update.interval` を数えるためのドメイン。native `collect_record_domains`
/// と同じく `record.domain` を使い、空・未解決は共通バケットへ寄せる。
async fn update_domain(runtime: &WorkerRuntime, target: &JobTarget) -> String {
    let Some(id) = resolve_novel_id(runtime.novels.as_ref(), target).await else {
        return UNKNOWN_DOMAIN_KEY.to_string();
    };
    runtime
        .novels
        .get(id)
        .await
        .ok()
        .flatten()
        .and_then(|record| record.domain)
        .filter(|domain| !domain.is_empty())
        .unwrap_or_else(|| UNKNOWN_DOMAIN_KEY.to_string())
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
    sink: &dyn messages::MessageSink,
) -> JobOutcome {
    match result {
        Ok(result) => match result.status {
            UpdateStatus::Ok | UpdateStatus::None => {
                // native `commands::update` parity: a checked update —
                // changed or not — clears `modified`, stamps
                // `last_check_date`, and syncs the `end` tag.
                if job.kind == JobKind::Update
                    && let Err(error) = runtime
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
                // native `commands::{download,update}` は成功した小説を直後に
                // 変換する (`narou download` → `narou convert`)。Worker では
                // `/api/convert` と同じ Convert ジョブを積む。投入自体が
                // 失敗したらジョブ全体を Retryable に倒す — 変換テキストが
                // 無いまま「成功」で閉じないため。
                match enqueue_followup_convert(runtime, job, &result, sink).await {
                    Ok(()) => JobOutcome::Succeeded,
                    // native は変換失敗を `convert_error_line` でユーザーへ
                    // 通知する (`commands::{download,update}` の
                    // `convert_error_line` emit) ので、ここでも同じ行を出す。
                    Err(reason) => {
                        sink.emit(
                            messages::Stream::Stdout,
                            &messages::download::convert_error_line(&reason),
                        );
                        JobOutcome::Retryable { reason }
                    }
                }
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
/// ダウンロード成功後の自動変換 (native `commands::{download,update}::auto_convert`
/// の Web 経路と同じ形)。`/api/convert` が積むのと同じ Convert ジョブを
/// `enqueue_plan` で積む。台帳の dedupe (`kind:target:options`) が同じ投入を
/// べき等にするので、再配信で二重に走ることはない。
///
/// 戻り値 `Err` は「変換ジョブを積めなかった」= 台帳/キューの障害。
/// 呼び出し側はジョブ全体をリトライ対象に倒し、変換テキストが無いまま
/// 成功で閉じない。
async fn enqueue_followup_convert(
    runtime: &WorkerRuntime,
    job: &JobPlan,
    result: &DownloadResult,
    sink: &dyn messages::MessageSink,
) -> std::result::Result<(), String> {
    // 連鎖するのは Download / Update のみ。Convert 自身がここを通らない
    // (consumer が execute_convert に分岐する) ので無限連鎖は起きない。
    if !matches!(job.kind, JobKind::Download | JobKind::Update) {
        return Ok(());
    }
    // `--no-convert` / `-n` は native と同じく変換を完全に止める。
    if has_option(&job.options, &["--no-convert", "-n"]) {
        return Ok(());
    }
    let convert = match result.status {
        UpdateStatus::Ok => {
            // `--convert-only-new-arrival` / `-a` オプションと
            // `update.convert-only-new-arrival` 設定は Update の `Ok` にだけ
            // 効く (native `commands::update` の needs_convert 分岐と同じ)。
            // Download は native 同様、取得成功なら無条件で変換する。
            let only_new = job.kind == JobKind::Update
                && (has_option(&job.options, &["--convert-only-new-arrival", "-a"])
                    || convert_only_new_arrival_setting(runtime).await);
            !only_new || result.new_arrivals
        }
        // `None` (差分なし) の native 更新パリティは `convert_failure`
        // フラグが立つときだけ再変換する。加えて「変換テキストが無い」
        // (ダウンロード直後/前回変換の取りこぼし) も変換する — EPUB
        // 配信がこのオブジェクトを読むので、無いまま進めない。
        UpdateStatus::None => {
            // native `commands::update` の needs_convert: `convert_failure`
            // フラグが立っていれば再変換し、そのとき
            // `reconvert_after_failure` の行を出す。
            let convert = needs_convert_after_unchanged(runtime, NovelId(result.id)).await;
            if convert {
                let reconvert = runtime
                    .novels
                    .get(NovelId(result.id))
                    .await
                    .ok()
                    .flatten()
                    .map(|record| record.convert_failure)
                    .unwrap_or(false);
                if reconvert {
                    // native は web mode で `colored(..., "yellow")` が
                    // goldenrod の span になる。同じ出力を共有形で出す
                    // (`termcolor` は native-runtime 限定なのでここでは
                    // console 形を使う)。
                    sink.emit(
                        messages::Stream::Stdout,
                        &messages::jobs::console_reconvert_note(),
                    );
                }
            }
            convert
        }
        UpdateStatus::Canceled | UpdateStatus::Failed => false,
    };
    if !convert {
        return Ok(());
    }

    let outcome = runtime
        .enqueue_plan(JobPlan {
            kind: JobKind::Convert,
            target: JobTarget::Id(NovelId(result.id)),
            options: Vec::new(),
        })
        .await
        .map_err(|error| format!("failed to enqueue convert for novel {}: {error}", result.id))?;
    if let Some(reason) = outcome.blocked {
        return Err(format!(
            "convert job for novel {} could not be dispatched: {reason}",
            result.id
        ));
    }
    console_log!(
        "job: queued convert job {} for novel {} (sent={})",
        outcome.job_id,
        result.id,
        outcome.sent
    );
    Ok(())
}

fn has_option(options: &[String], names: &[&str]) -> bool {
    options.iter().any(|option| names.contains(&option.as_str()))
}

/// `update.convert-only-new-arrival` (native `load_local_setting_bool` 相当)。
/// 読めないときは native の設定なしと同じ `false` に倒す。
async fn convert_only_new_arrival_setting(runtime: &WorkerRuntime) -> bool {
    runtime
        .write_services()
        .settings
        .get("update.convert-only-new-arrival")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// 差分なし (`UpdateStatus::None`) での再変換判定。native はレコードの
/// `convert_failure` フラグを見る。それに加えて「変換テキストのオブジェクトが
/// 無い」場合も変換する — 失敗の取りこぼしや手動削除を自己修復し、
/// `download.epub` の 409 を起こさないため。
async fn needs_convert_after_unchanged(runtime: &WorkerRuntime, id: NovelId) -> bool {
    let Ok(Some(record)) = runtime.novels.get(id).await else {
        // レコードを読めないならキュー自身も読めない可能性が高い。
        // 保守側に倒さず、後続実行で判定し直す。
        return false;
    };
    if record.convert_failure {
        return true;
    }
    let Ok(keys) = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
    else {
        return false;
    };
    // 変換済みテキストを保存しない構成 (`convert.keep-txt=false`) では
    // そもそも保存されないので、変換を挟む意味がない (EPUB 取得時に組む)。
    if !crate::convert::keep_converted_text(runtime).await {
        return false;
    }
    // 存在確認がエラーでも「無い」とみなして変換に回す: ストア障害時でも
    // convert ジョブが queue_failed として失敗を表面化する (黙って
    // 「テキスト無しのまま成功」にしないため)。
    // 削除・上書きを伴う変換判定なので primary 直行のストアで存在確認する。
    !runtime.write_objects().exists(&keys.converted_text()).await.unwrap_or(false)
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
