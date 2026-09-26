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

use std::sync::Arc;

use crate::converter::ini::IniData;
use crate::converter::settings::{NovelSettings, parse_replace_patterns};
use crate::converter::user_converter::UserConverter;
use crate::converter::{ConverterCapabilities, NovelConverter};
use crate::db::NovelRecord;
use crate::downloader::persistence::PersistenceService;
use crate::downloader::{SectionFile, TocObject};
use crate::error::{NarouError, Result};
use crate::platform::{AssetStore, NovelObjectKeys, ObjectKey, ObjectStore};
use crate::setting_core::SettingScope;

use super::messages::{self, MessageSink, Stream};
use super::settings::SettingsStore;

/// `convert_and_store` の結果。変換テキスト本体と、書き出したオブジェクト
/// キー (native の出力ファイルパス相当 — Worker では `<prefix>/novel.txt`)。
#[derive(Debug)]
pub struct ConvertedNovel {
    pub text: String,
    pub output: ObjectKey,
}

/// 1 件の変換結果。native `cmd_convert` の per-target 分岐
/// (`src/commands/convert.rs`) に対応する。
#[derive(Debug)]
pub enum ConvertItemReport {
    /// 変換テキストが `output` へ書き出された (`{name} を出力しました`)。
    Written { output: ObjectKey },
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
/// コピー/送信・inspect の行は Worker の変換 (保存済み本文から `novel.txt`
/// を書くだけ) に存在しないので出さない。`{filename}` は native の
/// `Path::file_name` 相当で、キーの末尾コンポーネントを使う。
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
            let filename = converted
                .output
                .as_ref()
                .rsplit('/')
                .next()
                .unwrap_or_else(|| converted.output.as_ref());
            sink.emit(
                Stream::Stdout,
                &messages::convert::output_written(filename),
            );
            sink.emit(
                Stream::Stdout,
                &messages::convert::completed(INDEX, TOTAL, target),
            );
            (
                1,
                ConvertItemReport::Written {
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
    /// 本文は目次順に、保存されているセクションだけを読む (未取得の話は飛ばす)。
    pub async fn convert_and_store(&self, record: &NovelRecord) -> Result<ConvertedNovel> {
        let keys = NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        )?;
        let persistence = PersistenceService::new(self.objects.clone(), self.assets.clone());
        let toc = persistence.load_toc(&keys).await?.ok_or_else(|| {
            NarouError::Platform(format!("toc がありません: {}", keys.toc().as_ref()))
        })?;

        let names = persistence.list_novel_object_names(&keys).await?;
        let stored = PersistenceService::existing_section_indices(&names);
        let mut sections: Vec<SectionFile> = Vec::new();
        for subtitle in &toc.subtitles {
            if !stored.contains(&subtitle.index) {
                continue;
            }
            if let Some(section) = persistence.load_section(&keys, subtitle).await? {
                sections.push(section);
            }
        }
        if sections.is_empty() {
            return Err(NarouError::Platform(format!(
                "本文がありません: {}",
                keys.prefix().as_ref()
            )));
        }

        let mut settings = NovelSettings::from_sources(
            Some(record.id),
            &record.title,
            &record.author,
            &self.load_ini(&keys).await?,
            &self
                .settings
                .load(SettingScope::Local)
                .await
                .unwrap_or_default(),
            false,
            false,
        );
        if let Some(text) = self.read_text(&keys.replace()).await? {
            // 小説ごとの replace.txt。全体設定側 (インストール先) は native 専用。
            settings.replace_patterns = parse_replace_patterns(&text);
        }
        let converter_key = crate::platform::ObjectKey::try_new(format!(
            "{}/converter.yaml",
            keys.prefix().as_ref()
        ))?;
        let user_converter = match self.read_text(&converter_key).await? {
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
        self.objects
            .write_small(&output, text.clone().into_bytes())
            .await?;
        Ok(ConvertedNovel { text, output })
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
            assert_eq!(converted.output, keys.converted_text());
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

    /// `emit_convert_item_lines` が native `narou convert <id>` と同じ行を
    /// 同じ順序で sink へ送ることを固定する (Worker の convert ジョブが
    /// 共有実装を通じて出すコンソール行)。
    mod console_lines {
        use super::{ConvertedNovel, ConvertItemReport, emit_convert_item_lines};
        use crate::application::messages::{MessageSink, Stream};
        use crate::error::NarouError;
        use crate::platform::ObjectKey;
        use futures::executor::block_on;
        use parking_lot::Mutex;

        struct Lines(Mutex<Vec<(Stream, String)>>);

        impl MessageSink for Lines {
            fn emit(&self, stream: Stream, text: &str) {
                self.0.lock().push((stream, text.to_string()));
            }
        }

        fn take(lines: &Lines) -> Vec<(Stream, String)> {
            std::mem::take(&mut *lines.0.lock())
        }

        fn converted(output: &str) -> ConvertedNovel {
            ConvertedNovel {
                text: String::new(),
                output: ObjectKey::try_new(output).unwrap(),
            }
        }

        #[test]
        fn emits_native_lines_in_order_on_success() {
            let sink = Lines(Mutex::new(Vec::new()));
            let report = block_on(emit_convert_item_lines(&sink, "7", 7, || async {
                Ok(Some(converted("example/test/novel.txt")))
            }));
            assert!(matches!(report, ConvertItemReport::Written { .. }));
            assert_eq!(
                take(&sink),
                vec![
                    (Stream::Stdout, "変換処理開始: 1件の小説を処理します".to_string()),
                    (Stream::Stdout, "[1/1] 処理中: 7".to_string()),
                    (Stream::Stdout, "novel.txt を出力しました".to_string()),
                    (Stream::Stdout, "[1/1] 完了: 7".to_string()),
                    (Stream::Stdout, "変換処理完了: 1/1件が正常に変換されました".to_string()),
                ]
            );
        }

        #[test]
        fn emits_id_missing_lines_for_unknown_record() {
            let sink = Lines(Mutex::new(Vec::new()));
            let report = block_on(emit_convert_item_lines(&sink, "999", 999, || async {
                Ok(None)
            }));
            assert!(matches!(report, ConvertItemReport::Missing));
            assert_eq!(
                take(&sink),
                vec![
                    (Stream::Stdout, "変換処理開始: 1件の小説を処理します".to_string()),
                    (Stream::Stdout, "[1/1] 処理中: 999".to_string()),
                    (Stream::Stdout, "  Error: ID: 999 は存在しません".to_string()),
                    (Stream::Stdout, "変換処理完了: 0/1件が正常に変換されました".to_string()),
                ]
            );
        }

        #[test]
        fn emits_item_error_lines_on_failure() {
            let sink = Lines(Mutex::new(Vec::new()));
            let report = block_on(emit_convert_item_lines(&sink, "7", 7, || async {
                Err(NarouError::Conversion("broken converter".to_string()))
            }));
            assert!(matches!(report, ConvertItemReport::Failed { .. }));
            assert_eq!(
                take(&sink),
                vec![
                    (Stream::Stdout, "変換処理開始: 1件の小説を処理します".to_string()),
                    (Stream::Stdout, "[1/1] 処理中: 7".to_string()),
                    (
                        Stream::Stdout,
                        "[1/1] エラー: 7 - Conversion error: broken converter".to_string(),
                    ),
                    (Stream::Stdout, "変換処理完了: 0/1件が正常に変換されました".to_string()),
                ]
            );
        }
    }
}
