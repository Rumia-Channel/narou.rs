//! Scheduled auto-update planner (Phase 8).
//!
//! Cron only plans bounded pages and sends discrete `Update` jobs through the
//! shared Worker dispatcher. The D1 checkpoint is a resumable cursor: the
//! current generation is deduplicated, while a later generation takes over a
//! stale running claim and repairs pending ledger rows from the beginning.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use narou_rs::application::{
    events::FreezeStore, CheckpointClaim, JobKind, JobPlan, JobTarget,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelFilter, NovelId};
use serde_yaml::Value as YamlValue;
use worker::{console_log, Env, ScheduledEvent};

use crate::composition::WorkerRuntime;

/// Bounded D1 scan page for one planner invocation.
pub const AUTO_UPDATE_PAGE_SIZE: usize = 100;

/// Cron entry point: decide, claim the generation, plan pages, enqueue.
pub async fn run_scheduled_plan(event: ScheduledEvent, env: &Env) -> Result<()> {
    let runtime = WorkerRuntime::build(env).map_err(|error| {
        NarouError::Platform(format!("Worker runtime error: {error}"))
    })?;
    // The cron trigger time in milliseconds is the run generation: Cloudflare
    // redelivers the *same* scheduled event on retries, so equal generations
    // mean "already planned this run".
    let generation = event.schedule() as u64;
    plan_auto_update(&runtime, generation).await
}

/// Plan + enqueue one bounded auto-update page.
pub async fn plan_auto_update(runtime: &WorkerRuntime, generation: u64) -> Result<()> {
    let loaded = runtime.checkpoint.load().await?;
    let mut checkpoint = if loaded.state
        == narou_rs::application::CheckpointState::Running
    {
        if loaded.generation == generation {
            // A redelivered cron event must not plan the same page twice. The
            // next minute's generation will repair any pending rows that were
            // left behind by a crash before the checkpoint save.
            console_log!("auto-update generation {generation} already running; skipping");
            return Ok(());
        }
        // A previous generation crashed after claiming its run. A new
        // generation takes over with a fresh cursor; active ledger dedupe
        // prevents already-dispatched jobs from stacking duplicates.
        match runtime
            .checkpoint
            .claim(generation, &runtime.now_rfc3339())
            .await?
        {
            CheckpointClaim::Duplicate => return Ok(()),
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
        }
    } else {
        let services = &runtime.services;
        let enabled = settings_bool(services, "update.auto-schedule.enable").await?;
        let schedule_string = settings_string(services, "update.auto-schedule").await?;
        let interval_secs = settings_f64(services, "update.interval").await?;
        let timezone = settings_timezone(services, "update.auto-schedule.timezone").await?;
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
        let now = runtime.now_rfc3339();
        match runtime.checkpoint.claim(generation, &now).await? {
            CheckpointClaim::Duplicate => {
                console_log!("auto-update generation {generation} already claimed; skipping");
                return Ok(());
            }
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
        }
    };

    let done = plan_page(runtime, &mut checkpoint).await?;
    if done {
        checkpoint = checkpoint.finish(&runtime.now_rfc3339());
    }
    runtime.checkpoint.save(&checkpoint).await
}

/// Scan and dispatch at most one bounded page. The cursor is advanced only
/// after every plan in the page has been sent or durably blocked.
async fn plan_page(
    runtime: &WorkerRuntime,
    checkpoint: &mut narou_rs::application::SchedulerCheckpoint,
) -> Result<bool> {
    let frozen_ids: HashSet<i64> = runtime.freeze.frozen_ids().await?;
    let after_id = checkpoint.cursor.map(NovelId);
    let ids = runtime
        .novels
        .scan_ids(&NovelFilter::default(), after_id, AUTO_UPDATE_PAGE_SIZE)
        .await?;
    if ids.is_empty() {
        return Ok(true);
    }
    let page_full = ids.len() == AUTO_UPDATE_PAGE_SIZE;
    let next_cursor = ids.last().map(|id| id.0);
    let plans: Vec<JobPlan> = ids
        .into_iter()
        .filter(|id| !frozen_ids.contains(&id.0))
        .map(|id| JobPlan {
            kind: JobKind::Update,
            target: JobTarget::Id(id),
            options: Vec::new(),
        })
        .collect();
    runtime.enqueue_batch(&plans).await?;
    if let Some(cursor) = next_cursor {
        *checkpoint = checkpoint.advance(cursor);
    }
    console_log!(
        "auto-update: dispatched page up to id {:?} (full: {page_full}, jobs: {})",
        next_cursor,
        plans.len()
    );
    Ok(!page_full)
}

async fn settings_timezone(
    services: &narou_rs::application::AppServices,
    name: &str,
) -> Result<chrono_tz::Tz> {
    let value = settings_string(services, name).await?;
    if value.trim().is_empty() {
        return Ok(chrono_tz::Asia::Tokyo);
    }
    value.parse().map_err(|_| {
        NarouError::Platform(format!(
            "invalid scheduler timezone {value:?}; expected an IANA timezone"
        ))
    })
}

async fn settings_bool(services: &narou_rs::application::AppServices, name: &str) -> Result<bool> {
    match settings_value(services, name).await? {
        Some(YamlValue::Bool(value)) => Ok(value),
        Some(YamlValue::String(value)) => Ok(matches!(
            value.as_str(),
            "true" | "yes" | "on" | "1"
        )),
        Some(_) | None => Ok(false),
    }
}

async fn settings_string(services: &narou_rs::application::AppServices, name: &str) -> Result<String> {
    match settings_value(services, name).await? {
        Some(YamlValue::String(value)) => Ok(value),
        Some(YamlValue::Number(value)) => Ok(value.to_string()),
        Some(_) | None => Ok(String::new()),
    }
}

async fn settings_f64(services: &narou_rs::application::AppServices, name: &str) -> Result<Option<f64>> {
    match settings_value(services, name).await? {
        Some(YamlValue::Number(value)) => Ok(value.as_f64()),
        Some(YamlValue::String(value)) => Ok(value.parse::<f64>().ok()),
        Some(_) | None => Ok(None),
    }
}

async fn settings_value(
    services: &narou_rs::application::AppServices,
    name: &str,
) -> Result<Option<YamlValue>> {
    services.settings.get(name).await.map_err(|error| {
        NarouError::Platform(format!("cannot read setting {name:?}: {error}"))
    })
}
