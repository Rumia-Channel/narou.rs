//! 作品単位の reader lease / writer fence を、全入口で共有する port。
//!
//! 背景は `docs/client-transfer-design.md` §11.2。ここにあるのは
//! `crate::platform` の primitive (`NovelAccessFence` / `*_guarded` /
//! `read_stream_leased`) を呼び出し側がそのまま組み立て直さなくて済む
//! 薄い所有物だけで、別のロック規約や第二の解放経路は作らない。
//!
//! * writer は [`NovelWriteGuard::capture`] で論理操作の開始時点 (ネットワーク
//!   取得や長い await の前) に epoch を 1 度だけ固定し、その token を全
//!   canonical write/delete にそのまま使う。await 後の再 capture は古い
//!   結果を追認することになるので行わない。
//! * reader は [`NovelReadLease::admit`] で admission gate と同じ CAS 境界の
//!   lease を得て、論理ファイルを開くたびに [`NovelReadLease::open`] /
//!   [`NovelReadLease::read_small`] を通す。lease は 30 秒、進行中は 10 秒
//!   間隔で renew し、gate が閉じた後の renew 拒否はそのままエラーになる。
//! * `access_fence()` を持たない保存先は「未対応」として必ず失敗させる。
//!   unguarded な write/read への fallback は存在しない。

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// Operation state crosses threads on native, but never leaves a wasm isolate.
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc as SharedState;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc as SharedState;

use futures::StreamExt;

use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, AssetStream, Clock, GateLease, NovelAccessFence, NovelObjectKeys, ObjectKey, ObjectPrefix,
    ObjectFingerprint, ObjectFingerprintMismatch, ObjectFingerprintReader,
    ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectStore, ObjectUpdateResult,
    PlatformFuture, PlatformService, SmallObjectMutator, SystemClock, WriteFence,
    READER_LEASE_SECS, is_reserved_key,
};

/// 進行中 reader の renew 間隔 (秒)。lease TTL は [`READER_LEASE_SECS`]。
pub const RENEW_INTERVAL_SECS: i64 = 10;

// Random identity avoids collisions across Worker isolates (which share
// neither a process id nor the in-memory monotonic counter).
fn unique_reader_id(hint: &str) -> Result<String> {
    let nonce = crate::login::crypto::random_bytes::<16>()?;
    Ok(reader_id_for_nonce(hint, nonce))
}

fn reader_id_for_nonce(hint: &str, nonce: [u8; 16]) -> String {
    let capacity = if hint.is_empty() { "reader".len() } else { hint.len().min(40) };
    let mut id = String::with_capacity(capacity + 33);
    for character in hint.chars().take(40) {
        id.push(if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') { character } else { '_' });
    }
    if id.is_empty() { id.push_str("reader"); }
    id.push('-');
    let mut encoded = [0u8; 32];
    hex::encode_to_slice(nonce, &mut encoded).expect("fixed nonce output size");
    id.push_str(std::str::from_utf8(&encoded).expect("hex is ASCII"));
    id
}

fn unsupported_fence(prefix: &ObjectPrefix) -> NarouError {
    NarouError::Platform(format!(
        "novel access fence unsupported by this store: {}",
        prefix.as_ref()
    ))
}

fn key_outside_novel(prefix: &ObjectPrefix, key: &ObjectKey) -> NarouError {
    NarouError::Platform(format!(
        "novel key outside leased prefix: {} not under {}",
        key.as_ref(),
        prefix.as_ref()
    ))
}

/// Legacy filesystem codecs accept only private snapshots, never canonical
/// library paths. Resolve symlinks and missing leaf paths before containment.
#[cfg(feature = "native-runtime")]
#[doc(hidden)]
pub fn require_private_path(path: &std::path::Path) -> Result<()> {
    if is_private_path(path)? { return Ok(()); }
    Err(NarouError::Platform(format!(
        "canonical novel filesystem access requires a leased private snapshot: {}",
        path.display(),
    )))
}

/// Classify without treating filesystem/discovery failures as a canonical path.
#[cfg(feature = "native-runtime")]
#[doc(hidden)]
pub fn is_private_path(path: &std::path::Path) -> Result<bool> {
    let mut roots: Vec<std::path::PathBuf> = crate::db::DATABASE.lock().as_ref()
        .map(|database| vec![database.archive_root().to_path_buf()])
        .unwrap_or_default();
    // The cwd-relative archive dir is canonical even with no loaded library.
    roots.push(std::path::PathBuf::from(crate::downloader::ARCHIVE_ROOT_DIR));
    // Inventory::with_default_root discovers an ancestor `.narou` marker; while
    // DATABASE is unloaded (or the cwd sits inside that archive) its archive is
    // still canonical. A missing marker means "no library", while discovery IO
    // errors propagate - never a private fallback.
    let cwd = std::env::current_dir()?;
    for ancestor in cwd.ancestors() {
        if ancestor.join(".narou").try_exists()? {
            roots.push(ancestor.join(crate::downloader::ARCHIVE_ROOT_DIR));
            break;
        }
    }
    let candidate = resolved_private_candidate(path)?;
    // An explicit absolute path may enter a library unrelated to the cwd, so
    // the candidate's own marker ancestors identify further canonical archives.
    for ancestor in candidate.ancestors().skip(1) {
        match ancestor.join(".narou").try_exists() {
            Ok(true) => roots.push(ancestor.join(crate::downloader::ARCHIVE_ROOT_DIR)),
            Ok(false) => {}
            Err(error) => return Err(error.into()),
        }
    }
    for root in roots {
        if candidate.starts_with(resolved_private_candidate(&root)?) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(feature = "native-runtime")]
fn resolved_private_candidate(path: &std::path::Path) -> Result<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
    let mut existing = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut suffix = Vec::new();
    let mut normalize_parent = false;
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut resolved) => {
                for component in suffix.into_iter().rev() { resolved.push(component); }
                let mut normalized = PathBuf::new();
                for component in resolved.components() {
                    match component {
                        Component::CurDir => {}
                        Component::ParentDir => {
                            if !normalized.pop() {
                                return Err(NarouError::Platform("invalid private workspace path".into()));
                            }
                        }
                        other => normalized.push(other.as_os_str()),
                    }
                }
                if normalize_parent { return resolved_private_candidate(&normalized); }
                return Ok(normalized);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&existing) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let target = std::fs::read_link(&existing)?;
                        existing = if target.is_absolute() {
                            target
                        } else {
                            existing.parent().ok_or_else(|| NarouError::Platform("invalid symlink parent".into()))?.join(target)
                        };
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                let component = existing.components().next_back().ok_or_else(|| {
                    NarouError::Platform(format!("cannot resolve private workspace path: {}", path.display()))
                })?;
                if !matches!(component, Component::Normal(_) | Component::CurDir | Component::ParentDir) {
                    return Err(error.into());
                }
                normalize_parent |= component == Component::ParentDir;
                suffix.push(component.as_os_str().to_owned());
                if !existing.pop() { return Err(error.into()); }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// 論理操作の開始時点に固定した writer epoch と、それを使う guarded write
/// 群。`clone` は同じ操作内で helper へ渡すためのもので、操作をまたいだ
/// 使い回しはしない (epoch は capture 時点の値のまま)。
#[derive(Clone)]
pub struct NovelWriteGuard {
    objects: Arc<dyn ObjectStore>,
    assets: Option<Arc<dyn AssetStore>>,
    fence: WriteFence,
}

impl NovelWriteGuard {
    /// `objects.access_fence().capture_writer(prefix)` を 1 度だけ呼ぶ。
    /// fence を持たない保存先、gate が Open でない保存先はそのままエラー。
    ///
    /// `assets` は [`Self::write_stream`] / [`Self::install_staged`] を使う
    /// 操作だけ渡す。`None` のまま呼ぶと同メソッドは capability エラーになる。
    pub async fn capture(
        objects: &Arc<dyn ObjectStore>,
        assets: Option<&Arc<dyn AssetStore>>,
        prefix: &ObjectPrefix,
    ) -> Result<Self> {
        let fence = objects
            .access_fence()
            .ok_or_else(|| unsupported_fence(prefix))?
            .capture_writer(prefix)
            .await?;
        Ok(Self {
            objects: objects.clone(),
            assets: assets.cloned(),
            fence,
        })
    }

    /// 小説キー集合から prefix を取って [`Self::capture`] する。
    pub async fn capture_for(
        objects: &Arc<dyn ObjectStore>,
        assets: Option<&Arc<dyn AssetStore>>,
        keys: &NovelObjectKeys,
    ) -> Result<Self> {
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        Self::capture(objects, assets, &prefix).await
    }

    /// 呼び出し側が既に固定した [`WriteFence`] を包む。再 capture はしない。
    /// checkpoint 等の ordinary writer epoch を引き継ぐ経路向け。
    /// 保存先が fence を持たない場合だけエラーにする。
    pub fn from_fence(
        objects: Arc<dyn ObjectStore>,
        assets: Option<Arc<dyn AssetStore>>,
        fence: WriteFence,
    ) -> Result<Self> {
        if objects.access_fence().is_none() {
            return Err(unsupported_fence(&fence.prefix));
        }
        Ok(Self {
            objects,
            assets,
            fence,
        })
    }

    pub fn fence(&self) -> &WriteFence {
        &self.fence
    }

    /// Bind invocation ownership without changing the original prefix or epoch.
    pub fn bind_job_owner(&mut self, owner: Arc<crate::platform::JobExecutionOwner>) -> Result<()> {
        self.fence.bind_job_owner(owner)
    }

    /// Bind the original reservation resource before cloning this writer.
    pub fn retain_reservation(&mut self, reservation: &crate::platform::NovelIdReservation) -> Result<()> {
        self.fence.retain_reservation(reservation)
    }

    pub fn prefix(&self) -> &ObjectPrefix {
        &self.fence.prefix
    }

    pub fn epoch(&self) -> u64 {
        self.fence.epoch
    }

    /// guard 対象はこの小説 prefix 配下の canonical キーだけ。staging /
    /// transfer の私有キーを canonical 動詞へ通さない。
    fn check_canonical(&self, key: &ObjectKey) -> Result<()> {
        if !self.fence.prefix.matches(key) {
            return Err(key_outside_novel(&self.fence.prefix, key));
        }
        if is_reserved_key(key) {
            return Err(NarouError::Platform(format!(
                "reserved storage key is not a canonical novel object: {key}"
            )));
        }
        Ok(())
    }

    fn require_assets(&self) -> Result<&Arc<dyn AssetStore>> {
        self.assets.as_ref().ok_or_else(|| {
            NarouError::Platform(format!(
                "asset store required for guarded stream write: {}",
                self.fence.prefix.as_ref()
            ))
        })
    }

    /// `write_small_guarded` そのまま。epoch が失効していれば保存先が
    /// 拒否し、新しい canonical データは残る。
    pub async fn write_small(&self, key: &ObjectKey, data: Vec<u8>) -> Result<()> {
        self.check_canonical(key)?;
        self.objects
            .write_small_guarded(&self.fence, key, data)
            .await
    }

    /// `write_stream_guarded` そのまま (`AssetStore` 経由)。
    pub async fn write_stream(&self, key: &ObjectKey, stream: AssetStream) -> Result<()> {
        self.check_canonical(key)?;
        self.require_assets()?
            .write_stream_guarded(&self.fence, key, stream)
            .await
    }

    /// `delete_guarded` そのまま。
    pub async fn delete(&self, key: &ObjectKey) -> Result<()> {
        self.check_canonical(key)?;
        self.objects.delete_guarded(&self.fence, key).await
    }

    /// 私有 stage 済みの byte を canonical キーへ epoch 付きで公開する。
    ///
    /// `commit_staged` は gate を閉じた publisher (transfer v2) 専用で、
    /// 通常 writer は stage を読み直して `write_stream_guarded` する
    /// こちらを使う。stage 側の削除は呼び出し側の都合に任せる。
    pub async fn install_staged(&self, stage: &ObjectKey, canonical: &ObjectKey) -> Result<()> {
        self.check_canonical(canonical)?;
        let assets = self.require_assets()?;
        let Some(stream) = assets.read_stream(stage).await? else {
            return Err(NarouError::Platform(format!(
                "staged object disappeared before install: {stage}"
            )));
        };
        assets
            .write_stream_guarded(&self.fence, canonical, stream)
            .await
    }

    /// 制御オブジェクトの read-modify-commit を epoch 付き CAS として行う
    /// (`ObjectStore::update_small_guarded`)。存在しない時だけ書く
    /// "create if absent" もここ。unguarded な read+write への縮退はしない。
    pub async fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> Result<ObjectUpdateResult> {
        self.check_canonical(key)?;
        self.objects
            .update_small_guarded(&self.fence, key, mutate)
            .await
    }

}

/// 1 回の論理 read 操作 (複数ファイル可) にまたがる reader admission。
///
/// 成功した EOF / エラー時は `release().await` で返す。キャンセル・パニック
/// ではそのまま drop する — detached task を立てず、30 秒の TTL が落とす。
struct ReaderRenewal {
    lease: Arc<GateLease>,
    next_renewal: i64,
}

struct SharedReader {
    renewal: futures::lock::Mutex<ReaderRenewal>,
    owners: AtomicUsize,
}

pub struct NovelReadLease {
    objects: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
    prefix: Arc<ObjectPrefix>,
    reader_hint: Arc<str>,
    lease: Arc<GateLease>,
    shared: Arc<SharedReader>,
    retired: bool,
}

impl Clone for NovelReadLease {
    fn clone(&self) -> Self {
        self.shared.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            objects: self.objects.clone(), clock: self.clock.clone(),
            prefix: self.prefix.clone(), reader_hint: self.reader_hint.clone(),
            lease: self.lease.clone(), shared: self.shared.clone(), retired: false,
        }
    }
}


impl Drop for NovelReadLease {
    fn drop(&mut self) {
        if !self.retired {
            // Cancellation never spawns work. An abandoned final handle is
            // recovered by the durable lease TTL, not by async Drop.
            self.shared.owners.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl NovelReadLease {
    /// `SystemClock` 版の [`Self::admit_with_clock`]。
    pub async fn admit(
        objects: &Arc<dyn ObjectStore>,
        prefix: &ObjectPrefix,
        reader_id: &str,
    ) -> Result<Self> {
        Self::admit_with_clock(objects, Arc::new(SystemClock), prefix, reader_id).await
    }

    /// 小説キー集合から prefix を取って [`Self::admit`] する。
    pub async fn admit_for(
        objects: &Arc<dyn ObjectStore>,
        keys: &NovelObjectKeys,
        reader_id: &str,
    ) -> Result<Self> {
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        Self::admit(objects, &prefix, reader_id).await
    }

    /// admission gate と同じ CAS 境界で reader を登録する。gate が閉じて
    /// いる・fence を持たない保存先では lease を返さない。
    pub async fn admit_with_clock(
        objects: &Arc<dyn ObjectStore>,
        clock: Arc<dyn Clock>,
        prefix: &ObjectPrefix,
        reader_id: &str,
    ) -> Result<Self> {
        let lease = objects
            .access_fence()
            .ok_or_else(|| unsupported_fence(prefix))?
            .admit_reader(prefix, &unique_reader_id(reader_id)?, READER_LEASE_SECS)
            .await?;
        let lease = Arc::new(lease);
        let next_renewal = clock.now_unix_secs() + RENEW_INTERVAL_SECS;
        Ok(Self {
            objects: objects.clone(), prefix: Arc::new(prefix.clone()), clock,
            reader_hint: Arc::from(reader_id),
            lease: lease.clone(),
            shared: Arc::new(SharedReader {
                renewal: futures::lock::Mutex::new(ReaderRenewal { lease, next_renewal }),
                owners: AtomicUsize::new(1),
            }),
            retired: false,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.lease.epoch
    }

    pub fn lease(&self) -> &GateLease {
        &self.lease
    }

    pub fn prefix(&self) -> &ObjectPrefix {
        &self.prefix
    }

    /// 10 秒間隔を越えて進行しているなら renew する。admission と同じ
    /// CAS 条件なので、gate 閉鎖後はここで `novel_busy` 系のエラーになり、
    /// 失効 reader が新しい generation を読み続けることを止める。
    pub async fn refresh(&mut self) -> Result<()> {
        let mut current = self.shared.renewal.lock().await;
        let now = self.clock.now_unix_secs();
        if now >= current.next_renewal {
            current.lease = Arc::new(self.objects.access_fence()
                .ok_or_else(|| unsupported_fence(&self.prefix))?
                .renew_reader(&self.prefix, &current.lease, READER_LEASE_SECS).await?);
            current.next_renewal = now + RENEW_INTERVAL_SECS;
        }
        self.lease = current.lease.clone();
        Ok(())
    }

    /// Authoritative metadata barrier. Unlike cadence-limited refresh, this
    /// always performs the same atomic CAS as admission, so a gate closed
    /// within the first ten seconds cannot expose a mixed metadata response.
    pub async fn validate(&mut self) -> Result<()> {
        let mut current = self.shared.renewal.lock().await;
        current.lease = Arc::new(self.objects.access_fence()
            .ok_or_else(|| unsupported_fence(&self.prefix))?
            .renew_reader(&self.prefix, &current.lease, READER_LEASE_SECS).await?);
        current.next_renewal = self.clock.now_unix_secs() + RENEW_INTERVAL_SECS;
        self.lease = current.lease.clone();
        Ok(())
    }

    /// 論理ファイルを開くたびの入口。`refresh()` で lease を前進させてから
    /// `read_stream_leased` へ渡す。`range.end == u64::MAX` は末尾まで。
    pub async fn open(
        &mut self,
        key: &ObjectKey,
        range: Range<u64>,
    ) -> Result<Option<AssetStream>> {
        if !self.prefix.matches(key) {
            return Err(key_outside_novel(&self.prefix, key));
        }
        self.refresh().await?;
        self.objects
            .read_stream_leased(&self.prefix, &self.lease, key, range)
            .await
    }

    /// 先頭から末尾までの [`Self::open`]。
    pub async fn open_all(&mut self, key: &ObjectKey) -> Result<Option<AssetStream>> {
        self.open(key, 0..u64::MAX).await
    }

    /// 制御 object (toc/section/setting/replace 等) を leased stream 経由で
    /// 丸ごと読む。`read_small` の unguarded 近道は使わない。
    pub async fn read_small(&mut self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        let Some(mut stream) = self.open_all(key).await? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            self.refresh().await?;
            let chunk = chunk?;
            if (bytes.len() as u64).saturating_add(chunk.len() as u64) > crate::platform::SMALL_CAP {
                return Err(NarouError::Platform(format!("control object exceeds {} bytes: {key}", crate::platform::SMALL_CAP)));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }

    /// Recheck immutable references without re-fetching their bodies. Mutable
    /// fallbacks still read through this exact lease, never an unguarded store.
    pub async fn verify_fingerprints<'input>(
        &mut self,
        fingerprints: &'input [ObjectFingerprint],
    ) -> Result<Option<ObjectFingerprintMismatch<'input>>> {
        for fingerprint in fingerprints {
            if !self.prefix.matches(&fingerprint.key) {
                return Err(key_outside_novel(&self.prefix, &fingerprint.key));
            }
        }
        self.validate().await?;
        let objects = self.objects.clone();
        let mismatch = objects.verify_fingerprints(fingerprints, self).await?;
        self.validate().await?;
        Ok(mismatch)
    }

    /// lease を response body 相当の stream へ移す。
    ///
    /// 先に `open` で対象を開き、成功した read access を返す stream の所有物
    /// にする (`Some` の場合)。stream は chunk を流す間 10 秒間隔で renew
    /// し、EOF または最初のエラーで `release_reader` を inline で呼んでから
    /// 終わる — body が走り終わるまで read access を持ち続け、Response を
    /// 組み立てた直後に解放することはない。途中で drop された場合
    /// (cancel) は TTL が解放する。`Ok(None)` (object 無し) と `Err` の
    /// 場合はここで解放済みの lease を返さない。
    pub async fn into_open(
        self,
        key: &ObjectKey,
        range: Range<u64>,
    ) -> Result<Option<AssetStream>> {
        let mut this = self;
        let opened = this.open(key, range).await;
        match opened {
            Ok(Some(inner)) => Ok(Some(into_leased_stream(this, inner))),
            Ok(None) => {
                let _ = this.release_inner().await;
                Ok(None)
            }
            Err(error) => {
                let _ = this.release_inner().await;
                Err(error)
            }
        }
    }

    /// `into_open` または呼び出し側の EOF/エラー処理で使う内部解放。
    /// renew/readmit が差し替えた *現在の* token を解放する (`self.lease` の
    /// スナップショットは clone 間でずれ得るため使わない)。
    async fn release_inner(&mut self) -> Result<()> {
        if self.retired { return Ok(()); }
        self.retired = true;
        if self.shared.owners.fetch_sub(1, Ordering::AcqRel) != 1 { return Ok(()); }
        let token = self.shared.renewal.lock().await.lease.clone();
        self.objects.access_fence().ok_or_else(|| unsupported_fence(&self.prefix))?
            .release_reader(&self.prefix, &token).await
    }

    /// 操作の正常終了時に呼ぶ冪等な早期解放。失敗しても TTL が落とすので
    /// 呼び出し側で再試行は要らない。
    pub async fn release(mut self) -> Result<()> {
        self.release_inner().await
    }

    /// 共有されている *現在の* token が durable な期限切れかどうか。
    /// `renewal` ロックを取るので renew/readmit 中の更新を見逃さない。
    /// 時計は admission に注入されたものと同じ (`self.clock`) を使う。
    async fn expired(&mut self) -> bool {
        let current = self.shared.renewal.lock().await;
        self.clock.now_unix_secs() >= current.lease.expires_at
    }

    /// TTL 切れした共有 token を、**同じ gate epoch のまま** 新しい admission
    /// で置き換える。`self.lease` だけでなく共有 renewal を更新するため、
    /// この lease を clone した既存の leased stream も自動的に生き返る。
    /// gate が閉じてから再開放されている (`epoch != expected_epoch`) 場合は
    /// 新しいリースを即座に解放して `novel_busy` で fail-closed する —
    /// 転送・削除をまたいだ新世代への暗黙昇格はしない。
    async fn readmit(&mut self, expected_epoch: u64) -> Result<()> {
        let mut current = self.shared.renewal.lock().await;
        // `admit_reader` は admission CAS と同じ境界なので、閉鎖中はここで
        // `novel_recovering` 系のエラーになり epoch 検証に到達しない。
        let next = self.objects.access_fence()
            .ok_or_else(|| unsupported_fence(&self.prefix))?
            .admit_reader(&self.prefix, &unique_reader_id(&self.reader_hint)?, READER_LEASE_SECS)
            .await?;
        if next.epoch != expected_epoch {
            let _ = self.objects.access_fence().ok_or_else(|| unsupported_fence(&self.prefix))?
                .release_reader(&self.prefix, &next).await;
            return Err(NarouError::Platform("novel_busy: writer origin changed before reader re-admission".into()));
        }
        let previous = std::mem::replace(&mut current.lease, Arc::new(next));
        current.next_renewal = self.clock.now_unix_secs() + RENEW_INTERVAL_SECS;
        self.lease = current.lease.clone();
        drop(current);
        // 失効行は GC が拾うが、開いている epoch の行は drain 対象として残さない
        // よう best-effort で削除する (idempotent なので失敗してもよい)。
        let _ = self.objects.access_fence().ok_or_else(|| unsupported_fence(&self.prefix))?
            .release_reader(&self.prefix, &previous).await;
        Ok(())
    }
}

impl ObjectFingerprintReader for NovelReadLease {
    fn read_small<'a>(&'a mut self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(NovelReadLease::read_small(self, key))
    }
}

/// `NovelReadLease::into_open` が返す、lease を所有する stream。
fn into_leased_stream(lease: NovelReadLease, inner: AssetStream) -> AssetStream {
    Box::pin(futures::stream::try_unfold(
        (lease, inner),
        |(mut lease, mut inner)| async move {
            // chunk を消費する直前に renew しておく。gate が閉じれば
            // renew 失敗としてここでエラーを返し、古い generation の
            // 残り chunk を成功扱いにしない。
            if let Err(error) = lease.refresh().await {
                let _ = lease.release_inner().await;
                return Err(error);
            }
            match inner.next().await {
                Some(Ok(chunk)) => Ok(Some((chunk, (lease, inner)))),
                Some(Err(error)) => {
                    let _ = lease.release_inner().await;
                    Err(error)
                }
                None => {
                    let _ = lease.release_inner().await;
                    Ok(None)
                }
            }
        },
    ))
    // 上の型が `(NovelReadLease, AssetStream)` を持ち回るので、そのままの
    // シグネチャで AssetStream (= Pin<Box<dyn Stream<..>>>) に入る。
}

/// Durable writer origin for a resumed logical operation. Missing legacy origins
/// cannot prove ownership of already-completed work and must restart the job.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccessOrigin {
    pub prefix: String,
    pub epoch: u64,
}

impl NovelWriteGuard {
    pub fn origin(&self) -> AccessOrigin {
        AccessOrigin { prefix: self.fence.prefix.as_ref().to_owned(), epoch: self.fence.epoch }
    }

    /// Atomically rebind only the persisted open epoch and its backend proof.
    /// A closed/changed epoch is rejected, never replaced by today's token.
    pub async fn resume(
        objects: Arc<dyn ObjectStore>,
        assets: Option<Arc<dyn AssetStore>>,
        origin: &AccessOrigin,
    ) -> Result<Self> {
        let prefix = ObjectPrefix::new(origin.prefix.clone())?;
        let fence = objects.access_fence().ok_or_else(|| unsupported_fence(&prefix))?
            .rebind_writer(&prefix, origin.epoch).await?;
        Self::from_fence(objects, assets, fence)
    }
}

/// The operation-owned pair; services never hide admission in a long-lived
/// singleton. Capture this before fetching, then carry it through every caller.
pub struct NovelAccess {
    write: Option<NovelWriteGuard>,
    read: Option<NovelReadLease>,
    clock: Arc<dyn Clock>,
    reader_hint: Arc<str>,
}

impl NovelAccess {
    pub async fn for_operation(
        objects: &Arc<dyn ObjectStore>,
        assets: Option<&Arc<dyn AssetStore>>,
        prefix: &ObjectPrefix,
        reader_id: &str,
    ) -> Result<Self> {
        Self::for_operation_with_clock(objects, assets, prefix, reader_id, Arc::new(SystemClock)).await
    }

    /// テストが読み取り寿命を制御できる clock 注入版。
    pub async fn for_operation_with_clock(
        objects: &Arc<dyn ObjectStore>,
        assets: Option<&Arc<dyn AssetStore>>,
        prefix: &ObjectPrefix,
        reader_id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let write = NovelWriteGuard::capture(objects, assets, prefix).await?;
        Self::from_writer_with_clock(write, reader_id, clock).await
    }

    pub async fn from_writer(write: NovelWriteGuard, reader_id: &str) -> Result<Self> {
        Self::from_writer_with_clock(write, reader_id, Arc::new(SystemClock)).await
    }

    /// clock 注入版の [`Self::from_writer`]。`ensure_read` の期限切れ判定に
    /// 同じ時計を使うため、reader 側には `admit_with_clock` で同じ clock を
    /// 渡す。
    pub async fn from_writer_with_clock(
        write: NovelWriteGuard,
        reader_id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let read = NovelReadLease::admit_with_clock(&write.objects, clock.clone(), write.prefix(), reader_id).await?;
        if read.epoch() != write.epoch() {
            let _ = read.release().await;
            return Err(NarouError::Platform("novel_busy: writer origin changed before reader admission".into()));
        }
        Ok(Self { write: Some(write), read: Some(read), clock, reader_hint: Arc::from(reader_id) })
    }

    pub fn read_only(read: NovelReadLease) -> Self {
        let clock = read.clock.clone();
        let reader_hint = read.reader_hint.clone();
        Self { write: None, read: Some(read), clock, reader_hint }
    }

    pub fn write(&self) -> Result<&NovelWriteGuard> {
        self.write.as_ref().ok_or_else(|| NarouError::Platform("novel operation has no writer fence".into()))
    }

    /// 生のリース参照。期限切れかどうかは **見ない**。ストレージ読み取りの
    /// 直前には [`Self::ensure_read`] を使うこと。
    pub fn read(&mut self) -> Result<&mut NovelReadLease> {
        self.read.as_mut().ok_or_else(|| NarouError::Platform("novel operation has no reader lease".into()))
    }

    pub fn take_read(&mut self) -> Result<NovelReadLease> {
        self.read.take().ok_or_else(|| NarouError::Platform("novel operation has no reader lease".into()))
    }

    /// 外部 HTTP 待機やリースを必要としない重い作業の前に、保持中の
    /// リースを明示的に解放して `self.read = None` にする。アイドルな
    /// リースが drain を妨げたり、待機中に TTL 切れで失効したりするのを
    /// 防ぐ。リースを必要としない呼び出しの前に使うこと。
    pub async fn release_read(&mut self) -> Result<()> {
        match self.read.take() {
            Some(read) => read.release().await,
            None => Ok(()),
        }
    }

    /// ストレージ読み取りの直前に呼ぶ。writer (`self.write`) を持つ操作に
    /// 限り、失効したリースを **同じ gate epoch** で再 admit する。
    ///
    /// - `self.read` がまだ有効なら何もせずそのリースを返す。
    /// - `None` (前回 `release_read` した) か、TTL が切れているなら再
    ///   admit する。再 admit が返した `read.epoch() != write.epoch()` なら
    ///   (その間に転送・削除が gate を閉じて epoch を進めた) 新しいリースを
    ///   即座に解放して fail-closed する — 新世代への暗黙昇格はしない。
    /// - `self.write` を持たない `read_only` では再 admit しない。期限切れの
    ///   まま従来どおりのリースを返し、renew/read で fail-closed させる。
    ///
    /// `renew` は同じ CAS 境界で `expires_at > now` を要求する。30 秒を越える
    /// 外部 I/O の間は `release_read` → `ensure_read` で再 admit するほうが、
    /// リースを保持したまま期限切れになるより drain も早く安全である。
    pub async fn ensure_read(&mut self) -> Result<&mut NovelReadLease> {
        let Some(write) = self.write.as_ref() else {
            return self.read();
        };
        let epoch = write.epoch();
        match self.read.as_mut() {
            Some(read) => {
                if read.epoch() != epoch {
                    // ゲートが閉じて再開放された (epoch が進んだ)。読み取りを
                    // 引き継げないので fail-closed する。
                    return Err(NarouError::Platform("novel_busy: writer origin changed before reader re-admission".into()));
                }
                if read.expired().await {
                    read.readmit(epoch).await?;
                }
            }
            None => {
                let read = NovelReadLease::admit_with_clock(
                    &write.objects, self.clock.clone(), write.prefix(), &self.reader_hint,
                ).await?;
                if read.epoch() != epoch {
                    let _ = read.release().await;
                    return Err(NarouError::Platform("novel_busy: writer origin changed before reader re-admission".into()));
                }
                self.read = Some(read);
            }
        }
        self.read()
    }

    pub async fn release(mut self) -> Result<()> {
        match self.read.take() {
            Some(read) => read.release().await,
            None => Ok(()),
        }
    }
}

/// Lazy per-prefix admission for a virtual book or another multi-novel read.
/// The scope must be moved into a deferred body/section stream, not released
/// immediately after constructing it. Cancellation falls back to lease expiry.
#[derive(Clone)]
pub struct NovelReadScope {
    objects: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
    reader_hint: Arc<str>,
    leases: SharedState<futures::lock::Mutex<BTreeMap<String, NovelReadLease>>>,
}

impl NovelReadScope {
    pub fn new(objects: &Arc<dyn ObjectStore>, reader_hint: &str) -> Self {
        Self::with_clock(objects, Arc::new(SystemClock), reader_hint)
    }

    pub fn with_clock(objects: &Arc<dyn ObjectStore>, clock: Arc<dyn Clock>, reader_hint: &str) -> Self {
        Self { objects: objects.clone(), clock, reader_hint: Arc::from(reader_hint), leases: SharedState::new(futures::lock::Mutex::new(BTreeMap::new())) }
    }

    pub async fn lease_for(&mut self, prefix: &ObjectPrefix) -> Result<NovelReadLease> {
        let mut leases = self.leases.lock().await;
        if !leases.contains_key(prefix.as_ref()) {
            let lease = NovelReadLease::admit_with_clock(
                &self.objects, self.clock.clone(), prefix, &self.reader_hint,
            ).await?;
            leases.insert(prefix.as_ref().to_owned(), lease);
        }
        let lease = leases.get_mut(prefix.as_ref()).expect("admitted prefix");
        lease.refresh().await?;
        Ok(lease.clone())
    }

    pub async fn open(&mut self, prefix: &ObjectPrefix, key: &ObjectKey, range: Range<u64>) -> Result<Option<AssetStream>> {
        self.lease_for(prefix).await?.into_open(key, range).await
    }

    /// Synchronize a clone of only the longest ALREADY-admitted reader. A
    /// preferred backend may open with this same reader ID/epoch; this never
    /// resolves a binding or creates a second admission for an image key.
    pub async fn lease_for_key(&mut self, key: &ObjectKey) -> Result<NovelReadLease> {
        let mut lease = {
            let leases = self.leases.lock().await;
            leases.values().filter(|lease| lease.prefix().matches(key))
                .max_by_key(|lease| lease.prefix().as_ref().trim_end_matches('/').len())
                .cloned()
                .ok_or_else(|| NarouError::Platform(format!("novel content key has no already-admitted reader: {key}")))?
        };
        lease.refresh().await?;
        Ok(lease)
    }

    /// Open only under the longest admission already retained by this
    /// operation. Never infer/admit a prefix from an image key, nor fall back
    /// from a stale child admission to a still-open parent admission.
    pub async fn open_key(&mut self, key: &ObjectKey, range: Range<u64>) -> Result<Option<AssetStream>> {
        self.lease_for_key(key).await?.into_open(key, range).await
    }

    pub async fn read_small(&mut self, prefix: &ObjectPrefix, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        self.lease_for(prefix).await?.read_small(key).await
    }

    /// Renew every retained prefix while a composite/deferred response makes
    /// progress. Never re-admit an expired prefix at a newer epoch.
    pub async fn refresh_all(&mut self) -> Result<()> {
        let mut leases = self.leases.lock().await;
        for lease in leases.values_mut() { lease.refresh().await?; }
        Ok(())
    }

    pub async fn release_all(self) -> Result<()> {
        // Other scopes still own the same map and its original admissions.
        let Ok(leases) = SharedState::try_unwrap(self.leases) else { return Ok(()); };
        let mut failure = None;
        for (_, lease) in leases.into_inner() {
            if let Err(error) = lease.release().await && failure.is_none() { failure = Some(error); }
        }
        match failure { Some(error) => Err(error), None => Ok(()) }
    }
}

/// Resolve the runtime's authoritative durable novel binding, never a guessed
/// path-depth split. `None` is only for global/private, non-novel keys.
pub trait NovelPrefixResolver: PlatformService {
    fn novel_prefix<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectPrefix>>>;
}

/// Original writer epochs shared by source and destination for one bounded
/// migration page. Never reuse these origins in a later admin invocation.
pub struct NovelMigrationOrigins {
    authority: Arc<dyn ObjectStore>,
    resolver: Arc<dyn NovelPrefixResolver>,
    state: futures::lock::Mutex<MigrationOriginState>,
}

#[derive(Default)]
struct MigrationOriginState {
    bindings: BTreeMap<String, Option<ObjectPrefix>>,
    writers: BTreeMap<String, NovelWriteGuard>,
}

impl NovelMigrationOrigins {
    pub fn new(authority: Arc<dyn ObjectStore>, resolver: Arc<dyn NovelPrefixResolver>) -> Result<SharedState<Self>> {
        if authority.access_fence().is_none() {
            return Err(NarouError::Platform("novel migration requires an authoritative access fence".into()));
        }
        Ok(SharedState::new(Self { authority, resolver, state: futures::lock::Mutex::new(MigrationOriginState::default()) }))
    }

    /// Return the same original token for legacy-only retirement as for
    /// reading/copying. A destination must not recapture after source IO.
    pub async fn writer(&self, key: &ObjectKey) -> Result<Option<NovelWriteGuard>> {
        let mut state = self.state.lock().await;
        let prefix = match state.bindings.get(key.as_ref()) {
            Some(prefix) => prefix.clone(),
            None => {
                let prefix = self.resolver.novel_prefix(key).await?;
                if let Some(prefix) = &prefix {
                    if !prefix.matches(key) { return Err(key_outside_novel(prefix, key)); }
                } else if key.as_ref() == "novels" || key.as_ref().starts_with("novels/") {
                    return Err(NarouError::Platform(format!("novel gate binding is missing: {key}")));
                }
                state.bindings.insert(key.as_ref().to_owned(), prefix.clone());
                prefix
            }
        };
        let Some(prefix) = prefix else { return Ok(None); };
        if let Some(writer) = state.writers.get(prefix.as_ref()) { return Ok(Some(writer.clone())); }
        let writer = NovelWriteGuard::capture(&self.authority, None, &prefix).await?;
        state.writers.insert(prefix.as_ref().to_owned(), writer.clone());
        Ok(Some(writer))
    }
}

/// Logical reference-aware view for the existing migration algorithms. Both
/// facets share origins; each scope must use that facet's exact object Arc.
/// Never pass raw legacy D1/S3 bytes where an immutable canonical ref exists.
/// Drop facets/streams, then release the retained scopes on success or error.
/// Cancellation uses the existing durable lease TTL, without detached tasks.
pub struct NovelMigrationStore {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    origins: SharedState<NovelMigrationOrigins>,
    reads: NovelReadScope,
}

impl NovelMigrationStore {
    pub fn new(
        objects: Arc<dyn ObjectStore>, assets: Arc<dyn AssetStore>,
        origins: SharedState<NovelMigrationOrigins>, reads: NovelReadScope,
    ) -> Result<Self> {
        if objects.access_fence().is_none() {
            return Err(NarouError::Platform("migration logical view has no access fence".into()));
        }
        if !Arc::ptr_eq(&objects, &reads.objects) {
            return Err(NarouError::Platform("migration read scope belongs to another logical store view".into()));
        }
        Ok(Self { objects, assets, origins, reads })
    }

    async fn reader_for(&self, writer: &NovelWriteGuard) -> Result<NovelReadLease> {
        let mut scope = self.reads.clone();
        let mut reader = scope.lease_for(writer.prefix()).await?;
        if reader.epoch() != writer.epoch() {
            let _ = reader.release().await;
            return Err(NarouError::Platform("novel_busy: migration origin changed before source open".into()));
        }
        reader.validate().await?;
        Ok(reader)
    }

    async fn metadata_lease(&self, key: &ObjectKey) -> Result<Option<NovelReadLease>> {
        match self.origins.writer(key).await? {
            Some(writer) => self.reader_for(&writer).await.map(Some),
            None => Ok(None),
        }
    }

    async fn open(&self, key: &ObjectKey, range: Range<u64>) -> Result<Option<AssetStream>> {
        match self.origins.writer(key).await? {
            Some(writer) => self.reader_for(&writer).await?.into_open(key, range).await,
            None => self.assets.read_stream_range(key, range).await,
        }
    }

    async fn retirement_writer(&self, logical: &ObjectKey) -> Result<NovelWriteGuard> {
        let (prefix, _) = logical.as_ref().rsplit_once("/挿絵/")
            .filter(|(prefix, filename)| !prefix.is_empty() && !filename.is_empty() && !filename.contains('/'))
            .ok_or_else(|| NarouError::Platform(format!("migration retirement has no illustration origin: {logical}")))?;
        let state = self.origins.state.lock().await;
        let binding = state.bindings.get(logical.as_ref()).and_then(Option::as_ref)
            .filter(|binding| binding.as_ref().trim_end_matches('/') == prefix)
            .ok_or_else(|| NarouError::Platform(format!("migration retirement has no cached source binding: {logical}")))?;
        state.writers.get(binding.as_ref()).filter(|writer| writer.prefix().matches(logical)).cloned()
            .ok_or_else(|| NarouError::Platform(format!("migration retirement has no cached original writer fence: {logical}")))
    }
}

impl ObjectStore for NovelMigrationStore {
    fn access_fence(&self) -> Option<&dyn NovelAccessFence> { self.origins.authority.access_fence() }

    fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a GateLease,
        key: &'a ObjectKey, range: Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        self.objects.read_stream_leased(prefix, lease, key, range)
    }

    fn is_remote_reference<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut reader = self.metadata_lease(key).await?;
            let result = self.objects.is_remote_reference(key).await;
            if let Some(reader) = &mut reader { reader.validate().await?; }
            result
        })
    }

    fn write_small_guarded<'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey,
        bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        self.objects.write_small_guarded(fence, key, bytes)
    }

    fn delete_guarded<'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        self.objects.delete_guarded(fence, key)
    }

    fn update_small_guarded<'a, 'data: 'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        self.objects.update_small_guarded(fence, key, mutate)
    }

    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(async move {
            let mut reader = self.metadata_lease(key).await?;
            let result = self.objects.stat(key).await;
            if let Some(reader) = &mut reader { reader.validate().await?; }
            result
        })
    }

    fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(async move {
            let page = self.objects.list_page(request).await?;
            // Pin the page's original epochs before source IO/destination
            // publication can await. Both facets use this same cache.
            for metadata in &page.objects { let _ = self.origins.writer(&metadata.key).await?; }
            Ok(page)
        })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            match self.origins.writer(key).await? {
                Some(writer) => {
                    let mut reader = self.reader_for(&writer).await?;
                    let read = reader.read_small(key).await;
                    let _ = reader.release().await;
                    read
                }
                None => self.objects.read_small(key).await,
            }
        })
    }

    fn write_small<'a>(&'a self, key: &'a ObjectKey, bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.origins.writer(key).await? {
                Some(writer) => self.objects.write_small_guarded(writer.fence(), key, bytes).await,
                None => self.objects.write_small(key, bytes).await,
            }
        })
    }

    fn update_small<'a, 'data: 'a>(&'a self, key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move {
            match self.origins.writer(key).await? {
                Some(writer) => self.objects.update_small_guarded(writer.fence(), key, mutate).await,
                None => self.objects.update_small(key, mutate).await,
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.origins.writer(key).await? {
                Some(writer) => self.objects.delete_guarded(writer.fence(), key).await,
                None => self.objects.delete(key).await,
            }
        })
    }

    fn retire_legacy<'a>(&'a self, logical: &'a ObjectKey, pool: &'a ObjectKey,
        size: u64, sha256: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            // Discovery/open must have captured this token before pool IO.
            // No resolver, admission, capture, DELETE or tombstone fallback.
            let original = self.retirement_writer(logical).await?;
            self.objects.retire_legacy_guarded(original.fence(), logical, pool, size, sha256).await
        })
    }

    fn retire_legacy_guarded<'a>(&'a self, fence: &'a WriteFence, logical: &'a ObjectKey,
        pool: &'a ObjectKey, size: u64, sha256: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let original = self.retirement_writer(logical).await?;
            if original.prefix() != &fence.prefix || original.epoch() != fence.epoch {
                return Err(NarouError::Platform(format!("novel_busy: migration retirement lost its original writer fence: {logical}")));
            }
            self.objects.retire_legacy_guarded(original.fence(), logical, pool, size, sha256).await
        })
    }
}

impl AssetStore for NovelMigrationStore {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(async move {
            let mut reader = self.metadata_lease(key).await?;
            let result = self.assets.stat(key).await;
            if let Some(reader) = &mut reader { reader.validate().await?; }
            result
        })
    }

    fn read_stream<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(self.open(key, 0..u64::MAX))
    }

    fn read_stream_range<'a>(&'a self, key: &'a ObjectKey, range: Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(self.open(key, range))
    }

    fn read_range<'a>(&'a self, key: &'a ObjectKey, range: Range<u64>) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let length = range.end.checked_sub(range.start).filter(|length| *length <= 64 * 1024)
                .ok_or_else(|| NarouError::Platform("migration asset range exceeds 64 KiB".into()))?;
            let Some(mut stream) = self.open(key, range).await? else { return Ok(None); };
            let mut bytes = Vec::with_capacity(length as usize);
            while let Some(part) = stream.next().await {
                let part = part?;
                if bytes.len().saturating_add(part.len()) > length as usize {
                    return Err(NarouError::Platform("migration range exceeds requested length".into()));
                }
                bytes.extend_from_slice(&part);
            }
            if bytes.len() != length as usize {
                return Err(NarouError::Platform("migration asset range truncated".into()));
            }
            Ok(Some(bytes))
        })
    }

    fn write_stream_guarded<'a>(&'a self, fence: &'a WriteFence, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        self.assets.write_stream_guarded(fence, key, stream)
    }

    fn write_stream<'a>(&'a self, key: &'a ObjectKey, stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.origins.writer(key).await? {
                Some(writer) => self.assets.write_stream_guarded(writer.fence(), key, stream).await,
                None => self.assets.write_stream(key, stream).await,
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.origins.writer(key).await? {
                Some(writer) => self.objects.delete_guarded(writer.fence(), key).await,
                None => self.assets.delete(key).await,
            }
        })
    }

    fn copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let writer = self.origins.writer(destination).await?;
            let stream = self.open(source, 0..u64::MAX).await?.ok_or_else(|| NarouError::NotFound(source.to_string()))?;
            match writer {
                Some(writer) => self.assets.write_stream_guarded(writer.fence(), destination, stream).await,
                None => self.assets.write_stream(destination, stream).await,
            }
        })
    }

    fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if source == destination { return Ok(()); }
            let _source = self.origins.writer(source).await?;
            AssetStore::copy(self, source, destination).await?;
            AssetStore::delete(self, source).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{NovelAccessFence, mocks::MemoryObjectStore};
    use futures::StreamExt;

    #[test]
    fn writer_owner_and_reservation_clones_keep_one_immutable_runtime_capability() {
        futures::executor::block_on(async {
            let owner = Arc::new(crate::platform::JobExecutionOwner::new("job", "original-secret"));
            let replacement = Arc::new(crate::platform::JobExecutionOwner::new("job", "replacement-secret"));
            let objects: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
            let prefix = ObjectPrefix::new("novels/fixture/job-owner").unwrap();
            let mut write = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            let origin = write.origin();
            write.bind_job_owner(owner.clone()).unwrap();
            let mut detached = write.clone();
            assert!(Arc::ptr_eq(detached.fence().job_owner().unwrap(), &owner));
            detached.bind_job_owner(owner.clone()).unwrap();
            assert!(detached.bind_job_owner(replacement).is_err());
            assert!(Arc::ptr_eq(detached.fence().job_owner().unwrap(), &owner));
            assert_eq!(detached.origin(), origin);
            let reservation = crate::platform::NovelIdReservation::new(crate::platform::NovelId(1), 1)
                .with_job_owner(owner.clone()).unwrap();
            let mut from_reservation = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            from_reservation.retain_reservation(&reservation).unwrap();
            assert!(Arc::ptr_eq(from_reservation.fence().job_owner().unwrap(), &owner));
            assert_eq!(from_reservation.origin(), origin);
            let serialized = serde_json::to_string(&detached.origin()).unwrap();
            assert!(!serialized.contains("original-secret"));
            assert!(!format!("{:?}", detached.fence()).contains("original-secret"));
            reservation.adopt();
        });
    }

    #[cfg(feature = "native-runtime")]
    #[test]
    fn writer_clones_retain_the_same_locked_reservation_file_after_adoption() {
        futures::executor::block_on(async {
            let file = tempfile::NamedTempFile::new().unwrap();
            let resource = Arc::new(file.reopen().unwrap());
            fs2::FileExt::lock_exclusive(resource.as_ref()).unwrap();
            let weak = Arc::downgrade(&resource);
            let reservation = crate::platform::NovelIdReservation::new(crate::platform::NovelId(1), 1).with_liveness(resource);
            let objects: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
            let mut write = NovelWriteGuard::capture(&objects, None, &ObjectPrefix::new("novels/fixture/retained").unwrap()).await.unwrap();
            write.retain_reservation(&reservation).unwrap();
            let mut detached = write.clone();
            reservation.adopt();
            drop(write);
            assert!(weak.upgrade().is_some(), "a detached writer owns the original file resource");
            let probe = file.reopen().unwrap();
            assert!(fs2::FileExt::try_lock_exclusive(&probe).is_err(), "adoption cannot unlock a still-running writer");
            let replacement = crate::platform::NovelIdReservation::new(crate::platform::NovelId(1), 1)
                .with_liveness(Arc::new(file.reopen().unwrap()));
            assert!(detached.retain_reservation(&replacement).is_err(), "equal scalar IDs cannot replace an opaque lifetime");
            assert!(fs2::FileExt::try_lock_exclusive(&probe).is_err());
            drop(detached);
            assert!(weak.upgrade().is_none());
            fs2::FileExt::try_lock_exclusive(&probe).unwrap();
            fs2::FileExt::unlock(&probe).unwrap();
            replacement.adopt();
        });
    }

    #[test]
    fn stale_write_and_delete_preserve_replacement_bytes() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let assets: Arc<dyn AssetStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/novel.txt").unwrap();
            let stale = NovelWriteGuard::capture(&objects, Some(&assets), &prefix).await.unwrap();
            stale.write_small(&key, b"old".to_vec()).await.unwrap();
            let epoch = memory.close_gate(&prefix, "replacement").await.unwrap();
            memory.open_gate(&prefix, "replacement", epoch).await.unwrap();
            let fresh = NovelWriteGuard::capture(&objects, Some(&assets), &prefix).await.unwrap();
            fresh.write_small(&key, b"replacement".to_vec()).await.unwrap();
            assert!(stale.write_small(&key, b"late old result".to_vec()).await.is_err());
            assert!(stale.write_stream(&key, crate::platform::staging::owned_bytes_stream(b"late stream".to_vec())).await.is_err());
            assert!(stale.delete(&key).await.is_err());
            assert_eq!(memory.read_small(&key).await.unwrap().unwrap(), b"replacement");
        });
    }

    #[test]
    fn shared_scope_retains_body_epoch_through_later_image_open() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let body = ObjectKey::try_new("novels/fixture/book/本文/1.yaml").unwrap();
            let image = ObjectKey::try_new("novels/fixture/book/挿絵/a.png").unwrap();
            let write = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            write.write_small(&body, b"old body".to_vec()).await.unwrap();
            write.write_small(&image, b"old image".to_vec()).await.unwrap();
            let mut operation = NovelReadScope::new(&objects, "epub");
            let mut section_pass = operation.clone();
            assert_eq!(section_pass.read_small(&prefix, &body).await.unwrap().unwrap(), b"old body");
            section_pass.release_all().await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            let epoch = memory.close_gate(&prefix, "replace").await.unwrap();
            memory.open_gate(&prefix, "replace", epoch).await.unwrap();
            let replacement = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            replacement.write_small(&image, b"new image".to_vec()).await.unwrap();
            // Same reader id/epoch remains: image phase must not re-admit and
            // combine old body with replacement illustrations.
            assert!(operation.read_small(&prefix, &image).await.is_err());
            operation.release_all().await.unwrap();
            assert_eq!(memory.read_small(&image).await.unwrap().unwrap(), b"new image");
        });
    }

    #[test]
    fn streamed_eof_releases_only_the_final_owner_inline() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/novel.txt").unwrap();
            memory.write_small(&key, b"response bytes".to_vec()).await.unwrap();
            let owner = NovelReadLease::admit(&objects, &prefix, "response").await.unwrap();
            let mut body = owner.clone().into_open(&key, 0..u64::MAX).await.unwrap().unwrap();
            let mut actual = Vec::new();
            while let Some(chunk) = body.next().await { actual.extend(chunk.unwrap()); }
            assert_eq!(actual, b"response bytes");
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            owner.release().await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
        });
    }

    #[test]
    fn failed_control_read_can_release_the_operation_admission() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/toc.yaml").unwrap();
            memory.write_small(&key, vec![b'x'; crate::platform::SMALL_CAP as usize + 1]).await.unwrap();
            let mut read = NovelReadLease::admit(&objects, &prefix, "bounded-control").await.unwrap();
            assert!(read.read_small(&key).await.is_err());
            read.release().await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
        });
    }

    #[cfg(feature = "native-runtime")]
    #[test]
    fn private_proof_rejects_marker_discovered_archives_beyond_cwd_defaults() {
        struct RestoreDatabase(Option<crate::db::Database>);
        impl Drop for RestoreDatabase {
            fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); }
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let _database = RestoreDatabase(crate::db::DATABASE.lock().take());
        // DATABASE=None and cwd INSIDE the canonical book dir: the cwd-relative
        // default (book/小説データ) must not let this discovered archive pass.
        let canonical = temp.path().join(crate::downloader::ARCHIVE_ROOT_DIR).join("site/book");
        std::fs::create_dir_all(&canonical).unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(&canonical);
        assert!(require_private_path(&canonical.join("replace.txt")).is_err());
        // Absolute path into a library unrelated to the cwd: its own marker
        // ancestors make that archive canonical too.
        let other = tempfile::tempdir().unwrap();
        let other_archive = other.path().join(crate::downloader::ARCHIVE_ROOT_DIR).join("other");
        std::fs::create_dir_all(other.path().join(".narou")).unwrap();
        std::fs::create_dir_all(&other_archive).unwrap();
        assert!(require_private_path(&other_archive.join("setting.ini")).is_err());
        // The library root itself is not inside an archive, so it stays private.
        require_private_path(&temp.path().join("notes.txt")).unwrap();
    }

    #[cfg(feature = "native-runtime")]
    #[test]
    fn private_codec_boundary_rejects_canonical_and_missing_leaf_paths() {
        struct RestoreDatabase(Option<crate::db::Database>);
        impl Drop for RestoreDatabase {
            fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); }
        }
        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        let _database = RestoreDatabase(crate::db::DATABASE.lock().take());
        let canonical = temp.path().join(crate::downloader::ARCHIVE_ROOT_DIR).join("site/book");
        std::fs::create_dir_all(&canonical).unwrap();
        assert!(require_private_path(&canonical).is_err());
        assert!(require_private_path(&canonical.join("missing/挿絵/a.png")).is_err());
        assert!(require_private_path(&canonical.join("missing/../a.png")).is_err());
        assert!(crate::illustration_store::IllustrationStore::load(&canonical).is_err());
        let configured = temp.path().join("configured-library");
        let database = crate::db::Database::with_root(configured).unwrap();
        let configured_archive = database.archive_root().to_path_buf();
        *crate::db::DATABASE.lock() = Some(database);
        assert!(require_private_path(&configured_archive.join("site/book/missing.png")).is_err());
        assert!(require_private_path(&canonical).is_err(), "default root remains canonical with a configured library");
        assert!(crate::illustration_store::IllustrationStore::default().cached_filename_for_source("https://fixture.test/image", &canonical).is_err());
        assert!(crate::illustration_store::find_saved_illustration_filename(&canonical, "missing").is_err());
        let private = tempfile::tempdir().unwrap();
        require_private_path(private.path()).unwrap();
        require_private_path(&private.path().join("missing/../a.png")).unwrap();
        crate::illustration_store::IllustrationStore::load(private.path()).unwrap();
        #[cfg(unix)]
        {
            let link = private.path().join("canonical-link");
            std::os::unix::fs::symlink(&canonical, &link).unwrap();
            assert!(require_private_path(&link).is_err());
            assert!(require_private_path(&link.join("missing.png")).is_err());
            assert!(require_private_path(&private.path().join("missing/../canonical-link/image.png")).is_err());
            let dangling = private.path().join("dangling-canonical-link");
            std::os::unix::fs::symlink(canonical.join("not-yet-created"), &dangling).unwrap();
            assert!(require_private_path(&dangling.join("image.png")).is_err());
            let loop_path = private.path().join("symlink-loop");
            std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
            assert!(require_private_path(&loop_path).is_err(), "canonicalize errors must never choose a private fallback");
        }
    }

    #[cfg(all(feature = "native-runtime", unix))]
    #[test]
    #[ignore = "subprocess helper"]
    fn private_cwd_marker_io_failure_child() {
        let candidate = std::env::var_os("NAROU_PRIVATE_PATH_PROBE").expect("probe path");
        let error = require_private_path(std::path::Path::new(&candidate)).unwrap_err();
        assert!(matches!(error, NarouError::Io(_)), "marker I/O must not become a private fallback: {error}");
    }

    #[cfg(all(feature = "native-runtime", unix))]
    #[test]
    fn cwd_marker_io_failure_rejects_unrelated_private_snapshots() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-libraries");
        std::fs::create_dir_all(&base).unwrap();
        let temp = tempfile::Builder::new().prefix("private-marker-").tempdir_in(base).unwrap();
        let cwd = temp.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let marker = cwd.join(".narou");
        std::os::unix::fs::symlink(&marker, &marker).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "application::novel_access::tests::private_cwd_marker_io_failure_child", "--ignored", "--nocapture"])
            .current_dir(&cwd).env("NAROU_PRIVATE_PATH_PROBE", temp.path().join("private.txt"))
            .output().unwrap();
        assert!(result.status.success(), "private marker probe failed: {}{}",
            String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    }

    struct TestPrefixes(Vec<ObjectPrefix>);
    impl NovelPrefixResolver for TestPrefixes {
        fn novel_prefix<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectPrefix>>> {
            Box::pin(async move {
                Ok(self.0.iter().filter(|prefix| prefix.matches(key))
                    .max_by_key(|prefix| prefix.as_ref().trim_end_matches('/').len()).cloned())
            })
        }
    }

    #[test]
    fn migration_reference_published_after_listing_is_not_missing_or_overwritten() {
        struct ReferencedAfterOpen {
            local: Arc<MemoryObjectStore>,
            referenced: std::sync::atomic::AtomicBool,
        }
        impl ObjectStore for ReferencedAfterOpen {
            fn access_fence(&self) -> Option<&dyn NovelAccessFence> { Some(self.local.as_ref()) }
            fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
                ObjectStore::stat(self.local.as_ref(), key)
            }
            fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
                ObjectStore::list_page(self.local.as_ref(), request)
            }
            fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
                ObjectStore::read_small(self.local.as_ref(), key)
            }
            fn write_small<'a>(&'a self, key: &'a ObjectKey, bytes: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
                ObjectStore::write_small(self.local.as_ref(), key, bytes)
            }
            fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
                ObjectStore::delete(self.local.as_ref(), key)
            }
            fn is_remote_reference<'a>(&'a self, _: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
                Box::pin(async move { Ok(self.referenced.load(Ordering::SeqCst)) })
            }
            fn read_stream_leased<'a>(&'a self, _: &'a ObjectPrefix, _: &'a GateLease,
                _: &'a ObjectKey, _: Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
                Box::pin(async move {
                    self.referenced.store(true, Ordering::SeqCst);
                    Ok(None)
                })
            }
        }
        futures::executor::block_on(async {
            let local = Arc::new(MemoryObjectStore::new());
            let target = Arc::new(MemoryObjectStore::new());
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/挿絵/a.png").unwrap();
            local.write_small(&key, b"stale local mirror".to_vec()).await.unwrap();
            target.write_small(&key, b"new canonical remote bytes".to_vec()).await.unwrap();
            let objects: Arc<dyn ObjectStore> = Arc::new(ReferencedAfterOpen {
                local: local.clone(), referenced: std::sync::atomic::AtomicBool::new(false),
            });
            let assets: Arc<dyn AssetStore> = local.clone();
            let origins = NovelMigrationOrigins::new(objects.clone(), Arc::new(TestPrefixes(vec![prefix.clone()]))).unwrap();
            let reads = NovelReadScope::new(&objects, "reference-published-after-list");
            let facet = Arc::new(NovelMigrationStore::new(objects, assets, origins, reads.clone()).unwrap());
            let from_objects: Arc<dyn ObjectStore> = facet.clone();
            let from_assets: Arc<dyn AssetStore> = facet;
            let to_objects: Arc<dyn ObjectStore> = target.clone();
            let to_assets: Arc<dyn AssetStore> = target.clone();
            let mut state = crate::platform::store_migration::StoreMigrationState::default();
            crate::platform::store_migration::migrate_page(
                &from_objects, &from_assets, &to_assets, &to_objects, "copy", 10, &mut state,
            ).await.unwrap();
            assert!(state.failed.is_empty(), "a canonical remote reference is not missing local data: {:?}", state.failed);
            assert_eq!((state.copied, state.skipped), (0, 1));
            assert_eq!(target.read_small(&key).await.unwrap().unwrap(), b"new canonical remote bytes");
            local.close_gate(&prefix, "replacement").await.unwrap();
            assert!(ObjectStore::is_remote_reference(from_objects.as_ref(), &key).await.is_err(), "the marker probe must retain the original admission too");
            drop(from_objects); drop(from_assets);
            reads.release_all().await.unwrap();
        });
    }

    #[test]
    fn migration_source_and_destination_share_the_original_epoch() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let assets: Arc<dyn AssetStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/挿絵/a.png").unwrap();
            memory.write_small(&key, b"old migration source".to_vec()).await.unwrap();
            let origins = NovelMigrationOrigins::new(objects.clone(), Arc::new(TestPrefixes(vec![prefix.clone()]))).unwrap();
            let source_reads = NovelReadScope::new(&objects, "migration-source-test");
            let target_reads = NovelReadScope::new(&objects, "migration-target-test");
            let source = NovelMigrationStore::new(objects.clone(), assets.clone(), origins.clone(), source_reads.clone()).unwrap();
            let target = NovelMigrationStore::new(objects.clone(), assets, origins, target_reads.clone()).unwrap();
            let old = ObjectStore::read_small(&source, &key).await.unwrap().unwrap();
            assert_eq!(old, b"old migration source");
            let epoch = memory.close_gate(&prefix, "replacement").await.unwrap();
            memory.open_gate(&prefix, "replacement", epoch).await.unwrap();
            let replacement = memory.capture_writer(&prefix).await.unwrap();
            memory.write_small_guarded(&replacement, &key, b"new canonical reference bytes".to_vec()).await.unwrap();
            assert!(ObjectStore::write_small(&target, &key, old.clone()).await.is_err());
            assert!(AssetStore::write_stream(&target, &key, crate::platform::staging::owned_bytes_stream(old)).await.is_err());
            assert!(ObjectStore::delete(&target, &key).await.is_err());
            assert!(ObjectStore::read_small(&source, &key).await.is_err());
            assert_eq!(memory.read_small(&key).await.unwrap().unwrap(), b"new canonical reference bytes");
            drop(source); drop(target);
            source_reads.release_all().await.unwrap();
            target_reads.release_all().await.unwrap();
        });
    }

    #[test]
    fn migration_copy_pins_destination_before_awaiting_source_resolution() {
        use std::sync::atomic::AtomicBool;
        struct ReplaceWhenSourceOpens {
            memory: Arc<MemoryObjectStore>, source_prefix: ObjectPrefix,
            target_prefix: ObjectPrefix, target: ObjectKey, replaced: AtomicBool,
        }
        impl NovelPrefixResolver for ReplaceWhenSourceOpens {
            fn novel_prefix<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectPrefix>>> {
                Box::pin(async move {
                    if self.source_prefix.matches(key) {
                        if !self.replaced.swap(true, Ordering::SeqCst) {
                            let epoch = self.memory.close_gate(&self.target_prefix, "replacement").await?;
                            self.memory.open_gate(&self.target_prefix, "replacement", epoch).await?;
                            let fresh = self.memory.capture_writer(&self.target_prefix).await?;
                            self.memory.write_small_guarded(&fresh, &self.target, b"new destination".to_vec()).await?;
                        }
                        Ok(Some(self.source_prefix.clone()))
                    } else if self.target_prefix.matches(key) { Ok(Some(self.target_prefix.clone())) }
                    else { Ok(None) }
                })
            }
        }
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let assets: Arc<dyn AssetStore> = memory.clone();
            let source_prefix = ObjectPrefix::new("novels/fixture/source").unwrap();
            let target_prefix = ObjectPrefix::new("novels/fixture/target").unwrap();
            let source = ObjectKey::try_new("novels/fixture/source/挿絵/a.png").unwrap();
            let target = ObjectKey::try_new("novels/fixture/target/挿絵/a.png").unwrap();
            memory.write_small(&source, b"old source".to_vec()).await.unwrap();
            let resolver = Arc::new(ReplaceWhenSourceOpens {
                memory: memory.clone(), source_prefix, target_prefix, target: target.clone(), replaced: AtomicBool::new(false),
            });
            let origins = NovelMigrationOrigins::new(objects.clone(), resolver.clone()).unwrap();
            let reads = NovelReadScope::new(&objects, "migration-copy-test");
            let facet = NovelMigrationStore::new(objects, assets, origins, reads.clone()).unwrap();
            assert!(AssetStore::copy(&facet, &source, &target).await.is_err());
            assert!(resolver.replaced.load(Ordering::SeqCst));
            assert_eq!(memory.read_small(&target).await.unwrap().unwrap(), b"new destination");
            assert_eq!(memory.read_small(&source).await.unwrap().unwrap(), b"old source");
            drop(facet);
            reads.release_all().await.unwrap();
        });
    }

    #[test]
    fn migration_missing_binding_never_falls_back_to_raw_novel_bytes() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let assets: Arc<dyn AssetStore> = memory.clone();
            let key = ObjectKey::try_new("novels/fixture/book/挿絵/a.png").unwrap();
            memory.write_small(&key, b"protected bytes".to_vec()).await.unwrap();
            let origins = NovelMigrationOrigins::new(objects.clone(), Arc::new(TestPrefixes(Vec::new()))).unwrap();
            let reads = NovelReadScope::new(&objects, "unbound-migration-test");
            let facet = NovelMigrationStore::new(objects, assets, origins, reads.clone()).unwrap();
            assert!(ObjectStore::read_small(&facet, &key).await.is_err());
            assert!(ObjectStore::write_small(&facet, &key, b"unguarded replacement".to_vec()).await.is_err());
            assert!(ObjectStore::delete(&facet, &key).await.is_err());
            assert_eq!(memory.read_small(&key).await.unwrap().unwrap(), b"protected bytes");
            drop(facet);
            reads.release_all().await.unwrap();
        });
    }

    #[test]
    fn migration_scope_must_match_the_exact_logical_store_view() {
        let source: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
        let other = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = other.clone();
        let assets: Arc<dyn AssetStore> = other;
        let origins = NovelMigrationOrigins::new(objects.clone(), Arc::new(TestPrefixes(Vec::new()))).unwrap();
        let reads = NovelReadScope::new(&source, "wrong-view");
        assert!(NovelMigrationStore::new(objects, assets, origins, reads).is_err());
    }

    #[test]
    fn open_key_requires_prior_admission_and_retains_it_through_stream_eof() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/挿絵/a.png").unwrap();
            memory.write_small(&key, b"actual image bytes".to_vec()).await.unwrap();
            let mut scope = NovelReadScope::new(&objects, "epub-images");
            assert!(scope.open_key(&key, 0..u64::MAX).await.is_err());
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
            scope.lease_for(&prefix).await.unwrap().release().await.unwrap();
            let mut body = scope.open_key(&key, 0..u64::MAX).await.unwrap().unwrap();
            scope.release_all().await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            let mut actual = Vec::new();
            while let Some(chunk) = body.next().await { actual.extend(chunk.unwrap()); }
            assert_eq!(actual, b"actual image bytes");
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
        });
    }

    #[test]
    fn open_key_uses_longest_old_admission_not_a_shorter_open_prefix() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let parent = ObjectPrefix::new("novels/fixture").unwrap();
            let child = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/挿絵/a.png").unwrap();
            memory.write_small(&key, b"old image".to_vec()).await.unwrap();
            let mut scope = NovelReadScope::new(&objects, "virtual-member-images");
            scope.lease_for(&parent).await.unwrap().release().await.unwrap();
            scope.lease_for(&child).await.unwrap().release().await.unwrap();
            let epoch = memory.close_gate(&child, "replacement").await.unwrap();
            memory.open_gate(&child, "replacement", epoch).await.unwrap();
            let fresh = memory.capture_writer(&child).await.unwrap();
            memory.write_small_guarded(&fresh, &key, b"replacement image".to_vec()).await.unwrap();
            assert!(scope.open_key(&key, 0..u64::MAX).await.is_err());
            assert_eq!(memory.read_small(&key).await.unwrap().unwrap(), b"replacement image");
            scope.release_all().await.unwrap();
        });
    }

    #[test]
    fn migration_new_key_resolves_its_child_binding_not_a_cached_broader_gate() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let parent = ObjectPrefix::new("novels/fixture").unwrap();
            let child = ObjectPrefix::new("novels/fixture/book").unwrap();
            let parent_key = ObjectKey::try_new("novels/fixture/挿絵/parent.png").unwrap();
            let child_key = ObjectKey::try_new("novels/fixture/book/挿絵/child.png").unwrap();
            let origins = NovelMigrationOrigins::new(objects, Arc::new(TestPrefixes(vec![parent, child.clone()]))).unwrap();
            let parent_writer = origins.writer(&parent_key).await.unwrap().unwrap();
            parent_writer.write_small(&parent_key, b"parent bytes".to_vec()).await.unwrap();
            let child_writer = origins.writer(&child_key).await.unwrap().unwrap();
            child_writer.write_small(&child_key, b"old child".to_vec()).await.unwrap();
            let epoch = memory.close_gate(&child, "replacement").await.unwrap();
            memory.open_gate(&child, "replacement", epoch).await.unwrap();
            let fresh = memory.capture_writer(&child).await.unwrap();
            memory.write_small_guarded(&fresh, &child_key, b"new child".to_vec()).await.unwrap();
            assert!(child_writer.write_small(&child_key, b"late child".to_vec()).await.is_err());
            assert_eq!(memory.read_small(&child_key).await.unwrap().unwrap(), b"new child");
            parent_writer.write_small(&parent_key, b"unrelated parent still usable".to_vec()).await.unwrap();
        });
    }

    #[test]
    fn restored_origin_commits_metadata_only_while_its_original_epoch_remains_open() {
        use crate::platform::{NovelMutation, NovelRepository, mocks::MemoryNovelRepository};
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let record: crate::db::NovelRecord = serde_yaml::from_str("id: 1\nauthor: Author\ntitle: Original\nfile_title: book\ntoc_url: https://fixture.test/book/\nsitename: fixture\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
            let repository = MemoryNovelRepository::from_records(vec![record.clone()]);
            let keys = NovelObjectKeys::new("fixture", "book", false).unwrap();
            let original = NovelWriteGuard::capture_for(&objects, None, &keys).await.unwrap().origin();
            let resumed = NovelWriteGuard::resume(objects.clone(), None, &original).await.unwrap();
            let mut continued = record.clone();
            continued.title = "Resumed metadata".into();
            repository.apply_batch_guarded(resumed.fence(), vec![NovelMutation::Upsert(continued)]).await.unwrap();
            assert_eq!(repository.get(1.into()).await.unwrap().unwrap().title, "Resumed metadata");
            let prefix = ObjectPrefix::from(keys.prefix().clone());
            let epoch = memory.close_gate(&prefix, "replacement").await.unwrap();
            memory.open_gate(&prefix, "replacement", epoch).await.unwrap();
            let fresh = memory.capture_writer(&prefix).await.unwrap();
            let mut replacement = record.clone();
            replacement.title = "Published replacement".into();
            repository.apply_batch_guarded(&fresh, vec![NovelMutation::Upsert(replacement)]).await.unwrap();
            assert!(NovelWriteGuard::resume(objects, None, &original).await.is_err());
            assert!(repository.apply_batch_guarded(resumed.fence(), vec![NovelMutation::Upsert(record)]).await.is_err());
            assert_eq!(repository.get(1.into()).await.unwrap().unwrap().title, "Published replacement");
        });
    }

    #[test]
    fn independent_nonces_do_not_release_each_others_reader_row() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/novel.txt").unwrap();
            memory.write_small(&key, b"reader two remains admitted".to_vec()).await.unwrap();
            let first_id = reader_id_for_nonce("same-epub", [0x11; 16]);
            let second_id = reader_id_for_nonce("same-epub", [0x22; 16]);
            assert_ne!(first_id, second_id);
            let first = memory.admit_reader(&prefix, &first_id, READER_LEASE_SECS).await.unwrap();
            let second = memory.admit_reader(&prefix, &second_id, READER_LEASE_SECS).await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 2);
            memory.release_reader(&prefix, &first).await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            let mut body = memory.read_stream_leased(&prefix, &second, &key, 0..u64::MAX).await.unwrap().unwrap();
            let mut actual = Vec::new();
            while let Some(chunk) = body.next().await { actual.extend(chunk.unwrap()); }
            assert_eq!(actual, b"reader two remains admitted");
            memory.release_reader(&prefix, &second).await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
        });
    }

    /// Test-only controllable clock for lease-TTL regressions.
    struct TestClock(std::sync::atomic::AtomicI64);

    impl crate::platform::Clock for TestClock {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from_timestamp(self.0.load(std::sync::atomic::Ordering::SeqCst), 0).unwrap()
        }
        fn advance(&self, duration: std::time::Duration) {
            self.0.fetch_add(duration.as_secs() as i64, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl TestClock {
        fn set(&self, secs: i64) {
            self.0.store(secs, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The production failure mode: a >30s external-I/O gap lets the durable
    /// lease lapse while the gate stays open at the same epoch. `ensure_read`
    /// must re-admit under the same epoch so the download can continue —
    /// without crossing a gate boundary (never silently upgraded to a newer
    /// epoch after a close/reopen).
    #[test]
    fn ensure_read_readmits_after_a_long_idle_gap_at_the_same_epoch() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/novel.txt").unwrap();
            let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(chrono::Utc::now().timestamp())));
            let write = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            write.write_small(&key, b"body".to_vec()).await.unwrap();
            let epoch = write.epoch();
            let mut access = NovelAccess::from_writer_with_clock(write, "dl", clock.clone()).await.unwrap();
            let original_reader = access.read().unwrap().lease.reader_id.clone();

            // 1st section write, then a >30s "external HTTP" gap.
            clock.advance(std::time::Duration::from_secs(READER_LEASE_SECS as u64 + 1));
            assert!(access.read().unwrap().expired().await);

            // Same epoch → re-admit succeeds; a fresh lease row now exists.
            access.ensure_read().await.unwrap();
            let read = access.read().unwrap();
            assert_eq!(read.epoch(), epoch);
            assert_ne!(read.lease.reader_id, original_reader, "the lapsed token must be replaced, not renewed");
            // The fake clock still sits +31s ahead; the fresh store-side token
            // was stamped with real time, so bring the test clock back before
            // checking `expired` (production uses one clock throughout).
            clock.set(chrono::Utc::now().timestamp());
            assert!(!read.expired().await);
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1);
            assert_eq!(read.read_small(&key).await.unwrap().unwrap(), b"body");
        });
    }

    #[test]
    fn ensure_read_fails_closed_while_the_gate_is_closed() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(chrono::Utc::now().timestamp())));
            let write = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            let mut access = NovelAccess::from_writer_with_clock(write, "dl", clock.clone()).await.unwrap();
            let original_reader = access.read().unwrap().lease.reader_id.clone();
            clock.advance(std::time::Duration::from_secs(READER_LEASE_SECS as u64 + 1));

            let closed_epoch = memory.close_gate(&prefix, "transfer").await.unwrap();
            let error = match access.ensure_read().await {
                Ok(_) => panic!("re-admission behind a closed gate must fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("novel_recovering"), "unexpected: {error}");
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1, "no new lease may be granted behind a closed gate");
            memory.open_gate(&prefix, "transfer", closed_epoch).await.unwrap();

            // Gate closed → reopened (epoch advanced) while the lease was out.
            let error = match access.ensure_read().await {
                Ok(_) => panic!("re-admission into an advanced epoch must fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("novel_busy"), "unexpected: {error}");
            let info = memory.gate_info(&prefix).await.unwrap();
            assert_eq!(info.pending_readers, 1, "only the stale row may remain; the new-epoch token is released");
            assert_eq!(access.read().unwrap().lease.reader_id, original_reader);
        });
    }

    /// A pure reader (`read_only`, no writer) must never re-admit: the shared
    /// lease stays dead and reads keep failing at the gate boundary, exactly
    /// as before the `ensure_read` machinery existed.
    #[test]
    fn read_only_never_readmits_and_stays_fail_closed() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let key = ObjectKey::try_new("novels/fixture/book/novel.txt").unwrap();
            let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(chrono::Utc::now().timestamp())));
            memory.write_small(&key, b"body".to_vec()).await.unwrap();

            let read = NovelReadLease::admit_with_clock(&objects, clock.clone(), &prefix, "epub").await.unwrap();
            let original_reader = read.lease.reader_id.clone();
            let mut access = NovelAccess::read_only(read);
            clock.advance(std::time::Duration::from_secs(READER_LEASE_SECS as u64 + 1));

            // `ensure_read` must NOT re-admit a read-only access.
            assert_eq!(access.ensure_read().await.unwrap().lease.reader_id, original_reader);
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 1, "no new admission may appear");

            // The dead lease still fails at the first gated read (fail-closed).
            let epoch = memory.close_gate(&prefix, "transfer").await.unwrap();
            assert!(access.ensure_read().await.unwrap().read_small(&key).await.is_err());
            memory.open_gate(&prefix, "transfer", epoch).await.unwrap();
            let error = access.ensure_read().await.unwrap().read_small(&key).await.unwrap_err();
            assert!(error.to_string().contains("novel_recovering"), "unexpected: {error}");
            assert_eq!(access.read().unwrap().lease.reader_id, original_reader);
        });
    }

    /// `release_read` drops the idle lease so a long external wait does not
    /// hold a reader slot; the next `ensure_read` re-admits at the same epoch.
    #[test]
    fn release_read_then_ensure_read_readmits_at_the_same_epoch() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let objects: Arc<dyn ObjectStore> = memory.clone();
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(chrono::Utc::now().timestamp())));
            let write = NovelWriteGuard::capture(&objects, None, &prefix).await.unwrap();
            let epoch = write.epoch();
            let mut access = NovelAccess::from_writer_with_clock(write, "dl", clock.clone()).await.unwrap();
            access.release_read().await.unwrap();
            assert_eq!(memory.gate_info(&prefix).await.unwrap().pending_readers, 0);
            access.ensure_read().await.unwrap();
            assert_eq!(access.read().unwrap().epoch(), epoch);
        });
    }

    /// `renew_reader` error classification: a missing/expired admission is a
    /// dead lease (`novel_lease_expired`), not a gate boundary problem; a
    /// token minted before a close/reopen is `novel_recovering`.
    #[test]
    fn renew_reader_distinguishes_expired_from_gate_conflicts() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();

            // Missing row (released before renew raced in) → expired, not closed.
            let live = memory.admit_reader(&prefix, "r-live", READER_LEASE_SECS).await.unwrap();
            memory.release_reader(&prefix, &live).await.unwrap();
            let error = memory.renew_reader(&prefix, &live, READER_LEASE_SECS).await.unwrap_err();
            assert!(error.to_string().contains("novel_lease_expired"), "unexpected: {error}");

            // A token minted before a close/reopen is a gate boundary
            // problem: renewing it now reports `novel_recovering` on every
            // backend.
            let stale = memory.admit_reader(&prefix, "r-stale", READER_LEASE_SECS).await.unwrap();
            let epoch = memory.close_gate(&prefix, "transfer").await.unwrap();
            memory.open_gate(&prefix, "transfer", epoch).await.unwrap();
            memory.admit_reader(&prefix, "r-stale", READER_LEASE_SECS).await.unwrap();
            let error = memory.renew_reader(&prefix, &stale, READER_LEASE_SECS).await.unwrap_err();
            assert!(error.to_string().contains("novel_recovering"), "unexpected: {error}");

            // A live row at the current epoch still renews (control).
            let mutated = memory.admit_reader(&prefix, "r-epoch", READER_LEASE_SECS).await.unwrap();
            let renewed = memory.renew_reader(&prefix, &mutated, READER_LEASE_SECS).await.unwrap();
            assert_eq!(renewed.epoch, 1);
        });
    }

    /// A forged stale token (expired `expires_at`, no matching live row) must
    /// report `novel_lease_expired`, never silently resurrect.
    #[test]
    fn renew_reader_with_a_stale_expired_token_reports_lease_expired() {
        futures::executor::block_on(async {
            let memory = Arc::new(MemoryObjectStore::new());
            let prefix = ObjectPrefix::new("novels/fixture/book").unwrap();
            let stale = crate::platform::access_fence::GateLease {
                reader_id: "ghost".into(), epoch: 0, expires_at: 1,
            };
            let error = memory.renew_reader(&prefix, &stale, READER_LEASE_SECS).await.unwrap_err();
            assert!(error.to_string().contains("novel_lease_expired"), "unexpected: {error}");
        });
    }
}
