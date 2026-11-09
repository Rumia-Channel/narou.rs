//! Image containers are replayed one encoded/decoded frame at a time.
//! Unchanged images never enter a decoder, so APNG ancillary chunks and frames
//! pass through byte-for-byte. Transforms use a shared animation canvas/crop.

use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;

use aozora_epub3_lite::image::AnimationTransform;
use aozora_epub3_lite::IniSettings;
use futures::{StreamExt, TryStreamExt, io::{AsyncRead, AsyncReadExt}};
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageEncoder, ImageFormat};
#[cfg(test)]
use image::RgbaImage;

use super::{conversion_error, ImageDimensions};
use crate::error::Result;
use crate::platform::{AssetReader, AssetStore, AssetStream, ObjectKey, StagedAsset};

const CHUNK: usize = 64 * 1024;
const FRAME_BYTES: usize = 16 * 1024 * 1024;
#[path = "epub_lite_canvas.rs"]
mod canvas;
#[path = "epub_lite_png_rows.rs"]
mod png_rows;
#[path = "epub_lite_png_forward.rs"]
mod png_forward;
#[path = "epub_lite_webp_stream.rs"]
mod webp_stream;
#[path = "epub_lite_frame_sink.rs"]
mod frame_sink;
use frame_sink::{FrameChunks, FrameSink};
use canvas::Canvas;
pub use canvas::ImageScratch;

#[cfg(not(target_arch = "wasm32"))]
type StreamReader = Pin<Box<dyn AsyncRead + Send>>;
#[cfg(target_arch = "wasm32")]
type StreamReader = Pin<Box<dyn AsyncRead>>;

enum Reader { Range(AssetReader), Stream(StreamReader) }

struct Input {
    reader: Reader,
    position: u64,
    size: u64,
}
impl Input {
    fn new(store: Arc<dyn AssetStore>, key: ObjectKey, size: u64) -> Self {
        Self { reader: Reader::Range(AssetReader::new(store, key, size)), position: 0, size }
    }
    async fn stream(store: Arc<dyn AssetStore>, key: ObjectKey, size: u64) -> Result<Self> {
        let stream = store.read_stream(&key).await?
            .ok_or_else(|| conversion_error("image snapshot missing"))?
            .map_err(|error| std::io::Error::other(error.to_string()));
        Ok(Self { reader: Reader::Stream(Box::pin(stream.into_async_read())), position: 0, size })
    }
    async fn finish(&mut self) -> Result<()> {
        if self.position != self.size { return Err(conversion_error("image stream ended early")); }
        let mut byte = [0];
        let count = match &mut self.reader {
            Reader::Range(reader) => reader.read(&mut byte).await?,
            Reader::Stream(reader) => reader.read(&mut byte).await?,
        };
        if count != 0 { return Err(conversion_error("image stream exceeds declared length")); }
        Ok(())
    }
    async fn bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        if count > FRAME_BYTES || self.position.checked_add(count as u64).is_none_or(|end| end > self.size) {
            return Err(conversion_error("truncated or oversized image frame"));
        }
        let mut bytes = vec![0; count];
        match &mut self.reader {
            Reader::Range(reader) => reader.read_exact(&mut bytes).await?,
            Reader::Stream(reader) => reader.read_exact(&mut bytes).await?,
        }
        self.position += count as u64;
        Ok(bytes)
    }
    async fn skip(&mut self, mut count: u64) -> Result<()> {
        while count != 0 {
            let length = count.min(CHUNK as u64) as usize;
            self.bytes(length).await?;
            count -= length as u64;
        }
        Ok(())
    }
}

fn dimensions(width: u32, height: u32) -> Result<(u32, u32)> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > isize::MAX as u64 / 8 {
        return Err(conversion_error("invalid image dimensions"));
    }
    Ok((width, height))
}

const IMAGE_MEMORY_BUDGET: u64 = 96 * 1024 * 1024;
const STORAGE_PART_BYTES: u64 = 8 * 1024 * 1024;
const CODEC_SCRATCH_BYTES: u64 = 2 * 1024 * 1024;

fn admit_image_memory(bytes: u64) -> Result<()> {
    // One admitted Worker job reserves the other 32 MiB for its runtime,
    // book/ZIP metadata and HTTP bookkeeping. This checks live allocations,
    // not a pixel-count ceiling; streaming and unchanged images need no canvas.
    if cfg!(target_arch = "wasm32") && bytes > IMAGE_MEMORY_BUDGET {
        return Err(conversion_error(format!("image codec needs {bytes} working bytes; Worker image budget is {IMAGE_MEMORY_BUDGET}")));
    }
    Ok(())
}

fn progressive_components(bytes: &[u8]) -> Option<u8> {
    let mut offset = 2;
    while offset < bytes.len() {
        if bytes[offset] != 0xff { return None; }
        while bytes.get(offset) == Some(&0xff) { offset += 1; }
        let marker = *bytes.get(offset)?; offset += 1;
        if matches!(marker, 0xd9 | 0xda) { return None; }
        if matches!(marker, 0x01 | 0xd0..=0xd8) { continue; }
        let length = u16::from_be_bytes([*bytes.get(offset)?, *bytes.get(offset + 1)?]) as usize;
        if length < 2 { return None; }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return matches!(marker, 0xc2 | 0xc6 | 0xca | 0xce).then(|| bytes.get(offset + 7).copied()).flatten();
        }
        offset = offset.checked_add(length)?;
    }
    None
}

fn decode_pixels(bytes: &[u8], format: ImageFormat, retained: u64) -> Result<DynamicImage> {
    // JPEG's adapter owns a compressed-input copy. Progressive JPEG also owns
    // coefficient blocks for the padded source, unlike baseline row decoding.
    let retained = retained + if format == ImageFormat::Jpeg { bytes.len() as u64 } else { 0 };
    admit_image_memory(retained + STORAGE_PART_BYTES + CODEC_SCRATCH_BYTES)?;
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    if cfg!(target_arch = "wasm32") {
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(IMAGE_MEMORY_BUDGET.saturating_sub(retained + STORAGE_PART_BYTES));
        reader.limits(limits);
    }
    let mut decoder = reader.into_decoder().map_err(conversion_error)?;
    let (width, height) = decoder.dimensions();
    let coefficients = if format == ImageFormat::Jpeg {
        progressive_components(bytes).map_or(0, |components| u64::from(width.div_ceil(16) * 16) * u64::from(height.div_ceil(16) * 16) * u64::from(components) * 2)
    } else { 0 };
    let decoded = decoder.total_bytes();
    admit_image_memory(retained + decoded + coefficients + STORAGE_PART_BYTES + CODEC_SCRATCH_BYTES)?;
    if cfg!(target_arch = "wasm32") {
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(IMAGE_MEMORY_BUDGET.saturating_sub(retained + decoded + STORAGE_PART_BYTES));
        decoder.set_limits(limits).map_err(conversion_error)?;
    }
    DynamicImage::from_decoder(decoder).map_err(conversion_error)
}
fn allocation_bytes(image: &DynamicImage) -> u64 {
    match image {
        DynamicImage::ImageLuma8(value) => value.as_raw().capacity() as u64,
        DynamicImage::ImageLumaA8(value) => value.as_raw().capacity() as u64,
        DynamicImage::ImageRgb8(value) => value.as_raw().capacity() as u64,
        DynamicImage::ImageRgba8(value) => value.as_raw().capacity() as u64,
        DynamicImage::ImageLuma16(value) => value.as_raw().capacity() as u64 * 2,
        DynamicImage::ImageLumaA16(value) => value.as_raw().capacity() as u64 * 2,
        DynamicImage::ImageRgb16(value) => value.as_raw().capacity() as u64 * 2,
        DynamicImage::ImageRgba16(value) => value.as_raw().capacity() as u64 * 2,
        DynamicImage::ImageRgb32F(value) => value.as_raw().capacity() as u64 * 4,
        DynamicImage::ImageRgba32F(value) => value.as_raw().capacity() as u64 * 4,
        _ => image.as_bytes().len() as u64,
    }
}
fn frame_output_limit(retained: u64) -> Result<usize> {
    admit_image_memory(retained + STORAGE_PART_BYTES + CODEC_SCRATCH_BYTES)?;
    if cfg!(target_arch = "wasm32") {
        usize::try_from(IMAGE_MEMORY_BUDGET - retained - STORAGE_PART_BYTES - CODEC_SCRATCH_BYTES).map_err(conversion_error)
    } else { Ok(usize::MAX) }
}

fn be32(bytes: &[u8]) -> u32 { u32::from_be_bytes(bytes[..4].try_into().expect("four bytes")) }
fn le32(bytes: &[u8]) -> u32 { u32::from_le_bytes(bytes[..4].try_into().expect("four bytes")) }
fn le24(bytes: &[u8]) -> u32 { u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) }
fn put24(bytes: &mut Vec<u8>, value: u32) { bytes.extend_from_slice(&value.to_le_bytes()[..3]); }

/// Reads headers/marker segments through bounded ranges, including JPEG SOF
/// markers after arbitrarily many APP segments. No full-image metadata read.
pub async fn image_metadata(store: Arc<dyn AssetStore>, key: ObjectKey, size: u64, media: &str) -> Result<Option<ImageDimensions>> {
    let mut input = Input::new(store, key, size);
    let prefix = input.bytes(size.min(32) as usize).await?;
    let value = match media {
        "image/png" if prefix.len() >= 24 && prefix.starts_with(b"\x89PNG\r\n\x1a\n") => Some((be32(&prefix[16..20]), be32(&prefix[20..24]))),
        "image/gif" if prefix.len() >= 10 && (prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a")) => Some((u16::from_le_bytes([prefix[6], prefix[7]]) as u32, u16::from_le_bytes([prefix[8], prefix[9]]) as u32)),
        "image/bmp" if prefix.len() >= 26 && prefix.starts_with(b"BM") => {
            let height = i32::from_le_bytes(prefix[22..26].try_into().unwrap()).unsigned_abs();
            Some((le32(&prefix[18..22]), height))
        }
        "image/webp" if prefix.len() >= 30 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP" => {
            match &prefix[12..16] {
                b"VP8X" => Some((le24(&prefix[24..27]) + 1, le24(&prefix[27..30]) + 1)),
                b"VP8L" if prefix[20] == 0x2f => {
                    let bits = le32(&prefix[21..25]);
                    Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
                }
                b"VP8 " if &prefix[23..26] == b"\x9d\x01\x2a" => Some((u16::from_le_bytes([prefix[26], prefix[27]]) as u32 & 0x3fff, u16::from_le_bytes([prefix[28], prefix[29]]) as u32 & 0x3fff)),
                _ => None,
            }
        }
        "image/jpeg" => {
            use futures::io::AsyncSeekExt;
            let Reader::Range(reader) = &mut input.reader else { unreachable!() };
            reader.seek(std::io::SeekFrom::Start(0)).await?;
            input.position = 0;
            if input.bytes(2).await? != [0xff, 0xd8] { return Ok(None); }
            loop {
                if input.position == size { break None; }
                if input.bytes(1).await?[0] != 0xff { return Err(conversion_error("invalid JPEG marker")); }
                let marker = loop { let byte = input.bytes(1).await?[0]; if byte != 0xff { break byte; } };
                if matches!(marker, 0xd9 | 0xda) { break None; }
                if matches!(marker, 0x01 | 0xd0..=0xd8) { continue; }
                let length = input.bytes(2).await?;
                let length = u16::from_be_bytes([length[0], length[1]]) as usize;
                if length < 2 { return Err(conversion_error("invalid JPEG segment")); }
                if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                    if length < 7 { return Err(conversion_error("invalid JPEG dimensions")); }
                    let header = input.bytes(5).await?;
                    break Some((u16::from_be_bytes([header[3], header[4]]) as u32, u16::from_be_bytes([header[1], header[2]]) as u32));
                }
                input.skip((length - 2) as u64).await?;
            }
        }
        _ => None,
    };
    Ok(value.map(|(width, height)| ImageDimensions { width, height }))
}

/// Returns a staged replacement only when pixels actually change. A two-pass
/// margin scan uses the immutable source; unchanged animations are never decoded.
/// Temporary WebP body stages are deleted explicitly (expiry covers cancellation).
pub async fn prepare_image(store: Arc<dyn AssetStore>, source: &StagedAsset, media: &str, ini: &IniSettings, cover: bool, rotate: i32, scratch: ImageScratch) -> Result<Option<StagedAsset>> {
    let result = prepare_image_inner(store.clone(), source, media, ini, cover, rotate, scratch.clone()).await;
    let keep = result.as_ref().ok().and_then(Option::as_ref).map(|asset| asset.key.clone());
    if let Err(error) = scratch.cleanup(store.as_ref(), keep.as_ref()).await {
        if let Some(key) = keep && store.delete(&key).await.is_ok() { scratch.forget(&key); }
        return Err(error);
    }
    if let Some(key) = keep { scratch.forget(&key); }
    result
}

async fn prepare_image_inner(store: Arc<dyn AssetStore>, source: &StagedAsset, media: &str, ini: &IniSettings, cover: bool, rotate: i32, scratch: ImageScratch) -> Result<Option<StagedAsset>> {
    let dim = image_metadata(store.clone(), source.key.clone(), source.size, media).await?
        .ok_or_else(|| conversion_error("image dimensions missing"))?;
    if !aozora_epub3_lite::image::needs_pixel_transform(ini, cover, rotate, (dim.width, dim.height)) { return Ok(None); }
    dimensions(dim.width, dim.height)?;
    if media == "image/png" && rotate == 0 && (cover || !ini.get_bool("AutoMargin").unwrap_or(false))
        && let Some(stream) = png_forward::try_stream(store.clone(), source, ini, cover).await? {
        let staged = store.stage_stream(stream).await?;
        scratch.remember(staged.key.clone());
        return Ok(Some(staged));
    }
    let mut decoder = Decoder::open(store.clone(), source.key.clone(), source.size, media, scratch.clone()).await?;
    let first = decoder.next().await?.ok_or_else(|| conversion_error("image has no frames"))?;
    let mut transform = AnimationTransform::new(ini, cover, rotate);
    transform.observe(&first.image).map_err(conversion_error)?;
    let first = if !cover && ini.get_bool("AutoMargin").unwrap_or(false) {
        drop(first);
        while let Some(frame) = decoder.next().await? { transform.observe(&frame.image).map_err(conversion_error)?; }
        decoder = Decoder::open(store.clone(), source.key.clone(), source.size, media, scratch.clone()).await?;
        decoder.next().await?.ok_or_else(|| conversion_error("image replay has no frames"))?
    } else { first };
    if media == "image/png" {
        let staged = store.stage_stream(png_rows::stream(decoder, first, transform)?).await?;
        scratch.remember(staged.key.clone());
        return Ok(Some(staged));
    }
    let stream = if media == "image/webp" { webp_stream::stream(decoder, first, transform) }
        else { frame_stream(decoder, first, transform, media, ini).await? };
    let staged = store.stage_stream(stream).await?;
    scratch.remember(staged.key.clone());
    if media != "image/webp" { return Ok(Some(staged)); }
    // RIFF declares its length before the first chunk. The bounded body stage
    // supplies that length without a second encode or a whole-file allocation.
    let length = staged.size.checked_add(4).and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| conversion_error("WebP RIFF length overflow"))?;
    let mut header = b"RIFF".to_vec(); header.extend_from_slice(&length.to_le_bytes()); header.extend_from_slice(b"WEBP");
    let body = store.read_stream(&staged.key).await?.ok_or_else(|| conversion_error("WebP body stage missing"))?;
    let stream: AssetStream = Box::pin(futures::stream::once(async move { Ok(header) }).chain(body));
    let output = store.stage_stream(stream).await?;
    scratch.remember(output.key.clone());
    store.delete(&staged.key).await?;
    scratch.forget(&staged.key);
    Ok(Some(output))
}

async fn frame_stream(mut decoder: Decoder, first: Frame, transform: AnimationTransform, media: &str, ini: &IniSettings) -> Result<AssetStream> {
    let mut output = Encoder::new(media, &decoder, ini)?;
    decoder.park().await?;
    let first = Frame { image: apply_transform(&transform, first.image)?, ..first };
    let chunks = output.frame_chunks(first, decoder.live_bytes())?;
    let state = EncodeState { decoder, transform, output, chunks, done: false };
    Ok(Box::pin(futures::stream::try_unfold(state, |mut state| async move {
        loop {
            if let Some(chunk) = state.chunks.pop_front() { return Ok(Some((chunk, state))); }
            if state.done { return Ok(None); }
            let frame = state.decoder.next().await?;
            state.output.sync_metadata(&state.decoder)?;
            match frame {
                Some(frame) => {
                    state.decoder.park().await?;
                    let frame = Frame { image: apply_transform(&state.transform, frame.image)?, ..frame };
                    state.chunks = state.output.frame_chunks(frame, state.decoder.live_bytes())?;
                }
                None => { state.chunks = state.output.finish_chunks()?; state.done = true; }
            }
        }
    })))
}

fn apply_transform(transform: &AnimationTransform, image: Arc<DynamicImage>) -> Result<Arc<DynamicImage>> {
    let bytes = allocation_bytes(&image);
    let copies = if Arc::strong_count(&image) == 1 { 1 } else { 2 };
    let rotation_bitmap = (u64::from(image.width()) * u64::from(image.height())).div_ceil(8);
    admit_image_memory(bytes * copies + rotation_bitmap + transform.row_memory_bytes().map_err(conversion_error)? + STORAGE_PART_BYTES + CODEC_SCRATCH_BYTES)?;
    let image = match Arc::try_unwrap(image) {
        Ok(image) => transform.apply(image),
        Err(image) => transform.apply_ref(&image),
    }.map_err(conversion_error)?;
    Ok(Arc::new(image))
}

struct EncodeState { decoder: Decoder, transform: AnimationTransform, output: Encoder, chunks: FrameChunks, done: bool }
struct Frame { image: Arc<DynamicImage>, delay_num: u16, delay_den: u16, delay_ms: u32, default_image: bool }
#[derive(Clone, Copy)]
struct Control { x: u32, y: u32, width: u32, height: u32, dispose: u8, blend: bool, delay_num: u16, delay_den: u16, delay_ms: u32 }

enum Decoder { Png(Png), Gif(Gif), Webp(Webp), Static { input: Input, media: ImageFormat, done: bool } }
impl Decoder {
    async fn open(store: Arc<dyn AssetStore>, key: ObjectKey, size: u64, media: &str, scratch: ImageScratch) -> Result<Self> {
        let input = Input::stream(store.clone(), key, size).await?;
        Ok(match media {
            "image/png" => Self::Png(Png::open(input, store, scratch).await?),
            "image/gif" => Self::Gif(Gif::open(input, store, scratch).await?),
            "image/webp" => Self::Webp(Webp::open(input, store, scratch).await?),
            "image/jpeg" => Self::Static { input, media: ImageFormat::Jpeg, done: false },
            "image/bmp" => Self::Static { input, media: ImageFormat::Bmp, done: false },
            _ => return Err(conversion_error("unsupported image transform")),
        })
    }
    fn live_bytes(&self) -> u64 {
        match self {
            Self::Png(value) => value.canvas.as_ref().map_or(0, Canvas::live_bytes),
            Self::Gif(value) => value.canvas.live_bytes(),
            Self::Webp(value) => value.canvas.as_ref().map_or(0, Canvas::live_bytes),
            Self::Static { .. } => 0,
        }
    }
    async fn park(&mut self) -> Result<()> {
        match self {
            Self::Png(value) => if let Some(canvas) = &mut value.canvas { canvas.park().await?; },
            Self::Gif(value) => value.canvas.park().await?,
            Self::Webp(value) => if let Some(canvas) = &mut value.canvas { canvas.park().await?; },
            Self::Static { .. } => {},
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Frame>> {
        match self {
            Self::Png(value) => value.next().await,
            Self::Gif(value) => value.next().await,
            Self::Webp(value) => value.next().await,
            Self::Static { input, media, done } => {
                if *done { return Ok(None); } *done = true;
                let size = usize::try_from(input.size).map_err(conversion_error)?;
                let bytes = input.bytes(size).await?;
                input.finish().await?;
                let image = Arc::new(decode_pixels(&bytes, *media, bytes.capacity() as u64)?);
                Ok(Some(Frame { image, delay_num: 0, delay_den: 100, delay_ms: 0, default_image: false }))
            }
        }
    }
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes()); output.extend_from_slice(kind); output.extend_from_slice(payload);
    let mut crc = crc32fast::Hasher::new(); crc.update(kind); crc.update(payload); output.extend_from_slice(&crc.finalize().to_be_bytes());
}
struct Png { input: Input, header: Vec<u8>, shared: Vec<u8>, canvas: Option<Canvas>, store: Arc<dyn AssetStore>, scratch: ImageScratch, control: Option<Control>, animation: Option<(u32, u32)>, frames: u32, sequence: u32, done: bool }
impl Png {
    async fn open(mut input: Input, store: Arc<dyn AssetStore>, scratch: ImageScratch) -> Result<Self> {
        if input.bytes(8).await? != b"\x89PNG\r\n\x1a\n" { return Err(conversion_error("invalid PNG signature")); }
        let (kind, header) = Self::chunk(&mut input).await?;
        if &kind != b"IHDR" || header.len() != 13 { return Err(conversion_error("invalid PNG header")); }
        dimensions(be32(&header[..4]), be32(&header[4..8]))?;
        Ok(Self { input, header, shared: Vec::new(), canvas: None, store, scratch, control: None, animation: None, frames: 0, sequence: 0, done: false })
    }
    async fn chunk(input: &mut Input) -> Result<([u8; 4], Vec<u8>)> {
        let header = input.bytes(8).await?;
        let length = be32(&header[..4]) as usize;
        let kind: [u8; 4] = header[4..8].try_into().unwrap();
        Self::payload(input, kind, length).await
    }
    async fn payload(input: &mut Input, kind: [u8; 4], length: usize) -> Result<([u8; 4], Vec<u8>)> {
        let limit = match &kind { b"IHDR" => Some(13), b"PLTE" => Some(768), b"tRNS" => Some(256), b"acTL" => Some(8), b"fcTL" => Some(26), b"IEND" => Some(0), _ => None };
        let retain = limit.is_some();
        if limit.is_some_and(|limit| length > limit) { return Err(conversion_error("invalid PNG metadata length")); }
        let mut crc = crc32fast::Hasher::new(); crc.update(&kind);
        let mut payload = Vec::with_capacity(if retain { length } else { 0 });
        let mut left = length;
        while left > 0 { let bytes = input.bytes(left.min(CHUNK)).await?; crc.update(&bytes); if retain { payload.extend_from_slice(&bytes); } left -= bytes.len(); }
        let expected = input.bytes(4).await?;
        if crc.finalize() != be32(&expected) { return Err(conversion_error("PNG chunk checksum mismatch")); }
        Ok((kind, payload))
    }
    fn sequence(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() < 4 || be32(bytes) != self.sequence { return Err(conversion_error("APNG sequence mismatch")); }
        self.sequence = self.sequence.checked_add(1).ok_or_else(|| conversion_error("APNG sequence overflow"))?; Ok(())
    }
    fn reserve_frame(&self, encoded: &mut Vec<u8>, additional: usize) -> Result<()> {
        let required = encoded.len().checked_add(additional).ok_or_else(|| conversion_error("PNG frame size overflow"))?;
        if required > FRAME_BYTES { return Err(conversion_error("encoded PNG frame exceeds memory budget")); }
        if required > encoded.capacity() {
            let capacity = required.max(encoded.capacity().saturating_mul(2)).min(FRAME_BYTES);
            let canvas = self.canvas.as_ref().map_or(0, Canvas::live_bytes);
            admit_image_memory(canvas + encoded.capacity() as u64 + capacity as u64 + CODEC_SCRATCH_BYTES)?;
            encoded.try_reserve_exact(capacity - encoded.len()).map_err(conversion_error)?;
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Frame>> {
        if self.done { if let Some(canvas) = &mut self.canvas { canvas.finish().await?; } return Ok(None); }
        let mut control = self.control.take();
        let mut encoded = Vec::new();
        let mut data = false;
        loop {
            let chunk_header = self.input.bytes(8).await?;
            let length = be32(&chunk_header) as usize;
            let kind: [u8; 4] = chunk_header[4..8].try_into().unwrap();
            if matches!(&kind, b"IDAT" | b"fdAT") {
                let mut original_crc = crc32fast::Hasher::new(); original_crc.update(&kind);
                let length = if &kind == b"fdAT" {
                    if length < 4 { return Err(conversion_error("invalid APNG frame data")); }
                    let sequence = self.input.bytes(4).await?; self.sequence(&sequence)?; original_crc.update(&sequence); length - 4
                } else { length };
                if !data {
                    encoded.extend_from_slice(b"\x89PNG\r\n\x1a\n");
                    let mut header = self.header.clone();
                    if let Some(control) = control { header[..4].copy_from_slice(&control.width.to_be_bytes()); header[4..8].copy_from_slice(&control.height.to_be_bytes()); }
                    png_chunk(&mut encoded, b"IHDR", &header); encoded.extend_from_slice(&self.shared); data = true;
                }
                self.reserve_frame(&mut encoded, length.saturating_add(24))?;
                encoded.extend_from_slice(&(length as u32).to_be_bytes()); encoded.extend_from_slice(b"IDAT");
                let mut output_crc = crc32fast::Hasher::new(); output_crc.update(b"IDAT");
                let mut remaining = length;
                while remaining != 0 {
                    let bytes = self.input.bytes(remaining.min(CHUNK)).await?;
                    remaining -= bytes.len(); original_crc.update(&bytes); output_crc.update(&bytes); encoded.extend_from_slice(&bytes);
                }
                if original_crc.finalize() != be32(&self.input.bytes(4).await?) { return Err(conversion_error("PNG chunk checksum mismatch")); }
                encoded.extend_from_slice(&output_crc.finalize().to_be_bytes());
                continue;
            }
            let (kind, payload) = Self::payload(&mut self.input, kind, length).await?;
            match &kind {
                b"acTL" => {
                    if payload.len() != 8 || self.animation.is_some() || data || be32(&payload) == 0 { return Err(conversion_error("invalid APNG animation control")); }
                    self.animation = Some((be32(&payload), be32(&payload[4..])));
                }
                b"fcTL" => {
                    if payload.len() != 26 || self.animation.is_none() { return Err(conversion_error("invalid APNG frame control")); }
                    self.sequence(&payload)?;
                    self.frames += 1;
                    let num = u16::from_be_bytes([payload[20], payload[21]]); let den = u16::from_be_bytes([payload[22], payload[23]]);
                    if payload[24] > 2 || payload[25] > 1 { return Err(conversion_error("invalid APNG disposal/blend")); }
                    let next = Control { width: be32(&payload[4..]), height: be32(&payload[8..]), x: be32(&payload[12..]), y: be32(&payload[16..]), delay_num: num, delay_den: den, delay_ms: u32::from(num) * 1000 / u32::from(if den == 0 { 100 } else { den }), dispose: payload[24], blend: payload[25] == 1 };
                    dimensions(next.width, next.height)?;
                    if data { self.control = Some(next); break; }
                    control = Some(next);
                }
                b"PLTE" | b"tRNS" => {
                    if self.shared.len() + payload.len() > 4096 || data { return Err(conversion_error("invalid PNG palette")); }
                    png_chunk(&mut self.shared, &kind, &payload);
                }
                b"IEND" => {
                    if !payload.is_empty() || self.input.position != self.input.size || !data { return Err(conversion_error("invalid PNG end")); }
                    if self.animation.is_some_and(|(frames, _)| frames != self.frames) { return Err(conversion_error("APNG frame count mismatch")); }
                    self.input.finish().await?;
                    self.done = true; break;
                }
                _ => {}
            }
        }
        png_chunk(&mut encoded, b"IEND", &[]);
        let default_image = control.is_none() && self.animation.is_some();
        let width = be32(&self.header[..4]); let height = be32(&self.header[4..8]);
        let control = control.unwrap_or(Control { width, height, x: 0, y: 0, dispose: 0, blend: false, delay_num: 0, delay_den: 100, delay_ms: 0 });
        if control.x.checked_add(control.width).is_none_or(|value| value > width)
            || control.y.checked_add(control.height).is_none_or(|value| value > height) { return Err(conversion_error("PNG frame outside canvas")); }
        // Independent full-source frames do not need a compositing allocation.
        // A subsequent restore-previous/partial frame does need the saved canvas.
        let next_independent = self.control.is_none_or(|next| next.x == 0 && next.y == 0 && next.width == width && next.height == height && !next.blend && next.dispose != 2);
        let independent = control.x == 0 && control.y == 0 && control.width == width && control.height == height && !control.blend;
        let image = if default_image || self.animation.is_none() || (independent && next_independent) {
            if let Some(mut canvas) = self.canvas.take() { canvas.finish().await?; }
            Arc::new(decode_pixels(&encoded, ImageFormat::Png, encoded.capacity() as u64)?)
        } else {
            if self.canvas.is_none() {
                self.canvas = Some(Canvas::new(self.store.clone(), self.scratch.clone(), width, height, image::Rgba([0; 4]), self.header[8] == 16)?);
            }
            self.canvas.as_mut().unwrap().png(&encoded, encoded.capacity(), control).await?
        };
        Ok(Some(Frame { image, delay_num: control.delay_num, delay_den: control.delay_den, delay_ms: control.delay_ms, default_image }))
    }
}

struct Gif { input: Input, header: Vec<u8>, canvas: Canvas, control: Vec<u8>, repeat: Option<u16>, done: bool }
impl Gif {
    async fn open(mut input: Input, store: Arc<dyn AssetStore>, scratch: ImageScratch) -> Result<Self> {
        let mut header = input.bytes(13).await?;
        if !header.starts_with(b"GIF87a") && !header.starts_with(b"GIF89a") { return Err(conversion_error("invalid GIF signature")); }
        let width = u16::from_le_bytes([header[6], header[7]]) as u32; let height = u16::from_le_bytes([header[8], header[9]]) as u32;
        if header[10] & 0x80 != 0 { header.extend(input.bytes(3usize << ((header[10] & 7) + 1)).await?); }
        Ok(Self { input, header, canvas: Canvas::new(store, scratch, width, height, image::Rgba([0, 0, 0, 0]), false)?, control: Vec::new(), repeat: None, done: false })
    }
    async fn blocks(input: &mut Input, output: &mut Vec<u8>) -> Result<()> {
        loop { let size = input.bytes(1).await?[0]; if output.len() + size as usize + 1 > FRAME_BYTES { return Err(conversion_error("GIF frame exceeds memory budget")); } output.push(size); if size == 0 { return Ok(()); } output.extend(input.bytes(size as usize).await?); }
    }
    async fn next(&mut self) -> Result<Option<Frame>> {
        if self.done { return Ok(None); }
        loop {
            match self.input.bytes(1).await?[0] {
                0x3b => { self.input.finish().await?; self.canvas.finish().await?; self.done = true; return Ok(None); }
                0x21 => {
                    let kind = self.input.bytes(1).await?[0];
                    let mut extension = vec![0x21, kind]; Self::blocks(&mut self.input, &mut extension).await?;
                    if kind == 0xf9 { if extension.len() != 8 { return Err(conversion_error("invalid GIF control")); } self.control = extension; }
                    else if kind == 0xff && extension.len() >= 19 && (&extension[3..14] == b"NETSCAPE2.0" || &extension[3..14] == b"ANIMEXTS1.0") {
                        self.repeat = Some(u16::from_le_bytes([extension[16], extension[17]]));
                    }
                }
                0x2c => {
                    let descriptor = self.input.bytes(9).await?;
                    let x = u16::from_le_bytes([descriptor[0], descriptor[1]]) as u32; let y = u16::from_le_bytes([descriptor[2], descriptor[3]]) as u32;
                    let width = u16::from_le_bytes([descriptor[4], descriptor[5]]) as u32; let height = u16::from_le_bytes([descriptor[6], descriptor[7]]) as u32;
                    dimensions(width, height)?;
                    let mut encoded = self.header.clone(); encoded[6..8].copy_from_slice(&(width as u16).to_le_bytes()); encoded[8..10].copy_from_slice(&(height as u16).to_le_bytes());
                    encoded.extend_from_slice(&self.control); encoded.push(0x2c);
                    let mut local = descriptor; local[..4].fill(0); encoded.extend_from_slice(&local);
                    if local[8] & 0x80 != 0 { encoded.extend(self.input.bytes(3usize << ((local[8] & 7) + 1)).await?); }
                    encoded.extend(self.input.bytes(1).await?); Self::blocks(&mut self.input, &mut encoded).await?; encoded.push(0x3b);
                    admit_image_memory(self.canvas.live_bytes() + encoded.capacity() as u64 + u64::from(width) * u64::from(height) + CODEC_SCRATCH_BYTES)?;
                    let mut options = gif::DecodeOptions::new(); options.set_color_output(gif::ColorOutput::Indexed);
                    let mut decoder = options.read_info(Cursor::new(encoded)).map_err(conversion_error)?;
                    let global_palette = decoder.global_palette().unwrap_or_default().to_vec();
                    let frame = decoder.read_next_frame().map_err(conversion_error)?.ok_or_else(|| conversion_error("GIF frame missing"))?;
                    let palette = frame.palette.as_deref().unwrap_or(&global_palette);
                    let delay = frame.delay;
                    let disposal = match frame.dispose { gif::DisposalMethod::Background => 1, gif::DisposalMethod::Previous => 2, _ => 0 };
                    let control = Control { x, y, width, height, dispose: disposal, blend: true, delay_num: delay, delay_den: 100, delay_ms: u32::from(delay) * 10 };
                    let image = self.canvas.gif(frame, palette, control).await?; self.control.clear();
                    return Ok(Some(Frame { image, delay_num: delay, delay_den: 100, delay_ms: control.delay_ms, default_image: false }));
                }
                _ => return Err(conversion_error("invalid GIF block")),
            }
        }
    }
}

struct Webp { input: Input, canvas: Option<Canvas>, store: Arc<dyn AssetStore>, scratch: ImageScratch, repeat: u16, animated: bool, done: bool }
impl Webp {
    async fn open(mut input: Input, store: Arc<dyn AssetStore>, scratch: ImageScratch) -> Result<Self> {
        let header = input.bytes(12).await?;
        if &header[..4] != b"RIFF" || &header[8..12] != b"WEBP" || u64::from(le32(&header[4..])) + 8 != input.size { return Err(conversion_error("invalid WebP RIFF header")); }
        Ok(Self { input, canvas: None, store, scratch, repeat: 0, animated: false, done: false })
    }
    async fn next(&mut self) -> Result<Option<Frame>> {
        if self.done { return Ok(None); }
        let mut static_chunks = Vec::new();
        while self.input.position < self.input.size {
            let header = self.input.bytes(8).await?; let length = le32(&header[4..]) as usize;
            let payload = self.input.bytes(length).await?; if !length.is_multiple_of(2) { self.input.bytes(1).await?; }
            match &header[..4] {
                b"VP8X" => {
                    if payload.len() != 10 { return Err(conversion_error("invalid WebP extended header")); }
                    self.animated = payload[0] & 2 != 0;
                    if self.animated {
                        self.canvas = Some(Canvas::new(self.store.clone(), self.scratch.clone(), le24(&payload[4..7]) + 1, le24(&payload[7..10]) + 1, image::Rgba([0, 0, 0, 0]), false)?);
                    } else { riff_chunk(&mut static_chunks, b"VP8X", &payload); }
                }
                b"ANIM" => {
                    if payload.len() != 6 || !self.animated { return Err(conversion_error("invalid WebP animation header")); }
                    self.repeat = u16::from_le_bytes([payload[4], payload[5]]);
                    if let Some(canvas) = &mut self.canvas { canvas.background = image::Rgba([payload[2], payload[1], payload[0], payload[3]]); canvas.clear_background()?; }
                }
                b"ANMF" => {
                    if payload.len() < 16 || !self.animated { return Err(conversion_error("invalid WebP frame")); }
                    let width = le24(&payload[6..9]) + 1; let height = le24(&payload[9..12]) + 1; dimensions(width, height)?;
                    let mut chunks = Vec::new(); let mut extended = vec![0x10, 0, 0, 0]; put24(&mut extended, width - 1); put24(&mut extended, height - 1);
                    riff_chunk(&mut chunks, b"VP8X", &extended); chunks.extend_from_slice(&payload[16..]);
                    let delay = le24(&payload[12..15]);
                    let control = Control { x: le24(&payload[..3]) * 2, y: le24(&payload[3..6]) * 2, width, height, dispose: payload[15] & 1, blend: payload[15] & 2 == 0, delay_num: 0, delay_den: 1000, delay_ms: delay };
                    drop(payload);
                    let image = self.canvas.as_mut().ok_or_else(|| conversion_error("WebP canvas missing"))?.webp(chunks, control).await?;
                    return Ok(Some(Frame { image, delay_num: 0, delay_den: 1000, delay_ms: delay, default_image: false }));
                }
                b"ALPH" | b"VP8 " | b"VP8L" if !self.animated => {
                    if static_chunks.len().saturating_add(payload.len() + 9) > FRAME_BYTES { return Err(conversion_error("WebP frame exceeds memory budget")); }
                    riff_chunk(&mut static_chunks, header[..4].try_into().unwrap(), &payload);
                }
                _ => {}
            }
        }
        self.input.finish().await?;
        self.done = true;
        if self.animated { if let Some(canvas) = &mut self.canvas { canvas.finish().await?; } return Ok(None); }
        if static_chunks.is_empty() { return Err(conversion_error("WebP image missing")); }
        let encoded = riff(static_chunks)?;
        let image = Arc::new(decode_pixels(&encoded, ImageFormat::WebP, encoded.capacity() as u64)?);
        Ok(Some(Frame { image, delay_num: 0, delay_den: 100, delay_ms: 0, default_image: false }))
    }
}
fn riff_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) { output.extend_from_slice(kind); output.extend_from_slice(&(data.len() as u32).to_le_bytes()); output.extend_from_slice(data); if !data.len().is_multiple_of(2) { output.push(0); } }
fn riff(mut chunks: Vec<u8>) -> Result<Vec<u8>> {
    let length = u32::try_from(chunks.len() + 4).map_err(conversion_error)?;
    let mut header = *b"RIFF\0\0\0\0WEBP"; header[4..8].copy_from_slice(&length.to_le_bytes());
    chunks.try_reserve_exact(header.len()).map_err(conversion_error)?;
    drop(chunks.splice(0..0, header));
    Ok(chunks)
}

enum Encoder {
    #[cfg(test)]
    Png { animation: Option<(u32, u32)>, sequence: u32, started: bool },
    Gif { writer: Option<gif::Encoder<FrameSink>>, sink: FrameSink, repeat: Option<u16> },
    #[cfg(test)]
    Webp { animated: bool, repeat: u16, started: bool },
    Static { format: ImageFormat, jpeg_quality: u8 },
}
impl Encoder {
    fn new(media: &str, decoder: &Decoder, ini: &IniSettings) -> Result<Self> {
        Ok(match (media, decoder) {
            #[cfg(test)]
            ("image/png", Decoder::Png(value)) => Self::Png { animation: value.animation, sequence: 0, started: false },
            ("image/gif", Decoder::Gif(value)) => Self::Gif { writer: None, sink: FrameSink::new(), repeat: value.repeat },
            #[cfg(test)]
            ("image/webp", Decoder::Webp(value)) => Self::Webp { animated: value.animated, repeat: value.repeat, started: false },
            ("image/jpeg", _) => Self::Static { format: ImageFormat::Jpeg, jpeg_quality: ini.get("JpegQuality").and_then(|value| value.parse().ok()).unwrap_or(80).clamp(1, 100) },
            ("image/bmp", _) => Self::Static { format: ImageFormat::Bmp, jpeg_quality: 80 },
            _ => return Err(conversion_error("unsupported image encoder")),
        })
    }
    fn sync_metadata(&mut self, decoder: &Decoder) -> Result<()> {
        if let (Self::Gif { writer, repeat, .. }, Decoder::Gif(source)) = (self, decoder)
            && source.repeat != *repeat
        {
            if let (Some(writer), Some(value)) = (writer.as_mut(), source.repeat) {
                writer.set_repeat(if value == 0 { gif::Repeat::Infinite } else { gif::Repeat::Finite(value) })
                    .map_err(conversion_error)?;
            }
            *repeat = source.repeat;
        }
        Ok(())
    }
    #[cfg(test)]
    fn frame(&mut self, frame: Frame) -> Result<Vec<u8>> { Ok(self.frame_chunks(frame, 0)?.into_iter().flatten().collect()) }
    fn frame_chunks(&mut self, frame: Frame, retained: u64) -> Result<FrameChunks> {
        let (width, height) = frame.image.dimensions(); dimensions(width, height)?;
        let image = Arc::try_unwrap(frame.image).map_err(|_| conversion_error("encoded frame still borrowed"))?;
        match self {
            #[cfg(test)]
            Self::Png { animation, sequence, started } => {
                let mut encoded = Cursor::new(Vec::new()); image.write_to(&mut encoded, ImageFormat::Png).map_err(conversion_error)?; let encoded = encoded.into_inner();
                let mut output = Vec::new();
                if !*started { output.extend_from_slice(&encoded[..33]); if let Some((frames, loops)) = animation { let mut control = frames.to_be_bytes().to_vec(); control.extend_from_slice(&loops.to_be_bytes()); png_chunk(&mut output, b"acTL", &control); } }
                if animation.is_some() && !frame.default_image {
                    let mut control = sequence.to_be_bytes().to_vec(); *sequence += 1; control.extend_from_slice(&width.to_be_bytes()); control.extend_from_slice(&height.to_be_bytes()); control.extend_from_slice(&[0; 8]); control.extend_from_slice(&frame.delay_num.to_be_bytes()); control.extend_from_slice(&frame.delay_den.to_be_bytes()); control.extend_from_slice(&[0, 0]); png_chunk(&mut output, b"fcTL", &control);
                }
                let mut offset = 8;
                while offset + 12 <= encoded.len() {
                    let length = be32(&encoded[offset..]) as usize; let end = offset + length + 12; if end > encoded.len() { return Err(conversion_error("encoded PNG truncated")); }
                    if &encoded[offset + 4..offset + 8] == b"IDAT" {
                        if !*started { output.extend_from_slice(&encoded[offset..end]); }
                        else { let mut payload = sequence.to_be_bytes().to_vec(); *sequence += 1; payload.extend_from_slice(&encoded[offset + 8..end - 4]); png_chunk(&mut output, b"fdAT", &payload); }
                    }
                    offset = end;
                }
                *started = true; Ok(frame_sink::test_chunks(output))
            }
            Self::Gif { writer, sink, repeat } => {
                let indices = u64::from(width) * u64::from(height);
                admit_image_memory(retained + allocation_bytes(&image) + indices + CODEC_SCRATCH_BYTES)?;
                sink.set_limit(frame_output_limit(retained + indices)?)?;
                if writer.is_none() {
                    let mut value = gif::Encoder::new(sink.clone(), u16::try_from(width).map_err(conversion_error)?, u16::try_from(height).map_err(conversion_error)?, &[]).map_err(conversion_error)?;
                    if let Some(repeat) = repeat { value.set_repeat(if *repeat == 0 { gif::Repeat::Infinite } else { gif::Repeat::Finite(*repeat) }).map_err(conversion_error)?; }
                    *writer = Some(value);
                }
                let mut pixels = image.into_rgba8().into_raw(); let mut image = gif::Frame::from_rgba_speed(width as u16, height as u16, &mut pixels, 10); image.delay = frame.delay_num; image.dispose = gif::DisposalMethod::Background;
                drop(pixels);
                writer.as_mut().unwrap().write_frame(&image).map_err(conversion_error)?; Ok(sink.take())
            }
            #[cfg(test)]
            Self::Webp { animated, repeat, started } => {
                let mut encoded = Vec::new(); image::codecs::webp::WebPEncoder::new_lossless(&mut encoded).write_image(image.as_bytes(), width, height, image.color().into()).map_err(conversion_error)?;
                drop(image);
                if !*animated { *started = true; encoded.drain(..12); return Ok(frame_sink::test_chunks(encoded)); }
                // Reuse the standalone frame allocation for ANMF. Copying it
                // through payload and container buffers would triple its peak.
                let mut offset = 12; let mut kept = 0;
                while offset + 8 <= encoded.len() {
                    let length = le32(&encoded[offset + 4..]) as usize;
                    let end = offset + 8 + length + length % 2;
                    if end > encoded.len() { return Err(conversion_error("encoded WebP truncated")); }
                    if matches!(&encoded[offset..offset + 4], b"ALPH" | b"VP8 " | b"VP8L") {
                        encoded.copy_within(offset..end, kept); kept += end - offset;
                    }
                    offset = end;
                }
                encoded.truncate(kept);
                let mut prefix = Vec::new();
                if !*started {
                    let mut extended = vec![0x12, 0, 0, 0]; put24(&mut extended, width - 1); put24(&mut extended, height - 1);
                    riff_chunk(&mut prefix, b"VP8X", &extended);
                    let mut control = vec![0; 4]; control.extend_from_slice(&repeat.to_le_bytes()); riff_chunk(&mut prefix, b"ANIM", &control);
                }
                prefix.extend_from_slice(b"ANMF");
                prefix.extend_from_slice(&u32::try_from(encoded.len() + 16).map_err(conversion_error)?.to_le_bytes());
                prefix.extend_from_slice(&[0; 6]); put24(&mut prefix, width - 1); put24(&mut prefix, height - 1); put24(&mut prefix, frame.delay_ms); prefix.push(2);
                encoded.try_reserve_exact(prefix.len()).map_err(conversion_error)?;
                drop(encoded.splice(0..0, prefix));
                *started = true; Ok(frame_sink::test_chunks(encoded))
            }
            Self::Static { format, jpeg_quality } => {
                let mut sink = FrameSink::new();
                sink.set_limit(frame_output_limit(retained + allocation_bytes(&image))?)?;
                if *format == ImageFormat::Jpeg {
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut sink, *jpeg_quality).write_image(image.as_bytes(), width, height, image.color().into()).map_err(conversion_error)?;
                } else {
                    image::codecs::bmp::BmpEncoder::new(&mut sink).write_image(image.as_bytes(), width, height, image.color().into()).map_err(conversion_error)?;
                }
                Ok(sink.take())
            }
        }
    }
    #[cfg(test)]
    fn finish(&mut self) -> Result<Vec<u8>> { Ok(self.finish_chunks()?.into_iter().flatten().collect()) }
    fn finish_chunks(&mut self) -> Result<FrameChunks> {
        match self {
            #[cfg(test)]
            Self::Png { .. } => { let mut output = Vec::new(); png_chunk(&mut output, b"IEND", &[]); Ok(frame_sink::test_chunks(output)) }
            Self::Gif { writer, sink, .. } => { if let Some(writer) = writer.take() { writer.into_inner().map_err(conversion_error)?; } Ok(sink.take()) }
            _ => Ok(FrameChunks::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{mocks::MemoryObjectStore, staging::owned_bytes_stream};

    fn frame(index: u8) -> Frame {
        let mut pixels = RgbaImage::new(2, 1);
        pixels.put_pixel(0, 0, image::Rgba([index, 40, 90, 128]));
        pixels.put_pixel(1, 0, image::Rgba([70, index, 110, 255]));
        Frame {
            image: Arc::new(DynamicImage::ImageRgba8(pixels)), delay_num: 37 + u16::from(index),
            delay_den: 125, delay_ms: 296 + u32::from(index) * 8, default_image: false,
        }
    }

    async fn stage(store: &Arc<MemoryObjectStore>, bytes: Vec<u8>) -> StagedAsset {
        store.stage_stream(owned_bytes_stream(bytes)).await.unwrap()
    }

    #[test]
    fn transformed_apng_retains_every_frame_alpha_delay_and_loop() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            let mut encoder = Encoder::Png { animation: Some((2, 7)), sequence: 0, started: false };
            let mut bytes = encoder.frame(frame(1)).unwrap();
            bytes.extend(encoder.frame(frame(2)).unwrap());
            bytes.extend(encoder.finish().unwrap());
            let source = stage(&store, bytes).await;
            let ini = IniSettings::parse("Gamma=0\nAutoMargin=0\nResizeW=0\nResizeH=0\n").unwrap();
            let output = prepare_image(store.clone(), &source, "image/png", &ini, false, 90, ImageScratch::default()).await.unwrap().unwrap();
            let mut decoder = Decoder::open(store.clone(), output.key.clone(), output.size, "image/png", ImageScratch::default()).await.unwrap();
            for index in 1..=2 {
                let value = decoder.next().await.unwrap().unwrap();
                assert_eq!(value.image.dimensions(), (1, 2));
                assert_eq!((value.delay_num, value.delay_den), (37 + index, 125));
                assert_eq!(value.image.to_rgba8().get_pixel(0, 0).0, [index as u8, 40, 90, 128]);
            }
            assert!(decoder.next().await.unwrap().is_none());
            let Decoder::Png(png) = decoder else { unreachable!() };
            assert_eq!(png.animation, Some((2, 7)));
        });
    }

    #[test]
    fn partial_apng_restores_previous_canvas_before_next_frame() {
        futures::executor::block_on(async {
            use image::Pixel;
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
            encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(3, 2).unwrap(); encoder.set_frame_delay(3, 100).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255].repeat(4)).unwrap();
            writer.set_frame_dimension(1, 1).unwrap(); writer.set_frame_position(0, 0).unwrap();
            writer.set_blend_op(png::BlendOp::Over).unwrap(); writer.set_dispose_op(png::DisposeOp::Previous).unwrap();
            writer.set_frame_delay(7, 100).unwrap(); writer.write_image_data(&[0, 0, 255, 128]).unwrap();
            writer.set_frame_position(1, 1).unwrap(); writer.set_blend_op(png::BlendOp::Source).unwrap();
            writer.set_dispose_op(png::DisposeOp::None).unwrap(); writer.set_frame_delay(11, 100).unwrap();
            writer.write_image_data(&[0, 255, 0, 255]).unwrap(); writer.finish().unwrap();
            let store = Arc::new(MemoryObjectStore::new());
            let source = stage(&store, bytes).await;
            let output = prepare_image(store.clone(), &source, "image/png", &IniSettings::parse("").unwrap(), false, 90, ImageScratch::default()).await.unwrap().unwrap();
            let mut decoder = Decoder::open(store, output.key, output.size, "image/png", ImageScratch::default()).await.unwrap();
            let mut mixed = image::Rgba([255, 0, 0, 255]); mixed.blend(&image::Rgba([0, 0, 255, 128]));
            for (index, delay) in [3, 7, 11].into_iter().enumerate() {
                let frame = decoder.next().await.unwrap().unwrap();
                assert_eq!((frame.delay_num, frame.delay_den), (delay, 100));
                let pixels = frame.image.to_rgba8();
                assert_eq!(pixels.get_pixel(1, 0).0, if index == 1 { mixed.0 } else { [255, 0, 0, 255] });
                assert_eq!(pixels.get_pixel(0, 1).0, if index == 2 { [0, 255, 0, 255] } else { [255, 0, 0, 255] });
            }
            assert!(decoder.next().await.unwrap().is_none());
        });
    }

    #[test]
    fn transformed_apng_keeps_default_image_outside_animation_frame_count() {
        futures::executor::block_on(async {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 7).unwrap(); encoder.set_sep_def_img(true).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[80, 0, 0, 255].repeat(2)).unwrap();
            writer.set_frame_delay(11, 100).unwrap();
            writer.write_image_data(&[0, 90, 0, 128].repeat(2)).unwrap();
            writer.set_frame_delay(13, 100).unwrap();
            writer.write_image_data(&[0, 0, 100, 64].repeat(2)).unwrap();
            writer.finish().unwrap();
            let store = Arc::new(MemoryObjectStore::new());
            let source = stage(&store, bytes).await;
            let output = prepare_image(store.clone(), &source, "image/png", &IniSettings::parse("").unwrap(), false, 90, ImageScratch::default()).await.unwrap().unwrap();
            let mut decoder = Decoder::open(store, output.key, output.size, "image/png", ImageScratch::default()).await.unwrap();
            for (index, color) in [[80, 0, 0, 255], [0, 90, 0, 128], [0, 0, 100, 64]].into_iter().enumerate() {
                let frame = decoder.next().await.unwrap().unwrap();
                assert_eq!(frame.default_image, index == 0);
                assert_eq!(frame.image.dimensions(), (1, 2));
                assert_eq!(frame.image.to_rgba8().get_pixel(0, 0).0, color);
                if index != 0 { assert_eq!((frame.delay_num, frame.delay_den), (9 + index as u16 * 2, 100)); }
            }
            assert!(decoder.next().await.unwrap().is_none());
            let Decoder::Png(png) = decoder else { unreachable!() };
            assert_eq!(png.animation, Some((2, 7)));
        });
    }

    #[test]
    fn corrupt_animation_tail_fails_instead_of_publishing_partial_image() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            let mut encoder = Encoder::Png { animation: Some((2, 0)), sequence: 0, started: false };
            let mut bytes = encoder.frame(frame(1)).unwrap();
            bytes.extend(encoder.frame(frame(2)).unwrap());
            bytes.extend(encoder.finish().unwrap());
            *bytes.last_mut().unwrap() ^= 1;
            let source = stage(&store, bytes).await;
            let objects_before = store.len();
            assert!(prepare_image(store.clone(), &source, "image/png", &IniSettings::parse("").unwrap(), false, 90, ImageScratch::default()).await.is_err());
            assert_eq!(store.len(), objects_before);
        });
    }

    #[test]
    fn gif_and_webp_transforms_keep_animation() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            for media in ["image/gif", "image/webp"] {
                let mut encoder = if media == "image/gif" {
                    Encoder::Gif { writer: None, sink: FrameSink::new(), repeat: Some(3) }
                } else { Encoder::Webp { animated: true, repeat: 3, started: false } };
                let mut bytes = encoder.frame(frame(1)).unwrap();
                bytes.extend(encoder.frame(frame(2)).unwrap());
                bytes.extend(encoder.finish().unwrap());
                if media == "image/webp" { bytes = riff(bytes).unwrap(); }
                let source = stage(&store, bytes).await;
                let output = prepare_image(store.clone(), &source, media, &IniSettings::parse("").unwrap(), false, 90, ImageScratch::default()).await.unwrap().unwrap();
                let mut decoder = Decoder::open(store.clone(), output.key, output.size, media, ImageScratch::default()).await.unwrap();
                for index in 1..=2 {
                    let value = decoder.next().await.unwrap().unwrap();
                    assert_eq!(value.image.dimensions(), (1, 2));
                    assert_eq!(value.delay_ms, if media == "image/gif" { (37 + index) * 10 } else { 296 + index * 8 });
                }
                assert!(decoder.next().await.unwrap().is_none());
            }
        });
    }

    #[test]
    fn four_k_png_resize_crop_and_gamma_match_native_pixels() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            let image = RgbaImage::from_fn(4096, 4096, |x, y| {
                if !(512..3584).contains(&x) || !(512..3584).contains(&y) { image::Rgba([255, 255, 255, 255]) }
                else { image::Rgba([(x / 32) as u8, (y / 32) as u8, 180, 255]) }
            });
            let mut encoded = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(image).write_to(&mut encoded, ImageFormat::Png).unwrap();
            let encoded = encoded.into_inner();
            assert!(encoded.len() < 16 * 1024 * 1024);
            let ini = IniSettings::parse("ResizeW=1\nResizeNumW=1024\nResizeH=1\nResizeNumH=1024\nAutoMargin=1\nAutoMarginLimitH=30\nAutoMarginLimitV=30\nAutoMarginWhiteLevel=80\nAutoMarginPadding=0\nAutoMarginNombre=0\nGamma=1\nGammaValue=1.8\n").unwrap();
            let expected = aozora_epub3_lite::image::process(&encoded, "image/png", &ini, false, 0).unwrap();
            let expected = image::load_from_memory_with_format(&expected, ImageFormat::Png).unwrap().into_rgba8();
            assert_eq!(expected.dimensions(), (1024, 1024));
            let source = stage(&store, encoded).await;
            let scratch = ImageScratch::default();
            let output = prepare_image(store.clone(), &source, "image/png", &ini, false, 0, scratch.clone()).await.unwrap().unwrap();
            let mut stream = store.read_stream(&output.key).await.unwrap().unwrap();
            let mut encoded = Vec::new();
            while let Some(chunk) = stream.next().await { let chunk = chunk.unwrap(); assert!(chunk.len() <= CHUNK); encoded.extend(chunk); }
            let actual = image::load_from_memory_with_format(&encoded, ImageFormat::Png).unwrap().into_rgba8();
            assert_eq!(actual, expected);
            assert!(scratch.take_keys().is_empty());
            store.delete(&source.key).await.unwrap(); store.delete(&output.key).await.unwrap();
            assert_eq!(store.len(), 0);
        });
    }

    #[cfg(feature = "native-runtime")]
    #[test]
    fn forty_eight_mebibyte_apng_completes_epub_with_all_frames() {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryObjectStore::new());
            let encoder = Encoder::Png { animation: Some((48, 0)), sequence: 0, started: false };
            let stream: AssetStream = Box::pin(futures::stream::try_unfold(
                (encoder, 0u32, Vec::<u8>::new(), 0usize),
                |(mut encoder, mut index, mut bytes, mut offset)| async move {
                    if offset == bytes.len() {
                        if index == 49 { return Ok(None); }
                        bytes = if index == 48 { encoder.finish()? } else {
                            let mut seed = index + 1;
                            let pixels = RgbaImage::from_fn(512, 512, |_, _| {
                                let mut channels = [0; 4];
                                for channel in &mut channels { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; *channel = seed as u8; }
                                image::Rgba(channels)
                            });
                            encoder.frame(Frame { image: Arc::new(DynamicImage::ImageRgba8(pixels)), delay_num: 1, delay_den: 8, delay_ms: 125, default_image: false })?
                        };
                        index += 1; offset = 0;
                    }
                    let end = (offset + CHUNK).min(bytes.len());
                    let output = bytes[offset..end].to_vec(); offset = end;
                    Ok(Some((output, (encoder, index, bytes, offset))))
                },
            ));
            let source = store.stage_stream(stream).await.unwrap();
            assert!(source.size > 48 * 1024 * 1024);
            let ini = IniSettings::parse("Gamma=0\nAutoMargin=0\nResizeW=0\nResizeH=0\n").unwrap();
            assert!(prepare_image(store.clone(), &source, "image/png", &ini, false, 0, ImageScratch::default()).await.unwrap().is_none());
            let metadata = image_metadata(store.clone(), source.key.clone(), source.size, "image/png").await.unwrap().unwrap();
            assert_eq!((metadata.width, metadata.height), (512, 512));
            use std::io::{Read, Seek};
            let book = aozora_epub3_lite::EpubBook::new(
                aozora_epub3_lite::EpubMetadata::new("large animation", "urn:large-animation"),
                "<p><img src=\"../image/0001.png\"/></p>",
            ).with_assets([aozora_epub3_lite::EpubAsset::lazy("image/0001.png", "image/png")]);
            let file = tempfile::tempfile().unwrap();
            let mut writer = book.stream_writer(file).unwrap();
            while let Some(entry) = writer.next_entry() {
                if entry.asset_path.is_some() {
                    writer.begin_current().unwrap();
                    let mut stream = store.read_stream(&source.key).await.unwrap().unwrap();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.unwrap();
                        assert!(chunk.len() <= CHUNK);
                        writer.write_chunk(&chunk).unwrap();
                    }
                    writer.end_current().unwrap();
                } else {
                    writer.write_current(None).unwrap();
                }
            }
            let mut file = writer.finish().unwrap();
            file.rewind().unwrap();
            let mut epub = zip::ZipArchive::new(file).unwrap();
            let mut image = epub.by_name("item/image/0001.png").unwrap();
            assert_eq!(image.size(), source.size);
            let mut signature = [0; 8];
            image.read_exact(&mut signature).unwrap();
            assert_eq!(&signature, b"\x89PNG\r\n\x1a\n");
            let mut frames = 0;
            loop {
                let mut header = [0; 8];
                image.read_exact(&mut header).unwrap();
                let length = be32(&header[..4]) as u64;
                if &header[4..] == b"fcTL" {
                    let mut control = [0; 26];
                    assert_eq!(length, 26);
                    image.read_exact(&mut control).unwrap();
                    assert_eq!(u16::from_be_bytes([control[20], control[21]]), 1);
                    assert_eq!(u16::from_be_bytes([control[22], control[23]]), 8);
                    frames += 1;
                } else {
                    assert_eq!(std::io::copy(&mut (&mut image).take(length), &mut std::io::sink()).unwrap(), length);
                }
                image.read_exact(&mut [0; 4]).unwrap();
                if &header[4..] == b"IEND" { break; }
            }
            assert_eq!(frames, 48);
            assert_eq!(image.read(&mut [0; 1]).unwrap(), 0);
        });
    }
}
