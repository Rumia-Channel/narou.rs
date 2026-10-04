//! Scheduled auto-update planner (Phase 8).
//!
//! Cron only plans bounded pages and sends discrete `Update` jobs through the
//! shared Worker dispatcher. The D1 checkpoint is a resumable cursor: the
//! current generation is deduplicated, while a later generation takes over a
//! stale running claim and repairs pending ledger rows from the beginning.

use std::collections::HashSet;

use chrono::{DateTime, SecondsFormat, Utc};
use narou_rs::application::{
    events::FreezeStore, retry_policy, CheckpointClaim, JobId, JobKind, JobPlan, JobTarget,
    Schedule, SchedulerCheckpoint, WorkerJobEnvelope,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelFilter, NovelId, NovelQuery, NovelSort, NovelSortKey};
use serde_yaml::Value as YamlValue;
use worker::{console_log, console_warn, Env, ScheduledEvent};

use crate::composition::WorkerRuntime;
use crate::consumer::{broadcast_failure, notify_queue_changed, CF_CONSUMER_MAX_RETRIES};
use crate::push_hub::PushHubClient;

/// Bounded D1 scan page for one planner invocation.
pub const AUTO_UPDATE_PAGE_SIZE: usize = 100;

/// Cron is only a probe. The persisted checkpoint owns the logical
/// generation and cursor, so every minute does not reset an in-flight run.
pub async fn run_scheduled_plan(event: ScheduledEvent, env: &Env) -> Result<()> {
    let runtime = WorkerRuntime::build(env).await.map_err(|error| {
        NarouError::Platform(format!("Worker runtime error: {error}"))
    })?;
    // DLQ 行きで取り残された行の回収は auto-update の有無と無関係に毎分走らせる。
    // 失敗しても planner は続行する (次の tick で再試行される)。
    let push = PushHubClient::new(env, runtime.subrequests.clone());
    if let Err(error) = reap_stuck_jobs(&runtime, &push).await {
        console_warn!("job reaper failed: {error}");
    }
    plan_auto_update(&runtime, event.schedule() as u64).await
}

/// 1 回の cron で reaper が処理する行数の上限。
/// 滞留がこれを超える場合は次の tick が続きを拾う (重い滞留で planner を
/// 押し出さないための制限)。
const REAPER_PAGE_SIZE: usize = 50;

/// アクティブ行の「予定された進行時点」に対する検出猶予。cron 間隔
/// (1 分) ちょうどでは CF Queue の遅延再配信が到着する前に拾い過ぎるので、
/// 配信・消費の遅れ分として 1 tick 分を上乗せする。重複配信が起きても
/// claim の Busy 分岐が直列化するため安全側に倒す。
const STUCK_GRACE: chrono::TimeDelta = chrono::TimeDelta::minutes(2);

/// Cloudflare Queues の再配信予算 (`max_retries`) を使い切って DLQ に落ちた
/// メッセージは二度と届かず、対応する台帳行は `pending` / `retryable` /
/// `running` (lease 切れ) のまま残る。UI では `pending`/`retryable` が
/// 「待機中」と表示されるため、配信が失われた行は「永遠に待機中」になる。
///
/// この reaper は毎分の cron で、バックオフ予定時刻 (`lease_until`) または
/// 更新時刻から `STUCK_GRACE` 経っても進んでいないアクティブ行を拾い、
///
/// - `retryable` でリトライ枠を使い切った行 (`attempts >= max_retries`) は
///   再投入しても次の失敗で即枯渇するだけなので、その場で `permanent` に
///   確定する (consumer と同じ `queue_failed` イベントを出す)。
/// - それ以外の行は新しい v2 envelope を Queue に再送する。新しい
///   メッセージは `max_retries` の配信予算を持ち直すので、以後は通常の
///   claim/retry 経路に復帰する。
///
/// 再送前の `touch_for_reenqueue` CAS と finalize の CAS が、生存している
/// 配信 (claim 済み) や並行 cron とのレースを排除する。
pub async fn reap_stuck_jobs(runtime: &WorkerRuntime, push: &PushHubClient) -> Result<()> {
    let stuck_before =
        (runtime.clock.now_utc() - STUCK_GRACE).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let stuck = runtime
        .ledger
        .stuck_jobs(&stuck_before, REAPER_PAGE_SIZE)
        .await?;
    if stuck.is_empty() {
        return Ok(());
    }
    let max_retries = effective_max_retries(runtime).await;
    let mut changed = false;
    for job in &stuck {
        match reap_one(runtime, push, job, &stuck_before, max_retries).await {
            Ok(row_changed) => changed |= row_changed,
            Err(error) => {
                console_warn!("job reaper failed for {}: {error}", job.job_id);
            }
        }
    }
    if changed {
        notify_queue_changed(push).await;
    }
    Ok(())
}

/// consumer の `settle_retryable` と同じ解釈で `queue.max-retries` を読み、
/// Cloudflare 側の配信上限 (`CF_CONSUMER_MAX_RETRIES`) にクランプする。
async fn effective_max_retries(runtime: &WorkerRuntime) -> u32 {
    let value = runtime
        .services
        .settings
        .get(retry_policy::MAX_RETRIES_KEY)
        .await
        .ok()
        .flatten();
    retry_policy::max_retries(value.as_ref()).min(CF_CONSUMER_MAX_RETRIES)
}

async fn reap_one(
    runtime: &WorkerRuntime,
    push: &PushHubClient,
    job: &crate::ledger::StuckJob,
    stuck_before: &str,
    max_retries: u32,
) -> Result<bool> {
    let attempts = u32::try_from(job.attempts).unwrap_or(u32::MAX);
    if job.status == "retryable" && attempts >= max_retries {
        let reason = format!(
            "retries exhausted after {attempts} attempts and the scheduled \
             redelivery was lost (dead-lettered): {}",
            job.last_error.as_deref().unwrap_or("unknown error")
        );
        if !runtime
            .ledger
            .finalize_stuck(&job.job_id, stuck_before, max_retries, &reason)
            .await?
        {
            // CAS が外れた = 生存している配信が先に claim した。何もしない。
            return Ok(false);
        }
        console_warn!("job {} permanently failed: {reason}", job.job_id);
        broadcast_failure(runtime, push, &JobId(job.job_id.clone()), &reason).await;
        return Ok(true);
    }

    // 生存している配信・並行 cron とのレースを CAS で排除してから送る。
    // `updated_at`/`lease_until` の bump は次の検出を STUCK_GRACE 先送りし、
    // send 失敗時の再送頻度も同じ粒度に揃える。
    if !runtime
        .ledger
        .touch_for_reenqueue(&job.job_id, stuck_before)
        .await?
    {
        return Ok(false);
    }
    let envelope = WorkerJobEnvelope::v2(JobId(job.job_id.clone()));
    if let Err(error) = runtime.queue.send(&envelope).await {
        console_warn!("job reaper could not re-send {}: {error}", job.job_id);
        return Ok(false);
    }
    console_log!(
        "job reaper re-enqueued {} (status={}, attempts={attempts})",
        job.job_id,
        job.status
    );
    Ok(true)
}

/// Plan + enqueue one bounded auto-update page.
///
/// `probe_generation` is retained only for source compatibility with the
/// previous helper signature. It is deliberately not persisted or compared
/// with the scheduler checkpoint.
pub async fn plan_auto_update(runtime: &WorkerRuntime, _probe_generation: u64) -> Result<()> {
    let loaded = runtime.checkpoint.load().await?;
    let now = runtime.now_rfc3339();
    let mut checkpoint = if loaded.state
        == narou_rs::application::CheckpointState::Running
    {
        match runtime.checkpoint.claim_running_probe(&now).await? {
            CheckpointClaim::Busy | CheckpointClaim::Duplicate => return Ok(()),
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
        }
    } else {
        // スケジュール判断は投入判断の材料なので primary + キャッシュ無しで読む。
        let services = &runtime.write_services();
        // One scoped load instead of three app_state scans per cron tick.
        let mut values = services
            .settings
            .get_many(&[
                "update.auto-schedule.enable",
                "update.auto-schedule",
                "update.auto-schedule.timezone",
            ])
            .await
            .map_err(|error| {
                NarouError::Platform(format!("cannot read scheduler settings: {error}"))
            })?
            .into_iter();
        let enabled = yaml_bool(values.next().flatten());
        let schedule_string = yaml_string(values.next().flatten());
        let schedule = match parse_auto_update_schedule(enabled, &schedule_string) {
            Ok(Some(schedule)) => schedule,
            Ok(None) => return Ok(()),
            Err(message) => {
                console_warn!("{message}");
                return Ok(());
            }
        };
        let timezone = yaml_timezone(values.next().flatten())?;
        let policy = services.scheduler.policy(enabled, schedule, Vec::new());
        let last_run = loaded
            .last_run
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let decision = services
            .scheduler
            .decide_in_timezone(&policy, last_run, timezone);
        if !decision.run_now {
            return Ok(());
        }
        match runtime
            .checkpoint
            .claim_new_generation(loaded.generation, loaded.generation.saturating_add(1), &now)
            .await?
        {
            CheckpointClaim::Busy | CheckpointClaim::Duplicate => return Ok(()),
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
        }
    };

    let planner_token = checkpoint.planner_token.clone().ok_or_else(|| {
        NarouError::Platform("claimed scheduler checkpoint has no planner token".to_string())
    })?;
    let done = plan_page(runtime, &mut checkpoint).await?;
    if done {
        checkpoint = checkpoint.finish(&runtime.now_rfc3339());
    }
    runtime
        .checkpoint
        .save_owned(&checkpoint, &planner_token)
        .await
}

/// Scan and dispatch at most one bounded page. The cursor is advanced only
/// after every plan in the page has been sent or durably blocked.
async fn plan_page(
    runtime: &WorkerRuntime,
    checkpoint: &mut narou_rs::application::SchedulerCheckpoint,
) -> Result<bool> {
    let frozen_ids: HashSet<i64> = runtime.freeze.frozen_ids().await?;
    let (ids, next_cursor) = scan_page(runtime, checkpoint).await?;
    if ids.is_empty() {
        return Ok(true);
    }
    let page_full = ids.len() == AUTO_UPDATE_PAGE_SIZE;
    let plans: Vec<JobPlan> = ids
        .into_iter()
        .filter(|id| !frozen_ids.contains(&id.0))
        .map(|id| JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Id(id),
            options: Vec::new(),
        })
        .collect();
    let dispatch = runtime.enqueue_batch(&plans).await.map(|_| ());
    apply_page_dispatch(checkpoint, next_cursor, dispatch)?;
    console_log!(
        "auto-update: dispatched page up to cursor {:?} (full: {page_full}, jobs: {})",
        next_cursor,
        plans.len()
    );
    Ok(!page_full)
}

/// 1 ページ分の対象 ID と次カーソルを返す。
///
/// `update.sort-by` が設定されていれば native `commands::update` と同じ順序
/// (`sort_update_ids_by_key`: 日付系は新しい順) で `novels.query` を捲る。
/// ソート順は id 順と一致しないためキーセットカーソルは張れず、checkpoint の
/// `cursor` は「消費済み行数 (offset)」になる。`scan_sort_key` に使用中の
/// キーを記録し、世代途中で設定が変わったときはカーソルをリセットして
/// 先頭から読み直す (再 enqueue されても台帳側で更新済み扱いになるだけ)。
/// 未設定・`id`・未知キー・読み取り失敗は既定の id キーセットスキャン。
async fn scan_page(
    runtime: &WorkerRuntime,
    checkpoint: &mut SchedulerCheckpoint,
) -> Result<(Vec<NovelId>, Option<i64>)> {
    let sort = configured_scan_sort(runtime).await;
    let sort_key = sort.map(|sort| sort.key.as_db_key());
    if checkpoint.scan_sort_key.as_deref() != sort_key {
        checkpoint.cursor = None;
        checkpoint.scan_sort_key = sort_key.map(str::to_string);
    }
    match sort {
        None => {
            let after_id = checkpoint.cursor.map(NovelId);
            let ids = runtime
                .novels
                .scan_ids(&NovelFilter::default(), after_id, AUTO_UPDATE_PAGE_SIZE)
                .await?;
            let next_cursor = ids.last().map(|id| id.0);
            Ok((ids, next_cursor))
        }
        Some(sort) => {
            let offset = usize::try_from(checkpoint.cursor.unwrap_or(0).max(0))
                .unwrap_or(usize::MAX);
            let records = runtime
                .novels
                .query(&NovelQuery::page(
                    NovelFilter::default(),
                    sort,
                    offset,
                    AUTO_UPDATE_PAGE_SIZE,
                ))
                .await?;
            let ids: Vec<NovelId> = records
                .iter()
                .map(|record| NovelId(record.id))
                .collect();
            let next_cursor = i64::try_from(offset + ids.len()).ok();
            Ok((ids, next_cursor))
        }
    }
}

/// `update.sort-by` の設定値を `NovelSort` へ解決する。
async fn configured_scan_sort(runtime: &WorkerRuntime) -> Option<NovelSort> {
    let raw = match runtime.write_services().settings.get("update.sort-by").await {
        Ok(value) => value,
        Err(error) => {
            console_log!(
                "auto-update: cannot read update.sort-by ({error}); scanning in id order"
            );
            return None;
        }
    };
    resolve_scan_sort(&yaml_string(raw))
}

/// `update.sort-by` の文字列を `NovelSort` に変換する。空・未知キー・`id`
/// (キーセットスキャンと同順) は `None` = 既定の id スキャン。キーの
/// 解釈 (日付系は降順) は platform 側の共有規則に委ねる。
fn resolve_scan_sort(configured: &str) -> Option<NovelSort> {
    let configured = configured.trim();
    if configured.is_empty() {
        return None;
    }
    if NovelSortKey::from_db_key(&configured.to_lowercase()).is_none() {
        console_log!("auto-update: unknown update.sort-by {configured:?}; scanning in id order");
        return None;
    }
    narou_rs::platform::resolve_update_scan_sort(configured)
}

/// Commit a successful queue page and only then advance the durable cursor.
///
/// A failed dispatch leaves the cursor untouched so the next planner run can
/// repair the pending ledger rows from the same page.
fn apply_page_dispatch(
    checkpoint: &mut SchedulerCheckpoint,
    next_cursor: Option<i64>,
    dispatch: Result<()>,
) -> Result<()> {
    dispatch?;
    if let Some(cursor) = next_cursor {
        *checkpoint = checkpoint.advance(cursor);
    }
    Ok(())
}

fn parse_auto_update_schedule(
    enabled: bool,
    schedule_string: &str,
) -> std::result::Result<Option<Schedule>, String> {
    if !enabled || schedule_string.trim().is_empty() {
        return Ok(None);
    }
    let schedule = Schedule::parse(schedule_string);
    if schedule.is_enabled() {
        Ok(Some(schedule))
    } else {
        Err(narou_rs::application::messages::jobs::auto_update_schedule_invalid(
            schedule_string,
        ))
    }
}

fn yaml_timezone(value: Option<YamlValue>) -> Result<chrono_tz::Tz> {
    let text = match value {
        Some(YamlValue::String(value)) => value,
        Some(YamlValue::Number(value)) => value.to_string(),
        Some(_) | None => String::new(),
    };
    if text.trim().is_empty() {
        return Ok(chrono_tz::Asia::Tokyo);
    }
    text.parse().map_err(|_| {
        NarouError::Platform(format!(
            "invalid scheduler timezone {text:?}; expected an IANA timezone"
        ))
    })
}

fn yaml_bool(value: Option<YamlValue>) -> bool {
    match value {
        Some(YamlValue::Bool(value)) => value,
        Some(YamlValue::String(value)) => {
            matches!(value.as_str(), "true" | "yes" | "on" | "1")
        }
        Some(_) | None => false,
    }
}

fn yaml_string(value: Option<YamlValue>) -> String {
    match value {
        Some(YamlValue::String(value)) => value,
        Some(YamlValue::Number(value)) => value.to_string(),
        Some(_) | None => String::new(),
    }
}


#[cfg(test)]
mod tests {
    use super::{SchedulerCheckpoint, apply_page_dispatch, parse_auto_update_schedule};
    use narou_rs::error::NarouError;
    #[test]
    fn auto_update_schedule_requires_enabled_nonempty_valid_value() {
        assert!(parse_auto_update_schedule(false, "0800").unwrap().is_none());
        assert!(parse_auto_update_schedule(true, "  ").unwrap().is_none());
        assert_eq!(
            parse_auto_update_schedule(true, "not-a-time").unwrap_err(),
            "自動アップデートスケジューラーの時刻指定が不正です: not-a-time"
        );
        assert_eq!(
            parse_auto_update_schedule(true, "0800").unwrap().unwrap().times,
            vec![(8, 0)]
        );
    }


    #[test]
    fn failed_page_dispatch_preserves_cursor_for_pending_repair() {
        let mut checkpoint = SchedulerCheckpoint::default().advance(100);
        let result = apply_page_dispatch(
            &mut checkpoint,
            Some(200),
            Err(NarouError::Platform("queue unavailable".into())),
        );
        assert!(result.is_err());
        assert_eq!(checkpoint.cursor, Some(100));

        apply_page_dispatch(&mut checkpoint, Some(200), Ok(())).unwrap();
        assert_eq!(checkpoint.cursor, Some(200));
    }

}
