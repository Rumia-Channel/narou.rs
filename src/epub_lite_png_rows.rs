//! PNG/APNG encoding drains each row before requesting the next decoded frame.

use std::io::Write;
use std::sync::Arc;

use aozora_epub3_lite::image::{AnimationTransform, RgbaRows};
use image::{DynamicImage, GenericImageView};
use super::{CHUNK, Decoder, Frame, conversion_error, apply_transform, be32, png_chunk};
use crate::error::Result;
use crate::platform::AssetStream;

type Sink = super::super::super::ChunkSink;

trait Rows: Send {
    const DEPTH: png::BitDepth;
    fn new(image: Arc<DynamicImage>, transform: &AnimationTransform) -> Result<Self> where Self: Sized;
    fn dimensions(&self) -> (u32, u32);
    fn next(&mut self) -> Option<&[u8]>;
}

impl Rows for RgbaRows<'static> {
    const DEPTH: png::BitDepth = png::BitDepth::Eight;
    fn new(image: Arc<DynamicImage>, transform: &AnimationTransform) -> Result<Self> {
        transform.rgba_rows_owned(image).map_err(conversion_error)
    }
    fn dimensions(&self) -> (u32, u32) { RgbaRows::dimensions(self) }
    fn next(&mut self) -> Option<&[u8]> { self.next_row() }
}

struct SixteenRows { image: Arc<DynamicImage>, row: u32, bytes: Vec<u8> }

impl Rows for SixteenRows {
    const DEPTH: png::BitDepth = png::BitDepth::Sixteen;
    fn new(image: Arc<DynamicImage>, transform: &AnimationTransform) -> Result<Self> {
        Ok(Self { image: apply_transform(transform, image)?, row: 0, bytes: Vec::new() })
    }
    fn dimensions(&self) -> (u32, u32) { self.image.dimensions() }
    fn next(&mut self) -> Option<&[u8]> {
        let Self { image, row, bytes } = self;
        if *row == image.height() { return None; }
        bytes.clear(); bytes.reserve(image.width() as usize * 8);
        for x in 0..image.width() {
            let pixel = match image.as_ref() {
                DynamicImage::ImageLuma16(value) => { let p = value.get_pixel(x, *row).0; [p[0], p[0], p[0], u16::MAX] },
                DynamicImage::ImageLumaA16(value) => { let p = value.get_pixel(x, *row).0; [p[0], p[0], p[0], p[1]] },
                DynamicImage::ImageRgb16(value) => { let p = value.get_pixel(x, *row).0; [p[0], p[1], p[2], u16::MAX] },
                DynamicImage::ImageRgba16(value) => value.get_pixel(x, *row).0,
                _ => unreachable!("sixteen-bit PNG row source"),
            };
            for sample in pixel { bytes.extend_from_slice(&sample.to_be_bytes()); }
        }
        *row += 1;
        Some(bytes)
    }
}

fn is_sixteen(image: &DynamicImage) -> bool {
    matches!(image.color(), image::ColorType::L16 | image::ColorType::La16 | image::ColorType::Rgb16 | image::ColorType::Rgba16)
}

fn new_rows<R: Rows>(image: Arc<DynamicImage>, transform: &AnimationTransform) -> Result<R> {
    if is_sixteen(&image) != (R::DEPTH == png::BitDepth::Sixteen) {
        return Err(conversion_error("transformed PNG frame geometry changed"));
    }
    let source_bytes = image.as_bytes().len() as u64;
    let previous = if Arc::strong_count(&image) > 1 && source_bytes <= 32 * 1024 * 1024 { source_bytes.min(8 * 1024 * 1024) } else { 0 };
    super::admit_image_memory(source_bytes + previous + transform.row_memory_bytes().map_err(conversion_error)?
        + u64::from(image.width().max(image.height())) * 8 * 3 + super::STORAGE_PART_BYTES + super::CODEC_SCRATCH_BYTES)?;
    R::new(image, transform)
}

struct State<R> {
    decoder: Decoder,
    transform: AnimationTransform,
    writer: Option<png::StreamWriter<'static, Sink>>,
    rows: Option<R>,
    format: (png::ColorType, png::BitDepth),
    dimensions: (u32, u32),
    animation: Option<(u32, u32)>,
    frame_index: u32,
    sequence: u32,
    delay: (u16, u16),
    default_image: bool,
    header_seen: bool,
    finished: bool,
    sink: Sink,
    output: Vec<u8>,
    offset: usize,
}

fn frame_writer(sink: Sink, dimensions: (u32, u32), format: (png::ColorType, png::BitDepth)) -> Result<png::StreamWriter<'static, Sink>> {
    let mut encoder = png::Encoder::new(sink, dimensions.0, dimensions.1);
    encoder.set_color(format.0);
    encoder.set_depth(format.1);
    encoder.write_header().map_err(conversion_error)?
        .into_stream_writer_with_size(CHUNK - 16).map_err(conversion_error)
}

impl<R> State<R> {
    fn next_sequence(&mut self) -> Result<[u8; 4]> {
        let sequence = self.sequence.to_be_bytes();
        self.sequence = self.sequence.checked_add(1).ok_or_else(|| conversion_error("APNG sequence overflow"))?;
        Ok(sequence)
    }

    fn drain_encoded(&mut self) -> Result<()> {
        let bytes = self.sink.take();
        if bytes.is_empty() { return Ok(()); }
        self.output.clear();
        self.offset = 0;
        let mut position = 0;
        if !self.header_seen {
            if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Err(conversion_error("PNG encoder signature missing")); }
            position = 8;
            self.header_seen = true;
        }
        // ChunkSink is drained only between synchronous encoder calls. Each
        // write_chunk finishes before returning, so these are complete chunks,
        // never a complete encoded frame. Keep png's tested static row encoder;
        // its 0.18 streaming APNG path incorrectly prefixes IDAT with a sequence.
        while position < bytes.len() {
            if bytes.len() - position < 12 { return Err(conversion_error("PNG encoder chunk truncated")); }
            let length = be32(&bytes[position..]) as usize;
            let end = position.checked_add(length).and_then(|end| end.checked_add(12))
                .filter(|&end| end <= bytes.len()).ok_or_else(|| conversion_error("PNG encoder chunk exceeds output"))?;
            let kind = &bytes[position + 4..position + 8];
            let payload = &bytes[position + 8..end - 4];
            match kind {
                b"IHDR" => {
                    if self.frame_index == 0 {
                        self.output.extend_from_slice(b"\x89PNG\r\n\x1a\n");
                        self.output.extend_from_slice(&bytes[position..end]);
                        if let Some((frames, loops)) = self.animation {
                            let mut animation = [0; 8];
                            animation[..4].copy_from_slice(&frames.to_be_bytes());
                            animation[4..].copy_from_slice(&loops.to_be_bytes());
                            png_chunk(&mut self.output, b"acTL", &animation);
                        }
                    }
                    if self.animation.is_some() && !self.default_image {
                        let mut control = [0; 26];
                        control[..4].copy_from_slice(&self.next_sequence()?);
                        control[4..8].copy_from_slice(&self.dimensions.0.to_be_bytes());
                        control[8..12].copy_from_slice(&self.dimensions.1.to_be_bytes());
                        control[20..22].copy_from_slice(&self.delay.0.to_be_bytes());
                        control[22..24].copy_from_slice(&self.delay.1.to_be_bytes());
                        png_chunk(&mut self.output, b"fcTL", &control);
                    }
                }
                b"IDAT" if self.frame_index != 0 => {
                    let sequence = self.next_sequence()?;
                    self.output.extend_from_slice(&((length + 4) as u32).to_be_bytes());
                    self.output.extend_from_slice(b"fdAT");
                    self.output.extend_from_slice(&sequence);
                    self.output.extend_from_slice(payload);
                    let mut crc = crc32fast::Hasher::new();
                    crc.update(b"fdAT"); crc.update(&sequence); crc.update(payload);
                    self.output.extend_from_slice(&crc.finalize().to_be_bytes());
                }
                b"IEND" => {}
                _ if self.frame_index == 0 => self.output.extend_from_slice(&bytes[position..end]),
                _ => return Err(conversion_error("unexpected PNG encoder frame chunk")),
            }
            position = end;
        }
        Ok(())
    }
}

pub(super) fn stream(decoder: Decoder, first: Frame, transform: AnimationTransform) -> Result<AssetStream> {
    // Keep row state inline without a large enum or an extra allocation/frame.
    if is_sixteen(&first.image) { stream_rows::<SixteenRows>(decoder, first, transform) }
    else { stream_rows::<RgbaRows<'static>>(decoder, first, transform) }
}

fn stream_rows<R: Rows + 'static>(decoder: Decoder, first: Frame, transform: AnimationTransform) -> Result<AssetStream> {
    let animation = match &decoder { Decoder::Png(value) => value.animation, _ => None };
    let delay = (first.delay_num, first.delay_den);
    let default_image = first.default_image;
    let rows = new_rows::<R>(first.image, &transform)?;
    let dimensions = rows.dimensions();
    let format = (png::ColorType::Rgba, R::DEPTH);
    let sink = Sink::new();
    let writer = frame_writer(sink.clone(), dimensions, format)?;
    let state = State {
        decoder, transform, writer: Some(writer), rows: Some(rows), format, dimensions, animation,
        frame_index: 0, sequence: 0, delay, default_image, header_seen: false, finished: false,
        sink, output: Vec::new(), offset: 0,
    };
    Ok(Box::pin(futures::stream::try_unfold(state, |mut state| async move {
        loop {
            if state.offset < state.output.len() {
                let end = (state.offset + CHUNK).min(state.output.len());
                let bytes = state.output[state.offset..end].to_vec(); state.offset = end;
                return Ok(Some((bytes, state)));
            }
            if state.finished { return Ok(None); }
            state.drain_encoded()?;
            if state.offset < state.output.len() { continue; }
            if let Some(rows) = &mut state.rows {
                if let Some(row) = rows.next() {
                    state.writer.as_mut().unwrap().write_all(row)?;
                    continue;
                }
                state.rows = None;
                state.writer.take().unwrap().finish().map_err(conversion_error)?;
                continue;
            }
            match state.decoder.next().await? {
                Some(frame) => {
                    let rows = new_rows::<R>(frame.image, &state.transform)?;
                    if rows.dimensions() != state.dimensions {
                        return Err(conversion_error("transformed PNG frame geometry changed"));
                    }
                    state.frame_index += 1;
                    state.delay = (frame.delay_num, frame.delay_den);
                    state.default_image = frame.default_image;
                    state.header_seen = false;
                    state.writer = Some(frame_writer(state.sink.clone(), state.dimensions, state.format)?);
                    state.rows = Some(rows);
                }
                None => {
                    state.output.clear(); state.offset = 0;
                    png_chunk(&mut state.output, b"IEND", &[]);
                    state.finished = true;
                }
            }
        }
    })))
}
