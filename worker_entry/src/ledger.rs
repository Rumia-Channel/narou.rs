//! D1-backed job ledger and scheduler checkpoint (Phase 8).
//!
//! Implements [`JobQueue`] over the `worker_jobs` table (migration 0005):
//! idempotent claims, durable terminal transitions, bounded retryable
//! attempts with a durable backoff hold (`lease_until` on `retryable`
//! rows), active dedupe (one active job per kind+target+effective
//! options), and per-novel exclusion — a claim can never flip to `running`
//! while another row covering the same novel id is running (native
//! `job_conflicts_with_running` parity; the claim SQL lives in
//! `narou_rs::application::worker_ledger` so host tests pin it against
//! SQLite). The scheduler checkpoint lives in `app_state`.
//!
//! All SQL is D1 prepared statements with the same conventions as
//! `d1_repository.rs` (RFC3339 nanosecond TEXT timestamps, bound parameters).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use narou_rs::application::{
    CheckpointClaim, JobClaim, JobId, JobKind, JobLedgerStatus, JobPlan, JobQueue, JobTarget,
    QueuedJob, QueuedJobView, SchedulerCheckpoint, WorkerExecutionCheckpoint,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{Clock, PlatformFuture};
use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1PreparedStatement, D1Result};

use crate::db_handle::DbHandle;

/// A running claim remains owned until this lease expires. The bounded
/// execution guard is shorter than the lease, so a live redelivery is never
/// allowed to execute concurrently with the original invocation.
const JOB_LEASE: chrono::TimeDelta = chrono::TimeDelta::minutes(15);

/// Plans per ledger batch round trip. Bounded so one invocation never builds
/// an unbounded statement vector; the D1 batch and the 100-message Queue
/// batch both stay at or below this page size.
const ENQUEUE_CHUNK: usize = 100;

/// Fresh-id retries per chunk before an enqueue is reported as failed. The
/// only expected retry is an improbable job-id collision; a dedupe race is
/// resolved by re-reading the key instead of a retry.
const ENQUEUE_MAX_ATTEMPTS: usize = 3;

/// Active rows for a batch of dedupe keys in one request. The partial unique
/// index (`worker_jobs_active_dedupe`) keeps at most one active row per key;
/// should legacy rows violate that, the oldest `(created_at, job_id)` row
/// wins so repeated calls agree on the same job identity. `json_each` keeps
/// the key list to a single bind (same pattern as `d1_repository`).
const ACTIVE_BY_DEDUPE_KEYS_SQL: &str = r#"
SELECT job_id, kind, target, options, status, attempts, last_error, created_at, updated_at,
       checkpoint_json, dedupe_key
FROM worker_jobs
WHERE status IN ('pending', 'running', 'retryable')
  AND dedupe_key IN (SELECT value FROM json_each(?))
ORDER BY created_at ASC, job_id ASC
"#;

/// Fresh pending rows. `INSERT OR IGNORE` maps a dedupe race or an id
/// collision to a zero-change statement instead of aborting the whole batch;
/// the caller re-reads the dedupe key and rebuilds only the ignored rows.
const INSERT_OR_IGNORE_SQL: &str = r#"
INSERT OR IGNORE INTO worker_jobs
    (job_id, kind, target, options, status, dedupe_key, attempts, last_error, created_at, updated_at)
VALUES (?, ?, ?, ?, 'pending', ?, 0, NULL, ?, ?)
"#;

/// Fold one frozen-skipped job into the terminal `partial` state. The status
/// guard is what keeps a batch's frozen snapshot from touching a job that is
/// already executing: a dedupe hit on a running row keeps its status, lease,
/// token and checkpoint, so the live execution still owns the row (only the
/// executor's own recheck can settle it).
const SETTLE_SKIPPED_SQL: &str = r#"
UPDATE worker_jobs
SET status = ?, last_error = ?, updated_at = ?,
    lease_until = NULL, execution_token = NULL, checkpoint_json = NULL
WHERE job_id = ? AND status IN ('pending', 'retryable')
"#;

/// One plan's outcome from [`D1JobLedger::enqueue_batch`]: the active row that
/// now owns the plan's dedupe key (freshly inserted or pre-existing) and that
/// row's status. The payload is the requested plan — the dedupe key fixes
/// kind, target and effective options, so whoever created the row stored the
/// same job.
#[derive(Debug, Clone)]
pub struct EnqueuedJob {
    pub queued: QueuedJob,
    pub status: JobLedgerStatus,
}

/// D1-backed [`JobQueue`] adapter.
pub struct D1JobLedger {
    db: DbHandle,
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
    pub fn new(db: DbHandle, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    fn prepare(&self, sql: &str, values: Vec<BindValue>) -> Result<D1PreparedStatement> {
        bind_statement(self.db.prepare(sql), values)
    }

    fn prepare_core(&self, statement: narou_rs::application::worker_ledger::Statement) -> Result<D1PreparedStatement> {
        self.prepare(&statement.sql, statement.binds.into_iter().map(BindValue::Text).collect())
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
                    checkpoint_json
             FROM worker_jobs WHERE job_id = ? LIMIT 1",
            vec![BindValue::Text(job_id.to_string())],
        )?;
        let row = statement.first::<JobRow>(None).await.map_err(worker_error)?;
        Ok(row)
    }

    /// Active rows for a chunk of dedupe keys, in one request.
    async fn select_active_by_dedupe_keys(
        &self,
        dedupe_keys: &[String],
    ) -> Result<HashMap<String, JobRow>> {
        if dedupe_keys.is_empty() {
            return Ok(HashMap::new());
        }
        let keys = serde_json::to_string(dedupe_keys).map_err(|error| {
            NarouError::Platform(format!("job dedupe key serialization: {error}"))
        })?;
        let statement = self.prepare(ACTIVE_BY_DEDUPE_KEYS_SQL, vec![BindValue::Text(keys)])?;
        let rows: Vec<JobRow> = statement
            .all()
            .await
            .map_err(worker_error)?
            .results()
            .map_err(worker_error)?;
        let mut active = HashMap::with_capacity(rows.len());
        for row in rows {
            // Oldest row wins for a legacy duplicate; the partial unique
            // index makes this unreachable for rows this ledger wrote.
            if let Some(key) = row.dedupe_key.clone() {
                active.entry(key).or_insert(row);
            }
        }
        Ok(active)
    }

    /// One fresh pending insert statement (id, kind, target, options, dedupe
    /// key, timestamps). `INSERT OR IGNORE` keeps an id collision or a dedupe
    /// race from failing the whole batch.
    fn pending_insert_statement(
        &self,
        job_id: &JobId,
        job: &JobPlan,
        options: &str,
        dedupe_key: &str,
        now: &str,
    ) -> Result<D1PreparedStatement> {
        self.prepare(
            INSERT_OR_IGNORE_SQL,
            vec![
                BindValue::Text(job_id.as_str().to_string()),
                BindValue::Text(job.kind.as_str().to_string()),
                BindValue::Text(job.target.as_str()),
                BindValue::Text(options.to_string()),
                BindValue::Text(dedupe_key.to_string()),
                BindValue::Text(now.to_string()),
                BindValue::Text(now.to_string()),
            ],
        )
    }

    /// Enqueue several plans with the same dedupe/insert semantics as the
    /// single-plan path, but with bounded batch round trips: one active-key
    /// SELECT per chunk, one INSERT OR IGNORE batch per chunk, and a dedupe
    /// re-read only for statements that were ignored. Outcomes stay in input
    /// order, and two plans that share a dedupe key (duplicate targets in one
    /// request) get the same job id — no double insertion.
    pub async fn enqueue_batch(&self, jobs: &[JobPlan]) -> Result<Vec<EnqueuedJob>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let mut options = Vec::with_capacity(jobs.len());
        for job in jobs {
            options.push(serde_json::to_string(&job.options).map_err(|error| {
                NarouError::Platform(format!("job options serialization: {error}"))
            })?);
        }
        let dedupe_keys: Vec<String> = jobs.iter().map(JobPlan::dedupe_key).collect();
        let mut outcomes: Vec<Option<EnqueuedJob>> =
            std::iter::repeat_with(|| None).take(jobs.len()).collect();
        for start in (0..jobs.len()).step_by(ENQUEUE_CHUNK) {
            let end = (start + ENQUEUE_CHUNK).min(jobs.len());
            let existing = self
                .select_active_by_dedupe_keys(&dedupe_keys[start..end])
                .await?;
            let mut pending: Vec<usize> = Vec::new();
            for index in start..end {
                match existing.get(&dedupe_keys[index]) {
                    Some(row) => {
                        outcomes[index] = Some(EnqueuedJob {
                            queued: QueuedJob {
                                job_id: JobId(row.job_id.clone()),
                                job: jobs[index].clone(),
                            },
                            status: JobLedgerStatus::parse(&row.status)?,
                        });
                    }
                    None => pending.push(index),
                }
            }
            for _ in 0..ENQUEUE_MAX_ATTEMPTS {
                if pending.is_empty() {
                    break;
                }
                let now = self.now_rfc3339();
                let mut statements = Vec::with_capacity(pending.len());
                let mut planned: Vec<(usize, JobId)> = Vec::with_capacity(pending.len());
                for &index in &pending {
                    let job_id = JobId(Self::new_job_id());
                    statements.push(self.pending_insert_statement(
                        &job_id,
                        &jobs[index],
                        &options[index],
                        &dedupe_keys[index],
                        &now,
                    )?);
                    planned.push((index, job_id));
                }
                let results = self.db.batch(statements).await.map_err(worker_error)?;
                ensure_batch_success(&results)?;
                let mut ignored: Vec<usize> = Vec::new();
                for (position, (index, job_id)) in planned.iter().enumerate() {
                    if changed_rows(&results[position])? > 0 {
                        outcomes[*index] = Some(EnqueuedJob {
                            queued: QueuedJob {
                                job_id: job_id.clone(),
                                job: jobs[*index].clone(),
                            },
                            status: JobLedgerStatus::Pending,
                        });
                    } else {
                        ignored.push(*index);
                    }
                }
                if ignored.is_empty() {
                    pending.clear();
                    break;
                }
                // A zero-change INSERT is either a dedupe race (another
                // writer won the same key) or an id collision. One re-read
                // resolves the races; the rest regenerate ids.
                let retry_keys: Vec<String> =
                    ignored.iter().map(|index| dedupe_keys[*index].clone()).collect();
                let winners = self.select_active_by_dedupe_keys(&retry_keys).await?;
                let mut retry = Vec::new();
                for index in ignored {
                    match winners.get(&dedupe_keys[index]) {
                        Some(row) => {
                            outcomes[index] = Some(EnqueuedJob {
                                queued: QueuedJob {
                                    job_id: JobId(row.job_id.clone()),
                                    job: jobs[index].clone(),
                                },
                                status: JobLedgerStatus::parse(&row.status)?,
                            });
                        }
                        None => retry.push(index),
                    }
                }
                pending = retry;
            }
            if let Some(index) = pending.first() {
                return Err(NarouError::Platform(format!(
                    "D1 job enqueue failed after {ENQUEUE_MAX_ATTEMPTS} attempts for {} plan(s) \
                     (first target {:?})",
                    pending.len(),
                    jobs[*index].target.as_str(),
                )));
            }
        }
        outcomes
            .into_iter()
            .map(|outcome| {
                outcome.ok_or_else(|| {
                    NarouError::Platform("job enqueue left a plan unresolved".to_string())
                })
            })
            .collect()
    }

    /// A fresh, practically-unique job id (timestamp + random hex).
    fn new_job_id() -> String {
        let now = js_sys::Date::now() as u64;
        let random = (js_sys::Math::random() * 4_294_967_296.0) as u64;
        format!("j{now:016x}{random:08x}")
    }
    fn checkpoint_from_row(row: &JobRow) -> Result<Option<WorkerExecutionCheckpoint>> {
        row.checkpoint_json
            .as_deref()
            .map(|value| {
                serde_json::from_str(value).map_err(|error| {
                    NarouError::Platform(format!(
                        "execution checkpoint for job {} is corrupt: {error}",
                        row.job_id
                    ))
                })
            })
            .transpose()
    }

    fn claimed_from_row(row: &JobRow, execution_token: String) -> Result<JobClaim> {
        Ok(JobClaim::Claimed {
            execution_token,
            checkpoint: Self::checkpoint_from_row(row)?,
        })
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

    /// `GET /api/jobs/:id` 用の読み取り (`lib.rs::api_job`)。
    ///
    /// [`JobQueue::get`] (実行経路) は計画が必須なので未知の `kind` を拒否
    /// するが、HTTP の閲覧では `record_rejected` が書く `kind='unknown'` の
    /// poison 行も閲覧できなければならない — 行は存在するので 404 も 500 も
    /// なく、生の kind 文字列のまま [`JobView`] で返す。
    pub async fn get_view(&self, job_id: &JobId) -> Result<Option<JobView>> {
        let Some(row) = self.select_row(job_id.as_str()).await? else {
            return Ok(None);
        };
        row.into_view().map(Some)
    }

    // ------------------------------------------------------------------
    // Web UI queue mutations (token-blind operator actions)
    // ------------------------------------------------------------------
    //
    // These back `webui/queue_actions.rs`. The worker has no `cancelled`
    // ledger status and the Cloudflare Queue keeps already-sent envelopes,
    // so removing a job cannot be a row deletion: a delivered envelope for
    // a deleted row is durably re-ledgered as a phantom `blocked` row by
    // `record_rejected`, and `JobLedgerStatus::parse` rejects any
    // non-canonical status (a custom tombstone would retry-storm the
    // consumer). The only consistent "this job never runs" state is a
    // terminal row, written unconditionally — these are operator actions,
    // not executor outcomes. Pending work loses its token; cancelled running
    // work retains a draining lease until its invocation stops. Every later
    // delivery sees `AlreadyTerminal` and acks. `queue/status` counts it under `failed`
    // (the only terminal bucket the Web UI has for non-success rows) —
    // documented in `webui/queue_actions.rs`.

    /// Mark every pending / retryable row terminal (`permanent` + `reason`).
    /// Returns the number of tombstoned rows. Running rows are untouched —
    /// native `clear_non_running` parity.
    pub async fn cancel_all_pending(&self, reason: &str) -> Result<usize> {
        let statement = self.prepare(
            "UPDATE worker_jobs
             SET status = ?, last_error = ?, updated_at = ?,
                 lease_until = NULL, execution_token = NULL, checkpoint_json = NULL, admission_waiting = 0
             WHERE status IN ('pending', 'retryable')",
            vec![
                BindValue::Text(status_str(JobLedgerStatus::Permanent).to_string()),
                BindValue::Text(reason.to_string()),
                BindValue::Text(self.now_rfc3339()),
            ],
        )?;
        changed_rows(&statement.run().await.map_err(worker_error)?)
    }

    /// Tombstone one pending / retryable row; `false` when no such row
    /// exists (native `remove_pending` parity: running, terminal, and
    /// unknown ids are not removable).
    pub async fn cancel_pending_job(&self, job_id: &str, reason: &str) -> Result<bool> {
        let statement = self.prepare(
            "UPDATE worker_jobs
             SET status = ?, last_error = ?, updated_at = ?,
                 lease_until = NULL, execution_token = NULL, checkpoint_json = NULL, admission_waiting = 0
             WHERE job_id = ? AND status IN ('pending', 'retryable')",
            vec![
                BindValue::Text(status_str(JobLedgerStatus::Permanent).to_string()),
                BindValue::Text(reason.to_string()),
                BindValue::Text(self.now_rfc3339()),
                BindValue::Text(job_id.to_string()),
            ],
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    /// Settle a just-enqueued plan as skipped (`partial` + reason) when the
    /// planning boundary decides the job must not run (frozen target). Same
    /// token-blind tombstone shape as [`cancel_pending_job`] — the row drops
    /// out of the active set, any later delivery claims it `AlreadyTerminal`
    /// and acks, and `partial` is the same terminal bucket the native web
    /// worker reports for a single-target frozen skip (mistook=1 → partial).
    /// `false` when the row is not pending/retryable (a dedupe hit on a
    /// running job leaves it running).
    pub async fn settle_skipped(&self, job_id: &JobId, reason: &str) -> Result<bool> {
        let statement = self.prepare(
            SETTLE_SKIPPED_SQL,
            vec![
                BindValue::Text(status_str(JobLedgerStatus::Partial).to_string()),
                BindValue::Text(reason.to_string()),
                BindValue::Text(self.now_rfc3339()),
                BindValue::Text(job_id.as_str().to_string()),
            ],
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    /// Cancel logically without releasing physical execution ownership. Wasm
    /// keeps running until its next outcome boundary; successors must wait.
    pub async fn cancel_running_job(&self, job_id: &str, reason: &str) -> Result<bool> {
        let statement = self.prepare(
            "UPDATE worker_jobs
             SET status = ?, last_error = ?, updated_at = ?,
                 checkpoint_json = NULL
             WHERE job_id = ? AND status = 'running'",
            vec![
                BindValue::Text(status_str(JobLedgerStatus::Permanent).to_string()),
                BindValue::Text(reason.to_string()),
                BindValue::Text(self.now_rfc3339()),
                BindValue::Text(job_id.to_string()),
            ],
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    pub async fn finish_cancelled(&self, job_id: &JobId, execution_token: &str) -> Result<bool> {
        let statement = self.prepare(
            "UPDATE worker_jobs SET lease_until = NULL, execution_token = NULL
             WHERE job_id = ? AND status = 'permanent' AND execution_token = ?",
            vec![BindValue::Text(job_id.as_str().to_string()), BindValue::Text(execution_token.to_string())],
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    /// All currently-running job ids (any lease state), FIFO order.
    pub async fn running_job_ids(&self) -> Result<Vec<String>> {
        let statement = self.db.prepare(
            "SELECT job_id FROM worker_jobs
             WHERE status = 'running'
             ORDER BY created_at ASC, job_id ASC",
        );
        let rows = statement
            .all()
            .await
            .map_err(worker_error)?
            .results::<JobIdRow>()
            .map_err(worker_error)?;
        Ok(rows.into_iter().map(|row| row.job_id).collect())
    }

    /// Number of active rows (`pending` / `retryable` / `running`) in one
    /// snapshot query. The `webui.table.reload-timing = queue` gate treats
    /// 0 as "queue drained" — native `queue.active_pending_count() == 0 &&
    /// running_jobs.is_empty()` over the same active set. A `retryable`
    /// row whose backoff hold elapsed still counts: it is still queued.
    pub async fn active_job_count(&self) -> Result<usize> {
        let statement = self.db.prepare(
            "SELECT COUNT(*) AS n FROM worker_jobs
             WHERE status IN ('pending', 'retryable', 'running')",
        );
        let count = statement
            .first::<i64>(Some("n"))
            .await
            .map_err(worker_error)?;
        Ok(usize::try_from(count.unwrap_or(0)).unwrap_or(0))
    }

    /// Expired executions eligible for manual recovery, with their queue lane.
    pub async fn stale_running_jobs(&self) -> Result<Vec<QueuedJobRoute>> {
        let statement = self.prepare(
            "SELECT job_id, kind FROM worker_jobs
             WHERE status = 'running' AND (lease_until IS NULL OR lease_until <= ?)
             ORDER BY created_at ASC, job_id ASC",
            vec![BindValue::Text(self.now_rfc3339())],
        )?;
        let rows = statement
            .all()
            .await
            .map_err(worker_error)?
            .results::<QueuedJobRoute>()
            .map_err(worker_error)?;
        Ok(rows)
    }

    pub async fn park_for_admission(&self, job_id: &JobId, normalized_target: &str, site_key: Option<&str>) -> Result<()> {
        let statement = self.prepare_core(narou_rs::application::worker_ledger::park_admission(
            &self.now_rfc3339(), job_id.as_str(), normalized_target, site_key,
        ))?;
        statement.run().await.map_err(worker_error)?;
        Ok(())
    }

    pub async fn waiting_jobs(&self) -> Result<Vec<QueuedJobRoute>> {
        let statement = self.prepare_core(narou_rs::application::worker_ledger::select_waiting(
            &self.now_rfc3339(), 8,
        ))?;
        statement.all().await.map_err(worker_error)?.results().map_err(worker_error)
    }

    pub async fn mark_waiting_dispatched(&self, job_id: &str) -> Result<bool> {
        let statement = self.prepare_core(narou_rs::application::worker_ledger::mark_waiting_dispatched(
            &self.now_rfc3339(), job_id,
        ))?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    /// Active rows whose scheduled progress point passed `stuck_before`
    /// (canonical RFC3339). See
    /// [`narou_rs::application::worker_ledger::select_stuck`]; `limit` caps
    /// how many rows one call returns (one cron page).
    pub async fn stuck_jobs(&self, stuck_before: &str, limit: usize) -> Result<Vec<StuckJob>> {
        let statement = narou_rs::application::worker_ledger::select_stuck(stuck_before, limit);
        let statement = self.prepare(
            &statement.sql,
            statement.binds.into_iter().map(BindValue::Text).collect(),
        )?;
        let rows = statement
            .all()
            .await
            .map_err(worker_error)?
            .results::<StuckJob>()
            .map_err(worker_error)?;
        Ok(rows)
    }

    /// Throttle-mark one stuck row before re-sending its envelope
    /// (`worker_ledger::mark_reenqueue`). Returns `true` when the row was
    /// still active and still stuck — only then may the caller send a fresh
    /// envelope. `false` means a live delivery or a concurrent reaper tick
    /// already moved the row, and no duplicate must be produced.
    pub async fn touch_for_reenqueue(&self, job_id: &str, stuck_before: &str) -> Result<bool> {
        let statement = narou_rs::application::worker_ledger::mark_reenqueue(
            &self.now_rfc3339(),
            job_id,
            stuck_before,
        );
        let statement = self.prepare(
            &statement.sql,
            statement.binds.into_iter().map(BindValue::Text).collect(),
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    /// Durably close one stuck `retryable` row whose retry budget is spent
    /// (`worker_ledger::finalize_stuck`). Returns `true` when the row was
    /// tombstoned; `false` when it moved on (a live delivery claimed it or
    /// it already settled).
    pub async fn finalize_stuck(
        &self,
        job_id: &str,
        stuck_before: &str,
        min_attempts: u32,
        reason: &str,
    ) -> Result<bool> {
        let statement = narou_rs::application::worker_ledger::finalize_stuck(
            reason,
            &self.now_rfc3339(),
            job_id,
            stuck_before,
            min_attempts,
        );
        let statement = self.prepare(
            &statement.sql,
            statement.binds.into_iter().map(BindValue::Text).collect(),
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }
}

impl JobQueue for D1JobLedger {
    fn enqueue<'a>(&'a self, job: JobPlan) -> PlatformFuture<'a, Result<QueuedJob>> {
        Box::pin(async move {
            // Shared with the batch path: dedupe lookup + bounded insert
            // retries. A single action never needs more than one outcome.
            self.enqueue_batch(std::slice::from_ref(&job))
                .await
                .map(|mut jobs| jobs.remove(0).queued)
        })
    }

    fn get<'a>(&'a self, job_id: &'a JobId) -> PlatformFuture<'a, Result<Option<QueuedJobView>>> {
        Box::pin(async move {
            let Some(row) = self.select_row(job_id.as_str()).await? else {
                return Ok(None);
            };
            // 実行経路は計画が必須: 未知の kind は従来どおりエラー
            // (閲覧専用の生 kind 表現は `get_view` を参照)。
            row.into_view()?.try_into_typed().map(Some)
        })
    }

    fn claim<'a>(&'a self, job_id: &'a JobId, normalized_target: &'a str, site_key: Option<&'a str>) -> PlatformFuture<'a, Result<JobClaim>> {
        Box::pin(async move {
            // Preserve the stored target for display, dedupe and checkpoint
            // identity. Only admission uses the YAML-resolved target.
            let Some(row) = self.select_row(job_id.as_str()).await? else {
                self.record_rejected(job_id.as_str(), "job id not found in ledger")
                    .await?;
                return Ok(JobClaim::Unknown);
            };
            let status = JobLedgerStatus::parse(&row.status)?;
            if status.is_terminal() {
                return Ok(JobClaim::AlreadyTerminal);
            }

            let now = self.now_rfc3339();
            let execution_token = Self::new_execution_token();

            // Fresh claim: pending, or retryable whose backoff hold elapsed.
            // The same statement carries the per-novel exclusion, so two
            // envelopes targeting the same novel can never both flip to
            // `running` (native `job_conflicts_with_running` parity — here
            // lane-blind because every consumer runs jobs concurrently).
            if status != JobLedgerStatus::Running {
                let statement = self.prepare_core(narou_rs::application::worker_ledger::claim_fresh(
                    &now,
                    &self.lease_until_rfc3339(),
                    &execution_token,
                    job_id.as_str(),
                    normalized_target,
                    site_key,
                ))?;
                if changed_rows(&statement.run().await.map_err(worker_error)?)? > 0 {
                    let row = self.select_row(job_id.as_str()).await?.ok_or_else(|| {
                        NarouError::Platform(format!("claimed job {job_id} disappeared"))
                    })?;
                    return Self::claimed_from_row(&row, execution_token);
                }
                return Ok(JobClaim::Busy);
            }

            // Reclaim: the row is running but its lease expired (crashed or
            // lost invocation). The same exclusion applies so a stale
            // redelivery cannot resurrect a novel that another executor is
            // actively working on.
            let statement = self.prepare_core(narou_rs::application::worker_ledger::claim_reclaim(
                &now,
                &self.lease_until_rfc3339(),
                &execution_token,
                job_id.as_str(),
                normalized_target,
                site_key,
            ))?;
            if changed_rows(&statement.run().await.map_err(worker_error)?)? > 0 {
                let row = self.select_row(job_id.as_str()).await?.ok_or_else(|| {
                    NarouError::Platform(format!("reclaimed job {job_id} disappeared"))
                })?;
                Self::claimed_from_row(&row, execution_token)
            } else {
                Ok(JobClaim::Busy)
            }
        })
    }

    fn yield_for_continuation<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        checkpoint: &'a narou_rs::application::WorkerExecutionCheckpoint,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let value = serde_json::to_string(checkpoint).map_err(|error| {
                NarouError::Platform(format!("execution checkpoint serialization: {error}"))
            })?;
            let statement = self.prepare_core(narou_rs::application::worker_ledger::yield_execution(
                &self.now_rfc3339(), job_id.as_str(), execution_token, &value,
            ))?;
            if changed_rows(&statement.run().await.map_err(worker_error)?)? == 0 {
                return Err(NarouError::Platform(format!(
                    "execution claim for job {job_id} is no longer valid"
                )));
            }
            Ok(())
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
            // Unlike yield_for_continuation this keeps status='running' and
            // the lease intact: it only refreshes the resume cursor.
            let statement = self.prepare(
                "UPDATE worker_jobs
                 SET checkpoint_json = ?, updated_at = ?
                 WHERE job_id = ? AND status = 'running' AND execution_token = ?",
                vec![
                    BindValue::Text(value),
                    BindValue::Text(self.now_rfc3339()),
                    BindValue::Text(job_id.as_str().to_string()),
                    BindValue::Text(execution_token.to_string()),
                ],
            )?;
            // A stale token means another executor owns the job; the write is
            // best-effort progress, so that is not an error.
            let _ = statement.run().await.map_err(worker_error)?;
            Ok(())
        })
    }

    fn record_attempt<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        error: &'a str,
        resume_after: Duration,
    ) -> PlatformFuture<'a, Result<u32>> {
        Box::pin(async move {
            // The attempt's queue backoff becomes the row's lease_until: a
            // `retryable` row stays unclaimable until it elapses, so an early
            // redelivery (same job id, ahead of schedule) must wait instead
            // of short-circuiting the backoff (native retry-count parity).
            // `Duration::ZERO` clears the hold (claimable immediately).
            let hold_until = if resume_after.is_zero() {
                BindValue::Null
            } else {
                BindValue::Text(
                    (self.clock.now_utc() + resume_after)
                        .to_rfc3339_opts(SecondsFormat::Nanos, true),
                )
            };
            let update = self.prepare(
                "UPDATE worker_jobs
                 SET attempts = attempts + 1, status = 'retryable', last_error = ?,
                     updated_at = ?, lease_until = ?
                 WHERE job_id = ? AND status = 'running' AND execution_token = ?",
                vec![
                    BindValue::Text(error.to_string()),
                    BindValue::Text(self.now_rfc3339()),
                    hold_until,
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
    db: DbHandle,
}

impl D1SchedulerCheckpoint {
    const SCOPE: &'static str = "scheduler";
    const KEY: &'static str = "auto_update";

    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }

    fn prepare(&self, sql: &str, values: Vec<BindValue>) -> Result<D1PreparedStatement> {

        bind_statement(self.db.prepare(sql), values)
    }
    fn generation_bind(generation: u64) -> Result<BindValue> {
        i64::try_from(generation)
            .map(BindValue::Int)
            .map_err(|_| {
                NarouError::Platform(format!(
                    "scheduler generation {generation} exceeds D1 INTEGER range"
                ))
            })
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
        if let Some(value) = row.value_yaml.as_deref()
            && let Ok(checkpoint) = serde_yaml::from_str(value)
        {
            return Ok(checkpoint);
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



    /// Persist a claimed checkpoint only while the planner still owns its
    /// generation and token. A stale cron cannot overwrite a reclaimed run.
    async fn persist_claim(
        &self,
        current: &SchedulerCheckpoint,
        next: &SchedulerCheckpoint,
        now: &str,
        running_claim: bool,
    ) -> Result<bool> {
        let value_json = serde_json::to_string(next).map_err(|error| {
            NarouError::Platform(format!("scheduler checkpoint serialization: {error}"))
        })?;
        let value_yaml = serde_yaml::to_string(next).map_err(|error| {
            NarouError::Platform(format!("scheduler checkpoint YAML serialization: {error}"))
        })?;
        let _next_generation = Self::generation_bind(next.generation)?;
        let condition = if running_claim {
            "AND json_extract(app_state.value_json, '$.state') = 'running'
             AND (json_extract(app_state.value_json, '$.planner_lease_until') IS NULL
                  OR json_extract(app_state.value_json, '$.planner_lease_until') <= ?)"
        } else {
            "AND json_extract(app_state.value_json, '$.state') = 'done'"
        };
        let mut values = vec![
            BindValue::Text(Self::SCOPE.to_string()),
            BindValue::Text(Self::KEY.to_string()),
            BindValue::Text(value_yaml),
            BindValue::Text(value_json),
            Self::generation_bind(current.generation)?,
        ];
        if running_claim {
            values.push(BindValue::Text(now.to_string()));
        }
        let statement = self.prepare(
            &format!(
                "INSERT INTO app_state (scope, key, value_yaml, value_json)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT (scope, key) DO UPDATE SET
                   value_yaml = excluded.value_yaml, value_json = excluded.value_json
                 WHERE CAST(json_extract(app_state.value_json, '$.generation') AS INTEGER) = ?
                   {condition}"
            ),
            values,
        )?;
        Ok(changed_rows(&statement.run().await.map_err(worker_error)?)? > 0)
    }

    fn planner_lease_until(now: &str) -> Result<String> {
        let now = DateTime::parse_from_rfc3339(now).map_err(|error| {
            NarouError::Platform(format!("invalid planner timestamp {now:?}: {error}"))
        })?;
        Ok((now + TimeDelta::minutes(2)).to_rfc3339_opts(SecondsFormat::Nanos, true))
    }

    fn new_planner_token() -> String {
        let now = js_sys::Date::now() as u64;
        let random = (js_sys::Math::random() * 18_446_744_073_709_551_616.0) as u64;
        format!("p{now:016x}{random:016x}")
    }

    /// Claim the existing running generation for this probe, preserving its
    /// cursor. An active lease is a normal skip, not a new generation.
    pub async fn claim_running_probe(&self, now: &str) -> Result<CheckpointClaim> {
        let current = self.load().await?;
        if current.state != narou_rs::application::CheckpointState::Running {
            return Ok(CheckpointClaim::Duplicate);
        }
        let token = Self::new_planner_token();
        let lease_until = Self::planner_lease_until(now)?;
        let next = match current.begin_with_lease(
            current.generation,
            now,
            token,
            lease_until,
        ) {
            CheckpointClaim::Busy => return Ok(CheckpointClaim::Busy),
            CheckpointClaim::Duplicate => return Ok(CheckpointClaim::Duplicate),
            CheckpointClaim::Claimed(next) => next,
        };
        if self.persist_claim(&current, &next, now, true).await? {
            Ok(CheckpointClaim::Claimed(next))
        } else {
            Ok(CheckpointClaim::Busy)
        }
    }

    /// Atomically start one new logical generation from a completed
    /// checkpoint. `expected_generation` is bound as an integer in D1.
    pub async fn claim_new_generation(
        &self,
        expected_generation: u64,
        generation: u64,
        now: &str,
    ) -> Result<CheckpointClaim> {
        let current = self.load().await?;
        if current.state != narou_rs::application::CheckpointState::Done
            || current.generation != expected_generation
        {
            return Ok(CheckpointClaim::Busy);
        }
        let next = match current.begin_with_lease(
            generation,
            now,
            Self::new_planner_token(),
            Self::planner_lease_until(now)?,
        ) {
            CheckpointClaim::Busy => return Ok(CheckpointClaim::Busy),
            CheckpointClaim::Duplicate => return Ok(CheckpointClaim::Duplicate),
            CheckpointClaim::Claimed(next) => next,
        };
        if self.persist_claim(&current, &next, now, false).await? {
            Ok(CheckpointClaim::Claimed(next))
        } else {
            Ok(CheckpointClaim::Busy)
        }
    }

    /// Save a page transition or finish only while `planner_token` still owns
    /// the generation. This is the write-side half of the planner lease.
    pub async fn save_owned(
        &self,
        checkpoint: &SchedulerCheckpoint,
        planner_token: &str,
    ) -> Result<()> {
        let value_json = serde_json::to_string(checkpoint).map_err(|error| {
            NarouError::Platform(format!("scheduler checkpoint JSON serialization: {error}"))
        })?;
        let value_yaml = serde_yaml::to_string(checkpoint).map_err(|error| {
            NarouError::Platform(format!("scheduler checkpoint YAML serialization: {error}"))
        })?;
        let generation = Self::generation_bind(checkpoint.generation)?;
        let statement = self.prepare(
            "UPDATE app_state SET value_yaml = ?, value_json = ?
             WHERE scope = ? AND key = ?
               AND CAST(json_extract(value_json, '$.generation') AS INTEGER) = ?
               AND json_extract(value_json, '$.planner_token') = ?",
            vec![
                BindValue::Text(value_yaml),
                BindValue::Text(value_json),
                BindValue::Text(Self::SCOPE.to_string()),
                BindValue::Text(Self::KEY.to_string()),
                generation,
                BindValue::Text(planner_token.to_string()),
            ],
        )?;
        if changed_rows(&statement.run().await.map_err(worker_error)?)? == 0 {
            return Err(NarouError::Platform(
                "scheduler planner lease is no longer valid".to_string(),
            ));
        }
        Ok(())
    }

}

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
    checkpoint_json: Option<String>,
    /// Only the batch dedupe lookup selects this column; per-id reads leave
    /// it unset.
    #[serde(default)]
    dedupe_key: Option<String>,
}

impl JobRow {
    /// Row → [`JobView`]. Everything except `kind` must parse (a corrupt
    /// target / options / status / timestamp is still an error), but an
    /// unparsable `kind` is carried through as the raw string: rows written
    /// by [`D1JobLedger::record_rejected`] use `kind='unknown'`, which has
    /// no [`JobKind`] counterpart.
    fn into_view(self) -> Result<JobView> {
        let kind = match JobKind::parse(&self.kind) {
            Some(kind) => JobKindView::Known(kind),
            None => JobKindView::Unparsed(self.kind),
        };
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
        Ok(JobView {
            job_id: JobId(self.job_id),
            job: JobPlanView {
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

/// One ledger row as `GET /api/jobs/:id` renders it.
///
/// The JSON is field-for-field identical to [`QueuedJobView`]; the only
/// difference is `job.kind`, which keeps the raw ledger string when the row
/// carries a kind [`JobKind::parse`] rejects (e.g. the `kind='unknown'`
/// poison rows written by [`D1JobLedger::record_rejected`]) — the row stays
/// viewable (status / attempts / last_error) instead of failing the read.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobView {
    pub job_id: JobId,
    pub job: JobPlanView,
    pub status: JobLedgerStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// [`JobPlan`] with the raw-kind escape hatch (see [`JobView`]).
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobPlanView {
    pub kind: JobKindView,
    pub target: JobTarget,
    pub options: Vec<String>,
}

/// A row's kind: parsed for canonical ledger values, the raw string for
/// values that have no [`JobKind`] (`kind='unknown'` poison rows).
#[derive(Debug, Clone)]
pub enum JobKindView {
    Known(JobKind),
    Unparsed(String),
}

impl serde::Serialize for JobKindView {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Known(kind) => kind.serialize(serializer),
            Self::Unparsed(raw) => serializer.serialize_str(raw),
        }
    }
}

impl JobView {
    /// The typed view [`JobQueue::get`] hands to execution planning: an
    /// unknown kind can never be planned, so it keeps the original error
    /// (only the HTTP read path in [`D1JobLedger::get_view`] shows it).
    fn try_into_typed(self) -> Result<QueuedJobView> {
        let kind = match self.job.kind {
            JobKindView::Known(kind) => kind,
            JobKindView::Unparsed(raw) => {
                return Err(NarouError::Platform(format!(
                    "unknown job kind in ledger: {raw:?}"
                )));
            }
        };
        Ok(QueuedJobView {
            job_id: self.job_id,
            job: JobPlan {
                kind,
                target: self.job.target,
                options: self.job.options,
            },
            status: self.status,
            attempts: self.attempts,
            last_error: self.last_error,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AttemptRow {
    attempts: i64,
}

#[derive(Debug, Deserialize)]
struct JobIdRow {
    job_id: String,
}

#[derive(Debug, Deserialize)]
pub struct QueuedJobRoute {
    pub job_id: String,
    pub kind: String,
}

/// One stuck active row selected by
/// [`narou_rs::application::worker_ledger::select_stuck`] for the cron
/// reaper. `attempts` is the ledger attempt counter (a `retryable` row
/// with `attempts >= max_retries` has spent its retry budget).
#[derive(Debug, Deserialize)]
pub struct StuckJob {
    pub job_id: String,
    pub kind: String,
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
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
    Int(i64),
    Null,
}

fn bind_statement(
    statement: D1PreparedStatement,
    values: Vec<BindValue>,
) -> Result<D1PreparedStatement> {
    let values: Vec<JsValue> = values
        .into_iter()
        .map(|value| match value {
            BindValue::Text(value) => JsValue::from_str(&value),
            BindValue::Int(value) => JsValue::from_f64(value as f64),
            BindValue::Null => JsValue::NULL,
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


#[cfg(test)]
mod tests {
    use super::{D1JobLedger, JobRow};
    use narou_rs::application::{
        ExecutionPhase, JobId, WorkerExecutionCheckpoint,
    };
    use narou_rs::platform::NovelId;

    #[test]
    fn job_row_checkpoint_round_trips_from_d1_shape() {
        let checkpoint = WorkerExecutionCheckpoint::planned(
            JobId("job-1".into()),
            Some(NovelId(42)),
        )
        .advance(ExecutionPhase::Fetching, Some(3));
        let row_json = serde_json::json!({
            "job_id": "job-1",
            "kind": "update",
            "target": "42",
            "options": "[]",
            "status": "pending",
            "attempts": 0,
            "last_error": null,
            "created_at": "2026-08-10T00:00:00Z",
            "updated_at": "2026-08-10T00:00:00Z",
            "lease_until": null,
            "execution_token": null,
            "checkpoint_json": serde_json::to_string(&checkpoint).unwrap(),
        });
        let row: JobRow = serde_json::from_value(row_json).unwrap();
        assert_eq!(
            D1JobLedger::checkpoint_from_row(&row).unwrap(),
            Some(checkpoint)
        );
    }
}
