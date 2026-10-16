use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, Clock, NovelObjectKeys, ObjectListRequest, ObjectPrefix, ObjectStore, SystemClock,
};

use super::types::{SectionElement, SectionFile, SubtitleInfo, TocFile};

/// 本文セクションを先読みする件数。
///
/// 1 件ずつ直列に待つのをやめ、最初の失敗まで最大この件数を並行に読む。
/// 先読みの出力バッファは `read_small` の上限 × 窓の件数まで。デコード後の
/// セクション本体や設定入力は別に保持されるため、変換全体のメモリ上限ではない。
pub const SECTION_READ_WINDOW: usize = 4;

/// [`PersistenceService::load_sections_ordered`] の失敗。
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

/// Storage-backed downloader persistence.
///
/// The codec functions below remain pure; this service owns only the
/// ObjectStore/AssetStore calls and the injected clock.
#[derive(Clone)]
pub struct PersistenceService {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    clock: Arc<dyn Clock>,
}

impl PersistenceService {
    pub fn new(objects: Arc<dyn ObjectStore>, assets: Arc<dyn AssetStore>) -> Self {
        Self::with_clock(objects, assets, Arc::new(SystemClock))
    }

    pub fn with_clock(
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            objects,
            assets,
            clock,
        }
    }

    pub fn objects(&self) -> &Arc<dyn ObjectStore> {
        &self.objects
    }

    pub async fn load_toc(&self, keys: &NovelObjectKeys) -> Result<Option<TocFile>> {
        let Some(bytes) = self.objects.read_small(&keys.toc()).await? else {
            return Ok(None);
        };
        Ok(Some(deserialize_toc(&bytes)?))
    }

    pub async fn save_toc(&self, keys: &NovelObjectKeys, toc: &TocFile) -> Result<()> {
        self.objects
            .write_small(&keys.toc(), serialize_toc(toc)?.into_bytes())
            .await
    }

    pub async fn load_section(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
    ) -> Result<Option<SectionFile>> {
        let Some(bytes) = self
            .objects
            .read_small(&keys.section(&subtitle.index, &subtitle.file_subtitle))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(deserialize_section(&bytes)?))
    }

    /// 本文セクションを最大 [`SECTION_READ_WINDOW`] 件の並行読みで取得し、
    /// 入力と同じ順で返す。
    ///
    /// 失敗の扱いは 1 件ずつ直列に [`Self::load_section`] を呼んだ場合と
    /// 同じで、走査順で最初の失敗 (`Missing` または `Read`) を返す。先に
    /// 読み終えた後続があっても、返る順序と「最初の失敗」の意味は変わらない。
    /// 最初の失敗より先 (窓の分だけ) を先読みしていた読み込みは破棄される。
    pub async fn load_sections_ordered(
        &self,
        keys: &NovelObjectKeys,
        subtitles: &[SubtitleInfo],
    ) -> std::result::Result<Vec<SectionFile>, SectionLoadFailure> {
        let mut remaining = subtitles.iter();
        let mut pending = futures::stream::FuturesOrdered::new();
        for subtitle in remaining.by_ref().take(SECTION_READ_WINDOW) {
            pending.push_back(self.load_section(keys, subtitle));
        }
        let mut sections = Vec::with_capacity(subtitles.len());
        let mut section_index = 0;
        while let Some(result) = pending.next().await {
            match result {
                Ok(Some(section)) => sections.push(section),
                Ok(None) => return Err(SectionLoadFailure::Missing { section_index }),
                Err(error) => return Err(SectionLoadFailure::Read(error)),
            }
            section_index += 1;
            if let Some(subtitle) = remaining.next() {
                pending.push_back(self.load_section(keys, subtitle));
            }
        }
        Ok(sections)
    }

    pub async fn save_section(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        section: &SectionElement,
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
        };
        self.objects
            .write_small(
                &keys.section(&subtitle.index, &subtitle.file_subtitle),
                serialize_section(&file_data)?.into_bytes(),
            )
            .await?;
        Ok(download_time)
    }

    pub async fn section_needs_update(
        &self,
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        new_section: &SectionElement,
    ) -> Result<bool> {
        let Some(existing) = self.load_section(keys, subtitle).await? else {
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
        keys: &NovelObjectKeys,
        subtitle: &SubtitleInfo,
        raw_html: &str,
    ) -> Result<()> {
        if raw_html.is_empty() {
            return Ok(());
        }
        self.objects
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
        keys: &NovelObjectKeys,
        timestamp: &str,
        subtitle: &SubtitleInfo,
    ) -> Result<bool> {
        let source = keys.section(&subtitle.index, &subtitle.file_subtitle);
        if !self.objects.exists(&source).await? {
            return Ok(false);
        }
        let destination = keys.cached_section(timestamp, &subtitle.index, &subtitle.file_subtitle);
        #[cfg(feature = "native-runtime")]
        self.assets.move_or_copy(&source, &destination).await?;
        #[cfg(not(feature = "native-runtime"))]
        self.assets.copy(&source, &destination).await?;
        Ok(true)
    }

    /// Record a persisted version after its first successfully cached section, or
    /// explicitly record an empty version after a completed latest-only update.
    pub async fn record_cache_version(
        &self,
        keys: &NovelObjectKeys,
        timestamp: &str,
    ) -> Result<()> {
        self.objects.write_small(&keys.cached_version_marker(timestamp), Vec::new()).await
    }

    /// Write `setting.ini` / `replace.txt` defaults for files that do not yet
    /// exist. When `existing_names` is provided (from
    /// [`Self::list_novel_object_names`]) existence is decided from the set
    /// with no extra storage round trips; otherwise each file is probed with
    /// `stat`.
    pub async fn ensure_default_files(
        &self,
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
            match self.objects.update_small(&keys.setting(), &mut mutate).await? {
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
            match self.objects.update_small(&keys.replace(), &mut mutate).await? {
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
                    crate::application::debug::emit_error(format!(
                        "挿絵: 保存済みの一覧を取れません ({error})"
                    ));
                    return None;
                }
            }
        }
        None
    }

    pub async fn load_cache(
        &self,
        keys: &NovelObjectKeys,
    ) -> Result<Option<crate::illustration_store::IllustrationIndex>> {
        let Some(bytes) = self.objects.read_small(&keys.illustration_cache()).await? else {
            return Ok(None);
        };
        Ok(Some(serde_yaml::from_slice(&bytes)?))
    }

    pub async fn save_cache(
        &self,
        keys: &NovelObjectKeys,
        index: &crate::illustration_store::IllustrationIndex,
    ) -> Result<()> {
        let body = index.to_cache_yaml()?;
        self.objects
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
        }
    }

    #[test]
    fn cache_persistence_distinguishes_missing_sections_and_explicit_empty_versions() {
        use crate::application::webui::diff::{plan_diff_cache_clean, read_diff_cache_catalog};

        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone(), store.clone());
        let keys = NovelObjectKeys::new("site", "title", false).unwrap();
        let subtitle = subtitle();
        futures::executor::block_on(async {
            assert!(!service.cache_section(&keys, "missing", &subtitle).await.unwrap());
            assert!(plan_diff_cache_clean(store.as_ref(), keys.prefix()).await.unwrap().is_empty());
            service.record_cache_version(&keys, "empty").await.unwrap();
            let empty = read_diff_cache_catalog(store.as_ref(), keys.prefix(), &[]).await.unwrap();
            assert_eq!(empty.len(), 1);
            assert_eq!(empty[0].version, "empty");
            assert!(empty[0].only_latest_update);

            let section = section_file(&subtitle, "previous body");
            service.save_section(&keys, &subtitle, &section.element).await.unwrap();
            assert!(service.cache_section(&keys, "snapshot", &subtitle).await.unwrap());
            service.record_cache_version(&keys, "snapshot").await.unwrap();
            #[cfg(feature = "native-runtime")]
            assert!(service.load_section(&keys, &subtitle).await.unwrap().is_none());
            #[cfg(not(feature = "native-runtime"))]
            assert_eq!(service.load_section(&keys, &subtitle).await.unwrap().unwrap().element.body, "previous body");
            let cached = store.read_small(&keys.cached_section("snapshot", "1", &subtitle.file_subtitle))
                .await.unwrap().unwrap();
            assert_eq!(serde_yaml::from_slice::<SectionFile>(&cached).unwrap().element.body, "previous body");
            let versions = read_diff_cache_catalog(store.as_ref(), keys.prefix(), &[]).await.unwrap();
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
        let service = PersistenceService::with_clock(objects.clone(), assets, clock);
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
                .save_section(&keys, &subtitle, &section)
                .await
                .unwrap();
            assert_eq!(time, "2026-08-09 01:02:03.000000 +0000");
            let loaded = service
                .load_section(&keys, &subtitle)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(loaded.element.body, "本文");
            assert_eq!(loaded.download_time.as_deref(), Some(time.as_str()));

            service
                .ensure_default_files(&keys, "題名", "作者", "https://example.test", None)
                .await
                .unwrap();
            service
                .save_toc(
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
                .ensure_default_files(&keys, "題名", "作者", "https://example.test", None)
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
        }
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
    fn load_sections_ordered_prefetches_out_of_order_with_a_bounded_window() {
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
        let service = PersistenceService::new(store.clone(), Arc::new(MemoryObjectStore::new()));
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
            futures::executor::block_on(service.load_sections_ordered(&keys, &subtitles)).unwrap();
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
    }

    /// 走査順で最初の失敗 (欠落 or 読み込みエラー) だけを返す。
    #[test]
    fn load_sections_ordered_reports_the_first_failure_in_input_order() {
        let store = Arc::new(MemoryObjectStore::new());
        let service = PersistenceService::new(store.clone(), store.clone());
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
        match futures::executor::block_on(service.load_sections_ordered(&keys, &subtitles)) {
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
        match futures::executor::block_on(service.load_sections_ordered(&keys, &subtitles)) {
            Err(SectionLoadFailure::Read(_)) => {}
            Err(SectionLoadFailure::Missing { section_index }) => {
                panic!("読み込みエラーが先に報告されるべき: missing {section_index}")
            }
            Ok(sections) => panic!("失敗するはずが {} 件返った", sections.len()),
        }
    }

    /// 最初の失敗で打ち切り、先読みは窓の件数を超えない。
    #[test]
    fn load_sections_ordered_stops_reading_after_the_first_failure() {
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
        let service = PersistenceService::new(store.clone(), Arc::new(MemoryObjectStore::new()));
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

        let failure = futures::executor::block_on(service.load_sections_ordered(&keys, &subtitles))
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
    }
}
