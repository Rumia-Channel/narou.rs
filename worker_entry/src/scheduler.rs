//! Scheduled auto-update planner (Phase 8).
//!
//! The cron handler only *plans and enqueues* discrete `Update` jobs; it
//! never crawls anything itself. The decision comes from the shared
//! [`SchedulerService`] catch-up logic; the D1 checkpoint
//! (`app_state`, scope `scheduler`, key `auto_update`) records the
//! generation, keyset cursor, and run timestamps so a duplicate delivery of
//! the same cron generation never enqueues twice.
//!
//! Crash semantics (explicit, not claimed as true resume): if a run crashes
//! mid-scan, the *same* generation is never re-executed (duplicate delivery
//! is skipped) and the *next* generation re-scans from the start. This is
//! safe and duplicate-free because enqueueing goes through the ledger's
//! active dedupe key — already-active jobs return their existing id — so a
//! full rescan fills only the gaps. The `cursor` field is therefore
//! informational progress within a generation, not a cross-generation
//! resumption point.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use narou_rs::application::{
    events::FreezeStore, CheckpointClaim, JobKind, JobPlan, JobQueue, JobTarget,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{NovelFilter, NovelId};
use serde_yaml::Value as YamlValue;
use worker::{console_log, Env, ScheduledEvent};

use crate::composition::WorkerRuntime;

/// Bounded D1 scan page size for auto-update planning.
pub const AUTO_UPDATE_PAGE_SIZE: usize = 500;

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

/// Plan + enqueue one auto-update run (pure policy, bounded scans).
pub async fn plan_auto_update(runtime: &WorkerRuntime, generation: u64) -> Result<()> {
    let services = &runtime.services;
    let enabled = settings_bool(services, "update.auto-schedule.enable").await?;
    let schedule_string = settings_string(services, "update.auto-schedule").await?;
    let interval_secs = settings_f64(services, "update.interval").await?;

    let policy = services
        .scheduler
        .policy(enabled, &schedule_string, interval_secs, Vec::new());

    let checkpoint = runtime.checkpoint.load().await?;
    let last_run = checkpoint
        .last_run
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let decision = services.scheduler.decide(&policy, last_run);
    if !decision.run_now {
        return Ok(());
    }

    let now = runtime.now_rfc3339();
    match runtime.checkpoint.claim(generation, &now).await? {
        CheckpointClaim::Duplicate => {
            console_log!("auto-update generation {generation} already planned; skipping");
            Ok(())
        }
        CheckpointClaim::Claimed(mut checkpoint) => {
            plan_pages(runtime, &mut checkpoint).await?;
            // Release the generation regardless of how many pages completed:
            // already-enqueued jobs stay active in the ledger (dedupe), and
            // the next scheduled event re-scans and fills the gaps.
            runtime.checkpoint.save(&checkpoint.finish(&runtime.now_rfc3339())).await?;
            Ok(())
        }
    }
}

/// Scan bounded pages of novel ids (excluding frozen novels) and enqueue one
/// `Update` plan per novel.
async fn plan_pages(runtime: &WorkerRuntime, checkpoint: &mut narou_rs::application::SchedulerCheckpoint) -> Result<()> {
    let frozen_ids: HashSet<i64> = runtime.freeze.frozen_ids().await?;
    let mut after_id: Option<NovelId> = checkpoint.cursor.map(NovelId);

    loop {
        let ids = runtime
            .novels
            .scan_ids(&NovelFilter::default(), after_id, AUTO_UPDATE_PAGE_SIZE)
            .await?;
        if ids.is_empty() {
            break;
        }
        let page_full = ids.len() >= AUTO_UPDATE_PAGE_SIZE;
        after_id = ids.last().map(|id| NovelId(id.0));

        let mut enqueued_any = false;
        for id in ids {
            if frozen_ids.contains(&id.0) {
                continue;
            }
            let plan = JobPlan {
                kind: JobKind::Update,
                target: JobTarget::Id(id),
                options: Vec::new(),
            };
            runtime.ledger.enqueue(plan).await?;
            enqueued_any = true;
        }
        console_log!(
            "auto-update: planned page up to id {:?} (full: {page_full}, enqueued: {enqueued_any})",
            after_id
        );

        // Persist the cursor after every full page so a crash mid-run resumes
        // (or a fresh generation re-scans — dedupe keeps it duplicate-free).
        if let Some(cursor) = after_id {
            *checkpoint = checkpoint.advance(cursor.0);
            runtime.checkpoint.save(checkpoint).await?;
        }
        if !page_full {
            break;
        }
    }
    Ok(())
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
