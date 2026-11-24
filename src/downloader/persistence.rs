use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};
use crate::application::novel_access::{NovelReadLease, NovelWriteGuard};
use crate::platform::{
    Clock, NovelObjectKeys, ObjectKey, ObjectListRequest, ObjectPrefix, ObjectStore, SystemClock,
    ObjectFingerprint, ObjectFingerprintMismatch,
};

use super::types::{SectionElement, SectionFile, SubtitleInfo, TocFile};

/// 本文セクションを先読みする件数。
///
/// Native は最大 4 件を並行に読む。Worker はデコード済みの本文を
/// 複数保持しないよう 1 件に固定し、消費側の pull を次の読み取り境界にする。
pub const SECTION_READ_WINDOW: usize = if cfg!(target_arch = "wasm32") { 1 } else { 4 };

/// An owned, ordered section source. Only the section currently requested by
/// the consumer is decoded; dropping it cancels the remaining reads.
#[cfg(not(target_arch = "wasm32"))]
pub type SectionStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<SectionFile>> + Send + 'static>>;
#[cfg(target_arch = "wasm32")]
pub type SectionStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<SectionFile>> + 'static>>;

// No .yaml suffix: this is transient metadata, never a section/history object.
const DOWNLOAD_RESUME_FILE: &str = ".download_resume";

/// [`PersistenceService::stream_sections_ordered`] の失敗。
///
/// 呼び出し側が文脈に合うエラー文言へ変換する (`convert` と resume で
/// 「欠落」の扱いが異なるため、ここではどちらなのかだけを伝える)。
#[derive(Debug)]
pub enum SectionLoadFailure {
    /// 目次にある本文オブジェクトが無い。`section_index` は渡したスライス内の位置。
    Missing { section_index: usize },
    /// 読み込み/デコードの失敗 (走査順で最初のもの)。
    Read(NarouError),
}

pub fn compute_section_hash(section: &SectionElement) -> String {
    let mut hasher = Sha256::new();
    hasher.update(section.body.as_bytes());
    hasher.update(section.introduction.as_bytes());
    hasher.update(section.postscript.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn section_filename(subtitle: &SubtitleInfo) -> String {
    let safe_subtitle =
        crate::downloader::util::sanitize_filename_with_limit(&subtitle.file_subtitle, None);
    format!("{} {}.yaml", subtitle.index, safe_subtitle)
}

pub fn raw_filename(subtitle: &SubtitleInfo) -> String {
    let section = section_filename(subtitle);
    let stem = section.strip_suffix(".yaml").unwrap_or(&section);
    format!("{stem}.html")
}

/// Ruby 版由来のサニタイズ前キー (例: 末尾空白付き) や v0.6.10 以前の
/// 80 文字切り詰めキーへのフォールバック読み。正規キーが無い場合にだけ
/// 差分のある旧キー候補を順に試す。通常ケースでは追加の読み出しを行わない。
/// `(bytes, 実際に読めたキー)` を返す。
async fn read_section_bytes(
    lease: &mut NovelReadLease,
    keys: &NovelObjectKeys,
    subtitle: &SubtitleInfo,
) -> Result<Option<(Vec<u8>, ObjectKey)>> {
    let key = keys.section(&subtitle.index, &subtitle.file_subtitle);
    if let Some(bytes) = lease.read_small(&key).await? {
        return Ok(Some((bytes, key)));
    }
    for fallback in keys.legacy_section_fallbacks(&subtitle.index, &subtitle.file_subtitle) {
        if let Some(bytes) = lease.read_small(&fallback).await? {
            return Ok(Some((bytes, fallback)));
        }
    }
    Ok(None)
}

/// Match the stored body to the generation declared by its TOC entry.
pub(crate) fn section_matches_generation(section: &SectionFile, subtitle: &SubtitleInfo) -> bool {
    // Ruby-era bodies can lack download_time even when their TOC has it.
    // This exception only applies to the unstamped legacy generation.
    let unstamped_legacy = section.download_revision == 0 && section.download_time.is_none();
    section.download_revision.max(1) == subtitle.download_revision.max(1)
        && (unstamped_legacy || subtitle.download_time.as_ref()
            .is_none_or(|time| section.download_time.as_ref() == Some(time)))
}

/// Storage-backed downloader persistence.
///
/// The codec functions below remain pure; this service owns the ObjectStore
/// and clock. Writes use the caller's original NovelWriteGuard.
#[derive(Clone)]
pub struct PersistenceService {
    objects: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
}

impl PersistenceService {
    pub fn new(objects: Arc<dyn ObjectStore>) -> Self {
        Self::with_clock(objects, Arc::new(SystemClock))
    }

    pub fn with_clock(
        objects: Arc<dyn ObjectStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            objects,
            clock,
        }
    }

    pub fn objects(&self) -> &Arc<dyn ObjectStore> {
        &self.objects
    }

    pub async fn load_toc(&self, lease: &mut NovelReadLease, keys: &NovelObjectKeys) -> Result<Option<TocFile>> {
        let Some(bytes) = lease.read_small(&keys.toc()).await? else {
            return Ok(None);
        };
        Ok(Some(deserialize_toc(&bytes)?))
    }

    pub async fn save_toc(&self, access: &NovelWriteGuard, keys: &NovelObjectKeys, toc: &TocFile) -> Result<()> {
        let data = serialize_toc(toc)?.into_bytes();
        // 更新確認では TOC はほぼ常に同一。同一バイト列への書き戻しは
        // atomic rename + fsync を毎小説支払うだけなので、内容が一致する
        // ときは書き込み自体を省く (issue #58)。
        if self.objects.read_small(&keys.toc()).await?.is_some_and(|existing| existing == data) {
            return Ok(());
        }
        access
            .write_small(&keys.toc(), data)
            .await
    }

    pub(crate) async fn load_download_resume(&self, lease: &mut NovelReadLease, keys: &NovelObjectKeys) -> Result<Option<TocFile>> {
        let key = keys.prefix().join(DOWNLOAD_RESUME_FILE)?;
        lease.read_small(&key).await?
            .map(|bytes| deserialize_toc(&bytes)).transpose()
    }

    pub(crate) async fn save_download_resume(&self, access: &NovelWriteGuard, keys: &NovelObjectKeys, toc: &TocFile) -> Result<()> {
        access.write_small(
            &keys.prefix().join(DOWNLOAD_RESUME_FILE)?,
            serialize_toc(toc)?.into_bytes(),
        ).await
    }

    pub(crate) async fn clear_download_resume(&self, access: &NovelWriteGuard, keys: &NovelObjectKeys) -> Result<()> {
        access.delete(&keys.prefix().join(DOWNLOAD_RESUME_FILE)?).await
    }

    pub async fn load_section(
        &self,
        lease: &mut NovelReadLease,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
    ) -> Result<Option<SectionFile>> {
        let Some((bytes, _)) = read_section_bytes(lease, keys, subtitle).await? else {
            return Ok(None);
        };
        Ok(Some(deserialize_section(&bytes)?))
    }

    /// Pin conversion to the supplied TOC generation without retaining bodies.
    ///
    /// Current downloads publish section revisions before the replacement TOC.
    /// Reject in-flight bodies rather than combining generations, then recheck
    /// their content fingerprints before EOF. Immutable stores can compare
    /// authoritative digests; mutable/legacy stores re-read their bytes. The
    /// consumer must not publish a partial result before successful EOF.
    pub fn stream_sections_snapshot(&self, lease: NovelReadLease, keys: NovelObjectKeys, toc: TocFile) -> SectionStream {
        let expected_toc = serialize_toc(&toc).map(|text| <[u8; 32]>::from(Sha256::digest(text.as_bytes())));
        Box::pin(futures::stream::try_unfold(
            (lease, keys, toc, expected_toc, 0usize, Vec::<ObjectFingerprint>::new()),
            |(mut lease, keys, toc, expected_toc, position, mut fingerprints)| async move {
                let result: Result<Option<SectionFile>> = async {
                    let expected = expected_toc.as_ref().map_err(|error| NarouError::Platform(error.to_string()))?;
                    if let Some(subtitle) = toc.subtitles.get(position) {
                        let (bytes, key) = read_section_bytes(&mut lease, &keys, subtitle).await?.ok_or_else(|| {
                            NarouError::Io(std::io::Error::new(std::io::ErrorKind::NotFound,
                                format!("section file not found: expected '{}' in {}/本文", section_filename(subtitle), keys.prefix())))
                        })?;
                        let section = deserialize_section(&bytes)?;
                        if !section_matches_generation(&section, subtitle) {
                            return Err(NarouError::Platform(format!("section generation changed during conversion: {key}")));
                        }
                        fingerprints.push(ObjectFingerprint { key, sha256: Sha256::digest(&bytes).into() });
                        return Ok(Some(section));
                    }
                    if let Some(mismatch) = lease.verify_fingerprints(&fingerprints).await? {
                        return Err(NarouError::Platform(match mismatch {
                            ObjectFingerprintMismatch::Missing(key) => format!("section disappeared during conversion: {key}"),
                            ObjectFingerprintMismatch::Changed(key) => format!("section changed during conversion: {key}"),
                        }));
                    }
                    let current = lease.read_small(&keys.toc()).await?.ok_or_else(|| {
                        NarouError::Platform(format!("toc disappeared during conversion: {}", keys.toc()))
                    })?;
                    if <[u8; 32]>::from(Sha256::digest(serialize_toc(&deserialize_toc(&current)?)?.as_bytes())) != *expected {
                        return Err(NarouError::Platform(format!("toc changed during conversion: {}", keys.toc())));
                    }
                    Ok(None)
                }.await;
                match result {
                    Ok(Some(section)) => Ok(Some((section, (lease, keys, toc, expected_toc, position + 1, fingerprints)))),
                    Ok(None) => { let _ = lease.release().await; Ok(None) }
                    Err(error) => { let _ = lease.release().await; Err(error) }
                }
            },
        ))
    }

    /// 本文セクションを最大 [`SECTION_READ_WINDOW`] 件の並行読みで取得し、
    /// 入力と同じ順で返す。
    ///
    /// 失敗の扱いは 1 件ずつ直列に [`Self::load_section`] を呼んだ場合と
    /// 同じで、走査順で最初の失敗 (`Missing` または `Read`) を返す。先に
    /// 読み終えた後続があっても、返る順序と「最初の失敗」の意味は変わらない。
    /// 最初の失敗より先 (窓の分だけ) を先読みしていた読み込みは破棄される。
    pub fn stream_sections_ordered<'a>(
        &'a self,
        lease: NovelReadLease,
        keys: &'a NovelObjectKeys,
        subtitles: &'a [SubtitleInfo],
    ) -> impl futures::Stream<Item = std::result::Result<SectionFile, SectionLoadFailure>> + 'a {
        let shared = lease.clone();
        let pending = futures::stream::iter((0..subtitles.len()).map(move |section_index| {
            let mut reader = shared.clone();
            async move {
                let read = self.load_section(&mut reader, keys, &subtitles[section_index]).await;
                let _ = reader.release().await;
                match read {
                    Ok(Some(section)) => Ok(section),
                    Ok(None) => Err(SectionLoadFailure::Missing { section_index }),
                    Err(error) => Err(SectionLoadFailure::Read(error)),
                }
            }
        })).buffered(SECTION_READ_WINDOW);
        futures::stream::try_unfold((pending, lease), |(mut pending, mut lease)| async move {
            if let Err(error) = lease.refresh().await {
                drop(pending);
                let _ = lease.release().await;
                return Err(SectionLoadFailure::Read(error));
            }
            match pending.next().await {
                Some(Ok(section)) => Ok(Some((section, (pending, lease)))),
                Some(Err(error)) => {
                    drop(pending);
                    let _ = lease.release().await;
                    Err(error)
                }
                None => {
                    drop(pending);
                    let _ = lease.release().await;
                    Ok(None)
                }
            }
        })
    }

    pub async fn save_section(
        &self,
        access: &NovelWriteGuard,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        section: &SectionElement,
        download_revision: u64,
    ) -> Result<String> {
        let download_time = self
            .clock
            .now_utc()
            .format("%Y-%m-%d %H:%M:%S%.6f %z")
            .to_string();
        let file_data = SectionFile {
            index: subtitle.index.clone(),
            href: subtitle.href.clone(),
            chapter: subtitle.chapter.clone(),
            subchapter: subtitle.subchapter.clone(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: subtitle.subupdate.clone(),
            download_time: Some(download_time.clone()),
            element: section.clone(),
            download_revision,
        };
        access
            .write_small(
                &keys.section(&subtitle.index, &subtitle.file_subtitle),
                serialize_section(&file_data)?.into_bytes(),
            )
            .await?;
        Ok(download_time)
    }

    pub async fn section_needs_update(
        &self,
        lease: &mut NovelReadLease,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        new_section: &SectionElement,
    ) -> Result<bool> {
        let Some(existing) = self.load_section(lease, keys, subtitle).await? else {
            return Ok(true);
        };
        Ok(compute_section_hash(&existing.element) != compute_section_hash(new_section))
    }

    /// List every object name under the novel prefix in one listing pass.
    ///
    /// One `list_page` round trip (or one bounded directory walk on native)
    /// replaces a per-section `read_small`/`stat` probe. Callers that only
    /// need existence should prefer this over N individual reads.
    pub async fn list_novel_object_names(&self, keys: &NovelObjectKeys) -> Result<HashSet<String>> {
        let prefix = ObjectPrefix::from(keys.prefix().clone());
        let mut names = HashSet::new();
        let mut request = ObjectListRequest::new(prefix, NonZeroUsize::new(1000).unwrap());
        loop {
            let page = self.objects.list_page(&request).await?;
            for object in &page.objects {
                if let Some(name) = object
                    .key
                    .as_ref()
                    .strip_prefix(keys.prefix().as_ref())
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    names.insert(name.to_string());
                }
            }
            match page.next_cursor {
                Some(cursor) => request = request.after(cursor),
                None => break,
            }
        }
        Ok(names)
    }

    /// Section indices that already have a persisted `本文/` file.
    ///
    /// Matches both the canonical `<index> <file_subtitle>.yaml` name and the
    /// legacy `<index>.yaml` / `<index> <old subtitle>.yaml` fallback that
    /// `resolve_read_path` accepts on native.
    pub fn existing_section_indices(names: &HashSet<String>) -> HashSet<String> {
        names
            .iter()
            .filter_map(|name| {
                let file = name.strip_prefix("本文/")?;
                if file.starts_with('.') || !file.ends_with(".yaml") {
                    return None;
                }
                let stem = file.strip_suffix(".yaml")?;
                let index = stem.split_once(' ').map(|(index, _)| index).unwrap_or(stem);
                if index.is_empty() {
                    None
                } else {
                    Some(index.to_string())
                }
            })
            .collect()
    }

    pub async fn save_raw(
        &self,
        access: &NovelWriteGuard,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        raw_html: &str,
    ) -> Result<()> {
        if raw_html.is_empty() {
            return Ok(());
        }
        access
            .write_small(
                &keys.raw_section(&subtitle.index, &subtitle.file_subtitle),
                raw_html.as_bytes().to_vec(),
            )
            .await
    }

    /// Cache the previous section. Portable stores retain the current key until
    /// its replacement commits; native keeps the legacy move behavior.
    /// Return `false` for a missing source, which is not a cached version.
    pub async fn cache_section(
        &self,
        access: &NovelWriteGuard,
        lease: &mut NovelReadLease,
        keys: &NovelObjectKeys,
        timestamp: &str,
        subtitle: &SubtitleInfo,
    ) -> Result<bool> {
        let source = keys.section(&subtitle.index, &subtitle.file_subtitle);
        let Some(stream) = lease.open_all(&source).await? else { return Ok(false); };
        let destination = keys.cached_section(timestamp, &subtitle.index, &subtitle.file_subtitle);
        access.write_stream(&destination, stream).await?;
        #[cfg(feature = "native-runtime")]
        access.delete(&source).await?;
        Ok(true)
    }

    /// Record a persisted version after its first successfully cached section, or
    /// explicitly record an empty version after a completed latest-only update.
    pub async fn record_cache_version(
        &self,
        access: &NovelWriteGuard,
        keys: &NovelObjectKeys,
        timestamp: &str,
    ) -> Result<()> {
        access.write_small(&keys.cached_version_marker(timestamp), Vec::new()).await
    }

    /// Write `setting.ini` / `replace.txt` defaults for files that do not yet
    /// exist. When `existing_names` is provided (from
    /// [`Self::list_novel_object_names`]) existence is decided from the set
    /// with no extra storage round trips; otherwise each file is probed with
    /// `stat`.
    pub async fn ensure_default_files(
        &self,
        access: &NovelWriteGuard,
        keys: &NovelObjectKeys,
        title: &str,
        author: &str,
        toc_url: &str,
        existing_names: Option<&HashSet<String>>,
    ) -> Result<()> {
        let setting_exists = match existing_names {
            Some(names) => names.contains("setting.ini"),
            None => self.objects.stat(&keys.setting()).await?.is_some(),
        };
        if !setting_exists {
            let content = default_setting_ini();
            let mut mutate = |current: Option<&[u8]>| {
                Ok(current.is_none().then_some(std::borrow::Cow::Borrowed(content.as_bytes())))
            };
            match access.update_small(&keys.setting(), &mut mutate).await? {
                crate::platform::ObjectUpdateResult::Committed => {}
                crate::platform::ObjectUpdateResult::ProjectionFailed(error) => return Err(error),
                crate::platform::ObjectUpdateResult::Conflict => {
                    return Err(crate::error::NarouError::Platform("setting.ini initialization conflicted".into()));
                }
            }
        }
        let replace_exists = match existing_names {
            Some(names) => names.contains("replace.txt"),
            None => self.objects.stat(&keys.replace()).await?.is_some(),
        };
        if !replace_exists {
            let content = default_replace_txt(title, author, toc_url);
            let mut mutate = |current: Option<&[u8]>| {
                Ok(current.is_none().then_some(std::borrow::Cow::Borrowed(content.as_bytes())))
            };
            match access.update_small(&keys.replace(), &mut mutate).await? {
                crate::platform::ObjectUpdateResult::Committed => {}
                crate::platform::ObjectUpdateResult::ProjectionFailed(error) => return Err(error),
                crate::platform::ObjectUpdateResult::Conflict => {
                    return Err(crate::error::NarouError::Platform("replace.txt initialization conflicted".into()));
                }
            }
        }
        Ok(())
    }

    /// `<prefix>/挿絵` に実際に置かれているファイル名。
    ///
    /// 索引 (`.illustration_cache.yaml`) はメタデータなので実体の存在を保証
    /// しない (ストアの切替・手動削除・別環境での取得)。索引だけを信じると
    /// 「索引にあるのに実体が無い」挿絵を永久に取り逃すため、保存先を直接見る。
    /// 上限ページ数を超えた場合と一覧に失敗した場合は `None` を返し、呼び出し
    /// 側は索引だけで判断する従来の挙動へ戻る。
    pub async fn list_illustration_names(
        &self,
        keys: &NovelObjectKeys,
        max_pages: usize,
    ) -> Option<HashSet<String>> {
        let prefix = ObjectPrefix::new(format!("{}/挿絵", keys.prefix().as_ref())).ok()?;
        let mut names = HashSet::new();
        let mut cursor = None;
        for _ in 0..max_pages {
            let mut request =
                ObjectListRequest::new(prefix.clone(), NonZeroUsize::new(1000).unwrap());
            request.cursor = cursor.take();
            match self.objects.list_page(&request).await {
                Ok(page) => {
                    for metadata in page.objects {
                        if let Some(name) = metadata.key.as_ref().rsplit('/').next() {
                            names.insert(name.to_string());
                        }
                    }
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => return Some(names),
                    }
                }
                Err(error) => {
                    crate::application::debug::emit_error_async(format!(
                        "挿絵: 保存済みの一覧を取れません ({error})"
                    )).await;
                    return None;
                }
            }
        }
        None
    }

    pub async fn load_cache(
        &self,
        lease: &mut NovelReadLease,
        keys: &NovelObjectKeys,
    ) -> Result<Option<crate::illustration_store::IllustrationIndex>> {
        let Some(bytes) = lease.read_small(&keys.illustration_cache()).await? else {
            return Ok(None);
        };
        Ok(Some(serde_yaml::from_slice(&bytes)?))
    }

    pub async fn save_cache(
        &self,
        access: &NovelWriteGuard,
        keys: &NovelObjectKeys,
        index: &crate::illustration_store::IllustrationIndex,
    ) -> Result<()> {
        let body = index.to_cache_yaml()?;
        access
            .write_small(&keys.illustration_cache(), body)
            .await
    }
}

pub fn default_setting_ini() -> String {
    crate::converter::ini::IniData::new().to_ini_string()
}

pub fn default_replace_txt(title: &str, author: &str, toc_url: &str) -> String {
    format!(
        "; 単純置換用ファイル\n;\n; 対象小説情報\n; タイトル: {}\n; 作者: {}\n; URL: {}\n;\n; 書式\n; 置換対象<tab>置換文字\n;\n; サンプル\n; 一〇歳\t十歳\n; 第一章\t［＃ゴシック体］第一章［＃ゴシック体終わり］\n;\n; 正規表現での置換などは converter.yaml で対応して下さい\n",
        title, author, toc_url
    )
}

pub fn serialize_section(section: &SectionFile) -> Result<String> {
    let yaml_body = serde_yaml::to_string(section)?;
    Ok(format!("---\n{}\n", yaml_body))
}

pub fn deserialize_section(bytes: &[u8]) -> Result<SectionFile> {
    Ok(serde_yaml::from_slice(bytes)?)
}

pub fn serialize_toc(toc: &TocFile) -> Result<String> {
    let yaml_body = serde_yaml::to_string(toc)?;
    Ok(format!("---\n{}\n", fix_yaml_block_scalar(&yaml_body)))
}

pub fn deserialize_toc(bytes: &[u8]) -> Result<TocFile> {
    Ok(serde_yaml::from_slice(bytes)?)
}

/// Metadata needed by publication without allocating the TOC chapter vector.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TocMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub chapter_count: u64,
    pub novel_type: Option<u8>,
}

/// Consume the complete YAML stream, retaining only the root story scalar.
pub fn deserialize_toc_story(bytes: &[u8]) -> Result<Option<String>> {
    project_toc(bytes, true).map(|(story, _)| story)
}

/// Consume the complete YAML stream, counting subtitle mapping records in
/// place. Scalar/sequence aliases retain only bounded projections, not chapters.
pub fn deserialize_toc_metadata(bytes: &[u8]) -> Result<TocMetadata> {
    project_toc(bytes, false).map(|(_, metadata)| metadata)
}

fn project_toc(bytes: &[u8], want_story: bool) -> Result<(Option<String>, TocMetadata)> {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use saphyr_parser::{Event, Parser, ScalarStyle};

    #[derive(Clone)]
    enum Anchor<'a> {
        Scalar(Cow<'a, str>, ScalarStyle, u8),
        Mapping,
        Sequence { count: u64, mappings_only: bool },
        Pending,
    }
    struct Frame {
        mapping: bool,
        anchor: usize,
        count: u64,
        mappings_only: bool,
        subtitles: bool,
    }
    fn invalid(message: impl std::fmt::Display) -> NarouError {
        NarouError::Platform(format!("invalid toc.yaml projection: {message}"))
    }
    fn charge(retained: &mut usize, limit: usize, bytes: usize) -> Result<()> {
        *retained = retained.checked_add(bytes).ok_or_else(|| {
            NarouError::Platform("transfer_metadata_budget: TOC projection overflow".into())
        })?;
        if *retained > limit {
            return Err(NarouError::Platform("transfer_metadata_budget: TOC projection exceeds metadata budget".into()));
        }
        Ok(())
    }
    fn sequence_item(frames: &mut [Frame], mapping: bool) -> Result<()> {
        if let Some(frame) = frames.last_mut().filter(|frame| !frame.mapping) {
            frame.count = frame.count.checked_add(1).ok_or_else(|| invalid("subtitle count overflow"))?;
            frame.mappings_only &= mapping;
        }
        Ok(())
    }
    fn mark(seen: &mut [bool; 6], field: usize) -> Result<()> {
        if field != 0 && std::mem::replace(&mut seen[field], true) { return Err(invalid("duplicate projected field")); }
        Ok(())
    }

    if bytes.len() as u64 > crate::platform::SMALL_CAP {
        return Err(NarouError::Platform("transfer_metadata_budget: toc.yaml exceeds 16 MiB".into()));
    }
    let text = std::str::from_utf8(bytes).map_err(invalid)?;
    let mut parser = Parser::new_from_str(text);
    // Input + retained projections <=18 MiB, with 5 MiB pending append and
    // 1 MiB slack inside 24 MiB. One parser scalar is transient codec space.
    let retention_limit = (18 * 1024 * 1024usize).saturating_sub(bytes.len());
    let mut retained = 0usize;
    let mut anchors: BTreeMap<usize, Anchor<'_>> = BTreeMap::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut documents = 0usize;
    let mut root_seen = false;
    let mut root_done = false;
    let mut key_next = true;
    // 0 ignored, 1 story, 2 title, 3 author, 4 subtitles, 5 novel_type.
    let mut field = 0usize;
    let mut seen = [false; 6];
    let mut story = None;
    let mut metadata = TocMetadata::default();
    loop {
        let (event, _) = parser.next_event().ok_or_else(|| invalid("unexpected parser EOF"))?.map_err(invalid)?;
        let mapping_event = matches!(&event, Event::MappingStart(..) | Event::MappingEnd);
        let (value, style, tag_kind) = match event {
            Event::StreamStart | Event::Nothing => continue,
            Event::DocumentStart(_) => {
                documents += 1;
                if documents != 1 { return Err(invalid("multiple YAML documents")); }
                continue;
            }
            Event::DocumentEnd => {
                if !root_done || !frames.is_empty() { return Err(invalid("incomplete root mapping")); }
                continue;
            }
            Event::StreamEnd => {
                if documents != 1 || !root_done { return Err(invalid("root must be one mapping")); }
                return Ok((story, metadata));
            }
            Event::MappingStart(anchor, _) | Event::SequenceStart(anchor, _) => {
                let mut subtitles = false;
                if frames.is_empty() {
                    if root_seen || !mapping_event { return Err(invalid("root must be one mapping")); }
                    root_seen = true;
                } else if frames.len() == 1 {
                    if key_next { return Err(invalid("root mapping key must be a scalar")); }
                    mark(&mut seen, field)?;
                    if field != 0 && field != 4 { return Err(invalid("projected field must be a scalar")); }
                    if field == 4 {
                        if mapping_event { return Err(invalid("subtitles must be a sequence")); }
                        subtitles = true;
                    }
                    key_next = true;
                }
                if frames.len() >= 128 { return Err(invalid("metadata nesting exceeds limit")); }
                sequence_item(&mut frames, mapping_event)?;
                if anchor != 0 {
                    charge(&mut retained, retention_limit, 128)?;
                    anchors.insert(anchor, if mapping_event { Anchor::Mapping } else { Anchor::Pending });
                }
                frames.push(Frame { mapping: mapping_event, anchor, count: 0, mappings_only: true, subtitles });
                continue;
            }
            Event::MappingEnd | Event::SequenceEnd => {
                let frame = frames.pop().ok_or_else(|| invalid("unmatched collection end"))?;
                if frame.mapping != mapping_event { return Err(invalid("mismatched collection end")); }
                if frame.subtitles {
                    if !frame.mappings_only { return Err(invalid("subtitle records must be mappings")); }
                    metadata.chapter_count = frame.count;
                }
                if frame.anchor != 0 {
                    anchors.insert(frame.anchor, if frame.mapping { Anchor::Mapping }
                        else { Anchor::Sequence { count: frame.count, mappings_only: frame.mappings_only } });
                }
                if frames.is_empty() {
                    if !key_next { return Err(invalid("root mapping value missing")); }
                    root_done = true;
                }
                continue;
            }
            Event::Scalar(value, style, anchor, tag) => {
                sequence_item(&mut frames, false)?;
                // String projections ignore non-null tags. novel_type also
                // distinguishes an explicit integer from a quoted string.
                let tag_kind = match tag.as_deref() {
                    None => 0,
                    Some(tag) if tag.is_yaml_core_schema() && tag.suffix == "null" => 1,
                    Some(tag) if tag.is_yaml_core_schema() && tag.suffix == "str" => 2,
                    Some(tag) if tag.is_yaml_core_schema() && tag.suffix == "int" => 3,
                    Some(_) => 4,
                };
                if anchor != 0 {
                    charge(&mut retained, retention_limit, value.len().saturating_add(128))?;
                    anchors.insert(anchor, Anchor::Scalar(value.clone(), style, tag_kind));
                }
                (value, style, tag_kind)
            }
            Event::Alias(anchor) => {
                let value = anchors.get(&anchor).cloned().ok_or_else(|| invalid("unknown alias"))?;
                sequence_item(&mut frames, matches!(&value, Anchor::Mapping))?;
                match value {
                    Anchor::Scalar(value, style, tag_kind) => (value, style, tag_kind),
                    collection => {
                        if frames.is_empty() { return Err(invalid("root must be a mapping")); }
                        if frames.len() == 1 {
                            if key_next { return Err(invalid("root key alias must be a scalar")); }
                            mark(&mut seen, field)?;
                            if field == 4 {
                                match collection {
                                    Anchor::Sequence { count, mappings_only: true } => metadata.chapter_count = count,
                                    _ => return Err(invalid("subtitles alias must contain mapping records")),
                                }
                            } else if field != 0 { return Err(invalid("projected alias must be a scalar")); }
                            key_next = true;
                        }
                        continue;
                    }
                }
            }
        };
        if frames.is_empty() { return Err(invalid("root must be a mapping")); }
        if frames.len() != 1 { continue; }
        if key_next {
            field = match value.as_ref() {
                "story" if want_story => 1,
                "title" if !want_story => 2,
                "author" if !want_story => 3,
                "subtitles" if !want_story => 4,
                "novel_type" if !want_story => 5,
                _ => 0,
            };
            key_next = false;
            continue;
        }
        key_next = true;
        mark(&mut seen, field)?;
        if field == 0 { continue; }
        if field == 4 { return Err(invalid("subtitles must be a sequence")); }
        let plain = style == ScalarStyle::Plain;
        let null_spelling = matches!(value.as_ref(), "null" | "Null" | "NULL" | "~");
        let null = if matches!(field, 1 | 5) && plain && tag_kind == 1 {
            if !null_spelling { return Err(invalid("invalid explicitly tagged null")); }
            true
        } else { matches!(field, 1 | 5) && plain && tag_kind == 0 && (value.is_empty() || null_spelling) };
        if field == 5 {
            if !null {
                if (!plain && tag_kind != 3) || tag_kind == 2 { return Err(invalid("novel_type must be an integer or null")); }
                let number = value.strip_prefix('+').unwrap_or(&value);
                let (radix, digits) = if let Some(digits) = number.strip_prefix("0x") { (16, digits) }
                    else if let Some(digits) = number.strip_prefix("0o") { (8, digits) }
                    else if let Some(digits) = number.strip_prefix("0b") { (2, digits) }
                    else { (10, number) };
                metadata.novel_type = Some(u8::from_str_radix(digits, radix).map_err(invalid)?);
            }
        } else if field != 1 || !null {
            let retained_bytes = match &value { Cow::Owned(value) => value.capacity(), Cow::Borrowed(value) => value.len() };
            charge(&mut retained, retention_limit, retained_bytes)?;
            let value = Some(value.into_owned());
            match field { 1 => story = value, 2 => metadata.title = value, 3 => metadata.author = value, _ => unreachable!() }
        }
    }
}

pub fn fix_yaml_block_scalar(yaml: &str) -> String {
    let re = regex::Regex::new(r"(?m)^story:\s*\|[-+]?\s*$").unwrap();
    re.replace_all(yaml, "story: |-").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::types::SectionElement;
    use crate::error::Result;
    use crate::platform::mocks::MemoryObjectStore;
    use crate::platform::{
        AssetStore, Clock, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
        ObjectStore, PlatformFuture,
    };
    use chrono::{DateTime, Utc};
    use std::sync::Arc;
    use futures::TryStreamExt;

    async fn test_write(service: &PersistenceService, assets: Arc<dyn AssetStore>, keys: &NovelObjectKeys) -> NovelWriteGuard {
        NovelWriteGuard::capture_for(&service.objects, Some(&assets), keys).await.unwrap()
    }

    async fn test_read(service: &PersistenceService, keys: &NovelObjectKeys) -> NovelReadLease {
        NovelReadLease::admit_for(&service.objects, keys, "persistence-test").await.unwrap()
    }

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now_utc(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn subtitle() -> SubtitleInfo {
        SubtitleInfo {
            index: "1".to_string(),
            href: "https://example.test/1".to_string(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "第一話".to_string(),
            file_subtitle: "第一話".to_string(),
            subdate: "2026-08-01".to_string(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        }
    }

    #[test]
    fn cache_persistence_distinguishes_missing_sections_and_explicit_empty_versions() {
        use crate::application::webui::diff::{plan_diff_cache_clean, read_diff_cache_catalog};

        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("site", "title", false).unwrap();
        let subtitle = subtitle();
        futures::executor::block_on(async {
            assert!(!service.cache_section(&test_write(&service, store.clone(), &keys).await, &mut test_read(&service, &keys).await, &keys, "missing", &subtitle).await.unwrap());
            assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());
            service.record_cache_version(&test_write(&service, store.clone(), &keys).await, &keys, "empty").await.unwrap();
            let empty = read_diff_cache_catalog(&mut crate::application::novel_access::NovelReadScope::new(&service.objects, "cache-test"), store.as_ref(), keys.prefix(), &[]).await.unwrap();
            assert_eq!(empty.len(), 1);
            assert_eq!(empty[0].version, "empty");
            assert!(empty[0].only_latest_update);

            let section = section_file(&subtitle, "previous body");
            service.save_section(&test_write(&service, store.clone(), &keys).await, &keys, &subtitle, &section.element, 2).await.unwrap();
            assert!(service.cache_section(&test_write(&service, store.clone(), &keys).await, &mut test_read(&service, &keys).await, &keys, "snapshot", &subtitle).await.unwrap());
            service.record_cache_version(&test_write(&service, store.clone(), &keys).await, &keys, "snapshot").await.unwrap();
            #[cfg(feature = "native-runtime")]
            assert!(service.load_section(&mut test_read(&service, &keys).await, &keys, &subtitle).await.unwrap().is_none());
            #[cfg(not(feature = "native-runtime"))]
            assert_eq!(service.load_section(&mut test_read(&service, &keys).await, &keys, &subtitle).await.unwrap().unwrap().element.body, "previous body");
            let cached = store.read_small(&keys.cached_section("snapshot", "1", &subtitle.file_subtitle))
                .await.unwrap().unwrap();
            assert_eq!(serde_yaml::from_slice::<SectionFile>(&cached).unwrap().element.body, "previous body");
            let versions = read_diff_cache_catalog(&mut crate::application::novel_access::NovelReadScope::new(&service.objects, "cache-test"), store.as_ref(), keys.prefix(), &[]).await.unwrap();
            assert_eq!(versions.len(), 2);
            assert_eq!(versions[0].version, "snapshot");
            assert!(!versions[0].only_latest_update);
            assert_eq!(versions[0].entries, vec![("1".to_string(), "第一話".to_string())]);
            assert!(store.read_small(&keys.cached_version_marker("missing")).await.unwrap().is_none());
        });
    }

    #[test]
    fn persistence_roundtrip_uses_logical_keys_and_fixed_clock() {
        let store = Arc::new(MemoryObjectStore::new());
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let clock = Arc::new(FixedClock(
            DateTime::parse_from_rfc3339("2026-08-09T01:02:03Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        let service = PersistenceService::with_clock(objects.clone(), clock);
        let keys = NovelObjectKeys::new("example.test", "n1234ab", true).unwrap();
        let subtitle = subtitle();
        let section = SectionElement {
            data_type: "text".to_string(),
            introduction: String::new(),
            postscript: String::new(),
            body: "本文".to_string(),
        };

        futures::executor::block_on(async {
            let time = service
                .save_section(&test_write(&service, assets.clone(), &keys).await, &keys, &subtitle, &section, 2)
                .await
                .unwrap();
            assert_eq!(time, "2026-08-09 01:02:03.000000 +0000");
            let loaded = service
                .load_section(&mut test_read(&service, &keys).await, &keys, &subtitle)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(loaded.element.body, "本文");
            assert_eq!(loaded.download_time.as_deref(), Some(time.as_str()));

            service
                .ensure_default_files(&test_write(&service, assets.clone(), &keys).await, &keys, "題名", "作者", "https://example.test", None)
                .await
                .unwrap();
            service
                .save_toc(&test_write(&service, assets.clone(), &keys).await, 
                    &keys,
                    &TocFile {
                        title: "題名".to_string(),
                        author: "作者".to_string(),
                        toc_url: "https://example.test".to_string(),
                        story: None,
                        subtitles: vec![subtitle.clone()],
                        novel_type: Some(1),
                    },
                )
                .await
                .unwrap();
            service
                .ensure_default_files(&test_write(&service, assets.clone(), &keys).await, &keys, "題名", "作者", "https://example.test", None)
                .await
                .unwrap();
        });

        let section_key = keys.section(&subtitle.index, &subtitle.file_subtitle);
        assert!(
            futures::executor::block_on(objects.read_small(&section_key))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            futures::executor::block_on(objects.read_small(&keys.setting()))
                .unwrap()
                .unwrap(),
            default_setting_ini().into_bytes()
        );
    }

    fn subtitle_at(index: &str) -> SubtitleInfo {
        SubtitleInfo {
            index: index.to_string(),
            href: format!("https://example.test/{index}"),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: format!("第{index}話"),
            file_subtitle: format!("第{index}話"),
            subdate: "2026-08-01".to_string(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        }
    }

    #[test]
    fn section_snapshot_rejects_body_changes_before_successful_eof() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("site", "book", false).unwrap();
        let subtitle = subtitle();
        let toc = TocFile {
            title: "title".into(), author: "author".into(), toc_url: "https://example.test/".into(),
            story: None, subtitles: vec![subtitle.clone()], novel_type: Some(1),
        };
        futures::executor::block_on(async {
            service.save_toc(&test_write(&service, store.clone(), &keys).await, &keys, &toc).await.unwrap();
            let key = keys.section(&subtitle.index, &subtitle.file_subtitle);
            store.write_small(&key, serialize_section(&section_file(&subtitle, "old")).unwrap().into_bytes())
                .await.unwrap();
            let mut stream = service.stream_sections_snapshot(test_read(&service, &keys).await, keys.clone(), toc.clone());
            assert_eq!(stream.next().await.unwrap().unwrap().element.body, "old");
            // Even a legacy/manual rewrite without a revision bump is detected.
            store.write_small(&key, serialize_section(&section_file(&subtitle, "new")).unwrap().into_bytes())
                .await.unwrap();
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(error, NarouError::Platform(_)));
            assert!(stream.next().await.is_none());

            let mut stream = service.stream_sections_snapshot(test_read(&service, &keys).await, keys, toc);
            assert_eq!(stream.next().await.unwrap().unwrap().element.body, "new");
            ObjectStore::delete(store.as_ref(), &key).await.unwrap();
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(error, NarouError::Platform(_)));
            assert!(stream.next().await.is_none());
        });
    }

    #[test]
    fn section_snapshot_accepts_legacy_body_without_the_toc_timestamp() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("site", "book", false).unwrap();
        let mut subtitle = subtitle();
        subtitle.download_time = Some("2021-01-24 20:20:55.807706000 +09:00".into());
        let key = keys.section(&subtitle.index, &subtitle.file_subtitle);
        futures::executor::block_on(async {
            let mut section = section_file(&subtitle, "legacy body");
            store.write_small(&key, serialize_section(&section).unwrap().into_bytes()).await.unwrap();
            let mut toc = TocFile {
                title: "title".into(), author: "author".into(), toc_url: "https://example.test/".into(),
                story: None, subtitles: vec![subtitle], novel_type: Some(1),
            };
            for revision in [0, 1, 2] {
                toc.subtitles[0].download_revision = revision;
                service.save_toc(&test_write(&service, store.clone(), &keys).await, &keys, &toc).await.unwrap();
                let mut stream = service.stream_sections_snapshot(test_read(&service, &keys).await, keys.clone(), toc.clone());
                let loaded = stream.next().await.unwrap();
                if revision < 2 {
                    assert_eq!(loaded.unwrap().element.body, "legacy body");
                    assert!(stream.next().await.is_none());
                } else {
                    assert!(loaded.unwrap_err().to_string().contains("section generation changed"));
                }
            }

            // An explicitly stamped body must still agree with the TOC.
            section.download_revision = 2;
            section.download_time = Some("2026-11-18 00:00:00.000000000 +00:00".into());
            store.write_small(&key, serialize_section(&section).unwrap().into_bytes()).await.unwrap();
            let mut stream = service.stream_sections_snapshot(test_read(&service, &keys).await, keys, toc);
            assert!(stream.next().await.unwrap().unwrap_err().to_string().contains("section generation changed"));
        });
    }

    #[test]
    fn section_snapshot_rejects_uncommitted_download_generation() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("site", "book", false).unwrap();
        let subtitle = subtitle();
        let toc = TocFile {
            title: "title".into(), author: "author".into(), toc_url: "https://example.test/".into(),
            story: None, subtitles: vec![subtitle.clone()], novel_type: Some(1),
        };
        futures::executor::block_on(async {
            service.save_toc(&test_write(&service, store.clone(), &keys).await, &keys, &toc).await.unwrap();
            let mut section = section_file(&subtitle, "new generation");
            section.download_revision = 2;
            store.write_small(
                &keys.section(&subtitle.index, &subtitle.file_subtitle),
                serialize_section(&section).unwrap().into_bytes(),
            ).await.unwrap();
            let mut stream = service.stream_sections_snapshot(test_read(&service, &keys).await, keys, toc);
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(error, NarouError::Platform(_)));
        });
    }

    fn section_file(subtitle: &SubtitleInfo, body: &str) -> SectionFile {
        SectionFile {
            index: subtitle.index.clone(),
            href: subtitle.href.clone(),
            chapter: subtitle.chapter.clone(),
            subchapter: subtitle.subchapter.clone(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: subtitle.subupdate.clone(),
            download_time: None,
            element: SectionElement {
                data_type: "text".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: body.to_string(),
            },
            download_revision: subtitle.download_revision,
        }
    }

    /// 先読みの完了順と同時実行数をテスト側から制御するストア。
    ///
    /// `GatedStore::new` に渡したキーの `read_small` は、対応する sender で
    /// gate を開けるまで完了しない。開始・完了の記録を見て順番に開けるので、
    /// 完了順が入力順とずれる状況を決定的に作れる。
    struct GatedStore {
        data: MemoryObjectStore,
        gates: parking_lot::Mutex<Vec<(String, futures::channel::oneshot::Receiver<()>)>>,
        progress: parking_lot::Mutex<Progress>,
        progress_cv: parking_lot::Condvar,
        inflight: std::sync::atomic::AtomicUsize,
        max_inflight: std::sync::atomic::AtomicUsize,
    }

    #[derive(Default)]
    struct Progress {
        started: Vec<String>,
        completed: Vec<String>,
    }

    /// gate を開ける側。ストアが持つ receiver と対になる。テストが参照を
    /// 保持している限り未送信の gate は pending のまま (テスト終了時に送信
    /// されていない sender が落ちても、読み込み先は既に破棄されている)。
    type Gates = parking_lot::Mutex<
        std::collections::HashMap<String, futures::channel::oneshot::Sender<()>>,
    >;

    impl GatedStore {
        fn new(gated_keys: &[String]) -> (Arc<Self>, Arc<Gates>) {
            let mut gates = Vec::new();
            let mut senders = std::collections::HashMap::new();
            for key in gated_keys {
                let (sender, receiver) = futures::channel::oneshot::channel();
                gates.push((key.clone(), receiver));
                senders.insert(key.clone(), sender);
            }
            let store = Arc::new(Self {
                data: MemoryObjectStore::new(),
                gates: parking_lot::Mutex::new(gates),
                progress: parking_lot::Mutex::new(Progress::default()),
                progress_cv: parking_lot::Condvar::new(),
                inflight: std::sync::atomic::AtomicUsize::new(0),
                max_inflight: std::sync::atomic::AtomicUsize::new(0),
            });
            (store, Arc::new(parking_lot::Mutex::new(senders)))
        }

        fn wait_for_started(&self, count: usize) {
            let mut progress = self.progress.lock();
            while progress.started.len() < count {
                self.progress_cv.wait(&mut progress);
            }
        }

        fn wait_for_completion(&self, key: &str) {
            let mut progress = self.progress.lock();
            while !progress.completed.iter().any(|completed| completed == key) {
                self.progress_cv.wait(&mut progress);
            }
        }

        fn started(&self) -> Vec<String> {
            self.progress.lock().started.clone()
        }

        fn completed(&self) -> Vec<String> {
            self.progress.lock().completed.clone()
        }
    }

    impl ObjectStore for GatedStore {
        fn access_fence(&self) -> Option<&dyn crate::platform::NovelAccessFence> { Some(&self.data) }
        fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a crate::platform::GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<crate::platform::AssetStream>>> {
            Box::pin(async move {
                let snapshot = self.data.read_stream_leased(prefix, lease, key, range).await?;
                self.read_small(key).await?;
                Ok(snapshot)
            })
        }

        fn stat<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
            ObjectStore::stat(&self.data, key)
        }

        fn read_small<'a>(
            &'a self,
            key: &'a ObjectKey,
        ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
            let gate = {
                let mut gates = self.gates.lock();
                let position = gates
                    .iter()
                    .position(|(gated, _)| gated.as_str() == key.as_ref());
                position.map(|position| gates.swap_remove(position).1)
            };
            let Some(gate) = gate else {
                return self.data.read_small(key);
            };
            Box::pin(async move {
                let key_name = key.as_ref().to_string();
                let inflight = self.inflight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                self.max_inflight
                    .fetch_max(inflight, std::sync::atomic::Ordering::SeqCst);
                {
                    let mut progress = self.progress.lock();
                    progress.started.push(key_name.clone());
                    self.progress_cv.notify_all();
                }
                // gate に対応する sender が落ちた場合も読み込み自体は進める。
                let _ = gate.await;
                let result = self.data.read_small(key).await;
                self.inflight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                {
                    let mut progress = self.progress.lock();
                    progress.completed.push(key_name);
                    self.progress_cv.notify_all();
                }
                result
            })
        }

        fn write_small<'a>(
            &'a self,
            key: &'a ObjectKey,
            data: Vec<u8>,
        ) -> PlatformFuture<'a, Result<()>> {
            self.data.write_small(key, data)
        }

        fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
            ObjectStore::delete(&self.data, key)
        }

        fn list_page<'a>(
            &'a self,
            request: &'a ObjectListRequest,
        ) -> PlatformFuture<'a, Result<ObjectListPage>> {
            self.data.list_page(request)
        }
    }

    /// 後続が先に読み終えても返す順は入力順のまま、同時読み込みは窓の
    /// 件数までに収まる。

    #[test]
    fn stream_sections_ordered_prefetches_out_of_order_with_a_bounded_window() {
        let keys = NovelObjectKeys::new("example.test", "n1234ab", true).unwrap();
        let subtitles: Vec<SubtitleInfo> = ["1", "2", "3", "4", "5", "6"]
            .iter()
            .map(|index| subtitle_at(index))
            .collect();
        let section_keys: Vec<String> = subtitles
            .iter()
            .map(|subtitle| {
                keys.section(&subtitle.index, &subtitle.file_subtitle)
                    .as_ref()
                    .to_string()
            })
            .collect();

        let (store, senders) = GatedStore::new(&section_keys);
        let service = PersistenceService::new(store.clone());
        futures::executor::block_on(async {
            for (index, subtitle) in subtitles.iter().enumerate() {
                let section = section_file(subtitle, &format!("本文{}", index + 1));
                ObjectStore::write_small(
                    store.as_ref(),
                    &keys.section(&subtitle.index, &subtitle.file_subtitle),
                    serialize_section(&section).unwrap().into_bytes(),
                )
                .await
                .unwrap();
            }
        });

        // 最初の窓が全部開始したら、入力順の逆に gate を開けて完了させる。
        let releaser = std::thread::spawn({
            let store = store.clone();
            let senders = senders.clone();
            let section_keys = section_keys.clone();
            move || {
                store.wait_for_started(super::SECTION_READ_WINDOW);
                for index in [3usize, 2, 1, 0] {
                    let sender = senders.lock().remove(&section_keys[index]).unwrap();
                    sender.send(()).unwrap();
                    store.wait_for_completion(&section_keys[index]);
                }
                store.wait_for_started(section_keys.len());
                for index in [5usize, 4] {
                    let sender = senders.lock().remove(&section_keys[index]).unwrap();
                    sender.send(()).unwrap();
                    store.wait_for_completion(&section_keys[index]);
                }
            }
        });

        let sections =
            futures::executor::block_on(async { service.stream_sections_ordered(test_read(&service, &keys).await, &keys, &subtitles).try_collect::<Vec<_>>().await }).unwrap();
        releaser.join().unwrap();

        assert_eq!(
            sections
                .iter()
                .map(|section| section.element.body.clone())
                .collect::<Vec<_>>(),
            vec!["本文1", "本文2", "本文3", "本文4", "本文5", "本文6"],
            "先に読み終えた後続があっても本文は目次順に返る"
        );
        let mut started = store.started();
        started.sort();
        let mut expected = section_keys.clone();
        expected.sort();
        assert_eq!(started, expected, "全ての本文を先読みする");
        assert_eq!(
            store.completed(),
            vec![
                section_keys[3].clone(),
                section_keys[2].clone(),
                section_keys[1].clone(),
                section_keys[0].clone(),
                section_keys[5].clone(),
                section_keys[4].clone(),
            ],
            "テストは完了順を意図どおり制御している"
        );
        assert_eq!(
            store
                .max_inflight
                .load(std::sync::atomic::Ordering::SeqCst),
            super::SECTION_READ_WINDOW,
            "同時に走る読み込みは窓の件数まで"
        );
        assert_eq!(futures::executor::block_on(crate::platform::NovelAccessFence::gate_info(
            &store.data, &ObjectPrefix::from(keys.prefix().clone()),
        )).unwrap().pending_readers, 0, "EOF drops the prefetch window before releasing its final lease");
    }

    /// 走査順で最初の失敗 (欠落 or 読み込みエラー) だけを返す。
    #[test]
    fn stream_sections_ordered_reports_the_first_failure_in_input_order() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("example.test", "n1234ab", true).unwrap();
        let subtitles: Vec<SubtitleInfo> = ["1", "2", "3"]
            .iter()
            .map(|index| subtitle_at(index))
            .collect();
        let section_key =
            |subtitle: &SubtitleInfo| keys.section(&subtitle.index, &subtitle.file_subtitle);

        // 1 話目は正常、2 話目は欠落、3 話目は壊れた YAML。直列時の最初の
        // 失敗は 2 話目の欠落 (後続のエラーが先に見つかっても勝たない)。
        let first = section_file(&subtitles[0], "本文1");
        futures::executor::block_on(async {
            ObjectStore::write_small(
                store.as_ref(),
                &section_key(&subtitles[0]),
                serialize_section(&first).unwrap().into_bytes(),
            )
            .await
            .unwrap();
            ObjectStore::write_small(
                store.as_ref(),
                &section_key(&subtitles[2]),
                b"not: [valid yaml".to_vec(),
            )
            .await
            .unwrap();
        });
        match futures::executor::block_on(async { service.stream_sections_ordered(test_read(&service, &keys).await, &keys, &subtitles).try_collect::<Vec<_>>().await }) {
            Err(SectionLoadFailure::Missing { section_index }) => assert_eq!(section_index, 1),
            Err(SectionLoadFailure::Read(error)) => {
                panic!("欠落が先に報告されるべき: {error}")
            }
            Ok(sections) => panic!("失敗するはずが {} 件返った", sections.len()),
        }

        // 1 話目の YAML を壊す。2 話目は欠落のままなので、走査順で先の
        // 読み込みエラーが最初の失敗になる。
        futures::executor::block_on(ObjectStore::write_small(
            store.as_ref(),
            &section_key(&subtitles[0]),
            b"not: [valid yaml".to_vec(),
        ))
        .unwrap();
        match futures::executor::block_on(async { service.stream_sections_ordered(test_read(&service, &keys).await, &keys, &subtitles).try_collect::<Vec<_>>().await }) {
            Err(SectionLoadFailure::Read(_)) => {}
            Err(SectionLoadFailure::Missing { section_index }) => {
                panic!("読み込みエラーが先に報告されるべき: missing {section_index}")
            }
            Ok(sections) => panic!("失敗するはずが {} 件返った", sections.len()),
        }
    }

    /// 最初の失敗で打ち切り、先読みは窓の件数を超えない。
    #[test]
    fn stream_sections_ordered_stops_reading_after_the_first_failure() {
        let keys = NovelObjectKeys::new("example.test", "n1234ab", true).unwrap();
        let subtitles: Vec<SubtitleInfo> = ["1", "2", "3", "4", "5", "6"]
            .iter()
            .map(|index| subtitle_at(index))
            .collect();
        let section_keys: Vec<String> = subtitles
            .iter()
            .map(|subtitle| {
                keys.section(&subtitle.index, &subtitle.file_subtitle)
                    .as_ref()
                    .to_string()
            })
            .collect();

        let (store, senders) = GatedStore::new(&section_keys);
        let service = PersistenceService::new(store.clone());
        // 1 話目は欠落させたまま、最初の窓が開始したら gate を開ける。
        // 残りの gate はテストが保持し続けるので、破棄された先読みは完了しない。
        let releaser = std::thread::spawn({
            let store = store.clone();
            let senders = senders.clone();
            let first_key = section_keys[0].clone();
            move || {
                store.wait_for_started(super::SECTION_READ_WINDOW);
                let sender = senders.lock().remove(&first_key).unwrap();
                sender.send(()).unwrap();
            }
        });

        let failure = futures::executor::block_on(async { service.stream_sections_ordered(test_read(&service, &keys).await, &keys, &subtitles).try_collect::<Vec<_>>().await })
            .unwrap_err();
        releaser.join().unwrap();

        assert!(matches!(
            failure,
            SectionLoadFailure::Missing { section_index: 0 }
        ));
        assert_eq!(
            store.completed(),
            vec![section_keys[0].clone()],
            "最初の失敗より先の先読みは破棄される"
        );
        assert!(
            store.started().len() <= super::SECTION_READ_WINDOW,
            "開始する読み込みは窓の件数まで: {:?}",
            store.started()
        );
        assert_eq!(futures::executor::block_on(crate::platform::NovelAccessFence::gate_info(
            &store.data, &ObjectPrefix::from(keys.prefix().clone()),
        )).unwrap().pending_readers, 0, "failed prefetch tasks cannot retain the operation admission");
    }

    #[test]
    fn toc_story_projection_matches_legacy_scalar_and_alias_semantics() {
        for (story, expected) in [
            ("null", None), ("~", None), ("", None),
            ("'null'", Some("null")), ("!!str null", Some("null")),
            ("true", Some("true")), ("123", Some("123")),
            ("\"escaped\\u0020story\"", Some("escaped story")),
        ] {
            let yaml = format!("title: t\nauthor: a\ntoc_url: https://fixture.test/\nsubtitles: []\nstory: {story}\n");
            let legacy: TocFile = serde_yaml::from_str(&yaml).unwrap();
            let actual = deserialize_toc_story(yaml.as_bytes()).unwrap();
            assert_eq!(actual, expected.map(str::to_owned));
            assert_eq!(actual, legacy.story);
        }
        let yaml = b"holder: &text 'aliased story'\nsubtitles: [{story: nested-not-root}]\nstory: *text\n";
        assert_eq!(deserialize_toc_story(yaml).unwrap().as_deref(), Some("aliased story"));
    }

    #[test]
    fn toc_story_projection_checks_trailing_yaml_without_collecting_chapters() {
        let mut yaml = String::from("story: retained story\nsubtitles:\n");
        for index in 0..50_000 { yaml.push_str(&format!("  - index: '{index}'\n    subtitle: chapter\n")); }
        assert_eq!(deserialize_toc_story(yaml.as_bytes()).unwrap().as_deref(), Some("retained story"));
        yaml.push_str("broken: [unterminated\n");
        assert!(deserialize_toc_story(yaml.as_bytes()).is_err());
    }

    #[test]
    fn toc_story_projection_rejects_non_scalar_duplicate_and_over_budget_metadata() {
        for yaml in ["story: [nested]\n", "story: one\nstory: two\n", "story: yes\n---\nstory: no\n"] {
            assert!(deserialize_toc_story(yaml.as_bytes()).is_err(), "{yaml:?}");
        }
        let yaml = format!("story: &text '{}'\n", "x".repeat(7 * 1024 * 1024));
        assert!(deserialize_toc_story(yaml.as_bytes()).unwrap_err().to_string().contains("transfer_metadata_budget"));
    }

    #[test]
    fn ordered_prefetch_renews_its_original_lease_during_pinned_file_reads() {
        use crate::platform::{AssetStream, NovelAccessFence};
        use std::sync::atomic::{AtomicI64, Ordering};

        struct ProgressClock(AtomicI64);
        impl Clock for ProgressClock {
            fn now_utc(&self) -> DateTime<Utc> {
                DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
            }
        }
        struct PinnedReadStore {
            data: Arc<MemoryObjectStore>,
            clock: Arc<ProgressClock>,
        }
        impl ObjectStore for PinnedReadStore {
            fn access_fence(&self) -> Option<&dyn NovelAccessFence> { Some(self.data.as_ref()) }
            fn read_stream_leased<'a>(&'a self, prefix: &'a ObjectPrefix, lease: &'a crate::platform::GateLease, key: &'a ObjectKey, range: std::ops::Range<u64>) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
                Box::pin(async move {
                    // Model a filesystem handle pinned atomically at open:
                    // its bytes remain old even after admission closes.
                    let Some(opened) = self.data.read_stream_leased(prefix, lease, key, range).await? else { return Ok(None); };
                    drop(opened);
                    let bytes = self.data.read_small(key).await?.unwrap();
                    let chunks = crate::platform::staging::owned_bytes_stream(bytes);
                    let data = self.data.clone();
                    let clock = self.clock.clone();
                    let prefix = prefix.clone();
                    let stream: AssetStream = Box::pin(futures::stream::try_unfold(
                        (chunks, data, clock, prefix, false),
                        |(mut chunks, data, clock, prefix, closed)| async move {
                            let Some(chunk) = chunks.next().await else { return Ok(None); };
                            if !closed {
                                clock.0.fetch_add(10, Ordering::SeqCst);
                                data.close_gate(&prefix, "publication").await?;
                            }
                            Ok(Some((chunk?, (chunks, data, clock, prefix, true))))
                        },
                    ));
                    Ok(Some(stream))
                })
            }
            fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
                ObjectStore::stat(self.data.as_ref(), key)
            }
            fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
                self.data.read_small(key)
            }
            fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
                self.data.write_small(key, data)
            }
            fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
                ObjectStore::delete(self.data.as_ref(), key)
            }
            fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
                self.data.list_page(request)
            }
        }
        futures::executor::block_on(async {
            let data = Arc::new(MemoryObjectStore::new());
            let clock = Arc::new(ProgressClock(AtomicI64::new(Utc::now().timestamp())));
            let objects: Arc<dyn ObjectStore> = Arc::new(PinnedReadStore { data: data.clone(), clock: clock.clone() });
            let service = PersistenceService::new(objects.clone());
            let keys = NovelObjectKeys::new("site", "pinned-read", false).unwrap();
            let subtitle = subtitle();
            let section = section_file(&subtitle, &"old body".repeat(32 * 1024));
            let key = keys.section(&subtitle.index, &subtitle.file_subtitle);
            let original = serialize_section(&section).unwrap().into_bytes();
            data.write_small(&key, original.clone()).await.unwrap();
            let prefix = ObjectPrefix::from(keys.prefix().clone());
            let lease = NovelReadLease::admit_with_clock(&objects, clock, &prefix, "prefetch").await.unwrap();
            let subtitles = [subtitle];
            let error = service.stream_sections_ordered(lease, &keys, &subtitles).try_collect::<Vec<_>>().await.unwrap_err();
            assert!(matches!(error, SectionLoadFailure::Read(_)));
            assert!(format!("{error:?}").contains("novel_recovering"), "lease renewal must be denied after the gate closes mid-read: {error:?}");
            assert_eq!(data.read_small(&key).await.unwrap().unwrap(), original);
        });
    }

    #[test]
    fn toc_metadata_projects_scalar_and_subtitle_sequence_aliases() {
        let yaml = b"text: &title 'aliased title'\nchapters: &chapters [{index: '1', subtitle: one}, {index: '2', subtitle: two}]\ntitle: *title\nauthor: null\nsubtitles: *chapters\nnovel_type: &type 2\n";
        assert_eq!(deserialize_toc_metadata(yaml).unwrap(), TocMetadata {
            title: Some("aliased title".into()), author: Some("null".into()), chapter_count: 2, novel_type: Some(2),
        });
        let yaml = b"record: &chapter {index: '1', subtitle: one}\ntitle: title\nauthor: author\nsubtitles: [*chapter, *chapter]\nnovel_type: null\n";
        assert_eq!(deserialize_toc_metadata(yaml).unwrap().chapter_count, 2);
        assert_eq!(deserialize_toc_metadata(yaml).unwrap().novel_type, None);
        let expected = TocFile {
            title: "legacy title".into(), author: "legacy author".into(),
            toc_url: "https://fixture.test/book/".into(), story: Some("story".into()),
            subtitles: vec![subtitle(), subtitle()], novel_type: Some(2),
        };
        let yaml = serialize_toc(&expected).unwrap();
        let legacy = deserialize_toc(yaml.as_bytes()).unwrap();
        let projected = deserialize_toc_metadata(yaml.as_bytes()).unwrap();
        assert_eq!(projected.title, Some(legacy.title));
        assert_eq!(projected.author, Some(legacy.author));
        assert_eq!(projected.chapter_count, legacy.subtitles.len() as u64);
        assert_eq!(projected.novel_type, legacy.novel_type);
    }

    #[test]
    fn toc_metadata_counts_large_list_and_validates_complete_tail() {
        let mut yaml = String::from("title: projected title\nauthor: projected author\nnovel_type: 1\nsubtitles:\n");
        for index in 0..50_000 { yaml.push_str(&format!("  - index: '{index}'\n    subtitle: chapter\n")); }
        let metadata = deserialize_toc_metadata(yaml.as_bytes()).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("projected title"));
        assert_eq!(metadata.author.as_deref(), Some("projected author"));
        assert_eq!(metadata.chapter_count, 50_000);
        assert_eq!(metadata.novel_type, Some(1));
        yaml.push_str("broken: [unterminated\n");
        assert!(deserialize_toc_metadata(yaml.as_bytes()).is_err());
    }

    #[test]
    fn toc_metadata_rejects_wrong_types_duplicates_and_over_budget_retention() {
        for yaml in [
            "title: [nested]\n", "subtitles: {index: one}\n", "subtitles: [one]\n",
            "subtitles: [[{index: one}]]\n", "title: one\ntitle: two\n", "novel_type: 256\n",
            "chapters: &chapters [one]\nsubtitles: *chapters\n",
        ] { assert!(deserialize_toc_metadata(yaml.as_bytes()).is_err(), "{yaml:?}"); }
        let yaml = format!("title: &text '{}'\n", "x".repeat(7 * 1024 * 1024));
        assert!(deserialize_toc_metadata(yaml.as_bytes()).unwrap_err().to_string().contains("transfer_metadata_budget"));
    }

    /// Ruby 版が書いた末尾空白付きキー (`本文/65 テスト .yaml`) だけが
    /// 残っている小説でも、正規キー (末尾空白を落としたキー) が見つから
    /// ない場合は raw キーへ 1 回だけフォールバックして読めること。
    /// 通常ケース (raw キー == 正規キー) では追加の読み出しは走らない。
    #[test]
    fn section_reads_fall_back_to_unsanitized_ruby_key() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("site", "title", false).unwrap();
        let subtitle = SubtitleInfo {
            file_subtitle: "テスト ".to_string(),
            ..subtitle()
        };
        let section = section_file(&subtitle, "legacy body");

        futures::executor::block_on(async {
            let raw_key = ObjectKey::try_new("novels/site/title/本文/1 テスト .yaml").unwrap();
            assert_ne!(raw_key, keys.section(&subtitle.index, &subtitle.file_subtitle));
            let write = test_write(&service, store.clone(), &keys).await;
            write.write_small(&raw_key, serialize_section(&section).unwrap().into_bytes()).await.unwrap();

            let mut lease = test_read(&service, &keys).await;
            let loaded = service.load_section(&mut lease, &keys, &subtitle).await.unwrap();
            assert_eq!(loaded.unwrap().element.body, "legacy body");

            let toc = TocFile {
                title: "title".into(), author: "author".into(),
                toc_url: "https://example.test/".into(), story: None,
                subtitles: vec![subtitle.clone()], novel_type: None,
            };
            write.write_small(&keys.toc(), serialize_toc(&toc).unwrap().into_bytes()).await.unwrap();
            let snapshot_lease = test_read(&service, &keys).await;
            let mut stream = service.stream_sections_snapshot(snapshot_lease, keys.clone(), toc);
            let streamed = stream.next().await.unwrap().unwrap();
            assert_eq!(streamed.element.body, "legacy body");
            assert!(stream.next().await.is_none());
        });
    }

    /// 正規キーが存在するときは raw キーを読みに行かない (raw 側が別の
    /// 内容を持っていても正規キーの内容が返る)。
    #[test]
    fn section_reads_prefer_the_canonical_key() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone());
        let keys = NovelObjectKeys::new("site", "title", false).unwrap();
        let subtitle = SubtitleInfo {
            file_subtitle: "テスト ".to_string(),
            ..subtitle()
        };

        futures::executor::block_on(async {
            let canonical = keys.section(&subtitle.index, &subtitle.file_subtitle);
            let raw_key = ObjectKey::try_new("novels/site/title/本文/1 テスト .yaml").unwrap();
            assert_ne!(raw_key, canonical);
            let write = test_write(&service, store.clone(), &keys).await;
            let canonical_section = section_file(&subtitle, "canonical body");
            write.write_small(&canonical, serialize_section(&canonical_section).unwrap().into_bytes()).await.unwrap();
            let raw_section = section_file(&subtitle, "raw body");
            write.write_small(&raw_key, serialize_section(&raw_section).unwrap().into_bytes()).await.unwrap();

            let mut lease = test_read(&service, &keys).await;
            let loaded = service.load_section(&mut lease, &keys, &subtitle).await.unwrap();
            assert_eq!(loaded.unwrap().element.body, "canonical body");
        });
    }
}
