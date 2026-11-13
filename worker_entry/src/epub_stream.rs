//! EPUB HTTP boundary: one heavy permit, immutable stages, bounded ZIP writes.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use futures::StreamExt;
use sha2::{Digest, Sha256};
use narou_rs::application::convert::ConvertedText;
use narou_rs::epub_lite::{self, ChunkSink, EpubStreamWriter, StreamingBuild, StreamingPreparation};
use narou_rs::platform::{AssetStore, AssetStream, ObjectKey, StagedAsset};
use worker::{Env, Response, Result};

use crate::{epub_download_filename, epub_image_key, load_global_settings_map, load_illustration_index,
    load_local_settings_map, load_novel_settings, report_missing_illustrations, sanitize_content_disposition, setting_text, stream_error};

const CHUNK: usize = 64 * 1024;
const MAX_IMAGES: usize = 512;

pub(crate) async fn build_epub_response(env: &Env, id: i64, virtual_only: bool) -> Result<Response> {
    let Some(permit) = crate::memory_budget::try_heavy() else { return crate::memory_budget::busy_response(); };
    let services = match crate::composition::build_read_services(env).await {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    let record = match services.app.library.get(narou_rs::platform::NovelId(id)).await {
        Ok(Some(record)) => record,
        Ok(None) => return Response::error("Not Found", 404),
        Err(error) => return Response::error(format!("Repository error: {error}"), 500),
    };
    let is_virtual = record.ncode.as_deref().is_some_and(narou_rs::virtual_novel::is_virtual_ncode);
    if virtual_only && !is_virtual { return Response::error("Not Found", 404); }
    let keys = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).map_err(stream_error)?;
    let local_map = load_local_settings_map(env).await;
    let global_map = load_global_settings_map(env).await;
    let push_sink = crate::push_hub::PushHubSink::shared(crate::push_hub::PushHubClient::new(env, crate::budget::SubrequestBudget::new()), None);
    let debug = narou_rs::application::debug::enabled_in_settings(&local_map);
    let context = narou_rs::application::messages::MessageContext::new(push_sink.clone(), debug);
    push_sink.run(context.scope(async {
        let mut cleanup = StageCleanup { store: services.assets.clone(), keys: Vec::new(), scratch: epub_lite::ImageScratch::default() };
        let cached = if !is_virtual && crate::convert::keep_converted_text_with(&local_map) {
            ConvertedText::from_stored(services.assets.clone(), keys.converted_text()).await.map_err(stream_error)?
        } else { None };
        let mut illustration_sources = BTreeMap::new();
        let (source, settings) = if let Some(source) = cached {
            cleanup.keys.push(source.key.clone());
            let settings = load_novel_settings(&services.objects, &record, &keys, &local_map).await.map_err(stream_error)?;
            (source, settings)
        } else {
            let converted = if is_virtual {
                let persistence = narou_rs::downloader::persistence::PersistenceService::new(services.objects.clone(), services.assets.clone());
                let book = crate::virtual_novels::load_book(&services.db.primary_view(), &record, services.novels.as_ref(), &persistence).await.map_err(stream_error)?;
                illustration_sources = book.illustrations;
                services.conversion.convert_source(&record, book.toc, book.sections, false).await
            } else { services.conversion.convert_only(&record).await }.map_err(stream_error)?;
            cleanup.keys.push(converted.source.key.clone());
            (converted.source, converted.settings)
        };
        let filename = epub_download_filename(&record, &settings, &local_map);
        let index = if is_virtual { None } else { load_illustration_index(&services.objects, &keys).await.map(Arc::new) };
        let illust_prefix = narou_rs::platform::ObjectPrefix::new(format!("{}/挿絵", keys.prefix().as_ref())).map_err(stream_error)?;
        let mut names = Vec::new();
        let mut cover_image = None;
        if settings.enable_illust {
            for filename in narou_rs::converter::render::COVER_IMAGE_NAMES {
                let key = keys.prefix().join(filename).map_err(stream_error)?;
                if services.assets.stat(&key).await.map_err(stream_error)?.is_some() {
                    cover_image = Some(filename);
                    names.push(filename.to_owned());
                    break;
                }
            }
        }
        if !is_virtual {
            let mut request = narou_rs::platform::ObjectListRequest::new(illust_prefix.clone(), 100.try_into().unwrap());
            loop {
                let page = services.objects.list_page(&request).await.map_err(stream_error)?;
                for meta in page.objects {
                    let name = meta.key.as_ref().rsplit('/').next().unwrap_or_default();
                    if !epub_lite::is_supported_image(name) { continue; }
                    if names.len() == MAX_IMAGES { return Response::error(format!("Illustrations exceed the Worker budget ({MAX_IMAGES} images)"), 413); }
                    names.push(format!("挿絵/{name}"));
                }
                match page.next_cursor { Some(cursor) => request.cursor = Some(cursor), None => break }
            }
        }
        let mut options = epub_lite::EpubBuildOptions::from_settings(epub_lite::EpubSettingsInput {
            title: &record.title, author: &record.author, settings: &settings,
            has_cover_image: cover_image.is_some(), extra_assets: Vec::new(), assets_dir: None, kindle: false,
            rotate_image: local_map.get("convert.rotate-image").and_then(setting_text)
                .and_then(|value| narou_rs::converter::settings::ImageRotation::from_setting(&value)),
        });
        let mut preparation = StreamingPreparation::scan(&source, &options, Vec::new(), index, cover_image).await.map_err(stream_error)?;
        let mut probed = std::collections::HashSet::new();
        for reference in preparation.references() {
            if names.iter().any(|name| name == reference) || !reference.starts_with("挿絵/") { continue; }
            // Cache negative results too, without retaining an unbounded set
            // of missing names. References beyond the cache still get probed.
            let duplicate = if probed.len() < MAX_IMAGES {
                !probed.insert(reference.as_str())
            } else {
                probed.contains(reference.as_str())
            };
            if duplicate { continue; }
            let key = epub_image_key(&keys, &illustration_sources, reference).map_err(stream_error)?;
            if services.assets.stat(&key).await.map_err(stream_error)?.is_some() {
                if names.len() == MAX_IMAGES { return Response::error(format!("Illustrations exceed the Worker budget ({MAX_IMAGES} images)"), 413); }
                names.push(reference.clone());
            }
        }
        if settings.enable_illust && !preparation.references().is_empty() && names.is_empty() {
            report_missing_illustrations(env, &services.objects, &keys, illust_prefix.as_ref(), is_virtual).await;
        }
        preparation.set_image_names(names).map_err(stream_error)?;
        options.extra_assets = epub_lite::narou_font_assets(preparation.uses_dakuten,
            epub_lite::epub_font_always(global_map.get("convert.epub-font").and_then(setting_text).as_deref()),
            narou_rs::converter::settings::line_height_from_value(global_map.get("line-height")));
        let mut plan = preparation.asset_plan().map_err(stream_error)?;
        let mut snapshots = BTreeMap::new();
        while let Some(request) = plan.next_request().map_err(stream_error)? {
            let source_name = request.source.to_owned();
            let snapshot = match snapshots.entry(source_name) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let key = epub_image_key(&keys, &illustration_sources, request.source).map_err(stream_error)?;
                    let stream = services.assets.read_stream(&key).await.map_err(stream_error)?
                        .ok_or_else(|| stream_error(format!("missing illustration: {key}")))?;
                    let snapshot = services.assets.stage_stream(stream).await.map_err(stream_error)?;
                    cleanup.keys.push(snapshot.key.clone());
                    entry.insert(snapshot)
                }
            };
            let dimensions = epub_lite::image_metadata(services.assets.clone(), snapshot.key.clone(), snapshot.size, request.media_type).await.map_err(stream_error)?;
            plan.provide_dimensions(dimensions).map_err(stream_error)?;
        }
        let (assets, cover) = plan.complete().map_err(stream_error)?;
        let build = preparation.finish(&source, services.assets.clone(), assets, cover, &options).await.map_err(stream_error)?;
        cleanup.keys.push(build.fragments.key.clone());
        let StreamingBuild { book, images, config, fragments, ranges, body_start } = build;
        let mut image_assets = BTreeMap::new();
        for image in images {
            let mut snapshot = snapshots.get(&image.source).cloned().ok_or_else(|| stream_error(format!("image snapshot missing: {}", image.source)))?;
            if let Some(transformed) = epub_lite::prepare_image(services.assets.clone(), &snapshot, &image.media_type, &config.ini, image.is_cover, image.rotate, cleanup.scratch.clone()).await.map_err(stream_error)? {
                cleanup.keys.push(transformed.key.clone());
                snapshot = transformed;
            }
            image_assets.insert(image.epub_path, snapshot);
        }
        // No second pass can still need converted text after its XHTML snapshot.
        source.cleanup().await.map_err(stream_error)?;
        cleanup.keys.retain(|key| key != &source.key);
        drop(source); drop(options); drop(config); drop(snapshots);
        let sink = ChunkSink::new();
        let writer = book.stream_writer(sink.clone()).map_err(stream_error)?;
        let state = EpubStreamState {
            writer: Some(writer), sink, store: services.assets.clone(), fragments, ranges, body_start,
            fragment_hash: Sha256::new(), fragment_bytes: 0,
            fragment_stream: None, fragment_pending: Vec::new(), fragment_offset: 0, fragments_finished: false,
            images: image_assets, active: None, cleanup,
            permit: Some(permit), context: narou_rs::application::messages::MessageContext::new(push_sink.clone(), debug),
            messages: push_sink.clone(),
        };
        let stream = futures::stream::unfold(state, |mut state| async move {
            let context = state.context.clone();
            let messages = state.messages.clone();
            messages.run(context.scope(async move {
                let next = state.next_chunk().await;
                match next {
                    Ok(Some(chunk)) => Some((Ok(chunk), state)),
                    Ok(None) => None,
                    Err(error) => {
                        state.writer = None; state.active = None;
                        state.fragment_stream = None; state.fragment_pending = Vec::new();
                        let _ = state.cleanup.finish().await;
                        state.permit.take();
                        Some((Err(error), state))
                    }
                }
            })).await
        });
        worker::ResponseBuilder::new().with_header("Content-Type", "application/epub+zip")?
            .with_header("Cache-Control", "no-store")?
            .with_header("Content-Disposition", &sanitize_content_disposition(&filename))?.from_stream(stream)
    })).await
}

struct StageCleanup { store: Arc<dyn AssetStore>, keys: Vec<ObjectKey>, scratch: epub_lite::ImageScratch }
impl StageCleanup {
    async fn finish(&mut self) -> Result<()> {
        self.keys.extend(self.scratch.take_keys());
        let mut failed = Vec::new();
        let mut error = None;
        for key in std::mem::take(&mut self.keys) {
            if let Err(value) = self.store.delete(&key).await { error = Some(stream_error(value)); failed.push(key); }
        }
        self.keys = failed;
        error.map_or(Ok(()), Err)
    }
}
impl Drop for StageCleanup {
    fn drop(&mut self) {
        let mut keys = std::mem::take(&mut self.keys);
        keys.extend(self.scratch.take_keys());
        if keys.is_empty() { return; }
        let store = self.store.clone();
        // Cancellation drops the reader and permit synchronously. Deletion is
        // best-effort here; the durable staging expiry journal is the fallback
        // for failed cleanup, request termination and isolate loss.
        wasm_bindgen_futures::spawn_local(async move {
            for key in keys { if let Err(error) = store.delete(&key).await { worker::console_log!("EPUB stage cleanup {key}: {error}"); } }
        });
    }
}

enum EntryInput {
    Bytes { bytes: Vec<u8>, offset: usize },
    Stream { stream: AssetStream, remaining: u64, hash: Sha256, expected: String },
}
struct EpubStreamState {
    writer: Option<EpubStreamWriter<ChunkSink>>,
    sink: ChunkSink,
    store: Arc<dyn AssetStore>,
    fragments: StagedAsset,
    fragment_hash: Sha256,
    fragment_bytes: u64,
    fragment_stream: Option<AssetStream>,
    fragment_pending: Vec<u8>,
    fragment_offset: usize,
    fragments_finished: bool,
    ranges: Vec<Range<u64>>,
    body_start: usize,
    images: BTreeMap<String, StagedAsset>,
    active: Option<EntryInput>,
    cleanup: StageCleanup,
    permit: Option<crate::memory_budget::MemoryPermit>,
    context: narou_rs::application::messages::MessageContext,
    messages: Arc<crate::push_hub::PushHubSink>,
}
impl EpubStreamState {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let buffered = self.sink.take();
            if !buffered.is_empty() { return Ok(Some(buffered)); }
            if self.writer.is_none() {
                self.cleanup.finish().await?;
                self.permit.take();
                return Ok(None);
            }
            if let Some(active) = &mut self.active {
                let chunk = match active {
                    EntryInput::Bytes { bytes, offset } if *offset < bytes.len() => {
                        let end = (*offset + CHUNK).min(bytes.len());
                        self.writer.as_mut().unwrap().write_chunk(&bytes[*offset..end]).map_err(stream_error)?;
                        *offset = end;
                        continue;
                    }
                    EntryInput::Stream { stream, remaining, hash, expected } => {
                        match stream.next().await {
                            Some(bytes) => {
                                let bytes = bytes.map_err(stream_error)?;
                                if bytes.len() > CHUNK || bytes.len() as u64 > *remaining {
                                    return Err(stream_error("invalid EPUB image stream length"));
                                }
                                *remaining -= bytes.len() as u64;
                                hash.update(&bytes);
                                Some(bytes)
                            }
                            None => {
                                if *remaining != 0 || hex::encode(hash.clone().finalize()) != *expected {
                                    return Err(stream_error("EPUB image stage integrity mismatch"));
                                }
                                None
                            }
                        }
                    }
                    _ => None,
                };
                if let Some(bytes) = chunk { self.writer.as_mut().unwrap().write_chunk(&bytes).map_err(stream_error)?; }
                else { self.active = None; self.writer.as_mut().unwrap().end_current().map_err(stream_error)?; }
                continue;
            }
            let info = self.writer.as_ref().unwrap().next_entry();
            let Some(info) = info else {
                if !self.fragments_finished || self.fragment_bytes != self.fragments.size || hex::encode(self.fragment_hash.clone().finalize()) != self.fragments.sha256 {
                    return Err(stream_error("EPUB fragment stage integrity mismatch"));
                }
                self.writer.take().unwrap().finish().map_err(stream_error)?;
                continue;
            };
            let input = if let Some(path) = &info.asset_path {
                let asset = self.images.get(path).ok_or_else(|| stream_error(format!("unexpected EPUB image: {path}")))?;
                let stream = self.store.read_stream(&asset.key).await.map_err(stream_error)?
                    .ok_or_else(|| stream_error(format!("missing EPUB image stage: {}", asset.key)))?;
                EntryInput::Stream { stream, remaining: asset.size, hash: Sha256::new(), expected: asset.sha256.clone() }
            } else {
                let fragment = if let Some(index) = info.section_index {
                    let range = index.checked_sub(self.body_start).and_then(|index| self.ranges.get(index))
                        .ok_or_else(|| stream_error("EPUB section index mismatch"))?.clone();
                    Some(self.read_fragment(range).await?)
                } else { None };
                let bytes = self.writer.as_mut().unwrap().render_current(fragment.as_deref()).map_err(stream_error)?;
                EntryInput::Bytes { bytes, offset: 0 }
            };
            self.active = Some(input);
            self.writer.as_mut().unwrap().begin_current().map_err(stream_error)?;
        }
    }
    async fn read_fragment(&mut self, range: Range<u64>) -> Result<String> {
        if range.start != self.fragment_bytes { return Err(stream_error("EPUB fragment replay order mismatch")); }
        if self.fragment_stream.is_none() && !self.fragments_finished {
            self.fragment_stream = Some(self.store.read_stream(&self.fragments.key).await.map_err(stream_error)?
                .ok_or_else(|| stream_error("EPUB fragment stage missing"))?);
        }
        let length = usize::try_from(range.end - range.start).map_err(stream_error)?;
        let mut bytes = Vec::with_capacity(length);
        while bytes.len() < length {
            if self.fragment_offset == self.fragment_pending.len() {
                let chunk = self.fragment_stream.as_mut().ok_or_else(|| stream_error("EPUB fragments ended early"))?
                    .next().await.ok_or_else(|| stream_error("truncated EPUB fragment"))?.map_err(stream_error)?;
                if chunk.len() > CHUNK || self.fragment_bytes + chunk.len() as u64 > self.fragments.size {
                    return Err(stream_error("invalid EPUB fragment stream length"));
                }
                self.fragment_pending = chunk;
                self.fragment_offset = 0;
                if self.fragment_pending.is_empty() { continue; }
            }
            let count = (length - bytes.len()).min(self.fragment_pending.len() - self.fragment_offset);
            let end = self.fragment_offset + count;
            let chunk = &self.fragment_pending[self.fragment_offset..end];
            bytes.extend_from_slice(chunk);
            self.fragment_hash.update(chunk);
            self.fragment_bytes += count as u64;
            self.fragment_offset = end;
        }
        if self.fragment_bytes == self.fragments.size && !self.fragments_finished {
            if self.fragment_offset != self.fragment_pending.len() { return Err(stream_error("extra EPUB fragment bytes")); }
            let mut stream = self.fragment_stream.take().ok_or_else(|| stream_error("EPUB fragment stream missing"))?;
            while let Some(chunk) = stream.next().await {
                if !chunk.map_err(stream_error)?.is_empty() { return Err(stream_error("extra EPUB fragment bytes")); }
            }
            self.fragment_pending.clear();
            self.fragment_offset = 0;
            self.fragments_finished = true;
        }
        String::from_utf8(bytes).map_err(stream_error)
    }
}
