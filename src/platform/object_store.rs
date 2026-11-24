//! Object and asset storage abstractions.
//!
//! Domain-level files are addressed by canonical logical keys, never by OS
//! `PathBuf`. Native adapters map keys onto the existing `.narou/` and
//! `小説データ/` layout; Worker adapters can map the same keys onto S3.

use std::borrow::Cow;
use std::fmt;
use std::num::NonZeroUsize;
use std::pin::Pin;

use futures::{Stream, StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};

use super::{PlatformFuture, PlatformService};

/// Logical object key. It is not an operating-system path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// Construct a validated logical object key from external input.
    pub fn try_new(key: impl Into<String>) -> Result<Self> {
        let key = Self(key.into());
        key.validate()?;
        Ok(key)
    }

    /// Construct a key from an internal generator that already enforces the
    /// logical-key invariant.
    pub(crate) fn new_unchecked(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn validate(&self) -> Result<()> {
        validate_logical_key(&self.0)
    }

    pub fn join(&self, component: impl AsRef<str>) -> Result<Self> {
        let component = component.as_ref();
        if component.is_empty() || component.contains('/') || component.contains('\\') {
            return Err(NarouError::Platform(format!(
                "invalid object key component: {component:?}"
            )));
        }
        Self::try_new(format!("{}/{}", self.0.trim_end_matches('/'), component))
    }
}


impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ObjectKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Logical identity for generated converter output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GeneratedAssetKey(ObjectKey);

impl GeneratedAssetKey {
    pub fn new(namespace: &str, filename: &str) -> Result<Self> {
        let namespace = sanitize_key_component(namespace);
        let filename = sanitize_key_component(filename);
        Ok(Self(ObjectKey::try_new(format!(
            "generated/{namespace}/{filename}"
        ))?))
    }

    pub fn as_object_key(&self) -> &ObjectKey {
        &self.0
    }
}

/// 保存用拡張子から `Content-Type` を決める。未知の拡張子は `None`。
///
/// 論理キーと native パスの両方から、割り当てなしで同じ対応表を使う。
pub fn content_type_for_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "yaml" | "yml" => Some("application/yaml"),
        "txt" | "ini" => Some("text/plain; charset=utf-8"),
        "html" => Some("text/html; charset=utf-8"),
        "epub" => Some("application/epub+zip"),
        "zip" => Some("application/zip"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// 論理キーの拡張子から保存時の `Content-Type` を決める。
pub fn content_type_for_key(key: &ObjectKey) -> Option<&'static str> {
    key.as_ref().rsplit('.').next().and_then(content_type_for_extension)
}

/// A validated prefix used for paginated listing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectPrefix(String);

impl ObjectPrefix {
    pub fn new(prefix: impl Into<String>) -> Result<Self> {
        let prefix = prefix.into();
        if prefix.is_empty() {
            return Ok(Self(prefix));
        }
        validate_logical_key(prefix.trim_end_matches('/'))?;
        Ok(Self(prefix))
    }

    pub fn matches(&self, key: &ObjectKey) -> bool {
        let prefix = self.0.trim_end_matches('/');
        prefix.is_empty()
            || key.as_ref() == prefix
            || (key.as_ref().starts_with(prefix)
                && key.as_ref().as_bytes().get(prefix.len()) == Some(&b'/'))
    }
}

impl AsRef<str> for ObjectPrefix {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<ObjectKey> for ObjectPrefix {
    fn from(key: ObjectKey) -> Self {
        Self(key.0)
    }
}

/// Metadata returned by object and asset stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub key: ObjectKey,
    pub size: u64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// One bounded chunk in an asset stream.
pub type AssetChunk = Vec<u8>;

#[cfg(not(target_arch = "wasm32"))]
pub type AssetStream = Pin<Box<dyn Stream<Item = Result<AssetChunk>> + Send + 'static>>;

#[cfg(target_arch = "wasm32")]
pub type AssetStream = Pin<Box<dyn Stream<Item = Result<AssetChunk>> + 'static>>;

/// A bounded, cursor-based object listing request.
#[derive(Debug, Clone)]
pub struct ObjectListRequest {
    pub prefix: ObjectPrefix,
    pub cursor: Option<String>,
    pub limit: NonZeroUsize,
}

impl ObjectListRequest {
    pub fn new(prefix: ObjectPrefix, limit: NonZeroUsize) -> Self {
        Self {
            prefix,
            cursor: None,
            limit,
        }
    }

    pub fn after(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectListPage {
    pub objects: Vec<ObjectMetadata>,
    pub next_cursor: Option<String>,
}

/// Apply the shared `ObjectListRequest` cursor/limit contract to a listing
/// already sorted by key ascending.
///
/// Every backend (filesystem, SQLite, in-memory, D1) resolves its own key
/// set, then delegates the page cut here so the contract lives once: the
/// cursor is the key of the **last object returned** by the previous page
/// (`next_cursor`), and resumption skips to the first key strictly greater
/// than the cursor. Because the cursor names a returned key, deleting the
/// object it points to does not lose the next page — unlike a cursor that
/// names the first *unreturned* key, which is never emitted and would drop
/// one object at every page boundary.
///
/// Returns the page plus the cursor for the next request (`None` when this
/// page reached the end of the listing).
pub fn paginate_object_listing<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    limit: usize,
    key_of: impl Fn(&T) -> &str,
) -> (Vec<T>, Option<String>) {
    let start = match cursor {
        Some(cursor) => items
            .iter()
            .position(|item| key_of(item) > cursor)
            .unwrap_or(items.len()),
        None => 0,
    };
    let end = start.saturating_add(limit).min(items.len());
    let next_cursor = (end < items.len())
        .then(|| &items[end - 1])
        .map(|item| key_of(item).to_string());
    (
        items
            .into_iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect(),
        next_cursor,
    )
}

/// String-prefix range bound for a `WHERE key >= ? AND key < ?` scan.
///
/// Appending the highest code point makes the bound cover every key that starts
/// with the prefix — including keys whose next character sorts above `'0'`
/// (a naive `{prefix}0` bound hides every CJK or letter-leading name).
pub fn prefix_upper_bound(prefix: &str) -> String {
    format!("{prefix}\u{10FFFF}")
}

/// Pure transform invoked inside an adapter's atomic control-object boundary.
/// `None` leaves the current object untouched. Retries may invoke it again.
pub type SmallObjectMutator<'data> =
    dyn FnMut(Option<&[u8]>) -> Result<Option<Cow<'data, [u8]>>> + Send + 'data;

/// A canonical write may succeed even if its optional filesystem mirror fails.
#[derive(Debug)]
pub enum ObjectUpdateResult {
    Committed,
    ProjectionFailed(NarouError),
    Conflict,
}

/// SHA-256 of the decoded bytes already consumed from one logical object.
#[derive(Debug)]
pub struct ObjectFingerprint {
    pub key: ObjectKey,
    pub sha256: [u8; 32],
}

/// The first object that no longer matches a consumed snapshot.
#[derive(Debug, PartialEq, Eq)]
pub enum ObjectFingerprintMismatch<'a> {
    Missing(&'a ObjectKey),
    Changed(&'a ObjectKey),
}

/// Caller-owned byte reads used when a fingerprint has no immutable digest.
/// Verification must retain the caller's original admission and lease.
pub trait ObjectFingerprintReader: PlatformService {
    fn read_small<'a>(&'a mut self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>>;
}

/// Blob store for bounded control objects.
///
/// `read_small`/`write_small` deliberately make the buffering contract
/// explicit. Large or generated binary assets use [`AssetStore`] instead.
pub trait ObjectStore: PlatformService {
    fn access_fence(&self) -> Option<&dyn super::NovelAccessFence> { None }

    fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a super::GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        let _ = (prefix, lease, key, range);
        Box::pin(async { Err(NarouError::Platform("leased content reads unsupported".into())) })
    }
    /// Filesystem path backing `key`, when the store keeps each object as a
    /// real file a snapshot can hardlink. Remote/object-backed stores return
    /// `None` so callers fall back to a streamed copy. Used only to avoid a
    /// byte copy for a point-in-time private snapshot (issue #58).
    fn native_source_path(&self, key: &ObjectKey) -> Option<std::path::PathBuf> {
        let _ = key;
        None
    }

    fn write_small_guarded<'a>(&'a self, fence: &'a super::WriteFence, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        let _ = (fence, key, data);
        Box::pin(async { Err(NarouError::Platform("epoch-guarded writes unsupported".into())) })
    }

    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a super::WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{
        let _=(fence,key,mutate);Box::pin(async{Err(NarouError::Platform("epoch-guarded atomic updates unsupported".into()))})
    }

    fn delete_guarded<'a>(&'a self, fence: &'a super::WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let _ = (fence, key);
        Box::pin(async { Err(NarouError::Platform("epoch-guarded deletion unsupported".into())) })
    }
    /// Only a drained CLOSED gate with this exact owner/epoch may unlink.
    fn delete_closed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{let _=(prefix,owner,epoch,key);Box::pin(async{Err(NarouError::Platform("closed-owner deletion unsupported".into()))})}

    /// Publish a verified explicit pool reference, then retire only obsolete
    /// legacy physical bytes. This must never tombstone the logical image.
    fn retire_legacy<'a>(&'a self,logical:&'a ObjectKey,pool:&'a ObjectKey,size:u64,sha256:&'a str)->PlatformFuture<'a,Result<()>>{
        let _=(logical,pool,size,sha256);Box::pin(async{Err(NarouError::Platform("legacy retirement requires an admitted source adapter".into()))})
    }
    fn retire_legacy_guarded<'a>(&'a self,fence:&'a super::WriteFence,logical:&'a ObjectKey,pool:&'a ObjectKey,size:u64,sha256:&'a str)->PlatformFuture<'a,Result<()>>{
        let _=(fence,logical,pool,size,sha256);Box::pin(async{Err(NarouError::Platform("guarded legacy retirement unsupported".into()))})
    }

    fn stat<'a>(&'a self, key: &'a ObjectKey)
    -> PlatformFuture<'a, Result<Option<ObjectMetadata>>>;

    fn exists<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.stat(key).await?.is_some()) })
    }

    /// Whether `key` is currently backed by an immutable remote/canonical
    /// reference rather than directly stored local bytes. Read adapters that
    /// resolve references transparently override this so callers can
    /// distinguish "absent" from "served elsewhere". Stores without a
    /// reference concept answer `false`.
    fn is_remote_reference<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        let _ = key;
        Box::pin(async { Ok(false) })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>>;

    /// Recheck consumed content in input order, without retaining its bodies.
    ///
    /// Mutable stores re-read through the caller's admitted reader. An adapter
    /// may instead compare fresh authoritative digests of immutable,
    /// checksum-verified payloads, forwarding this reader for every fallback.
    /// Cached digests, timestamps and opaque ETags are not sufficient.
    fn verify_fingerprints<'a, 'input: 'a>(
        &'a self,
        fingerprints: &'input [ObjectFingerprint],
        reader: &'a mut dyn ObjectFingerprintReader,
    ) -> PlatformFuture<'a, Result<Option<ObjectFingerprintMismatch<'input>>>> {
        Box::pin(async move {
            for fingerprint in fingerprints {
                let Some(bytes) = reader.read_small(&fingerprint.key).await? else {
                    return Ok(Some(ObjectFingerprintMismatch::Missing(&fingerprint.key)));
                };
                if <[u8; 32]>::from(Sha256::digest(&bytes)) != fingerprint.sha256 {
                    return Ok(Some(ObjectFingerprintMismatch::Changed(&fingerprint.key)));
                }
            }
            Ok(None)
        })
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Atomically read, transform and commit a primary control object.
    /// Asset-only adapters may reject this capability; never emulate it with
    /// separate `read_small` and `write_small` calls.
    fn update_small<'a, 'data: 'a>(
        &'a self,
        _key: &'a ObjectKey,
        _mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async {
            Err(NarouError::Platform("atomic control-object updates are unsupported by this store".into()))
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>>;

    /// Delete every object stored below `prefix`.
    ///
    /// The default implementation walks the listing and deletes key by key;
    /// backends with a bulk operation (SQLite) override it so a prefix delete
    /// does not pay one round trip per object.
    fn delete_prefix<'a>(&'a self, prefix: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let prefix = ObjectPrefix::from(prefix.clone());
        Box::pin(async move {
            let mut cursor: Option<String> = None;
            loop {
                let request = ObjectListRequest::new(
                    prefix.clone(),
                    NonZeroUsize::new(100).expect("page size is non-zero"),
                );
                let request = match &cursor {
                    Some(cursor) => request.after(cursor.clone()),
                    None => request,
                };
                let page = self.list_page(&request).await?;
                for object in &page.objects {
                    self.delete(&object.key).await?;
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => return Ok(()),
                }
            }
        })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>>;
}

/// Storage boundary for potentially large binary assets.
///
/// The interface never exposes `File`, `Path`, an SDK stream, or an
/// unbounded `Vec`. Implementations may use chunked filesystem, HTTP, or
/// object-storage streams internally.
pub trait AssetStore: PlatformService {

    /// Raw legacy-physical GC capabilities, used only after an authoritative
    /// guarded pool-ref + GC-intent commit. Unsupported stores fail closed.
    fn legacy_etag<'a>(&'a self,logical:&'a ObjectKey)->PlatformFuture<'a,Result<Option<String>>>{let _=logical;Box::pin(async{Err(NarouError::Platform("legacy physical ETag lookup unsupported".into()))})}
    fn delete_legacy_if_match<'a>(&'a self,logical:&'a ObjectKey,pool:&'a ObjectKey,etag:&'a str)->PlatformFuture<'a,Result<()>>{let _=(logical,pool,etag);Box::pin(async{Err(NarouError::Platform("conditional legacy physical GC unsupported".into()))})}
    fn write_stream_guarded<'a>(&'a self, fence: &'a super::WriteFence, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        let _ = (fence, key, stream);
        Box::pin(async { Err(NarouError::Platform("epoch-guarded asset writes unsupported".into())) })
    }

    fn stat<'a>(&'a self, key: &'a ObjectKey)
    -> PlatformFuture<'a, Result<Option<ObjectMetadata>>>;

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>>;

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Read an exclusive-end range of at most 64 KiB. Missing objects are
    /// distinct from short or inconsistent reads, which are errors.
    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>>;

    /// Read an exclusive-end byte range of **any** length as a bounded
    /// stream (each item ≤ 64 KiB).
    ///
    /// This is the transfer read path: one 8 MiB HTTP batch must never
    /// degrade into 128 separate `read_range` calls. Backends with real
    /// range I/O override the default: S3 issues exactly one `Range` GET,
    /// DB stores bound every fetched chunk row to the object generation
    /// pinned when the range was opened, and the filesystem performs one
    /// `seek`. `range.end == u64::MAX` means "to end of object" — the
    /// backend resolves the concrete size itself.
    ///
    /// The default walks `read_stream` and discards the prefix. It is
    /// correct but linear in `range.start`; never leave it on a backend
    /// that can seek or issue range requests.
    ///
    /// `Ok(None)` = object absent. Truncated/changed objects and ranges
    /// exceeding the object are errors, never short silent reads.
    fn read_stream_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        let range = range.clone();
        Box::pin(async move {
            if range.start >= range.end {
                return match self.stat(key).await? {
                    Some(_) => {
                        let empty: AssetStream =
                            Box::pin(futures::stream::empty::<Result<AssetChunk>>());
                        Ok(Some(empty))
                    }
                    None => Ok(None),
                };
            }
            let Some(stream) = self.read_stream(key).await? else {
                return Ok(None);
            };
            let to_end = range.end == u64::MAX;
            let out = futures::stream::try_unfold(
                (stream, 0u64),
                move |(mut stream, position)| {
                    let wanted = range.clone();
                    async move {
                        if position >= wanted.end {
                            return Ok(None);
                        }
                        let Some(chunk) = stream.next().await else {
                            if to_end {
                                return Ok(None);
                            }
                            return Err(NarouError::Platform(
                                "asset range exceeds object".into(),
                            ));
                        };
                        let chunk = chunk?;
                        let chunk_start = position;
                        let chunk_end = position.saturating_add(chunk.len() as u64);
                        if chunk_end <= wanted.start {
                            return Ok(Some((Vec::new(), (stream, chunk_end))));
                        }
                        let lo = wanted.start.saturating_sub(chunk_start) as usize;
                        let hi = (wanted.end.min(chunk_end) - chunk_start) as usize;
                        Ok(Some((chunk[lo..hi].to_vec(), (stream, chunk_end))))
                    }
                },
            )
            // drop the empty trim items without copying
            .try_filter_map(|chunk| async move {
                Ok(if chunk.is_empty() { None } else { Some(chunk) })
            });
            Ok(Some(Box::pin(out) as AssetStream))
        })
    }

    /// Conditionally write an object that must not already exist, hashing
    /// the streamed bytes in the same pass and comparing the actual digest
    /// to `expected_sha256` (lowercase hex) before committing.
    ///
    /// This is the write-once contract for hash-named pools (dedup
    /// illustrations) and for transfer stages that must stay byte-exact:
    /// * `Ok(WriteOnce::Written)` — the object was absent and is now stored.
    /// * `Ok(WriteOnce::Reused)` — it already existed with byte-identical
    ///   content (the caller's hash-named key was right and the bytes are
    ///   proven, not assumed).
    /// * `Err` — absent-write precondition failed *and* the existing bytes
    ///   differ, the actual digest ≠ `expected_sha256`, or the backend
    ///   lacks a conditional write. There is no unconditional-PUT fallback.
    ///
    /// `expected_size` must equal the decoded byte length.
    fn write_stream_once<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
        expected_size: u64,
        expected_sha256: &'a str,
    ) -> PlatformFuture<'a, Result<super::transfer_stage::WriteOnce>> {
        let _ = (key, stream, expected_size, expected_sha256);
        Box::pin(async {
            Err(NarouError::Platform(
                "conditional write-once is unsupported by this store".into(),
            ))
        })
    }

    /// Write a private immutable temporary object and hash it in the same pass.
    fn stage_stream<'a>(
        &'a self,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<super::staging::StagedAsset>> {
        Box::pin(super::staging::stage(self, stream))
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>>;

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>>;

    /// Move semantics are logical; native may rename while S3 may copy/delete.
    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>>;
}

/// Canonical keys for novel-owned objects.
///
/// The native mapping strips the `novels/` namespace and joins the remainder
/// below the existing archive root. Worker mappings can use the full key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NovelObjectKeys {
    prefix: ObjectKey,
}

impl NovelObjectKeys {
    pub fn new(sitename: &str, file_title: &str, use_subdirectory: bool) -> Result<Self> {
        let safe_site = sanitize_key_component(sitename);
        let safe_title = sanitize_key_component(file_title);
        let mut parts = vec!["novels".to_string(), safe_site];
        if use_subdirectory {
            let subdirectory = create_subdirectory_name(&safe_title);
            if !subdirectory.is_empty() {
                parts.push(subdirectory);
            }
        }
        parts.push(safe_title);
        ObjectKey::try_new(parts.join("/")).map(|prefix| Self { prefix })
    }

    pub fn prefix(&self) -> &ObjectKey {
        &self.prefix
    }

    pub fn toc(&self) -> ObjectKey {
        self.child("toc.yaml")
    }

    pub fn section(&self, index: &str, file_subtitle: &str) -> ObjectKey {
        self.child(&format!(
            "本文/{} {}.yaml",
            index,
            sanitize_key_component_with_limit(file_subtitle, None)
        ))
    }

    /// 互換読み出し用の旧本文キー候補 (正規キーと一致するものは含めない)。
    ///
    /// 1. Ruby 版由来のサニタイズ前キー (`<index> <file_subtitle>.yaml`、末尾空白等)
    /// 2. v0.6.10 以前の上限 80 文字で切り詰められた正規キー (`file_subtitle` が 80 文字超のとき)
    pub fn legacy_section_fallbacks(&self, index: &str, file_subtitle: &str) -> Vec<ObjectKey> {
        let canonical = self.section(index, file_subtitle);
        let mut fallbacks = Vec::new();
        if let Ok(raw_key) = self
            .prefix()
            .join("本文")
            .and_then(|dir| dir.join(format!("{index} {file_subtitle}.yaml")))
            && raw_key != canonical
        {
            fallbacks.push(raw_key);
        }
        let truncated = self.child(&format!(
            "本文/{} {}.yaml",
            index,
            sanitize_key_component_with_limit(file_subtitle, Some(80))
        ));
        if truncated != canonical && !fallbacks.contains(&truncated) {
            fallbacks.push(truncated);
        }
        fallbacks
    }

    pub fn raw_section(&self, index: &str, file_subtitle: &str) -> ObjectKey {
        let section = self.section(index, file_subtitle);
        let filename = section
            .as_ref()
            .rsplit('/')
            .next()
            .unwrap_or("section.yaml")
            .strip_suffix(".yaml")
            .unwrap_or("section");
        self.child(&format!("raw/{filename}.html"))
    }

    pub fn setting(&self) -> ObjectKey {
        self.child("setting.ini")
    }

    pub fn replace(&self) -> ObjectKey {
        self.child("replace.txt")
    }

    pub fn diff(&self) -> ObjectKey {
        self.child("diff.txt")
    }

    /// Fully converted 青空文庫 text produced by a text-only convert run
    /// (Worker path). Native layouts keep their per-title output file names;
    /// this fixed key is the portable download-time EPUB source.
    pub fn converted_text(&self) -> ObjectKey {
        self.child("novel.txt")
    }

    pub fn illustration(&self, filename: &str) -> Result<ObjectKey> {
        illustration_key(&self.prefix, filename)
    }

    pub fn illustration_cache(&self) -> ObjectKey {
        self.child(".illustration_cache.yaml")
    }

    pub fn cached_section(&self, timestamp: &str, index: &str, file_subtitle: &str) -> ObjectKey {
        self.child(&format!(
            "本文/cache/{timestamp}/{} {}.yaml",
            index,
            sanitize_key_component_with_limit(file_subtitle, None)
        ))
    }

    /// A durable cache-version record, including completed updates with no
    /// previous sections to archive. It is deliberately not a YAML section.
    pub fn cached_version_marker(&self, timestamp: &str) -> ObjectKey {
        self.child(&format!("本文/cache/{timestamp}/.version"))
    }

    fn child(&self, component: &str) -> ObjectKey {
        ObjectKey::new_unchecked(format!("{}/{}", self.prefix.0, component))
    }
}

pub fn sanitize_key_component(value: &str) -> String {
    sanitize_key_component_with_limit(value, None)
}

/// 挿絵の論理キー (`<小説プレフィックス>/挿絵/<file>`) を組み立てる。
///
/// **保存側と読み出し側は必ずこの関数を通す** (`IllustrationStorageService` も
/// `NovelObjectKeys::illustration` も同じ実装を呼ぶ)。ファイル名の sanitize を
/// 片側だけに掛けると、書いたキーと読むキーがずれて「保存したのに見つからない」
/// 状態になるため、キーの作り方はここ 1 箇所に閉じ込める。
pub fn illustration_key(prefix: &ObjectKey, filename: &str) -> Result<ObjectKey> {
    let filename = sanitize_key_component(filename);
    ObjectKey::try_new(format!("{}/挿絵/{filename}", prefix.as_ref()))
}

pub fn sanitize_key_component_with_limit(value: &str, limit: Option<usize>) -> String {
    let sanitized = value
        .chars()
        .filter_map(|ch| {
            if ch.is_control() {
                None
            } else if matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                Some('_')
            } else {
                Some(ch)
            }
        })
        .collect::<String>();
    let limited = limit
        .map(|limit| sanitized.chars().take(limit).collect::<String>())
        .unwrap_or(sanitized);
    let mut candidate = limited.trim_end_matches([' ', '.']).to_string();
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        candidate = "_".to_string();
    }
    if is_windows_reserved_name(&candidate) {
        candidate.insert(0, '_');
    }
    candidate
}

/// Stored-payload codec for the `objects`/`object_chunks`/`section_bodies`
/// tables.
///
/// `None` stores raw bytes; `Brotli` stores brotli-compressed bytes (pure
/// Rust, wasm32-safe — zstd needs clang and cannot build for the Worker).
/// `Deflate` is decode-only legacy support for rows written by the first
/// revision of this schema. Compression is applied to the whole payload
/// *before* chunking so chunk boundaries never split a compressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectEncoding {
    None,
    Brotli,
    Deflate,
}

impl ObjectEncoding {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Brotli => "brotli",
            Self::Deflate => "deflate",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "brotli" => Ok(Self::Brotli),
            "deflate" => Ok(Self::Deflate),
            other => Err(NarouError::Platform(format!(
                "unknown object encoding: {other}"
            ))),
        }
    }
}

/// Below this size compression costs more CPU than the bytes it saves.
const COMPRESS_MIN_BYTES: usize = 512;

/// CRC-32 of an uncompressed payload, stored alongside the encoding so
/// reads and `db verify` can detect corruption independent of the codec.
pub fn object_crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Compress `data` when it is large enough and brotli actually shrinks it.
/// Returns the stored bytes plus the encoding marker to persist.
pub fn compress_object_payload(data: &[u8]) -> (Vec<u8>, ObjectEncoding) {
    let (stored, encoding) = encode_object_payload(data);
    (stored.into_owned(), encoding)
}

/// Keep uncompressed bytes borrowed when the caller already owns the payload.
pub fn encode_object_payload(data: &[u8]) -> (Cow<'_, [u8]>, ObjectEncoding) {
    if data.len() < COMPRESS_MIN_BYTES {
        return (Cow::Borrowed(data), ObjectEncoding::None);
    }
    let compressed = brotli_compress(data);
    if compressed.len() < data.len() {
        (Cow::Owned(compressed), ObjectEncoding::Brotli)
    } else {
        (Cow::Borrowed(data), ObjectEncoding::None)
    }
}

/// Reverse [`compress_object_payload`]. `encoding` comes from the object's
/// `encoding` column.
pub fn decompress_object_payload(data: &[u8], encoding: ObjectEncoding) -> Result<Vec<u8>> {
    match encoding {
        ObjectEncoding::None => Ok(data.to_vec()),
        ObjectEncoding::Brotli => brotli_decompress(data),
        ObjectEncoding::Deflate => {
            miniz_oxide::inflate::decompress_to_vec(data).map_err(|error| {
                NarouError::Platform(format!("invalid deflate object payload: {error}"))
            })
        }
    }
}

/// Decode a bounded control object without copying an uncompressed payload.
pub fn decode_object_payload_limited(data: &[u8], encoding: ObjectEncoding, limit: usize) -> Result<Cow<'_, [u8]>> {
    if data.len() > limit {
        return Err(NarouError::Platform("stored object exceeds the read cap".into()));
    }
    let decoded = match encoding {
        ObjectEncoding::None => return Ok(Cow::Borrowed(data)),
        ObjectEncoding::Brotli => {
            use std::io::Read;
            let mut decoded = Vec::new();
            brotli::Decompressor::new(data, 4096)
                .take((limit as u64).saturating_add(1)).read_to_end(&mut decoded)?;
            decoded
        }
        ObjectEncoding::Deflate => miniz_oxide::inflate::decompress_to_vec_with_limit(data, limit)
            .map_err(|error| NarouError::Platform(format!("invalid or oversized deflate object: {error}")))?,
    };
    if decoded.len() > limit {
        return Err(NarouError::Platform("decoded object exceeds the read cap".into()));
    }
    Ok(Cow::Owned(decoded))
}

/// Verify `data` (uncompressed) against a stored CRC-32.
pub fn verify_object_crc32(data: &[u8], expected: u32) -> Result<()> {
    let actual = object_crc32(data);
    if actual != expected {
        return Err(NarouError::Platform(format!(
            "object payload CRC mismatch: expected {expected:08x}, got {actual:08x}"
        )));
    }
    Ok(())
}

fn brotli_compress(data: &[u8]) -> Vec<u8> {
    // Quality 6: near-best ratio for text at a fraction of q11's CPU cost.
    let mut output = Vec::new();
    let params = brotli::enc::BrotliEncoderParams {
        quality: 6,
        ..Default::default()
    };
    let mut input = std::io::Cursor::new(data);
    if brotli::BrotliCompress(&mut input, &mut output, &params).is_err() {
        return data.to_vec();
    }
    output
}

fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut input = std::io::Cursor::new(data);
    brotli::BrotliDecompress(&mut input, &mut output)
        .map_err(|error| NarouError::Platform(format!("invalid brotli object payload: {error}")))?;
    Ok(output)
}

fn create_subdirectory_name(file_title: &str) -> String {
    let chars = if file_title.starts_with('n') {
        file_title.chars().skip(1).take(2).collect::<String>()
    } else {
        file_title.chars().take(2).collect::<String>()
    };
    chars.trim().to_string()
}

fn is_windows_reserved_name(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches([' ', '.']);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn validate_logical_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.starts_with('/')
        || key.ends_with('/')
        || key.contains('\\')
        || key.contains("//")
    {
        return Err(NarouError::Platform(format!(
            "invalid logical object key: {key:?}"
        )));
    }
    for component in key.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(NarouError::Platform(format!(
                "invalid logical object key component: {component:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefix_upper_bound_covers_every_child() {
        let bound = super::prefix_upper_bound("webnovel/");
        assert!("webnovel/contract-test.yaml" < bound.as_str());
        assert!("webnovel/挿絵/0001.jpg" < bound.as_str());
        assert!(super::prefix_upper_bound("").as_str() > "novels/site/title/本文/1 x.yaml");
    }

    use super::*;

    #[test]
    fn content_type_extensions_preserve_storage_metadata() {
        for (extension, expected) in [
            ("yaml", "application/yaml"),
            ("yml", "application/yaml"),
            ("txt", "text/plain; charset=utf-8"),
            ("ini", "text/plain; charset=utf-8"),
            ("html", "text/html; charset=utf-8"),
            ("epub", "application/epub+zip"),
            ("zip", "application/zip"),
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("png", "image/png"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
        ] {
            assert_eq!(content_type_for_extension(extension), Some(expected));
        }
        for extension in ["", "unknown", "bin", "YAML", "PNG"] {
            assert_eq!(content_type_for_extension(extension), None);
        }
    }

    #[test]
    fn object_key_display_and_validation() {
        assert_eq!(
            ObjectKey::try_new("novels/123/toc.yaml").unwrap().to_string(),
            "novels/123/toc.yaml"
        );
        assert_eq!(ObjectKey::try_new("a").unwrap().as_ref(), "a");
        assert!(ObjectKey::try_new("../escape").is_err());
        assert!(ObjectKey::try_new(r"C:\escape").is_err());
    }

    /// 壊れた brotli ペイロードは panic ではなくエラーになること。
    ///
    /// 保存オブジェクトはデータベースの値から復号されるため、壊れた値や
    /// 攻撃者が書き込んだ値で本体が落ちないことを固定する。CVE-2026-111497
    /// (Google Brotli デコーダの範囲外書き込み) に対応する版は、該当しうる
    /// 入力で正しいエラーコードを返す。
    #[test]
    fn corrupt_brotli_payloads_are_rejected_without_panicking() {
        let payload = "あいうえお".repeat(64);
        let (stored, encoding) = compress_object_payload(payload.as_bytes());
        assert_eq!(encoding, ObjectEncoding::Brotli);
        assert_eq!(
            decompress_object_payload(&stored, encoding).unwrap(),
            payload.as_bytes()
        );

        // 切り詰め・反転・ゼロ埋めはどれもエラーになる。
        for cut in 1..stored.len() {
            let truncated = &stored[..stored.len() - cut];
            assert!(
                decompress_object_payload(truncated, encoding).is_err(),
                "cut={cut} で壊れたペイロードが受理された"
            );
        }
        let mut flipped = stored.clone();
        flipped[0] ^= 0xff;
        assert!(decompress_object_payload(&flipped, encoding).is_err());

        // 上限つき経路も同じく拒否する (panic しない)。
        assert!(
            decode_object_payload_limited(&flipped, encoding, 1024).is_err(),
            "上限つき経路が壊れたペイロードを受理した"
        );
    }

    #[test]
    fn novel_keys_preserve_native_layout_components() {
        let keys = NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        assert_eq!(keys.prefix().as_ref(), "novels/site/12/n1234ab");
        assert_eq!(
            keys.section("1", "第1話"),
            ObjectKey::try_new("novels/site/12/n1234ab/本文/1 第1話.yaml").unwrap()
        );
        assert_eq!(
            keys.raw_section("1", "第1話"),
            ObjectKey::try_new("novels/site/12/n1234ab/raw/1 第1話.html").unwrap()
        );
    }

    #[test]
    fn illustration_key_is_shared_by_writers_and_readers() {
        let keys = NovelObjectKeys::new("site", "n1234ab", false).unwrap();
        let prefix = keys.prefix();
        // 書き込み側 (`IllustrationStorageService`) と読み出し側
        // (`NovelObjectKeys::illustration`) は同じ関数を通る。
        assert_eq!(
            illustration_key(prefix, "i422674.jpg").unwrap(),
            keys.illustration("i422674.jpg").unwrap()
        );
        // sanitize が掛かる名前でも一致する (片側だけ掛けるとキーがずれる)。
        assert_eq!(
            illustration_key(prefix, "a:b.jpg").unwrap().as_ref(),
            "novels/site/n1234ab/挿絵/a_b.jpg"
        );
        assert_eq!(
            illustration_key(prefix, "a:b.jpg").unwrap(),
            keys.illustration("a:b.jpg").unwrap()
        );
    }

    #[test]
    fn list_request_has_nonzero_limit() {
        let prefix = ObjectPrefix::new("novels/site").unwrap();
        let request = ObjectListRequest::new(prefix, NonZeroUsize::new(10).unwrap());
        assert_eq!(request.limit.get(), 10);
    }

    #[test]
    fn generated_keys_are_platform_neutral() {
        let key = GeneratedAssetKey::new("epub", r#"book\final.zip"#).unwrap();
        assert_eq!(
            key.as_object_key().as_ref(),
            "generated/epub/book_final.zip"
        );
        assert!(ObjectPrefix::new("novels\\bad").is_err());
    }

    #[test]
    fn object_prefix_matches_components_not_string_prefixes() {
        let prefix = ObjectPrefix::new("novels/site").unwrap();
        assert!(prefix.matches(&ObjectKey::try_new("novels/site/book/toc.yaml").unwrap()));
        assert!(prefix.matches(&ObjectKey::try_new("novels/site").unwrap()));
        assert!(!prefix.matches(
            &ObjectKey::try_new("novels/site-archive/book/toc.yaml").unwrap()
        ));
    }

    /// The D1 store used to emit the first *unreturned* key as
    /// `next_cursor`, silently dropping one object at every page boundary.
    /// These tests pin the shared contract every backend now delegates to:
    /// the cursor is the last returned key and paging yields each object
    /// exactly once.
    fn metas(keys: &[&str]) -> Vec<ObjectMetadata> {
        keys.iter()
            .map(|key| ObjectMetadata {
                key: ObjectKey::try_new(*key).unwrap(),
                size: 1,
                etag: None,
                content_type: None,
                last_modified: None,
            })
            .collect()
    }

    fn keys_of(page: &[ObjectMetadata]) -> Vec<String> {
        page.iter()
            .map(|meta| meta.key.as_ref().to_string())
            .collect()
    }

    fn meta_key(meta: &ObjectMetadata) -> &str {
        meta.key.as_ref()
    }

    #[test]
    fn paginated_listing_yields_every_object_exactly_once() {
        let all = metas(&["generated/a", "generated/b", "generated/c"]);
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let (page, next) = paginate_object_listing(all.clone(), cursor.as_deref(), 2, meta_key);
            seen.extend(keys_of(&page));
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, vec!["generated/a", "generated/b", "generated/c"]);
    }

    #[test]
    fn paginate_next_cursor_is_last_returned_key() {
        let (page, next) = paginate_object_listing(
            metas(&["generated/a", "generated/b", "generated/c"]),
            None,
            2,
            meta_key,
        );
        assert_eq!(keys_of(&page), vec!["generated/a", "generated/b"]);
        assert_eq!(next.as_deref(), Some("generated/b"));
    }

    #[test]
    fn paginate_continues_when_cursor_object_is_deleted() {
        let (page, next) = paginate_object_listing(
            metas(&["generated/a", "generated/b", "generated/c"]),
            None,
            1,
            meta_key,
        );
        assert_eq!(keys_of(&page), vec!["generated/a"]);
        let cursor = next.unwrap();

        // Delete the cursor's object before resuming, as a concurrent
        // `delete` (or a failed migration entry) can do.
        let remaining = metas(&["generated/b", "generated/c"]);
        let (page, next) = paginate_object_listing(remaining, Some(&cursor), 1, meta_key);
        assert_eq!(keys_of(&page), vec!["generated/b"]);
        let (page, next) = paginate_object_listing(
            metas(&["generated/c"]),
            next.as_deref(),
            1,
            meta_key,
        );
        assert_eq!(keys_of(&page), vec!["generated/c"]);
        assert_eq!(next, None);
    }

    #[test]
    fn paginate_past_the_end_and_empty_listing() {
        let (page, next) =
            paginate_object_listing(metas(&["generated/a"]), Some("generated/z"), 2, meta_key);
        assert!(page.is_empty());
        assert_eq!(next, None);
        let (page, next) = paginate_object_listing(metas(&[]), None, 2, meta_key);
        assert!(page.is_empty());
        assert_eq!(next, None);
    }

    /// `clean` の expected stem (`"{index} {file_subtitle}"` の素直な連結) と
    /// `keys.section` のファイル名部分が、80 文字を超える file_subtitle でも
    /// 一致すること。section キーが独自に 80 文字で打ち切られると
    /// `narou clean` が現行本文を孤児として削除してしまう。
    #[test]
    fn section_key_keeps_full_file_subtitle_for_clean_stem_match() {
        let keys = NovelObjectKeys::new("example.test", "book", false).unwrap();
        let long = "あ".repeat(120);
        let file_name = keys
            .section("7", &long)
            .as_ref()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(file_name, format!("7 {long}.yaml"));
        let cached_name = keys
            .cached_section("20240101000000", "7", &long)
            .as_ref()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(cached_name, file_name);
    }
}
