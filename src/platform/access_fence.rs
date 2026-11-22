//! Per-novel reader/writer access fence (the `NovelContentAccess` port).
//!
//! This is the durable coordination boundary every novel-content reader and
//! writer passes through; it is **not** the legacy `NovelLockGuard`, which
//! only serializes cooperating CLI jobs. The fence lives per logical novel
//! prefix ([`ObjectPrefix`] — the same `NovelObjectKeys::prefix` identity on
//! filesystem, SQLite and D1+S3), so callers never deal in OS paths.
//!
//! Ordering contract (publication §11.2):
//!
//! 1. The writer calls [`close_gate`](NovelAccessFence::close_gate) under its
//!    mutation fence — this *first* stops new reader admission, durably,
//!    then drain begins. `close_gate` returns a writer epoch.
//! 2. Existing readers drain: a lease expires unless renewed, and renewal
//!    uses the *same* CAS condition as admission (gate open + same epoch), so
//!    a closed gate rejects renewals and readers expire within the lease
//!    TTL. `pending_readers` reports the residual.
//! 3. Writes happen to private stage only; the canonical install is exactly
//!    one epoch-guarded commit per file via
//!    [`commit_staged`](NovelAccessFence::commit_staged) (or, for
//!    reference-model stores like the Worker's S3 `content_objects`, the
//!    equivalent guarded logical-ref swap the concrete backend exposes —
//!    `ContentStore::install_reference`).
//! 4. [`open_gate`](NovelAccessFence::open_gate) re-admits readers after
//!    commit or completed recovery.
//!
//! Concrete backends:
//!
//! * `NativeObjectStore` (YAML/FS): gate record + reader leases as small
//!   JSON objects under `.objects/.narou-transfer/.gate/`; epoch check and
//!   `rename` happen inside a non-stealable `fs2` OS lock on a per-novel
//!   `.lock/` file. POSIX readers holding open handles keep reading the
//!   pre-image; Windows open handles can make the rename fail → the caller
//!   surfaces `novel_busy`, never a forced overwrite.
//! * `SqliteObjectStore` / `D1ObjectStore`: `novel_access_gates` /
//!   `novel_reader_leases` rows; admission, renewal, close and commit are
//!   single-transaction/batch CAS. Canonical commits additionally bump
//!   `objects.generation`, which every chunk read is bound to.
//! * `MemoryNovelAccess` (tests) mirrors the same state machine.
//!
//! Readers that hold a lease MUST revalidate it at every logical file open;
//! the stores bind each chunk read to the pinned `objects.generation`, so a
//! reader whose epoch expired mid-read fails instead of mixing old and new
//! chunks into one successful stream.

use crate::error::{NarouError, Result};

use super::object_store::{ObjectKey, ObjectPrefix};
use super::{PlatformFuture, PlatformService};

/// Lease TTL written by readers when none is negotiated: 30 s admission,
/// 10 s renewal cadence for in-flight reads.
pub const READER_LEASE_SECS: i64 = 30;

/// A live reader admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateLease {
    /// Unique per reader attempt; stable while renewing.
    pub reader_id: String,
    /// Gate epoch the reader was admitted under. The next logical file open
    /// and every chunk read carry this expectation.
    pub epoch: u64,
    /// Unix seconds after which the lease lapses without renewal.
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Open,
    /// Admission closed; drain in progress or commit underway.
    Closed,
}

/// Observable gate state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateInfo {
    pub epoch: u64,
    /// Canonical byte/ref mutations, independent of admission epoch changes.
    pub content_revision: u64,
    pub state: GateState,
    /// Readers with an unexpired lease under the current epoch.
    pub pending_readers: u32,
    /// Writer owner while `state == Closed`.
    pub owner: Option<String>,
    /// Unix seconds of the last durable gate mutation.
    pub updated_at: i64,
}
/// One canonical removal owner grammar, shared by callers and recovery.
pub fn parse_removal_owner(owner:&str)->Result<(super::NovelId,bool)>{
    let invalid=||NarouError::Platform("invalid removal owner UUID/identity".into());let mut fields=owner.strip_prefix("remove-v1:").ok_or_else(invalid)?.split(':');
    let raw_id=fields.next().ok_or_else(invalid)?;let id=raw_id.parse::<i64>().map_err(|_|invalid())?;if id.to_string()!=raw_id{return Err(invalid());}
    let delete_files=match fields.next(){Some("0")=>false,Some("1")=>true,_=>return Err(invalid())};let uuid=fields.next().ok_or_else(invalid)?;
    if fields.next().is_some()||uuid.len()!=36||!uuid.bytes().enumerate().all(|(index,byte)|if matches!(index,8|13|18|23){byte==b'-'}else{byte.is_ascii_hexdigit()}){return Err(invalid());}Ok((super::NovelId(id),delete_files))
}
pub fn new_removal_owner(id:super::NovelId,delete_files:bool)->Result<String>{
    use std::fmt::Write;let mut bytes:[u8;16]=crate::login::crypto::random_bytes()?;bytes[6]=(bytes[6]&0x0f)|0x40;bytes[8]=(bytes[8]&0x3f)|0x80;
    let mut owner=String::with_capacity(80);write!(&mut owner,"remove-v1:{}:{}:",id.0,u8::from(delete_files)).expect("String write");for(index,byte)in bytes.into_iter().enumerate(){if matches!(index,4|6|8|10){owner.push('-');}write!(&mut owner,"{byte:02x}").expect("String write");}Ok(owner)
}

/// Durable reader/writer fence for one novel.
///
/// Implementations must place admission, renewal, close and commit behind a
/// single atomicity boundary (OS lock file / DB transaction / D1 batch).
/// No primitive may steal a lock whose owner only went quiet.
#[doc(hidden)]
pub trait WriteFenceProof:PlatformService+std::fmt::Debug{
    fn source_id(&self)->usize;
    fn run_many(&self,fences:&[WriteFence],invalidate:bool,operation:&mut dyn FnMut()->Result<()>)->Result<()>;
}
#[derive(Debug,Clone)]
pub struct WriteFence {pub prefix:ObjectPrefix,pub epoch:u64,proof:Option<std::sync::Arc<dyn WriteFenceProof>>,liveness:Option<std::sync::Arc<dyn super::repository::ReservationLiveness>>,job_owner:Option<std::sync::Arc<super::JobExecutionOwner>>}
impl WriteFence{
    pub fn new(prefix:ObjectPrefix,epoch:u64)->Self{Self{prefix,epoch,proof:None,liveness:None,job_owner:None}}
    pub(crate) fn with_proof(prefix:ObjectPrefix,epoch:u64,proof:std::sync::Arc<dyn WriteFenceProof>)->Self{Self{prefix,epoch,proof:Some(proof),liveness:None,job_owner:None}}
    pub fn bind_job_owner(&mut self,owner:std::sync::Arc<super::JobExecutionOwner>)->Result<()>{
        if let Some(current)=&self.job_owner{if current.as_ref()!=owner.as_ref(){return Err(NarouError::Platform("job_owner_changed: writer belongs to another execution".into()));}}else{self.job_owner=Some(owner);}Ok(())
    }
    pub fn job_owner(&self)->Option<&std::sync::Arc<super::JobExecutionOwner>>{self.job_owner.as_ref()}
    /// Every detached writer clone retains the allocating token's SAME native
    /// OS resource. Worker reservations instead remain owned by their job row.
    pub fn retain_reservation(&mut self,reservation:&super::NovelIdReservation)->Result<()>{
        if let Some(owner)=reservation.job_owner(){self.bind_job_owner(owner.clone())?;}
        let resource=reservation.liveness();if let Some(current)=&self.liveness{if !resource.as_ref().is_some_and(|resource|std::sync::Arc::ptr_eq(current,resource)){return Err(NarouError::Platform("writer reservation lifetime cannot be replaced".into()));}}else{self.liveness=resource;}Ok(())
    }
    pub(crate) fn proof_source_id(&self)->Option<usize>{self.proof.as_ref().map(|proof|proof.source_id())}
    pub(crate) fn run_many_proven(fences:&[Self],invalidate:bool,operation:&mut dyn FnMut()->Result<()>)->Result<()>{
        if fences.is_empty(){return Err(NarouError::Platform("record commit has no writer fence".into()));}
        fences[0].proof.as_ref().ok_or_else(||NarouError::Platform("writer token has no in-memory gate proof".into()))?.run_many(fences,invalidate,operation)
    }
}

/// Component-boundary overlap, never a lexical starts-with namespace test.
pub fn namespace_prefixes_overlap(left:&str,right:&str)->bool{
    let left=left.trim_end_matches('/');let right=right.trim_end_matches('/');
    left==right||left.strip_prefix(right).is_some_and(|rest|rest.starts_with('/'))||right.strip_prefix(left).is_some_and(|rest|rest.starts_with('/'))
}

/// One `pending_removals` page: `(gate prefix, removal owner, closed epoch)`
/// rows plus the continuation cursor when the scan was bounded.
pub type PendingRemovalPage=(Vec<(ObjectPrefix,String,u64)>,Option<String>);

pub trait NovelAccessFence: PlatformService {
    fn capture_writer<'a>(&'a self, prefix: &'a ObjectPrefix) -> PlatformFuture<'a, Result<WriteFence>> {
        Box::pin(async move {
            let info = self.gate_info(prefix).await?;
            if info.state != GateState::Open { return Err(novel_busy_error(prefix)); }
            Ok(WriteFence::new(prefix.clone(),info.epoch))
        })
    }
    /// Reattach a persisted operation's exact epoch under the backend's atomic
    /// admission boundary. Never upgrade an old operation to the current epoch.
    fn rebind_writer<'a>(&'a self,prefix:&'a ObjectPrefix,expected_epoch:u64)->PlatformFuture<'a,Result<WriteFence>>{
        let _=(prefix,expected_epoch);Box::pin(async{Err(NarouError::Platform("atomic expected-epoch writer binding unsupported".into()))})
    }
    /// Close before cleanup using the ORIGINAL admitted epoch, persisting the
    /// self-contained removal owner in the same atomic gate transition.
    fn retire_writer<'a>(&'a self,fence:&'a WriteFence,owner:&'a str)->PlatformFuture<'a,Result<u64>>{let _=(fence,owner);Box::pin(async{Err(NarouError::Platform("closed-owner removal unsupported".into()))})}
    /// Bounded gate scan. An empty result with a cursor is NOT end-of-input.
    fn pending_removals<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<PendingRemovalPage>>{let _=(cursor,limit);Box::pin(async{Err(NarouError::Platform("pending removal enumeration unsupported".into()))})}
    /// Admit a reader. Succeeds only while the gate is `Open`; returns the
    /// epoch pinned at admission. A closed gate returns
    /// `novel_recovering`/`novel_busy`-class errors, never a lease.
    fn admit_reader<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        reader_id: &'a str,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<GateLease>>;

    /// Renew an existing reader lease. This is the *same* CAS condition as
    /// admission — gate open in the same epoch — so renewal after
    /// `close_gate` is denied and the reader lapses.
    fn renew_reader<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        lease: &'a GateLease,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<GateLease>>;

    /// Release early; lapsed leases also expire by TTL. Idempotent.
    fn release_reader<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        lease: &'a GateLease,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Current gate state, including live reader count.
    fn gate_info<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
    ) -> PlatformFuture<'a, Result<GateInfo>>;

    /// Close admission under `owner` and return the new writer epoch.
    /// Idempotent for the same owner (a retried close returns the existing
    /// epoch). A different live owner's closed gate is
    /// `gate_owner_conflict`. Closing never waits for readers — it only
    /// stops new admission; the caller drains via `gate_info`.
    fn close_gate<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        owner: &'a str,
    ) -> PlatformFuture<'a, Result<u64>>;

    /// Heartbeat the writer epoch so a crashed-looking but alive writer is
    /// not superseded by a later close attempt.
    fn renew_gate<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        owner: &'a str,
        epoch: u64,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Re-open admission after commit or finished recovery. Requires the
    /// `(owner, epoch)` that closed the gate — a superseded writer cannot
    /// reopen a gate a successor now owns.
    fn open_gate<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        owner: &'a str,
        epoch: u64,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Canonical install of one staged file, epoch-guarded.
    ///
    /// * FS: verifies `(owner, epoch)` under the non-stealable OS lock, then
    ///   renames `stage` → `canonical` inside the same critical section.
    ///   Rename failure due to an open handle is `novel_busy` (honest
    ///   Windows behaviour); the pre-image stays intact.
    /// * SQLite/D1: one transaction/batch — gate CAS + the canonical
    ///   object/chunk install + `objects.generation` bump.
    ///
    /// Success also removes the staged bytes. A caller that lost its epoch
    /// gets an error and must recover, never retry as a different epoch.
    fn commit_staged<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        owner: &'a str,
        epoch: u64,
        stage: &'a ObjectKey,
        canonical: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Roll back a newly installed private novel path only if its bytes still
    /// match. Shared hash-pool objects must never be removed by rollback.
    fn remove_committed<'a>(
        &'a self, prefix: &'a ObjectPrefix, owner: &'a str, epoch: u64,
        canonical: &'a ObjectKey, expected_sha256: &'a str,
    ) -> PlatformFuture<'a, Result<()>>;
}

/// Shared error helpers with stable machine prefixes.
pub fn gate_closed_error(prefix: &ObjectPrefix) -> NarouError {
    NarouError::Platform(format!(
        "novel_recovering: admission closed for {}",
        prefix.as_ref()
    ))
}


/// Deterministic digest of a parent's ordered member set, for plan CAS.
/// Member order is significant and the caller supplies it; the digest
/// changes iff the ordered membership does — unrelated edits to other
/// parents never invalidate a plan.
pub fn membership_digest<S:AsRef<str>>(members: impl IntoIterator<Item = S>) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    // length-prefixed fields: "ab|c" must never equal "a|bc"
    for member in members {
        let bytes = member.as_ref().as_bytes();
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}
pub fn gate_epoch_error(prefix: &ObjectPrefix, expected: u64, actual: u64) -> NarouError {
    NarouError::Platform(format!(
        "novel_epoch_conflict: {} epoch {actual} != {expected}",
        prefix.as_ref()
    ))
}

pub fn gate_owner_error(prefix: &ObjectPrefix, owner: &str) -> NarouError {
    NarouError::Platform(format!(
        "gate_owner_conflict: {} is owned by {owner}",
        prefix.as_ref()
    ))
}

pub fn novel_busy_error(prefix: &ObjectPrefix) -> NarouError {
    NarouError::Platform(format!("novel_busy: {}", prefix.as_ref()))
}


/// A reader lease token that is missing or past `expires_at` even though the
/// gate itself is open at the expected epoch. Kept distinct from
/// [`gate_closed_error`] (`novel_recovering`) so callers can tell an expired
/// admission apart from a real gate closure.
pub fn lease_expired_error(prefix: &ObjectPrefix) -> NarouError {
    NarouError::Platform(format!(
        "novel_lease_expired: reader lease expired for {}",
        prefix.as_ref()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_digest_is_order_and_boundary_sensitive() {
        let a = membership_digest(["one", "two"].iter().copied());
        let b = membership_digest(["one", "two"].iter().copied());
        let reordered = membership_digest(["two", "one"].iter().copied());
        let concatenated = membership_digest(["onetwo"].iter().copied());
        let empty = membership_digest(std::iter::empty::<&str>());
        assert_eq!(a, b);
        assert_ne!(a, reordered);
        assert_ne!(a, concatenated);
        assert_ne!(a, empty);
    }
}
