//! 変換テキストの組み立て（fs を使わない）。
//!
//! 保存済みの `toc.yaml` と `本文/*.yaml` を ObjectStore から読み、同じ
//! ObjectStore の `<prefix>/novel.txt` へ変換結果を書く。native の
//! `convert_novel_by_id` が書く固定名ミラーと同じキーなので、Worker の
//! EPUB 応答（`download.epub`）がそのまま入力にできる。
//!
//! 小説単位の入力（`setting.ini` / `replace.txt` / `converter.yaml`）も
//! ObjectStore から読む。`default.*` / `force.*` は `SettingsStore`
//! (native=Inventory, Worker=D1) から読んだ値を適用する。

use std::collections::HashMap;
use std::sync::Arc;

use crate::converter::ini::IniData;
use crate::converter::settings::{NovelSettings, parse_replace_patterns};
use crate::converter::user_converter::UserConverter;
use crate::converter::{ConverterCapabilities, NovelConverter};
use crate::db::NovelRecord;
use crate::downloader::persistence::{PersistenceService, SectionLoadFailure};
use crate::downloader::{SectionFile, TocObject};
use crate::error::{NarouError, Result};
use crate::platform::{AssetStore, NovelObjectKeys, ObjectKey, ObjectStore};
use crate::setting_core::SettingScope;

use super::messages::{self, MessageSink, Stream};
use super::settings::SettingsStore;

/// 変換結果。`output` は実際に保存した場合だけ、そのオブジェクトキーを持つ。
#[derive(Debug)]
pub struct ConvertedNovel {
    pub text: String,
    pub output: Option<ObjectKey>,
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
    sink.emit(
        Stream::Stdout,
        &messages::convert::convert_started(TOTAL),
    );
    sink.emit(
        Stream::Stdout,
        &messages::convert::processing(INDEX, TOTAL, target),
    );
    let (completed, report) = match run().await {
        Ok(Some(converted)) => {
            if let Some(output) = &converted.output {
                let filename = output.as_ref().rsplit('/').next().unwrap_or(output.as_ref());
                sink.emit(Stream::Stdout, &messages::convert::output_written(filename));
            }
            sink.emit(
                Stream::Stdout,
                &messages::convert::completed(INDEX, TOTAL, target),
            );
            (
                1,
                ConvertItemReport::Converted {
                    output: converted.output,
                },
            )
        }
        Ok(None) => {
            sink.emit(Stream::Stdout, &messages::convert::id_missing(id));
            (0, ConvertItemReport::Missing)
        }
        Err(error) => {
            sink.emit(
                Stream::Stdout,
                &messages::convert::item_error(INDEX, TOTAL, target, &error),
            );
            (0, ConvertItemReport::Failed { error })
        }
    };
    sink.emit(
        Stream::Stdout,
        &messages::convert::convert_finished(completed, TOTAL),
    );
    report
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
    /// 本文は目次順に全話読む。native `load_sections_from_dir` と同じく、
    /// 目次にあるのに本文が無い話はエラーで止める (不完全な novel.txt を
    /// 成功として書かない)。
    pub async fn convert_and_store(&self, record: &NovelRecord) -> Result<ConvertedNovel> {
        self.convert_inner(record, true).await
    }

    /// 変換だけを行う (`convert_and_store` と同じだが、結果を保存しない)。
    ///
    /// 変換済みテキストを保存しない構成 (`convert.keep-txt=false`) で、
    /// 必要になった時点で組み立て直すために使う。
    pub async fn convert_only(&self, record: &NovelRecord) -> Result<ConvertedNovel> {
        self.convert_inner(record, false).await
    }

    async fn convert_inner(&self, record: &NovelRecord, store: bool) -> Result<ConvertedNovel> {
        let keys = NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        )?;
        let persistence = PersistenceService::new(self.objects.clone(), self.assets.clone());
        let toc = persistence.load_toc(&keys).await?.ok_or_else(|| {
            NarouError::Platform(format!("toc がありません: {}", keys.toc().as_ref()))
        })?;

        // 本文 (目次順) と、小説単位の独立な設定ファイルを同時に読む。
        // エラーの優先順位は直列だったときの読取順 (本文 → setting.ini →
        // 置換 → converter.yaml) のままになるよう、join 後にこの順で評価する。
        let replace_key = keys.replace();
        let (sections, ini, local_settings, replacement, converter_yaml) = futures::join!(
            persistence.load_sections_ordered(&keys, &toc.subtitles),
            self.load_ini(&keys),
            self.settings.load(SettingScope::Local),
            self.read_text(&replace_key),
            async {
                let converter_key =
                    ObjectKey::try_new(format!("{}/converter.yaml", keys.prefix().as_ref()))?;
                self.read_text(&converter_key).await
            },
        );
        let sections: Vec<SectionFile> = match sections {
            Ok(sections) => sections,
            Err(SectionLoadFailure::Missing { section_index }) => {
                let subtitle = &toc.subtitles[section_index];
                // native の `section file not found: expected '{index}
                // {file_subtitle}.yaml' in {section_dir}` に対応する。
                // ObjectStore では見たキー自体が「期待した場所」なので、
                // ファイル名相当部分は実際に探したキーの末尾を出す。
                let section_key = keys.section(&subtitle.index, &subtitle.file_subtitle);
                let key: &str = section_key.as_ref();
                let filename = key.rsplit('/').next().unwrap_or(key);
                return Err(NarouError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "section file not found: expected '{filename}' in {}/本文",
                        keys.prefix().as_ref()
                    ),
                )));
            }
            Err(SectionLoadFailure::Read(error)) => return Err(error),
        };
        if sections.is_empty() {
            return Err(NarouError::Platform(format!(
                "本文がありません: {}",
                keys.prefix().as_ref()
            )));
        }

        let ini = ini?;
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
        if let Some(text) = replacement? {
            // 小説ごとの replace.txt。全体設定側 (インストール先) は native 専用。
            settings.replace_patterns = parse_replace_patterns(&text);
        }
        let user_converter = match converter_yaml? {
            Some(yaml) => {
                UserConverter::from_yaml(&yaml, &settings.title_for_output(&record.title))
            }
            None => None,
        };

        let mut converter =
            NovelConverter::build(settings, user_converter, self.capabilities.clone());
        let toc_object = TocObject {
            title: toc.title.clone(),
            author: toc.author.clone(),
            toc_url: toc.toc_url.clone(),
            story: toc.story.clone(),
            subtitles: toc.subtitles.clone(),
            novel_type: toc.novel_type,
        };
        let text = converter.convert_novel(&toc_object, &sections)?;
        let output = keys.converted_text();
        let output = if store && Self::keep_converted_text(&local_settings) {
            self.objects
                .write_small(&output, text.clone().into_bytes())
                .await?;
            Some(output)
        } else {
            None
        };
        Ok(ConvertedNovel { text, output })
    }

    /// 変換済みテキストを保存するか (`convert.keep-txt`)。
    ///
    /// 環境変数 `NAROU_RS_KEEP_TXT` (0/false/no/off) が最優先、次に local
    /// 設定。未設定は保存する (Worker 側は `worker_entry::convert` が別途
    /// 既定 false を与えるので、このフォールバックは主に native と共有の
    /// テスト/ユースケース向け)。local 設定は変換の入力に使った同じ
    /// スナップショットを見る (このために二度読みしない)。
    fn keep_converted_text(local: &HashMap<String, serde_yaml::Value>) -> bool {
        if let Ok(value) = std::env::var("NAROU_RS_KEEP_TXT") {
            let value = value.trim().to_ascii_lowercase();
            return !matches!(value.as_str(), "0" | "false" | "no" | "off");
        }
        match local.get("convert.keep-txt") {
            Some(serde_yaml::Value::Bool(value)) => *value,
            Some(serde_yaml::Value::String(value)) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            _ => true,
        }
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
        let store = Arc::new(MemoryObjectStore::new());
        let settings = Arc::new(crate::application::settings::MemorySettingsStore::new());
        let service = ConvertService::new(store.clone(), store.clone(), settings);
        let target = record(7);
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
            let text = &converted.text;
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
            let first_at = converted.text.find("みかん").unwrap();
            let second_at = converted.text.find("りんご").unwrap();
            let third_at = converted.text.find("ぶどう").unwrap();
            assert!(
                first_at < second_at && second_at < third_at,
                "sections must keep TOC order: {}",
                converted.text
            );
        });
    }

    /// `convert.keep-txt=false` のとき、変換は成功するが `novel.txt`
    /// オブジェクトは書かれない (EPUB は都度再変換で組み立てる構成)。
    #[test]
    fn keeps_no_converted_text_when_keep_txt_is_disabled() {
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
            assert!(converted.text.contains("本文。"), "{}", converted.text);
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

}
