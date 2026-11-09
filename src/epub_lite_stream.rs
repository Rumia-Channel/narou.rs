//! Bounded asynchronous EPUB preparation. Only metadata and one section are resident.

use std::collections::VecDeque;
use std::ops::Range;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aozora_epub3_lite::{EpubSection, image_reference_occurrences};
use futures::{Stream, StreamExt, TryStreamExt};
use futures::io::{AsyncBufReadExt, BufReader};

use super::*;
use crate::application::convert::ConvertedText;
use crate::illustration_store::IllustrationIndex;
use crate::platform::{AssetStore, AssetStream, StagedAsset};

#[path = "epub_lite_images.rs"]
mod images;
pub use images::{ImageScratch, image_metadata, prepare_image};

#[cfg(not(target_arch = "wasm32"))]
type Lines = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;
#[cfg(target_arch = "wasm32")]
type Lines = Pin<Box<dyn Stream<Item = Result<String>>>>;

async fn text_lines(source: &ConvertedText) -> Result<Lines> {
    let stream = source.read_stream().await?
        .map_err(|error| std::io::Error::other(error.to_string()));
    Ok(Box::pin(BufReader::with_capacity(64 * 1024, stream.into_async_read())
        .lines().map_err(NarouError::Io)))
}

fn conversion_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Conversion(format!("EPUB: {error}"))
}

fn rewrite_line(line: String, index: Option<&IllustrationIndex>) -> String {
    let Some(index) = index else { return line };
    crate::illustration_refs::rewrite(line, |reference| {
        if reference.starts_with("挿絵/") { return Some(reference.to_owned()); }
        index.filename_for_source(reference)
            .or_else(|| crate::illustration_store::mitemin_illustration_id(reference)
                .and_then(|id| index.filename_for_mitemin_id(&id)))
            .map(|filename| format!("挿絵/{filename}"))
    }).0
}

/// The first pass retains only metadata, image names/aliases and configuration.
/// Both passes read the same immutable conversion asset.
pub struct StreamingPreparation {
    input: Input,
    config: AozoraConfig,
    detected: BookMeta,
    references: Vec<String>,
    index: Option<Arc<IllustrationIndex>>,
    cover_setting: Option<&'static str>,
    pub uses_dakuten: bool,
}

impl StreamingPreparation {
    pub async fn scan(
        source: &ConvertedText,
        options: &EpubBuildOptions,
        image_names: Vec<String>,
        index: Option<Arc<IllustrationIndex>>,
    ) -> Result<Self> {
        let mut config = config_for(options.assets_dir.as_deref());
        apply_rotate_image(&mut config, options.rotate_image);
        config.vertical = options.vertical;
        config.image_alt_map.clear();
        let mut metadata = aozora_epub3_lite::metadata::MetadataScanner::new(TitleType::TitleAuthor, false);
        let mut lines = text_lines(source).await?;
        let mut references = Vec::new();
        let mut uses_dakuten = false;
        while let Some(line) = lines.next().await {
            let line = rewrite_line(line?, index.as_deref());
            metadata.push_line(&line, &config.gaiji);
            collect_image_alts(&line, &mut config);
            uses_dakuten |= text_has_dakuten_marker(&line);
            if !config.no_illust {
                references.extend(image_reference_occurrences(&line));
            }
        }
        let input = Input::from_source(Arc::new(EpubTextSource::new(String::new(), image_names)))
            .map_err(conversion_error)?;
        Ok(Self {
            input, config, detected: metadata.finish(), references, index,
            cover_setting: options.cover_from_first_image.then_some("0"), uses_dakuten,
        })
    }

    pub fn references(&self) -> &[String] { &self.references }

    pub fn set_image_names(&mut self, names: Vec<String>) -> Result<()> {
        self.input = Input::from_source(Arc::new(EpubTextSource::new(String::new(), names)))
            .map_err(conversion_error)?;
        Ok(())
    }

    pub fn asset_plan(&self) -> Result<AssetPlan<'_>> {
        aozora_epub3_lite::pipeline::plan_assets_from_references(
            &self.input, &self.input.text_entries()[0], self.references.clone(), self.cover_setting, None,
        ).map_err(conversion_error)
    }

    /// Render into an expiry-managed immutable asset, never a Vec of all XHTML.
    /// Reflow keeps the last section until its successor decides whether inline
    /// images join it; chapter remapping is the same as the synchronous pipeline.
    pub async fn finish(
        self,
        source: &ConvertedText,
        store: Arc<dyn AssetStore>,
        assets: Vec<CollectedAsset>,
        cover: Option<String>,
        options: &EpubBuildOptions,
    ) -> Result<StreamingBuild> {
        for reference in &self.references {
            if !assets.iter().any(|asset| asset.references.contains(reference)) {
                crate::application::messages::emit_default_async(
                    crate::application::messages::Stream::Stderr,
                    &crate::application::messages::download::warn_illustration_missing(reference),
                ).await;
            }
        }
        let lines = text_lines(source).await?;
        let completed = Arc::new(Mutex::new(None));
        let state = RenderState {
            renderer: Some(aozora_epub3_lite::text::SectionStream::new(
                self.config.clone(), self.detected.title_line.is_none(),
            )),
            lines,
            config: self.config,
            detected: self.detected,
            references: self.references,
            index: self.index,
            assets,
            cover,
            original_index: 0,
            line_index: 0,
            wrappers: VecDeque::new(),
            pending_lines: VecDeque::new(),
            after_metadata: false,
            raw_metadata: String::new(),
            fragments: VecDeque::new(),
            held: None,
            origins: Vec::new(),
            ranges: Vec::new(),
            image_only: Vec::new(),
            extra_fonts: Vec::new(),
            bytes: Vec::new(),
            byte_offset: 0,
            output_offset: 0,
            records: None,
            completed: completed.clone(),
        };
        let stream: AssetStream = Box::pin(futures::stream::try_unfold(state, |mut state| async move {
            match state.next_chunk().await? {
                Some(chunk) => Ok(Some((chunk, state))),
                None => Ok(None),
            }
        }));
        let fragments = store.stage_stream(stream).await?;
        let rendered = completed.lock().map_err(|_| conversion_error("render state poisoned"))
            .and_then(|mut value| value.take().ok_or_else(|| conversion_error("render stream did not finish")));
        let rendered = match rendered {
            Ok(rendered) => rendered,
            Err(error) => { let _ = store.delete(&fragments.key).await; return Err(error); }
        };
        let Rendered { config, detected, raw_metadata, assets, cover, records, ranges, image_only, mut extra_fonts } = rendered;
        let (title, creator) = resolved_title_creator(options, &detected);
        let title_markup = build_title_markup(&raw_metadata, &detected, &config, title, creator, options.vertical);
        if let Err(error) = append_gaiji_assets(&mut extra_fonts, &config, &[], &title_markup.title,
            title_markup.creator.as_deref(), title_markup.page.as_deref()) {
            let _ = store.delete(&fragments.key).await;
            return Err(conversion_error(error));
        }
        let cover = cover.map(|path| {
            let dimensions = assets.iter().find(|asset| asset.asset.path == path)
                .and_then(|asset| asset.dimensions).map(|value| (value.width, value.height));
            (path, dimensions)
        });
        let mut images = Vec::with_capacity(assets.len());
        let mut epub_assets = Vec::with_capacity(assets.len() + extra_fonts.len() + options.extra_assets.len());
        for asset in assets {
            images.push(EpubImageSource {
                epub_path: asset.asset.path.clone(), source: asset.source,
                media_type: asset.asset.media_type.clone(), is_cover: asset.is_cover, rotate: asset.rotate,
            });
            epub_assets.push(asset.asset);
        }
        epub_assets.extend(extra_fonts);
        append_extra_assets(&mut epub_assets, &options.extra_assets);
        let section_count = ranges.len();
        let mut book = finish_book(&config, options, title, creator, BookContent {
            sections: vec![String::new(); section_count], records, assets: epub_assets,
        }, cover, Some(title_markup));
        // finish_book may insert its small title page before the deferred body.
        let body_start = book.sections.len().saturating_sub(section_count);
        for (section, image_only) in book.sections[body_start..].iter_mut().zip(image_only) {
            *section = EpubSection::deferred(image_only);
        }
        images.sort_unstable_by(|left, right| left.epub_path.cmp(&right.epub_path));
        Ok(StreamingBuild { book, images, config, fragments, ranges, body_start })
    }
}

pub struct StreamingBuild {
    pub book: EpubBook,
    pub images: Vec<EpubImageSource>,
    pub config: AozoraConfig,
    pub fragments: StagedAsset,
    pub ranges: Vec<Range<u64>>,
    pub body_start: usize,
}

struct Rendered {
    config: AozoraConfig,
    detected: BookMeta,
    raw_metadata: String,
    assets: Vec<CollectedAsset>,
    cover: Option<String>,
    records: Vec<ChapterRecord>,
    ranges: Vec<Range<u64>>,
    image_only: Vec<bool>,
    extra_fonts: Vec<EpubAsset>,
}

struct RenderState {
    renderer: Option<aozora_epub3_lite::text::SectionStream>,
    lines: Lines,
    config: AozoraConfig,
    detected: BookMeta,
    references: Vec<String>,
    index: Option<Arc<IllustrationIndex>>,
    assets: Vec<CollectedAsset>,
    cover: Option<String>,
    original_index: usize,
    line_index: usize,
    wrappers: VecDeque<String>,
    pending_lines: VecDeque<String>,
    after_metadata: bool,
    raw_metadata: String,
    fragments: VecDeque<(usize, String)>,
    held: Option<(usize, String)>,
    origins: Vec<usize>,
    ranges: Vec<Range<u64>>,
    image_only: Vec<bool>,
    extra_fonts: Vec<EpubAsset>,
    bytes: Vec<u8>,
    byte_offset: usize,
    output_offset: u64,
    records: Option<Vec<ChapterRecord>>,
    completed: Arc<Mutex<Option<Rendered>>>,
}

impl RenderState {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if self.byte_offset < self.bytes.len() {
                let end = (self.byte_offset + 64 * 1024).min(self.bytes.len());
                let chunk = self.bytes[self.byte_offset..end].to_vec();
                self.byte_offset = end;
                if end == self.bytes.len() { self.bytes.clear(); self.byte_offset = 0; }
                return Ok(Some(chunk));
            }
            if let Some((origin, fragment)) = self.fragments.pop_front() {
                self.origins.push(origin);
                self.image_only.push(fragment.trim().starts_with("<svg ") && fragment.trim().ends_with("</svg>"));
                append_gaiji_assets(&mut self.extra_fonts, &self.config, std::slice::from_ref(&fragment), "", None, None)
                    .map_err(conversion_error)?;
                let end = self.output_offset.checked_add(fragment.len() as u64)
                    .ok_or_else(|| conversion_error("section length overflow"))?;
                self.ranges.push(self.output_offset..end);
                self.output_offset = end;
                self.bytes = fragment.into_bytes();
                continue;
            }
            if self.renderer.is_none() {
                let mut records = self.records.take().unwrap_or_default();
                for record in &mut records {
                    if let Some(index) = self.origins.iter().position(|origin| *origin == record.section_index) {
                        record.section_index = index;
                    }
                }
                // Raw title text starts at meta_line_start, not the original
                // document's leading blank/comment lines. Rebase only metadata.
                let mut detected = self.detected.clone();
                let start = detected.meta_line_start.unwrap_or(0);
                for line in [&mut detected.title_line, &mut detected.creator_line, &mut detected.publisher_line,
                    &mut detected.subtitle_line, &mut detected.meta_line_start, &mut detected.title_end_line] {
                    *line = line.map(|value| value.saturating_sub(start));
                }
                *self.completed.lock().map_err(|_| conversion_error("render state poisoned"))? = Some(Rendered {
                    config: std::mem::take(&mut self.config), detected,
                    raw_metadata: std::mem::take(&mut self.raw_metadata), assets: std::mem::take(&mut self.assets),
                    cover: self.cover.take(), records, ranges: std::mem::take(&mut self.ranges),
                    image_only: std::mem::take(&mut self.image_only), extra_fonts: std::mem::take(&mut self.extra_fonts),
                });
                return Ok(None);
            }
            let line = if let Some(line) = self.pending_lines.pop_front() {
                Some(line)
            } else {
                self.next_body_line().await?
            };
            let mut emitted = Vec::new();
            match line {
                Some(line) => self.renderer.as_mut().expect("renderer exists")
                    .push_line(&line, &mut |fragment| emitted.push(fragment)).map_err(conversion_error)?,
                None => {
                    let mut renderer = self.renderer.take().expect("renderer exists");
                    self.records = Some(renderer.finish(&mut |fragment| emitted.push(fragment)).map_err(conversion_error)?);
                }
            }
            for fragment in emitted { self.layout(fragment); }
            if self.renderer.is_none() {
                if let Some(fragment) = self.held.take() { self.fragments.push_back(fragment); }
                // EpubBook's historical empty-input representation is one empty
                // section. Keep it deferred too, so the writer asks for it.
                if self.fragments.is_empty() && self.ranges.is_empty() {
                    self.fragments.push_back((0, String::new()));
                }
            }
        }
    }

    async fn next_body_line(&mut self) -> Result<Option<String>> {
        loop {
            let Some(line) = self.lines.next().await else {
                return Ok(self.wrappers.pop_front());
            };
            let line = rewrite_line(line?, self.index.as_deref());
            let index = self.line_index;
            self.line_index += 1;
            if let (Some(start), Some(end)) = (self.detected.meta_line_start, self.detected.title_end_line) {
                if (start..=end).contains(&index) {
                    self.wrappers.clear();
                    self.raw_metadata.push_str(&line);
                    self.raw_metadata.push('\n');
                    self.after_metadata = true;
                    continue;
                }
                let wrapper = line.trim().starts_with("［＃ここから") || line.trim().starts_with("［＃ここで");
                if index < start && wrapper { self.wrappers.push_back(line); continue; }
                if index > end && self.after_metadata && wrapper { continue; }
                if index > end { self.after_metadata = false; }
            }
            if !self.wrappers.is_empty() {
                self.pending_lines.extend(self.wrappers.drain(..));
                self.pending_lines.push_back(line);
                return Ok(self.pending_lines.pop_front());
            }
            return Ok(Some(line));
        }
    }

    fn layout(&mut self, fragment: String) {
        let origin = self.original_index;
        self.original_index += 1;
        let mut sections = [fragment];
        decorate_image_tags(&mut sections, &mut self.assets, &self.config, false);
        for asset in &self.assets {
            for reference in &asset.references {
                if asset.resolved != *reference { rewrite_image_source(&mut sections, reference, &asset.resolved); }
            }
        }
        let resolved = self.assets.iter().flat_map(|asset| asset.references.iter().cloned()).collect::<Vec<_>>();
        remove_missing_image_sources(&mut sections, &self.references, &resolved);
        remove_promoted_cover(&mut sections, &self.config, self.cover.as_deref());
        for piece in aozora_epub3_lite::pipeline::split_image_page_sections(&sections[0], &self.assets, &self.config) {
            if piece.trim().is_empty() || aozora_epub3_lite::pipeline::is_page_marker_only_fragment(&piece) { continue; }
            let inline = aozora_epub3_lite::pipeline::standalone_image_page_type(&piece, &self.assets, &self.config)
                == Some(aozora_epub3_lite::pipeline::ImagePageType::Inline);
            if inline && let Some((_, previous)) = &mut self.held {
                previous.push_str(&piece);
            } else {
                if let Some(previous) = self.held.replace((origin, piece)) { self.fragments.push_back(previous); }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{ObjectKey, mocks::MemoryObjectStore, staging::owned_bytes_stream};

    fn options() -> EpubBuildOptions {
        EpubBuildOptions {
            title: "題名".into(), author: "著者".into(), vertical: true,
            cover_from_first_image: false, assets_dir: None, kindle: false,
            extra_assets: Vec::new(), rotate_image: None,
        }
    }

    async fn source(store: Arc<MemoryObjectStore>, text: String) -> ConvertedText {
        let key = ObjectKey::try_new("fixture/novel.txt").unwrap();
        store.write_stream(&key, owned_bytes_stream(text.into_bytes())).await.unwrap();
        ConvertedText::from_stored(store, key).await.unwrap().unwrap()
    }

    async fn fragments(store: &dyn AssetStore, build: &StreamingBuild) -> Vec<String> {
        let mut output = Vec::new();
        for range in &build.ranges {
            let mut position = range.start;
            let mut bytes = Vec::new();
            while position < range.end {
                let end = (position + 64 * 1024).min(range.end);
                bytes.extend(store.read_range(&build.fragments.key, position..end).await.unwrap().unwrap());
                position = end;
            }
            output.push(String::from_utf8(bytes).unwrap());
        }
        output
    }

    #[test]
    fn staged_sections_match_native_markup_chapters_and_metadata() {
        futures::executor::block_on(async {
            let text = "題名\n著者\n\n［＃改ページ］\n［＃大見出し］\n章の名前\n［＃大見出し終わり］\n本文の｜漢字《かんじ》。\n［＃改ページ］\n後の章と末尾。\n";
            let store = Arc::new(MemoryObjectStore::new());
            let source = source(store.clone(), text.into()).await;
            let options = options();
            let preparation = StreamingPreparation::scan(&source, &options, Vec::new(), None).await.unwrap();
            let (assets, cover) = preparation.asset_plan().unwrap().finish(|_| unreachable!()).unwrap();
            let build = preparation.finish(&source, store.clone(), assets, cover, &options).await.unwrap();
            let original = super::super::prepare_book_from_source(
                Arc::new(EpubTextSource::new(text.into(), Vec::new())), &options,
            ).unwrap().finish(Vec::new(), None, &options).unwrap();
            let actual = fragments(store.as_ref(), &build).await;
            let expected = original.book.sections[build.body_start..].iter().map(|section| section.body_fragment.clone()).collect::<Vec<_>>();
            assert_eq!(actual, expected);
            assert_eq!(build.book.chapters, original.book.chapters);
            assert!(build.book.sections[build.body_start..].iter().all(|section| section.body_fragment.is_empty()));
            source.cleanup().await.unwrap();
            store.delete(&build.fragments.key).await.unwrap();
        });
    }

    #[test]
    fn long_book_keeps_every_section_without_retaining_xhtml_bodies() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            let mut text = String::from("題名\n著者\n\n");
            for index in 0..300 {
                text.push_str(&format!("［＃改ページ］\n第{index}話の前書き。\n{}\n第{index}話の後書き。\n", "長い本文。".repeat(300)));
            }
            let source = source(store.clone(), text).await;
            let options = options();
            let preparation = StreamingPreparation::scan(&source, &options, Vec::new(), None).await.unwrap();
            let (assets, cover) = preparation.asset_plan().unwrap().finish(|_| unreachable!()).unwrap();
            let build = preparation.finish(&source, store.clone(), assets, cover, &options).await.unwrap();
            assert!(build.ranges.len() >= 300);
            assert!(build.book.sections[build.body_start..].iter().all(|section| section.body_fragment.is_empty()));
            let fragments = fragments(store.as_ref(), &build).await;
            let config = embedded_config();
            for index in 0..300 {
                let introduction = inline_to_xhtml(&format!("第{index}話の前書き。"), &config);
                let postscript = inline_to_xhtml(&format!("第{index}話の後書き。"), &config);
                assert!(fragments.iter().any(|fragment| fragment.contains(&introduction)));
                assert!(fragments.iter().any(|fragment| fragment.contains(&postscript)));
            }
            source.cleanup().await.unwrap();
            store.delete(&build.fragments.key).await.unwrap();
        });
    }
}
