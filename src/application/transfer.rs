//! Client-driven novel transfer sessions (`/api/transfer/v2`).
//!
//! This module is the HTTP-neutral engine and port layer shared by the native
//! axum adapter (`crate::web::transfer` + `crate::native::transfer_v2`) and the
//! Cloudflare Workers adapter (`worker_entry::transfer`). It owns:
//!
//! * the v2 session state machines (import and export),
//! * idempotent record/chunk/operation receipts,
//! * bounded `advance` work units for manifest planning, snapshot copying and
//!   per-novel publication,
//! * the publication ordering (gate → drain → prepare → install → commit,
//!   or rollback) driven through backend primitives.
//!
//! Everything durable lives behind two ports so the semantics stay identical
//! across backends:
//!
//! * [`TransferJournal`] — small durable rows (sessions, files, chunk
//!   receipts, operations, idempotent create records, publications, journal
//!   blobs). Rows are individual records; no `app_state` JSON blob.
//! * [`TransferBackend`] — library-facing work (capabilities, URL index,
//!   stage read/write, source reads, snapshot pins, publication primitives,
//!   reservations, GC).
//!
//! Wire-level concerns (NDJSON record schema, batch frame codec, route names,
//! JSON field names) belong to `crate::transfer::protocol` and the web
//! adapters, not to this module.
//!
//! # `TransferService` API surface
//!
//! All session methods take `scope` + `session_id` + `secret` and authorize
//! before touching state. Control-plane mutations take
//! [`ControlOp`] (`operation_id` + `If-Match` revision); binary record
//! appends and `advance` are intrinsically idempotent.
//!
//! ```text
//! capabilities(scope) -> Capabilities
//! create_import(scope, &CreateImportRequest, secret) -> CreateResult
//! create_export(scope, &CreateExportRequest, secret) -> CreateResult
//! session(scope, session_id, secret) -> SessionStatus
//! list_sessions(scope, cursor) -> SessionPage
//! delete_session(scope, session_id, Option<secret>, ControlOp) -> SessionStatus
//! put_manifest(scope, session_id, secret, Vec<BatchRecord>) -> BatchResult
//! seal_manifest(scope, session_id, secret, ControlOp, size, sha256) -> SessionStatus
//! files(scope, session_id, secret, cursor) -> FilePage
//! plan(scope, session_id, secret, cursor) -> PlanPage
//! put_records(scope, session_id, secret, Vec<BatchRecord>) -> BatchResult
//! select(scope, session_id, secret, ControlOp, SelectionRequest) -> SessionStatus
//! catalog(scope, session_id, secret, cursor) -> CatalogPage
//! export_manifest(scope, session_id, secret, index) -> BinaryChunk
//! export_batch(scope, session_id, secret, index) -> BatchRead
//! pause(scope, session_id, secret, ControlOp) -> SessionStatus
//! resume(scope, session_id, secret, ControlOp) -> SessionStatus
//! complete(scope, session_id, secret, ControlOp) -> SessionStatus
//! advance(scope, session_id, secret, operation_id) -> AdvanceResult
//! recover(scope) -> RecoveryReport          // startup drain
//! expire_idle(scope) -> u32                 // idle-session GC tick
// Re-exported so adapters share one role vocabulary with the portable core.
pub use crate::transfer::projection::{ClassifyContext, FileRole, TransferProfile};
pub use crate::transfer::protocol::NovelKind;
pub use crate::transfer::ConflictPolicy;
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::platform::clock::Clock;
use crate::platform::{NovelId, PlatformFuture, PlatformService};

/// `Option<u64>` as a decimal string (mirrors `protocol::decimal_u64`).
pub mod decimal_u64_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => v.to_string().serialize(s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        let v = Option::<serde_json::Value>::deserialize(d)?;
        match v {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => s
                .parse::<u64>()
                .map(Some)
                .map_err(serde::de::Error::custom),
            Some(serde_json::Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
                serde::de::Error::custom("expected unsigned integer")
            }),
            Some(_) => Err(serde::de::Error::custom("expected decimal string")),
        }
    }
}


// ---------------------------------------------------------------------------
// Constants (docs/client-transfer-design.md §5, §6.6)
// ---------------------------------------------------------------------------

/// HTTP `protocol_version` for `/api/transfer/v2`. Distinct from the archive
/// `format_version` (which stays 2 and can still read version 1 archives).
pub const PROTOCOL_VERSION: u32 = crate::transfer::protocol::PROTOCOL_VERSION;
/// Maximum HTTP binary body including framing.
pub const MAX_BINARY_BODY_BYTES: usize =
    crate::transfer::protocol::BINARY_BODY_MAX_BYTES as usize;
/// Maximum control JSON body.
pub const MAX_CONTROL_BODY_BYTES: usize =
    crate::transfer::protocol::CONTROL_BODY_MAX_BYTES as usize;
/// Maximum records per binary batch.
pub const MAX_BATCH_RECORDS: usize = crate::transfer::protocol::BATCH_MAX_RECORDS;
/// Maximum framed record header length.
pub const MAX_RECORD_HEADER_BYTES: usize =
    crate::transfer::protocol::FRAME_HEADER_MAX_BYTES;
/// Fixed per-file chunk size negotiated at session creation
/// (`8 MiB - 64 KiB`); the last chunk of a file is shorter.
pub const CHUNK_BYTES: u64 = crate::transfer::protocol::CHUNK_BYTES;
/// Maximum serialized wire manifest (NDJSON) size.
pub const MAX_WIRE_MANIFEST_BYTES: u64 =
    crate::transfer::protocol::WIRE_MANIFEST_MAX_BYTES;
/// Retained working-metadata budget for a session's manifest/catalog.
pub const MAX_METADATA_BYTES: usize = 24 * 1024 * 1024;
/// Page limits shared by catalog/files/plan/session pages.
pub const MAX_PAGE_ITEMS: usize = crate::transfer::protocol::PAGE_MAX_ITEMS;
pub const MAX_PAGE_BYTES: usize = crate::transfer::protocol::PAGE_MAX_BYTES;
/// Reserved `file_id` inside binary batches that carries wire-manifest bytes.
/// Real file ids are the manifest's `file` record ordinals starting at 0, so
/// they can never collide with `u32::MAX`.
pub const MANIFEST_FILE_ID: u32 = crate::transfer::protocol::MANIFEST_FILE_ID;
/// Default maximum live sessions per auth scope.
pub const DEFAULT_MAX_SESSIONS_PER_SCOPE: u32 = 2;
/// Idle time-to-live for sessions and their private stages.
pub const SESSION_IDLE_TTL_SECS: i64 = 24 * 3600;
/// Idempotent-create terminal-result retention.
pub const CREATE_RECEIPT_TTL_SECS: i64 = 7 * 24 * 3600;
/// Creation ticket validity.
pub const CREATION_TICKET_TTL_SECS: i64 = 10 * 60;
/// One `advance` invocation's byte budget.
pub const ADVANCE_BYTE_BUDGET: u64 = 8 * 1024 * 1024;
/// One `advance` invocation's record/item budget.
pub const ADVANCE_ITEM_BUDGET: u32 = 100;
/// Display title truncation for plan/catalog pages (characters).
pub const DISPLAY_TITLE_CHARS: usize = 200;
/// SHA-256 hex digest length in bytes (32 raw).
pub const SHA256_LEN: usize = 32;

// ---------------------------------------------------------------------------
// Identity types
// ---------------------------------------------------------------------------

/// Opaque session identifier, server-generated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Opaque operation identifier for idempotent control-plane operations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub String);

/// Session direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Import,
    Export,
}

/// Authentication management unit. Not a cookie or key value: the library id
/// plus the administrative principal under the current single-admin model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuthScope {
    pub library_id: String,
    pub principal: String,
}

/// SHA-256 of the client-generated 256-bit session secret. The raw secret
/// never reaches the server; it travels only as `x-narou-transfer-secret` and
/// is compared here by hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretHash(pub String);

impl SecretHash {
    /// Hash a raw secret (any length; 256 bits expected from the client).
    pub fn of(secret: &[u8]) -> Self {
        Self(sha256_hex(secret))
    }
}

/// SHA-256 helper — hex lowercase.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

/// Decode a lowercase hex SHA-256 into raw bytes.
pub fn sha256_from_hex(hex: &str) -> Option<[u8; SHA256_LEN]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; SHA256_LEN];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors surfaced by the transfer engine. Each variant carries the exact wire
/// `error.code`; the web layer maps codes to HTTP statuses and never invents
/// its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// Malformed request (bad cursor, missing field, oversize control body).
    InvalidRequest { code: &'static str, message: String },
    /// Session does not exist (or belongs to another scope).
    SessionNotFound,
    /// Session expired while idle; `resumable` distinguishes a resumable
    /// stage from a fully reclaimed one.
    SessionExpired { resumable: bool },
    /// Secret hash mismatch.
    SecretMismatch,
    /// `If-Match` revision does not match the current session revision.
    RevisionMismatch { current: u64 },
    /// `If-Match` required but absent.
    PreconditionRequired,
    /// Create `request_id` replayed with a different body or secret.
    IdempotencyConflict,
    /// Same operation_id already in flight with a different request.
    OperationConflict,
    /// Operation not allowed in the current state.
    InvalidState { state: &'static str, expected: &'static str },
    /// Record offset/hash conflict (`chunk_conflict`, `unexpected_offset`).
    ChunkConflict { code: &'static str, message: String },
    /// Plan revision is stale (library changed since planning).
    PlanStale { message: String },
    /// Client-confirmed selection digest does not match the server's
    /// projection of the profile/roles over the catalog.
    SelectionMismatch { server_digest: String },
    /// A resource exceeded a limit; `code` names which one.
    ExceedsLimit { code: &'static str, message: String },
    /// Too many live sessions for the scope.
    TooManySessions { max: u32 },
    /// Destination storage lacks capacity for the reservation.
    InsufficientStorage { message: String },
    /// Backend lacks the capability (e.g. `sqlite.mirror-files=false`
    /// import); `reason` is surfaced verbatim.
    Unsupported { code: &'static str, reason: String },
    /// Publication recovery failed; the novel is preserved and gated.
    RecoveryRequired { message: String },
    /// A novel is fenced/gated by another operation.
    Busy { code: &'static str, message: String },
    /// Transient platform failure (retryable).
    Platform { message: String },
}

impl TransferError {
    /// Stable wire `error.code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest { code, .. } => code,
            Self::SessionNotFound => "session_not_found",
            Self::SessionExpired { .. } => "session_expired",
            Self::SecretMismatch => "secret_mismatch",
            Self::RevisionMismatch { .. } => "revision_mismatch",
            Self::PreconditionRequired => "precondition_required",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::OperationConflict => "operation_conflict",
            Self::InvalidState { .. } => "invalid_state",
            Self::ChunkConflict { code, .. } => code,
            Self::PlanStale { .. } => "plan_stale",
            Self::SelectionMismatch { .. } => "selection_mismatch",
            Self::ExceedsLimit { code, .. } => code,
            Self::TooManySessions { .. } => "too_many_sessions",
            Self::InsufficientStorage { .. } => "insufficient_storage",
            Self::Unsupported { code, .. } => code,
            Self::RecoveryRequired { .. } => "recovery_required",
            Self::Busy { code, .. } => code,
            Self::Platform { .. } => "platform_error",
        }
    }

    /// Whether the client may retry the same identity after backoff.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Platform { .. } | Self::Busy { .. } | Self::RecoveryRequired { .. }
        )
    }

    /// Human-readable message (never carries secrets or raw paths from the
    /// client beyond what the client supplied).
    pub fn message(&self) -> String {
        match self {
            Self::InvalidRequest { message, .. } => message.clone(),
            Self::SessionNotFound => "session does not exist".into(),
            Self::SessionExpired { resumable } => {
                if *resumable {
                    "session expired; staged data may still be recoverable".into()
                } else {
                    "session expired and its staged data was reclaimed".into()
                }
            }
            Self::SecretMismatch => "session secret does not match".into(),
            Self::RevisionMismatch { current } => {
                format!("session revision is now {current}")
            }
            Self::PreconditionRequired => "If-Match revision is required".into(),
            Self::IdempotencyConflict => {
                "request_id was already used with a different request".into()
            }
            Self::OperationConflict => {
                "operation_id was already used with a different request".into()
            }
            Self::InvalidState { state, expected } => {
                format!("operation is not valid while state is {state} (expected {expected})")
            }
            Self::ChunkConflict { message, .. } => message.clone(),
            Self::PlanStale { message } => message.clone(),
            Self::SelectionMismatch { server_digest } => {
                format!("selection digest does not match server projection ({server_digest})")
            }
            Self::ExceedsLimit { message, .. } => message.clone(),
            Self::TooManySessions { max } => {
                format!("too many live transfer sessions (max {max})")
            }
            Self::InsufficientStorage { message } => message.clone(),
            Self::Unsupported { reason, .. } => reason.clone(),
            Self::RecoveryRequired { message } => message.clone(),
            Self::Busy { message, .. } => message.clone(),
            Self::Platform { message } => message.clone(),
        }
    }

    pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidRequest { code, message: message.into() }
    }

    pub(crate) fn platform(error: impl std::fmt::Display) -> Self {
        Self::Platform { message: error.to_string() }
    }
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}
impl std::error::Error for TransferError {}

// ---------------------------------------------------------------------------
// Session model
// ---------------------------------------------------------------------------

/// Unified session state across both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    // import
    ReceivingManifest,
    Planning,
    Transferring,
    Ready,
    Publishing,
    Completed,
    CompletedWithErrors,
    // export
    Cataloguing,
    Selecting,
    Preparing,
    Reading,
    Released,
    // shared overlays
    Paused,
    Cancelling,
    Cancelled,
    Failed,
    Expired,
}

impl SessionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::CompletedWithErrors
                | Self::Released
                | Self::Cancelled
                | Self::Failed
                | Self::Expired
        )
    }

    /// Wire name (`receiving_manifest`, `completed_with_errors`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReceivingManifest => "receiving_manifest",
            Self::Planning => "planning",
            Self::Transferring => "transferring",
            Self::Ready => "ready",
            Self::Publishing => "publishing",
            Self::Completed => "completed",
            Self::CompletedWithErrors => "completed_with_errors",
            Self::Cataloguing => "cataloguing",
            Self::Selecting => "selecting",
            Self::Preparing => "preparing",
            Self::Reading => "reading",
            Self::Released => "released",
            Self::Paused => "paused",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }
}

/// What the client should do next; also the unit `advance` executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    /// Client must send more binary records (manifest or file chunks).
    Upload,
    /// Client must call `advance` again (bounded server work remains).
    Advance,
    /// A sealed file needs external-store finalization via `advance`.
    FinalizeFile,
    /// Export: user must confirm the catalog via `select`.
    Select,
    /// Import: user must review the plan and `publish` (or the export is
    /// ready to read: paired with `Reading` state the action is `read`).
    Confirm,
    /// Export: snapshot is ready; client reads batches/chunks.
    Read,
    /// Export: client must report `complete` after closing the ZIP.
    Complete,
    /// Nothing further (terminal or waiting on a human for an unspecified
    /// reason).
    None,
}

impl NextAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Advance => "advance",
            Self::FinalizeFile => "finalize_file",
            Self::Select => "select",
            Self::Confirm => "confirm",
            Self::Read => "read",
            Self::Complete => "complete",
            Self::None => "none",
        }
    }
}

/// How a session can be resumed after interruption, derived from retained
/// resources — never a client claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
    /// Re-verify source fingerprint / snapshot pin and continue.
    Continue,
    /// Import: re-select the same ZIP locally (fingerprint check) first.
    ReselectSource,
    /// Export: the previous output file cannot be appended; generate a new
    /// ZIP from the same snapshot.
    RestartOutput,
    /// A publication gate/journal must be drained or recovered first.
    RecoverFirst,
    /// Not resumable.
    None,
}

/// Per-file receipt state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// Metadata registered; nothing received/staged yet.
    Pending,
    /// Some chunks confirmed.
    Receiving,
    /// All payload receipts are durable; external seal completion is pending.
    Finalizing,
    /// Whole file verified into immutable stage (`final` record accepted).
    Verified,
    /// Bound to a pre-verified server-side pin (no upload needed).
    Reused,
    /// Excluded by projection/policy — never receives bytes.
    Skipped,
    /// Failed validation; needs resend from offset 0 of the file.
    Failed,
    /// Export: source is snapshot-copied and batch-servable.
    Staged,
    /// Export: source content was pinned in place (verified immutable).
    Pinned,
}

/// Per-novel publication state (import direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Pending,
    /// Gate closed; existing readers draining.
    Draining,
    /// Pre-image/journal recorded; reservations held.
    Prepared,
    /// File installation in progress (bounded).
    Installing,
    /// Commit boundary in progress.
    Committing,
    Committed,
    /// Failed before commit; pre-image restore in progress.
    RollingBack,
    /// Recovered to old content.
    RolledBack,
    /// Recovery failed; gate kept, manual/again recovery required.
    RecoveryRequired,
    /// Skipped by plan (policy Skip or dependency failure).
    Skipped,
}

// ---------------------------------------------------------------------------
// Durable rows (TransferJournal payloads)
// ---------------------------------------------------------------------------

/// Direction-specific session extension data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionExt {
    Import(ImportSessionExt),
    Export(ExportSessionExt),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSessionExt {
    /// Client-computed fingerprint of the fully validated source ZIP.
    pub source_fingerprint: String,
    /// Declared size/SHA of the wire manifest (checked at seal).
    pub manifest_size: u64,
    pub manifest_sha256: String,
    /// StageRef of the sealed manifest staging object.
    pub manifest_stage: Option<String>,
    /// Sealed manifest byte length (0 until sealed).
    pub manifest_confirmed: u64,
    /// Durable NDJSON parse cursor (byte offset into the sealed manifest).
    pub manifest_cursor: u64,
    /// Number of `file` records catalogued so far.
    pub catalogued_files: u64,
    /// Number of `novel` records catalogued so far.
    pub catalogued_novels: u32,
    /// Bounded-pass marker for post-parse graph validation.
    pub graph_cursor: Option<String>,
    #[serde(default)]
    pub planning_phase: u8,
    #[serde(default)]
    pub planning_ordinal: u32,
    /// Publication pass cursor (next publication row ordinal).
    pub publish_cursor: u32,
    #[serde(default)]
    pub publication_pass:u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSessionExt {
    /// Requested novel ids (server resolves virtual members).
    pub requested_ids: Vec<i64>,
    /// Fixed once `select` succeeds.
    pub selection_digest: Option<String>,
    /// Number of catalog rows written.
    pub catalogued: u64,
    /// Snapshot copy pass cursor.
    pub snapshot_cursor: Option<String>,
    #[serde(default)]
    pub snapshot_done: bool,
    /// Batch boundary table version.
    pub batch_plan: Option<String>,
    #[serde(default)]
    pub catalog_cursor: Option<String>,
    #[serde(default)]
    pub selection_cursor: Option<String>,
    #[serde(default)]
    pub selection_hash: Option<Vec<u8>>,
    #[serde(default)]
    pub prepare_phase: u8,
    #[serde(default)]
    pub manifest_offset: u64,
    /// Two alternating tails keep the active replay input immutable until CAS.
    #[serde(default)]
    pub manifest_pending_slot: u8,
    #[serde(default)]
    pub batch_file_cursor: Option<String>,
    #[serde(default)]
    pub batch_chunk: u32,
    #[serde(default)]
    pub batch_count: u32,
    #[serde(default)]
    pub selected_files_count: u32,
    #[serde(default)]
    pub metadata_cursor: Option<String>,
    #[serde(default)]
    pub metadata_bytes: u64,
    #[serde(default)]
    pub manifest_build_cursor: Option<String>,
}

/// Durable session row. `confirmed_bytes` is maintained by `commit_chunk` so
/// status reads never scan chunk rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub session_id: SessionId,
    pub scope: AuthScope,
    pub secret_hash: SecretHash,
    pub direction: TransferDirection,
    pub state: SessionState,
    /// State before `paused`/`cancelling` overlaid (`resume_phase`).
    pub resume_phase: Option<SessionState>,
    pub revision: u64,
    pub conflict_policy: ConflictPolicy,
    pub profile: TransferProfile,
    pub extra_roles: Vec<String>,
    pub policy_version: u32,
    /// `planning` output revision bound into `publish`.
    pub plan_revision: Option<String>,
    /// Export catalog revision (bumps only while cataloguing).
    pub catalog_revision: u64,
    pub chunk_bytes: u64,
    pub confirmed_bytes: u64,
    #[serde(default)]
    pub published_novels: u32,
    pub next_action: NextAction,
    /// Current (or last completed) operation id for `advance`-driven work.
    pub operation_id: Option<OperationId>,
    #[serde(default)]
    pub last_operation_id: Option<OperationId>,
    #[serde(default)]
    pub last_operation_result: Option<Vec<u8>>,
    /// Bounded cursor for post-terminal resource release.
    #[serde(default)]
    pub gc_cursor: Option<String>,
    /// Last terminal error summary for `failed` (code + message).
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ext: SessionExt,
}

/// Durable per-file row. `file_id` equals `manifest_file_index` (the wire
/// manifest `file` record ordinal) for imports and the frozen export file
/// ordinal for exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRow {
    pub session_id: SessionId,
    pub file_id: u32,
    pub novel_ordinal: u32,
    pub logical_path: String,
    pub role: FileRole,
    /// Expected/actual whole-file size and SHA-256 (hex).
    pub size: u64,
    pub sha256: String,
    pub state: FileState,
    /// Bytes durably confirmed at the stage (import) or copied (export).
    pub confirmed_bytes: u64,
    /// Next expected `chunk_index`.
    pub next_chunk_index: u32,
    /// SHA-256 (hex) of the confirmed prefix, for client resume checks.
    pub confirmed_prefix_sha256: Option<String>,
    /// Serialized hash checkpoint (algorithm-tagged; internal only).
    pub hash_checkpoint: Option<Vec<u8>>,
    /// Private stage handle once bytes exist.
    pub stage: Option<String>,
    /// Reuse pin handle when `state == Reused`/`Pinned`.
    pub pin: Option<String>,
    /// Intrinsic operation identity for an external finalization retry.
    pub finalize_operation: Option<OperationId>,
}

/// Idempotent `(session, file, chunk)` receipt row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRow {
    pub session_id: SessionId,
    pub file_id: u32,
    pub chunk_index: u32,
    /// Payload size in bytes.
    pub payload_size: u64,
    /// Client-declared record SHA-256 (import uploads; hex).
    pub declared_sha256: Option<String>,
    /// Server-computed SHA-256 of the stored payload (hex).
    pub stored_sha256: String,
    /// Running confirmed prefix SHA-256 after this chunk (hex).
    pub prefix_sha256: String,
    /// Running hash checkpoint after this chunk.
    pub hash_checkpoint: Option<Vec<u8>>,
    /// Byte offset of this chunk within the file.
    pub offset: u64,
    pub is_final: bool,
}

/// Idempotent control-plane operation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRow {
    pub session_id: SessionId,
    pub operation_id: OperationId,
    /// Operation kind (`select`, `seal`, `ready`, `publish`, `pause`,
    /// `resume`, `delete`, `complete`, `advance:<label>`).
    pub kind: String,
    /// Fingerprint of the operation request (params + bound revision).
    pub request_fingerprint: String,
    /// Session revision the operation was issued against.
    pub if_match_revision: Option<u64>,
    /// Serialized result once finished (service-defined payload).
    pub result: Option<Vec<u8>>,
    pub completed: bool,
    pub created_at: DateTime<Utc>,
}

/// `request_id` → created session receipt (7-day retention).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRow {
    pub request_id: String,
    pub scope: AuthScope,
    /// Hash of (direction + canonical request body + secret hash).
    pub request_fingerprint: String,
    /// Set once the session row exists; remains for replay recovery.
    pub session_id: Option<SessionId>,
    /// Serialized `CreateResult` for terminal replays.
    pub receipt: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
}

/// Publication row — one per novel ordinal in an import plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationRow {
    pub session_id: SessionId,
    /// Plan novel ordinal (stable across the session).
    pub publication_id: u32,
    pub state: PublicationState,
    /// Serialized `PublicationSpec` snapshot used by recovery.
    pub spec: Vec<u8>,
    /// Bounded-phase cursor (adapter-defined resume token).
    pub phase_cursor: Option<String>,
    /// `NovelId` allocated/assigned at commit, for receipts.
    pub assigned_id: Option<i64>,
    /// Serialized per-novel receipt (`PublicationReceipt`).
    pub receipt: Option<Vec<u8>>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Engine-level request/response types (HTTP-neutral)
// ---------------------------------------------------------------------------

/// `If-Match` + persistent `operation_id` bound to control-plane mutations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlOp {
    pub operation_id: OperationId,
    /// Session revision from `If-Match` (already parsed to u64).
    pub if_match_revision: u64,
}

/// Import session creation request (POST /imports body fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateImportRequest {
    pub request_id: String,
    pub creation_ticket: String,
    /// Fingerprint over the locally validated archive (order-stable).
    pub source_fingerprint: String,
    pub profile: TransferProfile,
    /// Additional roles the client opted into (`raw`, `history`, ...).
    pub extra_roles: Vec<String>,
    pub policy_version: u32,
    pub conflict_policy: ConflictPolicy,
    /// Declared wire-manifest size and SHA-256 (hex).
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub manifest_size: u64,
    pub manifest_sha256: String,
}

/// Export session creation request (POST /exports body fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateExportRequest {
    pub request_id: String,
    pub creation_ticket: String,
    /// Library novel ids to export (parents expand members server-side).
    pub ids: Vec<i64>,
    pub profile: TransferProfile,
    /// Additional roles opted in beyond the profile defaults.
    pub extra_roles: Vec<String>,
}

/// One parsed binary batch record (framing decoded by the web adapter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRecord {
    pub file_id: u32,
    pub chunk_index: u32,
    /// Raw uncompressed payload (≤ `chunk_bytes`).
    pub payload: Vec<u8>,
    /// Declared payload SHA-256 (required on import uploads).
    pub sha256: Option<[u8; SHA256_LEN]>,
    /// Final record of the file (folds the file seal).
    pub is_final: bool,
}

/// Outcome for one record inside `BatchResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkOutcome {
    pub file_id: u32,
    pub chunk_index: u32,
    /// `accepted` | `duplicate` (identical retry) | `sealed`.
    pub outcome: String,
    /// Confirmed file offset after this record.
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub confirmed_bytes: u64,
    /// Running prefix SHA-256 (hex) after this record.
    pub confirmed_prefix_sha256: String,
}

/// A record that could not be committed; batch processing stops after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedRecord {
    pub file_id: u32,
    pub chunk_index: u32,
    pub code: String,
    pub message: String,
}

/// Result of one binary batch (`put_records` / `put_manifest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchResult {
    /// Receipts for every committed record, in request order.
    pub receipts: Vec<ChunkOutcome>,
    /// First failed record, if any. Receipts before it remain committed.
    pub failure: Option<FailedRecord>,
    /// Updated session status after the batch.
    #[serde(flatten)]
    pub status: SessionStatus,
}

/// Wire-visible session status (maps 1:1 onto the frozen JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub direction: TransferDirection,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub revision: u64,
    pub state: SessionState,
    /// Pre-pause/pre-cancel state for `resume`.
    pub resume_phase: Option<SessionState>,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub confirmed_bytes: u64,
    #[serde(default)]
    pub published_novels: u32,
    pub next_action: NextAction,
    pub resume_mode: ResumeMode,
    /// The operation id the client must supply for the next `advance` call
    /// (rotates per bounded step); `None` when no advance is pending.
    pub operation_id: Option<OperationId>,
    pub expires_at: DateTime<Utc>,
    /// Negotiated per-file chunk size (fixed at creation).
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub chunk_bytes: u64,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub catalog_revision: u64,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub manifest_confirmed_bytes: u64,
    pub manifest_next_chunk_index: u32,
    /// Bound plan revision (imports, once planning completes).
    pub plan_revision: Option<String>,
    /// Export selection digest once `select` succeeded.
    pub selection_digest: Option<String>,
    /// Session error code+message when `failed`.
    pub error: Option<String>,
}

/// Create response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResult {
    #[serde(flatten)]
    pub status: SessionStatus,
    /// `true` when this is a replayed receipt of an existing session.
    pub replayed: bool,
}

/// `advance` response — status plus the work this invocation performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvanceResult {
    #[serde(flatten)]
    pub status: SessionStatus,
    /// Whether the bounded unit completed (or more advance calls remain).
    pub step_done: bool,
    pub processed_records: u32,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub processed_bytes: u64,
}

/// Export `select` request: the client's confirmation of the projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRequest {
    pub profile: TransferProfile,
    pub extra_roles: Vec<String>,
    /// Digest over (catalog revision, profile, roles, selected file set) that
    /// the client computed and confirmed. Compared to the server's own
    /// projection — a mismatch means the catalog changed underneath.
    pub selection_digest: String,
}

/// One row of a files page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    pub file_id: u32,
    /// Same value as `file_id` (frozen wire alias).
    pub manifest_file_index: u32,
    pub novel_ordinal: u32,
    pub logical_path: String,
    pub role: FileRole,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub size: u64,
    pub sha256: String,
    pub state: FileState,
    /// `true` when the client must upload bytes (not reuse/skip).
    pub upload_required: bool,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub confirmed_bytes: u64,
    pub next_chunk_index: u32,
    pub confirmed_prefix_sha256: Option<String>,
}

/// One row of a plan page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub novel_ordinal: u32,
    pub toc_url: String,
    /// Display title (already truncated for the page).
    pub title: String,
    pub kind: NovelKind,
    /// `create` | `overwrite` | `skip` | `reject`.
    pub action: PlanAction,
    /// Machine-oriented reason (`policy_skip`, `missing_member`, ...).
    pub reason: Option<String>,
    /// Planning conditions for conditional publish.
    pub existing_id: Option<i64>,
    pub record_revision: Option<String>,
    pub content_revision: Option<String>,
    /// Parent membership digest (virtual parents).
    pub membership_digest: Option<String>,
    pub file_count: u32,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub total_bytes: u64,
    #[serde(default)]
    pub receipt: Option<PublicationReceipt>,
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Create,
    Overwrite,
    Skip,
    /// Cannot be published (missing member, unsupported required data, ...).
    Reject,
}

/// One row of an export catalog page (per file, pre-snapshot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub novel_ordinal: u32,
    pub existing_id: i64,
    pub toc_url: String,
    pub title: String,
    pub kind: NovelKind,
    pub logical_path: String,
    pub role: FileRole,
    #[serde(with = "crate::transfer::protocol::decimal_u64")]
    pub size: u64,
    /// Whether policy allows this role at all.
    pub selectable: bool,
    /// Whether current profile/roles select it.
    pub selected: bool,
    /// Required dependency (body/toc/settings/index) — cannot be deselected.
    pub required: bool,
    /// Why the file is excluded, when excluded.
    pub exclusion_reason: Option<String>,
    /// Storage capability gate (`policy_version` applies to this row).
    pub policy_version: u32,
    pub runtime: CatalogDecision,
    pub complete: CatalogDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogDecision {
    pub send: String,
    pub default: bool,
    pub reason: Option<String>,
}

/// Opaque-cursor page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque cursor for the next page; `None` at the end.
    pub next_cursor: Option<String>,
}

pub type FilePage = Page<FileStatus>;
pub type SessionPage = Page<SessionStatus>;
pub type CatalogPage = Page<CatalogItem>;

/// Plan page carries the revision the entries were planned under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPage {
    pub plan_revision: String,
    pub items: Vec<PlanEntry>,
    pub next_cursor: Option<String>,
}

/// A bounded binary read (`export_manifest`, `export_file_chunk`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryChunk {
    /// Raw bytes (≤ 8 MiB for manifest chunks, ≤ chunk_bytes for files).
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub last: bool,
    /// SHA-256 of `bytes` (hex) — exposed as the ETag by the web layer.
    pub sha256: String,
}

/// A bounded export batch (`export_batch`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRead {
    /// Batch ordinal in the frozen batch plan.
    pub index: u32,
    /// Framed records (`[u32be header_len][json header][payload]` * n).
    pub body: Vec<u8>,
    pub record_count: u32,
    /// `body.len()` — echoed by the web layer as `x-transfer-body-bytes`.
    pub body_bytes: u64,
    /// Whether this is the last batch of the frozen plan.
    pub last: bool,
}

/// `capabilities` payload (service fills protocol/limits; backend fills
/// storage/build; the service binds the ticket).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol_version: u32,
    /// Wire/codec contract version.
    pub codec_version: u32,
    /// Actual compiled transfer WASM artifact, when this build ships one.
    pub build: TransferBuildInfo,
    pub limits: TransferLimits,
    pub storage: StorageCapabilities,
    /// Signed, ~10-minute creation ticket bound to scope + protocol.
    pub creation_ticket: String,
    pub server_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferBuildInfo {
    /// WASM build id when the artifact is actually embedded/served.
    pub wasm_build_id: Option<String>,
    /// Binding ABI version paired with `wasm_build_id`.
    pub abi_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferLimits {
    pub binary_body_bytes: u64,
    pub control_body_bytes: u64,
    pub page_items: u32,
    pub page_bytes: u64,
    pub codec_io_bytes: u64,
    pub chunk_bytes: u64,
    pub batch_records: u32,
    pub record_header_bytes: u32,
    pub manifest_bytes: u64,
    pub manifest_metadata_bytes: u64,
    pub max_entries: u32,
    pub max_novels: u32,
    pub max_sessions_per_scope: u32,
    pub session_idle_ttl_secs: u64,
    pub creation_ticket_ttl_secs: u64,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            binary_body_bytes: MAX_BINARY_BODY_BYTES as u64,
            control_body_bytes: MAX_CONTROL_BODY_BYTES as u64,
            page_items: MAX_PAGE_ITEMS as u32,
            page_bytes: MAX_PAGE_BYTES as u64,
            codec_io_bytes: 64 * 1024,
            chunk_bytes: CHUNK_BYTES,
            batch_records: MAX_BATCH_RECORDS as u32,
            record_header_bytes: MAX_RECORD_HEADER_BYTES as u32,
            manifest_bytes: MAX_WIRE_MANIFEST_BYTES,
            manifest_metadata_bytes: MAX_METADATA_BYTES as u64,
            max_entries: 50_000,
            max_novels: 10_000,
            max_sessions_per_scope: DEFAULT_MAX_SESSIONS_PER_SCOPE,
            session_idle_ttl_secs: SESSION_IDLE_TTL_SECS as u64,
            creation_ticket_ttl_secs: CREATION_TICKET_TTL_SECS as u64,
        }
    }
}

/// What the destination storage can persist — reported by the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCapabilities {
    /// Stable file-policy version; bumped when role rules change.
    pub policy_version: u32,
    /// Storage method names (`yaml`, `sqlite`, `d1`, `s3`, ...).
    pub methods: Vec<String>,
    pub import: CapabilityGate,
    pub export: CapabilityGate,
    /// Roles this destination cannot newly persist (`raw` on Worker).
    pub unpersistable_roles: Vec<FileRole>,
    /// Whether pre-verified reuse of existing stored bytes is supported.
    pub supports_reuse: bool,
    /// Free/remaining capacity hint for reservations (`None` = unknown).
    #[serde(with = "crate::application::transfer::decimal_u64_opt", default)]
    pub available_bytes: Option<u64>,
    #[serde(default)]
    pub backend_limits: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityGate {
    pub allowed: bool,
    pub reason: Option<String>,
}

/// Server-side lookup result for one `toc_url` during planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryMatch {
    /// Existing record matched by `toc_url`.
    pub id: NovelId,
    pub title: String,
    pub kind: NovelKind,
    /// Conditional-publish conditions.
    pub record_revision: String,
    pub content_revision: String,
    /// For virtual parents: digest of the ordered member list.
    pub membership_digest: Option<String>,
}

/// A file the backend lists for one novel during export cataloguing.
/// The service classifies `FileRole` itself — backends report what they
/// know (`known_role`) and leave `None` for the shared classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFileEntry {
    /// Logical archive path relative to the novel (`本文/x.yaml`, `raw/…`).
    pub logical_path: String,
    pub size: u64,
    /// Backend-known role hint; the shared classifier may still override.
    pub known_role: Option<FileRole>,
    /// How the bytes can be obtained for snapshotting.
    pub source: SourceRef,
}

/// How a source file's bytes are obtained for export snapshotting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceRef {
    /// Range-readable under the exclusive file-read port.
    Readable,
    /// Already-verified immutable content the backend can pin in place.
    /// The pin keeps a content reference alive for the session.
    Pinnable,
    /// Not readable (missing/corrupt); catalogued as an error row.
    Unavailable(String),
}

/// A reusable pre-verified byte reference (import reuse or export pin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinRef {
    /// Session-scoped pin handle the backend maintains until released.
    /// Usable wherever a stage handle is accepted (`stage_read_range`,
    /// `stage_abort`, `release_pin`).
    pub pin: String,
    /// Verified size and SHA-256 (hex) of the pinned content.
    pub size: u64,
    pub sha256: String,
}

/// Publication target resolved from the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationTarget {
    /// `Create` → reservation id is provided by the backend at prepare time.
    pub action: PlanAction,
    /// Existing record for overwrite, when planned.
    pub existing_id: Option<i64>,
    /// Id reserved during `publication_prepare` for creates.
    pub reserved_id: Option<i64>,
    /// Conditional-publish expectations captured at planning.
    pub record_revision: Option<String>,
    pub content_revision: Option<String>,
    pub membership_digest: Option<String>,
}

/// One staged file belonging to a publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationFile {
    /// Novel-relative logical path (`本文/x.yaml`, `toc.yaml`, `setting.ini`).
    pub logical_path: String,
    pub size: u64,
    pub sha256: String,
    /// Immutable stage handle or reuse pin the backend installs from.
    pub stage: Option<String>,
    pub pin: Option<String>,
}

/// Publication work item passed to backend primitives on every call; the
/// backend's own journal stays authoritative for crash recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationSpec {
    pub session_id: SessionId,
    /// Immutable confirmed plan identity, also bound to the backend gate owner.
    pub plan_revision: String,
    /// Stable plan ordinal; also the publication id.
    pub publication_id: u32,
    pub toc_url: String,
    pub title: String,
    pub kind: NovelKind,
    /// Record fields for gate close and commit. Other phases use the
    /// persisted publication identity, even if staged metadata is unavailable.
    pub metadata: crate::transfer::ArchiveNovel,
    /// File IDs are dense within each novel in the sealed wire manifest.
    /// Backends fetch only their current bounded file window by direct ID.
    pub file_id_start: u32,
    pub file_count: u32,
    pub member_count: u32,
    /// Member destinations resolved to ids at publish time.
    pub member_ids: Vec<i64>,
    pub target: PublicationTarget,
}

/// Outcome of one bounded publication primitive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicationStep {
    /// Phase finished; `false` means call the same primitive again.
    pub done: bool,
    /// True only when the backend's authoritative commit marker exists.
    /// Recovery can discover this even if the publication row lagged COMMIT.
    pub committed: bool,
    /// Opaque resume token persisted into `PublicationRow.phase_cursor`.
    pub cursor: Option<String>,
    /// Work performed this invocation (for bounded budgets).
    pub processed_bytes: u64,
    /// `NovelId` assigned at commit (create) or confirmed (overwrite).
    pub assigned_id: Option<i64>,
    /// Serialized per-novel receipt when `done` after commit.
    pub receipt: Option<Vec<u8>>,
}

/// Export snapshot inventory entry written into catalog rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportNovelEntry {
    pub session_id: SessionId,
    pub novel_ordinal: u32,
    pub id: NovelId,
    pub toc_url: String,
    pub title: String,
    pub kind: NovelKind,
    /// Digest of the captured ordered source membership (not a second URL array).
    pub membership_digest: Option<String>,
    /// Record/content revision for snapshot fidelity.
    pub record_revision: String,
    pub content_revision: String,
    /// Frozen metadata layout on the first inventory page; no whole-value clone.
    pub metadata: Option<crate::transfer::NovelJsonLayout>,
    /// One bounded inventory page; the same novel ordinal repeats until
    /// `files_complete`. The backend cursor carries both novel and file position.
    pub files: Vec<SourceFileEntry>,
    pub files_complete: bool,
}

/// Report from `recover`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Sessions moved to `failed`/`expired` during recovery.
    pub failed_sessions: u32,
    /// Publications rolled back or completed.
    pub resolved_publications: u32,
    /// Publications still needing attention (`recovery_required`).
    pub pending_recovery: u32,
    pub stepped_publications: u32,
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// Durable small-record store for transfer sessions.
///
/// One row per session/file/chunk/operation/create/publication — never a
/// monolithic JSON blob. All methods may be retried after transport failure,
/// so every write is idempotent on its natural key.
///
/// Consistency rules the engine relies on:
/// * `update_session` must CAS on `expected_revision`.
/// * `commit_chunk` must durably write the chunk row, update the file row's
///   `confirmed_bytes`/`next_chunk_index`/`hash_checkpoint`/`stage`, and add
///   `payload_size` to the session's `confirmed_bytes` in one atomic unit.
/// * `begin_operation` must insert the row only if absent; `Ok(true)` =
///   caller owns execution, `Ok(false)` = replay/in-flight — read the row.
/// * `finish_operation` writes the result and `completed=true` atomically.
pub trait TransferJournal: PlatformService {
    // -- sessions -----------------------------------------------------------
    fn create_session<'a>(&'a self, row: &'a SessionRow) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn session<'a>(
        &'a self,
        scope: &'a AuthScope,
        id: &'a SessionId,
    ) -> PlatformFuture<'a, Result<Option<SessionRow>, TransferError>>;
    /// CAS update; fails with `RevisionMismatch{current}` on revision drift.
    fn update_session<'a>(
        &'a self,
        row: &'a SessionRow,
        expected_revision: u64,
    ) -> PlatformFuture<'a, Result<(), TransferError>>;
    /// Live (non-terminal) sessions for the scope, updated_at ascending.
    fn list_sessions<'a>(
        &'a self,
        scope: &'a AuthScope,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<SessionRow>, TransferError>>;
    fn count_live_sessions<'a>(
        &'a self,
        scope: &'a AuthScope,
    ) -> PlatformFuture<'a, Result<u32, TransferError>>;
    /// Sessions idle past `now` (for the expiry GC); excludes sessions with
    /// `recovering_publications` — the caller filters those via
    /// `pending_publications`.
    fn find_idle_sessions<'a>(
        &'a self,
        scope: &'a AuthScope,
        now: DateTime<Utc>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<SessionRow>, TransferError>>;
    /// Sessions with non-terminal publications (startup/periodic recovery).
    fn sessions_with_pending_publications<'a>(
        &'a self,
        scope: &'a AuthScope,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<SessionRow>, TransferError>>;

    // -- create idempotency --------------------------------------------------
    fn find_create<'a>(
        &'a self,
        scope: &'a AuthScope,
        request_id: &'a str,
    ) -> PlatformFuture<'a, Result<Option<CreateRow>, TransferError>>;
    /// Insert-or-ignore on `(scope, request_id)`.
    fn put_create<'a>(&'a self, row: &'a CreateRow) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn update_create<'a>(&'a self, row: &'a CreateRow) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- operations -----------------------------------------------------------
    fn operation<'a>(
        &'a self,
        session_id: &'a SessionId,
        operation_id: &'a OperationId,
    ) -> PlatformFuture<'a, Result<Option<OperationRow>, TransferError>>;
    /// Returns `true` when the row was inserted (caller owns execution).
    fn begin_operation<'a>(&'a self, row: &'a OperationRow) -> PlatformFuture<'a, Result<bool, TransferError>>;
    fn finish_operation<'a>(&'a self, row: &'a OperationRow) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- files ---------------------------------------------------------------
    fn put_file<'a>(&'a self, row: &'a FileRow) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn file<'a>(
        &'a self,
        session_id: &'a SessionId,
        file_id: u32,
    ) -> PlatformFuture<'a, Result<Option<FileRow>, TransferError>>;
    fn files<'a>(
        &'a self,
        session_id: &'a SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<FileRow>, TransferError>>;
    fn update_file<'a>(&'a self, row: &'a FileRow) -> PlatformFuture<'a, Result<(), TransferError>>;
    /// Import side: files still needing bytes or reuse decision.
    fn pending_files<'a>(
        &'a self,
        session_id: &'a SessionId,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<FileRow>, TransferError>>;
    /// Indexed external-finalization queue, including the reserved manifest.
    fn finalizing_files<'a>(
        &'a self, session_id: &'a SessionId, limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<FileRow>, TransferError>>;
    /// Any file row whose stage/pin must be released at session teardown.
    fn staged_files<'a>(
        &'a self,
        session_id: &'a SessionId,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<FileRow>, TransferError>>;

    /// Atomically discard this file's receipts and confirmed prefix after a
    /// corrupt/unknown hash checkpoint; verified files are never reset.
    fn reset_file<'a>(&'a self, row: &'a FileRow) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- chunks --------------------------------------------------------------
    fn chunk<'a>(
        &'a self,
        session_id: &'a SessionId,
        file_id: u32,
        chunk_index: u32,
    ) -> PlatformFuture<'a, Result<Option<ChunkRow>, TransferError>>;
    /// Atomic per-record commit (see trait docs). The provided file row is
    /// the post-commit expected state; the journal may verify the file's
    /// current `next_chunk_index`/`confirmed_bytes` equal `row`'s pre-state.
    fn commit_chunk<'a>(
        &'a self,
        chunk: &'a ChunkRow,
        file: &'a FileRow,
        session_confirmed_bytes: u64,
    ) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- publications ----------------------------------------------------------
    fn put_publication<'a>(&'a self, row: &'a PublicationRow) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn publication<'a>(
        &'a self,
        session_id: &'a SessionId,
        publication_id: u32,
    ) -> PlatformFuture<'a, Result<Option<PublicationRow>, TransferError>>;
    fn update_publication<'a>(&'a self, row: &'a PublicationRow) -> PlatformFuture<'a, Result<(), TransferError>>;
    /// Non-terminal publications (`pending` … `committing`, `rolling_back`).
    fn pending_publications<'a>(
        &'a self,
        session_id: &'a SessionId,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<PublicationRow>, TransferError>>;
    /// Publications that may own canonical/preimage resources. Planned Pending
    /// rows alone must not block expiry or terminal resource collection.
    fn active_publications<'a>(
        &'a self, session_id: &'a SessionId, limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<PublicationRow>, TransferError>>;
    fn publications<'a>(
        &'a self,
        session_id: &'a SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<PublicationRow>, TransferError>>;

    // -- journal blobs ---------------------------------------------------------
    /// Small durable keyed blob within a publication (undo/new metadata,
    /// `committed` mark, finish receipt). Total per key ≤ 256 KiB; file
    /// pre-images live in backend stage objects, not here.
    fn put_journal_entry<'a>(
        &'a self,
        session_id: &'a SessionId,
        publication_id: u32,
        key: &'a str,
        value: &'a [u8],
    ) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn journal_entry<'a>(
        &'a self,
        session_id: &'a SessionId,
        publication_id: u32,
        key: &'a str,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>, TransferError>>;
    fn delete_journal_entries<'a>(
        &'a self,
        session_id: &'a SessionId,
        publication_id: u32,
    ) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- plan / catalog --------------------------------------------------------
    /// Persist one page of plan entries (idempotent on `novel_ordinal`).
    fn put_plan_entries<'a>(
        &'a self,
        session_id: &'a SessionId,
        plan_revision: &'a str,
        entries: &'a [PlanEntry],
    ) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn plan_entries<'a>(
        &'a self,
        session_id: &'a SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<PlanEntry>, TransferError>>;
    /// Persist one page of export catalog rows (idempotent on
    /// `(novel_ordinal, logical_path)`).
    fn put_catalog_entries<'a>(
        &'a self,
        session_id: &'a SessionId,
        catalog_revision: u64,
        entries: &'a [CatalogItem],
    ) -> PlatformFuture<'a, Result<(), TransferError>>;
    fn catalog_entries<'a>(
        &'a self,
        session_id: &'a SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<CatalogItem>, TransferError>>;
    /// Same catalog, ordered by the UTF-8 canonical digest row (decimal
    /// ordinal, then logical path). This is an indexed, bounded scan.
    fn selection_catalog_entries<'a>(
        &'a self, session_id: &'a SessionId, cursor: Option<&'a str>, limit: usize,
    ) -> PlatformFuture<'a, Result<Page<CatalogItem>, TransferError>>;
    /// Selected catalog rows only (post-`select`), ordered by ordinal/path.
    fn selected_catalog_entries<'a>(
        &'a self,
        session_id: &'a SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<CatalogItem>, TransferError>>;
    /// Update selection flags for a batch of catalog keys after `select`.
    fn mark_catalog_selected<'a>(
        &'a self,
        session_id: &'a SessionId,
        keys: &'a [(u32, String)],
        selected: bool,
    ) -> PlatformFuture<'a, Result<(), TransferError>>;
}

/// Library-facing work for the transfer engine.
///
/// All reads that feed durable decisions must come from the primary /
/// `for_writes` equivalent — never a lagging replica or isolate cache.
pub trait TransferBackend: PlatformService {
    /// Cryptographically random opaque id (`sess_…`, `op_…`, batch keys).
    /// Sync because adapters seed it from their own CSPRNG once.
    fn opaque_id(&self, prefix: &str) -> String;

    // -- capabilities & tickets ------------------------------------------------
    fn storage_capabilities<'a>(&'a self, scope: &'a AuthScope)
        -> PlatformFuture<'a, Result<StorageCapabilities, TransferError>>;
    /// Issue a signed creation ticket bound to (scope, protocol, expiry).
    fn issue_ticket<'a>(
        &'a self,
        scope: &'a AuthScope,
        valid_for_secs: i64,
    ) -> PlatformFuture<'a, Result<String, TransferError>>;
    /// Verify a previously issued ticket (signature + expiry + scope).
    fn verify_ticket<'a>(
        &'a self,
        scope: &'a AuthScope,
        ticket: &'a str,
    ) -> PlatformFuture<'a, Result<bool, TransferError>>;

    // -- library index -----------------------------------------------------------
    /// Resolve `toc_url`s to existing records for planning (batched).
    /// Missing urls simply don't appear in the result.
    fn library_index<'a>(
        &'a self,
        scope: &'a AuthScope,
        toc_urls: &'a [String],
    ) -> PlatformFuture<'a, Result<Vec<LibraryMatch>, TransferError>>;

    // -- reuse --------------------------------------------------------------------
    /// Look for an already-verified stored copy of `(novel_ordinal's target,
    /// logical_path, size, sha256)` inside the session's allowed selection.
    /// `Some` pins the content for this session. Never an arbitrary-hash
    /// existence oracle: the backend restricts the search to the novels the
    /// plan touches.
    fn find_reusable<'a>(
        &'a self,
        scope: &'a AuthScope,
        session: &'a SessionRow,
        file: &'a FileRow,
    ) -> PlatformFuture<'a, Result<Option<PinRef>, TransferError>>;
    /// Release one pin taken by `find_reusable` or `pin_source`.
    fn release_pin<'a>(
        &'a self,
        session_id: &'a SessionId,
        pin: &'a str,
    ) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- stage ----------------------------------------------------------------------
    /// Open or resume a private transfer stage object. `expected_size` is the
    /// declared whole-file size (for reservation); resumes must return the
    /// same handle for the same `(session_id, file_id)`.
    fn stage_begin<'a>(
        &'a self,
        session_id: &'a SessionId,
        file_id: u32,
        expected_size: u64,
    ) -> PlatformFuture<'a, Result<String, TransferError>>;
    /// Append exactly-at-`offset` bytes; `offset` must equal the current
    /// confirmed length. Idempotent on `(handle, offset, sha256-of-chunk)`.
    fn stage_append<'a>(
        &'a self,
        handle: &'a str,
        offset: u64,
        data: &'a [u8],
    ) -> PlatformFuture<'a, Result<(), TransferError>>;
    /// Seal the stage: verify size/SHA and make it immutable. Returns the
    /// durable `StageRef` string stored on the file row.
    fn stage_seal<'a>(
        &'a self,
        handle: &'a str,
        expected_size: u64,
        expected_sha256: &'a str,
    ) -> PlatformFuture<'a, Result<String, TransferError>>;
    /// True when `stage_seal` may need an `advance`-driven `finalize` step
    /// (e.g. S3 multipart completion retried outside the request).
    fn stage_needs_finalize(&self) -> bool;
    /// Complete a seal that previously returned `StageRef` pending
    /// finalization; safe to call repeatedly with the same handle.
    fn stage_finalize<'a>(
        &'a self,
        handle: &'a str,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Range-read sealed stage bytes (bounded reads for export serving).
    fn stage_read_range<'a>(
        &'a self,
        stage: &'a str,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Vec<u8>, TransferError>>;
    /// Abort and reclaim a stage (session teardown / replaced upload).
    fn stage_abort<'a>(
        &'a self,
        session_id: &'a SessionId,
        stage: &'a str,
    ) -> PlatformFuture<'a, Result<(), TransferError>>;

    // -- export inventory -----------------------------------------------------------
    /// Resolve requested ids (expanding virtual members in order,
    /// de-duplicated) to exportable novel entries with their file listings.
    /// Bounded: returns up to `limit` entries starting after `cursor`.
    fn export_inventory<'a>(
        &'a self,
        scope: &'a AuthScope,
        session: &'a SessionId,
        ids: &'a [i64],
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<ExportNovelEntry>, TransferError>>;

    /// Pull stable, UTF-8 raw metadata fragments (at most100, each <=32KiB).
    fn export_metadata_page<'a>(
        &'a self, session: &'a SessionId, novel_ordinal: u32,
        cursor: &'a MetadataCursor, limit: usize,
    ) -> PlatformFuture<'a, Result<ExportMetadataPage, TransferError>>;

    /// Acquire/renew the novel mutation gate before copying any selected
    /// bytes; release only after every selected file of this novel is sealed.
    /// Acquisition closes admission before reader drain. The session/ordinal
    /// pair is the durable gate owner; invocations never steal an OS lock.
    fn snapshot_gate<'a>(
        &'a self, session_id: &'a SessionId, entry: &'a ExportNovelEntry, release: bool,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;

    /// Validate/freeze metadata-only novels too, then release all remaining
    /// snapshot gates in bounded pages before any ZIP bytes can be read.
    fn snapshot_finish<'a>(&'a self, session_id:&'a SessionId) -> PlatformFuture<'a,Result<PublicationStep,TransferError>>;

    /// Pin immutable/verified source content for export in place.
    /// `SourceRef::Pinnable` entries only. Returns the pin plus the verified
    /// size/SHA the archive manifest must advertise.
    fn pin_source<'a>(
        &'a self,
        session_id: &'a SessionId,
        entry: &'a ExportNovelEntry,
        file: &'a SourceFileEntry,
    ) -> PlatformFuture<'a, Result<PinRef, TransferError>>;
    /// Range-read one source file (≤ 8 MiB per call) through the exclusive
    /// streamed-range port — one S3 Range GET per call, chunked rows for DB,
    /// seek for FS. Used only while snapshot-copying a `Readable` source.
    fn source_read_range<'a>(
        &'a self,
        entry: &'a ExportNovelEntry,
        file: &'a SourceFileEntry,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Vec<u8>, TransferError>>;
    /// Current fingerprint of a source file for mid-copy verification.
    fn source_fingerprint<'a>(
        &'a self,
        entry: &'a ExportNovelEntry,
        file: &'a SourceFileEntry,
    ) -> PlatformFuture<'a, Result<Option<String>, TransferError>>;

    // -- publication primitives ---------------------------------------------------
    /// Close the reader/writer admission gate for this novel (durable gate +
    /// epoch). Returns `done=false` while the gate is still closing.
    fn publication_gate_close<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Bounded step draining existing readers (CAS-lease expiry).
    fn publication_drain<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Re-check plan conditions, reserve create id/prefix, record pre-image
    /// and metadata undo/new values into the backend journal.
    fn publication_prepare<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Bounded file installation from sealed stages/pins.
    fn publication_install<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Single commit point: record + tag sequence + membership + receipt +
    /// `committed` journal mark in one boundary.
    fn publication_commit<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Bounded rollback to pre-image (only private new files are removed;
    /// already-restored files are skipped by fingerprint).
    fn publication_rollback<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Post-commit finish (receipt persistence, pre-image collection).
    fn publication_finish<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
    /// Re-open the admission gate after commit/rollback.
    fn publication_gate_open<'a>(
        &'a self,
        spec: &'a PublicationSpec,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;

    // -- session teardown / GC --------------------------------------------------------
    /// Release every private resource of a session (stages, pins, export
    /// snapshot refs) that is not referenced by a live publication or reader.
    /// Bounded; returns `done` + cursor.
    fn release_session_resources<'a>(
        &'a self,
        session: &'a SessionRow,
        cursor: Option<&'a str>,
    ) -> PlatformFuture<'a, Result<PublicationStep, TransferError>>;
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

struct CreationParameters<'a> {
    scope: &'a AuthScope,
    direction: TransferDirection,
    request_id: &'a str,
    creation_ticket: &'a str,
    fingerprint: String,
    secret: &'a [u8],
    policy: ConflictPolicy,
    profile: TransferProfile,
    extra_roles: Vec<String>,
    policy_version: u32,
    gate: &'a CapabilityGate,
}

/// Shared transfer engine. Constructed once per process with the concrete
/// backend + journal; every method is a bounded, idempotent step.
pub struct TransferService {
    backend: Arc<dyn TransferBackend>,
    journal: Arc<dyn TransferJournal>,
    clock: Arc<dyn Clock>,
    build: TransferBuildInfo,
    limits: TransferLimits,
}

impl TransferService {
    pub fn new(
        backend: Arc<dyn TransferBackend>,
        journal: Arc<dyn TransferJournal>,
        clock: Arc<dyn Clock>,
        build: TransferBuildInfo,
    ) -> Self {
        Self { backend, journal, clock, build, limits: TransferLimits::default() }
    }

    pub fn with_limits(mut self, limits: TransferLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn backend(&self) -> &Arc<dyn TransferBackend> {
        &self.backend
    }

    pub fn journal(&self) -> &Arc<dyn TransferJournal> {
        &self.journal
    }

    /// SHA-256 fingerprint of a create request for idempotency binding:
    /// (scope, direction, canonical body, secret hash).
    pub fn create_fingerprint(
        scope: &AuthScope,
        direction: TransferDirection,
        canonical_body: &[u8],
        secret: &[u8],
    ) -> String {
        let mut material = Vec::with_capacity(canonical_body.len() + 128);
        material.extend_from_slice(scope.library_id.as_bytes());
        material.push(0);
        material.extend_from_slice(scope.principal.as_bytes());
        material.push(0);
        material.push(match direction {
            TransferDirection::Import => b'i',
            TransferDirection::Export => b'e',
        });
        material.extend_from_slice(canonical_body);
        material.extend_from_slice(SecretHash::of(secret).0.as_bytes());
        sha256_hex(&material)
    }

    /// Selection digest over (catalog_revision, profile, extra_roles,
    /// selected `(novel_ordinal, logical_path, size, role)` rows) — the exact
    /// set the client confirmed. Canonical byte string (SHA-256 hashed):
    ///
    /// ```text
    /// "v2" NUL <catalog_revision decimal> NUL <profile> NUL
    /// <sorted extra_roles joined by ","> NUL
    /// then for each selected row, sorted by (novel_ordinal, logical_path):
    ///   <novel_ordinal decimal> NUL <logical_path utf8> NUL
    ///   <size decimal> NUL <role serde name> SOH
    /// ```
    pub fn selection_digest(
        catalog_revision: u64,
        profile: TransferProfile,
        extra_roles: &[String],
        selected: &[(u32, String, u64, FileRole)],
    ) -> String {
        let mut rows: Vec<String> = selected
            .iter()
            .map(|(ordinal, path, size, role)| {
                format!("{ordinal}\x00{path}\x00{size}\x00{}", role.as_str())
            })
            .collect();
        rows.sort();
        let mut roles = extra_roles.to_vec();
        roles.sort();
        let mut material = format!(
            "v2\x00{catalog_revision}\x00{}\x00{}\x00",
            profile.as_str(),
            roles.join(",")
        );
        for row in &rows {
            material.push_str(row);
            material.push('\x01');
        }
        sha256_hex(material.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// SHA-256 durable checkpoints (sha2 0.11 SerializableState)
// ---------------------------------------------------------------------------

/// Algorithm tag prepended to serialized checkpoints so a future algorithm or
/// codec change can never be mistaken for a valid SHA-256 midstate.
const HASH_CHECKPOINT_TAG: u8 = 1;

fn sha256_checkpoint(hasher: &sha2::Sha256) -> Vec<u8> {
    use sha2::digest::common::hazmat::SerializableState;
    let state = SerializableState::serialize(hasher);
    let mut out = Vec::with_capacity(state.len() + 1);
    out.push(HASH_CHECKPOINT_TAG);
    out.extend_from_slice(state.as_ref());
    out
}

/// Restore a running hasher from a checkpoint. `None` on tag mismatch,
/// truncation, or corrupt state — callers restart the file from offset 0.
fn sha256_resume(checkpoint: &[u8]) -> Option<sha2::Sha256> {
    use sha2::digest::common::hazmat::{SerializableState, SerializedState};
    let (tag, state) = checkpoint.split_first()?;
    if *tag != HASH_CHECKPOINT_TAG {
        return None;
    }
    let state = SerializedState::<sha2::Sha256>::try_from(state).ok()?;
    <sha2::Sha256 as SerializableState>::deserialize(&state).ok()
}

// ---------------------------------------------------------------------------
// Engine internals
// ---------------------------------------------------------------------------

/// Publication-id namespace reserved for session-scoped journal blobs
/// (novel metadata, text fragments, export novel blobs, batch plans). Real
/// publications start at 0 and are bounded by `MAX_NOVELS`.
const SESSION_JOURNAL_PUB: u32 = u32::MAX;

const JKEY_NOVEL: &str = "novel:";
const JKEY_TEXT: &str = "text:";
const JKEY_EXPORT_NOVEL: &str = "xnovel:";
const JKEY_EXPORT_MANIFEST: &str = "xmanifest";
const JKEY_EXPORT_BATCH: &str = "xbatch:";
const JKEY_BATCH_COUNT: &str = "xbatch_count";

fn invalid_state(row: &SessionRow, expected: &'static str) -> TransferError {
    TransferError::InvalidState { state: row.state.as_str(), expected }
}

fn truncate_title(title: &str) -> String {
    let end = title.char_indices().nth(DISPLAY_TITLE_CHARS).map_or(title.len(), |(offset, _)| offset);
    title[..end].to_owned()
}


/// Batch plan entry persisted in the export journal (`xbatch:{index}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BatchPlanEntry {
    /// `(file_id, first_chunk_index, record_count)` segments in order.
    segments: Vec<(u32, u32, u32)>,
}

/// Per-novel receipt written back into `PublicationRow.receipt` (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationReceipt {
    pub novel_ordinal: u32,
    pub action: PlanAction,
    /// `created` | `overwritten` | `skipped` | `failed` | `rolled_back`.
    pub outcome: String,
    pub assigned_id: Option<i64>,
    pub files_installed: u32,
    pub error: Option<String>,
}

fn finish_publication(
    publication: &mut PublicationRow,
    spec: &PublicationSpec,
    committed: bool,
) -> Result<PublicationState, TransferError> {
    publication.phase_cursor = Some("finished".into());
    if committed {
        publication.error = None;
        return Ok(PublicationState::Committed);
    }
    let receipt = PublicationReceipt {
        novel_ordinal: publication.publication_id,
        action: spec.target.action,
        outcome: "rolled_back".into(),
        assigned_id: spec.target.existing_id,
        files_installed: 0,
        error: publication.error.clone(),
    };
    publication.receipt = Some(serde_json::to_vec(&receipt).map_err(TransferError::platform)?);
    Ok(PublicationState::RolledBack)
}

/// Cursor inside `advance` work, kept in the session's `advance` op rows.
#[derive(Debug, Default)]
struct AdvanceBudget {
    records: u32,
    bytes: u64,
}

impl AdvanceBudget {
    fn exhausted(&self) -> bool {
        self.records >= ADVANCE_ITEM_BUDGET || self.bytes >= ADVANCE_BYTE_BUDGET
    }
}

impl TransferService {
    // -- session loading ----------------------------------------------------

    async fn load_session(
        &self,
        scope: &AuthScope,
        session_id: &SessionId,
        secret: Option<&[u8]>,
    ) -> Result<SessionRow, TransferError> {
        let mut row = self
            .journal
            .session(scope, session_id)
            .await?
            .ok_or(TransferError::SessionNotFound)?;
        if row.scope != *scope {
            return Err(TransferError::SessionNotFound);
        }
        if let Some(secret) = secret {
            let supplied = SecretHash::of(secret);
            if !constant_time_eq(supplied.0.as_bytes(), row.secret_hash.0.as_bytes()) {
                return Err(TransferError::SecretMismatch);
            }
        }
        if !row.state.is_terminal() && !matches!(row.state, SessionState::Publishing | SessionState::Cancelling) && row.resume_phase != Some(SessionState::Publishing) && self.clock.now_utc() >= row.expires_at {
            // Idle expiry is enforced by `expire_idle`/reads: mark expired and
            // start resource release rather than serving a dead session.
            let pending = self
                .journal
                .active_publications(session_id, 1)
                .await?
                .len();
            if pending == 0 {
                row.state = SessionState::Expired;
                row.resume_phase = None;
                row.error = Some("session_expired".into());
                self.schedule(&mut row);
                let expected = row.revision;
                row.revision += 1;
                self.journal.update_session(&row, expected).await?;
                return Err(TransferError::SessionExpired { resumable: false });
            }
        }
        Ok(row)
    }

    /// Persist `row` with a CAS on the revision it was loaded with, then
    /// advance the in-memory copy. `next_action` is refreshed afterwards by
    /// [`TransferService::status_of`] readers; rows stored carry the
    /// action that made sense at commit time.
    async fn persist(&self, row: &mut SessionRow, bump_revision: bool) -> Result<(), TransferError> {
        let expected = row.revision;
        let mut stored = row.clone();
        stored.published_novels=self.journal.session(&row.scope,&row.session_id).await?.ok_or(TransferError::SessionNotFound)?.published_novels;
        if bump_revision {
            stored.revision = expected.saturating_add(1);
        }
        stored.updated_at = self.clock.now_utc();
        stored.expires_at = stored.updated_at + chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);
        self.journal.update_session(&stored, expected).await?;
        *row = stored;
        Ok(())
    }

    fn status_of(&self, row: &SessionRow) -> SessionStatus {
        SessionStatus {
            protocol_version: PROTOCOL_VERSION,
            session_id: row.session_id.clone(),
            direction: row.direction,
            revision: row.revision,
            state: row.state,
            resume_phase: row.resume_phase,
            confirmed_bytes: row.confirmed_bytes,
            published_novels: row.published_novels,
            next_action: row.next_action,
            resume_mode: resume_mode_of(row),
            operation_id: row.operation_id.clone(),
            expires_at: row.expires_at,
            chunk_bytes: row.chunk_bytes,
            catalog_revision: row.catalog_revision,
            manifest_confirmed_bytes: import_ext(row).map_or(0, |e| e.manifest_confirmed),
            manifest_next_chunk_index: import_ext(row).map_or(0, |e| ((e.manifest_confirmed + row.chunk_bytes.saturating_sub(1)) / row.chunk_bytes) as u32),
            plan_revision: row.plan_revision.clone(),
            selection_digest: export_ext(row).and_then(|e| e.selection_digest.clone()),
            error: row.error.clone(),
        }
    }

    async fn status_response(&self, row: &SessionRow) -> SessionStatus {
        self.status_of(row)
    }

    // -- capabilities ---------------------------------------------------------

    pub async fn capabilities(&self, scope: &AuthScope) -> Result<Capabilities, TransferError> {
        let storage = self.backend.storage_capabilities(scope).await?;
        let ticket = self
            .backend
            .issue_ticket(scope, CREATION_TICKET_TTL_SECS)
            .await?;
        Ok(Capabilities {
            protocol_version: PROTOCOL_VERSION,
            codec_version: PROTOCOL_VERSION,
            build: self.build.clone(),
            limits: self.limits.clone(),
            storage,
            creation_ticket: ticket,
            server_time: self.clock.now_utc(),
        })
    }

    // -- creation -------------------------------------------------------------

    async fn create_common(
        &self, parameters: CreationParameters<'_>, make_ext: impl FnOnce() -> SessionExt,
    ) -> Result<CreateResult, TransferError> {
        let CreationParameters { scope, direction, request_id, creation_ticket, fingerprint,
            secret, policy, profile, extra_roles, policy_version, gate } = parameters;
        if request_id.is_empty() || request_id.len() > 128 || secret.len() != 32 {
            return Err(TransferError::invalid("invalid_request", "request_id must be 1..128 bytes and session secret exactly 256 bits"));
        }
        crate::transfer::projection::validate_extra_roles(profile, &extra_roles).map_err(TransferError::platform)?;
        let previous = self.journal.find_create(scope, request_id).await?;
        if let Some(existing) = &previous {
            if existing.request_fingerprint != fingerprint { return Err(TransferError::IdempotencyConflict); }
            if let Some(id) = &existing.session_id
                && let Some(row) = self.journal.session(scope, id).await?
            {
                return Ok(CreateResult { status: self.status_of(&row), replayed: true });
            }
        } else if !self.backend.verify_ticket(scope, creation_ticket).await? {
            return Err(TransferError::invalid("request_expired", "creation ticket is invalid or expired"));
        }
        if !gate.allowed { return Err(TransferError::Unsupported { code: match direction { TransferDirection::Import => "import_unsupported", TransferDirection::Export => "export_unsupported" }, reason: gate.reason.clone().unwrap_or_else(|| "operation not supported".into()) }); }
        let now = self.clock.now_utc();
        let proposed = CreateRow { request_id: request_id.into(), scope: scope.clone(), request_fingerprint: fingerprint.clone(),
            session_id: Some(SessionId(self.backend.opaque_id("sess"))), receipt: None, created_at: now };
        self.journal.put_create(&proposed).await?;
        let mut adopted = self.journal.find_create(scope, request_id).await?.ok_or_else(|| TransferError::invalid("journal_missing", "create identity was not persisted"))?;
        if adopted.request_fingerprint != fingerprint { return Err(TransferError::IdempotencyConflict); }
        let session_id = adopted.session_id.clone().ok_or_else(|| TransferError::invalid("journal_corrupt", "create identity has no session allocation"))?;
        if let Some(row) = self.journal.session(scope, &session_id).await? { return Ok(CreateResult { status: self.status_of(&row), replayed: true }); }
        if self.journal.count_live_sessions(scope).await? >= self.limits.max_sessions_per_scope { return Err(TransferError::TooManySessions { max: self.limits.max_sessions_per_scope }); }
        let (state, next_action) = match direction { TransferDirection::Import => (SessionState::ReceivingManifest, NextAction::Upload), TransferDirection::Export => (SessionState::Cataloguing, NextAction::Advance) };
        let row = SessionRow { session_id: session_id.clone(), scope: scope.clone(), secret_hash: SecretHash::of(secret), direction, state, resume_phase: None, revision: 1, conflict_policy: policy, profile, extra_roles, policy_version, plan_revision: None, catalog_revision: 0, chunk_bytes: CHUNK_BYTES, confirmed_bytes: 0, published_novels: 0, next_action,
            operation_id: (next_action == NextAction::Advance).then(|| OperationId(self.backend.opaque_id("op"))), last_operation_id: None, last_operation_result: None, gc_cursor: None, error: None, created_at: now, updated_at: now, expires_at: now + chrono::Duration::seconds(SESSION_IDLE_TTL_SECS), ext: make_ext() };
        self.journal.create_session(&row).await?;
        let actual = self.journal.session(scope, &session_id).await?.ok_or(TransferError::SessionNotFound)?;
        let result = CreateResult { status: self.status_of(&actual), replayed: false };
        adopted.receipt = Some(serde_json::to_vec(&result).map_err(TransferError::platform)?);
        self.journal.update_create(&adopted).await?;
        Ok(result)
    }

    pub async fn create_import(
        &self,
        scope: &AuthScope,
        request: &CreateImportRequest,
        secret: &[u8],
    ) -> Result<CreateResult, TransferError> {
        if request.manifest_size == 0 || request.manifest_size > MAX_WIRE_MANIFEST_BYTES {
            return Err(TransferError::invalid(
                "manifest_too_large",
                format!(
                    "declared manifest size {} exceeds the {} byte limit",
                    request.manifest_size, MAX_WIRE_MANIFEST_BYTES
                ),
            ));
        }
        if sha256_from_hex(&request.manifest_sha256).is_none() {
            return Err(TransferError::invalid(
                "invalid_request",
                "manifest_sha256 must be a 64-char hex SHA-256",
            ));
        }
        if sha256_from_hex(&request.source_fingerprint).is_none() {
            return Err(TransferError::invalid(
                "invalid_request",
                "source_fingerprint must be a 64-char hex SHA-256",
            ));
        }
        let caps = self.backend.storage_capabilities(scope).await?;
        if caps.policy_version != request.policy_version {
            return Err(TransferError::invalid(
                "policy_version_mismatch",
                format!(
                    "client policy version {} does not match server {}",
                    request.policy_version, caps.policy_version
                ),
            ));
        }
        let fingerprint = Self::create_fingerprint(
            scope,
            TransferDirection::Import,
            serde_json::to_string(request).unwrap_or_default().as_bytes(),
            secret,
        );
        let manifest_size = request.manifest_size;
        let manifest_sha256 = request.manifest_sha256.clone();
        let source_fingerprint = request.source_fingerprint.clone();
        self.create_common(
            CreationParameters {
                scope, direction: TransferDirection::Import, request_id: &request.request_id,
                creation_ticket: &request.creation_ticket, fingerprint, secret,
                policy: request.conflict_policy, profile: request.profile,
                extra_roles: request.extra_roles.clone(), policy_version: request.policy_version,
                gate: &caps.import,
            },
            move || {
                SessionExt::Import(ImportSessionExt {
                    source_fingerprint,
                    manifest_size,
                    manifest_sha256,
                    ..Default::default()
                })
            },
        )
        .await
    }

    pub async fn create_export(
        &self,
        scope: &AuthScope,
        request: &CreateExportRequest,
        secret: &[u8],
    ) -> Result<CreateResult, TransferError> {
        if request.ids.is_empty() || request.ids.len() > 10_000 {
            return Err(TransferError::invalid(
                "invalid_request",
                "exports require 1..10000 novel ids",
            ));
        }
        let caps = self.backend.storage_capabilities(scope).await?;
        let fingerprint = Self::create_fingerprint(
            scope,
            TransferDirection::Export,
            serde_json::to_string(request).unwrap_or_default().as_bytes(),
            secret,
        );
        let ids = request.ids.clone();
        self.create_common(
            CreationParameters {
                scope, direction: TransferDirection::Export, request_id: &request.request_id,
                creation_ticket: &request.creation_ticket, fingerprint, secret,
                policy: ConflictPolicy::Skip, profile: request.profile,
                extra_roles: request.extra_roles.clone(), policy_version: caps.policy_version,
                gate: &caps.export,
            },
            move || {
                SessionExt::Export(ExportSessionExt {
                    requested_ids: ids,
                    ..Default::default()
                })
            },
        )
        .await
    }

    // -- reads ------------------------------------------------------------------

    pub async fn session(
        &self,
        scope: &AuthScope,
        session_id: &SessionId,
        secret: Option<&[u8]>,
    ) -> Result<SessionStatus, TransferError> {
        let row = self.load_session(scope, session_id, secret).await?;
        Ok(self.status_response(&row).await)
    }

    pub async fn list_sessions(
        &self,
        scope: &AuthScope,
        cursor: Option<&str>,
    ) -> Result<SessionPage, TransferError> {
        let page = self
            .journal
            .list_sessions(scope, cursor, MAX_PAGE_ITEMS)
            .await?;
        let mut items = Vec::with_capacity(page.items.len());
        let mut maintained = false;
        for row in &page.items {
            let mut row = row.clone();
            if !maintained && (row.state == SessionState::Cancelling || row.state.is_terminal() && row.next_action == NextAction::Advance) {
                if let Some(operation) = row.operation_id.clone() {
                    self.advance_authenticated(scope, &row.session_id, None, &operation).await?;
                    row = self.journal.session(scope, &row.session_id).await?.ok_or(TransferError::SessionNotFound)?;
                }
                maintained = true;
            }
            if !row.state.is_terminal() { items.push(self.status_response(&row).await); }
        }
        Ok(Page { items, next_cursor: page.next_cursor })
    }

}

fn export_ext(row: &SessionRow) -> Option<&ExportSessionExt> {
    match &row.ext {
        SessionExt::Export(e) => Some(e),
        _ => None,
    }
}

fn export_ext_mut(row: &mut SessionRow) -> Result<&mut ExportSessionExt, TransferError> {
    match &mut row.ext {
        SessionExt::Export(e) => Ok(e),
        _ => Err(TransferError::invalid("internal", "not an export session")),
    }
}

fn import_ext_mut(row: &mut SessionRow) -> Result<&mut ImportSessionExt, TransferError> {
    match &mut row.ext {
        SessionExt::Import(e) => Ok(e),
        _ => Err(TransferError::invalid("internal", "not an import session")),
    }
}

fn import_ext(row: &SessionRow) -> Option<&ImportSessionExt> {
    match &row.ext {
        SessionExt::Import(e) => Some(e),
        _ => None,
    }
}

fn resume_mode_of(row: &SessionRow) -> ResumeMode {
    if row.error.as_deref() == Some("recovery_required") {
        return ResumeMode::RecoverFirst;
    }
    match row.state {
        SessionState::Paused | SessionState::Cancelling => match row.resume_phase {
            Some(phase) => resume_mode_for_phase(row.direction, phase),
            None => ResumeMode::Continue,
        },
        SessionState::Failed => ResumeMode::RecoverFirst,
        SessionState::Cancelled | SessionState::Expired => ResumeMode::None,
        state => resume_mode_for_phase(row.direction, state),
    }
}

fn resume_mode_for_phase(direction: TransferDirection, state: SessionState) -> ResumeMode {
    match (direction, state) {
        (TransferDirection::Import, SessionState::ReceivingManifest)
        | (TransferDirection::Import, SessionState::Planning)
        | (TransferDirection::Import, SessionState::Transferring)
        | (TransferDirection::Import, SessionState::Ready)
        | (TransferDirection::Import, SessionState::Publishing) => ResumeMode::ReselectSource,
        (TransferDirection::Export, SessionState::Cataloguing)
        | (TransferDirection::Export, SessionState::Selecting)
        | (TransferDirection::Export, SessionState::Preparing) => ResumeMode::Continue,
        (TransferDirection::Export, SessionState::Ready)
        | (TransferDirection::Export, SessionState::Reading) => ResumeMode::RestartOutput,
        _ => ResumeMode::None,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ManifestCheckpoint {
    header: Option<crate::transfer::protocol::WireHeader>,
    novels: u32, files: u32, texts: u32, metadata_bytes: u64,
    current: Option<u32>, current_files: u32,
    #[serde(default)] current_bytes:u64,
    open_text: Option<(String, Option<u32>, u32)>,
    scalar_fields: u16, tags: u32, members: u32,
    title_nonempty: bool, url_nonempty: bool,
    ended: bool,
    #[serde(default)]
    projection_hash: Option<Vec<u8>>,
}

impl TransferService {
    async fn value<T: serde::de::DeserializeOwned>(&self, id: &SessionId, key: &str) -> Result<Option<T>, TransferError> {
        let Some(bytes) = self.journal.journal_entry(id, SESSION_JOURNAL_PUB, key).await? else { return Ok(None) };
        serde_json::from_slice(&bytes).map(Some).map_err(TransferError::platform)
    }

    async fn save_value<T: Serialize + ?Sized>(&self, id: &SessionId, key: &str, value: &T) -> Result<(), TransferError> {
        let bytes = serde_json::to_vec(value).map_err(TransferError::platform)?;
        if bytes.len() > MAX_CONTROL_BODY_BYTES { return Err(TransferError::invalid("metadata_record_too_large", "internal metadata record exceeds journal page")); }
        self.journal.put_journal_entry(id, SESSION_JOURNAL_PUB, key, &bytes).await
    }

    async fn advance_input<T:Serialize+serde::de::DeserializeOwned>(&self,row:&SessionRow,name:&str,input:T)->Result<T,TransferError>{
        let operation=row.operation_id.as_ref().ok_or(TransferError::OperationConflict)?;let key=format!("work:{}:{name}",operation.0);
        if let Some(input)=self.value(&row.session_id,&key).await?{return Ok(input);}
        self.save_value(&row.session_id,&key,&input).await?;Ok(input)
    }

    // Large metadata is split into independently durable, bounded journal rows.
    // The descriptor is written last, so an interrupted write cannot expose a tail.
    async fn save_blob(&self, id: &SessionId, key: &str, bytes: &[u8]) -> Result<(), TransferError> {
        for (part, bytes) in bytes.chunks(MAX_CONTROL_BODY_BYTES).enumerate() {
            self.journal.put_journal_entry(id, SESSION_JOURNAL_PUB, &format!("{key}:part:{part:08}"), bytes).await?;
        }
        self.save_value(id, key, &(bytes.len() as u64)).await
    }

    async fn blob(&self, id: &SessionId, key: &str, minimum_capacity:usize) -> Result<Vec<u8>, TransferError> {
        let size: u64 = self.value(id, key).await?.ok_or_else(|| TransferError::invalid("journal_missing", key))?;
        if size>MAX_BINARY_BODY_BYTES as u64||minimum_capacity>MAX_BINARY_BODY_BYTES{return Err(TransferError::invalid("metadata_budget","one journal blob exceeds8MiB"));}
        let mut bytes=Vec::with_capacity((size as usize).max(minimum_capacity));
        for part in 0..size.div_ceil(MAX_CONTROL_BODY_BYTES as u64) {
            let chunk = self.journal.journal_entry(id, SESSION_JOURNAL_PUB, &format!("{key}:part:{part:08}")).await?
                .ok_or_else(|| TransferError::invalid("journal_missing", "metadata fragment is missing"))?;
            if chunk.len()!=((size as usize-bytes.len()).min(MAX_CONTROL_BODY_BYTES)){return Err(TransferError::invalid("journal_corrupt","metadata fragment length mismatch"));}
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != size { return Err(TransferError::invalid("journal_corrupt", "metadata fragment length mismatch")); }
        Ok(bytes)
    }

    async fn begin_control(&self, row: &SessionRow, op: &ControlOp, kind: &str, data: &[u8]) -> Result<Option<SessionStatus>, TransferError> {
        if op.operation_id.0.is_empty() || op.operation_id.0.len() > 128 { return Err(TransferError::invalid("invalid_operation", "operation_id must be 1..128 bytes")); }
        let fingerprint = sha256_hex(&serde_json::to_vec(&(kind, op.if_match_revision, sha256_hex(data))).map_err(TransferError::platform)?);
        if let Some(existing) = self.journal.operation(&row.session_id, &op.operation_id).await? {
            if existing.request_fingerprint != fingerprint { return Err(TransferError::OperationConflict); }
            if let Some(bytes) = existing.result { return serde_json::from_slice(&bytes).map(Some).map_err(TransferError::platform); }
            if row.last_operation_id.as_ref() == Some(&op.operation_id)
                && let Some(bytes) = &row.last_operation_result
            {
                let mut done = existing; done.result = Some(bytes.clone()); done.completed = true;
                self.journal.finish_operation(&done).await?;
                return serde_json::from_slice(bytes).map(Some).map_err(TransferError::platform);
            }
        }
        if row.revision != op.if_match_revision { return Err(TransferError::RevisionMismatch { current: row.revision }); }
        let claim=OperationRow { session_id: row.session_id.clone(), operation_id: op.operation_id.clone(),kind:kind.to_string(),request_fingerprint:fingerprint.clone(),if_match_revision:Some(op.if_match_revision),result:None,completed:false,created_at:self.clock.now_utc() };
        self.journal.begin_operation(&claim).await?;
        let latest=self.journal.session(&row.scope,&row.session_id).await?.ok_or(TransferError::SessionNotFound)?;self.reconcile_last_operation(&latest).await?;
        let claimed=self.journal.operation(&row.session_id,&op.operation_id).await?.ok_or(TransferError::OperationConflict)?;
        if claimed.request_fingerprint!=fingerprint{self.journal.finish_operation(&claimed).await?;return Err(TransferError::OperationConflict);}
        if let Some(result)=&claimed.result{self.journal.finish_operation(&claimed).await?;return serde_json::from_slice(result).map(Some).map_err(TransferError::platform);}
        if latest.revision!=row.revision||latest.revision!=op.if_match_revision{self.journal.finish_operation(&claimed).await?;return Err(TransferError::RevisionMismatch{current:latest.revision});}Ok(None)
    }

    async fn reconcile_last_operation(&self, row: &SessionRow) -> Result<(), TransferError> {
        if let (Some(id), Some(result)) = (&row.last_operation_id, &row.last_operation_result)
            && let Some(mut receipt) = self.journal.operation(&row.session_id, id).await? {
            if receipt.result.as_ref().is_some_and(|saved| saved != result) {
                return Err(TransferError::RecoveryRequired { message: "immutable operation receipt conflicts with session CAS".into() });
            }
            if !receipt.completed || receipt.result.is_none() {
                receipt.result = Some(result.clone());
                receipt.completed = true;
                self.journal.finish_operation(&receipt).await?;
            }
        }
        Ok(())
    }

    async fn finish_control(&self, row: &mut SessionRow, op: &ControlOp) -> Result<SessionStatus, TransferError> {
        let mut next = row.clone();
        next.revision = row.revision.checked_add(1).ok_or_else(|| TransferError::invalid("revision_overflow", "revision exhausted"))?;
        next.updated_at = self.clock.now_utc(); next.expires_at = next.updated_at + chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);
        let status = self.status_of(&next);
        let result = serde_json::to_vec(&status).map_err(TransferError::platform)?;
        next.last_operation_id = Some(op.operation_id.clone()); next.last_operation_result = Some(result.clone());
        self.journal.update_session(&next, row.revision).await?; *row = next;
        let mut receipt = self.journal.operation(&row.session_id, &op.operation_id).await?.ok_or(TransferError::OperationConflict)?;
        receipt.result = Some(result); receipt.completed = true; self.journal.finish_operation(&receipt).await?;
        Ok(status)
    }

    fn schedule(&self, row: &mut SessionRow) { row.next_action = NextAction::Advance; row.operation_id = Some(OperationId(self.backend.opaque_id("op"))); }

    pub async fn put_manifest(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], records: Vec<BatchRecord>) -> Result<BatchResult, TransferError> {
        self.receive_records(scope, id, secret, records, true).await
    }

    pub async fn put_records(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], records: Vec<BatchRecord>) -> Result<BatchResult, TransferError> {
        self.receive_records(scope, id, secret, records, false).await
    }

    async fn receive_records(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], records: Vec<BatchRecord>, manifest: bool) -> Result<BatchResult, TransferError> {
        self.load_session(scope,id,Some(secret)).await?;let mut claim=OperationRow{session_id:id.clone(),operation_id:OperationId(self.backend.opaque_id("upload")),kind:"upload".into(),request_fingerprint:String::new(),if_match_revision:None,result:None,completed:false,created_at:self.clock.now_utc()};self.journal.begin_operation(&claim).await?;
        let result=self.receive_claimed_records(scope,id,secret,records,manifest).await;claim.completed=true;self.journal.finish_operation(&claim).await?;result
    }

    async fn receive_claimed_records(&self,scope:&AuthScope,id:&SessionId,secret:&[u8],records:Vec<BatchRecord>,manifest:bool)->Result<BatchResult,TransferError>{
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        self.reconcile_last_operation(&row).await?;
        if row.direction != TransferDirection::Import || (manifest && row.state != SessionState::ReceivingManifest) || (!manifest && row.state != SessionState::Transferring) {
            // Identical record retries remain valid after seal/ready: verify receipts below.
            if !records.iter().all(|r| r.file_id == MANIFEST_FILE_ID && manifest || r.file_id != MANIFEST_FILE_ID && !manifest) { return Err(invalid_state(&row, "import upload")); }
        }
        if records.is_empty() || records.len() > MAX_BATCH_RECORDS { return Err(TransferError::invalid("invalid_batch", "batch must contain 1..100 records")); }
        let mut result = BatchResult { receipts: Vec::new(), failure: None, status: self.status_of(&row) };
        let mut total = 0usize;
        for record in records {
            total = total.checked_add(record.payload.len()).ok_or_else(|| TransferError::invalid("transfer_chunk_too_large", "batch overflow"))?;
            if total > MAX_BINARY_BODY_BYTES { return Err(TransferError::invalid("transfer_chunk_too_large", "batch exceeds binary budget")); }
            match self.receive_one(&row, &record, manifest).await {
                Ok(outcome) => { result.receipts.push(outcome); row = self.load_session(scope, id, Some(secret)).await?; }
                Err(error) => { result.failure = Some(FailedRecord { file_id: record.file_id, chunk_index: record.chunk_index, code: error.code().into(), message: error.message() }); break; }
            }
        }
        result.status = self.status_of(&self.load_session(scope, id, Some(secret)).await?); Ok(result)
    }

    async fn receive_one(&self, row: &SessionRow, record: &BatchRecord, manifest: bool) -> Result<ChunkOutcome, TransferError> {
        use sha2::Digest;
        if (record.file_id == MANIFEST_FILE_ID) != manifest { return Err(TransferError::invalid("invalid_file", "record belongs to another upload endpoint")); }
        let digest = sha256_hex(&record.payload);
        let declared = record.sha256.ok_or_else(|| TransferError::invalid("sha256_required", "upload records require sha256"))?;
        if sha256_from_hex(&digest) != Some(declared) { return Err(TransferError::invalid("sha256_mismatch", "payload hash mismatch")); }
        let mut file = if let Some(file) = self.journal.file(&row.session_id, record.file_id).await? { file } else if manifest && row.state == SessionState::ReceivingManifest {
            let ext = import_ext(row).ok_or_else(|| invalid_state(row, "import"))?;
            let file = FileRow { session_id: row.session_id.clone(), file_id: MANIFEST_FILE_ID, novel_ordinal: u32::MAX, logical_path: String::new(), role: FileRole::Metadata, size: ext.manifest_size, sha256: ext.manifest_sha256.clone(), state: FileState::Pending, confirmed_bytes: 0, next_chunk_index: 0, confirmed_prefix_sha256: Some(sha256_hex(&[])), hash_checkpoint: Some(sha256_checkpoint(&sha2::Sha256::new())), stage: None, pin: None, finalize_operation: None };
            self.journal.put_file(&file).await?; file
        } else { return Err(TransferError::invalid("invalid_file", "unknown file ordinal")); };
        file = self.restart_corrupt_file(file).await?;
        if let Some(receipt) = self.journal.chunk(&row.session_id, record.file_id, record.chunk_index).await? {
            if receipt.stored_sha256 != digest || receipt.payload_size != record.payload.len() as u64 || receipt.is_final != record.is_final { return Err(TransferError::ChunkConflict { code: "chunk_conflict", message: "chunk identity already has different bytes or finality".into() }); }
            return Ok(ChunkOutcome { file_id: record.file_id, chunk_index: record.chunk_index, outcome: "duplicate".into(), confirmed_bytes: receipt.offset + receipt.payload_size, confirmed_prefix_sha256: receipt.prefix_sha256 });
        }
        if row.direction != TransferDirection::Import || row.state != if manifest { SessionState::ReceivingManifest } else { SessionState::Transferring } { return Err(invalid_state(row, "receiving upload")); }
        if !matches!(file.state, FileState::Pending | FileState::Receiving) { return Err(TransferError::invalid("file_not_uploadable", "file is not awaiting bytes")); }
        let offset = u64::from(record.chunk_index).checked_mul(row.chunk_bytes).ok_or_else(|| TransferError::invalid("unexpected_offset", "chunk offset overflow"))?;
        if record.chunk_index != file.next_chunk_index || offset != file.confirmed_bytes { return Err(TransferError::ChunkConflict { code: "unexpected_offset", message: format!("expected chunk {} at {}", file.next_chunk_index, file.confirmed_bytes) }); }
        let end = offset.checked_add(record.payload.len() as u64).ok_or_else(|| TransferError::invalid("unexpected_offset", "size overflow"))?;
        if end > file.size || record.payload.len() as u64 > row.chunk_bytes || record.is_final != (end == file.size) || (!record.is_final && record.payload.len() as u64 != row.chunk_bytes) { return Err(TransferError::invalid("invalid_chunk_length", "chunk size/finality differs from declared file")); }
        let mut hash = if file.confirmed_bytes == 0 { sha2::Sha256::new() } else { file.hash_checkpoint.as_deref().and_then(sha256_resume).ok_or_else(|| TransferError::invalid("hash_checkpoint_corrupt", "file must restart"))? };
        hash.update(&record.payload); let prefix = hex::encode(hash.clone().finalize());
        if record.is_final && prefix != file.sha256 { return Err(TransferError::invalid("file_sha256_mismatch", "whole-file SHA differs from sealed manifest")); }
        let handle = self.backend.stage_begin(&row.session_id, file.file_id, file.size).await?;
        self.backend.stage_append(&handle, offset, &record.payload).await?;
        let finalize = record.is_final && self.backend.stage_needs_finalize();
        file.stage = Some(if record.is_final && !finalize { self.backend.stage_seal(&handle, file.size, &file.sha256).await? } else { handle });
        file.state = if finalize { FileState::Finalizing } else if record.is_final { FileState::Verified } else { FileState::Receiving };
        file.finalize_operation = finalize.then(|| OperationId(self.backend.opaque_id("finalize")));
        file.confirmed_bytes = end; file.next_chunk_index = file.next_chunk_index.checked_add(1).ok_or_else(|| TransferError::invalid("chunk_index_overflow", "chunk count exceeds protocol"))?;
        file.confirmed_prefix_sha256 = Some(prefix.clone()); file.hash_checkpoint = Some(sha256_checkpoint(&hash));
        let receipt = ChunkRow { session_id: row.session_id.clone(), file_id: file.file_id, chunk_index: record.chunk_index, payload_size: record.payload.len() as u64, declared_sha256: Some(digest.clone()), stored_sha256: digest, prefix_sha256: prefix.clone(), hash_checkpoint: file.hash_checkpoint.clone(), offset, is_final: record.is_final };
        self.journal.commit_chunk(&receipt, &file, row.confirmed_bytes.checked_add(receipt.payload_size).ok_or_else(|| TransferError::invalid("size_overflow", "confirmed bytes overflow"))?).await?;
        Ok(ChunkOutcome { file_id: file.file_id, chunk_index: record.chunk_index, outcome: if finalize { "finalizing" } else if record.is_final { "sealed" } else { "accepted" }.into(), confirmed_bytes: end, confirmed_prefix_sha256: prefix })
    }

    async fn restart_corrupt_file(&self, mut file: FileRow) -> Result<FileRow, TransferError> {
        use sha2::Digest;
        if !matches!(file.state, FileState::Pending | FileState::Receiving | FileState::Failed) { return Ok(file); }
        let valid = if file.confirmed_bytes == 0 { file.confirmed_prefix_sha256.as_deref() == Some(sha256_hex(&[]).as_str()) } else {
            file.hash_checkpoint.as_deref().and_then(sha256_resume).is_some_and(|hash| file.confirmed_prefix_sha256.as_deref() == Some(hex::encode(hash.finalize()).as_str()))
        };
        if valid && file.state != FileState::Failed { return Ok(file); }
        let retired = file.stage.take(); file.pin = None; file.confirmed_bytes = 0; file.next_chunk_index = 0; file.state = FileState::Pending; file.finalize_operation = None;
        file.confirmed_prefix_sha256 = Some(sha256_hex(&[])); file.hash_checkpoint = Some(sha256_checkpoint(&sha2::Sha256::new()));
        self.journal.reset_file(&file).await?;
        if let Some(stage) = retired { self.backend.stage_abort(&file.session_id, &stage).await?; }
        Ok(file)
    }

    async fn finalize_file_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        let current=self.journal.finalizing_files(&row.session_id,1).await?.into_iter().next();
        let file=self.advance_input(row,"finalizing_file",current).await?;
        budget.records+=1;
        if let Some(mut file) = file {
            let handle = file.stage.as_deref().ok_or_else(|| TransferError::invalid("stage_missing", "finalizing file has no durable stage"))?;
            let sealed = self.backend.stage_seal(handle, file.size, &file.sha256).await?;
            let step = self.backend.stage_finalize(&sealed, None).await?; budget.bytes += step.processed_bytes;
            if step.done { file.stage = Some(sealed); file.state = FileState::Verified; file.finalize_operation = None; self.journal.update_file(&file).await?; }
            row.next_action = NextAction::FinalizeFile;
        } else {
            row.next_action=NextAction::Upload;
        }
        Ok(())
    }

    pub async fn seal_manifest(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp, size: u64, sha256: &str) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        let data = serde_json::to_vec(&(size, sha256)).map_err(TransferError::platform)?;
        if let Some(status) = self.begin_control(&row, op, "seal", &data).await? { return Ok(status); }
        if row.state != SessionState::ReceivingManifest { return Err(invalid_state(&row, "receiving_manifest")); }
        let file = self.journal.file(id, MANIFEST_FILE_ID).await?.ok_or_else(|| TransferError::invalid("manifest_incomplete", "manifest bytes have not been received"))?;
        let ext = import_ext_mut(&mut row)?;
        if file.state != FileState::Verified || size != ext.manifest_size || sha256 != ext.manifest_sha256 || size != file.confirmed_bytes { return Err(TransferError::invalid("manifest_incomplete", "manifest size or SHA does not match create request")); }
        ext.manifest_stage = file.stage; ext.manifest_confirmed = size; row.state = SessionState::Planning; self.schedule(&mut row);
        self.finish_control(&mut row, op).await
    }

    pub async fn files(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], cursor: Option<&str>) -> Result<FilePage, TransferError> {
        let row = self.load_session(scope, id, Some(secret)).await?;
        let mut page = self.journal.files(id, cursor, MAX_PAGE_ITEMS).await?;
        if row.direction == TransferDirection::Import { for file in &mut page.items { *file = self.restart_corrupt_file(file.clone()).await?; } }
        let items = page.items.into_iter().filter(|f| f.file_id != MANIFEST_FILE_ID).map(|f| FileStatus { file_id: f.file_id, manifest_file_index: f.file_id, novel_ordinal: f.novel_ordinal, logical_path: f.logical_path, role: f.role, size: f.size, sha256: f.sha256, upload_required: row.state == SessionState::Transferring && matches!(f.state, FileState::Pending | FileState::Receiving | FileState::Failed), state: f.state, confirmed_bytes: f.confirmed_bytes, next_chunk_index: f.next_chunk_index, confirmed_prefix_sha256: f.confirmed_prefix_sha256 }).collect();
        bounded_page(Page { items, next_cursor: page.next_cursor })
    }

    pub async fn plan(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], cursor: Option<&str>) -> Result<PlanPage, TransferError> {
        let row = self.load_session(scope, id, Some(secret)).await?;
        let page = self.journal.plan_entries(id, cursor, MAX_PAGE_ITEMS).await?;
        let page = bounded_page(page)?;
        Ok(PlanPage { plan_revision: row.plan_revision.unwrap_or_default(), items: page.items, next_cursor: page.next_cursor })
    }

    pub async fn catalog(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], cursor: Option<&str>) -> Result<CatalogPage, TransferError> {
        let row = self.load_session(scope, id, Some(secret)).await?;
        if row.direction != TransferDirection::Export { return Err(invalid_state(&row, "export")); }
        bounded_page(self.journal.catalog_entries(id, cursor, MAX_PAGE_ITEMS).await?)
    }

    pub async fn ready(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        if let Some(status) = self.begin_control(&row, op, "ready", &[]).await? { return Ok(status); }
        if row.state != SessionState::Transferring { return Err(invalid_state(&row, "transferring")); }
        if !self.journal.pending_files(id, 1).await?.is_empty() { return Err(TransferError::invalid("files_incomplete", "required files are not all verified")); }
        row.state = SessionState::Ready; row.next_action = NextAction::Confirm; row.operation_id = None;
        self.finish_control(&mut row, op).await
    }

    pub async fn publish(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp, plan_revision: &str) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        if let Some(status) = self.begin_control(&row, op, "publish", plan_revision.as_bytes()).await? { return Ok(status); }
        if row.state != SessionState::Ready { return Err(invalid_state(&row, "ready")); }
        if row.plan_revision.as_deref() != Some(plan_revision) { return Err(TransferError::PlanStale { message: "plan revision changed; review the current plan".into() }); }
        row.state = SessionState::Publishing; self.schedule(&mut row); self.finish_control(&mut row, op).await
    }

    pub async fn pause(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        if let Some(status) = self.begin_control(&row, op, "pause", &[]).await? { return Ok(status); }
        if row.state.is_terminal() || row.state == SessionState::Cancelling { return Err(invalid_state(&row, "active session")); }
        if row.state != SessionState::Paused { row.resume_phase = Some(row.state); row.state = SessionState::Paused; }
        if row.resume_phase == Some(SessionState::Publishing) { self.schedule(&mut row); } else { row.next_action = NextAction::None; row.operation_id = None; }
        self.finish_control(&mut row, op).await
    }

    pub async fn resume(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        if let Some(status) = self.begin_control(&row, op, "resume", &[]).await? { return Ok(status); }
        if row.state != SessionState::Paused { return Err(invalid_state(&row, "paused")); }
        row.state = row.resume_phase.take().ok_or_else(|| TransferError::invalid("invalid_state", "resume phase missing"))?;
        row.next_action = match row.state { SessionState::ReceivingManifest | SessionState::Transferring => NextAction::Upload, SessionState::Selecting => NextAction::Select, SessionState::Ready if row.direction == TransferDirection::Import => NextAction::Confirm, SessionState::Ready | SessionState::Reading => NextAction::Read, _ => NextAction::Advance };
        if row.next_action == NextAction::Advance { self.schedule(&mut row); } else { row.operation_id = None; }
        self.finish_control(&mut row, op).await
    }

    pub async fn delete_session(&self, scope: &AuthScope, id: &SessionId, secret: Option<&[u8]>, op: &ControlOp) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, secret).await?;
        if let Some(status) = self.begin_control(&row, op, "delete", &[]).await? { return Ok(status); }
        if !row.state.is_terminal() { row.resume_phase = Some(row.resume_phase.unwrap_or(row.state)); row.state = SessionState::Cancelling; self.schedule(&mut row); }
        self.finish_control(&mut row, op).await
    }

    pub async fn complete(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        if let Some(status) = self.begin_control(&row, op, "complete", &[]).await? { return Ok(status); }
        if row.direction != TransferDirection::Export || !matches!(row.state, SessionState::Ready | SessionState::Reading) { return Err(invalid_state(&row, "readable export")); }
        row.state = SessionState::Released; self.schedule(&mut row); self.finish_control(&mut row, op).await
    }
}

fn bounded_page<T: Serialize>(page: Page<T>) -> Result<Page<T>, TransferError> {
    if serde_json::to_vec(&page).map_err(TransferError::platform)?.len() > MAX_PAGE_BYTES {
        return Err(TransferError::invalid("page_too_large", "journal adapter must page by both item and encoded byte budgets"));
    }
    Ok(page)
}


impl TransferService {
    async fn parse_manifest_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        use crate::transfer::protocol::{WireRecord, WIRE_RECORD_MAX_BYTES, TEXT_FRAGMENT_MAX_BYTES};
        let ext = import_ext(row).ok_or_else(|| invalid_state(row, "import"))?.clone();
        let mut checkpoint: ManifestCheckpoint = ext.graph_cursor.as_deref().map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();
        let stage = ext.manifest_stage.as_ref().ok_or_else(|| TransferError::invalid("manifest_missing", "sealed manifest is missing"))?;
        let end = ext.manifest_cursor.saturating_add(ADVANCE_BYTE_BUDGET).min(ext.manifest_size);
        let bytes = self.backend.stage_read_range(stage, ext.manifest_cursor..end).await?;
        if bytes.len() as u64 != end - ext.manifest_cursor { return Err(TransferError::invalid("manifest_short_read", "sealed manifest was truncated")); }
        let mut consumed = 0usize;
        for raw in bytes.split_inclusive(|b| *b == b'\n') {
            if budget.records == ADVANCE_ITEM_BUDGET { break; }
            if raw.last() != Some(&b'\n') {
                if raw.len() >= WIRE_RECORD_MAX_BYTES || end == ext.manifest_size { return Err(TransferError::invalid("manifest_record_truncated", "wire record must terminate with newline and fit 256 KiB")); }
                break;
            }
            if raw.len() > WIRE_RECORD_MAX_BYTES || checkpoint.ended { return Err(TransferError::invalid("invalid_manifest", "oversize record or bytes after end")); }
            let record: WireRecord = serde_json::from_slice(raw).map_err(|e| TransferError::invalid("invalid_manifest", e.to_string()))?;
            match record {
                WireRecord::Header(header) => {
                    if checkpoint.header.is_some() || checkpoint.novels != 0 || header.protocol_version != PROTOCOL_VERSION || !matches!(header.format_version, 1 | 2) || header.policy_version != row.policy_version || header.profile != row.profile || header.extra_roles != row.extra_roles || header.fingerprint != ext.source_fingerprint || header.novels > 10_000 || header.files > 49_999 { return Err(TransferError::invalid("invalid_manifest_header", "manifest header does not match the admitted session")); }
                    checkpoint.projection_hash = Some(crate::transfer::projection::ProjectionDigest::new(header.policy_version, header.profile, &header.extra_roles, &header.fingerprint).map_err(TransferError::platform)?.checkpoint());
                    checkpoint.header = Some(header);
                }
                WireRecord::Novel(novel) => {
                    self.check_novel_end(&row.session_id, &checkpoint).await?;
                    if checkpoint.header.is_none() || novel.index != checkpoint.novels || checkpoint.novels >= 10_000 || checkpoint.open_text.is_some() { return Err(TransferError::invalid("invalid_novel_order", "novel ordinals must be dense and text fragments complete")); }
                    let mut digest = crate::transfer::projection::ProjectionDigest::from_checkpoint(checkpoint.projection_hash.as_deref().ok_or_else(|| TransferError::invalid("invalid_manifest", "selection hash missing"))?).map_err(TransferError::platform)?;
                    digest.novel(novel.index); checkpoint.projection_hash = Some(digest.checkpoint());
                    self.save_value(&row.session_id, &format!("{JKEY_NOVEL}{}", novel.index), &novel).await?;
                    self.save_value(&row.session_id, &format!("file_start:{}", novel.index), &checkpoint.files).await?;
                    checkpoint.current = Some(novel.index); checkpoint.current_files = 0; checkpoint.current_bytes = 0; checkpoint.scalar_fields=0;checkpoint.tags=0;checkpoint.members=0;checkpoint.title_nonempty=false;checkpoint.url_nonempty=false;checkpoint.novels += 1;
                }
                WireRecord::Text(text) => {
                    if checkpoint.current != Some(text.novel) || checkpoint.current_files != 0 || !crate::transfer::protocol::is_text_field(&text.field) || text.value.len() > TEXT_FRAGMENT_MAX_BYTES || ((text.field == "tag" || text.field == "member") != text.item.is_some()) { return Err(TransferError::invalid("invalid_text", "invalid text owner, field, order, item or fragment size")); }
                    let scalar=metadata_scalar_field(&text.field);
                    let completed=match text.field.as_str(){"tag"=>text.item!=Some(checkpoint.tags),"member"=>text.item!=Some(checkpoint.members),_=>scalar.is_none_or(|field|checkpoint.scalar_fields&(1<<field)!=0)};
                    if completed || match &checkpoint.open_text { Some((field, item, part)) => field != &text.field || item != &text.item || *part != text.part, None => text.part != 0 } { return Err(TransferError::invalid("invalid_text_sequence", "text fragments must be consecutive, unique and explicitly terminated")); }
                    if text.field=="title"&&!text.value.trim().is_empty(){checkpoint.title_nonempty=true;}
                    if text.field=="toc_url"&&!text.value.is_empty(){checkpoint.url_nonempty=true;}
                    let key = format!("{JKEY_TEXT}{}:{}:{}:{}", text.novel, text.field, text.item.unwrap_or(0), text.part);
                    self.journal.put_journal_entry(&row.session_id, SESSION_JOURNAL_PUB, &key, text.value.as_bytes()).await?;
                    if text.last {
                        self.save_value(&row.session_id, &format!("field:{}:{}:{}", text.novel, text.field, text.item.unwrap_or(0)), &text.part.checked_add(1).ok_or_else(||TransferError::invalid("invalid_text_sequence","fragment index overflow"))?).await?;
                        match text.field.as_str(){"tag"=>checkpoint.tags=checkpoint.tags.checked_add(1).ok_or_else(||TransferError::invalid("metadata_budget","too many tags"))?,"member"=>checkpoint.members=checkpoint.members.checked_add(1).ok_or_else(||TransferError::invalid("metadata_budget","too many members"))?,_=>checkpoint.scalar_fields|=1<<scalar.ok_or_else(||TransferError::invalid("invalid_text","unknown scalar"))?};checkpoint.open_text=None;
                    } else { checkpoint.open_text = Some((text.field, text.item, text.part.checked_add(1).ok_or_else(|| TransferError::invalid("invalid_text_sequence", "fragment index overflow"))?)); }
                    checkpoint.metadata_bytes = checkpoint.metadata_bytes.checked_add(text.value.len() as u64 + 64).ok_or_else(|| TransferError::invalid("metadata_budget", "metadata size overflow"))?;
                    checkpoint.texts += 1;
                }
                WireRecord::File(file) => {
                    if checkpoint.current != Some(file.novel) || file.file_id != checkpoint.files || checkpoint.files >= 49_999 || checkpoint.open_text.is_some() || sha256_from_hex(&file.sha256).is_none() { return Err(TransferError::invalid("invalid_file_order", "invalid file ordinal, owner or hash")); }
                    crate::transfer::validate_relative_path(&file.path).map_err(|e| TransferError::invalid("invalid_path", e.to_string()))?;
                    let novel: crate::transfer::protocol::WireNovel = self.value(&row.session_id, &format!("{JKEY_NOVEL}{}", file.novel)).await?.ok_or_else(|| TransferError::invalid("invalid_novel", "file owner missing"))?;
                    let role = crate::transfer::projection::classify(&file.path, novel.kind == NovelKind::VirtualParent, &ClassifyContext::default());
                    let caps = self.backend.storage_capabilities(&row.scope).await?;
                    let extras: Vec<&str> = row.extra_roles.iter().map(String::as_str).collect();
                    if role == FileRole::Internal || caps.unpersistable_roles.contains(&role) || !decision_selected(crate::transfer::projection::send_decision(row.profile, role, &extras)) { return Err(TransferError::invalid("file_policy_violation", format!("{} is excluded by the confirmed projection", file.path))); }
                    if novel.kind == NovelKind::VirtualParent && !crate::transfer::is_virtual_input_file(&file.path) { return Err(TransferError::invalid("invalid_parent_file", "virtual parent carries non-input data")); }
                    if caps.backend_limits.get("max_file_bytes").and_then(|v| v.parse::<u64>().ok()).is_some_and(|max| file.size > max) { return Err(TransferError::invalid("backend_file_limit", "declared file exceeds the destination capacity")); }
                    if file.size.div_ceil(CHUNK_BYTES)>u64::from(u32::MAX){return Err(TransferError::invalid("chunk_index_overflow","declared file exceeds representable chunk count"));}
                    let mut digest = crate::transfer::projection::ProjectionDigest::from_checkpoint(checkpoint.projection_hash.as_deref().ok_or_else(|| TransferError::invalid("invalid_manifest", "selection hash missing"))?).map_err(TransferError::platform)?;
                    digest.file(&file.path, file.size, &file.sha256).map_err(TransferError::platform)?; checkpoint.projection_hash = Some(digest.checkpoint());
                    let pathkey = format!("path:{}:{}", file.novel, sha256_hex(file.path.as_bytes()));
                    if let Some(previous) = self.value::<u32>(&row.session_id, &pathkey).await? && previous != file.file_id { return Err(TransferError::invalid("duplicate_path", "novel contains duplicate file paths")); }
                    self.save_value(&row.session_id, &pathkey, &file.file_id).await?;
                    self.journal.put_file(&FileRow { session_id: row.session_id.clone(), file_id: file.file_id, novel_ordinal: file.novel, logical_path: file.path.clone(), role, size: file.size, sha256: file.sha256, state: FileState::Pending, confirmed_bytes: 0, next_chunk_index: 0, confirmed_prefix_sha256: Some(sha256_hex(&[])), hash_checkpoint: None, stage: None, pin: None, finalize_operation: None }).await?;
                    checkpoint.files += 1; checkpoint.current_files += 1; checkpoint.current_bytes = checkpoint.current_bytes.checked_add(file.size).ok_or_else(|| TransferError::invalid("size_overflow", "novel size overflow"))?; checkpoint.metadata_bytes += file.path.len() as u64 + 256;
                }
                WireRecord::End(end) => {
                    self.check_novel_end(&row.session_id, &checkpoint).await?;
                    let header = checkpoint.header.as_ref().ok_or_else(|| TransferError::invalid("invalid_manifest", "header missing"))?;
                    if (end.novels, end.files, end.texts) != (checkpoint.novels, checkpoint.files, checkpoint.texts) || (header.novels, header.files, header.texts) != (end.novels, end.files, end.texts) || checkpoint.open_text.is_some() { return Err(TransferError::invalid("manifest_count_mismatch", "manifest ended before all declared records were complete")); }
                    let digest = crate::transfer::projection::ProjectionDigest::from_checkpoint(checkpoint.projection_hash.as_deref().ok_or_else(|| TransferError::invalid("invalid_manifest", "selection hash missing"))?).map_err(TransferError::platform)?.finish();
                    if !digest.eq_ignore_ascii_case(&header.selection_digest) { return Err(TransferError::invalid("selection_digest_mismatch", "projected manifest identities differ from the confirmed selection")); }
                    checkpoint.ended = true;
                }
            }
            if checkpoint.metadata_bytes > MAX_METADATA_BYTES as u64 { return Err(TransferError::invalid("metadata_budget", "decoded transfer metadata exceeds 24 MiB")); }
            consumed += raw.len(); budget.records += 1;
        }
        budget.bytes += bytes.len() as u64;
        let ext = import_ext_mut(row)?;
        ext.manifest_cursor += consumed as u64; ext.catalogued_files = checkpoint.files as u64; ext.catalogued_novels = checkpoint.novels;
        ext.graph_cursor = Some(serde_json::to_string(&checkpoint).map_err(TransferError::platform)?);
        if ext.manifest_cursor == ext.manifest_size {
            if !checkpoint.ended { return Err(TransferError::invalid("manifest_incomplete", "wire end record missing")); }
            ext.planning_phase = 1; ext.planning_ordinal = 0; ext.graph_cursor = None;
        }
        Ok(())
    }

    async fn check_novel_end(&self, id: &SessionId, state: &ManifestCheckpoint) -> Result<(), TransferError> {
        if state.open_text.is_some() { return Err(TransferError::invalid("incomplete_text", "text field has no last fragment")); }
        if let Some(ordinal) = state.current {
            let novel: crate::transfer::protocol::WireNovel = self.value(id, &format!("{JKEY_NOVEL}{ordinal}")).await?.ok_or_else(|| TransferError::invalid("invalid_novel", "novel metadata missing"))?;
            if novel.files != state.current_files || state.scalar_fields&0xf!=0xf || !state.title_nonempty || !state.url_nonempty { return Err(TransferError::invalid("incomplete_novel", "novel files or required metadata missing")); }
            if novel.kind==NovelKind::Normal&&state.members!=0{return Err(TransferError::invalid("invalid_members","ordinary novel cannot carry members"));}
            if novel.kind==NovelKind::VirtualParent&&state.members==0{return Err(TransferError::invalid("invalid_members","virtual parent requires ordinary members"));}
            let layout=crate::transfer::NovelJsonLayout{novel_type:novel.novel_type,optional:state.scalar_fields,tags:state.tags,members:(novel.kind==NovelKind::VirtualParent).then_some(state.members)};layout.fields().map_err(TransferError::platform)?;
            self.save_value(id, &format!("wire_layout:{ordinal}"), &layout).await?;
            self.save_value(id, &format!("novel_bytes:{ordinal}"), &state.current_bytes).await?;
        }
        Ok(())
    }


    async fn planning_step(&self,row:&mut SessionRow,budget:&mut AdvanceBudget)->Result<(),TransferError>{
        let ext=import_ext(row).ok_or_else(||invalid_state(row,"import"))?.clone();
        if ext.planning_phase==0{return self.parse_manifest_step(row,budget).await;}
        if ext.planning_phase==5{
            let mut file_id=ext.planning_ordinal;let mut actions=BTreeMap::new();
            while (file_id as u64)<ext.catalogued_files&&budget.records<ADVANCE_ITEM_BUDGET{
                let mut file=self.journal.file(&row.session_id,file_id).await?.ok_or_else(||TransferError::invalid("file_missing","planned file row missing"))?;
                let action=if let Some(action)=actions.get(&file.novel_ordinal){*action}else{let action:PlanAction=self.value(&row.session_id,&format!("action:{}",file.novel_ordinal)).await?.ok_or_else(||TransferError::invalid("plan_missing","file plan action missing"))?;actions.insert(file.novel_ordinal,action);action};
                if matches!(action,PlanAction::Skip|PlanAction::Reject){file.state=FileState::Skipped;self.journal.update_file(&file).await?;}
                else if !matches!(file.state,FileState::Verified|FileState::Reused)&&let Some(pin)=self.backend.find_reusable(&row.scope,row,&file).await?{if pin.size!=file.size||pin.sha256!=file.sha256{return Err(TransferError::invalid("reuse_identity_mismatch","backend returned wrong immutable reuse identity"));}file.state=FileState::Reused;file.pin=Some(pin.pin);file.confirmed_bytes=file.size;file.confirmed_prefix_sha256=Some(file.sha256.clone());self.journal.update_file(&file).await?;}
                file_id+=1;budget.records+=1;
            }
            import_ext_mut(row)?.planning_ordinal=file_id;
            if file_id as u64==ext.catalogued_files{row.state=SessionState::Transferring;row.next_action=NextAction::Upload;row.operation_id=None;row.error=None;}
            return Ok(());
        }
        if ext.planning_ordinal>=ext.catalogued_novels{let next=import_ext_mut(row)?;next.planning_phase+=1;next.planning_ordinal=0;next.graph_cursor=None;return Ok(());}
        let ordinal=ext.planning_ordinal;let key=format!("metadata:{ordinal}");
        match ext.planning_phase{
            1=>{
                let layout:crate::transfer::NovelJsonLayout=self.value(&row.session_id,&format!("wire_layout:{ordinal}")).await?.ok_or_else(||TransferError::invalid("metadata_missing","normalized wire layout missing"))?;
                let mut cursor:WireMetadataCursor=ext.graph_cursor.as_deref().map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();
                if !self.copy_wire_metadata_step(row,&key,ordinal,&layout,&mut cursor,budget).await?{import_ext_mut(row)?.graph_cursor=Some(serde_json::to_string(&cursor).map_err(TransferError::platform)?);return Ok(());}
                let url=self.metadata_text(&row.session_id,&key,0,8192).await?;let urlkey=format!("url:{}",sha256_hex(url.as_bytes()));
                if let Some(previous)=self.value::<u32>(&row.session_id,&urlkey).await?&&previous!=ordinal{return Err(TransferError::invalid("duplicate_novel_url","manifest repeats toc_url"));}
                self.save_value(&row.session_id,&urlkey,&ordinal).await?;
                let wire:crate::transfer::protocol::WireNovel=self.value(&row.session_id,&format!("{JKEY_NOVEL}{ordinal}")).await?.ok_or_else(||TransferError::invalid("novel_missing","normalized novel missing"))?;
                if wire.kind==NovelKind::Normal&&self.value::<u32>(&row.session_id,&format!("path:{ordinal}:{}",sha256_hex(b"toc.yaml"))).await?.is_none(){return Err(TransferError::invalid("required_file_missing","ordinary novel must include toc.yaml"));}import_ext_mut(row)?.graph_cursor=None;
            }
            2=>{
                let caps=self.backend.storage_capabilities(&row.scope).await?;let max_url=caps.backend_limits.get("max_indexed_url_bytes").and_then(|v|v.parse::<usize>().ok()).unwrap_or(8192);self.metadata_text(&row.session_id,&key,0,max_url).await?;
                if let Some(limit)=caps.backend_limits.get("max_record_bytes").and_then(|v|v.parse::<u64>().ok()){
                    let mut layout:crate::transfer::NovelJsonLayout=self.value(&row.session_id,&key).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata layout missing"))?;layout.optional&=!(1<<7);layout.members=None;
                    let mut measure:MetadataMeasure=ext.graph_cursor.as_deref().map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();let mut cached:Option<(crate::transfer::NovelJsonRequest,Vec<u8>,bool)>=None;
                    while budget.records<ADVANCE_ITEM_BUDGET&&budget.bytes+crate::transfer::protocol::IO_CHUNK_BYTES as u64<=ADVANCE_BYTE_BUDGET{
                        if let Some(request)=measure.request&&cached.as_ref().is_none_or(|(key,_,_)|*key!=request){let(bytes,last)=self.metadata_input(&row.session_id,&key,request).await?;cached=Some((request,bytes,last));}
                        let input=cached.as_ref().map(|(request,bytes,last)|std::str::from_utf8(bytes).map(|text|crate::transfer::NovelJsonText{field:request.field,part:request.part,text,last:*last})).transpose().map_err(TransferError::platform)?;
                        let output=crate::transfer::novel_json_pull(&layout,&mut measure.cursor,input,crate::transfer::protocol::IO_CHUNK_BYTES).map_err(TransferError::platform)?;measure.bytes=measure.bytes.checked_add(output.bytes.len() as u64).ok_or_else(||TransferError::invalid("metadata_budget","record size overflow"))?;budget.bytes+=output.bytes.len() as u64;budget.records+=1;
                        if measure.bytes>limit{return Err(TransferError::invalid("backend_metadata_limit",format!("record fields exceed backend{limit} byte row limit")));}
                        if let Some(request)=output.request{measure.request=Some(request);}
                        if output.done{import_ext_mut(row)?.graph_cursor=None;break;}
                        if budget.exhausted(){import_ext_mut(row)?.graph_cursor=Some(serde_json::to_string(&measure).map_err(TransferError::platform)?);return Ok(());}
                    }
                    if budget.exhausted(){import_ext_mut(row)?.graph_cursor=Some(serde_json::to_string(&measure).map_err(TransferError::platform)?);return Ok(());}
                }else{budget.records+=1;}
            }
            3=>{
                let layout:crate::transfer::NovelJsonLayout=self.value(&row.session_id,&key).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata layout missing"))?;
                if let Some(members)=layout.members{
                    let caps=self.backend.storage_capabilities(&row.scope).await?;let max_url=caps.backend_limits.get("max_indexed_url_bytes").and_then(|v|v.parse::<usize>().ok()).unwrap_or(8192);let url=self.metadata_text(&row.session_id,&key,0,max_url).await?;
                    if members==0||!crate::virtual_novel::is_portable_toc_url(&url){return Err(TransferError::invalid("invalid_members","parent requires portable URL and ordinary members"));}
                    let start=ext.graph_cursor.as_deref().map(str::parse::<u32>).transpose().map_err(TransferError::platform)?.unwrap_or(0);let end=start.saturating_add(ADVANCE_ITEM_BUDGET).min(members);
                    for index in start..end{let member=self.metadata_text(&row.session_id,&key,10+layout.tags+index,max_url).await?;
                        let member_ordinal:u32=self.value(&row.session_id,&format!("url:{}",sha256_hex(member.as_bytes()))).await?.ok_or_else(||TransferError::invalid("missing_member","member absent from projected manifest"))?;
                        let wire:crate::transfer::protocol::WireNovel=self.value(&row.session_id,&format!("{JKEY_NOVEL}{member_ordinal}")).await?.ok_or_else(||TransferError::invalid("missing_member","member metadata missing"))?;
                        if wire.kind!=NovelKind::Normal{return Err(TransferError::invalid("member_kind_mismatch","virtual member is not ordinary"));}
                        let owner_key=format!("member_owner:{member_ordinal}");let identity=(ordinal,index);
                        if self.value::<(u32,u32)>(&row.session_id,&owner_key).await?.is_some_and(|owner|owner!=identity){return Err(TransferError::invalid("membership_conflict","member duplicated or assigned to multiple parents"));}self.save_value(&row.session_id,&owner_key,&identity).await?;budget.records+=1;
                    }
                    if end<members{import_ext_mut(row)?.graph_cursor=Some(end.to_string());return Ok(());}import_ext_mut(row)?.graph_cursor=None;
                }else{budget.records+=1;}
            }
            4=>{
                if let Some(publication)=self.journal.publication(&row.session_id,ordinal).await?&&publication.state==PublicationState::Committed{return {import_ext_mut(row)?.planning_ordinal+=1;Ok(())};}
                let layout:crate::transfer::NovelJsonLayout=self.value(&row.session_id,&key).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata layout missing"))?;let kind=if layout.members.is_some(){NovelKind::VirtualParent}else{NovelKind::Normal};
                let toc_url=self.metadata_text(&row.session_id,&key,0,8192).await?;
                // A canonical first fragment contains the entire display prefix,
                // even when each character is four UTF-8 bytes.
                let (title,_)=self.metadata_input(&row.session_id,&key,crate::transfer::NovelJsonRequest{field:1,part:0}).await?;
                let title=truncate_title(std::str::from_utf8(&title).map_err(TransferError::platform)?);
                let existing=self.backend.library_index(&row.scope,std::slice::from_ref(&toc_url)).await?.into_iter().next();
                let action=match &existing{Some(m)if m.kind!=kind=>PlanAction::Reject,Some(_)if row.conflict_policy==ConflictPolicy::Skip=>PlanAction::Skip,Some(_)=>PlanAction::Overwrite,None=>PlanAction::Create};
                let wire:crate::transfer::protocol::WireNovel=self.value(&row.session_id,&format!("{JKEY_NOVEL}{ordinal}")).await?.ok_or_else(||TransferError::invalid("novel_missing","normalized novel missing"))?;
                let start:u32=self.value(&row.session_id,&format!("file_start:{ordinal}")).await?.ok_or_else(||TransferError::invalid("file_missing","novel first file index missing"))?;
                let total_bytes:u64=self.value(&row.session_id,&format!("novel_bytes:{ordinal}")).await?.unwrap_or(0);
                let revision=row.plan_revision.clone().unwrap_or_else(||sha256_hex(format!("{}:{}",row.session_id.0,ext.manifest_sha256).as_bytes()));row.plan_revision=Some(revision.clone());
                let plan=PlanEntry{novel_ordinal:ordinal,toc_url:toc_url.clone(),title:title.clone(),kind,action,reason:match action{PlanAction::Skip=>Some("policy_skip".into()),PlanAction::Reject=>Some("kind_mismatch".into()),_=>None},existing_id:existing.as_ref().map(|m|m.id.0),record_revision:existing.as_ref().map(|m|m.record_revision.clone()),content_revision:existing.as_ref().map(|m|m.content_revision.clone()),membership_digest:existing.as_ref().and_then(|m|m.membership_digest.clone()),file_count:wire.files,total_bytes,receipt:None};
                self.journal.put_plan_entries(&row.session_id,&revision,std::slice::from_ref(&plan)).await?;self.save_value(&row.session_id,&format!("action:{ordinal}"),&action).await?;
                let spec=PublicationSpec{session_id:row.session_id.clone(),plan_revision:revision.clone(),publication_id:ordinal,toc_url,title,kind,file_id_start:start,file_count:wire.files,member_count:layout.members.unwrap_or(0),metadata:crate::transfer::ArchiveNovel::default(),member_ids:Vec::new(),target:PublicationTarget{action,existing_id:plan.existing_id,reserved_id:None,record_revision:plan.record_revision.clone(),content_revision:plan.content_revision.clone(),membership_digest:plan.membership_digest.clone()}};self.store_spec(&spec).await?;
                let receipt=if matches!(action,PlanAction::Skip|PlanAction::Reject){Some(serde_json::to_vec(&PublicationReceipt{novel_ordinal:ordinal,action,outcome:if action==PlanAction::Skip{"skipped"}else{"failed"}.into(),assigned_id:plan.existing_id,files_installed:0,error:plan.reason}).map_err(TransferError::platform)?)}else{None};
                self.journal.put_publication(&PublicationRow{session_id:row.session_id.clone(),publication_id:ordinal,state:if receipt.is_some(){PublicationState::Skipped}else{PublicationState::Pending},spec:serde_json::to_vec(&format!("spec:{ordinal}")).map_err(TransferError::platform)?,phase_cursor:None,assigned_id:plan.existing_id,receipt,error:None}).await?;budget.records+=1;
            }
            _=>return Err(TransferError::invalid("invalid_plan_phase","unknown durable planning pass")),
        }
        import_ext_mut(row)?.planning_ordinal+=1;Ok(())
    }


    async fn store_spec(&self,spec:&PublicationSpec)->Result<(),TransferError>{
        let slim=PublicationSpec{session_id:spec.session_id.clone(),plan_revision:spec.plan_revision.clone(),publication_id:spec.publication_id,toc_url:spec.toc_url.clone(),title:truncate_title(&spec.title),kind:spec.kind,metadata:crate::transfer::ArchiveNovel::default(),file_id_start:spec.file_id_start,file_count:spec.file_count,member_count:spec.member_count,member_ids:spec.member_ids.clone(),target:spec.target.clone()};
        self.save_blob(&spec.session_id,&format!("spec:{}",spec.publication_id),&serde_json::to_vec(&slim).map_err(TransferError::platform)?).await
    }

    async fn publication_spec(&self,publication:&PublicationRow)->Result<PublicationSpec,TransferError>{
        let key:String=serde_json::from_slice(&publication.spec).map_err(TransferError::platform)?;
        serde_json::from_slice(&self.blob(&publication.session_id,&key,0).await?).map_err(TransferError::platform)
    }
}

fn decision_selected(decision: crate::transfer::projection::Send) -> bool {
    matches!(decision, crate::transfer::projection::Send::Required | crate::transfer::projection::Send::Optional { default: true })
}

fn catalog_decision(profile: TransferProfile, role: FileRole, forbidden: &[FileRole]) -> CatalogDecision {
    if forbidden.contains(&role) { return CatalogDecision { send: "excluded".into(), default: false, reason: Some("destination_policy".into()) }; }
    match crate::transfer::projection::send_decision(profile, role, &[]) {
        crate::transfer::projection::Send::Required => CatalogDecision { send: "required".into(), default: true, reason: None },
        crate::transfer::projection::Send::Optional { default } => CatalogDecision { send: "optional".into(), default, reason: (!default).then(|| "profile".into()) },
        crate::transfer::projection::Send::Excluded { reason } => CatalogDecision { send: "excluded".into(), default: false, reason: Some(reason.into()) },
    }
}


impl TransferService {
    async fn cataloguing_step(&self,row:&mut SessionRow,budget:&mut AdvanceBudget)->Result<(),TransferError>{
        let ext=export_ext(row).ok_or_else(||invalid_state(row,"export"))?.clone();
        let page=self.backend.export_inventory(&row.scope,&row.session_id,&ext.requested_ids,ext.catalog_cursor.as_deref(),1).await?;let caps=self.backend.storage_capabilities(&row.scope).await?;
        for entry in &page.items {
            if entry.session_id!=row.session_id{return Err(TransferError::invalid("inventory_owner","backend returned another session's source"));}
            let summary_key=format!("{JKEY_EXPORT_NOVEL}{}",entry.novel_ordinal);let metadata_key=format!("xmetadata:{}",entry.novel_ordinal);
            if self.value::<u64>(&row.session_id,&summary_key).await?.is_some(){
                let previous:ExportNovelEntry=serde_json::from_slice(&self.blob(&row.session_id,&summary_key,0).await?).map_err(TransferError::platform)?;
                if previous.record_revision!=entry.record_revision||previous.content_revision!=entry.content_revision||previous.membership_digest!=entry.membership_digest{return Err(TransferError::PlanStale{message:"export source changed during inventory pagination".into()});}
            }else{
                if entry.metadata.is_none(){return Err(TransferError::invalid("inventory_metadata_missing","first inventory page must include metadata"));}
                let mut summary=entry.clone();summary.metadata=None;summary.files.clear();summary.title=truncate_title(&summary.title);
                self.save_blob(&row.session_id,&summary_key,&serde_json::to_vec(&summary).map_err(TransferError::platform)?).await?;
            }
            let capture=self.advance_input(row,"catalog_metadata",self.value::<crate::transfer::NovelJsonLayout>(&row.session_id,&metadata_key).await?.is_none()).await?;
            if capture{
                let layout=entry.metadata.as_ref().ok_or_else(||TransferError::invalid("inventory_metadata_missing","first metadata page must repeat the frozen layout"))?;let fields=layout.fields().map_err(TransferError::platform)?;
                let mut cursor:MetadataCursor=ext.metadata_cursor.as_deref().map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();let page=self.backend.export_metadata_page(&row.session_id,entry.novel_ordinal,&cursor,ADVANCE_ITEM_BUDGET as usize).await?;
                if page.fragments.len()>MAX_PAGE_ITEMS||page.fragments.is_empty()&&page.next_cursor.is_some(){return Err(TransferError::invalid("metadata_page","metadata page exceeds bounds or makes no progress"));}let mut charge=0u64;
                for fragment in &page.fragments{while cursor.field<fragment.field&&!layout.present(cursor.field){cursor.field+=1;}if fragment.field!=cursor.field||fragment.part!=cursor.part||fragment.byte_offset!=cursor.byte||!layout.present(fragment.field){return Err(TransferError::invalid("metadata_page","metadata fragment order or byte cursor changed"));}self.save_metadata_fragment(&row.session_id,&metadata_key,fragment).await?;charge=charge.checked_add(fragment.bytes.len() as u64+64).ok_or_else(||TransferError::invalid("metadata_budget","metadata size overflow"))?;budget.bytes+=fragment.bytes.len() as u64;budget.records+=1;cursor.byte+=fragment.bytes.len();cursor.part+=1;if fragment.last{cursor.field+=1;cursor.byte=0;cursor.part=0;}}
                while cursor.field<fields&&!layout.present(cursor.field){cursor.field+=1;}let done=page.next_cursor.is_none();if let Some(mut next)=page.next_cursor{while next.field<fields&&!layout.present(next.field){next.field+=1;}if next!=cursor{return Err(TransferError::invalid("metadata_page","metadata continuation is not the confirmed prefix"));}}else if cursor.field!=fields{return Err(TransferError::invalid("metadata_page","metadata ended before all fields"));}
                if done{self.save_value(&row.session_id,&metadata_key,layout).await?;}let next=export_ext_mut(row)?;next.metadata_bytes=next.metadata_bytes.checked_add(charge).filter(|size|*size<=MAX_METADATA_BYTES as u64).ok_or_else(||TransferError::invalid("metadata_budget","export metadata exceeds24MiB"))?;next.metadata_cursor=if done{None}else{Some(serde_json::to_string(&cursor).map_err(TransferError::platform)?)};
                if !done||budget.exhausted(){return Ok(());}
            }
            if entry.files.len()>MAX_PAGE_ITEMS{return Err(TransferError::invalid("inventory_page_limit","backend returned more than100 files"));}
            let mut items=Vec::new();
            for file in &entry.files{
                crate::transfer::validate_relative_path(&file.logical_path).map_err(TransferError::platform)?;let source_key=format!("xsource:{}:{}",entry.novel_ordinal,sha256_hex(file.logical_path.as_bytes()));
                if let Some(previous)=self.value::<SourceFileEntry>(&row.session_id,&source_key).await?&&(previous.size!=file.size||previous.logical_path!=file.logical_path){return Err(TransferError::PlanStale{message:"source changed during inventory".into()});}
                self.save_value(&row.session_id,&source_key,file).await?;if let SourceRef::Unavailable(reason)=&file.source{return Err(TransferError::invalid("source_missing",reason));}
                let mut role=crate::transfer::projection::classify(&file.logical_path,entry.kind==NovelKind::VirtualParent,&ClassifyContext::default());if file.known_role==Some(FileRole::Generated)&&role==FileRole::Unknown{role=FileRole::Generated;}
                let runtime=catalog_decision(TransferProfile::Runtime,role,&caps.unpersistable_roles);let complete=catalog_decision(TransferProfile::Complete,role,&caps.unpersistable_roles);let extras=row.extra_roles.iter().map(String::as_str).collect::<Vec<_>>();let decision=crate::transfer::projection::send_decision(row.profile,role,&extras);let selectable=role!=FileRole::Internal&&!caps.unpersistable_roles.contains(&role);let selected=selectable&&decision_selected(decision);
                items.push(CatalogItem{novel_ordinal:entry.novel_ordinal,existing_id:entry.id.0,toc_url:entry.toc_url.clone(),title:truncate_title(&entry.title),kind:entry.kind,logical_path:file.logical_path.clone(),role,size:file.size,selectable,selected,required:matches!(decision,crate::transfer::projection::Send::Required),exclusion_reason:(!selectable).then(||"destination_policy".into()).or_else(||(!selected).then(||"profile".into())),policy_version:caps.policy_version,runtime,complete});
            }
            if entry.files_complete&&entry.kind==NovelKind::Normal&&self.value::<SourceFileEntry>(&row.session_id,&format!("xsource:{}:{}",entry.novel_ordinal,sha256_hex(b"toc.yaml"))).await?.is_none(){return Err(TransferError::invalid("required_file_missing","ordinary export source has no toc.yaml"));}
            self.journal.put_catalog_entries(&row.session_id,row.catalog_revision+1,&items).await?;
            let next=export_ext_mut(row)?;next.catalogued+=items.len() as u64;next.metadata_bytes+=items.iter().map(|i|i.logical_path.len() as u64+256).sum::<u64>();
            if next.catalogued>=50_000||entry.novel_ordinal>=10_000||next.metadata_bytes>MAX_METADATA_BYTES as u64{return Err(TransferError::invalid("metadata_budget","export catalog exceeds entry or metadata budget"));}
            budget.records+=entry.files.len().max(1) as u32;
        }
        row.catalog_revision+=1;export_ext_mut(row)?.catalog_cursor=page.next_cursor.clone();if page.next_cursor.is_none(){row.state=SessionState::Selecting;row.next_action=NextAction::Select;row.operation_id=None;}Ok(())
    }

    async fn export_novel(&self, id: &SessionId, ordinal: u32) -> Result<ExportNovelEntry, TransferError> {
        serde_json::from_slice(&self.blob(id,&format!("{JKEY_EXPORT_NOVEL}{ordinal}"),0).await?).map_err(TransferError::platform)
    }

    pub async fn select(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], op: &ControlOp, selection: &SelectionRequest) -> Result<SessionStatus, TransferError> {
        let mut row = self.load_session(scope, id, Some(secret)).await?;
        let data = serde_json::to_vec(selection).map_err(TransferError::platform)?;
        if let Some(status) = self.begin_control(&row, op, "select", &data).await? { return Ok(status); }
        if row.state != SessionState::Selecting { return Err(invalid_state(&row, "selecting")); }
        crate::transfer::projection::validate_extra_roles(selection.profile, &selection.extra_roles).map_err(TransferError::platform)?;
        if sha256_from_hex(&selection.selection_digest).is_none() { return Err(TransferError::invalid("invalid_selection_digest", "selection digest must be SHA-256")); }
        row.profile = selection.profile; row.extra_roles = selection.extra_roles.clone();
        let mut roles = row.extra_roles.clone(); roles.sort(); roles.dedup();
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        hash.update(format!("v2\0{}\0{}\0{}\0", row.catalog_revision, row.profile.as_str(), roles.join(",")).as_bytes());
        let ext = export_ext_mut(&mut row)?; ext.selection_digest = Some(selection.selection_digest.clone()); ext.selection_cursor = None; ext.selection_hash = Some(sha256_checkpoint(&hash)); ext.prepare_phase = 0;
        row.state = SessionState::Preparing; self.schedule(&mut row); self.finish_control(&mut row, op).await
    }

    async fn preparing_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        let ext = export_ext(row).ok_or_else(|| invalid_state(row, "export"))?.clone();
        match ext.prepare_phase {
            0 => {
                use sha2::Digest;
                let page = self.journal.selection_catalog_entries(&row.session_id, ext.selection_cursor.as_deref(), MAX_PAGE_ITEMS).await?;
                let extras: Vec<&str> = row.extra_roles.iter().map(String::as_str).collect();
                let mut hash = ext.selection_hash.as_deref().and_then(sha256_resume).ok_or_else(|| TransferError::invalid("selection_checkpoint_corrupt", "selection digest checkpoint is invalid"))?;
                for item in &page.items {
                    hash.update(format!("{}\0{}\0{}\0{}\x01", item.novel_ordinal, item.logical_path, item.size, item.role.as_str()).as_bytes());
                    let selected = item.selectable && decision_selected(crate::transfer::projection::send_decision(row.profile, item.role, &extras));
                    self.journal.mark_catalog_selected(&row.session_id, &[(item.novel_ordinal, item.logical_path.clone())], selected).await?; budget.records += 1;
                }
                let next = export_ext_mut(row)?; next.selection_cursor = page.next_cursor; next.selection_hash = Some(sha256_checkpoint(&hash));
                if next.selection_cursor.is_none() {
                    let digest = hex::encode(hash.finalize());
                    if next.selection_digest.as_deref() != Some(digest.as_str()) { return Err(TransferError::SelectionMismatch { server_digest: digest }); }
                    next.prepare_phase = 1;
                }
            }
            1 => self.snapshot_step(row, budget).await?,
            2 => self.export_manifest_build_step(row, budget).await?,
            3 => self.batch_plan_step(row, budget).await?,
            _ => return Err(TransferError::invalid("invalid_snapshot_phase", "unknown snapshot preparation pass")),
        }
        Ok(())
    }

    async fn snapshot_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        use sha2::Digest;
        let ext = export_ext(row).ok_or_else(|| invalid_state(row, "export"))?.clone();
        if ext.snapshot_done { let step=self.backend.snapshot_finish(&row.session_id).await?;budget.records+=1;if step.done{export_ext_mut(row)?.prepare_phase=2;}return Ok(()); }
        let page = self.journal.selected_catalog_entries(&row.session_id, ext.snapshot_cursor.as_deref(), 1).await?;
        let Some(item) = page.items.first() else { let step=self.backend.snapshot_finish(&row.session_id).await?;budget.records+=1;budget.bytes+=step.processed_bytes;if step.done{export_ext_mut(row)?.prepare_phase=2;}return Ok(()) };
        let entry = self.export_novel(&row.session_id, item.novel_ordinal).await?;
        let gate = self.backend.snapshot_gate(&row.session_id, &entry, false).await?;
        if !gate.done { return Ok(()); }
        let source: SourceFileEntry = self.value(&row.session_id, &format!("xsource:{}:{}", entry.novel_ordinal, sha256_hex(item.logical_path.as_bytes()))).await?.ok_or_else(|| TransferError::invalid("source_missing", "selected catalog source is missing"))?;
        let source = &source;
        let id = ext.selected_files_count;
        let mut file = self.journal.file(&row.session_id, id).await?.unwrap_or(FileRow { session_id: row.session_id.clone(), file_id: id, novel_ordinal: entry.novel_ordinal, logical_path: source.logical_path.clone(), role: item.role, size: source.size, sha256: String::new(), state: FileState::Pending, confirmed_bytes: 0, next_chunk_index: 0, confirmed_prefix_sha256: Some(sha256_hex(&[])), hash_checkpoint: None, stage: None, pin: None, finalize_operation: None });
        file=self.advance_input(row,"snapshot_file",file).await?;
        if matches!(file.state,FileState::Pending|FileState::Receiving)&&file.confirmed_bytes>0&&!file.hash_checkpoint.as_deref().and_then(sha256_resume).is_some_and(|hash|file.confirmed_prefix_sha256.as_deref().and_then(sha256_from_hex).is_some_and(|expected|hash.finalize().as_slice()==expected.as_slice())){
            if let Some(stage)=file.stage.take(){self.backend.stage_abort(&row.session_id,&stage).await?;}
            file.confirmed_bytes=0;file.next_chunk_index=0;file.hash_checkpoint=None;file.confirmed_prefix_sha256=Some(sha256_hex(&[]));file.state=FileState::Pending;self.journal.update_file(&file).await?;budget.records+=1;return Ok(());
        }
        if matches!(source.source, SourceRef::Pinnable) && file.state == FileState::Pending {
            let pin = self.backend.pin_source(&row.session_id, &entry, source).await?;
            if pin.size != source.size || sha256_from_hex(&pin.sha256).is_none() { return Err(TransferError::invalid("snapshot_identity_mismatch", "pinned source identity does not match catalog")); }
            file.pin = Some(pin.pin); file.sha256 = pin.sha256; file.confirmed_bytes = pin.size; file.state = FileState::Pinned;
        } else if !matches!(file.state, FileState::Staged | FileState::Pinned) {
            let end = file.confirmed_bytes.saturating_add(row.chunk_bytes).min(file.size);
            let bytes=match self.backend.source_read_range(&entry,source,file.confirmed_bytes..end).await{Ok(bytes)=>bytes,Err(TransferError::Busy{code:"snapshot_preparing",..})=>{budget.bytes+=source.size;budget.records+=1;return Ok(());},Err(error)=>return Err(error)};
            if bytes.len() as u64 != end - file.confirmed_bytes { return Err(TransferError::invalid("source_short_read", "snapshot source disappeared or changed length")); }
            let mut hash = if file.confirmed_bytes == 0 { sha2::Sha256::new() } else { file.hash_checkpoint.as_deref().and_then(sha256_resume).ok_or_else(|| TransferError::invalid("hash_checkpoint_corrupt", "snapshot hash checkpoint is corrupt"))? };
            hash.update(&bytes);
            let stage = self.backend.stage_begin(&row.session_id, id, file.size).await?;
            self.backend.stage_append(&stage, file.confirmed_bytes, &bytes).await?;
            file.confirmed_bytes = end; file.hash_checkpoint = Some(sha256_checkpoint(&hash)); file.confirmed_prefix_sha256 = Some(hex::encode(hash.finalize())); file.stage = Some(stage.clone());
            if end == file.size { file.sha256 = file.confirmed_prefix_sha256.clone().unwrap_or_default(); file.stage = Some(self.backend.stage_seal(&stage, file.size, &file.sha256).await?); file.state = FileState::Staged; } else { file.state = FileState::Receiving; }
            budget.bytes += bytes.len() as u64;
        }
        self.journal.put_file(&file).await?; self.journal.update_file(&file).await?;
        budget.records += 1;
        if matches!(file.state, FileState::Staged | FileState::Pinned) {
            self.save_value(&row.session_id, &format!("xfile:{}:{}", file.novel_ordinal, sha256_hex(file.logical_path.as_bytes())), &file.file_id).await?;
            let next_cursor = page.next_cursor;
            let next_page = if let Some(cursor) = &next_cursor { Some(self.journal.selected_catalog_entries(&row.session_id, Some(cursor), 1).await?) } else { None };
            if next_page.as_ref().and_then(|p| p.items.first()).is_none_or(|next| next.novel_ordinal != entry.novel_ordinal) { self.backend.snapshot_gate(&row.session_id, &entry, true).await?; }
            row.confirmed_bytes = row.confirmed_bytes.checked_add(file.size).ok_or_else(|| TransferError::invalid("size_overflow", "snapshot bytes overflow"))?;
            let next = export_ext_mut(row)?; next.selected_files_count += 1; next.snapshot_cursor = next_cursor;
            if next.snapshot_cursor.is_none() { next.snapshot_done=true; }
        }
        Ok(())
    }

    async fn export_manifest_build_step(&self,row:&mut SessionRow,budget:&mut AdvanceBudget)->Result<(),TransferError>{
        let ext=export_ext(row).ok_or_else(||invalid_state(row,"export"))?.clone();
        let mut cursor:ManifestBuildCursor=ext.manifest_build_cursor.as_deref().map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();
        let mut output=Vec::with_capacity(if cursor.phase==2{ADVANCE_ITEM_BUDGET as usize*crate::transfer::protocol::IO_CHUNK_BYTES+1024}else{crate::transfer::protocol::IO_CHUNK_BYTES});
        if ext.manifest_offset==0{output.extend_from_slice(format!("{{\"format_version\":2,\"app_version\":{},\"exported_at\":{},\"novels\":[",serde_json::to_string(option_env!("NAROU_RS_VERSION_OVERRIDE").unwrap_or(env!("CARGO_PKG_VERSION"))).map_err(TransferError::platform)?,serde_json::to_string(&row.created_at.to_rfc3339()).map_err(TransferError::platform)?).as_bytes());}
        let mut finished=false;
        match cursor.phase{
            0=>{
                if self.value::<u64>(&row.session_id,&format!("{JKEY_EXPORT_NOVEL}{}",cursor.novel)).await?.is_none(){output.extend_from_slice(b"]}");finished=true;}else{if cursor.novel>0{output.push(b',');}output.extend_from_slice(b"{\"files\":[");cursor.phase=1;cursor.first_file=true;}budget.records+=1;
            }
            1=>{
                while budget.records<ADVANCE_ITEM_BUDGET&&cursor.file_id<ext.selected_files_count{
                    let file=self.journal.file(&row.session_id,cursor.file_id).await?.ok_or_else(||TransferError::invalid("snapshot_missing","prepared file row missing"))?;
                    if file.novel_ordinal!=cursor.novel{break;}
                    if !cursor.first_file{output.push(b',');}cursor.first_file=false;
                    let entry=crate::transfer::ArchiveFile{path:file.logical_path,size:Some(file.size),sha256:Some(file.sha256),bytes:Vec::new()};
                    output.extend_from_slice(&serde_json::to_vec(&entry).map_err(TransferError::platform)?);cursor.file_id+=1;budget.records+=1;
                }
                let next=if cursor.file_id<ext.selected_files_count{self.journal.file(&row.session_id,cursor.file_id).await?}else{None};
                if next.as_ref().is_none_or(|f|f.novel_ordinal!=cursor.novel){output.extend_from_slice(b"],");cursor.phase=2;cursor.metadata=crate::transfer::NovelJsonPullCursor::default();cursor.metadata_request=None;cursor.metadata_started=false;}
            }
            2=>{
                let key=format!("xmetadata:{}",cursor.novel);let layout:crate::transfer::NovelJsonLayout=self.value(&row.session_id,&key).await?.ok_or_else(||TransferError::invalid("metadata_missing","export metadata layout missing"))?;let mut cached:Option<(crate::transfer::NovelJsonRequest,Vec<u8>,bool)>=None;
                while budget.records<ADVANCE_ITEM_BUDGET&&output.len()+crate::transfer::protocol::IO_CHUNK_BYTES<=ADVANCE_BYTE_BUDGET as usize{
                    if let Some(request)=cursor.metadata_request&&cached.as_ref().is_none_or(|(key,_,_)|*key!=request){let(bytes,last)=self.metadata_input(&row.session_id,&key,request).await?;cached=Some((request,bytes,last));}
                    let input=cached.as_ref().map(|(request,bytes,last)|std::str::from_utf8(bytes).map(|text|crate::transfer::NovelJsonText{field:request.field,part:request.part,text,last:*last})).transpose().map_err(TransferError::platform)?;
                    let encoded=crate::transfer::novel_json_pull(&layout,&mut cursor.metadata,input,crate::transfer::protocol::IO_CHUNK_BYTES).map_err(TransferError::platform)?;let chunk=encoded.bytes;
                    let bytes=if cursor.metadata_started{chunk.as_slice()}else{cursor.metadata_started=true;if chunk.first()!=Some(&b'{'){return Err(TransferError::invalid("metadata_codec","metadata encoder did not start an object"));}&chunk[1..]};output.extend_from_slice(bytes);budget.records+=1;
                    if let Some(request)=encoded.request{cursor.metadata_request=Some(request);}
                    if encoded.done{cursor.novel+=1;cursor.phase=0;cursor.metadata_request=None;break;}
                }
            }
            _=>return Err(TransferError::invalid("invalid_manifest_cursor","unknown export manifest phase")),
        }
        self.append_export_manifest(row,&output,finished).await?;
        export_ext_mut(row)?.manifest_build_cursor=Some(serde_json::to_string(&cursor).map_err(TransferError::platform)?);
        budget.bytes+=output.len() as u64;Ok(())
    }

    async fn append_export_manifest(&self,row:&mut SessionRow,bytes:&[u8],finished:bool)->Result<(),TransferError>{
        let ext=export_ext(row).ok_or_else(||invalid_state(row,"export"))?.clone();
        if ext.manifest_pending_slot>1{return Err(TransferError::invalid("metadata_corrupt","invalid pending manifest generation"));}let mut pending=if ext.manifest_offset==0{Vec::with_capacity(CHUNK_BYTES as usize)}else{self.blob(&row.session_id,&format!("xm:pending:{}",ext.manifest_pending_slot),CHUNK_BYTES as usize).await?};if pending.len() as u64!=ext.manifest_offset%CHUNK_BYTES{return Err(TransferError::invalid("metadata_corrupt","manifest tail differs from its durable offset"));}
        let mut index=ext.manifest_offset/CHUNK_BYTES;let mut input=bytes;
        while !input.is_empty(){let take=input.len().min(CHUNK_BYTES as usize-pending.len());pending.extend_from_slice(&input[..take]);input=&input[take..];if pending.len() as u64==CHUNK_BYTES{self.save_blob(&row.session_id,&format!("xm:{index}"),&pending).await?;pending.clear();index+=1;}}
        let offset=ext.manifest_offset.checked_add(bytes.len() as u64).filter(|n|*n<=MAX_WIRE_MANIFEST_BYTES).ok_or_else(||TransferError::invalid("manifest_too_large","export manifest exceeds64MiB serialized metadata"))?;
        let next_slot=ext.manifest_pending_slot^1;if finished{if !pending.is_empty(){self.save_blob(&row.session_id,&format!("xm:{index}"),&pending).await?;}self.save_value(&row.session_id,JKEY_EXPORT_MANIFEST,&offset).await?;export_ext_mut(row)?.prepare_phase=3;}else{self.save_blob(&row.session_id,&format!("xm:pending:{next_slot}"),&pending).await?;}
        let next=export_ext_mut(row)?;next.manifest_offset=offset;next.manifest_pending_slot=next_slot;Ok(())
    }

    async fn batch_plan_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        let ext = export_ext(row).ok_or_else(|| invalid_state(row, "export"))?.clone();
        let mut file_id = ext.batch_file_cursor.as_deref().unwrap_or("0").parse::<u32>().map_err(TransferError::platform)?;
        let mut chunk_index = ext.batch_chunk;
        let mut plan = BatchPlanEntry { segments: Vec::new() }; let mut body = 0u64; let mut records = 0u32;
        while file_id < ext.selected_files_count && records < MAX_BATCH_RECORDS as u32 {
            let file = self.journal.file(&row.session_id, file_id).await?.ok_or_else(|| TransferError::invalid("snapshot_missing", "snapshot file missing"))?;
            let offset = u64::from(chunk_index).checked_mul(CHUNK_BYTES).ok_or_else(|| TransferError::invalid("chunk_index_overflow", "export chunk offset overflow"))?;
            let payload = (file.size - offset).min(CHUNK_BYTES);
            // Charge the framed header cap, including its SHA-256, not a
            // shorter header with the required digest omitted.
            let overhead = crate::transfer::protocol::FRAME_HEADER_MAX_BYTES as u64 + 4;
            if body + overhead + payload > MAX_BINARY_BODY_BYTES as u64 { break; }
            plan.segments.push((file_id, chunk_index, 1)); body += overhead + payload; records += 1; chunk_index += 1;
            if offset + payload == file.size { file_id += 1; chunk_index = 0; }
        }
        if !plan.segments.is_empty() { self.save_value(&row.session_id, &format!("{JKEY_EXPORT_BATCH}{}", ext.batch_count), &plan).await?; export_ext_mut(row)?.batch_count += 1; }
        let next = export_ext_mut(row)?; next.batch_file_cursor = Some(file_id.to_string()); next.batch_chunk = chunk_index;
        if file_id == ext.selected_files_count {
            let count = next.batch_count; self.save_value(&row.session_id, JKEY_BATCH_COUNT, &count).await?;
            export_ext_mut(row)?.batch_plan = Some("v2".into()); row.state = SessionState::Ready; row.next_action = NextAction::Read; row.operation_id = None;
        }
        budget.records += records; Ok(())
    }

    async fn begin_export_read(&self,scope:&AuthScope,id:&SessionId,secret:&[u8],identity:&str)->Result<(SessionRow,OperationRow),TransferError>{
        let row=self.load_session(scope,id,Some(secret)).await?;self.ensure_export_readable(&row)?;
        let operation=OperationId(format!("read_{}",sha256_hex(format!("{}:{}:{identity}",id.0,row.revision).as_bytes())));
        let claim=OperationRow{session_id:id.clone(),operation_id:operation.clone(),kind:"read".into(),request_fingerprint:sha256_hex(identity.as_bytes()),if_match_revision:None,result:None,completed:false,created_at:self.clock.now_utc()};
        self.journal.begin_operation(&claim).await?;
        let current=self.journal.session(scope,id).await?.ok_or(TransferError::SessionNotFound)?;
        self.reconcile_last_operation(&current).await?;
        if let Err(error)=self.ensure_export_readable(&current){self.journal.finish_operation(&claim).await?;return Err(error);}
        Ok((current,claim))
    }

    async fn finish_export_read(&self,mut row:SessionRow,mut claim:OperationRow,success:bool)->Result<(),TransferError>{
        let activity=if success{let revision=row.revision;row.updated_at=self.clock.now_utc();row.expires_at=row.updated_at+chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);self.journal.update_session(&row,revision).await}else{Ok(())};
        claim.completed=success;let released=self.journal.finish_operation(&claim).await;activity?;released
    }

    pub async fn export_manifest(&self,scope:&AuthScope,id:&SessionId,secret:&[u8],index:u32)->Result<BinaryChunk,TransferError>{
        let(row,claim)=self.begin_export_read(scope,id,secret,&format!("manifest:{index}")).await?;
        let result=async{
            let total:u64=self.value(id,JKEY_EXPORT_MANIFEST).await?.ok_or_else(||TransferError::invalid("snapshot_missing","export manifest missing"))?;
            let start=u64::from(index)*CHUNK_BYTES;if start>=total{return Err(TransferError::invalid("invalid_range","manifest chunk out of range"));}
            let bytes=self.blob(id,&format!("xm:{index}"),0).await?;
            Ok(BinaryChunk{sha256:sha256_hex(&bytes),last:start+bytes.len() as u64==total,total_bytes:total,bytes})
        }.await;
        self.finish_export_read(row,claim,result.is_ok()).await?;result
    }

    fn ensure_export_readable(&self,row:&SessionRow)->Result<(),TransferError>{
        if row.direction!=TransferDirection::Export||!matches!(row.state,SessionState::Ready|SessionState::Reading){return Err(invalid_state(row,"readable export snapshot"));}Ok(())
    }

    async fn read_export_file(&self,id:&SessionId,file_id:u32,index:u32)->Result<BinaryChunk,TransferError>{
        let file=self.journal.file(id,file_id).await?.ok_or_else(||TransferError::invalid("invalid_file","snapshot file missing"))?;
        if !matches!(file.state,FileState::Staged|FileState::Pinned){return Err(TransferError::invalid("snapshot_unverified","export source is not an immutable verified snapshot"));}
        let start=u64::from(index).checked_mul(CHUNK_BYTES).ok_or_else(||TransferError::invalid("invalid_range","chunk offset overflow"))?;
        if start>file.size||(start==file.size&&file.size!=0){return Err(TransferError::invalid("invalid_range","file chunk out of range"));}
        let end=start.saturating_add(CHUNK_BYTES).min(file.size);let stage=file.stage.as_ref().or(file.pin.as_ref()).ok_or_else(||TransferError::invalid("snapshot_missing","snapshot bytes missing"))?;
        let bytes=self.backend.stage_read_range(stage,start..end).await?;if bytes.len() as u64!=end-start{return Err(TransferError::invalid("snapshot_short_read","snapshot is truncated"));}
        Ok(BinaryChunk{sha256:sha256_hex(&bytes),bytes,total_bytes:file.size,last:end==file.size})
    }

    pub async fn export_file_chunk(&self,scope:&AuthScope,id:&SessionId,secret:&[u8],file_id:u32,index:u32)->Result<BinaryChunk,TransferError>{
        let(row,claim)=self.begin_export_read(scope,id,secret,&format!("file:{file_id}:{index}")).await?;
        let result=self.read_export_file(id,file_id,index).await;self.finish_export_read(row,claim,result.is_ok()).await?;result
    }

    pub async fn export_batch(&self,scope:&AuthScope,id:&SessionId,secret:&[u8],index:u32)->Result<BatchRead,TransferError>{
        let(row,claim)=self.begin_export_read(scope,id,secret,&format!("batch:{index}")).await?;
        let result=async{
            let count:u32=self.value(id,JKEY_BATCH_COUNT).await?.ok_or_else(||TransferError::invalid("snapshot_missing","batch table missing"))?;if index>=count{return Err(TransferError::invalid("invalid_range","batch out of range"));}
            let plan:BatchPlanEntry=self.value(id,&format!("{JKEY_EXPORT_BATCH}{index}")).await?.ok_or_else(||TransferError::invalid("snapshot_missing","batch boundary missing"))?;
            let mut body=Vec::new();let mut record_count=0;
            for(file,first,records)in plan.segments{let end=first.checked_add(records).ok_or_else(||TransferError::invalid("invalid_batch_plan","chunk index overflow"))?;for chunk in first..end{
                let payload=self.read_export_file(id,file,chunk).await?;let header=crate::transfer::protocol::BatchHeader{file_id:file,chunk_index:chunk,payload_size:payload.bytes.len() as u64,is_final:payload.last,sha256:None};
                let header=serde_json::to_vec(&header).map_err(TransferError::platform)?;body.extend_from_slice(&(header.len() as u32).to_be_bytes());body.extend_from_slice(&header);body.extend_from_slice(&payload.bytes);record_count+=1;
            }}
            if body.len()>MAX_BINARY_BODY_BYTES||record_count>MAX_BATCH_RECORDS as u32{return Err(TransferError::invalid("invalid_batch_plan","frozen batch exceeds wire budget"));}
            Ok(BatchRead{index,body_bytes:body.len() as u64,body,record_count,last:index+1==count})
        }.await;
        self.finish_export_read(row,claim,result.is_ok()).await?;result
    }
}


impl TransferService {
    pub async fn advance(&self, scope: &AuthScope, id: &SessionId, secret: &[u8], operation_id: &OperationId) -> Result<AdvanceResult, TransferError> {
        self.advance_authenticated(scope, id, Some(secret), operation_id).await
    }

    async fn advance_authenticated(&self, scope: &AuthScope, id: &SessionId, secret: Option<&[u8]>, operation_id: &OperationId) -> Result<AdvanceResult, TransferError> {
        let mut row = self.load_session(scope, id, secret).await?;
        if let Some(receipt) = self.journal.operation(id, operation_id).await? {
            if receipt.kind != "advance" { return Err(TransferError::OperationConflict); }
            if let Some(result) = receipt.result { return serde_json::from_slice(&result).map_err(TransferError::platform); }
        }
        if row.last_operation_id.as_ref() == Some(operation_id) && let Some(result) = &row.last_operation_result { return serde_json::from_slice(result).map_err(TransferError::platform); }
        if row.operation_id.as_ref() != Some(operation_id) || !matches!(row.next_action, NextAction::Advance | NextAction::FinalizeFile) { return Err(TransferError::OperationConflict); }
        let receipt = OperationRow { session_id: id.clone(), operation_id: operation_id.clone(), kind: "advance".into(), request_fingerprint: sha256_hex(operation_id.0.as_bytes()), if_match_revision: None, result: None, completed: false, created_at: self.clock.now_utc() };
        self.journal.begin_operation(&receipt).await?;
        row=self.load_session(scope,id,secret).await?;self.reconcile_last_operation(&row).await?;let receipt=self.journal.operation(id,operation_id).await?.ok_or(TransferError::OperationConflict)?;
        if let Some(result)=&receipt.result{self.journal.finish_operation(&receipt).await?;return serde_json::from_slice(result).map_err(TransferError::platform);}
        if receipt.kind!="advance"||row.operation_id.as_ref()!=Some(operation_id)||!matches!(row.next_action,NextAction::Advance|NextAction::FinalizeFile){self.journal.finish_operation(&receipt).await?;return Err(TransferError::OperationConflict);}
        let before = row.revision; let mut budget = AdvanceBudget::default();
        let step = match row.state {
            SessionState::ReceivingManifest | SessionState::Transferring if row.next_action == NextAction::FinalizeFile => self.finalize_file_step(&mut row, &mut budget).await,
            SessionState::Planning => self.planning_step(&mut row, &mut budget).await,
            SessionState::Cataloguing => self.cataloguing_step(&mut row, &mut budget).await,
            SessionState::Preparing => self.preparing_step(&mut row, &mut budget).await,
            SessionState::Publishing => self.publishing_step(&mut row, &mut budget).await,
            SessionState::Paused if row.resume_phase == Some(SessionState::Publishing) => self.publishing_step(&mut row, &mut budget).await,
            SessionState::Cancelling => self.cancel_step(&mut row, &mut budget).await,
            state if state.is_terminal() => self.gc_step(&mut row, &mut budget).await,
            _ => Err(invalid_state(&row, "advanceable phase")),
        };
        if let Err(error) = step {
            if matches!(error.code(),"catalog_preparing"|"fingerprinting"|"file_restart_required") {
                budget.records=budget.records.max(1);if error.code()=="fingerprinting"{budget.bytes=CHUNK_BYTES;}
            }else{
                if matches!(error, TransferError::Platform { .. } | TransferError::Busy { .. } | TransferError::RecoveryRequired { .. }) { self.journal.finish_operation(&receipt).await?; return Err(error); }
                if matches!(error, TransferError::PlanStale { .. }) && row.direction == TransferDirection::Import {
                    row.state=SessionState::Planning;row.plan_revision=Some(self.backend.opaque_id("plan"));let ext=import_ext_mut(&mut row)?;ext.planning_phase=4;ext.planning_ordinal=0;ext.publication_pass=0;ext.publish_cursor=0;row.error=Some("plan_stale".into());
                }else{row.state=SessionState::Failed;row.error=Some(format!("{}: {}",error.code(),error.message()));}
                self.schedule(&mut row);
            }
        }
        row.published_novels=self.journal.session(scope,id).await?.ok_or(TransferError::SessionNotFound)?.published_novels;
        if matches!(row.next_action, NextAction::Advance | NextAction::FinalizeFile) { row.operation_id = Some(OperationId(self.backend.opaque_id("op"))); } else { row.operation_id = None; }
        row.revision = before.checked_add(1).ok_or_else(|| TransferError::invalid("revision_overflow", "revision exhausted"))?;
        row.updated_at = self.clock.now_utc(); row.expires_at = row.updated_at + chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);
        let result = AdvanceResult { status: self.status_of(&row), step_done: row.next_action != NextAction::Advance, processed_records: budget.records, processed_bytes: budget.bytes };
        let bytes = serde_json::to_vec(&result).map_err(TransferError::platform)?; row.last_operation_id = Some(operation_id.clone()); row.last_operation_result = Some(bytes.clone());
        if let Err(error)=self.journal.update_session(&row,before).await{self.journal.finish_operation(&receipt).await?;return Err(error);}
        let mut receipt = receipt; receipt.completed = true; receipt.result = Some(bytes); self.journal.finish_operation(&receipt).await?; Ok(result)
    }

    async fn publishing_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        let ext = import_ext(row).ok_or_else(|| invalid_state(row, "import"))?.clone();
        let pass = ext.publication_pass;
        let mut cursor = ext.publish_cursor;
        if pass==2{
            let page=self.journal.publications(&row.session_id,ext.graph_cursor.as_deref(),MAX_PAGE_ITEMS).await?;
            let mut errors:bool=self.value(&row.session_id,"publication_errors").await?.unwrap_or(false);
            errors|=page.items.iter().any(|p|p.error.is_some()||matches!(p.state,PublicationState::RolledBack|PublicationState::RecoveryRequired)||p.receipt.as_ref().and_then(|r|serde_json::from_slice::<PublicationReceipt>(r).ok()).is_some_and(|r|r.outcome=="failed"));
            self.save_value(&row.session_id,"publication_errors",&errors).await?;budget.records+=page.items.len() as u32;
            import_ext_mut(row)?.graph_cursor=page.next_cursor.clone();if page.next_cursor.is_none(){row.state=if errors{SessionState::CompletedWithErrors}else{SessionState::Completed};self.schedule(row);}return Ok(());
        }
        if cursor>=ext.catalogued_novels{let next=import_ext_mut(row)?;next.publication_pass+=1;next.publish_cursor=0;next.graph_cursor=None;return Ok(());}
        let publication=self.journal.publication(&row.session_id,cursor).await?.ok_or_else(||TransferError::invalid("publication_missing","planned publication missing"))?;
        let mut publication=self.advance_input(row,"publication",publication).await?;
        let mut spec = self.publication_spec(&publication).await?;
        if (pass == 0 && spec.kind == NovelKind::VirtualParent) || (pass == 1 && spec.kind == NovelKind::Normal) || matches!(publication.state, PublicationState::Skipped | PublicationState::RolledBack) || (publication.state == PublicationState::Committed && publication.phase_cursor.as_deref() == Some("finished")) {
            cursor += 1; import_ext_mut(row)?.publish_cursor = cursor; budget.records += 1; return Ok(());
        }
        if row.state == SessionState::Paused && publication.state == PublicationState::Pending { row.next_action = NextAction::None; row.operation_id = None; return Ok(()); }
        if publication.state == PublicationState::Pending && spec.member_count != 0 {
            let cursor_key=format!("resolved_cursor:{}",spec.publication_id);
            let ids_key=format!("resolved_ids:{}",spec.publication_id);
            let current:u32=self.value(&row.session_id,&cursor_key).await?.unwrap_or(0);let start=self.advance_input(row,"resolved_cursor",current).await?;
            let mut ids:Vec<i64>=self.value(&row.session_id,&ids_key).await?.unwrap_or_default();
            if ids.len()<start as usize{return Err(TransferError::invalid("journal_corrupt","resolved member checkpoint is incomplete"));}ids.truncate(start as usize);
            let layout:crate::transfer::NovelJsonLayout=self.value(&row.session_id,&format!("metadata:{}",spec.publication_id)).await?.ok_or_else(||TransferError::invalid("metadata_missing","member layout missing"))?;let end=(start as usize+ADVANCE_ITEM_BUDGET as usize).min(spec.member_count as usize);
            for index in start as usize..end{let url=self.metadata_text(&row.session_id,&format!("metadata:{}",spec.publication_id),10+layout.tags+index as u32,8192).await?;
                let ordinal:u32=self.value(&row.session_id,&format!("url:{}",sha256_hex(url.as_bytes()))).await?.ok_or_else(||TransferError::invalid("missing_member","member URL is absent"))?;
                let member=self.journal.publication(&row.session_id,ordinal).await?.ok_or_else(||TransferError::invalid("missing_member","member publication absent"))?;
                if !matches!(member.state,PublicationState::Committed|PublicationState::Skipped)||member.assigned_id.is_none(){publication.state=PublicationState::Skipped;publication.error=Some("member_failed".into());publication.receipt=Some(serde_json::to_vec(&PublicationReceipt{novel_ordinal:spec.publication_id,action:spec.target.action,outcome:"failed".into(),assigned_id:None,files_installed:0,error:Some("member_failed".into())}).map_err(TransferError::platform)?);self.journal.update_publication(&publication).await?;import_ext_mut(row)?.publish_cursor+=1;return Ok(());}
                ids.push(member.assigned_id.unwrap_or_default());budget.records+=1;
            }
            self.save_value(&row.session_id,&ids_key,&ids).await?;self.save_value(&row.session_id,&cursor_key,&(end as u32)).await?;
            if end<spec.member_count as usize||budget.exhausted(){return Ok(());}
            spec.member_ids=ids;self.store_spec(&spec).await?;
        }
        let result = match publication.state {
            PublicationState::Pending | PublicationState::Committing => async {
                // Missing staged fields must enter the publication rollback
                // path, not fail the entire session with its gate still closed.
                spec.metadata = self.record_metadata(&publication.session_id, &format!("metadata:{}", publication.publication_id)).await?;
                if publication.state == PublicationState::Pending {
                    self.backend.publication_gate_close(&spec, publication.phase_cursor.as_deref()).await
                } else {
                    self.backend.publication_commit(&spec, publication.phase_cursor.as_deref()).await
                }
            }.await,
            PublicationState::Draining => self.backend.publication_drain(&spec, publication.phase_cursor.as_deref()).await,
            PublicationState::Prepared => self.backend.publication_prepare(&spec, publication.phase_cursor.as_deref()).await,
            PublicationState::Installing => self.backend.publication_install(&spec, publication.phase_cursor.as_deref()).await,
            PublicationState::Committed => self.backend.publication_finish(&spec, publication.phase_cursor.as_deref()).await,
            PublicationState::RollingBack | PublicationState::RecoveryRequired => self.backend.publication_rollback(&spec, publication.phase_cursor.as_deref()).await,
            _ => return Ok(()),
        };
        match result {
            Ok(step) => {
                budget.bytes += step.processed_bytes; budget.records += 1; publication.phase_cursor = step.cursor;
                if let Some(id) = step.assigned_id { publication.assigned_id = Some(id); spec.target.reserved_id = Some(id); self.store_spec(&spec).await?; }
                if let Some(receipt) = step.receipt { publication.receipt = Some(receipt); }
                if step.done && step.committed && matches!(publication.state, PublicationState::RollingBack | PublicationState::RecoveryRequired) {
                    self.backend.publication_gate_open(&spec,None).await?;publication.state=PublicationState::Committed;publication.phase_cursor=Some("finished".into());publication.error=None;row.published_novels+=1;import_ext_mut(row)?.publish_cursor+=1;
                } else if step.done {
                    publication.phase_cursor = None;
                    publication.state = match publication.state {
                        PublicationState::Pending => PublicationState::Draining, PublicationState::Draining => PublicationState::Prepared, PublicationState::Prepared => PublicationState::Installing, PublicationState::Installing => PublicationState::Committing, PublicationState::Committing => PublicationState::Committed,
                        PublicationState::Committed => {
                            self.backend.publication_gate_open(&spec, None).await?;
                            row.published_novels += 1;
                            import_ext_mut(row)?.publish_cursor += 1;
                            finish_publication(&mut publication, &spec, true)?
                        },
                        PublicationState::RollingBack | PublicationState::RecoveryRequired => {
                            self.backend.publication_gate_open(&spec, None).await?;
                            import_ext_mut(row)?.publish_cursor += 1;
                            finish_publication(&mut publication, &spec, false)?
                        },
                        state => state,
                    };
                }
                self.journal.update_publication(&publication).await?;
            }
            Err(error) => {
                if matches!(error, TransferError::Busy { .. } | TransferError::Platform { .. }) { return Err(error); }
                if publication.state == PublicationState::Committed { return Err(TransferError::RecoveryRequired { message: error.message() }); }
                if matches!(publication.state, PublicationState::RollingBack | PublicationState::RecoveryRequired) { publication.state = PublicationState::RecoveryRequired; publication.error = Some(error.message()); self.journal.update_publication(&publication).await?; return Err(TransferError::RecoveryRequired { message: error.message() }); }
                publication.state = PublicationState::RollingBack; publication.phase_cursor = None; publication.error = Some(format!("{}: {}", error.code(), error.message())); self.journal.update_publication(&publication).await?;
                if matches!(error, TransferError::PlanStale { .. }) { row.error = Some("plan_stale".into()); }
            }
        }
        Ok(())
    }

    async fn cancel_step(&self,row:&mut SessionRow,budget:&mut AdvanceBudget)->Result<(),TransferError>{
        let current=self.journal.pending_publications(&row.session_id,1).await?.into_iter().next();
        let pending=self.advance_input(row,"cancel_publication",current).await?;
        if let Some(mut publication)=pending{
            let spec=self.publication_spec(&publication).await?;
            let step=if publication.state==PublicationState::Committed{self.backend.publication_finish(&spec,publication.phase_cursor.as_deref()).await?}else{
                if !matches!(publication.state,PublicationState::RollingBack|PublicationState::RecoveryRequired){publication.phase_cursor=None;}
                publication.state=PublicationState::RollingBack;self.journal.update_publication(&publication).await?;
                self.backend.publication_rollback(&spec,publication.phase_cursor.as_deref()).await?
            };
            let committed=publication.state==PublicationState::Committed||step.committed;
            publication.phase_cursor=step.cursor;budget.bytes+=step.processed_bytes;budget.records+=1;
            if let Some(receipt)=step.receipt{publication.receipt=Some(receipt);}
            if let Some(id)=step.assigned_id{publication.assigned_id=Some(id);}
            if step.done {
                self.backend.publication_gate_open(&spec,None).await?;
                publication.state = finish_publication(&mut publication, &spec, committed)?;
            }
            self.journal.update_publication(&publication).await?;return Ok(());
        }
        row.state=SessionState::Cancelled;row.resume_phase=None;self.schedule(row);self.gc_step(row,budget).await
    }

    async fn gc_step(&self, row: &mut SessionRow, budget: &mut AdvanceBudget) -> Result<(), TransferError> {
        if !self.journal.active_publications(&row.session_id, 1).await?.is_empty() { return Err(TransferError::RecoveryRequired { message: "unresolved publication protects session resources".into() }); }
        let step = self.backend.release_session_resources(row, row.gc_cursor.as_deref()).await?; budget.bytes += step.processed_bytes; budget.records += 1; row.gc_cursor = step.cursor;
        if step.done { row.next_action = NextAction::None; row.operation_id = None; }
        Ok(())
    }

    pub async fn recover(&self,scope:&AuthScope)->Result<RecoveryReport,TransferError>{
        let mut report=RecoveryReport::default();
        for candidate in self.journal.sessions_with_pending_publications(scope,MAX_PAGE_ITEMS).await?{
            let claim_id=OperationId(format!("recover_{}",sha256_hex(format!("{}:{}",candidate.session_id.0,candidate.revision).as_bytes())));
            let mut claim=OperationRow{session_id:candidate.session_id.clone(),operation_id:claim_id.clone(),kind:"recovery".into(),request_fingerprint:sha256_hex(claim_id.0.as_bytes()),if_match_revision:None,result:None,completed:false,created_at:self.clock.now_utc()};
            match self.journal.begin_operation(&claim).await{Ok(_)=>{},Err(TransferError::Busy{..})=>continue,Err(error)=>return Err(error)}
            let result:Result<(),TransferError>=async{
                let mut row=self.journal.session(scope,&candidate.session_id).await?.ok_or(TransferError::SessionNotFound)?;
                if row.revision!=candidate.revision{return Ok(());}
                let mut current=self.journal.active_publications(&row.session_id,1).await?;
                if current.is_empty()&&(matches!(row.state,SessionState::Publishing|SessionState::Cancelling)||row.resume_phase==Some(SessionState::Publishing)){current=self.journal.pending_publications(&row.session_id,1).await?;}
                let old_operation=row.operation_id.clone();row.operation_id=Some(claim_id.clone());
                let pending=self.advance_input(&row,"recovery_publication",current.into_iter().next()).await?;
                let Some(mut publication)=pending else{return Ok(())};
                let spec=self.publication_spec(&publication).await?;
                let was_committed=publication.state==PublicationState::Committed;
                let step=if was_committed{self.backend.publication_finish(&spec,publication.phase_cursor.as_deref()).await}else{self.backend.publication_rollback(&spec,publication.phase_cursor.as_deref()).await};
                let mut bytes=0;
                match step{
                    Ok(step)=>{
                        report.stepped_publications+=1;bytes=step.processed_bytes;let committed=was_committed||step.committed;publication.phase_cursor=step.cursor;
                        if let Some(receipt)=step.receipt{publication.receipt=Some(receipt);}
                        if let Some(id)=step.assigned_id{publication.assigned_id=Some(id);}
                        if step.done {
                            self.backend.publication_gate_open(&spec,None).await?;
                            publication.state = finish_publication(&mut publication, &spec, committed)?;
                            row.error = None;
                            report.resolved_publications += 1;
                        }
                        else{publication.state=if committed{PublicationState::Committed}else{PublicationState::RollingBack};report.pending_recovery+=1;}
                    },
                    Err(TransferError::Busy{code:"fingerprinting",..})=>{report.stepped_publications+=1;report.pending_recovery+=1;bytes=CHUNK_BYTES;},
                    Err(error)=>{publication.state=PublicationState::RecoveryRequired;publication.error=Some(error.message());row.error=Some("recovery_required".into());report.pending_recovery+=1;},
                }
                self.journal.update_publication(&publication).await?;
                row.published_novels=self.journal.session(scope,&row.session_id).await?.ok_or(TransferError::SessionNotFound)?.published_novels;
                if matches!(row.next_action,NextAction::Advance|NextAction::FinalizeFile){row.operation_id=Some(OperationId(self.backend.opaque_id("op")));}else{row.operation_id=None;}
                let revision=row.revision;row.revision+=1;row.updated_at=self.clock.now_utc();row.expires_at=row.updated_at+chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);
                if let Some(operation)=old_operation{
                    let response=AdvanceResult{status:self.status_of(&row),step_done:false,processed_records:1,processed_bytes:bytes};let encoded=serde_json::to_vec(&response).map_err(TransferError::platform)?;row.last_operation_id=Some(operation.clone());row.last_operation_result=Some(encoded.clone());
                    self.journal.update_session(&row,revision).await?;
                    if let Some(mut receipt)=self.journal.operation(&row.session_id,&operation).await?&&receipt.kind=="advance"&&receipt.result.is_none(){receipt.completed=true;receipt.result=Some(encoded);self.journal.finish_operation(&receipt).await?;}
                }else{self.journal.update_session(&row,revision).await?;}
                Ok(())
            }.await;
            claim.completed=result.is_ok();self.journal.finish_operation(&claim).await?;result?;
        }
        Ok(report)
    }

    pub async fn expire_idle(&self, scope: &AuthScope) -> Result<u32, TransferError> {
        let mut expired = 0;
        for mut row in self.journal.find_idle_sessions(scope, self.clock.now_utc(), MAX_PAGE_ITEMS).await? {
            if matches!(row.state, SessionState::Publishing | SessionState::Cancelling) || row.resume_phase == Some(SessionState::Publishing) || !self.journal.active_publications(&row.session_id, 1).await?.is_empty() { continue; }
            row.state = SessionState::Expired; row.error = Some("session_expired".into()); self.schedule(&mut row); self.persist(&mut row, true).await?;
            let mut budget = AdvanceBudget::default(); self.gc_step(&mut row, &mut budget).await?; self.persist(&mut row, true).await?; expired += 1;
        }
        Ok(expired)
    }
}


#[derive(Debug,Clone,Default,Serialize,Deserialize)]
struct ManifestBuildCursor { novel:u32,file_id:u32,phase:u8,first_file:bool,metadata:crate::transfer::NovelJsonPullCursor,metadata_request:Option<crate::transfer::NovelJsonRequest>,metadata_started:bool }

#[derive(Debug,Clone,Default,PartialEq,Eq,Serialize,Deserialize)]
pub struct MetadataCursor { pub field:u32, pub byte:usize, pub part:u32 }
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct MetadataFragment {pub field:u32,pub part:u32,pub byte_offset:usize,pub bytes:Vec<u8>,pub last:bool}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct ExportMetadataPage {pub fragments:Vec<MetadataFragment>,pub next_cursor:Option<MetadataCursor>}
#[derive(Debug,Clone,Default,Serialize,Deserialize)]
struct WireMetadataCursor {metadata:MetadataCursor,source_part:u32,pending_key:Option<String>}
#[derive(Debug,Clone,Default,Serialize,Deserialize)]
struct MetadataMeasure {cursor:crate::transfer::NovelJsonPullCursor,request:Option<crate::transfer::NovelJsonRequest>,bytes:u64}

fn metadata_scalar_field(name:&str)->Option<u32>{match name{"toc_url"=>Some(0),"title"=>Some(1),"author"=>Some(2),"sitename"=>Some(3),"domain"=>Some(4),"attribute_tag"=>Some(5),"ncode"=>Some(6),"subtitle"=>Some(7),"general_lastup"=>Some(8),"last_update"=>Some(9),_=>None}}
fn wire_metadata_field(layout:&crate::transfer::NovelJsonLayout,field:u32)->Result<(&'static str,Option<u32>),TransferError>{
    const NAMES:[&str;10]=["toc_url","title","author","sitename","domain","attribute_tag","ncode","subtitle","general_lastup","last_update"];
    if field<10{return Ok((NAMES[field as usize],None));}
    if field<10+layout.tags{return Ok(("tag",Some(field-10)));}
    if field<layout.fields().map_err(TransferError::platform)?{return Ok(("member",Some(field-10-layout.tags)));}
    Err(TransferError::invalid("metadata_corrupt","metadata field is outside layout"))
}

impl TransferService {
    async fn save_metadata_fragment(&self,id:&SessionId,key:&str,fragment:&MetadataFragment)->Result<(),TransferError>{
        if fragment.bytes.len()>crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES||(!fragment.last&&fragment.bytes.is_empty())||std::str::from_utf8(&fragment.bytes).is_err(){return Err(TransferError::invalid("metadata_fragment","metadata fragments must be bounded UTF-8"));}
        self.journal.put_journal_entry(id,SESSION_JOURNAL_PUB,&format!("{key}:raw:{}:{}",fragment.field,fragment.part),&fragment.bytes).await?;
        if fragment.last{let parts=fragment.part.checked_add(1).ok_or_else(||TransferError::invalid("metadata_budget","metadata fragment count overflow"))?;self.save_value(id,&format!("{key}:field:{}",fragment.field),&parts).await?;let length=fragment.byte_offset.checked_add(fragment.bytes.len()).ok_or_else(||TransferError::invalid("metadata_budget","metadata field size overflow"))?;self.save_value(id,&format!("{key}:length:{}",fragment.field),&length).await?;}Ok(())
    }

    async fn copy_wire_metadata_step(&self,row:&SessionRow,key:&str,ordinal:u32,layout:&crate::transfer::NovelJsonLayout,cursor:&mut WireMetadataCursor,budget:&mut AdvanceBudget)->Result<bool,TransferError>{
        let id=&row.session_id;let operation=row.operation_id.as_ref().ok_or_else(||TransferError::invalid("operation_missing","metadata normalization needs operation identity"))?;
        let fields=layout.fields().map_err(TransferError::platform)?;let mut pending=if let Some(key)=&cursor.pending_key{self.journal.journal_entry(id,SESSION_JOURNAL_PUB,key).await?.ok_or_else(||TransferError::invalid("metadata_corrupt","metadata normalization tail missing"))?}else{Vec::new()};
        if pending.len()>=crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES{return Err(TransferError::invalid("metadata_corrupt","metadata tail is not bounded"));}
        while cursor.metadata.field<fields&&budget.records<ADVANCE_ITEM_BUDGET&&budget.bytes+crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES as u64<=ADVANCE_BYTE_BUDGET{
            if !layout.present(cursor.metadata.field){cursor.metadata.field+=1;continue;}
            let(name,item)=wire_metadata_field(layout,cursor.metadata.field)?;let parts:u32=self.value(id,&format!("field:{ordinal}:{name}:{}",item.unwrap_or(0))).await?.ok_or_else(||TransferError::invalid("metadata_corrupt","wire field descriptor missing"))?;
            if parts==0||cursor.source_part>=parts{return Err(TransferError::invalid("metadata_corrupt","invalid wire field cursor"));}
            let bytes=self.journal.journal_entry(id,SESSION_JOURNAL_PUB,&format!("{JKEY_TEXT}{ordinal}:{name}:{}:{}",item.unwrap_or(0),cursor.source_part)).await?.ok_or_else(||TransferError::invalid("metadata_corrupt","wire metadata fragment missing"))?;
            if bytes.len()>crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES||std::str::from_utf8(&bytes).is_err(){return Err(TransferError::invalid("metadata_corrupt","invalid wire UTF-8 fragment"));}
            pending.extend_from_slice(&bytes);cursor.metadata.byte=cursor.metadata.byte.checked_add(bytes.len()).ok_or_else(||TransferError::invalid("metadata_budget","metadata length overflow"))?;cursor.source_part+=1;budget.records+=1;budget.bytes+=bytes.len() as u64;
            if pending.len()>=crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES{let text=std::str::from_utf8(&pending).map_err(TransferError::platform)?;let mut end=crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES;while !text.is_char_boundary(end){end-=1;}self.journal.put_journal_entry(id,SESSION_JOURNAL_PUB,&format!("{key}:raw:{}:{}",cursor.metadata.field,cursor.metadata.part),&pending[..end]).await?;cursor.metadata.part+=1;pending.copy_within(end..,0);pending.truncate(pending.len()-end);}
            if cursor.source_part==parts{
                if !pending.is_empty()||cursor.metadata.part==0{self.journal.put_journal_entry(id,SESSION_JOURNAL_PUB,&format!("{key}:raw:{}:{}",cursor.metadata.field,cursor.metadata.part),&pending).await?;cursor.metadata.part+=1;pending.clear();}
                self.save_value(id,&format!("{key}:field:{}",cursor.metadata.field),&cursor.metadata.part).await?;self.save_value(id,&format!("{key}:length:{}",cursor.metadata.field),&cursor.metadata.byte).await?;cursor.metadata.field+=1;cursor.metadata.byte=0;cursor.metadata.part=0;cursor.source_part=0;cursor.pending_key=None;
            }
        }
        if !pending.is_empty(){let tail=format!("wiretail:{ordinal}:{}",operation.0);self.journal.put_journal_entry(id,SESSION_JOURNAL_PUB,&tail,&pending).await?;cursor.pending_key=Some(tail);}else{cursor.pending_key=None;}
        if cursor.metadata.field==fields{self.save_value(id,key,layout).await?;Ok(true)}else{Ok(false)}
    }

    async fn metadata_text(&self,id:&SessionId,key:&str,field:u32,limit:usize)->Result<String,TransferError>{
        let length:usize=self.value(id,&format!("{key}:length:{field}")).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata field byte length missing"))?;
        if length>limit{return Err(TransferError::invalid("backend_metadata_limit",format!("metadata field exceeds{limit} byte limit")));}
        let parts:u32=self.value(id,&format!("{key}:field:{field}")).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata field part count missing"))?;
        if parts==0||parts as usize>length/(crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES-3)+1{return Err(TransferError::invalid("metadata_corrupt","metadata is not canonically fragmented"));}
        let mut text=String::with_capacity(length);for part in 0..parts{let bytes=self.journal.journal_entry(id,SESSION_JOURNAL_PUB,&format!("{key}:raw:{field}:{part}")).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata fragment missing"))?;if text.len()+bytes.len()>length{return Err(TransferError::invalid("metadata_corrupt","metadata exceeds declared byte length"));}text.push_str(std::str::from_utf8(&bytes).map_err(TransferError::platform)?);}if text.len()!=length{return Err(TransferError::invalid("metadata_corrupt","metadata byte length mismatch"));}Ok(text)
    }

    async fn record_metadata(&self,id:&SessionId,key:&str)->Result<crate::transfer::ArchiveNovel,TransferError>{
        let layout:crate::transfer::NovelJsonLayout=self.value(id,key).await?.ok_or_else(||TransferError::invalid("metadata_missing","novel metadata layout missing"))?;
        let mut novel=crate::transfer::ArchiveNovel{novel_type:layout.novel_type,..Default::default()};let mut retained=0usize;
        for field in 0..10+layout.tags{if field==7||!layout.present(field){continue;}let text=self.metadata_text(id,key,field,MAX_METADATA_BYTES.saturating_sub(retained)).await?;retained=retained.checked_add(text.len()+64).filter(|size|*size<=MAX_METADATA_BYTES).ok_or_else(||TransferError::invalid("metadata_budget","record metadata exceeds24MiB"))?;match field{0=>novel.toc_url=text,1=>novel.title=text,2=>novel.author=text,3=>novel.sitename=text,4=>novel.domain=Some(text),5=>novel.attribute_tag=Some(text),6=>novel.ncode=Some(text),8=>novel.general_lastup=Some(text),9=>novel.last_update=Some(text),_=>novel.tags.push(text)}}Ok(novel)
    }

    async fn metadata_input(&self,id:&SessionId,key:&str,request:crate::transfer::NovelJsonRequest)->Result<(Vec<u8>,bool),TransferError>{
        let parts:u32=self.value(id,&format!("{key}:field:{}",request.field)).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata descriptor missing"))?;
        if request.part>=parts{return Err(TransferError::invalid("metadata_corrupt","metadata fragment cursor exceeds field"));}let bytes=self.journal.journal_entry(id,SESSION_JOURNAL_PUB,&format!("{key}:raw:{}:{}",request.field,request.part)).await?.ok_or_else(||TransferError::invalid("metadata_missing","metadata fragment missing"))?;Ok((bytes,request.part+1==parts))
    }
}
