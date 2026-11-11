//! Non-interlaced static PNGs are decoded, resized and encoded by source row.
//! Neither encoded IDAT data nor a full decoded source/target is retained.

use std::io::Write;
use std::sync::Arc;
use aozora_epub3_lite::{IniSettings, image::{AnimationTransform, RowDepth, SourceRows}};
use super::{CHUNK, Input, Png, be32, conversion_error, png_chunk};
use crate::error::Result;
use crate::platform::{AssetStore, AssetStream, StagedAsset};

type Sink = super::super::super::ChunkSink;

struct Chunk { remaining: u32, crc: crc32fast::Hasher, emit: bool, end: bool }
struct PngInput { input: Input, current: Option<Chunk>, ended_data: bool, done: bool }
impl PngInput {
    async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if self.done { return Ok(None); }
            if let Some(mut chunk) = self.current.take() {
                if chunk.remaining != 0 {
                    let bytes = self.input.bytes((chunk.remaining as usize).min(CHUNK)).await?;
                    chunk.remaining -= bytes.len() as u32; chunk.crc.update(&bytes);
                    let emit = chunk.emit; self.current = Some(chunk);
                    if emit { return Ok(Some(bytes)); }
                    continue;
                }
                let crc = self.input.bytes(4).await?;
                if be32(&crc) != chunk.crc.finalize() { return Err(conversion_error("PNG chunk CRC mismatch")); }
                if chunk.end {
                    if self.input.position != self.input.size { return Err(conversion_error("trailing PNG bytes")); }
                    self.input.finish().await?; self.done = true;
                }
                if chunk.emit { return Ok(Some(crc)); }
                continue;
            }
            let header = self.input.bytes(8).await?;
            let length = be32(&header); let kind = &header[4..8];
            if matches!(kind, b"IHDR" | b"PLTE" | b"tRNS" | b"acTL" | b"fcTL" | b"fdAT") {
                return Err(conversion_error("unexpected PNG metadata after image data"));
            }
            let idat = kind == b"IDAT"; let end = kind == b"IEND";
            if idat && self.ended_data { return Err(conversion_error("non-consecutive PNG image data")); }
            if end && length != 0 { return Err(conversion_error("invalid PNG end chunk")); }
            if !idat { self.ended_data = true; }
            if !idat && !end && kind[0] & 0x20 == 0 { return Err(conversion_error("unsupported critical PNG chunk")); }
            let mut crc = crc32fast::Hasher::new(); crc.update(kind);
            self.current = Some(Chunk { remaining: length, crc, emit: idat || end, end });
            if idat || end { return Ok(Some(header)); }
        }
    }
}

struct Header {
    width: u32, height: u32, color: png::ColorType, bits: u8,
    packed: usize, bpp: usize, palette: Vec<u8>, transparent: Vec<u8>,
}
impl Header {
    fn from_info(info: &png::Info<'_>) -> Result<Self> {
        let bits = info.bit_depth as u8;
        let packed = usize::try_from((u64::from(info.width) * info.color_type.samples() as u64 * u64::from(bits)).div_ceil(8)).map_err(conversion_error)?;
        Ok(Self { width: info.width, height: info.height, color: info.color_type, bits, packed,
            bpp: (info.color_type.samples() * usize::from(bits)).div_ceil(8).max(1),
            palette: info.palette.as_deref().unwrap_or_default().to_vec(), transparent: info.trns.as_deref().unwrap_or_default().to_vec() })
    }
    fn expand8(&self, row: &[u8], output: &mut [u8]) -> Result<()> {
        for x in 0..self.width as usize {
            let pixel = match self.color {
                png::ColorType::Indexed => {
                    let index = packed_sample(row, x, self.bits) as usize;
                    let rgb = self.palette.get(index * 3..index * 3 + 3).ok_or_else(|| conversion_error("PNG palette index out of bounds"))?;
                    [rgb[0], rgb[1], rgb[2], self.transparent.get(index).copied().unwrap_or(255)]
                }
                png::ColorType::Grayscale => {
                    let sample = packed_sample(row, x, self.bits);
                    let gray = (u16::from(sample) * 255 / ((1u16 << self.bits) - 1)) as u8;
                    [gray, gray, gray, if self.transparent.first() == Some(&sample) { 0 } else { 255 }]
                }
                png::ColorType::Rgb => {
                    let rgb = &row[x * 3..x * 3 + 3];
                    [rgb[0], rgb[1], rgb[2], if self.transparent.as_slice() == rgb { 0 } else { 255 }]
                }
                png::ColorType::GrayscaleAlpha => { let gray = row[x * 2]; [gray, gray, gray, row[x * 2 + 1]] }
                png::ColorType::Rgba => row[x * 4..x * 4 + 4].try_into().unwrap(),
            };
            output[x * 4..x * 4 + 4].copy_from_slice(&pixel);
        }
        Ok(())
    }
    fn expand16(&self, row: &[u8], output: &mut [u16]) -> Result<()> {
        let sample = |index: usize| u16::from_be_bytes([row[index * 2], row[index * 2 + 1]]);
        for x in 0..self.width as usize {
            let pixel = match self.color {
                png::ColorType::Grayscale => {
                    let gray = sample(x);
                    [gray, gray, gray, if self.transparent.as_slice() == &row[x * 2..x * 2 + 2] { 0 } else { u16::MAX }]
                }
                png::ColorType::Rgb => {
                    [sample(x * 3), sample(x * 3 + 1), sample(x * 3 + 2), if self.transparent.as_slice() == &row[x * 6..x * 6 + 6] { 0 } else { u16::MAX }]
                }
                png::ColorType::GrayscaleAlpha => { let gray = sample(x * 2); [gray, gray, gray, sample(x * 2 + 1)] }
                png::ColorType::Rgba => [sample(x * 4), sample(x * 4 + 1), sample(x * 4 + 2), sample(x * 4 + 3)],
                png::ColorType::Indexed => return Err(conversion_error("invalid sixteen-bit PNG palette")),
            };
            output[x * 4..x * 4 + 4].copy_from_slice(&pixel);
        }
        Ok(())
    }
}
fn packed_sample(row: &[u8], x: usize, bits: u8) -> u8 {
    if bits == 8 { row[x] } else { (row[x * usize::from(bits) / 8] >> (8 - bits - (x * usize::from(bits) % 8) as u8)) & ((1 << bits) - 1) }
}
fn unfilter(filter: u8, row: &mut [u8], previous: &[u8], bpp: usize) -> Result<()> {
    if filter > 4 { return Err(conversion_error("invalid PNG row filter")); }
    for i in 0..row.len() {
        let left = if i >= bpp { row[i - bpp] } else { 0 };
        let up = previous[i]; let upper_left = if i >= bpp { previous[i - bpp] } else { 0 };
        let prediction = match filter {
            0 => 0, 1 => left, 2 => up, 3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
            _ => {
                let p = i32::from(left) + i32::from(up) - i32::from(upper_left);
                let a = (p - i32::from(left)).abs(); let b = (p - i32::from(up)).abs(); let c = (p - i32::from(upper_left)).abs();
                if a <= b && a <= c { left } else if b <= c { up } else { upper_left }
            }
        };
        row[i] = row[i].wrapping_add(prediction);
    }
    Ok(())
}

struct State {
    input: PngInput, decoder: png::StreamingDecoder, compressed: Vec<u8>, offset: usize,
    region: png::UnfilterRegion, inflated: Vec<u8>, consumed: usize,
    header: Header, previous: Vec<u8>, current: Vec<u8>, rgba8: Vec<u8>, rgba16: Vec<u16>, source_y: u32,
    rows: SourceRows, writer: Option<png::StreamWriter<'static, Sink>>, sink: Sink,
    output: Vec<u8>, output_offset: usize, done: bool,
}

pub(super) async fn try_stream(store: Arc<dyn AssetStore>, source: &StagedAsset, ini: &IniSettings, cover: bool) -> Result<Option<AssetStream>> {
    let mut input = Input::stream(store, source.key.clone(), source.size).await?;
    if input.bytes(8).await? != b"\x89PNG\r\n\x1a\n" { return Err(conversion_error("invalid PNG signature")); }
    let (kind, ihdr) = Png::chunk(&mut input).await?;
    if &kind != b"IHDR" || ihdr.len() != 13 { return Err(conversion_error("invalid PNG header")); }
    if ihdr[12] != 0 { return Ok(None); }
    let mut prefix = b"\x89PNG\r\n\x1a\n".to_vec(); png_chunk(&mut prefix, b"IHDR", &ihdr);
    let initial = loop {
        let header = input.bytes(8).await?; let length = be32(&header); let kind: [u8; 4] = header[4..8].try_into().unwrap();
        if matches!(&kind, b"acTL" | b"fcTL" | b"fdAT") { return Ok(None); }
        let mut crc = crc32fast::Hasher::new(); crc.update(&kind);
        if &kind == b"IDAT" { prefix.extend_from_slice(&header); break Chunk { remaining: length, crc, emit: true, end: false }; }
        if &kind == b"IEND" || &kind == b"IHDR" { return Err(conversion_error("PNG image data missing")); }
        let retain = matches!(&kind, b"PLTE" | b"tRNS");
        if (&kind == b"PLTE" && length > 768) || (&kind == b"tRNS" && length > 256) { return Err(conversion_error("invalid PNG palette metadata")); }
        if !retain && kind[0] & 0x20 == 0 { return Err(conversion_error("unsupported critical PNG chunk")); }
        let mut remaining = length; let mut payload = Vec::new();
        while remaining != 0 {
            let bytes = input.bytes((remaining as usize).min(CHUNK)).await?;
            remaining -= bytes.len() as u32; crc.update(&bytes);
            if retain { payload.extend_from_slice(&bytes); }
        }
        if be32(&input.bytes(4).await?) != crc.finalize() { return Err(conversion_error("PNG chunk CRC mismatch")); }
        if retain { png_chunk(&mut prefix, &kind, &payload); }
    };
    let mut decoder = png::StreamingDecoder::new(); decoder.set_ignore_adler32(false); decoder.set_skip_ancillary_crc_failures(false);
    let mut offset = 0;
    while offset < prefix.len() {
        let (consumed, event) = decoder.update(&prefix[offset..], None).map_err(conversion_error)?;
        offset += consumed;
        if matches!(event, png::Decoded::ChunkBegin(_, kind) if kind == png::chunk::IDAT) { break; }
        if consumed == 0 && matches!(event, png::Decoded::Nothing) { return Err(conversion_error("PNG header decoder stalled")); }
    }
    if offset != prefix.len() { return Err(conversion_error("PNG image-data header mismatch")); }
    let header = Header::from_info(decoder.info().ok_or_else(|| conversion_error("PNG header missing"))?)?;
    let transform = AnimationTransform::from_dimensions(ini, cover, 0, (header.width, header.height)).map_err(conversion_error)?;
    let depth = if header.bits == 16 { RowDepth::Sixteen } else { RowDepth::Eight };
    let sample_bytes = if header.bits == 16 { 2usize } else { 1 };
    // Deflate history, fixed input/output blocks, two unfilter rows, expanded
    // RGBA row, encoder rows and storage's one in-flight multipart part.
    let codec_scratch = 8 * 1024 * 1024 + 512 * 1024 + header.packed as u64 * 3
        + u64::from(header.width) * 4 * sample_bytes as u64 * 4;
    super::admit_image_memory(codec_scratch)?;
    super::admit_image_memory(codec_scratch + transform.source_row_memory_bytes(depth).map_err(conversion_error)?)?;
    let rows = transform.source_rows(depth).map_err(conversion_error)?;
    let (width, height) = rows.dimensions();
    let sink = Sink::new(); let mut encoder = png::Encoder::new(sink.clone(), width, height);
    encoder.set_color(png::ColorType::Rgba); encoder.set_depth(if header.bits == 16 { png::BitDepth::Sixteen } else { png::BitDepth::Eight });
    let writer = encoder.write_header().map_err(conversion_error)?.into_stream_writer_with_size(CHUNK - 16).map_err(conversion_error)?;
    let packed = header.packed; let samples = header.width as usize * 4;
    let state = State {
        input: PngInput { input, current: Some(initial), ended_data: false, done: false },
        decoder, compressed: Vec::new(), offset: 0, region: png::UnfilterRegion::default(),
        inflated: vec![0; 32768 + packed + 1 + CHUNK], consumed: 0,
        previous: vec![0; packed], current: vec![0; packed],
        rgba8: if sample_bytes == 1 { vec![0; samples] } else { Vec::new() },
        rgba16: if sample_bytes == 2 { vec![0; samples] } else { Vec::new() },
        header, source_y: 0, rows, writer: Some(writer), sink, output: Vec::new(), output_offset: 0, done: false,
    };
    Ok(Some(Box::pin(futures::stream::try_unfold(state, |mut state| async move {
        loop {
            if state.output_offset < state.output.len() {
                let end = (state.output_offset + CHUNK).min(state.output.len());
                let bytes = state.output[state.output_offset..end].to_vec(); state.output_offset = end;
                return Ok(Some((bytes, state)));
            }
            state.output = state.sink.take(); state.output_offset = 0;
            if !state.output.is_empty() { continue; }
            if state.writer.is_none() { return Ok(None); }
            if let Some(row) = state.rows.next_row() { state.writer.as_mut().unwrap().write_all(row)?; continue; }
            let row_length = state.header.packed + 1;
            if state.region.filled - state.consumed >= row_length {
                if state.source_y == state.header.height { return Err(conversion_error("extra PNG scanlines")); }
                let filter = state.inflated[state.consumed];
                state.current.copy_from_slice(&state.inflated[state.consumed + 1..state.consumed + row_length]);
                unfilter(filter, &mut state.current, &state.previous, state.header.bpp)?;
                if state.header.bits == 16 {
                    state.header.expand16(&state.current, &mut state.rgba16)?; state.rows.push_rgba16(&state.rgba16).map_err(conversion_error)?;
                } else {
                    state.header.expand8(&state.current, &mut state.rgba8)?; state.rows.push_rgba8(&state.rgba8).map_err(conversion_error)?;
                }
                std::mem::swap(&mut state.previous, &mut state.current);
                state.source_y += 1; state.consumed += row_length;
                continue;
            }
            if state.done {
                if state.consumed != state.region.filled || state.source_y != state.header.height { return Err(conversion_error("truncated PNG scanlines")); }
                state.rows.finish().map_err(conversion_error)?;
                state.writer.take().unwrap().finish().map_err(conversion_error)?;
                continue;
            }
            if state.offset == state.compressed.len() {
                state.compressed = state.input.next().await?.ok_or_else(|| conversion_error("PNG end chunk missing"))?;
                state.offset = 0;
            }
            // Preserve the inflater's 32 KiB history, but compact only when
            // another row needs space rather than copying that history per row.
            if state.inflated.len() - state.region.filled < row_length.max(CHUNK / 2) {
                let discard = state.consumed.min(state.region.available);
                if discard != 0 {
                    state.inflated.copy_within(discard..state.region.filled, 0);
                    state.region.filled -= discard; state.region.available -= discard; state.consumed -= discard;
                }
            }
            let before = state.region.filled;
            let (used, event) = state.decoder.update(&state.compressed[state.offset..], Some(&mut state.region.as_buf(&mut state.inflated))).map_err(conversion_error)?;
            state.offset += used;
            if matches!(event, png::Decoded::ChunkComplete(kind) if kind == png::chunk::IEND) { state.done = true; }
            if used == 0 && before == state.region.filled && matches!(event, png::Decoded::Nothing) { return Err(conversion_error("PNG row decoder stalled")); }
        }
    }))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use crate::platform::{mocks::MemoryObjectStore, staging::owned_bytes_stream};

    fn fixture(color: png::ColorType, depth: png::BitDepth, palette: Vec<u8>, transparent: Vec<u8>, rows: Vec<Vec<u8>>) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 8, rows.len() as u32);
        encoder.set_color(color); encoder.set_depth(depth);
        if !palette.is_empty() { encoder.set_palette(palette); }
        if !transparent.is_empty() { encoder.set_trns(transparent); }
        let mut writer = encoder.write_header().unwrap();
        {
            let mut output = writer.stream_writer().unwrap();
            for row in rows { output.write_all(&row).unwrap(); }
            output.finish().unwrap();
        }
        writer.finish().unwrap();
        bytes
    }

    #[test]
    fn incremental_png_matches_native_palette_transparency_and_sixteen_bit_pixels() {
        futures::executor::block_on(async {
            let mut fixtures = Vec::new();
            for (bits, depth) in [(1u8, png::BitDepth::One), (2, png::BitDepth::Two), (4, png::BitDepth::Four), (8, png::BitDepth::Eight)] {
                let colors = 1usize << bits;
                let palette = (0..colors).flat_map(|index| [index as u8, (index * 37) as u8, (index * 71) as u8]).collect();
                let transparency = (0..colors).map(|index| (index * 113) as u8).collect();
                let rows: Vec<Vec<u8>> = (0..5).map(|y| {
                    let mut row = vec![0; usize::from(bits)];
                    for x in 0..8usize { row[x * usize::from(bits) / 8] |= (((x + y) % colors) as u8) << (8 - bits - (x * usize::from(bits) % 8) as u8); }
                    row
                }).collect();
                fixtures.push(fixture(png::ColorType::Indexed, depth, palette, transparency, rows.clone()));
                fixtures.push(fixture(png::ColorType::Grayscale, depth, Vec::new(), vec![0, 1], rows));
            }
            let rgb: Vec<Vec<u8>> = (0..5).map(|y| (0..8).flat_map(|x| [17 + x * 7, 34 + y * 19, 51 + x * 11]).collect()).collect();
            fixtures.push(fixture(png::ColorType::Rgb, png::BitDepth::Eight, Vec::new(), vec![0, 17, 0, 34, 0, 51], rgb));
            for color in [png::ColorType::GrayscaleAlpha, png::ColorType::Rgba] {
                let rows = (0..5u8).map(|y| (0..8u8).flat_map(|x| {
                    (0..color.samples()).map(move |channel| 13 + x * 7 + y * 11 + channel as u8 * 17)
                }).collect()).collect();
                fixtures.push(fixture(color, png::BitDepth::Eight, Vec::new(), Vec::new(), rows));
            }
            for color in [png::ColorType::Grayscale, png::ColorType::Rgb, png::ColorType::GrayscaleAlpha, png::ColorType::Rgba] {
                let rows = (0..5u16).map(|y| (0..8u16).flat_map(|x| {
                    (0..color.samples()).flat_map(move |channel| (1000 + x * 2048 + y * 1024 + channel as u16 * 7000).to_be_bytes())
                }).collect()).collect();
                let transparent = match color {
                    png::ColorType::Grayscale => 1000u16.to_be_bytes().to_vec(),
                    png::ColorType::Rgb => [1000u16, 8000, 15000].into_iter().flat_map(u16::to_be_bytes).collect(),
                    _ => Vec::new(),
                };
                fixtures.push(fixture(color, png::BitDepth::Sixteen, Vec::new(), transparent, rows));
            }
            let ini = IniSettings::parse("ResizeW=1\nResizeNumW=3\nResizeH=1\nResizeNumH=2\nGamma=1\nGammaValue=1.8\n").unwrap();
            for bytes in fixtures {
                let expected = aozora_epub3_lite::image::process(&bytes, "image/png", &ini, false, 0).unwrap();
                let expected = image::load_from_memory_with_format(&expected, image::ImageFormat::Png).unwrap().into_rgba16();
                let store = Arc::new(MemoryObjectStore::new());
                let source = store.stage_stream(owned_bytes_stream(bytes)).await.unwrap();
                let mut stream = try_stream(store, &source, &ini, false).await.unwrap().unwrap();
                let mut output = Vec::new();
                while let Some(chunk) = stream.next().await { let chunk = chunk.unwrap(); assert!(chunk.len() <= CHUNK); output.extend(chunk); }
                let actual = image::load_from_memory_with_format(&output, image::ImageFormat::Png).unwrap().into_rgba16();
                assert_eq!(actual, expected);
            }
        });
    }

    #[test]
    fn six_k_cover_uses_source_rows_and_preserves_alpha() {
        futures::executor::block_on(async {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, 6000, 6000);
            encoder.set_color(png::ColorType::Indexed); encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![13, 37, 193]); encoder.set_trns(vec![127]);
            let mut writer = encoder.write_header().unwrap();
            {
                let mut output = writer.stream_writer().unwrap();
                let row = vec![0; 6000];
                for _ in 0..6000 { output.write_all(&row).unwrap(); }
                output.finish().unwrap();
            }
            writer.finish().unwrap();
            assert!(bytes.len() < 16 * 1024 * 1024);
            let ini = IniSettings::parse("CoverW=658\nCoverH=905\n").unwrap();
            let transform = AnimationTransform::from_dimensions(&ini, true, 0, (6000, 6000)).unwrap();
            assert!(transform.source_row_memory_bytes(RowDepth::Eight).unwrap() < 2 * 1024 * 1024);
            let store = Arc::new(MemoryObjectStore::new());
            let source = store.stage_stream(owned_bytes_stream(bytes)).await.unwrap();
            let mut stream = try_stream(store, &source, &ini, true).await.unwrap().unwrap();
            let mut output = Vec::new();
            while let Some(chunk) = stream.next().await { let chunk = chunk.unwrap(); assert!(chunk.len() <= CHUNK); output.extend(chunk); }
            let image = image::load_from_memory_with_format(&output, image::ImageFormat::Png).unwrap().into_rgba8();
            assert_eq!(image.dimensions(), (658, 658));
            assert!(image.pixels().all(|pixel| pixel.0 == [13, 37, 193, 127]));
        });
    }

    #[test]
    fn incremental_png_rejects_corrupt_tail_after_emitting_rows() {
        futures::executor::block_on(async {
            let mut bytes = fixture(png::ColorType::Grayscale, png::BitDepth::Eight, Vec::new(), Vec::new(), vec![vec![23; 8]; 5]);
            *bytes.last_mut().unwrap() ^= 1;
            let store = Arc::new(MemoryObjectStore::new());
            let source = store.stage_stream(owned_bytes_stream(bytes)).await.unwrap();
            let ini = IniSettings::parse("ResizeW=1\nResizeNumW=3\n").unwrap();
            let mut stream = try_stream(store.clone(), &source, &ini, false).await.unwrap().unwrap();
            let mut failure = None;
            while let Some(chunk) = stream.next().await { if let Err(error) = chunk { failure = Some(error); break; } }
            assert!(failure.unwrap().to_string().contains("CRC"));
            assert_eq!(store.len(), 1);
        });
    }
}
