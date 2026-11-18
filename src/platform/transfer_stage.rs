//! Durable, resumable staged uploads in the `.narou-transfer/` namespace.
//!
//! A staged upload is the storage-side half of a resumable transfer file:
//! bytes are appended in order, the running SHA-256 state is persisted with
//! every confirmed append, and `seal` freezes the object only after the whole
//! file's size and digest match the caller's expectation. Process death,
//! Worker isolate exchange or a lost HTTP response never lose confirmed
//! bytes — the durable ledger is the truth, never the client's counters.
//!
//! Namespace layout (all below [`TRANSFER_PREFIX`]):
//!
//! * `.narou-transfer/data/<stage_id>`   — the staged payload bytes
//! * `.narou-transfer/.ledger/<stage_id>` — owner/epoch/hash ledger record
//! * `.narou-transfer/.parts/<stage_id>/<page>` — S3 multipart part log
//! * `.narou-transfer/.gate/<hash>` / `.lock/<hash>` — access-fence state
//! * `.narou-transfer/.revisions/<hash>` — YAML-mode revision ledger
//!
//! The `.narou-staging/` inactivity TTL collector must never touch these
//! keys; reclamation is owner-based via [`TransferStageStore::stage_lease_page`]
//! and [`TransferStageStore::stage_reclaim`].
//!
//! # Idempotency and crash windows
//!
//! `stage_append` requires `offset == confirmed_bytes`: strictly-linear
//! durable writes. A caller retrying an already-receipted chunk dedups via
//! its own chunk journal *before* calling here. If the crash lands between
//! the stage append and the caller's receipt write, the caller re-reads
//! [`stage_cursor`](TransferStageStore::stage_cursor) or rewinds the stage to
//! a checkpoint it trusts with
//! [`stage_rewind`](TransferStageStore::stage_rewind) — never by re-reading a
//! multi-GiB file to rebuild hash state.
//!
//! On S3 the data object is a multipart upload: each append is one part
//! (non-final appends must satisfy the 5 MiB part floor,
//! [`stage_append_min`](TransferStageStore::stage_append_min)), part ETags
//! persist under `.parts/` before the ledger CAS advances, and `seal`
//! completes the upload. A late `CompleteMultipartUpload` can only ever
//! produce the *staged* object — it cannot overwrite canonical keys because
//! canonical publication always goes through an epoch-guarded commit, never
//! a direct mutable PUT.

use std::ops::Range;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sha2::digest::common::hazmat::{SerializableState, SerializedState};

use crate::error::{NarouError, Result};

use super::object_store::{AssetStream, AssetStore, ObjectKey, ObjectStore, ObjectUpdateResult};
use super::PlatformFuture;

/// Transfer-owned namespace. Everything below it is reclaimed only by
/// owner-lease GC, never by the `.narou-staging/` inactivity sweeper.
pub const TRANSFER_PREFIX: &str = ".narou-transfer/";
pub const LEDGER_PREFIX: &str = ".narou-transfer/.ledger/";
pub const PARTS_PREFIX: &str = ".narou-transfer/.parts/";
pub(crate) const DATA_PREFIX: &str = ".narou-transfer/data/";

/// Hash-checkpoint algorithm tag persisted next to [`StageCursor::hash_state`].
/// Version 1 is `sha2` 0.11 [`SerializableState`] of `Sha256`, verbatim bytes.
pub const HASH_CHECKPOINT_ALG: &str = "sha256-v1";

/// Per-request append bound. Matches the wire chunk size (8 MiB − 64 KiB is
/// the negotiated payload; the stage itself accepts whole 8 MiB appends).
pub const STAGE_APPEND_MAX: usize = 8 * 1024 * 1024;
/// S3 multipart requires every non-final part to be at least 5 MiB.
pub const S3_STAGE_PART_MIN: usize = 5 * 1024 * 1024;
/// Upper bound on CAS retries for ledger mutation.
const LEDGER_CAS_ATTEMPTS: usize = 8;

/// The payload object key for a stage id. `stage_id` is caller-chosen and
/// validated as a single key component (sessions bind it to file_id).
pub fn stage_data_key(stage_id: &str) -> Result<ObjectKey> {
    stage_component(stage_id)?;
    ObjectKey::try_new(format!("{DATA_PREFIX}{stage_id}"))
}

/// The ledger object key for a stage id.
pub fn stage_ledger_key(stage_id: &str) -> Result<ObjectKey> {
    stage_component(stage_id)?;
    ObjectKey::try_new(format!("{LEDGER_PREFIX}{stage_id}"))
}

pub fn stage_checkpoint_key(stage_id:&str,offset:u64)->Result<ObjectKey>{
    stage_component(stage_id)?;ObjectKey::try_new(format!(".narou-transfer/.checkpoints/{stage_id}/{offset:020}"))
}

/// One durable part record key (S3 multipart ETag journal).
pub fn parts_key(stage_id: &str, part: u32) -> Result<ObjectKey> {
    stage_component(stage_id)?;
    ObjectKey::try_new(format!("{PARTS_PREFIX}{stage_id}/{part:08}"))
}


fn stage_component(stage_id: &str) -> Result<()> {
    if stage_id.is_empty()
        || stage_id.contains('/')
        || stage_id.contains('\\')
        || stage_id.len() > 128
        || !stage_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(NarouError::Platform(format!(
            "invalid stage id: {stage_id:?}"
        )));
    }
    Ok(())
}

/// Pool names encode SHA-256 as either hex or URL-safe base64.
pub fn hash_named_digest(key:&ObjectKey)->Option<String>{
    use base64::Engine;
    if !super::split_store::is_illustration_key(key)&&!key.as_ref().starts_with("illustrations/"){return None;}
    let filename=key.as_ref().rsplit('/').next()?;let stem=filename.rsplit_once('.').map_or(filename,|(stem,_)|stem);
    if stem.len()==64&&stem.bytes().all(|byte|byte.is_ascii_hexdigit()){return Some(stem.to_ascii_lowercase());}
    if stem.len()==43{return base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(stem).ok().filter(|bytes|bytes.len()==32).map(hex::encode);}
    None
}
/// Stable physical pool key, independent of a future dedup configuration.
pub fn illustration_pool_key(logical:&ObjectKey)->Result<ObjectKey>{
    use base64::Engine;
    if !super::is_illustration_key(logical){return Err(NarouError::Platform("legacy retirement requires an illustration key".into()));}
    let digest=hash_named_digest(logical).ok_or_else(||NarouError::Platform("legacy illustration is not hash named".into()))?;
    let mut bytes=[0u8;32];hex::decode_to_slice(digest,&mut bytes).map_err(|error|NarouError::Platform(error.to_string()))?;
    let extension=logical.as_ref().rsplit_once('.').map(|(_,extension)|extension).ok_or_else(||NarouError::Platform("illustration extension missing".into()))?;
    ObjectKey::try_new(format!("illustrations/{}.{}",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),extension))
}
pub async fn verify_illustration_pool(store:&dyn AssetStore,logical:&ObjectKey,pool:&ObjectKey,size:u64,sha:&str)->Result<()>{
    if illustration_pool_key(logical)?!=*pool{return Err(NarouError::Platform("retirement pool key differs from logical image".into()));}check_hash_named_key(pool,sha)?;
    let mut stream=store.read_stream(pool).await?.ok_or_else(||NarouError::Platform("verified pool object is missing".into()))?;let mut hash=sha2::Sha256::new();let mut seen=0u64;
    while let Some(chunk)=stream.next().await{let bytes=chunk?;seen=seen.checked_add(bytes.len() as u64).filter(|value|*value<=size).ok_or_else(||NarouError::Platform("pool object exceeds expected size".into()))?;hash.update(bytes);}
    if seen!=size||!hex::encode(hash.finalize()).eq_ignore_ascii_case(sha){return Err(NarouError::Platform("pool size/SHA verification failed".into()));}Ok(())
}

pub fn check_hash_named_key(key: &ObjectKey, expected_sha256: &str) -> Result<()> {
    use base64::Engine;
    let filename = key.as_ref().rsplit('/').next().unwrap_or_default();
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let named = if stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(stem.to_ascii_lowercase())
    } else if stem.len() == 43 {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(stem).ok()
            .filter(|bytes| bytes.len() == 32).map(hex::encode)
    } else { None };
    if named.is_some_and(|digest| !digest.eq_ignore_ascii_case(expected_sha256)) {
        return Err(NarouError::Platform("hash-named key differs from payload SHA-256".into()));
    }
    Ok(())
}

/// Durable position inside a staged upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageCursor {
    /// Bytes durably persisted in the stage.
    pub confirmed_bytes: u64,
    /// Serialized `Sha256` state covering exactly `confirmed_bytes`
    /// (`HASH_CHECKPOINT_ALG`); `None` only when the checkpoint is unreadable
    /// and the file must restart from zero — callers then drop the stage's
    /// prior bytes via rewind-to-zero or abort.
    pub hash_state: Option<Vec<u8>>,
    /// The stage was sealed; appends are refused.
    pub sealed: bool,
    pub crc32: u32,
    /// Backend checkpoint cursor (next multipart part number on S3).
    pub storage_position: u32,
}

impl StageCursor {
    /// A receipt supplies SHA only; storage resolves its durable CRC/part cursor
    /// at that byte boundary rather than trusting fabricated backend state.
    pub fn from_sha_checkpoint(confirmed_bytes:u64,hash_state:Option<Vec<u8>>)->Self{
        Self{confirmed_bytes,hash_state,sealed:false,crc32:0,storage_position:0}
    }

    /// Hex SHA-256 of the confirmed prefix — the server half of the WASM
    /// `confirmed_prefix_sha256` resume contract. `None` when unconfirmed or
    /// when the checkpoint is unavailable.
    pub fn prefix_sha256(&self) -> Option<String> {
        if self.confirmed_bytes == 0 {
            return Some(hex::encode(sha2::Sha256::digest([])));
        }
        let state = self.hash_state.as_deref()?;
        let hasher = decode_sha_state(state).ok()?;
        Some(hex::encode(hasher.finalize()))
    }

    /// Rebuild a live hasher at this checkpoint. Used to continue hashing
    /// appended bytes without re-reading the file.
    pub fn resume_hasher(&self) -> Result<sha2::Sha256> {
        match self.hash_state.as_deref() {
            Some(state) => decode_sha_state(state),
            None if self.confirmed_bytes == 0 => Ok(sha2::Sha256::new()),
            None => Err(NarouError::Platform(
                "stage_checkpoint_invalid: restart this unsealed file".into(),
            )),
        }
    }
}

/// Encode a `Sha256` mid-stream state. These bytes are server-internal;
/// they are never accepted from a client.
pub fn encode_sha_state(hasher: &sha2::Sha256) -> Vec<u8> {
    hasher.serialize().as_slice().to_vec()
}

/// Decode a [`HASH_CHECKPOINT_ALG`] state. Corrupt or foreign-versioned
/// states are errors — the affected unconfirmed file restarts from zero.
pub fn decode_sha_state(state: &[u8]) -> Result<sha2::Sha256> {
    let state = SerializedState::<sha2::Sha256>::try_from(state)
        .map_err(|_| NarouError::Platform("stage_checkpoint_invalid: restart this unsealed file".into()))?;
    sha2::Sha256::deserialize(&state)
        .map_err(|_| NarouError::Platform("stage_checkpoint_invalid: restart this unsealed file".into()))
}

/// Durable stage record. Written in the same atomic boundary as the bytes it
/// confirms (DB: same transaction/batch; FS: same OS lock; S3: conditional
/// object write). GC decisions read this, never the data object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageLease {
    pub stage_id: String,
    /// Owning session/publication identity. Reclaim requires the *same*
    /// owner and epoch to have gone terminal — time alone never reassigns.
    pub owner: String,
    /// Owner epoch. Bump on owner lease renewal so a stale GC observation
    /// cannot reclaim a stage whose owner renewed in the meantime.
    pub epoch: u64,
    pub expires_at: i64,
    pub state: StageLeaseState,
    /// RFC 3339 creation time (informational; leases compare `expires_at`).
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageLeaseState {
    Writing,
    Sealing,
    Sealed,
    Aborting,
}

/// Full ledger record — the lease plus the confirmed write position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageLedgerRecord {
    #[serde(flatten)]
    pub lease: StageLease,
    pub confirmed_bytes: u64,
    /// Hex-encoded [`HASH_CHECKPOINT_ALG`] bytes.
    pub hash_state_hex: Option<String>,
    #[serde(default)]
    pub hash_algorithm:String,
    /// For S3 multipart: next part number and the upload id. For DB stages
    /// `next_part` tracks the next object_chunks seq *boundary* implicitly
    /// (derived from confirmed_bytes) and `upload_id` stays `None`.
    pub next_part: u32,
    pub upload_id: Option<String>,
    /// CRC-32 of the confirmed raw prefix (the public digest remains SHA-256).
    #[serde(default)]
    pub crc32: u32,
    #[serde(default)]
    pub final_received: bool,
    /// Claim before external I/O; identical retries may finish the same append.
    #[serde(default)]
    pub pending_append: Option<PendingAppend>,
    #[serde(default)]
    pub publication_target: Option<String>,
    #[serde(default)]
    pub publication_upload_id: Option<String>,
    /// Immutable physical target, never a canonical novel key. Only the S3
    /// adapter sets this before its first append; completion is conditional.
    #[serde(default)]
    pub direct_target:Option<String>,
    /// Generation of the verified sealed S3 bytes; never an inferred checksum.
    #[serde(default)]
    pub sealed_etag:Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAppend {
    pub offset: u64,
    pub size: u64,
    pub sha256: String,
    pub final_chunk: bool,
}

impl StageLedgerRecord {
    pub fn new(stage_id: &str, owner: &str, epoch: u64, lease_secs: i64) -> Result<Self> {
        stage_data_key(stage_id)?;
        let now = now_secs();
        Ok(Self {
            lease: StageLease { stage_id: stage_id.into(), owner: owner.into(), epoch,
                expires_at: now.saturating_add(lease_secs.max(1)), state: StageLeaseState::Writing, created_at: now },
            confirmed_bytes: 0, hash_state_hex: Some(hex::encode(encode_sha_state(&sha2::Sha256::new()))),hash_algorithm:HASH_CHECKPOINT_ALG.into(),
            next_part: 1, upload_id: None, crc32: 0, final_received: false, pending_append: None,
            publication_target: None, publication_upload_id: None,direct_target:None,sealed_etag:None,
        })
    }
    pub fn payload_key(&self)->Result<ObjectKey>{self.direct_target.as_deref().map(ObjectKey::try_new).unwrap_or_else(||stage_data_key(&self.lease.stage_id))}

    pub fn append(&mut self, owner: &str, epoch: u64, offset: u64, payload: &[u8], final_chunk: bool) -> Result<()> {
        require_writable(self, owner, epoch)?;
        if self.final_received || self.confirmed_bytes != offset {
            return Err(NarouError::Platform("stage_offset_conflict".into()));
        }
        if payload.len() > STAGE_APPEND_MAX || (payload.is_empty() && !final_chunk) {
            return Err(NarouError::Platform("invalid stage append length".into()));
        }
        let next = offset.checked_add(payload.len() as u64).filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| NarouError::Platform("stage size overflow".into()))?;
        let mut hasher = self.cursor().resume_hasher()?;
        hasher.update(payload);
        let mut crc = crc32fast::Hasher::new_with_initial(self.crc32);
        crc.update(payload);
        self.crc32 = crc.finalize();
        self.confirmed_bytes = next;
        self.hash_state_hex = Some(hex::encode(encode_sha_state(&hasher)));
        self.hash_algorithm=HASH_CHECKPOINT_ALG.into();
        self.final_received = final_chunk;
        self.pending_append = None;
        Ok(())
    }

    pub fn rewind(&mut self, owner: &str, epoch: u64, checkpoint: &StageCursor) -> Result<()> {
        require_writable(self, owner, epoch)?;
        if checkpoint.confirmed_bytes > self.confirmed_bytes || checkpoint.sealed {
            return Err(NarouError::Platform("invalid stage rewind boundary".into()));
        }
        let hasher = checkpoint.resume_hasher()?;
        self.confirmed_bytes = checkpoint.confirmed_bytes;
        self.hash_state_hex = Some(hex::encode(encode_sha_state(&hasher)));
        self.hash_algorithm=HASH_CHECKPOINT_ALG.into();
        self.crc32 = checkpoint.crc32;
        self.final_received = false;
        self.pending_append = None;
        Ok(())
    }

    pub fn verify_seal(&self, owner: &str, epoch: u64, size: u64, sha256: &str) -> Result<SealedAsset> {
        require_owner(self, owner, epoch)?;
        if self.lease.state == StageLeaseState::Aborting || self.pending_append.is_some()
            || !self.final_received || self.confirmed_bytes != size
            || !self.cursor().prefix_sha256().is_some_and(|value| value.eq_ignore_ascii_case(sha256)) {
            return Err(NarouError::Platform("stage size/SHA/final checkpoint mismatch".into()));
        }
        Ok(SealedAsset { key: self.payload_key()?, size, sha256: sha256.to_ascii_lowercase() })
    }

    pub fn cursor(&self) -> StageCursor {
        StageCursor {
            confirmed_bytes: self.confirmed_bytes,
            hash_state: if self.hash_algorithm==HASH_CHECKPOINT_ALG{self.hash_state_hex.as_deref().and_then(|state|hex::decode(state).ok())}else{None},
            sealed: self.lease.state == StageLeaseState::Sealed,
            crc32: self.crc32,
            storage_position: self.next_part,
        }
    }

    pub fn lease(&self) -> StageLease {
        self.lease.clone()
    }
}

pub fn decode_ledger(bytes: &[u8]) -> Result<StageLedgerRecord> {
    serde_json::from_slice(bytes)
        .map_err(|error| NarouError::Platform(format!("invalid stage ledger: {error}")))
}

pub fn encode_ledger(record: &StageLedgerRecord) -> Result<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|error| NarouError::Platform(format!("stage ledger encode: {error}")))
}

/// A sealed staged object: the byte identity the caller verified plus the
/// canonical-addressable key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedAsset {
    pub key: ObjectKey,
    pub size: u64,
    pub sha256: String,
}

/// Result of a conditional write-once store of a hash-named object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOnce {
    /// The object did not exist and the write committed.
    Written,
    /// The object already existed with byte-identical content and was
    /// reused. Distinct content is an error, never an overwrite.
    Reused,
}

/// Durable staged-upload primitive. Implemented by every store that can act
/// as a transfer staging target; `AssetStore` is a supertrait so one
/// `Arc<dyn TransferStageStore>` serves both ordinary asset calls and the
/// resumable protocol.
///
/// Every mutating call takes `(owner, epoch)` and is rejected unless the
/// persisted ledger still carries that pair — a dead or superseded owner
/// cannot seal, abort or append. `epoch` is bumped by `stage_touch`/`open`
/// renewals driven by the owning session's lease.
pub trait TransferStageStore: AssetStore {
    /// Minimum payload size for a non-final `stage_append`. 0 on byte
    /// backends; [`S3_STAGE_PART_MIN`] on S3 multipart.
    fn stage_append_min(&self) -> usize {
        0
    }

    /// Maximum payload size of one `stage_append`.
    fn stage_append_max(&self) -> usize {
        STAGE_APPEND_MAX
    }

    /// Create or resume the ledger for `stage_id` under `(owner, epoch)`.
    ///
    /// * absent ledger → create (owner+epoch+expiry recorded **before** any
    ///   data/object is written) and return an empty cursor.
    /// * live ledger with the same `(owner, epoch)` → return the durable
    ///   cursor (resume).
    /// * live ledger with a different owner or a mismatched epoch → error
    ///   (`stage_owner_conflict`); callers re-plan rather than steal.
    fn stage_open<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<StageCursor>>;

    /// Read the durable cursor without admitting ownership.
    fn stage_cursor<'a>(
        &'a self,
        stage_id: &'a str,
    ) -> PlatformFuture<'a, Result<Option<StageCursor>>>;

    /// Append `payload` at `offset`. `offset` must equal the durable
    /// `confirmed_bytes`; any other value is `stage_offset_conflict` and
    /// never partially applied. The data write and the ledger update
    /// (bytes + SHA-256 checkpoint + owner/epoch CAS) commit in the same
    /// atomic boundary — on backends without a shared transaction (S3, FS)
    /// the data/part write lands first and a lost ledger update is
    /// reconciled by [`stage_rewind`](Self::stage_rewind) or by replaying
    /// the same payload at the same deterministic position.
    ///
    /// `final_chunk` marks the file's last append: only then may the
    /// payload be smaller than `stage_append_min` or empty (an empty file
    /// is `final` with a zero-length payload).
    fn stage_append<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        offset: u64,
        payload: &'a [u8],
        final_chunk: bool,
    ) -> PlatformFuture<'a, Result<StageCursor>>;

    /// Truncate the stage back to a trusted `checkpoint` and restore its
    /// hash state. Used when the caller's receipt journal is behind the
    /// stage (crash between append and receipt) or when the caller decides
    /// an unconfirmed tail is untrusted. Only `checkpoint.hash_state`
    /// cursors may be targets; a missing state rewinds to byte 0.
    fn stage_rewind<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        checkpoint: &'a StageCursor,
    ) -> PlatformFuture<'a, Result<StageCursor>>;

    /// Bounded (≤ 64 KiB items) stream over an exclusive-end byte range of
    /// the stage. Reading requires a sealed stage — unsealed bytes are
    /// never served to readers (`Ok(None)` when the ledger is absent or
    /// still open).
    fn stage_read<'a>(
        &'a self,
        stage_id: &'a str,
        range: Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>>;

    /// Renew the owner's expiry in one CAS. Reclamation compares the exact
    /// ledger revision/expiry as well as owner+epoch, so renewal wins against
    /// a stale GC observation. Only the owning session assigns owner epochs.
    fn stage_touch<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Seal: verify the durable byte count and whole-file SHA-256 against
    /// `expected_size`/`expected_sha256` (hex), complete the backend commit
    /// (multipart complete / row finalize), and mark the stage immutable.
    /// Idempotent for `(owner, epoch)`: replaying a seal after a lost ACK
    /// returns the same [`SealedAsset`]. Size/SHA mismatch is an error and
    /// never seals.
    fn stage_seal<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        expected_size: u64,
        expected_sha256: &'a str,
    ) -> PlatformFuture<'a, Result<SealedAsset>>;

    /// Abort: remove data and ledger. Refused for a different or newer
    /// owner. Abandoned data that outlives this call is reclaimed by the
    /// owner-lease collector.
    fn stage_abort<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Page through every stage lease for owner-based GC. The caller
    /// decides liveness against its own session ledger; this list alone
    /// never deletes.
    fn stage_lease_page<'a>(
        &'a self,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<(Vec<StageLease>, Option<String>)>>;

    /// Reclaim one stage whose owner is confirmed dead. The CAS requires
    /// the recorded `(owner, epoch)` to still match — an owner that renewed
    /// between the listing and the reclaim wins. `true` means all owned stage
    /// resources are gone (also true on replay after deletion); `false` keeps
    /// the owner's GC row reachable while cleanup pages or pins remain.
    /// Explicit abort may mark `Aborting` for immediate terminal-owner cleanup.
    fn stage_reclaim<'a>(
        &'a self,
        stage_id: &'a str,
        expected_owner: &'a str,
        expected_epoch: u64,
    ) -> PlatformFuture<'a, Result<bool>>;
}

/// Consume a backpressured stream into the resumable port without whole-file
/// buffering. The caller reserves `id`/owner before invoking this operation.
pub async fn stage_asset_stream<S: TransferStageStore + ?Sized>(store:&S,id:&str,owner:&str,epoch:u64,ttl:i64,stream:AssetStream)->Result<SealedAsset>{
    let mut cursor=store.stage_open(id,owner,epoch,ttl).await?;
    if cursor.confirmed_bytes!=0 || cursor.sealed {return Err(NarouError::Platform("stream stage identity already used".into()));}
    let mut input=bounded_chunks(stream);
    let mut buffer=Vec::with_capacity(STAGE_APPEND_MAX-64*1024);let capacity=STAGE_APPEND_MAX-64*1024;
    while let Some(chunk)=input.next().await{
        let chunk=chunk?;let mut remaining=chunk.as_slice();
        while !remaining.is_empty(){
            if buffer.len()==capacity{cursor=store.stage_append(id,owner,epoch,cursor.confirmed_bytes,&buffer,false).await?;buffer.clear();}
            let size=remaining.len().min(capacity-buffer.len());buffer.extend_from_slice(&remaining[..size]);remaining=&remaining[size..];
        }
    }
    cursor=store.stage_append(id,owner,epoch,cursor.confirmed_bytes,&buffer,true).await?;
    let sha=cursor.prefix_sha256().ok_or_else(||NarouError::Platform("stream stage lost hash checkpoint".into()))?;
    store.stage_seal(id,owner,epoch,cursor.confirmed_bytes,&sha).await
}

// ---------------------------------------------------------------------------
// Shared ledger plumbing
// ---------------------------------------------------------------------------

/// Run `mutate` against the ledger record with CAS retries through
/// [`ObjectStore::update_small`]. Returns the committed record.
///
/// Backends whose `update_small` is a genuine atomic boundary (FS lock,
/// SQLite transaction, D1 guarded batch, in-memory mutex) satisfy the CAS
/// contract. S3 does not implement `update_small` — its ledger uses the
/// ETag-conditional helper on [`super::s3_store::S3Store`] instead.
pub async fn ledger_mutate<S, F>(
    store: &S,
    stage_id: &str,
    mut mutate: F,
) -> Result<StageLedgerRecord>
where
    S: ObjectStore + ?Sized,
    F: FnMut(Option<StageLedgerRecord>) -> Result<StageLedgerRecord> + Send,
{
    let key = stage_ledger_key(stage_id)?;
    for _ in 0..LEDGER_CAS_ATTEMPTS {
        let mut outcome: Option<StageLedgerRecord> = None;
        let mut mutator =
            |current: Option<&[u8]>| -> Result<Option<std::borrow::Cow<'_, [u8]>>> {
                let record = match current {
                    Some(bytes) => Some(decode_ledger(bytes)?),
                    None => None,
                };
                let next = mutate(record)?;
                let bytes = encode_ledger(&next)?;
                outcome = Some(next);
                Ok(Some(std::borrow::Cow::Owned(bytes)))
            };
        match store.update_small(&key, &mut mutator).await? {
            ObjectUpdateResult::Committed => {
                return outcome.ok_or_else(|| {
                    NarouError::Platform("stage ledger commit lost its record".into())
                });
            }
            ObjectUpdateResult::Conflict => continue,
            ObjectUpdateResult::ProjectionFailed(error) => return Err(error),
        }
    }
    Err(NarouError::Platform(
        "stage ledger CAS exhausted retries".into(),
    ))
}

/// Read a ledger record; `Ok(None)` when absent.
pub async fn ledger_read<S: ObjectStore + ?Sized>(
    store: &S,
    stage_id: &str,
) -> Result<Option<StageLedgerRecord>> {
    let key = stage_ledger_key(stage_id)?;
    let Some(bytes) = store.read_small(&key).await? else {
        return Ok(None);
    };
    Ok(Some(decode_ledger(&bytes)?))
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Common `(owner, epoch)` precondition check used by every mutating call.
pub fn require_owner(record: &StageLedgerRecord, owner: &str, epoch: u64) -> Result<()> {
    if record.lease.owner != owner {
        return Err(NarouError::Platform(format!(
            "stage_owner_conflict: stage {} is owned by {}",
            record.lease.stage_id, record.lease.owner
        )));
    }
    if record.lease.epoch != epoch {
        return Err(NarouError::Platform(format!(
            "stage_epoch_conflict: stage {} epoch {} != {}",
            record.lease.stage_id, record.lease.epoch, epoch
        )));
    }
    Ok(())
}

pub fn require_writable(record: &StageLedgerRecord, owner: &str, epoch: u64) -> Result<()> {
    require_owner(record, owner, epoch)?;
    if record.lease.state != StageLeaseState::Writing {
        return Err(NarouError::Platform(format!(
            "stage already sealed: {}", record.lease.stage_id
        )));
    }
    Ok(())
}

/// Shared GC listing: read every `.ledger/` object page and decode.
pub async fn lease_page<S: ObjectStore + ?Sized>(
    store: &S,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<StageLease>, Option<String>)> {
    let prefix = super::object_store::ObjectPrefix::new(LEDGER_PREFIX)?;
    let mut request = super::object_store::ObjectListRequest::new(
        prefix,
        std::num::NonZeroUsize::new(limit.max(1)).expect("nonzero"),
    );
    if let Some(cursor) = cursor {
        request = request.after(cursor.to_string());
    }
    let page = store.list_page(&request).await?;
    let mut leases = Vec::with_capacity(page.objects.len());
    for object in page.objects {
        let Some(bytes) = store.read_small(&object.key).await? else {
            continue;
        };
        match decode_ledger(&bytes) {
            Ok(record) => leases.push(record.lease()),
            // Corrupt ledgers are skipped, not deleted: a JSON bug must never
            // masquerade as a dead owner and free live bytes.
            Err(_) => continue,
        }
    }
    Ok((leases, page.next_cursor))
}

/// Split a backend stream into ≤ 64 KiB items without changing its bytes.
pub fn bounded_chunks(stream: AssetStream) -> AssetStream {
    struct Pending {
        buf: Vec<u8>,
        offset: usize,
    }
    Box::pin(futures::stream::try_unfold(
        (stream, Pending { buf: Vec::new(), offset: 0 }),
        |(mut stream, mut pending)| async move {
            loop {
                if pending.offset < pending.buf.len() {
                    if pending.offset==0&&pending.buf.len()<=super::staging::ASSET_CHUNK_BYTES{return Ok(Some((std::mem::take(&mut pending.buf),(stream,pending))));}
                    let end = (pending.offset + super::staging::ASSET_CHUNK_BYTES)
                        .min(pending.buf.len());
                    let out = pending.buf[pending.offset..end].to_vec();
                    pending.offset = end;
                    if pending.offset == pending.buf.len() {
                        pending.buf.clear();
                        pending.offset = 0;
                    }
                    return Ok(Some((out, (stream, pending))));
                }
                match stream.next().await {
                    Some(chunk) => {
                        let chunk = chunk?;
                        if chunk.is_empty() {
                            continue;
                        }
                        pending.buf = chunk;
                        pending.offset = 0;
                    }
                    None => return Ok(None),
                }
            }
        },
    ))
}

/// Default `stage_read` for backends where the data object is readable by
/// `read_stream_range` only after seal (and is simply absent before then).
pub async fn sealed_stage_read<S: TransferStageStore + ObjectStore + ?Sized>(
    store: &S,
    stage_id: &str,
    range: Range<u64>,
) -> Result<Option<AssetStream>> {
    let Some(record) = ledger_read(store, stage_id).await? else {
        return Ok(None);
    };
    if record.lease.state != StageLeaseState::Sealed {
        return Ok(None);
    }
    let key = record.payload_key()?;
    store.read_stream_range(&key, range).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_state_roundtrip_and_prefix() {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"hello ");
        let mid = encode_sha_state(&hasher);
        let mut resumed = decode_sha_state(&mid).unwrap();
        resumed.update(b"world");
        let mut expect = sha2::Sha256::new();
        expect.update(b"hello world");
        assert_eq!(
            <[u8; 32]>::from(resumed.finalize()),
            <[u8; 32]>::from(expect.finalize())
        );

        let cursor = StageCursor {
            confirmed_bytes: 6,
            hash_state: Some(mid),
            sealed: false,
            crc32: 0,
            storage_position: 1,
        };
        let mut check = sha2::Sha256::new();
        check.update(b"hello ");
        assert_eq!(
            cursor.prefix_sha256().unwrap(),
            hex::encode(check.finalize())
        );
        // corrupt checkpoint never fabricates a prefix digest
        let bad = StageCursor {
            confirmed_bytes: 6,
            hash_state: Some(vec![1, 2, 3]),
            sealed: false,
            crc32: 0,
            storage_position: 1,
        };
        assert!(bad.prefix_sha256().is_none());
        assert!(bad.resume_hasher().is_err());
        // empty file has the digest of nothing and needs no checkpoint
        let empty = StageCursor {
            confirmed_bytes: 0,
            hash_state: None,
            sealed: false,
            crc32: 0,
            storage_position: 1,
        };
        assert_eq!(
            empty.prefix_sha256().unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn stage_id_validation() {
        assert!(stage_data_key("a-1_B.2").is_ok());
        assert!(stage_data_key("").is_err());
        assert!(stage_data_key("a/b").is_err());
        assert!(stage_data_key("a\\b").is_err());
        assert!(stage_data_key(&"x".repeat(200)).is_err());
        assert!(stage_data_key("héllo").is_err());
    }
}
