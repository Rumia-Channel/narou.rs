//! Job planning service: deterministic job plans, no execution.
//!
//! The web UI's queue page and the auto-update scheduler need to describe
//! work (download, update, convert, send, mail, backup) as data before it is
//! handed to the queue. This service owns that planning logic: target
//! normalization, deduplication, and validation. It deliberately contains
//! no queue, process, or async-execution APIs — the queue layer consumes
//! [`JobPlan`] values and does the actual work.

use std::collections::HashSet;
use std::time::Duration;

use chrono::DateTime;

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

    /// Kinds the Worker queue consumer can execute with the shared portable
    /// capabilities (`Downloader` / `ConvertService`). Anything else is routed
    /// to a durable blocked state.
    /// is ported; download-time EPUB already reads the `novel.txt` object
    /// ([`crate::platform::NovelObjectKeys::converted_text`]) instead.
    pub fn is_worker_executable(self) -> bool {
        // Convert は保存済みの TOC と本文から変換テキストを組むだけで、
        // 外部プロセスを使わない (`ConvertService`)。
        matches!(self, Self::Download | Self::Update | Self::Convert)
    }
}

/// A normalized job target.
///
/// Numeric ids are used directly; ncodes are normalized to lowercase, and
/// HTTP(S) URLs are normalized before they enter the durable queue. Auto-update
/// is a targetless job and uses [`JobTarget::All`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum JobTarget {
    Id(NovelId),
    Ncode(String),
    Url(String),
    All,
}

impl JobTarget {
    /// The canonical string form used for deduplication and display.
    pub fn as_str(&self) -> String {
        match self {
            Self::Id(id) => id.0.to_string(),
            Self::Ncode(ncode) => ncode.clone(),
            Self::Url(url) => url.clone(),
            Self::All => "*".to_string(),
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for values a ledger could not
    /// have produced. Numeric strings are ids, `n…` strings are ncodes,
    /// HTTP(S) URLs are URL targets, and `*` is the auto-update marker.
    pub fn parse(value: &str) -> Option<Self> {
        if value == "*" {
            return Some(Self::All);
        }
        if is_valid_ncode(value) {
            return Some(Self::Ncode(value.to_string()));
        }
        if let Ok(id) = value.parse::<i64>() {
            return Some(Self::Id(NovelId(id)));
        }
        normalize_url_target(value).map(Self::Url)
    }
}

/// A request to plan one or more jobs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobRequest {
    pub kind: JobKind,
    /// Target novels. Each target is a numeric id, an ncode (`n1234ab`), or
    /// an HTTP(S) novel URL. Mixed forms are normalized and deduplicated.
    /// Auto-update accepts an empty list because it operates on the library.
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

/// Extract the set of numeric novel IDs from a job target string.
///
/// Shared by the native queue (`crate::queue`) and the Worker ledger
/// (`worker_jobs`) so both backends apply the same per-novel exclusion rule:
/// a claim must not start while another job that touches the same novel is
/// running. Native targets are tab-separated token lists (e.g.
/// `"12\tkindle"`, `"--force\t12"`) while Worker ledger `target` values are
/// canonical [`JobTarget::as_str`] strings; in both shapes only tokens that
/// parse as `i64` count, so flag tokens, `tag:…` selectors and ncode/URL
/// targets never trigger the lock. An empty result means the job is not
/// tied to a specific novel id (e.g. `AutoUpdate` broadcasts).
pub fn extract_novel_ids(target: &str) -> HashSet<i64> {
    target
        .split('\t')
        .filter_map(|part| part.parse::<i64>().ok())
        .collect()
}

/// Worker ledger (`worker_jobs` on D1) claim statements.
///
/// The claim UPDATEs are defined here — not inside the D1 adapter — so the
/// host-side tests can execute the *same* SQL against rusqlite and pin the
/// semantics that D1 cannot exercise locally: per-novel exclusion
/// ([`extract_novel_ids`]) and the retry backoff hold (`resume_after`
/// becomes `lease_until`, and a `retryable` row is not claimable until it
/// elapses).
///
/// Conventions mirror `worker_entry/src/ledger.rs`: timestamps are
/// canonical UTC RFC3339 strings so lexicographic comparison is sound, and
/// novel ids are bound as canonical decimal `target` strings (worker
/// targets are single normalized tokens, so an `i64` id and its decimal
/// string are the same value — the same scope as the native
/// `job_conflicts_with_running` check).
pub mod worker_ledger {
    use super::extract_novel_ids;
    use std::collections::HashSet;

    /// A fully-formed claim UPDATE: SQL text plus bind values in order.
    /// Every bind is a `TEXT` string (RFC3339 timestamps, job id, canonical
    /// target strings), matching the `BindValue::Text` usage in the D1
    /// adapter.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ClaimStatement {
        pub sql: String,
        pub binds: Vec<String>,
    }

    /// Build the "fresh" claim: promote a `pending` row, or a `retryable`
    /// row whose backoff hold (`lease_until`) has elapsed, to `running` —
    /// but only when no *other* row covering the same novel id is running.
    ///
    /// - `now` / `lease_until` / `job_id`: canonical strings
    /// - `execution_token`: the new claim token
    /// - `target`: the candidate row's canonical target (for extraction)
    ///
    /// Bind order: `updated_at, lease_until, execution_token, job_id, now,
    /// conflict target…`.
    pub fn claim_fresh(
        now: &str,
        lease_until: &str,
        execution_token: &str,
        job_id: &str,
        target: &str,
    ) -> ClaimStatement {
        let (clause, conflict) = conflict_clause(target);
        let mut binds = vec![
            now.to_string(),
            lease_until.to_string(),
            execution_token.to_string(),
            job_id.to_string(),
            now.to_string(),
        ];
        binds.extend(conflict);
        ClaimStatement {
            sql: format!(
                "UPDATE worker_jobs \
                 SET status = 'running', updated_at = ?, lease_until = ?, execution_token = ? \
                 WHERE job_id = ? \
                   AND (status = 'pending' \
                        OR (status = 'retryable' AND (lease_until IS NULL OR lease_until <= ?)))\
                 {clause}"
            ),
            binds,
        }
    }

    /// Build the lease-expired reclaim: take over a `running` row whose
    /// lease elapsed (or was never written), under the same per-novel
    /// exclusion as [`claim_fresh`].
    ///
    /// Bind order: `updated_at, lease_until, execution_token, job_id, now,
    /// conflict target…`.
    pub fn claim_reclaim(
        now: &str,
        lease_until: &str,
        execution_token: &str,
        job_id: &str,
        target: &str,
    ) -> ClaimStatement {
        let (clause, conflict) = conflict_clause(target);
        let mut binds = vec![
            now.to_string(),
            lease_until.to_string(),
            execution_token.to_string(),
            job_id.to_string(),
            now.to_string(),
        ];
        binds.extend(conflict);
        ClaimStatement {
            sql: format!(
                "UPDATE worker_jobs \
                 SET status = 'running', updated_at = ?, lease_until = ?, execution_token = ? \
                 WHERE job_id = ? AND status = 'running' \
                   AND (lease_until IS NULL OR lease_until <= ?)\
                 {clause}"
            ),
            binds,
        }
    }

    /// `AND NOT EXISTS …` clause plus the canonical target binds for the
    /// candidate's novel ids, or an empty clause when the target carries no
    /// numeric id. `other.job_id != worker_jobs.job_id` keeps the reclaim
    /// statement from seeing its own still-`running` row as a conflict.
    fn conflict_clause(target: &str) -> (String, Vec<String>) {
        let novel_ids: HashSet<i64> = extract_novel_ids(target);
        if novel_ids.is_empty() {
            return (String::new(), Vec::new());
        }
        // Deterministic order keeps the generated SQL (and tests) stable.
        let mut ids: Vec<i64> = novel_ids.into_iter().collect();
        ids.sort_unstable();
        let binds: Vec<String> = ids.iter().map(i64::to_string).collect();
        let placeholders = binds.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let clause = format!(
            " AND NOT EXISTS (SELECT 1 FROM worker_jobs AS other \
             WHERE other.status = 'running' AND other.job_id != worker_jobs.job_id \
               AND other.target IN ({placeholders}))"
        );
        (clause, binds)
    }
}

/// The result of planning a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobPlanResult {
    /// The deduplicated, normalized plans in input order.
    pub plans: Vec<JobPlan>,
    /// Targets that are neither a valid id, ncode, nor HTTP(S) URL.
    pub invalid: Vec<String>,
    /// Duplicate targets that were dropped (kept for reporting).
    pub duplicates: Vec<String>,
}

/// Concrete job planning service.
///
/// Pure and deterministic: no clock, no repository, no queue. Target
/// normalization accepts numeric ids, ncodes, and HTTP(S) URLs; anything else
/// is reported as invalid.
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
/// Accepts a plain numeric id (`123`), an ncode (`n1234ab`, case-insensitive),
/// or an absolute HTTP(S) URL. URL fragments are removed because they are not
/// sent in HTTP requests and must not create distinct queue jobs.
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
    normalize_url_target(trimmed).map(JobTarget::Url)
}

fn normalize_url_target(value: &str) -> Option<String> {
    let mut parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    parsed.set_fragment(None);
    Some(parsed.to_string())
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
        NarouError::DownloadResumeCorrupt(_)
        | NarouError::SiteSetting(_)
        | NarouError::Yaml(_)
        | NarouError::Regex(_) => JobFailureClass::Blocked,
        // A credential that cannot be decrypted (wrong key, missing
        // passphrase) needs the operator to import it again.
        NarouError::Login(_) => JobFailureClass::Blocked,
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
    /// 計画本体。**キューには載せない**（D1 台帳が唯一の権威）。
    /// 旧バージョンが積んだメッセージを読むためだけに残す。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobPlan>,
}

impl WorkerJobEnvelope {
    /// Build a version-2 envelope: the id only.
    ///
    /// 台帳 (D1) に計画があるので、メッセージは id だけを運ぶ。メッセージが
    /// 小さくなり、計画の書き換え・回復が台帳側だけで完結する。
    pub fn v2(job_id: JobId) -> Self {
        Self {
            version: WORKER_JOB_ENVELOPE_VERSION,
            job_id,
            job: None,
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
        // 定数名・doc・エラー文言が揃って「文字数」を指すので chars() で数える。
        // バイト長は `envelope_bytes` が別途直列化後に検査する。
        let chars = target.chars().count();
        if chars > job_limits::MAX_TARGET_CHARS {
            return Some(format!(
                "target too long: {} chars (max {})",
                chars,
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
        let chars = option.chars().count();
        if chars > job_limits::MAX_OPTION_CHARS {
            return Some(format!(
                "option too long: {} chars (max {})",
                chars,
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
    /// Monotonic logical run generation. Cron probe timestamps are not
    /// generations; a generation changes only when a due run starts.
    pub generation: u64,
    pub state: CheckpointState,
    /// Last novel id already enqueued by the current generation.
    pub cursor: Option<i64>,
    /// When the current logical generation was claimed.
    pub started_at: Option<String>,
    /// When the last completed generation finished (drives catch-up).
    pub last_run: Option<String>,
    /// Lease held by the planner that owns the current page.
    #[serde(default)]
    pub planner_lease_until: Option<String>,
    /// Ownership token for the current planner lease.
    #[serde(default)]
    pub planner_token: Option<String>,
}

impl Default for SchedulerCheckpoint {
    fn default() -> Self {
        Self {
            generation: 0,
            state: CheckpointState::Done,
            cursor: None,
            started_at: None,
            last_run: None,
            planner_lease_until: None,
            planner_token: None,
        }
    }
}

/// Outcome of trying to claim a planner generation or probe lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointClaim {
    /// Another planner currently owns the lease.
    Busy,
    /// The probe or generation was already handled.
    Duplicate,
    /// The planner may proceed with the returned checkpoint.
    Claimed(SchedulerCheckpoint),
}

/// Outcome of trying to claim a queued job for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobClaim {
    /// The caller owns this execution token until it expires or is completed.
    Claimed {
        execution_token: String,
        checkpoint: Option<WorkerExecutionCheckpoint>,
    },
    /// The row is terminal and a redelivered message can be acknowledged.
    AlreadyTerminal,
    /// Another invocation owns a still-valid lease. The message must be
    /// delayed until that lease expires; it must not be acknowledged.
    Busy { retry_after: Duration },
    /// No ledger row exists for the id carried by the envelope. The adapter
    /// durably records a blocked rejection before returning this outcome.
    Unknown,
}

impl SchedulerCheckpoint {
    /// Claim a logical generation without a planner lease.
    ///
    /// This compatibility helper is used by pure host tests. Worker
    /// scheduling uses [`Self::begin_with_lease`] so a minute probe cannot
    /// create a new generation while a page is still running.
    pub fn begin(&self, generation: u64, started_at: &str) -> CheckpointClaim {
        if self.generation >= generation {
            CheckpointClaim::Duplicate
        } else {
            CheckpointClaim::Claimed(Self {
                generation,
                state: CheckpointState::Running,
                cursor: None,
                started_at: Some(started_at.to_string()),
                last_run: self.last_run.clone(),
                planner_lease_until: None,
                planner_token: None,
            })
        }
    }

    /// Claim either a new logical generation or the existing running
    /// generation. An active planner lease returns `Busy`; an expired lease
    /// preserves the generation and cursor while replacing its owner token.
    pub fn begin_with_lease(
        &self,
        generation: u64,
        now: &str,
        planner_token: String,
        planner_lease_until: String,
    ) -> CheckpointClaim {
        if self.state == CheckpointState::Running {
            if self.planner_lease_active(now) {
                return CheckpointClaim::Busy;
            }
            return CheckpointClaim::Claimed(Self {
                generation: self.generation,
                state: CheckpointState::Running,
                cursor: self.cursor,
                started_at: self.started_at.clone().or_else(|| Some(now.to_string())),
                last_run: self.last_run.clone(),
                planner_lease_until: Some(planner_lease_until),
                planner_token: Some(planner_token),
            });
        }
        if self.generation >= generation {
            return CheckpointClaim::Duplicate;
        }
        CheckpointClaim::Claimed(Self {
            generation,
            state: CheckpointState::Running,
            cursor: None,
            started_at: Some(now.to_string()),
            last_run: self.last_run.clone(),
            planner_lease_until: Some(planner_lease_until),
            planner_token: Some(planner_token),
        })
    }

    /// Whether the planner lease is still valid at `now`.
    pub fn planner_lease_active(&self, now: &str) -> bool {
        let Some(lease_until) = self.planner_lease_until.as_deref() else {
            return false;
        };
        let Ok(now) = DateTime::parse_from_rfc3339(now) else {
            return false;
        };
        let Ok(lease_until) = DateTime::parse_from_rfc3339(lease_until) else {
            return false;
        };
        lease_until > now
    }

    /// Advance the keyset cursor after enqueueing a full page.
    pub fn advance(&self, cursor: i64) -> Self {
        Self {
            generation: self.generation,
            state: self.state,
            cursor: Some(cursor),
            started_at: self.started_at.clone(),
            last_run: self.last_run.clone(),
            planner_lease_until: self.planner_lease_until.clone(),
            planner_token: self.planner_token.clone(),
        }
    }

    /// Complete the generation: record the finish time and release the run
    /// and its planner ownership.
    pub fn finish(&self, finished_at: &str) -> Self {
        Self {
            generation: self.generation,
            state: CheckpointState::Done,
            cursor: None,
            started_at: None,
            last_run: Some(finished_at.to_string()),
            planner_lease_until: None,
            planner_token: None,
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

    /// Persist a budget-yielded checkpoint and release the execution claim.
    ///
    /// The next queue envelope is a continuation, not a failed attempt, so
    /// implementations must not increment `attempts`.
    fn yield_for_continuation<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        checkpoint: &'a WorkerExecutionCheckpoint,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
    /// Persist a mid-execution progress checkpoint without releasing the
    /// claim or changing the job status. Used so a crashed invocation resumes
    /// near the last completed section instead of restarting the download.
    /// Default is a no-op for backends without durable checkpoints.
    fn save_checkpoint<'a>(
        &'a self,
        _job_id: &'a JobId,
        _execution_token: &'a str,
        _checkpoint: &'a WorkerExecutionCheckpoint,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        Box::pin(async { Ok(()) })
    }


    /// Durably record one retryable attempt for the current execution token;
    /// returns the new attempt count. A stale token is an error.
    ///
    /// `resume_after` is the scheduled queue backoff for this attempt: the
    /// ledger keeps the row unclaimable until then, so a redelivery that
    /// arrives before the delay fires cannot short-circuit the backoff.
    /// Passing `Duration::ZERO` (or an already-elapsed duration) makes the
    /// row immediately claimable.
    fn record_attempt<'a>(
        &'a self,
        job_id: &'a JobId,
        execution_token: &'a str,
        error: &'a str,
        resume_after: Duration,
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
        assert_eq!(result.duplicates, vec!["n1234ab".to_string(), "n1234ab".to_string()]);
    }

    #[test]
    fn plan_normalizes_urls_and_deduplicates_fragments() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Download,
            targets: vec![
                " https://EXAMPLE.com/novel/42#chapter-1 ".into(),
                "https://example.com/novel/42#chapter-2".into(),
            ],
            options: Vec::new(),
        });
        assert_eq!(
            result.plans,
            vec![JobPlan {
                kind: JobKind::Download,
                target: JobTarget::Url("https://example.com/novel/42".into()),
                options: Vec::new(),
            }]
        );
        assert_eq!(
            result.duplicates,
            vec!["https://example.com/novel/42#chapter-2".to_string()]
        );
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
    fn validate_target_accepts_ids_ncodes_and_urls() {
        assert_eq!(
            service().validate_target("42").unwrap(),
            JobTarget::Id(NovelId(42))
        );
        assert_eq!(
            service().validate_target("N1234AB").unwrap(),
            JobTarget::Ncode("n1234ab".into())
        );
        assert_eq!(
            service()
                .validate_target("https://Example.com/work/1#fragment")
                .unwrap(),
            JobTarget::Url("https://example.com/work/1".into())
        );
        assert_eq!(
            JobTarget::parse("https://example.com/work/1"),
            Some(JobTarget::Url("https://example.com/work/1".into()))
        );
        assert!(service().validate_target("").is_err());
        assert!(service().validate_target("n12").is_err());
        assert!(service().validate_target("file:///tmp/novel").is_err());
        assert!(service().validate_target("https://user:pass@example.com/novel").is_err());
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
        assert_ne!(
            a.dedupe_key(),
            plan(
                JobKind::Update,
                JobTarget::Url("https://example.com/novel/7".into()),
                &["--force"],
            )
            .dedupe_key()
        );
    }

    #[test]
    fn job_plan_round_trips_through_json() {
        let job = plan(JobKind::Update, JobTarget::Id(NovelId(42)), &["--force"]);
        let json = serde_json::to_string(&job).unwrap();
        let back: JobPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, job);
        assert!(json.contains("42"), "target id should be inline: {json}");

        let envelope = WorkerJobEnvelope::v2(JobId("job_1".into()));
        let json = serde_json::to_string(&envelope).unwrap();
        let back: WorkerJobEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
        assert_eq!(back.version, WORKER_JOB_ENVELOPE_VERSION);

        let url_job = plan(
            JobKind::Download,
            JobTarget::Url("https://example.com/novel/42".into()),
            &[],
        );
        let json = serde_json::to_string(&url_job).unwrap();
        let back: JobPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, url_job);
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

        let request = JobRequest {
            kind: JobKind::AutoUpdate,
            targets: Vec::new(),
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Plan(_)
        ));

        let request = JobRequest {
            kind: JobKind::Download,
            targets: vec!["1".into(), "2".into()],
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Reject { .. }
        ));

        let request = JobRequest {
            kind: JobKind::Download,
            targets: vec!["not-a-target".into()],
            options: Vec::new(),
        };
        assert!(matches!(
            decode_legacy_envelope(&request),
            LegacyEnvelopeOutcome::Reject { .. }
        ));

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
        assert_eq!(
            classify_failure(&NarouError::DownloadResumeCorrupt("missing section".into())),
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
            CheckpointClaim::Busy => panic!("compatibility begin cannot be busy"),
        };
        assert_eq!(claimed.state, CheckpointState::Running);
        assert_eq!(claimed.generation, 7);
        assert_eq!(claimed.begin(7, "2026-08-10T00:00:01Z"), CheckpointClaim::Duplicate);
        let finished = claimed.finish("2026-08-10T00:05:00Z");
        assert_eq!(finished.begin(7, "2026-08-10T00:06:00Z"), CheckpointClaim::Duplicate);
        let next = match claimed.begin(8, "2026-08-10T00:30:00Z") {
            CheckpointClaim::Claimed(checkpoint) => checkpoint,
            CheckpointClaim::Duplicate => panic!("new generation must claim"),
            CheckpointClaim::Busy => panic!("compatibility begin cannot be busy"),
        };
        assert_eq!(next.generation, 8);
        assert_eq!(next.cursor, None);
        assert_eq!(next.last_run, claimed.last_run);
    }

    #[test]
    fn planner_lease_blocks_duplicate_probe_and_preserves_cursor_on_reclaim() {
        let checkpoint = SchedulerCheckpoint::default();
        let claimed = match checkpoint.begin_with_lease(
            1,
            "2026-08-10T00:00:00Z",
            "planner-a".into(),
            "2026-08-10T00:02:00Z".into(),
        ) {
            CheckpointClaim::Claimed(value) => value,
            CheckpointClaim::Duplicate | CheckpointClaim::Busy => {
                panic!("initial planner claim must succeed")
            }
        };
        let advanced = claimed.advance(42);
        assert_eq!(
            advanced.begin_with_lease(
                1,
                "2026-08-10T00:01:00Z",
                "planner-b".into(),
                "2026-08-10T00:03:00Z".into(),
            ),
            CheckpointClaim::Busy
        );

        let reclaimed = match advanced.begin_with_lease(
            1,
            "2026-08-10T00:03:00Z",
            "planner-b".into(),
            "2026-08-10T00:05:00Z".into(),
        ) {
            CheckpointClaim::Claimed(value) => value,
            CheckpointClaim::Duplicate | CheckpointClaim::Busy => {
                panic!("expired planner lease must be reclaimable")
            }
        };
        assert_eq!(reclaimed.generation, 1);
        assert_eq!(reclaimed.cursor, Some(42));
        assert_eq!(reclaimed.planner_token.as_deref(), Some("planner-b"));
    }

    #[test]
    fn checkpoint_cursor_and_finish() {
        let checkpoint = SchedulerCheckpoint::default();
        let claimed = match checkpoint.begin(1, "2026-08-10T00:00:00Z") {
            CheckpointClaim::Claimed(value) => value,
            CheckpointClaim::Duplicate => unreachable!(),
            CheckpointClaim::Busy => unreachable!(),
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
            targets: vec!["123".into(), "n1234ab".into(), "https://example.com/novel/1".into()],
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

        // 上限は「文字数」: 日本語 90 文字 (=270 bytes) は受理、257 文字は拒否。
        let multibyte_ok = JobRequest {
            kind: JobKind::Download,
            targets: vec!["あ".repeat(job_limits::MAX_TARGET_CHARS)],
            options: Vec::new(),
        };
        assert_eq!(validate_request_limits(&multibyte_ok), None);
        let multibyte_ng = JobRequest {
            kind: JobKind::Download,
            targets: vec!["あ".repeat(job_limits::MAX_TARGET_CHARS + 1)],
            options: Vec::new(),
        };
        let reason = validate_request_limits(&multibyte_ng).unwrap();
        assert_eq!(
            reason,
            format!(
                "target too long: {} chars (max {})",
                job_limits::MAX_TARGET_CHARS + 1,
                job_limits::MAX_TARGET_CHARS
            )
        );

        // メッセージは id だけを運ぶ (計画は台帳側)。
        let envelope = WorkerJobEnvelope::v2(JobId("j1".into()));
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

        let page = service().plan_auto_update_page(&[NovelId(9)], &[], 3);
        assert!(page.done);
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.plans.len(), 1);

        let page = service().plan_auto_update_page(&[], &[], 3);
        assert!(page.done);
        assert_eq!(page.next_cursor, None);
        assert!(page.plans.is_empty());
    }

    #[test]
    fn bounded_auto_update_page_walks_300_novels_without_resetting_cursor() {
        let ids: Vec<_> = (1..=300).map(NovelId).collect();
        let mut after = 0usize;
        let mut seen = Vec::new();

        for expected_cursor in [100usize, 200, 300] {
            let start = after;
            let end = start + 100;
            let page = service().plan_auto_update_page(
                &ids[start..end],
                &[],
                100,
            );
            assert!(!page.done);
            assert_eq!(page.next_cursor, Some(expected_cursor as i64));
            seen.extend(page.plans.into_iter().map(|job| match job.target {
                JobTarget::Id(id) => id.0,
                _ => panic!("scheduler emitted a non-novel target"),
            }));
            after = end;
        }

        let final_page = service().plan_auto_update_page(&[], &[], 100);
        assert!(final_page.done);
        assert_eq!(final_page.next_cursor, None);
        assert_eq!(seen, ids.iter().map(|id| id.0).collect::<Vec<_>>());
    }

    #[test]
    fn extract_novel_ids_matches_native_token_rule() {
        // Same rule the native queue applies (`crate::queue::extract_novel_ids`
        // delegates here): only bare i64 tokens count.
        assert!(extract_novel_ids("").is_empty());
        assert!(extract_novel_ids("tag:modified").is_empty());
        assert!(extract_novel_ids("*").is_empty());
        assert!(extract_novel_ids("n1234ab").is_empty());
        assert!(extract_novel_ids("https://example.com/n/1").is_empty());
        assert_eq!(extract_novel_ids("12"), HashSet::from([12]));
        assert_eq!(extract_novel_ids("12\tkindle"), HashSet::from([12]));
        assert_eq!(extract_novel_ids("1\t2\t3"), HashSet::from([1, 2, 3]));
        assert_eq!(extract_novel_ids("--force\t12\ttag:modified"), HashSet::from([12]));
        assert_eq!(extract_novel_ids("12\t\t34\tabc\t1.5"), HashSet::from([12, 34]));
        assert_eq!(extract_novel_ids("-7"), HashSet::from([-7]));
    }

    /// The ledger claim statements run verbatim against rusqlite: two
    /// envelopes for the same novel must never both become `running`, a
    /// `retryable` row inside its backoff hold is not claimable, and the
    /// exclusion never blocks unrelated novels or non-novel targets.
    #[cfg(feature = "native-runtime")]
    mod claim_sql {
        use super::super::worker_ledger::{claim_fresh, claim_reclaim};
        use rusqlite::Connection;

        fn db() -> Connection {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE worker_jobs (
                    job_id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    target TEXT NOT NULL,
                    options TEXT NOT NULL DEFAULT '[]',
                    status TEXT NOT NULL DEFAULT 'pending',
                    dedupe_key TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    lease_until TEXT,
                    execution_token TEXT,
                    checkpoint_json TEXT
                ) STRICT;",
            )
            .unwrap();
            conn
        }

        fn insert(conn: &Connection, job_id: &str, target: &str, status: &str, lease: Option<&str>) {
            conn.execute(
                "INSERT INTO worker_jobs (job_id, kind, target, status, dedupe_key, created_at, updated_at, lease_until)
                 VALUES (?, 'update', ?, ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', ?)",
                rusqlite::params![job_id, target, status, format!("{status}:{job_id}"), lease],
            )
            .unwrap();
        }

        fn status_of(conn: &Connection, job_id: &str) -> String {
            conn.query_row(
                "SELECT status FROM worker_jobs WHERE job_id = ?",
                [job_id],
                |row| row.get(0),
            )
            .unwrap()
        }

        const NOW: &str = "2026-01-01T00:10:00Z";
        const LEASE: &str = "2026-01-01T00:25:00Z";

        #[test]
        fn same_novel_cannot_run_twice_across_kinds() {
            let conn = db();
            // A running update for novel 42 (as produced by `update:42:`).
            insert(&conn, "j-upd", "42", "running", Some(LEASE));
            // A pending convert (`convert:42:`) — a different dedupe key and
            // kind, but the same novel.
            insert(&conn, "j-conv", "42", "pending", None);
            // An unrelated pending update for novel 99.
            insert(&conn, "j-99", "99", "pending", None);

            // The conflicting claim must not flip j-conv to running.
            let stmt = claim_fresh(NOW, LEASE, "tok-b", "j-conv", "42");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 0, "conflicting claim must not run");
            assert_eq!(status_of(&conn, "j-conv"), "pending");

            // The unrelated novel claims normally.
            let stmt = claim_fresh(NOW, LEASE, "tok-c", "j-99", "99");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 1);
            assert_eq!(status_of(&conn, "j-99"), "running");
        }

        #[test]
        fn conflicting_reclaim_is_blocked_and_self_row_is_not_a_conflict() {
            let conn = db();
            // j-stale is running with an expired lease for novel 7.
            insert(&conn, "j-stale", "7", "running", Some("2026-01-01T00:01:00Z"));
            // Another running job holds novel 7? Impossible in practice —
            // the exclusion made it so — but a *different* novel running
            // must not block the reclaim.
            insert(&conn, "j-other", "8", "running", Some(LEASE));

            // Reclaiming j-stale must succeed (its own running row is not a
            // self-conflict).
            let stmt = claim_reclaim(NOW, LEASE, "tok-r", "j-stale", "7");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 1, "expired lease is reclaimable");

            // But a pending job for novel 8 is blocked by j-other.
            insert(&conn, "j-blocked", "8", "pending", None);
            let stmt = claim_fresh(NOW, LEASE, "tok-x", "j-blocked", "8");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 0);
        }

        #[test]
        fn backoff_hold_keeps_retryable_unclaimable_until_due() {
            let conn = db();
            // record_attempt wrote lease_until = now + backoff.
            insert(&conn, "j-retry", "42", "retryable", Some("2026-01-01T00:11:00Z"));

            // Before the hold elapses the row is not claimable even though
            // no conflicting job runs.
            let stmt = claim_fresh(NOW, LEASE, "tok-e", "j-retry", "42");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 0, "backoff hold must block the claim");
            assert_eq!(status_of(&conn, "j-retry"), "retryable");

            // At/after the hold the same row claims normally.
            let stmt = claim_fresh("2026-01-01T00:11:00Z", LEASE, "tok-f", "j-retry", "42");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 1);

            // Legacy retryable rows without a hold remain claimable.
            insert(&conn, "j-legacy", "55", "retryable", None);
            let stmt = claim_fresh(NOW, LEASE, "tok-g", "j-legacy", "55");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 1);
        }

        #[test]
        fn non_novel_targets_never_conflict() {
            let conn = db();
            insert(&conn, "j-nc", "n1234ab", "running", Some(LEASE));
            insert(&conn, "j-nc2", "n1234ab", "pending", None);
            // ncode targets carry no numeric id: native parity says no lock.
            let stmt = claim_fresh(NOW, LEASE, "tok-h", "j-nc2", "n1234ab");
            let changed = conn
                .execute(&stmt.sql, rusqlite::params_from_iter(stmt.binds.iter()))
                .unwrap();
            assert_eq!(changed, 1);
        }
    }
}
