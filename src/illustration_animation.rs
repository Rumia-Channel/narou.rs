//! Pull-driven assembly of frame archives into PNG/APNG illustrations.
//!
//! The site definition declares millisecond delays in the `ugoira` query
//! parameter. An immutable staged ZIP is read through bounded ranges. A first
//! pass validates and counts decodable frames before acTL is emitted; the
//! second pass retains only one decoded frame, one scanline and a compressor.

use std::sync::Arc;

use crate::error::{NarouError, Result};
use crate::platform::{AssetStore, AssetStream, StagedAsset};

#[cfg(feature = "illustration-animation")]
use async_zip::base::read1::{ZipOptions, seek::ZipArchiveReader};
#[cfg(feature = "illustration-animation")]
use crate::platform::AssetReader;
#[cfg(feature = "illustration-animation")]
use futures::io::AsyncReadExt;

const DELAY_PARAM: &str = "ugoira";
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct Limits {
    max_frames: usize,
    max_frame_bytes: u64,
    max_frame_pixels: u64,
    max_output_bytes: usize,
}

// These are the existing accepted frame/output limits, not a memory allowance
// for a complete archive or output. HTTP transports retain their input caps.
const DEFAULT_LIMITS: Limits = Limits {
    max_frames: 512,
    max_frame_bytes: 16 * 1024 * 1024,
    max_frame_pixels: 1920 * 1080,
    max_output_bytes: 56 * 1024 * 1024,
};

/// Whether the URL declares at least one valid delay, without allocating.
pub fn has_frame_delays(url: &str) -> bool {
    delay_values(url).is_some_and(|mut delays| delays.next().is_some())
}

/// Frame delays declared on an illustration URL, if any.
pub fn frame_delays(url: &str) -> Option<Vec<u16>> {
    let delays: Vec<u16> = delay_values(url)?.take(DEFAULT_LIMITS.max_frames).collect();
    (!delays.is_empty()).then_some(delays)
}

fn delay_values(url: &str) -> Option<impl Iterator<Item = u16> + '_> {
    let query = url.split_once('?')?.1;
    let value = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == DELAY_PARAM).then_some(value)
    })?;
    Some(value.split(',').filter_map(|delay| delay.trim().parse().ok()))
}

/// Prepare a bounded PNG/APNG stream from a completed, immutable stage.
/// Ordinary images return `None`. Corrupt archives fail instead of publishing
/// the source ZIP under an image filename. The caller owns stage cleanup.
pub async fn assemble_animation_stream(
    url: &str,
    assets: Arc<dyn AssetStore>,
    source: &StagedAsset,
) -> Result<Option<AssetStream>> {
    let Some(delays) = frame_delays(url) else { return Ok(None) };
    if source.size < 4 { return Ok(None); }
    let signature = assets.read_range(&source.key, 0..source.size.min(4)).await?
        .ok_or_else(|| NarouError::Platform("animation source stage disappeared".into()))?;
    if !signature.starts_with(b"PK\x03\x04") && !signature.starts_with(b"PK\x05\x06") {
        return Ok(None);
    }
    build_apng_stream(assets, source, delays, DEFAULT_LIMITS).await.map(Some)
}

#[cfg(not(feature = "illustration-animation"))]
async fn build_apng_stream(
    _assets: Arc<dyn AssetStore>, _source: &StagedAsset, _delays: Vec<u16>, _limits: Limits,
) -> Result<AssetStream> {
    Err(NarouError::Platform("assembling an animated illustration needs the image codecs".into()))
}

#[cfg(feature = "illustration-animation")]
fn archive_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("animation archive: {error}"))
}

#[cfg(feature = "illustration-animation")]
struct FrameName {
    index: usize,
    name: String,
}

#[cfg(feature = "illustration-animation")]
async fn build_apng_stream(
    assets: Arc<dyn AssetStore>, source: &StagedAsset, delays: Vec<u16>, limits: Limits,
) -> Result<AssetStream> {
    // Even non-HTTP callers cannot supply metadata larger than the existing
    // native HTTP ceiling. The Worker transport applies its tighter input cap.
    if source.size > crate::downloader::security::MAX_RESPONSE_BYTES as u64 {
        return Err(archive_error("source exceeds the HTTP response limit"));
    }
    let options = ZipOptions {
        // Keep only eight-byte CD offsets, not a second copy of every name,
        // comment and extra field. Limits are checked before CD allocation.
        max_cd_num_files_load: 0,
        max_cd_num_files: source.size / 46,
        max_cd_size_in_bytes: source.size,
        max_compressed_size_per_file: source.size,
        max_uncompressed_size_per_file: limits.max_frame_bytes,
        ..ZipOptions::default()
    };
    let reader = AssetReader::new(assets, source.key.clone(), source.size);
    let mut zip = ZipArchiveReader::open_with_options(reader, options).await.map_err(archive_error)?;
    let count = zip.ceocdr().num_entries().map_err(archive_error)?;
    let mut names = Vec::new();
    for index in 0..count as usize {
        let file = zip.file(index).await.map_err(archive_error)?;
        let Some(name) = file.lf().insecure_file_name.as_str() else { continue };
        if name.ends_with('/') { continue; }
        if names.len() == limits.max_frames {
            return Err(archive_error(format!("frame count exceeds {}", limits.max_frames)));
        }
        names.push(FrameName { index, name: name.to_owned() });
    }
    names.sort_by(|a, b| a.name.cmp(&b.name));
    let mut valid = Vec::with_capacity(names.len());
    let mut dimensions = None;
    for entry in names {
        let Some(frame) = decode_frame(&mut zip, &entry, limits).await? else { continue };
        let size = frame.dimensions();
        if dimensions.is_some_and(|expected| expected != size) {
            return Err(archive_error(format!("frame {} has inconsistent dimensions", entry.name)));
        }
        dimensions = Some(size);
        valid.push(entry);
        // No decoded frame survives the preflight iteration.
    }
    let (width, height) = dimensions.ok_or_else(|| archive_error("no decodable frames"))?;
    let animated = valid.len() > 1;
    let header_len = 8 + 25 + if animated { 20 } else { 0 };
    if header_len > limits.max_output_bytes { return Err(output_limit(limits)); }
    let mut header = Vec::with_capacity(header_len);
    header.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = [0; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8..].copy_from_slice(&[8, 6, 0, 0, 0]);
    append_chunk(&mut header, b"IHDR", &ihdr);
    if animated {
        let mut actl = [0; 8];
        actl[..4].copy_from_slice(&(valid.len() as u32).to_be_bytes());
        append_chunk(&mut header, b"acTL", &actl);
    }
    let state = AnimationOutput {
        zip, frames: valid, delays, dimensions: (width, height), limits,
        frame_index: 0, sequence: 0, encoder: None, header: Some(header),
        emitted: 0, complete: false,
    };
    Ok(Box::pin(futures::stream::try_unfold(state, |mut state| async move {
        let Some(chunk) = state.next_chunk().await? else { return Ok(None) };
        Ok(Some((chunk, state)))
    })))
}

#[cfg(feature = "illustration-animation")]
async fn decode_frame(
    zip: &mut ZipArchiveReader<AssetReader>, entry: &FrameName, limits: Limits,
) -> Result<Option<image::RgbaImage>> {
    use image::ImageDecoder as _;

    let mut file = zip.file(entry.index).await.map_err(archive_error)?;
    let declared = file.lf().uncompressed_size().map_err(archive_error)?;
    if declared > limits.max_frame_bytes {
        return Err(archive_error(format!("frame {} exceeds the frame limit", entry.name)));
    }
    // Allocate the validated declaration exactly, then demand EOF (including
    // async_zip's CRC and size validation). Never grow a decompression-bomb Vec.
    let mut data = vec![0; declared as usize];
    file.read_exact(&mut data).await?;
    if file.read(&mut [0u8; 1]).await? != 0 {
        return Err(archive_error(format!("frame {} expands beyond its declaration", entry.name)));
    }
    drop(file);
    let mut reader = match image::ImageReader::new(std::io::Cursor::new(&data)).with_guessed_format() {
        Ok(reader) => reader,
        Err(_) => return Ok(None),
    };
    let mut decoder_limits = image::Limits::default();
    decoder_limits.max_alloc = Some(limits.max_frame_bytes + limits.max_frame_pixels * 8);
    reader.limits(decoder_limits);
    let decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(error @ image::ImageError::Limits(_)) => return Err(archive_error(error)),
        Err(error) => {
            tracing::debug!("animation frame {} could not be decoded: {error}", entry.name);
            return Ok(None);
        }
    };
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > limits.max_frame_pixels {
        return Err(archive_error(format!("frame {} is {width}x{height}, over the pixel limit", entry.name)));
    }
    match image::DynamicImage::from_decoder(decoder) {
        Ok(decoded) => Ok(Some(decoded.into_rgba8())),
        Err(error @ image::ImageError::Limits(_)) => Err(archive_error(error)),
        Err(error) => {
            tracing::debug!("animation frame {} could not be decoded: {error}", entry.name);
            Ok(None)
        }
    }
}

#[cfg(feature = "illustration-animation")]
fn output_limit(limits: Limits) -> NarouError {
    NarouError::Platform(format!("assembled animation exceeds {} bytes", limits.max_output_bytes))
}

#[cfg(feature = "illustration-animation")]
struct AnimationOutput {
    zip: ZipArchiveReader<AssetReader>,
    frames: Vec<FrameName>,
    delays: Vec<u16>,
    dimensions: (u32, u32),
    limits: Limits,
    frame_index: usize,
    sequence: u32,
    encoder: Option<FrameEncoder>,
    header: Option<Vec<u8>>,
    emitted: usize,
    complete: bool,
}

#[cfg(feature = "illustration-animation")]
impl AnimationOutput {
    fn emit(&mut self, chunk: Vec<u8>) -> Result<Option<Vec<u8>>> {
        if chunk.len() > self.limits.max_output_bytes - self.emitted {
            return Err(output_limit(self.limits));
        }
        self.emitted += chunk.len();
        Ok(Some(chunk))
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.complete { return Ok(None); }
        if let Some(header) = self.header.take() { return self.emit(header); }
        loop {
            if let Some(encoder) = &mut self.encoder && encoder.frame.is_some() {
                let remaining = self.limits.max_output_bytes - self.emitted;
                if let Some(data) = encoder.next_chunk(self.frame_index > 0, &mut self.sequence, remaining)? {
                    return self.emit(data);
                }
                encoder.frame = None;
                self.frame_index += 1;
            }
            if self.frame_index == self.frames.len() {
                if self.limits.max_output_bytes - self.emitted < 12 { return Err(output_limit(self.limits)); }
                self.complete = true;
                return self.emit(png_chunk(b"IEND", &[]));
            }
            let entry = &self.frames[self.frame_index];
            let frame = decode_frame(&mut self.zip, entry, self.limits).await?
                .ok_or_else(|| archive_error("immutable source frame changed after preflight"))?;
            if frame.dimensions() != self.dimensions {
                return Err(archive_error("immutable source dimensions changed after preflight"));
            }
            let encoder = self.encoder.get_or_insert_with(|| FrameEncoder::new(self.dimensions.0));
            encoder.start_frame(frame);
            if self.frames.len() > 1 {
                if self.limits.max_output_bytes - self.emitted < 38 { return Err(output_limit(self.limits)); }
                let delay = *self.delays.get(self.frame_index).unwrap_or_else(|| self.delays.last().unwrap());
                let payload = frame_control(self.sequence, self.dimensions, delay);
                self.sequence += 1;
                return self.emit(png_chunk(b"fcTL", &payload));
            }
        }
    }
}

/// An encoder that consumes one filtered scanline at a time and writes zlib
/// directly into the next PNG chunk. There is no intermediate frame PNG.
#[cfg(feature = "illustration-animation")]
struct FrameEncoder {
    frame: Option<image::RgbaImage>,
    compressor: Box<miniz_oxide::deflate::core::CompressorOxide>,
    row: Vec<u8>,
    row_offset: usize,
    next_row: u32,
    done: bool,
}

#[cfg(feature = "illustration-animation")]
impl FrameEncoder {
    fn new(width: u32) -> Self {
        Self {
            frame: None,
            compressor: Box::default(),
            row: vec![0; width as usize * 4 + 1],
            row_offset: width as usize * 4 + 1,
            next_row: 0,
            done: false,
        }
    }

    fn start_frame(&mut self, frame: image::RgbaImage) {
        self.compressor.reset();
        self.frame = Some(frame);
        self.row_offset = self.row.len();
        self.next_row = 0;
        self.done = false;
    }

    fn next_chunk(&mut self, later: bool, sequence: &mut u32, remaining: usize) -> Result<Option<Vec<u8>>> {
        use miniz_oxide::{MZFlush, MZStatus};
        if self.done { return Ok(None); }
        let overhead = if later { 16 } else { 12 };
        let capacity = remaining.min(CHUNK_BYTES);
        if capacity <= overhead { return Err(archive_error("assembled animation exceeds output limit")); }
        let start = overhead - 4;
        let end = capacity - 4;
        let mut chunk = vec![0; capacity];
        let mut cursor = start;
        let frame = self.frame.as_ref().expect("active frame");
        while cursor < end {
            if self.row_offset == self.row.len() && self.next_row < frame.height() {
                filter_row(frame, self.next_row, &mut self.row);
                self.next_row += 1;
                self.row_offset = 0;
            }
            let last = self.next_row == frame.height();
            let result = miniz_oxide::deflate::stream::deflate(
                &mut self.compressor, &self.row[self.row_offset..], &mut chunk[cursor..end],
                if last { MZFlush::Finish } else { MZFlush::None },
            );
            self.row_offset += result.bytes_consumed;
            cursor += result.bytes_written;
            match result.status {
                Ok(MZStatus::StreamEnd) => { self.done = true; break; }
                Ok(_) if result.bytes_consumed != 0 || result.bytes_written != 0 => {}
                status => return Err(archive_error(format!("PNG compressor made no progress: {status:?}"))),
            }
        }
        if cursor == start && self.done { return Ok(None); }
        let kind = if later { b"fdAT" } else { b"IDAT" };
        if later {
            chunk[8..12].copy_from_slice(&sequence.to_be_bytes());
            *sequence += 1;
        }
        chunk[..4].copy_from_slice(&((cursor - 8) as u32).to_be_bytes());
        chunk[4..8].copy_from_slice(kind);
        let crc = crc32fast::hash(&chunk[4..cursor]);
        chunk[cursor..cursor + 4].copy_from_slice(&crc.to_be_bytes());
        chunk.truncate(cursor + 4);
        Ok(Some(chunk))
    }
}

#[cfg(feature = "illustration-animation")]
fn filter_row(frame: &image::RgbaImage, row: u32, output: &mut [u8]) {
    let stride = frame.width() as usize * 4;
    let offset = row as usize * stride;
    let raw = frame.as_raw();
    let current = &raw[offset..offset + stride];
    let previous = (row > 0).then(|| &raw[offset - stride..offset]);
    let filtered = |kind: usize, index: usize| {
        let left = if index >= 4 { current[index - 4] } else { 0 };
        let up = previous.map_or(0, |bytes| bytes[index]);
        let corner = if index >= 4 { previous.map_or(0, |bytes| bytes[index - 4]) } else { 0 };
        let predictor = match kind {
            0 => 0,
            1 => left,
            2 => up,
            3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
            _ => paeth(left, up, corner),
        };
        current[index].wrapping_sub(predictor)
    };
    let mut scores = [0u64; 5];
    for (kind, score) in scores.iter_mut().enumerate() {
        for index in 0..stride { *score += u64::from((filtered(kind, index) as i8).unsigned_abs()); }
    }
    let kind = (0..5).min_by_key(|&kind| scores[kind]).unwrap();
    output[0] = kind as u8;
    for (index, byte) in output[1..].iter_mut().enumerate() { *byte = filtered(kind, index); }
}

#[cfg(feature = "illustration-animation")]
fn paeth(left: u8, up: u8, corner: u8) -> u8 {
    let p = i16::from(left) + i16::from(up) - i16::from(corner);
    let a = (p - i16::from(left)).abs();
    let b = (p - i16::from(up)).abs();
    let c = (p - i16::from(corner)).abs();
    if a <= b && a <= c { left } else if b <= c { up } else { corner }
}

#[cfg(feature = "illustration-animation")]
fn frame_control(sequence: u32, dimensions: (u32, u32), delay: u16) -> [u8; 26] {
    let mut payload = [0; 26];
    payload[..4].copy_from_slice(&sequence.to_be_bytes());
    payload[4..8].copy_from_slice(&dimensions.0.to_be_bytes());
    payload[8..12].copy_from_slice(&dimensions.1.to_be_bytes());
    payload[20..22].copy_from_slice(&delay.to_be_bytes());
    payload[22..24].copy_from_slice(&1000u16.to_be_bytes());
    payload
}

#[cfg(feature = "illustration-animation")]
fn append_chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let crc = crc32fast::hash(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

#[cfg(feature = "illustration-animation")]
fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(payload.len() + 12);
    append_chunk(&mut output, kind, payload);
    output
}

#[cfg(all(test, feature = "illustration-animation"))]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Write;

    fn png_frame(color: [u8; 4]) -> Vec<u8> {
        let frame = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
        let mut data = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(frame).write_to(&mut data, image::ImageFormat::Png).unwrap();
        data.into_inner()
    }

    fn zip_of(frames: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in frames {
            writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    async fn staged(bytes: Vec<u8>) -> (Arc<dyn AssetStore>, StagedAsset) {
        let assets: Arc<dyn AssetStore> = Arc::new(crate::platform::mocks::MemoryObjectStore::new());
        let stage = assets.stage_stream(Box::pin(futures::stream::once(async { Ok(bytes) }))).await.unwrap();
        (assets, stage)
    }

    // Whole-value collection is confined to small test fixtures, never production.
    async fn collect(mut stream: AssetStream) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            assert!(chunk.len() <= CHUNK_BYTES);
            result.extend_from_slice(&chunk);
        }
        Ok(result)
    }

    fn chunks(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut offset = 8;
        let mut output = Vec::new();
        while offset < data.len() {
            let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            let end = offset + 8 + length;
            assert_eq!(crc32fast::hash(&data[offset + 4..end]), u32::from_be_bytes(data[end..end + 4].try_into().unwrap()));
            output.push((data[offset + 4..offset + 8].try_into().unwrap(), &data[offset + 8..end]));
            offset = end + 4;
        }
        output
    }

    #[test]
    fn delays_are_read_from_url_and_bounded() {
        assert_eq!(frame_delays("https://e.test/a.zip?ugoira=120,80,80"), Some(vec![120, 80, 80]));
        assert!(frame_delays("https://e.test/a.zip?ugoira=").is_none());
        assert!(frame_delays("https://e.test/a.png").is_none());
        assert!(has_frame_delays("https://e.test/a.zip?ugoira=bad,120"));
        assert!(!has_frame_delays("https://e.test/a.zip?ugoira=bad,70000"));
        assert!(!has_frame_delays("https://e.test/a.png"));
        let url = format!("https://e.test/a.zip?ugoira={}", vec!["1"; 1024].join(","));
        assert_eq!(frame_delays(&url).unwrap().len(), DEFAULT_LIMITS.max_frames);
    }

    #[test]
    fn tiny_512_frame_archive_stays_below_the_consumer_range_request_budget() {
        use crate::platform::asset_reader::tests::RangeCountingStore;
        use image::AnimationDecoder;
        futures::executor::block_on(async {
            let color = [13, 37, 193, 127];
            let mut frame = std::io::Cursor::new(Vec::new());
            image::RgbaImage::from_pixel(8, 8, image::Rgba(color))
                .write_to(&mut frame, image::ImageFormat::Png).unwrap();
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            for index in 0..512 {
                zip.start_file(format!("{index:06}.png"),
                    zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
                zip.write_all(frame.get_ref()).unwrap();
            }
            let archive = zip.finish().unwrap().into_inner();
            assert!(archive.len() > CHUNK_BYTES);
            let assets = Arc::new(RangeCountingStore::new());
            let source = assets.stage_stream(Box::pin(futures::stream::once(async move { Ok(archive) }))).await.unwrap();
            let url = format!("https://e.test/frames.zip?ugoira={}", vec!["125"; 512].join(","));
            let stream = assemble_animation_stream(&url, assets.clone(), &source).await.unwrap().unwrap();
            let png = collect(stream).await.unwrap();
            assert!(assets.range_count() < 1_000, "consumer issued {} range requests", assets.range_count());
            let parsed = chunks(&png);
            let controls: Vec<_> = parsed.iter().filter(|(kind, _)| kind == b"fcTL").collect();
            assert_eq!(controls.len(), 512);
            assert!(controls.iter().all(|(_, data)| u16::from_be_bytes(data[20..22].try_into().unwrap()) == 125));
            let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(&png)).unwrap().apng().unwrap();
            let frames = decoder.into_frames().collect_frames().unwrap();
            assert_eq!(frames.len(), 512);
            for frame in frames {
                assert_eq!(frame.buffer().dimensions(), (8, 8));
                assert!(frame.buffer().pixels().all(|pixel| pixel.0 == color));
            }
        });
    }

    #[test]
    fn preflight_counts_valid_frames_and_preserves_sorted_pixels_alpha_and_delays() {
        futures::executor::block_on(async {
            let colors = [[1, 2, 3, 128], [20, 30, 40, 0], [50, 60, 70, 255]];
            let archive = zip_of(&[
                ("000003.png", png_frame(colors[2])),
                ("000001.png", b"invalid image".to_vec()),
                ("000002.png", png_frame(colors[1])),
                ("000000.png", png_frame(colors[0])),
            ]);
            let (assets, source) = staged(archive).await;
            let stream = assemble_animation_stream("https://e.test/a.zip?ugoira=120,80", assets, &source).await.unwrap().unwrap();
            let png = collect(stream).await.unwrap();
            let parsed = chunks(&png);
            let actl = parsed.iter().find(|(kind, _)| kind == b"acTL").unwrap().1;
            assert_eq!(u32::from_be_bytes(actl[..4].try_into().unwrap()), 3);
            let controls: Vec<_> = parsed.iter().filter(|(kind, _)| kind == b"fcTL").collect();
            assert_eq!(controls.iter().map(|(_, p)| u16::from_be_bytes(p[20..22].try_into().unwrap())).collect::<Vec<_>>(), vec![120, 80, 80]);
            let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(&png)).unwrap().apng().unwrap();
            use image::AnimationDecoder;
            let frames = decoder.into_frames().collect_frames().unwrap();
            assert_eq!(frames.len(), colors.len());
            for (frame, color) in frames.iter().zip(colors) {
                assert!(frame.buffer().pixels().all(|pixel| pixel.0 == color));
            }
        });
    }

    #[test]
    fn one_valid_frame_is_an_ordinary_png_and_ordinary_inputs_are_untouched() {
        futures::executor::block_on(async {
            let archive = zip_of(&[("a.png", png_frame([1, 2, 3, 128])), ("b.txt", b"bad".to_vec())]);
            let (assets, source) = staged(archive).await;
            assert!(assemble_animation_stream("https://e.test/a.zip", assets.clone(), &source).await.unwrap().is_none());
            let png = collect(assemble_animation_stream("https://e.test/a.zip?ugoira=10", assets, &source).await.unwrap().unwrap()).await.unwrap();
            assert!(!chunks(&png).iter().any(|(kind, _)| kind == b"acTL"));
            assert_eq!(image::load_from_memory(&png).unwrap().into_rgba8().get_pixel(0, 0).0, [1, 2, 3, 128]);
            let (assets, source) = staged(png).await;
            assert!(assemble_animation_stream("https://e.test/a.png?ugoira=10", assets, &source).await.unwrap().is_none());
        });
    }

    #[test]
    fn frame_and_output_limits_fail_before_publication() {
        futures::executor::block_on(async {
            let (assets, source) = staged(zip_of(&[("a.png", png_frame([1, 2, 3, 255])), ("b.png", png_frame([4, 5, 6, 255]))])).await;
            for limits in [
                Limits { max_frames: 1, ..DEFAULT_LIMITS },
                Limits { max_frame_pixels: 1, ..DEFAULT_LIMITS },
                Limits { max_frame_bytes: 1, ..DEFAULT_LIMITS },
                Limits { max_output_bytes: 16, ..DEFAULT_LIMITS },
            ] {
                assert!(build_apng_stream(assets.clone(), &source, vec![50], limits).await.is_err());
            }
            let stream = build_apng_stream(assets, &source, vec![50], Limits { max_output_bytes: 100, ..DEFAULT_LIMITS }).await.unwrap();
            assert!(collect(stream).await.is_err());
        });
    }

    #[test]
    fn encoder_emits_bounded_chunks_for_incompressible_rgba() {
        let mut seed = 7u32;
        let mut frame = image::RgbaImage::new(512, 512);
        for byte in frame.as_mut() {
            seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
            *byte = seed as u8;
        }
        let expected = frame.clone();
        let mut encoder = FrameEncoder::new(512);
        encoder.start_frame(frame);
        let mut sequence = 0;
        let mut compressed = Vec::new();
        while let Some(chunk) = encoder.next_chunk(false, &mut sequence, usize::MAX).unwrap() {
            assert!(chunk.len() <= CHUNK_BYTES);
            compressed.extend_from_slice(&chunk[8..chunk.len() - 4]);
        }
        let scanlines = miniz_oxide::inflate::decompress_to_vec_zlib(&compressed).unwrap();
        assert_eq!(scanlines.len(), (512 * 4 + 1) * 512);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = [0; 13];
        ihdr[..4].copy_from_slice(&512u32.to_be_bytes()); ihdr[4..8].copy_from_slice(&512u32.to_be_bytes()); ihdr[8..].copy_from_slice(&[8, 6, 0, 0, 0]);
        append_chunk(&mut png, b"IHDR", &ihdr); append_chunk(&mut png, b"IDAT", &compressed); append_chunk(&mut png, b"IEND", &[]);
        assert_eq!(image::load_from_memory(&png).unwrap().into_rgba8(), expected);
    }
}
