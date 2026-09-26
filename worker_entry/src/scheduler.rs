//! Scheduled auto-update planner (Phase 8).
//!
//! Cron only plans bounded pages and sends discrete `Update` jobs through the
//! shared Worker dispatcher. The D1 checkpoint is a resumable cursor: the
//! current generation is deduplicated, while a later generation takes over a
//! stale running claim and repairs pending ledger rows from the beginning.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use narou_rs::application::{
    events::FreezeStore, CheckpointClaim, JobKind, JobPlan, JobTarget, SchedulerCheckpoint,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelFilter, NovelId, NovelQuery, NovelSort, NovelSortKey};
use serde_yaml::Value as YamlValue;
use worker::{console_log, Env, ScheduledEvent};

use crate::composition::WorkerRuntime;

/// Bounded D1 scan page for one planner invocation.
pub const AUTO_UPDATE_PAGE_SIZE: usize = 100;

/// Cron is only a probe. The persisted checkpoint owns the logical
/// generation and cursor, so every minute does not reset an in-flight run.
pub async fn run_scheduled_plan(event: ScheduledEvent, env: &Env) -> Result<()> {
    let runtime = WorkerRuntime::build(env).await.map_err(|error| {
        NarouError::Platform(format!("Worker runtime error: {error}"))
    })?;
    plan_auto_update(&runtime, event.schedule() as u64).await
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
        let services = &runtime.services;
        // One scoped load instead of four app_state scans per cron tick.
        let mut values = services
            .settings
            .get_many(&[
                "update.auto-schedule.enable",
                "update.auto-schedule",
                "update.interval",
                "update.auto-schedule.timezone",
            ])
            .await
            .map_err(|error| {
                NarouError::Platform(format!("cannot read scheduler settings: {error}"))
            })?
            .into_iter();
        let enabled = yaml_bool(values.next().flatten());
        let schedule_string = yaml_string(values.next().flatten());
        let interval_secs = yaml_f64(values.next().flatten());
        let timezone = yaml_timezone(values.next().flatten())?;
        let policy = services
            .scheduler
            .policy(enabled, &schedule_string, interval_secs, Vec::new());
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
    let raw = match runtime.services.settings.get("update.sort-by").await {
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

fn yaml_f64(value: Option<YamlValue>) -> Option<f64> {
    match value {
        Some(YamlValue::Number(value)) => value.as_f64(),
        Some(YamlValue::String(value)) => value.parse::<f64>().ok(),
        Some(_) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{SchedulerCheckpoint, apply_page_dispatch};
    use narou_rs::error::NarouError;

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
