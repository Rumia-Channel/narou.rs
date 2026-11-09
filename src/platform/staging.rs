//! Private temporary assets, hashed during the same pull-driven write.

use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use crate::error::{NarouError, Result};
use super::object_store::{AssetStore, AssetStream, ObjectKey};

pub const ASSET_CHUNK_BYTES: usize = 64 * 1024;
pub const STAGING_PREFIX: &str = ".narou-staging/";
pub const STAGE_TTL_SECONDS: i64 = 24 * 60 * 60;
pub const STAGE_RENEW_SECONDS: i64 = 5 * 60;

/// Bounded timestamps only; never retains a stream, request or future.
#[derive(Debug, Default)]
pub struct StageLeaseCache(Mutex<std::collections::VecDeque<(ObjectKey, i64)>>);

impl StageLeaseCache {
    pub fn needs_renewal(&self, key: &ObjectKey, now: i64) -> bool {
        !self.0.lock().iter().any(|(stored, renewed)| stored == key && now.saturating_sub(*renewed) < STAGE_RENEW_SECONDS)
    }

    pub fn renewed(&self, key: &ObjectKey, now: i64) {
        let mut entries = self.0.lock();
        entries.retain(|(stored, _)| stored != key);
        if entries.len() == 64 { entries.pop_front(); }
        entries.push_back((key.clone(), now));
    }

    pub fn forget(&self, key: &ObjectKey) {
        self.0.lock().retain(|(stored, _)| stored != key);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedAsset {
    pub key: ObjectKey,
    pub size: u64,
    pub sha256: String,
}

pub fn is_staging_key(key: &ObjectKey) -> bool {
    key.as_ref().starts_with(STAGING_PREFIX)
}

pub fn new_staging_key() -> Result<ObjectKey> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut nonce = [0u8; 16];
    #[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
    getrandom::fill(&mut nonce).map_err(|error| NarouError::Platform(format!("stage nonce: {error}")))?;
    #[cfg(not(any(feature = "native-runtime", feature = "worker-runtime")))]
    nonce[..8].copy_from_slice(&sequence.to_le_bytes());
    ObjectKey::try_new(format!("{STAGING_PREFIX}{}-{sequence}-{}", chrono::Utc::now().timestamp(), hex::encode(nonce)))
}

pub fn validate_range(range: &std::ops::Range<u64>) -> Result<usize> {
    let length = range.end.checked_sub(range.start)
        .filter(|&length| length <= ASSET_CHUNK_BYTES as u64)
        .ok_or_else(|| NarouError::Platform("asset range must be ordered and at most 64 KiB".into()))?;
    Ok(length as usize)
}

pub async fn stage<S: AssetStore + ?Sized>(store: &S, input: AssetStream) -> Result<StagedAsset> {
    let key = new_staging_key()?;
    let state = Arc::new(Mutex::new((Sha256::new(), 0u64, false)));
    let progress = state.clone();
    let stream: AssetStream = Box::pin(futures::stream::try_unfold((input, progress), |(mut input, progress)| async move {
        match input.next().await {
            Some(chunk) => {
                let chunk = chunk?;
                let mut state = progress.lock();
                state.1 = state.1.checked_add(chunk.len() as u64)
                    .ok_or_else(|| NarouError::Platform("asset length overflow".into()))?;
                state.0.update(&chunk);
                drop(state);
                Ok(Some((chunk, (input, progress))))
            }
            None => { progress.lock().2 = true; Ok(None) }
        }
    }));
    // The concrete writer owns failed-write cleanup. Deleting a stage after
    // a failed S3 abort would erase its durable recovery journal.
    store.write_stream(&key, stream).await?;
    let state = state.lock();
    if !state.2 {
        return Err(NarouError::Platform("asset sink returned before consuming EOF".into()));
    }
    Ok(StagedAsset { key, size: state.1, sha256: hex::encode(state.0.clone().finalize()) })
}

/// Split an already-owned bounded control payload lazily, without a second
/// whole-object allocation. Not a transport adapter for large responses.
pub fn owned_bytes_stream(bytes: Vec<u8>) -> AssetStream {
    Box::pin(futures::stream::unfold((bytes, 0usize), |(bytes, offset)| async move {
        if offset == bytes.len() { return None; }
        let end = offset.saturating_add(ASSET_CHUNK_BYTES).min(bytes.len());
        Some((Ok(bytes[offset..end].to_vec()), (bytes, end)))
    }))
}
