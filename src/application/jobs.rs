//! Job planning service: deterministic job plans, no execution.
//!
//! The web UI's queue page and the auto-update scheduler need to describe
//! work (download, update, convert, send, mail, backup) as data before it is
//! handed to the queue. This service owns that planning logic: target
//! normalization, deduplication, and validation. It deliberately contains
//! no queue, process, or async-execution APIs — the queue layer consumes
//! [`JobPlan`] values and does the actual work.

use std::collections::HashSet;

use crate::application::error::ApplicationError;
use crate::error::NarouError;
use crate::platform::{NovelId, PlatformFuture};

/// The kind of work a job performs. Mirrors the queue's `JobType` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum JobKind {
    Download,
    Update,
    Convert,
    AutoUpdate,
    Send,
    Mail,
    Backup,
}

impl JobKind {
    /// The canonical string form used in dedupe keys and ledger rows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Update => "update",
            Self::Convert => "convert",
            Self::AutoUpdate => "auto_update",
            Self::Send => "send",
            Self::Mail => "mail",
            Self::Backup => "backup",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for unknown ledger values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "download" => Some(Self::Download),
            "update" => Some(Self::Update),
            "convert" => Some(Self::Convert),
            "auto_update" => Some(Self::AutoUpdate),
            "send" => Some(Self::Send),
            "mail" => Some(Self::Mail),
            "backup" => Some(Self::Backup),
            _ => None,
        }
    }

    /// Kinds the Worker queue consumer can execute with the shared
    /// Downloader. Anything else is routed to a durable blocked state.
    pub fn is_worker_executable(self) -> bool {
        matches!(self, Self::Download | Self::Update)
    }
}

/// A normalized job target.
///
/// Numeric ids are used directly; ncodes are kept as strings because
/// resolving an ncode to an id requires a repository lookup, which this pure
/// planning service does not perform. The queue layer resolves ncodes at
/// enqueue time. Auto-update is a targetless job and uses [`JobTarget::All`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum JobTarget {
    Id(NovelId),
    Ncode(String),
    All,
}

impl JobTarget {
    /// The canonical string form used for deduplication and display.
    pub fn as_str(&self) -> String {
        match self {
            Self::Id(id) => id.0.to_string(),
            Self::Ncode(ncode) => ncode.clone(),
            Self::All => "*".to_string(),
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for values a ledger could not
    /// have produced. Numeric strings are ids, `n…` strings are ncodes, and
    /// `*` is the targetless auto-update marker.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "*" => Some(Self::All),
            value if is_valid_ncode(value) => Some(Self::Ncode(value.to_string())),
            value => value.parse::<i64>().ok().map(|id| Self::Id(NovelId(id))),
        }
    }
}

/// A request to plan one or more jobs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobRequest {
    pub kind: JobKind,
    /// Target novels. Each target is either a numeric id or an ncode
    /// (`n1234ab`); mixed forms are normalized and deduplicated. Auto-update
    /// accepts an empty list because it operates on the whole library.
    pub targets: Vec<String>,
    /// Extra kind-specific options (e.g. `--force` for update).
    pub options: Vec<String>,
}

/// A planned job: one kind, one normalized target, and its options.
///
/// This is the unit the queue layer enqueues; it is deterministic — the same
/// request always produces the same plans in the same order. It is fully
/// serializable so it can travel through the queue as a compact payload
/// (never a `NovelRecord`, body, or TOC).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobPlan {
    pub kind: JobKind,
    pub target: JobTarget,
    pub options: Vec<String>,
}

impl JobPlan {
    /// The canonical deduplication key: `kind:target:options`.
    ///
    /// Options are trimmed, sorted, and deduplicated so `--force --verbose`
    /// and `--verbose --force` are the same effective options. The ledger
    /// keeps at most one *active* job per key and clears the key on terminal
    /// states, so a later identical request re-enqueues instead of stacking.
    pub fn dedupe_key(&self) -> String {
        let mut options: Vec<String> = self
            .options
            .iter()
            .map(|option| option.trim().to_string())
            .filter(|option| !option.is_empty())
            .collect();
        options.sort();
        options.dedup();
        format!(
            "{}:{}:{}",
            self.kind.as_str(),
            self.target.as_str(),
            options.join(",")
        )
    }
}

/// The result of planning a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobPlanResult {
    /// The deduplicated, normalized plans in input order.
    pub plans: Vec<JobPlan>,
    /// Targets that are neither a valid id nor a valid ncode.
    pub invalid: Vec<String>,
    /// Duplicate targets that were dropped (kept for reporting).
    pub duplicates: Vec<String>,
}

/// Concrete job planning service.
///
/// Pure and deterministic: no clock, no repository, no queue. Target
/// normalization accepts numeric ids and ncode strings; anything else is
/// reported as invalid.
pub struct JobService;
impl JobService {
    /// Plan a request into deduplicated [`JobPlan`]s.
    pub fn plan(&self, request: &JobRequest) -> JobPlanResult {
        if request.kind == JobKind::AutoUpdate && request.targets.is_empty() {
            return JobPlanResult {
                plans: vec![JobPlan {
                    kind: request.kind,
                    target: JobTarget::All,
                    options: request.options.clone(),
                }],
                invalid: Vec::new(),
                duplicates: Vec::new(),
            };
        }

        let mut plans = Vec::new();
        let mut invalid = Vec::new();
        let mut duplicates = Vec::new();
        let mut seen: HashSet<JobTarget> = HashSet::new();

        for raw in &request.targets {
            let target = raw.trim();
            if target.is_empty() {
                continue;
            }
            match normalize_target(target) {
                Some(normalized) => {
                    if seen.insert(normalized.clone()) {
                        plans.push(JobPlan {
                            kind: request.kind,
                            target: normalized,
                            options: request.options.clone(),
                        });
                    } else {
                        duplicates.push(target.to_string());
                    }
                }
                None => invalid.push(target.to_string()),
            }
        }

        JobPlanResult {
            plans,
            invalid,
            duplicates,
        }
    }

    /// Validate a single target string without planning.
    pub fn validate_target(&self, target: &str) -> Result<JobTarget, ApplicationError> {
        normalize_target(target).ok_or_else(|| {
            ApplicationError::InvalidRequest(format!("invalid job target: {target:?}"))
        })
    }

    /// Plan one bounded auto-update scan page: one discrete `Update` job per
    /// novel id, all sharing the same effective options.
    ///
    /// `page_size` bounds the number of ids the caller may scan per D1 round
    /// trip; a page that returns fewer than `page_size` ids is the final
    /// page (`done == true`, `next_cursor == None`).
    pub fn plan_auto_update_page(
        &self,
        ids: &[NovelId],
        options: &[String],
        page_size: usize,
    ) -> UpdateScanPage {
        let plans = ids
            .iter()
            .map(|id| JobPlan {
                kind: JobKind::Update,
                target: JobTarget::Id(*id),
                options: options.to_vec(),
            })
            .collect();
        let done = ids.len() < page_size;
        let next_cursor = if done { None } else { ids.last().map(|id| id.0) };
        UpdateScanPage {
            plans,
            next_cursor,
            done,
        }
    }
}

/// Normalize a target string to a [`JobTarget`].
///
/// Accepts a plain numeric id (`123`) or an ncode (`n1234ab`, case
/// insensitive, normalized to lowercase). Returns `None` for anything else.
fn normalize_target(target: &str) -> Option<JobTarget> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(id) = trimmed.parse::<i64>() {
        return Some(JobTarget::Id(NovelId(id)));
    }
    let lower = trimmed.to_ascii_lowercase();
    if is_valid_ncode(&lower) {
        return Some(JobTarget::Ncode(lower));
    }
    None
}

/// True for a well-formed ncode: `n` followed by 4–8 alphanumerics
/// (narou ncodes are `n` + 4–8 chars).
fn is_valid_ncode(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('n') else {
        return false;
    };
    (4..=8).contains(&rest.len()) && rest.chars().all(|ch| ch.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------------------
// Queue protocol types (Phase 8)
//
// These types are the platform-neutral contract between the planner and the
// queue layer, mirroring the application-owned ports in `events.rs`: no
// Cloudflare (or any other platform) type appears here. The Worker adapter
// implements [`JobQueue`] over D1 + the Queue binding; the native side keeps
// its `PersistentQueue`/`queue.yaml` untouched.
// ---------------------------------------------------------------------------

/// Current envelope version carried by [`WorkerJobEnvelope`].
pub const WORKER_JOB_ENVELOPE_VERSION: u32 = 2;

/// A job identifier in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JobId(pub String);

impl JobId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for JobId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One discrete queued job: one id, one plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedJob {
    pub job_id: JobId,
    pub job: JobPlan,
}

/// Ledger lifecycle states for one job row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobLedgerStatus {
    /// Enqueued, not yet claimed.
    Pending,
    /// Claimed and executing.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Finished with a partial/yielded result at a safe section boundary
    /// (or the bounded request/time guard tripped and partial state was
    /// recorded explicitly rather than running unbounded).
    Partial,
    /// Failed transiently; an attempt was recorded and a queue retry was
    /// scheduled. Still active until it resolves.
    Retryable,
    /// Cannot proceed without operator action (interactive auth/adult/digest
    /// decisions, unsupported job kinds, missing site definitions). Terminal.
    Blocked,
    /// Fatal failure (novel gone, invalid target, ...). Terminal.
    Permanent,
}

impl JobLedgerStatus {
    /// Active states participate in the dedupe index and can be claimed.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Running | Self::Retryable
        )
    }

    /// Terminal states are durably recorded before the queue message is acked.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Partial | Self::Blocked | Self::Permanent
        )
    }

    /// Parse the canonical ledger string form.
    pub fn parse(value: &str) -> crate::error::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "partial" => Ok(Self::Partial),
            "retryable" => Ok(Self::Retryable),
            "blocked" => Ok(Self::Blocked),
            "permanent" => Ok(Self::Permanent),
            other => Err(NarouError::Platform(format!(
                "unknown job ledger status: {other:?}"
            ))),
        }
    }
}

/// Why a job failed, used by the consumer to pick the next transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobFailureClass {
    /// Transient (network, transport, site rate limit): record an attempt and
    /// schedule a bounded queue retry.
    Retryable,
    /// Fatal: record the error and ack. Never retried.
    Permanent,
    /// Requires operator action (interactive decision on the Worker has no
    /// stdin; unsupported kind; missing site definition): record and ack.
    Blocked,
}

/// Classify a domain error into the worker's failure vocabulary.
///
/// This is deliberately conservative: anything unrecognized is terminal
/// (`Permanent`), never silently retried to success.
pub fn classify_failure(error: &NarouError) -> JobFailureClass {
    match error {
        NarouError::Http(_)
        | NarouError::Io(_)
        | NarouError::Platform(_)
        | NarouError::SuspendDownload(_)
        | NarouError::DownloadBudgetExpired { .. } => JobFailureClass::Retryable,
        NarouError::SiteSetting(_) | NarouError::Yaml(_) | NarouError::Regex(_) => {
            JobFailureClass::Blocked
        }
        // Interactive decisions (age verification, digest choice) have no
        // terminal on the Worker; the downloader reports them explicitly.
        NarouError::Unsupported(_) => JobFailureClass::Blocked,
        NarouError::NotFound(_)
        | NarouError::InvalidTarget(_)
        | NarouError::Conversion(_)
        | NarouError::Database(_) => JobFailureClass::Permanent,
    }
}

/// A queued job together with its ledger state (for `GET /api/jobs/:id`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedJobView {
    pub job_id: JobId,
    pub job: JobPlan,
    pub status: JobLedgerStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The version-2 worker queue envelope: one discrete job per message.
///
/// Version 1 carried a [`JobRequest`] (which plans *many* jobs); version 2
/// carries exactly one already-planned [`JobPlan`] plus its ledger id, so the
/// consumer never has to re-plan or split a message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerJobEnvelope {
    pub version: u32,
    pub job_id: JobId,
    pub job: JobPlan,
}

impl WorkerJobEnvelope {
    /// Build a version-2 envelope for a queued job.
    pub fn v2(job_id: JobId, job: JobPlan) -> Self {
        Self {
            version: WORKER_JOB_ENVELOPE_VERSION,
            job_id,
            job,
        }
    }
}

/// Result of decoding a legacy (version 1) envelope body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyEnvelopeOutcome {
    /// The request maps to exactly one unambiguous plan; promote it.
    Plan(JobPlan),
    /// The request is malformed, multi-target, or unsupported; it must be
    /// durably ledgered (blocked/permanent) and acked — never silently
    /// dropped or retried to success.
    Reject { reason: String },
}

/// Decode a legacy version-1 `JobRequest` body.
///
/// Safe only when the request plans exactly one discrete job with no invalid
/// targets (single target, or the targetless auto-update). Anything else is
/// rejected for durable ledgering.
pub fn decode_legacy_envelope(request: &JobRequest) -> LegacyEnvelopeOutcome {
    let result = JobService.plan(request);
    if result.plans.len() == 1 && result.invalid.is_empty() {
        return LegacyEnvelopeOutcome::Plan(result.plans.into_iter().next().unwrap());
    }
    let reason = if !result.invalid.is_empty() {
        format!(
            "legacy v1 envelope has invalid targets: {}",
            result.invalid.join(", ")
        )
    } else if result.plans.len() > 1 {
        "legacy v1 envelope maps to multiple discrete jobs".to_string()
    } else {
        "legacy v1 envelope plans no jobs".to_string()
    };
    LegacyEnvelopeOutcome::Reject { reason }
}

/// Bounded-payload limits for job requests and queue envelopes (Phase 8).
///
/// The Queue has its own message size cap, but sending an oversized payload
/// must never happen by accident: the API validates requests before
/// planning and re-checks the serialized envelope before the producer send.
pub mod job_limits {
    /// Maximum discrete targets accepted in one job request.
    pub const MAX_TARGETS_PER_REQUEST: usize = 100;
    /// Maximum characters per target string.
    pub const MAX_TARGET_CHARS: usize = 256;
    /// Maximum effective options per plan.
    pub const MAX_OPTIONS_PER_PLAN: usize = 16;
    /// Maximum characters per option string.
    pub const MAX_OPTION_CHARS: usize = 256;
    /// Maximum serialized envelope bytes sent to / accepted from the queue.
    pub const MAX_ENVELOPE_BYTES: usize = 4096;
}

/// Validate a job request against the bounded-payload limits.
///
/// Returns `None` when the request is within limits, else a reason.
pub fn validate_request_limits(request: &JobRequest) -> Option<String> {
    if request.targets.len() > job_limits::MAX_TARGETS_PER_REQUEST {
        return Some(format!(
            "too many targets: {} (max {})",
            request.targets.len(),
            job_limits::MAX_TARGETS_PER_REQUEST
        ));
    }
    for target in &request.targets {
        if target.len() > job_limits::MAX_TARGET_CHARS {
            return Some(format!(
                "target too long: {} chars (max {})",
                target.len(),
                job_limits::MAX_TARGET_CHARS
            ));
        }
    }
    if request.options.len() > job_limits::MAX_OPTIONS_PER_PLAN {
        return Some(format!(
            "too many options: {} (max {})",
            request.options.len(),
            job_limits::MAX_OPTIONS_PER_PLAN
        ));
    }
    for option in &request.options {
        if option.len() > job_limits::MAX_OPTION_CHARS {
            return Some(format!(
                "option too long: {} chars (max {})",
                option.len(),
                job_limits::MAX_OPTION_CHARS
            ));
        }
    }
    None
}

/// Serialized size (in bytes) of an envelope as it would cross the queue.
pub fn envelope_bytes(envelope: &WorkerJobEnvelope) -> crate::error::Result<usize> {
    serde_json::to_vec(envelope)
        .map(|bytes| bytes.len())
        .map_err(|error| {
            NarouError::Platform(format!("envelope serialization: {error}"))
        })
}

/// Execution phase of a worker job for soft-budget checkpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    /// Not started.
    Planned,
    /// Fetching TOC / sections from the site.
    Fetching,
    /// Persisting fetched content (object store / repository).
    Persisting,
    /// Completed.
    Finished,
}

/// Worker execution soft-budget / checkpoint shape (Phase 8).
///
/// Describes how far a job got: which novel, which phase, and the next section
/// index when the downloader yielded at a safe boundary. It deliberately
/// contains no Downloader state; persisted section data plus this cursor make
/// continuation idempotent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerExecutionCheckpoint {
    pub job_id: JobId,
    pub novel_id: Option<NovelId>,
    pub phase: ExecutionPhase,
    /// `Some` when the downloader yielded before starting this section.
    pub next_section_index: Option<u64>,
}

impl WorkerExecutionCheckpoint {
    /// Start a checkpoint for a job (phase `Planned`, no section position).
    pub fn planned(job_id: JobId, novel_id: Option<NovelId>) -> Self {
        Self {
            job_id,
            novel_id,
            phase: ExecutionPhase::Planned,
            next_section_index: None,
        }
    }

    /// Advance to a later phase, optionally recording the next section index.
    pub fn advance(&self, phase: ExecutionPhase, next_section_index: Option<u64>) -> Self {
        Self {
            job_id: self.job_id.clone(),
            novel_id: self.novel_id,
            phase,
            next_section_index,
        }
    }
}

/// One bounded auto-update scan page: the plans to enqueue plus the next
/// keyset cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateScanPage {
    pub plans: Vec<JobPlan>,
    /// Keyset cursor for the next page: the last id enqueued when the page
    /// was full; `None` when the scan is exhausted.
    pub next_cursor: Option<i64>,
    pub done: bool,
}

/// Planner checkpoint state, durably stored in D1 `app_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointState {
    Done,
    Running,
}

/// The auto-update planner checkpoint: generation + keyset cursor + run
/// timestamps. Pure transition logic, so the duplicate-cron rule is testable
/// on the host.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchedulerCheckpoint {
    /// Monotonic run generation (the cron trigger time in ms). Duplicate
    /// deliveries of the same generation are skipped.
    pub generation: u64,
    pub state: CheckpointState,
    /// Last novel id already enqueued by the current generation.
    pub cursor: Option<i64>,
    /// When the current generation was claimed.
    pub started_at: Option<String>,
    /// When the last completed generation finished (drives catch-up).
    pub last_run: Option<String>,
}

impl Default for SchedulerCheckpoint {
    fn default() -> Self {
        Self {
            generation: 0,
            state: CheckpointState::Done,
            cursor: None,
            started_at: None,
            last_run: None,
        }
    }
}

/// Outcome of trying to claim a planner generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointClaim {
    /// This exact generation is already running (duplicate cron delivery or
    /// overlapping retry): the planner must not enqueue again.
    Duplicate,
    /// The planner may proceed with the returned checkpoint.
    Claimed(SchedulerCheckpoint),
}

/// Outcome of trying to claim a queued job for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobClaim {
    /// The caller owns this execution token until it expires or is completed.
    Claimed { execution_token: String },
    /// The row is terminal and a redelivered message can be acknowledged.
    AlreadyTerminal,
    /// Another invocation owns a still-valid lease. The message must not be
    /// acknowledged or executed concurrently.
    Busy,
    /// No ledger row exists for the id carried by the envelope. The adapter
    /// durably records a blocked rejection before returning this outcome.
    Unknown,
}


impl SchedulerCheckpoint {
    /// Claim a generation for planning.
    ///
    /// A claim for the *same* generation is always rejected — whether the
    /// generation is still running (overlapping delivery) or already
    /// completed (redelivery after a crash or retry) — so a duplicate cron
    /// event never enqueues twice. A *new* generation takes over even when
    /// the previous one crashed mid-run (recovery happens at the next
    /// scheduled event); the ledger's active dedupe key keeps the re-enqueue
    /// from stacking duplicates.
    pub fn begin(&self, generation: u64, started_at: &str) -> CheckpointClaim {
        if self.generation == generation {
            CheckpointClaim::Duplicate
        } else {
            CheckpointClaim::Claimed(SchedulerCheckpoint {
                generation,
                state: CheckpointState::Running,
                cursor: None,
                started_at: Some(started_at.to_string()),
                last_run: self.last_run.clone(),
            })
        }
    }

    /// Advance the keyset cursor after enqueueing a full page.
    pub fn advance(&self, cursor: i64) -> Self {
        Self {
            generation: self.generation,
            state: self.state,
            cursor: Some(cursor),
            started_at: self.started_at.clone(),
            last_run: self.last_run.clone(),
        }
    }

    /// Complete the generation: record the finish time and release the run.
    pub fn finish(&self, finished_at: &str) -> Self {
        Self {
            generation: self.generation,
            state: CheckpointState::Done,
            cursor: None,
            started_at: None,
            last_run: Some(finished_at.to_string()),
        }
    }
}

/// Platform-neutral job queue port (Phase 8).
///
/// The queue layer consumes [`JobPlan`] values and reports discrete job
/// outcomes; the implementation owns durability (D1 on the Worker) and the
/// transport (Queue binding). The native desktop keeps its
/// `PersistentQueue`/`queue.yaml` unchanged and does not implement this
/// port.
pub trait JobQueue: Send + Sync {
    /// Enqueue one discrete job. When an *active* job with the same dedupe
    /// key already exists, returns that job's id instead of inserting a
    /// duplicate (idempotent enqueue).
    fn enqueue<'a>(&'a self, job: JobPlan) -> PlatformFuture<'a, crate::error::Result<QueuedJob>>;

    /// Read a queued job and its ledger state.
    fn get<'a>(
        &'a self,
        job_id: &'a JobId,
    ) -> PlatformFuture<'a, crate::error::Result<Option<QueuedJobView>>>;

    /// Atomically claim a job for execution.
    ///
    /// `Busy` is intentionally distinct from `AlreadyTerminal`: a valid
    /// running lease must cause redelivery, never an acknowledgement.
    fn claim<'a>(&'a self, job_id: &'a JobId) -> PlatformFuture<'a, crate::error::Result<JobClaim>>;

    /// Persist a resumable execution checkpoint while retaining the claim.
    fn save_checkpoint<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        checkpoint: &'a WorkerExecutionCheckpoint,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;

    /// Durably record one retryable attempt for the current execution token;
    /// returns the new attempt count. A stale token is an error.
    fn record_attempt<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        error: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<u32>>;

    /// Durably record a terminal state for the current execution token.
    /// The queue message is acked only after this succeeds.
    fn mark_terminal<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        status: JobLedgerStatus,
        error: Option<&'a str>,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> JobService {
        JobService
    }

    #[test]
    fn plan_deduplicates_and_normalizes_ids() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Update,
            targets: vec!["3".into(), "1".into(), "3".into(), " 2 ".into()],
            options: vec!["--force".into()],
        });
        assert_eq!(
            result.plans,
            vec![
                JobPlan {
                    kind: JobKind::Update,
                    target: JobTarget::Id(NovelId(3)),
                    options: vec!["--force".into()],
                },
                JobPlan {
                    kind: JobKind::Update,
                    target: JobTarget::Id(NovelId(1)),
                    options: vec!["--force".into()],
                },
                JobPlan {
                    kind: JobKind::Update,
                    target: JobTarget::Id(NovelId(2)),
                    options: vec!["--force".into()],
                },
            ]
        );
        assert_eq!(result.duplicates, vec!["3".to_string()]);
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn plan_normalizes_ncodes_and_deduplicates_case_insensitively() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Download,
            targets: vec!["N1234AB".into(), "n1234ab".into(), "n1234ab".into()],
            options: Vec::new(),
        });
        assert_eq!(
            result.plans,
            vec![JobPlan {
                kind: JobKind::Download,
                target: JobTarget::Ncode("n1234ab".into()),
                options: Vec::new(),
            }]
        );
        // Both later occurrences are duplicates (reported per occurrence).
        assert_eq!(result.duplicates, vec!["n1234ab".to_string(), "n1234ab".to_string()]);
    }

    #[test]
    fn plan_reports_invalid_targets() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Download,
            targets: vec!["1".into(), "not-a-target".into(), "".into()],
            options: Vec::new(),
        });
        assert_eq!(result.plans.len(), 1);
        assert_eq!(result.invalid, vec!["not-a-target".to_string()]);
    }

    #[test]
    fn validate_target_accepts_ids_and_ncodes() {
        assert_eq!(
            service().validate_target("42").unwrap(),
            JobTarget::Id(NovelId(42))
        );
        assert_eq!(
            service().validate_target("N1234AB").unwrap(),
            JobTarget::Ncode("n1234ab".into())
        );
        assert!(service().validate_target("").is_err());
        assert!(service().validate_target("n12").is_err());
    }

    #[test]
    fn plan_allows_targetless_auto_update() {
        let result = service().plan(&JobRequest {
            kind: JobKind::AutoUpdate,
            targets: Vec::new(),
            options: Vec::new(),
        });
        assert_eq!(
            result.plans,
            vec![JobPlan {
                kind: JobKind::AutoUpdate,
                target: JobTarget::All,
                options: Vec::new(),
            }]
        );
        assert!(result.invalid.is_empty());
        assert!(result.duplicates.is_empty());
    }

    #[test]
    fn empty_request_plans_nothing() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Backup,
            targets: Vec::new(),
            options: Vec::new(),
        });
        assert!(result.plans.is_empty());
        assert!(result.invalid.is_empty());
        assert!(result.duplicates.is_empty());
    }

    // --- Phase 8: queue protocol ---

    fn plan(kind: JobKind, target: JobTarget, options: &[&str]) -> JobPlan {
        JobPlan {
            kind,
            target,
            options: options.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn dedupe_key_is_canonical_and_option_order_independent() {
        let a = plan(JobKind::Update, JobTarget::Id(NovelId(7)), &["--force", "--verbose"]);
        let b = plan(JobKind::Update, JobTarget::Id(NovelId(7)), &["--verbose", " --force "]);
        assert_eq!(a.dedupe_key(), b.dedupe_key());
        assert_eq!(a.dedupe_key(), "update:7:--force,--verbose");

        // Different kinds or targets produce different keys.
        assert_ne!(
            a.dedupe_key(),
            plan(JobKind::Download, JobTarget::Id(NovelId(7)), &["--force"]).dedupe_key()
        );
        assert_ne!(
            a.dedupe_key(),
            plan(JobKind::Update, JobTarget::Id(NovelId(8)), &["--force"]).dedupe_key()
        );
        assert_ne!(
            a.dedupe_key(),
            plan(JobKind::Update, JobTarget::Ncode("n1234ab".into()), &["--force"]).dedupe_key()
        );
    }

    #[test]
    fn job_plan_round_trips_through_json() {
        let job = plan(JobKind::Update, JobTarget::Id(NovelId(42)), &["--force"]);
        let json = serde_json::to_string(&job).unwrap();
        let back: JobPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, job);
        assert!(json.contains("42"), "target id should be inline: {json}");

        let envelope = WorkerJobEnvelope::v2(JobId("job_1".into()), job);
        let json = serde_json::to_string(&envelope).unwrap();
        let back: WorkerJobEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
        assert_eq!(back.version, WORKER_JOB_ENVELOPE_VERSION);
    }

    #[test]
    fn legacy_envelope_decodes_only_single_unambiguous_requests() {
        let request = JobRequest {
            kind: JobKind::Download,
            targets: vec!["123".into()],
            options: vec!["--force".into()],
        };
        match decode_legacy_envelope(&request) {
            LegacyEnvelopeOutcome::Plan(job) => {
                assert_eq!(job, plan(JobKind::Download, JobTarget::Id(NovelId(123)), &["--force"]));
            }
            LegacyEnvelopeOutcome::Reject { reason } => panic!("expected plan, got reject: {reason}"),
        }

        // Auto-update with no targets is one unambiguous plan.
        let request = JobRequest {
            kind: JobKind::AutoUpdate,
            targets: Vec::new(),
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Plan(_)
        ));

        // Multi-target requests must be rejected, never split silently.
        let request = JobRequest {
            kind: JobKind::Download,
            targets: vec!["1".into(), "2".into()],
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Reject { .. }
        ));

        // Invalid targets are rejected.
        let request = JobRequest {
            kind: JobKind::Download,
            targets: vec!["not-a-target".into()],
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Reject { .. }
        ));

        // Unsupported kinds still decode (the executor blocks them later);
        // what must never happen is an empty request being treated as a job.
        let request = JobRequest {
            kind: JobKind::Backup,
            targets: Vec::new(),
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Reject { .. }
        ));
    }

    #[test]
    fn failure_classification_is_conservative() {
        use crate::error::NarouError;
        assert_eq!(
            classify_failure(&NarouError::Http("timeout".into())),
            JobFailureClass::Retryable
        );
        assert_eq!(
            classify_failure(&NarouError::SuspendDownload("slow down".into())),
            JobFailureClass::Retryable
        );

        assert_eq!(
            classify_failure(&NarouError::DownloadBudgetExpired {
                next_section_index: 4,
            }),
            JobFailureClass::Retryable
        );
        assert_eq!(
            classify_failure(&NarouError::NotFound("gone".into())),
            JobFailureClass::Permanent
        );
        assert_eq!(
            classify_failure(&NarouError::InvalidTarget("bad".into())),
            JobFailureClass::Permanent
        );
        assert_eq!(
            classify_failure(&NarouError::SiteSetting("no definition".into())),
            JobFailureClass::Blocked
        );
        assert_eq!(
            classify_failure(&NarouError::Unsupported(
                "interactive confirmation is not supported".into()
            )),
            JobFailureClass::Blocked
        );
    }

    #[test]
    fn ledger_status_active_terminal_partition() {
        for status in [
            JobLedgerStatus::Pending,
            JobLedgerStatus::Running,
            JobLedgerStatus::Retryable,
        ] {
            assert!(status.is_active());
            assert!(!status.is_terminal());
        }
        for status in [
            JobLedgerStatus::Succeeded,
            JobLedgerStatus::Partial,
            JobLedgerStatus::Blocked,
            JobLedgerStatus::Permanent,
        ] {
            assert!(!status.is_active());
            assert!(status.is_terminal());
        }
    }

    #[test]
    fn checkpoint_duplicate_generation_never_enqueues_twice() {
        let start = SchedulerCheckpoint::default();
        let claimed = match start.begin(7, "2026-08-10T00:00:00Z") {
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
            CheckpointClaim::Duplicate => panic!("fresh checkpoint must claim"),
        };
        assert_eq!(claimed.state, CheckpointState::Running);
        assert_eq!(claimed.generation, 7);

        // A duplicate delivery of the same generation is rejected.
        assert_eq!(claimed.begin(7, "2026-08-10T00:00:01Z"), CheckpointClaim::Duplicate);

        // A redelivery of an already-completed generation is also rejected
        // (never re-execute the same cron event).
        let finished = claimed.finish("2026-08-10T00:05:00Z");
        assert_eq!(finished.begin(7, "2026-08-10T00:06:00Z"), CheckpointClaim::Duplicate);

        // A new generation takes over (crash recovery) with a fresh cursor.
        let next = match claimed.begin(8, "2026-08-10T00:30:00Z") {
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
            CheckpointClaim::Duplicate => panic!("new generation must claim"),
        };
        assert_eq!(next.generation, 8);
        assert_eq!(next.cursor, None);
        assert_eq!(next.last_run, claimed.last_run);
    }

    #[test]
    fn checkpoint_cursor_and_finish() {
        let checkpoint = SchedulerCheckpoint::default();
        let claimed = match checkpoint.begin(1, "2026-08-10T00:00:00Z") {
            CheckpointClaim::Claimed(value) => value,
            CheckpointClaim::Duplicate => unreachable!(),
        };
        let advanced = claimed.advance(500);
        assert_eq!(advanced.cursor, Some(500));
        let finished = advanced.finish("2026-08-10T00:05:00Z");
        assert_eq!(finished.state, CheckpointState::Done);
        assert_eq!(finished.cursor, None);
        assert_eq!(finished.last_run.as_deref(), Some("2026-08-10T00:05:00Z"));
    }

    #[test]
    fn request_and_envelope_payload_limits_are_bounded() {
        use crate::application::job_limits;

        let ok = JobRequest {
            kind: JobKind::Download,
            targets: vec!["123".into(), "n1234ab".into()],
            options: vec!["--force".into()],
        };
        assert_eq!(validate_request_limits(&ok), None);

        let many_targets = JobRequest {
            kind: JobKind::Download,
            targets: (0..job_limits::MAX_TARGETS_PER_REQUEST + 1)
                .map(|id| id.to_string())
                .collect(),
            options: Vec::new(),
        };
        assert!(validate_request_limits(&many_targets).is_some());

        let long_target = JobRequest {
            kind: JobKind::Download,
            targets: vec!["x".repeat(job_limits::MAX_TARGET_CHARS + 1)],
            options: Vec::new(),
        };
        assert!(validate_request_limits(&long_target).is_some());

        let too_many_options = JobRequest {
            kind: JobKind::Download,
            targets: vec!["1".into()],
            options: (0..job_limits::MAX_OPTIONS_PER_PLAN + 1)
                .map(|i| format!("--opt-{i}"))
                .collect(),
        };
        assert!(validate_request_limits(&too_many_options).is_some());

        let long_option = JobRequest {
            kind: JobKind::Download,
            targets: vec!["1".into()],
            options: vec!["o".repeat(job_limits::MAX_OPTION_CHARS + 1)],
        };
        assert!(validate_request_limits(&long_option).is_some());

        // A normal envelope is well under the byte cap.
        let envelope = WorkerJobEnvelope::v2(
            JobId("j1".into()),
            plan(JobKind::Update, JobTarget::Id(NovelId(42)), &["--force"]),
        );
        let size = envelope_bytes(&envelope).unwrap();
        assert!(size <= job_limits::MAX_ENVELOPE_BYTES);
    }

    #[test]
    fn execution_checkpoint_shape_round_trips_and_advances() {
        let checkpoint = WorkerExecutionCheckpoint::planned(
            JobId("j1".into()),
            Some(NovelId(42)),
        );
        assert_eq!(checkpoint.phase, ExecutionPhase::Planned);
        assert_eq!(checkpoint.next_section_index, None);

        let advanced = checkpoint.advance(ExecutionPhase::Fetching, Some(3));
        assert_eq!(advanced.phase, ExecutionPhase::Fetching);
        assert_eq!(advanced.next_section_index, Some(3));
        assert_eq!(advanced.job_id, checkpoint.job_id);
        assert_eq!(advanced.novel_id, checkpoint.novel_id);

        let json = serde_json::to_string(&advanced).unwrap();
        let back: WorkerExecutionCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, advanced);
        assert!(json.contains("fetching"));
        assert!(json.contains("next_section_index"));
    }

    #[test]
    fn bounded_auto_update_page_plans_one_job_per_novel() {
        // A full page (ids == page_size) is not done and carries the cursor.
        let page = service().plan_auto_update_page(
            &[NovelId(1), NovelId(2), NovelId(3)],
            &["--force".into()],
            3,
        );
        assert!(!page.done);
        assert_eq!(page.next_cursor, Some(3));
        assert_eq!(
            page.plans,
            vec![
                plan(JobKind::Update, JobTarget::Id(NovelId(1)), &["--force"]),
                plan(JobKind::Update, JobTarget::Id(NovelId(2)), &["--force"]),
                plan(JobKind::Update, JobTarget::Id(NovelId(3)), &["--force"]),
            ]
        );

        // A short (final) page is marked done with no cursor.
        let page = service().plan_auto_update_page(&[NovelId(9)], &[], 3);
        assert!(page.done);
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.plans.len(), 1);

        // An empty page is done.
        let page = service().plan_auto_update_page(&[], &[], 3);
        assert!(page.done);
        assert_eq!(page.next_cursor, None);
        assert!(page.plans.is_empty());
    }
}
