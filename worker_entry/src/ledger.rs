//! D1-backed job ledger and scheduler checkpoint (Phase 8).
//!
//! Implements [`JobQueue`] over the `worker_jobs` table (migration 0005):
//! idempotent claims, durable terminal transitions, bounded retryable
//! attempts, and active dedupe (one active job per kind+target+effective
//! options). The scheduler checkpoint lives in `app_state`.
//!
//! All SQL is D1 prepared statements with the same conventions as
//! `d1_repository.rs` (RFC3339 nanosecond TEXT timestamps, bound parameters).

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use narou_rs::application::{
    CheckpointClaim, JobClaim, JobId, JobKind, JobLedgerStatus, JobPlan, JobQueue, JobTarget,
    QueuedJob, QueuedJobView, SchedulerCheckpoint,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{Clock, PlatformFuture};
use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, D1Result};

/// A running claim remains owned until this lease expires. The bounded
/// execution guard is shorter than the lease, so a live redelivery is never
/// allowed to execute concurrently with the original invocation.
const JOB_LEASE: chrono::TimeDelta = chrono::TimeDelta::minutes(15);

/// D1-backed [`JobQueue`] adapter.
pub struct D1JobLedger {
    db: Arc<D1Database>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for D1JobLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D1JobLedger").finish_non_exhaustive()
    }
}

impl Clone for D1JobLedger {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            clock: self.clock.clone(),
        }
    }
}

impl D1JobLedger {
    pub fn new(db: Arc<D1Database>, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    fn prepare(&self, sql: &str, values: Vec<BindValue>) -> Result<D1PreparedStatement> {
        bind_statement(self.db.prepare(sql), values)
    }

    fn now_rfc3339(&self) -> String {
        self.clock
            .now_utc()
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    fn lease_until_rfc3339(&self) -> String {
        (self.clock.now_utc() + JOB_LEASE)
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    fn new_execution_token() -> String {
        let now = js_sys::Date::now() as u64;
        let random = (js_sys::Math::random() * 18_446_744_073_709_551_616.0) as u64;
        format!("e{now:016x}{random:016x}")
    }

    /// Read one job row by id.
    async fn select_row(&self, job_id: &str) -> Result<Option<JobRow>> {
        let statement = self.prepare(
            "SELECT job_id, kind, target, options, status, attempts, last_error, created_at, updated_at,
                    lease_until, execution_token
             FROM worker_jobs WHERE job_id = ? LIMIT 1",
            vec![BindValue::Text(job_id.to_string())],
        )?;
        let row = statement.first::<JobRow>(None).await.map_err(worker_error)?;
        Ok(row)
    }

    /// Find an active row with the same dedupe key.
    async fn select_active_by_dedupe(&self, dedupe_key: &str) -> Result<Option<JobRow>> {
        let statement = self.prepare(
            "SELECT job_id, kind, target, options, status, attempts, last_error, created_at, updated_at,
                    lease_until, execution_token
             FROM worker_jobs WHERE dedupe_key = ? AND status IN ('pending', 'running', 'retryable') LIMIT 1",
            vec![BindValue::Text(dedupe_key.to_string())],
        )?;
        let row = statement.first::<JobRow>(None).await.map_err(worker_error)?;
        Ok(row)
    }

    /// Insert a fresh pending row; returns `false` on any uniqueness conflict
    /// (callers re-read the dedupe row and/or regenerate the id).
    async fn try_insert(&self, job_id: &JobId, job: &JobPlan, dedupe_key: &str) -> Result<bool> {
        let now = self.now_rfc3339();
        let options = serde_json::to_string(&job.options)
            .map_err(|error| NarouError::Platform(format!("job options serialization: {error}")))?;
        let statement = self.prepare(
            "INSERT INTO worker_jobs (job_id, kind, target, options, status, dedupe_key, attempts, last_error, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'pending', ?, 0, NULL, ?, ?)",
            vec![
                BindValue::Text(job_id.as_str().to_string()),
                BindValue::Text(job.kind.as_str().to_string()),
                BindValue::Text(job.target.as_str()),
                BindValue::Text(options),
                BindValue::Text(dedupe_key.to_string()),
                BindValue::Text(now.clone()),
                BindValue::Text(now),
            ],
        )?;
        let result = statement.run().await.map_err(worker_error)?;
        if result.success() {
            return Ok(true);
        }
        let error = result
            .error()
            .unwrap_or_else(|| "D1 insert failed".to_string());
        if error.contains("UNIQUE constraint failed") {
            Ok(false)
        } else {
            Err(NarouError::Platform(format!("D1 job insert failed: {error}")))
        }
    }

    /// A fresh, practically-unique job id (timestamp + random hex).
    fn new_job_id() -> String {
        let now = js_sys::Date::now() as u64;
        let random = (js_sys::Math::random() * 4_294_967_296.0) as u64;
        format!("j{now:016x}{random:08x}")
    }

    /// Durably record an envelope that could not be mapped to a job
    /// (malformed payload, unsupported version, unknown job id) as `blocked`.
    /// Idempotent per message id (`INSERT OR IGNORE`), so redeliveries of a
    /// poison payload settle on one row and are acked.
    pub async fn record_rejected(&self, job_id: &str, reason: &str) -> Result<()> {
        let now = self.now_rfc3339();
        let statement = self.prepare(
            "INSERT OR IGNORE INTO worker_jobs (job_id, kind, target, options, status, dedupe_key, attempts, last_error, created_at, updated_at)
             VALUES (?, 'unknown', '*', '[]', 'blocked', ?, 0, ?, ?, ?)",
            vec![
                BindValue::Text(job_id.to_string()),
                BindValue::Text(format!("rejected:{job_id}")),
                BindValue::Text(reason.to_string()),
                BindValue::Text(now.clone()),
                BindValue::Text(now),
            ],
        )?;
        let result = statement.run().await.map_err(worker_error)?;
        if !result.success() {
            return Err(NarouError::Platform(
                result
                    .error()
                    .unwrap_or_else(|| "D1 rejected-job recording failed".to_string()),
            ));
        }
        Ok(())
    }
}

impl JobQueue for D1JobLedger {
    fn enqueue<'a>(&'a self, job: JobPlan) -> PlatformFuture<'a, Result<QueuedJob>> {
        Box::pin(async move {
            let dedupe_key = job.dedupe_key();
            if let Some(existing) = self.select_active_by_dedupe(&dedupe_key).await? {
                return Ok(QueuedJob {
                    job_id: JobId(existing.job_id),
                    job,
                });
            }
            // Insert with a fresh id; a uniqueness conflict means either the
            // dedupe key raced in (re-read and return it) or the id collided
            // (regenerate). Bounded, never silent.
            for _ in 0..3 {
                let job_id = JobId(Self::new_job_id());
                match self.try_insert(&job_id, &job, &dedupe_key).await? {
                    true => return Ok(QueuedJob { job_id, job }),
                    false => {
                        if let Some(existing) = self.select_active_by_dedupe(&dedupe_key).await? {
                            return Ok(QueuedJob {
                                job_id: JobId(existing.job_id),
                                job,
                            });
                        }
                    }
                }
            }
            Err(NarouError::Platform(
                "D1 job enqueue failed after 3 attempts".to_string(),
            ))
        })
    }

    fn get<'a>(&'a self, job_id: &'a JobId) -> PlatformFuture<'a, Result<Option<QueuedJobView>>> {
        Box::pin(async move {
            let Some(row) = self.select_row(job_id.as_str()).await? else {
                return Ok(None);
            };
            row.into_view().map(Some)
        })
    }

    fn claim<'a>(&'a self, job_id: &'a JobId) -> PlatformFuture<'a, Result<JobClaim>> {
        Box::pin(async move {
            let now = self.now_rfc3339();
            let lease_until = self.lease_until_rfc3339();
            let execution_token = Self::new_execution_token();
            let fresh = self.prepare(
                "UPDATE worker_jobs
                 SET status = 'running', updated_at = ?, lease_until = ?, execution_token = ?
                 WHERE job_id = ? AND status IN ('pending', 'retryable')",
                vec![
                    BindValue::Text(now.clone()),
                    BindValue::Text(lease_until),
                    BindValue::Text(execution_token.clone()),
                    BindValue::Text(job_id.as_str().to_string()),
                ],
            )?;
            if changed_rows(&fresh.run().await.map_err(worker_error)?)? > 0 {
                return Ok(JobClaim::Claimed { execution_token });
            }

            let Some(row) = self.select_row(job_id.as_str()).await? else {
                self.record_rejected(job_id.as_str(), "job id not found in ledger")
                    .await?;
                return Ok(JobClaim::Unknown);
            };
            let status = JobLedgerStatus::parse(&row.status)?;
            match status {
                status if status.is_terminal() => Ok(JobClaim::AlreadyTerminal),
                JobLedgerStatus::Running => {
                    let reclaim = self.prepare(
                        "UPDATE worker_jobs
                         SET status = 'running', updated_at = ?, lease_until = ?, execution_token = ?
                         WHERE job_id = ? AND status = 'running'
                           AND (lease_until IS NULL OR lease_until <= ?)",
                        vec![
                            BindValue::Text(now),
                            BindValue::Text(self.lease_until_rfc3339()),
                            BindValue::Text(execution_token.clone()),
                            BindValue::Text(job_id.as_str().to_string()),
                            BindValue::Text(self.now_rfc3339()),
                        ],
                    )?;
                    if changed_rows(&reclaim.run().await.map_err(worker_error)?)? > 0 {
                        Ok(JobClaim::Claimed { execution_token })
                    } else {
                        Ok(JobClaim::Busy)
                    }
                }
                _ => Ok(JobClaim::Busy),
            }
        })
    }

    fn save_checkpoint<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        checkpoint: &'a narou_rs::application::WorkerExecutionCheckpoint,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let value = serde_json::to_string(checkpoint).map_err(|error| {
                NarouError::Platform(format!("execution checkpoint serialization: {error}"))
            })?;
            let statement = self.prepare(
                "UPDATE worker_jobs SET checkpoint_json = ?, updated_at = ?
                 WHERE job_id = ? AND status = 'running' AND execution_token = ?",
                vec![
                    BindValue::Text(value),
                    BindValue::Text(self.now_rfc3339()),
                    BindValue::Text(job_id.as_str().to_string()),
                    BindValue::Text(execution_token.to_string()),
                ],
            )?;
            if changed_rows(&statement.run().await.map_err(worker_error)?)? == 0 {
                return Err(NarouError::Platform(format!(
                    "execution claim for job {job_id} is no longer valid"
                )));
            }
            Ok(())
        })
    }

    fn record_attempt<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        error: &'a str,
    ) -> PlatformFuture<'a, Result<u32>> {
        Box::pin(async move {
            let update = self.prepare(
                "UPDATE worker_jobs
                 SET attempts = attempts + 1, status = 'retryable', last_error = ?,
                     updated_at = ?, lease_until = NULL
                 WHERE job_id = ? AND status = 'running' AND execution_token = ?",
                vec![
                    BindValue::Text(error.to_string()),
                    BindValue::Text(self.now_rfc3339()),
                    BindValue::Text(job_id.as_str().to_string()),
                    BindValue::Text(execution_token.to_string()),
                ],
            )?;
            let select = self.prepare(
                "SELECT attempts FROM worker_jobs WHERE job_id = ?",
                vec![BindValue::Text(job_id.as_str().to_string())],
            )?;
            let results = self.db.batch(vec![update, select]).await.map_err(worker_error)?;
            ensure_batch_success(&results)?;
            if changed_rows(results.first().ok_or_else(|| {
                NarouError::Platform("D1 attempt update returned no result".to_string())
            })?)? == 0 {
                return Err(NarouError::Platform(format!(
                    "execution claim for job {job_id} is no longer valid"
                )));
            }
            let row: AttemptRow = results
                .get(1)
                .ok_or_else(|| NarouError::Platform("D1 attempt read returned no row".to_string()))?
                .results()
                .map_err(worker_error)?
                .into_iter()
                .next()
                .ok_or_else(|| NarouError::Platform("D1 attempt row missing".to_string()))?;
            u32::try_from(row.attempts).map_err(|_| {
                NarouError::Platform("D1 attempt count overflow".to_string())
            })
        })
    }

    fn mark_terminal<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        status: JobLedgerStatus,
        error: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if !status.is_terminal() {
                return Err(NarouError::Platform(format!(
                    "cannot mark job {job_id} terminal with non-terminal status {status:?}"
                )));
            }
            let (sql, values) = match error {
                Some(error) => (
                    "UPDATE worker_jobs
                     SET status = ?, last_error = ?, updated_at = ?,
                         lease_until = NULL, execution_token = NULL, checkpoint_json = NULL
                     WHERE job_id = ? AND status IN ('running', 'retryable') AND execution_token = ?",
                    vec![
                        BindValue::Text(status_str(status).to_string()),
                        BindValue::Text(error.to_string()),
                        BindValue::Text(self.now_rfc3339()),
                        BindValue::Text(job_id.as_str().to_string()),
                        BindValue::Text(execution_token.to_string()),
                    ],
                ),
                None => (
                    "UPDATE worker_jobs
                     SET status = ?, last_error = NULL, updated_at = ?,
                         lease_until = NULL, execution_token = NULL, checkpoint_json = NULL
                     WHERE job_id = ? AND status IN ('running', 'retryable') AND execution_token = ?",
                    vec![
                        BindValue::Text(status_str(status).to_string()),
                        BindValue::Text(self.now_rfc3339()),
                        BindValue::Text(job_id.as_str().to_string()),
                        BindValue::Text(execution_token.to_string()),
                    ],
                ),
            };
            let statement = self.prepare(sql, values)?;
            let result = statement.run().await.map_err(worker_error)?;
            if changed_rows(&result)? == 0 {
                return Err(NarouError::Platform(format!(
                    "execution claim for job {job_id} is no longer valid"
                )));
            }
            Ok(())
        })
    }
}

/// D1-backed auto-update planner checkpoint (`app_state` row).
#[derive(Debug, Clone)]
pub struct D1SchedulerCheckpoint {
    db: Arc<D1Database>,
}

impl D1SchedulerCheckpoint {
    const SCOPE: &'static str = "scheduler";
    const KEY: &'static str = "auto_update";

    pub fn new(db: Arc<D1Database>) -> Self {
        Self { db }
    }

    fn prepare(&self, sql: &str, values: Vec<BindValue>) -> Result<D1PreparedStatement> {
        bind_statement(self.db.prepare(sql), values)
    }

    /// Load the stored checkpoint, or the default when no run ever happened.
    pub async fn load(&self) -> Result<SchedulerCheckpoint> {
        let statement = self.prepare(
            "SELECT value_yaml, value_json FROM app_state WHERE scope = ? AND key = ?",
            vec![
                BindValue::Text(Self::SCOPE.to_string()),
                BindValue::Text(Self::KEY.to_string()),
            ],
        )?;
        let row = statement.first::<CheckpointRow>(None).await.map_err(worker_error)?;
        let Some(row) = row else {
            return Ok(SchedulerCheckpoint::default());
        };
        if let Some(value) = row.value_yaml.as_deref() {
            if let Ok(checkpoint) = serde_yaml::from_str(value) {
                return Ok(checkpoint);
            }
        }
        if let Some(value) = row.value_json.as_deref() {
            return serde_json::from_str(value).map_err(|error| {
                NarouError::Platform(format!("scheduler checkpoint is corrupt: {error}"))
            });
        }
        Err(NarouError::Platform(
            "scheduler checkpoint has no YAML or JSON value".to_string(),
        ))
    }

    /// Persist the checkpoint (upsert on the `(scope, key)` primary key).
    pub async fn save(&self, checkpoint: &SchedulerCheckpoint) -> Result<()> {
        let value_json = serde_json::to_string(checkpoint).map_err(|error| {
            NarouError::Platform(format!("scheduler checkpoint JSON serialization: {error}"))
        })?;
        let value_yaml = serde_yaml::to_string(checkpoint).map_err(|error| {
            NarouError::Platform(format!("scheduler checkpoint YAML serialization: {error}"))
        })?;
        let statement = self.prepare(
            "INSERT INTO app_state (scope, key, value_yaml, value_json) VALUES (?, ?, ?, ?)
             ON CONFLICT (scope, key) DO UPDATE SET
               value_yaml = excluded.value_yaml, value_json = excluded.value_json",
            vec![
                BindValue::Text(Self::SCOPE.to_string()),
                BindValue::Text(Self::KEY.to_string()),
                BindValue::Text(value_yaml),
                BindValue::Text(value_json),
            ],
        )?;
        let result = statement.run().await.map_err(worker_error)?;
        if !result.success() {
            return Err(NarouError::Platform(
                result
                    .error()
                    .unwrap_or_else(|| "D1 checkpoint save failed".to_string()),
            ));
        }
        Ok(())
    }


    /// Load-checkpoint and claim a generation atomically against the
    /// currently persisted generation. Concurrent cron invocations race on
    /// one conditional D1 update; only one observes a changed row.
    pub async fn claim(
        &self,
        generation: u64,
        started_at: &str,
    ) -> Result<CheckpointClaim> {
        let current = self.load().await?;
        match current.begin(generation, started_at) {
            CheckpointClaim::Duplicate => Ok(CheckpointClaim::Duplicate),
            CheckpointClaim::Claimed(next) => {
                let expected = current.generation.to_string();
                let value_json = serde_json::to_string(&next).map_err(|error| {
                    NarouError::Platform(format!("scheduler checkpoint serialization: {error}"))
                })?;
                let value_yaml = serde_yaml::to_string(&next).map_err(|error| {
                    NarouError::Platform(format!("scheduler checkpoint YAML serialization: {error}"))
                })?;
                let statement = self.prepare(
                    "INSERT INTO app_state (scope, key, value_yaml, value_json)
                     VALUES (?, ?, ?, ?)
                     ON CONFLICT (scope, key) DO UPDATE SET
                       value_yaml = excluded.value_yaml,
                       value_json = excluded.value_json
                     WHERE json_extract(app_state.value_json, '$.generation') = ?",
                    vec![
                        BindValue::Text(Self::SCOPE.to_string()),
                        BindValue::Text(Self::KEY.to_string()),
                        BindValue::Text(value_yaml),
                        BindValue::Text(value_json),
                        BindValue::Text(expected),
                    ],
                )?;
                if changed_rows(&statement.run().await.map_err(worker_error)?)? == 0 {
                    let latest = self.load().await?;
                    return Ok(match latest.begin(generation, started_at) {
                        CheckpointClaim::Duplicate => CheckpointClaim::Duplicate,
                        CheckpointClaim::Claimed(_) => CheckpointClaim::Duplicate,
                    });
                }
                Ok(CheckpointClaim::Claimed(next))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Row types and helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct JobRow {
    job_id: String,
    kind: String,
    target: String,
    options: String,
    status: String,
    attempts: i64,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
}

impl JobRow {
    fn into_view(self) -> Result<QueuedJobView> {
        let kind = JobKind::parse(&self.kind).ok_or_else(|| {
            NarouError::Platform(format!("unknown job kind in ledger: {:?}", self.kind))
        })?;
        let target = JobTarget::parse(&self.target).ok_or_else(|| {
            NarouError::Platform(format!("unknown job target in ledger: {:?}", self.target))
        })?;
        let options: Vec<String> = serde_json::from_str(&self.options).map_err(|error| {
            NarouError::Platform(format!("job options in ledger are corrupt: {error}"))
        })?;
        let status = JobLedgerStatus::parse(&self.status)?;
        let attempts = u32::try_from(self.attempts).map_err(|_| {
            NarouError::Platform("job attempt count overflow".to_string())
        })?;
        let created_at = parse_rfc3339(&self.created_at)?;
        let updated_at = parse_rfc3339(&self.updated_at)?;
        Ok(QueuedJobView {
            job_id: JobId(self.job_id),
            job: JobPlan {
                kind,
                target,
                options,
            },
            status,
            attempts,
            last_error: self.last_error,
            created_at,
            updated_at,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AttemptRow {
    attempts: i64,
}

#[derive(Debug, Deserialize)]
struct CheckpointRow {
    value_yaml: Option<String>,
    value_json: Option<String>,
}

fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| NarouError::Platform(format!("invalid timestamp in ledger: {error}")))
}

fn status_str(status: JobLedgerStatus) -> &'static str {
    match status {
        JobLedgerStatus::Pending => "pending",
        JobLedgerStatus::Running => "running",
        JobLedgerStatus::Succeeded => "succeeded",
        JobLedgerStatus::Partial => "partial",
        JobLedgerStatus::Retryable => "retryable",
        JobLedgerStatus::Blocked => "blocked",
        JobLedgerStatus::Permanent => "permanent",
    }
}

#[derive(Debug, Clone)]
enum BindValue {
    Text(String),
}

fn bind_statement(
    statement: D1PreparedStatement,
    values: Vec<BindValue>,
) -> Result<D1PreparedStatement> {
    let values: Vec<JsValue> = values
        .into_iter()
        .map(|value| match value {
            BindValue::Text(value) => JsValue::from_str(&value),
        })
        .collect();
    statement.bind(&values).map_err(worker_error)
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker storage error: {error}"))
}

/// Number of rows changed by a `run()`; `None` means "no meta" (treat as 0).
fn changed_rows(result: &D1Result) -> Result<usize> {
    if !result.success() {
        return Err(NarouError::Platform(
            result
                .error()
                .unwrap_or_else(|| "D1 statement failed".to_string()),
        ));
    }
    Ok(result
        .meta()
        .map_err(worker_error)?
        .and_then(|meta| meta.changes)
        .unwrap_or(0))
}

fn ensure_batch_success(results: &[D1Result]) -> Result<()> {
    for result in results {
        if !result.success() {
            return Err(NarouError::Platform(
                result.error().unwrap_or_else(|| "D1 batch statement failed".to_string()),
            ));
        }
    }
    Ok(())
}
