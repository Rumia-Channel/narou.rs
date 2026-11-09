//! 変換テキストの組み立て（fs を使わない）。
//!
//! 保存済みの `toc.yaml` と `本文/*.yaml` を目次順に読み、一話ずつ変換する。
//! 完成するまで非公開の immutable asset に書き、EPUB のメタデータ走査と
//! 出力走査は同じ内容を再生する。`novel.txt` の公開は検証済み EOF の後だけ。
//!
//! 小説単位の入力（`setting.ini` / `replace.txt` / `converter.yaml`）も
//! ObjectStore から読む。`default.*` / `force.*` は `SettingsStore`
//! (native=Inventory, Worker=D1) から読んだ値を適用する。

use std::sync::Arc;
use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::converter::ini::IniData;
use crate::converter::settings::{NovelSettings, parse_replace_patterns};
use crate::converter::user_converter::UserConverter;
use crate::converter::{ConverterCapabilities, NovelConverter};
use crate::db::NovelRecord;
use crate::downloader::persistence::PersistenceService;
pub use crate::downloader::persistence::SectionStream;
#[cfg(test)]
use crate::downloader::SectionFile;
use crate::downloader::{TocFile, TocObject, UpdateStatus};
use crate::error::{NarouError, Result};
use crate::platform::{AssetStore, AssetStream, NovelObjectKeys, ObjectKey, ObjectStore, StagedAsset};
use crate::setting_core::SettingScope;

use super::messages::{self, MessageSink, Stream};
use super::settings::SettingsStore;

/// 変換結果。`output` は実際に保存した場合だけ、そのオブジェクトキーを持つ。
#[derive(Debug)]
pub struct ConvertedNovel {
    pub source: ConvertedText,
    pub output: Option<ObjectKey>,
    pub settings: NovelSettings,
}

/// Immutable, replayable converted text. The caller deletes the staging object
/// after its last pass; abandoned requests are covered by the store's expiry
/// journal. No body bytes are retained by this handle.
#[derive(Clone)]
pub struct ConvertedText {
    pub key: ObjectKey,
    pub size: u64,
    pub sha256: String,
    assets: Arc<dyn AssetStore>,
}

impl std::fmt::Debug for ConvertedText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ConvertedText")
            .field("key", &self.key)
            .field("size", &self.size)
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl ConvertedText {
    fn from_stage(assets: Arc<dyn AssetStore>, stage: StagedAsset) -> Self {
        Self { key: stage.key, size: stage.size, sha256: stage.sha256, assets }
    }

    /// Freeze a cached `novel.txt` using a single streamed read, not a second
    /// read of its mutable key during the EPUB output pass.
    pub async fn from_stored(
        assets: Arc<dyn AssetStore>,
        key: ObjectKey,
    ) -> Result<Option<Self>> {
        let Some(stream) = assets.read_stream(&key).await? else {
            return Ok(None);
        };
        let stage = assets.stage_stream(stream).await?;
        Ok(Some(Self::from_stage(assets, stage)))
    }

    /// Every replay verifies the immutable reference through EOF. A truncated
    /// or corrupt stage is an error, including after earlier chunks were read.
    pub async fn read_stream(&self) -> Result<AssetStream> {
        let stream = self.assets.read_stream(&self.key).await?.ok_or_else(|| {
            NarouError::Platform(format!("converted text snapshot disappeared: {}", self.key))
        })?;
        let mut expected_hash = [0u8; 32];
        hex::decode_to_slice(&self.sha256, &mut expected_hash).map_err(|error| {
            NarouError::Platform(format!("invalid converted text snapshot digest: {error}"))
        })?;
        Ok(Box::pin(futures::stream::try_unfold(
            (stream, Sha256::new(), 0u64, self.size, expected_hash),
            |(mut stream, mut hash, read, expected_size, expected_hash)| async move {
                match stream.next().await {
                    Some(chunk) => {
                        let chunk = chunk?;
                        let read = read.checked_add(chunk.len() as u64).ok_or_else(|| {
                            NarouError::Platform("converted text snapshot length overflow".into())
                        })?;
                        if read > expected_size {
                            return Err(NarouError::Platform("converted text snapshot grew during replay".into()));
                        }
                        hash.update(&chunk);
                        Ok(Some((chunk, (stream, hash, read, expected_size, expected_hash))))
                    }
                    None => {
                        if read != expected_size {
                            return Err(NarouError::Platform("converted text snapshot truncated during replay".into()));
                        }
                        if <[u8; 32]>::from(hash.finalize()) != expected_hash {
                            return Err(NarouError::Platform("converted text snapshot checksum mismatch".into()));
                        }
                        Ok(None)
                    }
                }
            },
        )))
    }

    pub async fn cleanup(&self) -> Result<()> {
        self.assets.delete(&self.key).await
    }
}

const CONVERTED_TEXT_CHUNK_BYTES: usize = 64 * 1024;

struct RenderStreamState {
    converter: NovelConverter,
    render: Option<crate::converter::NovelRenderState>,
    toc: TocObject,
    sections: SectionStream,
    pending: std::vec::IntoIter<u8>,
    emitted: usize,
}

async fn rendered_sections_stream(
    mut converter: NovelConverter,
    toc: TocObject,
    sections: SectionStream,
    record: &NovelRecord,
) -> AssetStream {
    let (render, header) = converter.begin_novel(None, &toc, toc.subtitles.len(), Some(record));
    converter.flush_progress().await;
    Box::pin(futures::stream::try_unfold(
        RenderStreamState {
            converter, render: Some(render), toc, sections,
            pending: header.into_bytes().into_iter(), emitted: 0,
        },
        |mut state| async move {
            loop {
                if !state.pending.as_slice().is_empty() {
                    let chunk = state.pending.by_ref().take(CONVERTED_TEXT_CHUNK_BYTES).collect();
                    if state.pending.as_slice().is_empty() {
                        state.pending = Vec::new().into_iter();
                    }
                    return Ok(Some((chunk, state)));
                }
                let Some(render) = state.render.as_mut() else {
                    return Ok(None);
                };
                messages::ready().await;
                match state.sections.next().await {
                    Some(section) => {
                        let section = section?;
                        if state.emitted >= state.toc.subtitles.len() {
                            return Err(NarouError::Platform("too many sections in conversion source".into()));
                        }
                        let text = state.converter.render_section(render, &state.toc, &section)?;
                        state.converter.flush_progress().await;
                        state.pending = text.into_bytes().into_iter();
                        state.emitted += 1;
                    }
                    None => {
                        if state.emitted != state.toc.subtitles.len() {
                            return Err(NarouError::Platform("incomplete conversion source".into()));
                        }
                        let text = state.converter.finish_novel(
                            state.render.take().expect("unfinished render"), &state.toc,
                        )?;
                        state.converter.flush_progress().await;
                        state.pending = text.into_bytes().into_iter();
                    }
                }
            }
        },
    ))
}

/// 1 件の変換結果。native `cmd_convert` の per-target 分岐
/// (`src/commands/convert.rs`) に対応する。
#[derive(Debug)]
pub enum ConvertItemReport {
    /// 変換完了。保存しない構成では `output` は None。
    Converted { output: Option<ObjectKey> },
    /// 対象 id のレコードが存在しない (`ID: {id} は存在しません`)。
    Missing,
    /// 変換中の失敗 (`[i/t] エラー: {target} - {error}`)。
    Failed { error: NarouError },
}

/// `narou convert <id>` が出すコンソール行の 1 件分を、native `cmd_convert`
/// と同じ文言・順序・Stream (全て stdout) で `sink` へ送る。Worker の
/// convert ジョブは常に 1 件なので index/total は `1/1` に固定する。
///
/// `run` はレコード解決込みの変換本体。`Ok(None)` はレコード不在
/// (native の `id_missing` 行) に対応する。device 選択・AozoraEpub3 出力・
/// コピー/送信・inspect の行は Worker の変換には存在しないので出さない。
/// 保存した場合だけキーの末尾を出力ファイル名として表示する。
pub async fn emit_convert_item_lines<Run, Fut>(
    sink: &dyn MessageSink,
    target: &str,
    id: i64,
    run: Run,
) -> ConvertItemReport
where
    Run: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<ConvertedNovel>>>,
{
    const INDEX: usize = 1;
    const TOTAL: usize = 1;
    sink.emit_async(
        Stream::Stdout,
        &messages::convert::convert_started(TOTAL),
    ).await;
    sink.emit_async(
        Stream::Stdout,
        &messages::convert::processing(INDEX, TOTAL, target),
    ).await;
    let result = match run().await {
        Ok(Some(converted)) => match converted.source.cleanup().await {
            Ok(()) => Ok(Some(converted)),
            Err(error) => Err(error),
        },
        other => other,
    };
    let (completed, report) = match result {
        Ok(Some(converted)) => {
            if let Some(output) = &converted.output {
                let filename = output.as_ref().rsplit('/').next().unwrap_or(output.as_ref());
                sink.emit_async(Stream::Stdout, &messages::convert::output_written(filename)).await;
            }
            sink.emit_async(
                Stream::Stdout,
                &messages::convert::completed(INDEX, TOTAL, target),
            ).await;
            (
                1,
                ConvertItemReport::Converted {
                    output: converted.output,
                },
            )
        }
        Ok(None) => {
            sink.emit_async(Stream::Stdout, &messages::convert::id_missing(id)).await;
            (0, ConvertItemReport::Missing)
        }
        Err(error) => {
            sink.emit_async(
                Stream::Stdout,
                &messages::convert::item_error(INDEX, TOTAL, target, &error),
            ).await;
            (0, ConvertItemReport::Failed { error })
        }
    };
    sink.emit_async(
        Stream::Stdout,
        &messages::convert::convert_finished(completed, TOTAL),
    ).await;
    report
}

/// `--no-convert` の適用範囲は native と Worker で既存差があるため、
/// 呼び出し側が自分の契約を明示する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoConvertScope {
    /// `--no-convert` は全ステータスを抑止する (Worker の現行)。
    Everything,
    /// `--no-convert` は `Ok` だけを抑止し、`None` の再変換は続行する
    /// (native の narou.rb 互換動作)。
    SuccessOnly,
}

/// 更新/ダウンロード結果から自動変換 (follow-up convert) を積むか。
///
/// 各入力は呼び出し側の契約:
/// - `convert_only_new_arrival` は Update の `Ok` にだけ効く (Download は
///   取得成功なら `false` を渡す)。
/// - `convert_failure` は保存済みレコードの `convert_failure` フラグ。
/// - `missing_cached_text` は Worker の自己修復 (keep-txt 有効で変換済み
///   テキストが無い) 用の実行時入力。native は存在確認を持たず `false`。
///
/// `Failed` / `Canceled` は常に積まない。
pub fn needs_convert_after_download(
    status: UpdateStatus,
    no_convert: bool,
    no_convert_scope: NoConvertScope,
    new_arrivals: bool,
    convert_only_new_arrival: bool,
    convert_failure: bool,
    missing_cached_text: bool,
) -> bool {
    let no_convert_blocks = match no_convert_scope {
        NoConvertScope::Everything => no_convert,
        NoConvertScope::SuccessOnly => no_convert && status == UpdateStatus::Ok,
    };
    if no_convert_blocks {
        return false;
    }
    match status {
        UpdateStatus::Ok => !convert_only_new_arrival || new_arrivals,
        UpdateStatus::None => convert_failure || missing_cached_text,
        UpdateStatus::Failed | UpdateStatus::Canceled => false,
    }
}

/// 小説 1 件の変換テキストを作って保存する。
pub struct ConvertService {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    settings: Arc<dyn SettingsStore>,
    capabilities: Option<ConverterCapabilities>,
}

impl ConvertService {
    pub fn new(
        objects: Arc<dyn ObjectStore>,
        assets: Arc<dyn AssetStore>,
        settings: Arc<dyn SettingsStore>,
    ) -> Self {
        Self {
            objects,
            assets,
            settings,
            capabilities: None,
        }
    }

    /// 挿絵のローカライズなど、変換中に使う平台能力を渡す (native と同じ扱い)。
    pub fn with_capabilities(mut self, capabilities: ConverterCapabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    /// 保存済みデータから変換テキストを作り、`<prefix>/novel.txt` に書く。
    /// 戻り値の `output` は native の出力ファイルパス相当 (表示名は末尾の
    /// `novel.txt`) で、コンソールの `{name} を出力しました` 行が使う。
    ///
    /// 本文は一話ずつ読み、世代と内容ハッシュを検証してから公開する。
    /// 目次にある本文が欠ける場合は既存の出力を変更しない。
    pub async fn convert_and_store(&self, record: &NovelRecord) -> Result<ConvertedNovel> {
        self.convert_inner(record, true, None).await
    }

    /// 変換だけを行う (`convert_and_store` と同じだが、結果を保存しない)。
    ///
    /// 変換済みテキストを保存しない構成 (`convert.keep-txt=false`) で、
    /// 必要になった時点で組み立て直すために使う。
    pub async fn convert_only(&self, record: &NovelRecord) -> Result<ConvertedNovel> {
        self.convert_inner(record, false, None).await
    }

    /// Convert an owned, ordered source (including virtual-book members).
    /// The source must report generation/hash changes as errors before EOF.
    pub async fn convert_source(
        &self,
        record: &NovelRecord,
        toc: TocFile,
        sections: SectionStream,
        store: bool,
    ) -> Result<ConvertedNovel> {
        self.convert_inner(record, store, Some((toc, sections))).await
    }

    async fn convert_inner(
        &self,
        record: &NovelRecord,
        store: bool,
        source: Option<(TocFile, SectionStream)>,
    ) -> Result<ConvertedNovel> {
        let keys = NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        )?;
        // 目次・本文と、小説単位の独立な設定ファイルを同時に読む。
        // エラーの優先順位は直列だったときの読取順 (本文 → setting.ini →
        // 置換 → converter.yaml) のままになるよう、join 後にこの順で評価する。
        let replace_key = keys.replace();
        let (source, ini, local_settings, replacement, converter_yaml) = futures::join!(
            async {
                match source {
                    Some(source) => Ok(source),
                    None => self.load_source(&keys).await,
                }
            },
            self.load_ini(&keys),
            self.settings.load(SettingScope::Local),
            self.read_text(&replace_key),
            async {
                let converter_key =
                    ObjectKey::try_new(format!("{}/converter.yaml", keys.prefix().as_ref()))?;
                self.read_text(&converter_key).await
            },
        );
        let (toc, mut sections) = source?;
        if toc.subtitles.is_empty() {
            return Err(NarouError::Platform(format!(
                "本文がありません: {}",
                keys.prefix().as_ref()
            )));
        }

        let inputs = (|| Ok::<_, NarouError>((ini?, replacement?, converter_yaml?)))();
        let (ini, replacement, converter_yaml) = match inputs {
            Ok(inputs) => inputs,
            Err(error) => {
                // Preserve source-before-settings error precedence without
                // eagerly retaining all source sections on the successful path.
                while let Some(section) = sections.next().await {
                    section?;
                }
                return Err(error);
            }
        };
        let local_settings = local_settings.unwrap_or_default();
        let mut settings = NovelSettings::from_sources(
            Some(record.id),
            &record.title,
            &record.author,
            &ini,
            &local_settings,
            false,
            false,
        );
        if let Some(text) = replacement {
            // 小説ごとの replace.txt。全体設定側 (インストール先) は native 専用。
            settings.replace_patterns = parse_replace_patterns(&text);
        }
        let user_converter = match converter_yaml {
            Some(yaml) => {
                UserConverter::from_yaml(&yaml, &settings.title_for_output(&record.title))
            }
            None => None,
        };

        let result_settings = settings.clone();
        let converter =
            NovelConverter::build(settings, user_converter, self.capabilities.clone());
        let toc_object = TocObject {
            title: toc.title,
            author: toc.author,
            toc_url: toc.toc_url,
            story: toc.story,
            subtitles: toc.subtitles,
            novel_type: toc.novel_type,
        };
        let stream = rendered_sections_stream(converter, toc_object, sections, record).await;
        let stage = self.assets.stage_stream(stream).await?;
        let source = ConvertedText::from_stage(self.assets.clone(), stage);
        let output = keys.converted_text();
        // `convert.keep-txt` の解釈は native のファイル経路 (`converter::mod`)
        // と Worker で共有する (`setting_core::keep_converted_text`)。既定は
        // native と同じ「保存する」。
        let keep_text = crate::setting_core::keep_converted_text(
            &local_settings,
            std::env::var("NAROU_RS_KEEP_TXT").ok().as_deref(),
            true,
        );
        let output = if store && keep_text {
            if let Err(error) = self.assets.copy(&source.key, &output).await {
                let _ = source.cleanup().await;
                return Err(error);
            }
            Some(output)
        } else {
            None
        };
        Ok(ConvertedNovel { source, output, settings: result_settings })
    }

    async fn load_source(&self, keys: &NovelObjectKeys) -> Result<(TocFile, SectionStream)> {
        let persistence = PersistenceService::new(self.objects.clone(), self.assets.clone());
        let toc = persistence.load_toc(keys).await?.ok_or_else(|| {
            NarouError::Platform(format!("toc がありません: {}", keys.toc().as_ref()))
        })?;
        let sections = persistence.stream_sections_snapshot(keys.clone(), toc.clone());
        Ok((toc, sections))
    }

    async fn load_ini(&self, keys: &NovelObjectKeys) -> Result<IniData> {
        Ok(match self.objects.read_small(&keys.setting()).await? {
            Some(bytes) => IniData::load(&String::from_utf8_lossy(&bytes)),
            None => IniData::new(),
        })
    }

    async fn read_text(
        &self,
        key: &crate::platform::ObjectKey,
    ) -> Result<Option<String>> {
        Ok(self
            .objects
            .read_small(key)
            .await?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::persistence::{serialize_section, serialize_toc};
    use crate::downloader::types::{SectionElement, SubtitleInfo, TocFile};
    use crate::platform::mocks::MemoryObjectStore;
    use futures::executor::block_on;

    async fn collected_text(source: &ConvertedText) -> String {
        let mut stream = source.read_stream().await.unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend(chunk.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }

    /// `NAROU_RS_KEEP_TXT` が設定された環境でもテスト結果が変わらないよう、
    /// テストの間だけ外す。プロセス共通の env を触るため、既存の全体ロックを
    /// 共有して直列化する。
    struct KeepTxtEnvGuard {
        _state: crate::test_support::GlobalStateGuard,
        original: Option<std::ffi::OsString>,
    }

    impl KeepTxtEnvGuard {
        fn cleared() -> Self {
            let state = crate::test_support::global_state_guard();
            let original = std::env::var_os("NAROU_RS_KEEP_TXT");
            // SAFETY: 全体ロックを保持している間だけ書き換える。
            unsafe { std::env::remove_var("NAROU_RS_KEEP_TXT") };
            Self {
                _state: state,
                original,
            }
        }
    }

    impl Drop for KeepTxtEnvGuard {
        fn drop(&mut self) {
            // SAFETY: 同上。
            unsafe {
                match self.original.take() {
                    Some(value) => std::env::set_var("NAROU_RS_KEEP_TXT", value),
                    None => std::env::remove_var("NAROU_RS_KEEP_TXT"),
                }
            }
        }
    }

    fn streaming_fixture(count: usize, body: &str) -> (TocFile, Vec<SectionFile>) {
        let mut subtitles = Vec::new();
        let mut sections = Vec::new();
        for index in 1..=count {
            let subtitle = SubtitleInfo {
                index: index.to_string(), href: format!("https://example.test/{index}"),
                chapter: if index == 1 { "章".into() } else { String::new() },
                subchapter: "節".into(), subtitle: format!("第{index}話"),
                file_subtitle: format!("第{index}話"), subdate: String::new(),
                subupdate: None, download_time: None, download_revision: 0,
            };
            sections.push(SectionFile {
                index: subtitle.index.clone(), href: subtitle.href.clone(),
                chapter: subtitle.chapter.clone(), subchapter: subtitle.subchapter.clone(),
                subtitle: subtitle.subtitle.clone(), file_subtitle: subtitle.file_subtitle.clone(),
                subdate: String::new(), subupdate: None, download_time: None, download_revision: 0,
                element: SectionElement {
                    data_type: "text".into(), introduction: "前書き".into(),
                    body: body.into(), postscript: "後書き".into(),
                },
            });
            subtitles.push(subtitle);
        }
        (TocFile {
            title: "作品".into(), author: "作者".into(), toc_url: "https://example.test/".into(),
            story: Some("あらすじ".into()), subtitles, novel_type: Some(1),
        }, sections)
    }

    fn toc_object(toc: TocFile) -> TocObject {
        TocObject {
            title: toc.title, author: toc.author, toc_url: toc.toc_url,
            story: toc.story, subtitles: toc.subtitles, novel_type: toc.novel_type,
        }
    }

    #[test]
    fn section_render_stream_obeys_backpressure_and_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        let (toc, sections) = streaming_fixture(3, &"あ".repeat(50_000));
        let source: SectionStream = Box::pin(futures::stream::iter(sections).map(move |section| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(section)
        }));
        let converter = NovelConverter::build(NovelSettings {
            enable_inspect: false, ..NovelSettings::default()
        }, None, None);
        block_on(async {
            let mut stream = rendered_sections_stream(converter, toc_object(toc), source, &record(1)).await;
            let header = stream.next().await.unwrap().unwrap();
            assert!(header.len() <= CONVERTED_TEXT_CHUNK_BYTES);
            assert_eq!(reads.load(Ordering::SeqCst), 0);
            for _ in 0..2 {
                let chunk = stream.next().await.unwrap().unwrap();
                assert!(chunk.len() <= CONVERTED_TEXT_CHUNK_BYTES);
                assert_eq!(reads.load(Ordering::SeqCst), 1);
            }
            drop(stream);
            assert_eq!(reads.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn streamed_conversion_preserves_complete_section_regex_and_native_bytes() {
        let body = format!(
            "{}跨ぎ開始\n跨ぎ終わり\n漢字《かんじ》",
            "あ".repeat((CONVERTED_TEXT_CHUNK_BYTES - 7) / 3),
        );
        let (toc, sections) = streaming_fixture(2, &body);
        let user = UserConverter::from_yaml(
            "before:\n  - pattern: '跨ぎ開始\\n跨ぎ終わり'\n    replacement: '境界を越える'\n    text_type: [body]\n",
            &toc.title,
        ).unwrap();
        let target = record(1);
        for style in ["default", "simple", "plain"] {
            let settings = NovelSettings {
                author_comment_style: style.into(), enable_inspect: false,
                enable_auto_join_line: false, ..NovelSettings::default()
            };
            let toc = toc_object(toc.clone());
            let expected = NovelConverter::build(settings.clone(), Some(user.clone()), None)
                .convert_novel(&toc, &sections, Some(&target)).unwrap();
            let source: SectionStream = Box::pin(futures::stream::iter(
                sections.clone().into_iter().map(Ok),
            ));
            let stream = rendered_sections_stream(
                NovelConverter::build(settings, Some(user.clone()), None), toc, source, &target,
            );
            let actual = block_on(async {
                let mut stream = stream.await;
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.unwrap();
                    assert!(chunk.len() <= CONVERTED_TEXT_CHUNK_BYTES);
                    bytes.extend(chunk);
                }
                String::from_utf8(bytes).unwrap()
            });
            assert_eq!(actual, expected, "author comment style {style}");
            assert!(actual.contains("境界を越える"));
            assert!(!actual.contains("跨ぎ開始"));
            assert!(actual.contains("漢字《かんじ》"));
            assert!(actual.contains("前書き") && actual.contains("後書き"));
        }
    }

    #[test]
    fn cached_text_snapshot_is_immutable_replayable_and_explicitly_cleaned() {
        let store = Arc::new(MemoryObjectStore::new());
        let key = ObjectKey::try_new("novels/site/book/novel.txt").unwrap();
        block_on(async {
            store.write_small(&key, b"first generation".to_vec()).await.unwrap();
            let source = ConvertedText::from_stored(store.clone(), key.clone()).await.unwrap().unwrap();
            store.write_small(&key, b"second generation".to_vec()).await.unwrap();
            assert_eq!(collected_text(&source).await, "first generation");
            assert_eq!(collected_text(&source).await, "first generation");
            assert_eq!(source.size, 16);
            source.cleanup().await.unwrap();
            assert!(AssetStore::stat(store.as_ref(), &source.key).await.unwrap().is_none());
            assert_eq!(store.read_small(&key).await.unwrap().unwrap(), b"second generation");
        });
    }

    #[test]
    fn snapshot_replay_rejects_corruption_and_truncation_at_eof() {
        let store = Arc::new(MemoryObjectStore::new());
        let key = ObjectKey::try_new("novels/site/book/novel.txt").unwrap();
        block_on(async {
            store.write_small(&key, b"snapshot".to_vec()).await.unwrap();
            let source = ConvertedText::from_stored(store.clone(), key).await.unwrap().unwrap();
            for (bytes, message) in [
                (&b"tampered"[..], "checksum mismatch"),
                (&b"short"[..], "truncated"),
            ] {
                store.write_small(&source.key, bytes.to_vec()).await.unwrap();
                let mut stream = source.read_stream().await.unwrap();
                let mut failed = false;
                while let Some(chunk) = stream.next().await {
                    if let Err(error) = chunk {
                        assert!(error.to_string().contains(message), "{error}");
                        failed = true;
                        break;
                    }
                }
                assert!(failed, "corruption must not become successful EOF");
                assert!(stream.next().await.is_none());
            }
            source.cleanup().await.unwrap();
        });
    }

    #[test]
    fn failed_section_stream_does_not_replace_existing_output() {
        let _env = KeepTxtEnvGuard::cleared();
        let store = Arc::new(MemoryObjectStore::new());
        let service = ConvertService::new(
            store.clone(), store.clone(),
            Arc::new(crate::application::settings::MemorySettingsStore::new()),
        );
        let target = record(1);
        let keys = NovelObjectKeys::new(&target.sitename, &target.file_title, false).unwrap();
        let (toc, mut sections) = streaming_fixture(2, "本文");
        let source: SectionStream = Box::pin(futures::stream::iter([
            Ok(sections.remove(0)), Err(NarouError::Platform("source generation changed".into())),
        ]));
        block_on(async {
            store.write_small(&keys.converted_text(), b"previous output".to_vec()).await.unwrap();
            let error = service.convert_source(&target, toc, source, true).await.unwrap_err();
            assert!(error.to_string().contains("source generation changed"));
            assert_eq!(
                store.read_small(&keys.converted_text()).await.unwrap().unwrap(),
                b"previous output",
            );
        });
    }

    fn record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: "作者".into(),
            title: "テスト小説".into(),
            file_title: "test".into(),
            toc_url: format!("https://example.com/novel/{id}"),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    /// 保存済みの TOC と本文から変換テキストを組み立て、`novel.txt` に書く。
    /// Worker の `JobKind::Convert` はこの経路をそのまま呼ぶ。
    #[test]
    fn assembles_converted_text_from_stored_sections() {
        let _env = KeepTxtEnvGuard::cleared();
        let store = Arc::new(MemoryObjectStore::new());
        let settings = Arc::new(crate::application::settings::MemorySettingsStore::new().with_local(
            std::collections::HashMap::from([
                ("force.enable_add_date_to_title".into(), serde_yaml::Value::Bool(true)),
                ("force.enable_add_end_to_title".into(), serde_yaml::Value::Bool(true)),
                ("force.title_date_target".into(), serde_yaml::Value::String("general_lastup".into())),
                ("force.title_date_format".into(), serde_yaml::Value::String(" [$ns/%Y-%m-%d]".into())),
            ]),
        ));
        let service = ConvertService::new(store.clone(), store.clone(), settings);
        let mut target = record(7);
        target.tags.push("end".into());
        target.general_lastup = Some("2001-02-03T00:00:00Z".parse().unwrap());
        let keys =
            NovelObjectKeys::new(&target.sitename, &target.file_title, false).unwrap();

        let subtitle = SubtitleInfo {
            index: "1".to_string(),
            href: "https://example.com/novel/7/1".to_string(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "第一話".to_string(),
            file_subtitle: "第一話".to_string(),
            subdate: "2026-09-26 00:00:00".to_string(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        };
        let toc = TocFile {
            title: target.title.clone(),
            author: target.author.clone(),
            toc_url: target.toc_url.clone(),
            story: Some("あらすじ".to_string()),
            subtitles: vec![subtitle.clone()],
            novel_type: Some(1),
        };
        let section = SectionFile {
            index: subtitle.index.clone(),
            href: subtitle.href.clone(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: None,
            download_time: None,
            element: SectionElement {
                data_type: "text".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: "　本文の一行目。\n　本文の二行目。".to_string(),
            },
            download_revision: 0,
        };

        block_on(async {
            store
                .write_small(&keys.toc(), serialize_toc(&toc).unwrap().into_bytes())
                .await
                .unwrap();
            store
                .write_small(
                    &keys.section(&subtitle.index, &subtitle.file_subtitle),
                    serialize_section(&section).unwrap().into_bytes(),
                )
                .await
                .unwrap();
            let converted = service.convert_and_store(&target).await.unwrap();
            assert_eq!(converted.output, Some(keys.converted_text()));
            let text = collected_text(&converted.source).await;
            assert_eq!(text.lines().next(), Some("テスト小説 [example/2001-02-03] (完結)"));
            assert!(text.contains("本文の一行目。"), "{text}");
            assert!(text.contains("第一話"), "{text}");

            // EPUB 応答が読む固定名のキーに書かれていること。
            let stored = store.read_small(&keys.converted_text()).await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&stored.unwrap()),
                text.as_str(),
                "novel.txt must hold the converted text"
            );
        });

        // 保存が無い小説では失敗する (空の EPUB を配らない)。
        let mut missing = record(8);
        missing.file_title = "missing".to_string();
        assert!(block_on(service.convert_and_store(&missing)).is_err());
    }

    /// 目次にあるのに本文オブジェクトが無い話を含む小説は失敗する。
    /// native `load_sections_from_dir` の `section file not found` と同じく、
    /// 不完全な `novel.txt` を成功として書き出さない。
    #[test]
    fn fails_when_a_toc_section_body_is_missing() {
        let store = Arc::new(MemoryObjectStore::new());
        let settings = Arc::new(crate::application::settings::MemorySettingsStore::new());
        let service = ConvertService::new(store.clone(), store.clone(), settings);
        let target = record(9);
        let keys = NovelObjectKeys::new(&target.sitename, &target.file_title, false).unwrap();

        let subtitle = |index: &str| SubtitleInfo {
            index: index.to_string(),
            href: format!("https://example.com/novel/9/{index}"),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: format!("第{index}話"),
            file_subtitle: format!("第{index}話"),
            subdate: "2026-09-26 00:00:00".to_string(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        };
        let first = subtitle("1");
        let missing = subtitle("2");
        let toc = TocFile {
            title: target.title.clone(),
            author: target.author.clone(),
            toc_url: target.toc_url.clone(),
            story: Some("あらすじ".to_string()),
            subtitles: vec![first.clone(), missing.clone()],
            novel_type: Some(1),
        };
        let section = SectionFile {
            index: first.index.clone(),
            href: first.href.clone(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: first.subtitle.clone(),
            file_subtitle: first.file_subtitle.clone(),
            subdate: first.subdate.clone(),
            subupdate: None,
            download_time: None,
            element: SectionElement {
                data_type: "text".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: "　一話だけ本文がある。".to_string(),
            },
            download_revision: 0,
        };

        block_on(async {
            store
                .write_small(&keys.toc(), serialize_toc(&toc).unwrap().into_bytes())
                .await
                .unwrap();
            // 2 話ある目次に対し本文は 1 話分だけしか保存しない。
            store
                .write_small(
                    &keys.section(&first.index, &first.file_subtitle),
                    serialize_section(&section).unwrap().into_bytes(),
                )
                .await
                .unwrap();

            let error = service.convert_and_store(&target).await.unwrap_err();
            let message = error.to_string();
            let expected = format!("{} {}.yaml", missing.index, missing.file_subtitle);
            assert!(
                message.contains("section file not found"),
                "expected native-style missing-section error, got: {message}"
            );
            assert!(
                message.contains(&expected),
                "error should name the expected section file '{expected}', got: {message}"
            );
            assert!(
                store
                    .read_small(&keys.converted_text())
                    .await
                    .unwrap()
                    .is_none()
            );
        });
    }

    /// 目次の中間の話が欠落している場合、その話を名指しで失敗し、変換結果は
    /// 書かない。欠落を補うと本文は目次順に並ぶ (先読みの完了順に依存しない)。
    #[test]
    fn fails_when_a_middle_toc_section_body_is_missing_then_keeps_toc_order() {
        let store = Arc::new(MemoryObjectStore::new());
        let settings = Arc::new(crate::application::settings::MemorySettingsStore::new());
        let service = ConvertService::new(store.clone(), store.clone(), settings);
        let target = record(11);
        let keys = NovelObjectKeys::new(&target.sitename, &target.file_title, false).unwrap();

        let subtitle = |index: &str| SubtitleInfo {
            index: index.to_string(),
            href: format!("https://example.com/novel/11/{index}"),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: format!("第{index}話"),
            file_subtitle: format!("第{index}話"),
            subdate: "2026-09-26 00:00:00".to_string(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        };
        let first = subtitle("1");
        let missing = subtitle("2");
        let third = subtitle("3");
        let toc = TocFile {
            title: target.title.clone(),
            author: target.author.clone(),
            toc_url: target.toc_url.clone(),
            story: None,
            subtitles: vec![first.clone(), missing.clone(), third.clone()],
            novel_type: Some(1),
        };
        let section = |subtitle: &SubtitleInfo, body: &str| SectionFile {
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
            download_revision: 0,
        };

        block_on(async {
            store
                .write_small(&keys.toc(), serialize_toc(&toc).unwrap().into_bytes())
                .await
                .unwrap();
            // 中間の第 2 話だけ保存しない。
            let first_section = section(&first, "　みかん。");
            store
                .write_small(
                    &keys.section(&first.index, &first.file_subtitle),
                    serialize_section(&first_section).unwrap().into_bytes(),
                )
                .await
                .unwrap();
            let third_section = section(&third, "　ぶどう。");
            store
                .write_small(
                    &keys.section(&third.index, &third.file_subtitle),
                    serialize_section(&third_section).unwrap().into_bytes(),
                )
                .await
                .unwrap();

            let error = service.convert_and_store(&target).await.unwrap_err();
            let message = error.to_string();
            let expected = format!("{} {}.yaml", missing.index, missing.file_subtitle);
            assert!(
                message.contains("section file not found"),
                "expected native-style missing-section error, got: {message}"
            );
            assert!(
                message.contains(&expected),
                "error should name the middle section file '{expected}', got: {message}"
            );
            assert!(
                store
                    .read_small(&keys.converted_text())
                    .await
                    .unwrap()
                    .is_none(),
                "no output before validation"
            );

            // 欠落を補うと変換でき、本文は目次順に並ぶ。
            let missing_section = section(&missing, "　りんご。");
            store
                .write_small(
                    &keys.section(&missing.index, &missing.file_subtitle),
                    serialize_section(&missing_section).unwrap().into_bytes(),
                )
                .await
                .unwrap();
            let converted = service.convert_and_store(&target).await.unwrap();
            let text = collected_text(&converted.source).await;
            let first_at = text.find("みかん").unwrap();
            let second_at = text.find("りんご").unwrap();
            let third_at = text.find("ぶどう").unwrap();
            assert!(
                first_at < second_at && second_at < third_at,
                "sections must keep TOC order: {}",
                text
            );
        });
    }

    /// `convert.keep-txt=false` のとき、変換は成功するが `novel.txt`
    /// オブジェクトは書かれない (EPUB は都度再変換で組み立てる構成)。
    #[test]
    fn keeps_no_converted_text_when_keep_txt_is_disabled() {
        let _env = KeepTxtEnvGuard::cleared();
        let store = Arc::new(MemoryObjectStore::new());
        let settings = Arc::new(crate::application::settings::MemorySettingsStore::new().with_local(
            std::collections::HashMap::from([(
                "convert.keep-txt".to_string(),
                serde_yaml::Value::Bool(false),
            )]),
        ));
        let service = ConvertService::new(store.clone(), store.clone(), settings);
        let target = record(10);
        let keys = NovelObjectKeys::new(&target.sitename, &target.file_title, false).unwrap();

        let subtitle = SubtitleInfo {
            index: "1".into(),
            href: "https://example.com/novel/10/1".into(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "第一話".into(),
            file_subtitle: "第一話".into(),
            subdate: "2026-09-26 00:00:00".into(),
            subupdate: None,
            download_time: None,
            download_revision: 0,
        };
        let toc = TocFile {
            title: target.title.clone(),
            author: target.author.clone(),
            toc_url: target.toc_url.clone(),
            story: None,
            subtitles: vec![subtitle.clone()],
            novel_type: Some(1),
        };
        let section = SectionFile {
            index: subtitle.index.clone(),
            href: subtitle.href.clone(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: None,
            download_time: None,
            element: SectionElement {
                data_type: "text".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: "　本文。".to_string(),
            },
            download_revision: 0,
        };

        block_on(async {
            store
                .write_small(&keys.toc(), serialize_toc(&toc).unwrap().into_bytes())
                .await
                .unwrap();
            store
                .write_small(
                    &keys.section(&subtitle.index, &subtitle.file_subtitle),
                    serialize_section(&section).unwrap().into_bytes(),
                )
                .await
                .unwrap();

            let converted = service.convert_and_store(&target).await.unwrap();
            let text = collected_text(&converted.source).await;
            assert!(text.contains("本文。"), "{text}");
            assert!(converted.output.is_none());
            assert!(
                store
                    .read_small(&keys.converted_text())
                    .await
                    .unwrap()
                    .is_none(),
                "keep-txt=false must not persist converted text"
            );
        });
    }

    /// 自動変換の判定は native/Worker の契約モードを保持する
    /// (どちらも現行の outward 挙動のまま)。
    #[test]
    fn followup_convert_decision_preserves_both_contract_modes() {
        // `--no-convert`: Worker (Everything) は全ステータス、native
        // (SuccessOnly) は `Ok` だけを抑止する。
        for status in [
            UpdateStatus::Ok,
            UpdateStatus::None,
            UpdateStatus::Failed,
            UpdateStatus::Canceled,
        ] {
            assert!(
                !needs_convert_after_download(
                    status,
                    true,
                    NoConvertScope::Everything,
                    true,
                    false,
                    true,
                    true,
                ),
                "{status:?} with --no-convert must not convert (Everything)"
            );
        }
        assert!(!needs_convert_after_download(
            UpdateStatus::Ok,
            true,
            NoConvertScope::SuccessOnly,
            true,
            false,
            false,
            false,
        ));
        assert!(needs_convert_after_download(
            UpdateStatus::None,
            true,
            NoConvertScope::SuccessOnly,
            false,
            false,
            true,
            false,
        ));

        // `Ok`: `convert_only_new_arrival` は新着が無いときだけ抑止する
        // (Download は false 固定)。
        assert!(needs_convert_after_download(
            UpdateStatus::Ok,
            false,
            NoConvertScope::Everything,
            false,
            false,
            false,
            false,
        ));
        assert!(!needs_convert_after_download(
            UpdateStatus::Ok,
            false,
            NoConvertScope::Everything,
            false,
            true,
            false,
            false,
        ));
        assert!(needs_convert_after_download(
            UpdateStatus::Ok,
            false,
            NoConvertScope::Everything,
            true,
            true,
            false,
            false,
        ));

        // `None`: `convert_failure` と `missing_cached_text` のどちらでも変換する。
        assert!(!needs_convert_after_download(
            UpdateStatus::None,
            false,
            NoConvertScope::Everything,
            false,
            false,
            false,
            false,
        ));
        assert!(needs_convert_after_download(
            UpdateStatus::None,
            false,
            NoConvertScope::Everything,
            false,
            false,
            true,
            false,
        ));
        assert!(needs_convert_after_download(
            UpdateStatus::None,
            false,
            NoConvertScope::Everything,
            false,
            false,
            false,
            true,
        ));

        // `Failed` / `Canceled` は常に変換しない。
        for status in [UpdateStatus::Failed, UpdateStatus::Canceled] {
            assert!(!needs_convert_after_download(
                status,
                false,
                NoConvertScope::Everything,
                true,
                false,
                true,
                true,
            ));
        }
    }
}
