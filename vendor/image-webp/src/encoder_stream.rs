//! Owned, two-pass lossless encoding without retaining the compressed frame.

use std::io::{self, Write};
use std::mem::size_of;

use crate::encoder::{count_run, write_huffman_tree, write_run, write_single_entry_huffman_tree, BitWriter};
use crate::memory::{image_size, BudgetedVec, MemoryBudget};
use crate::{ColorType, EncoderParams, EncodingError};

const CHUNK_BYTES: usize = 64 * 1024;
// Four Huffman alphabets have at most 280 entries, each using at most seven bits
// to describe its code length, plus their small headers and transform descriptions.
const HEADER_BYTES: usize = 2048;

/// Optional metadata transferred into an owned streaming encoder without cloning.
#[derive(Default)]
pub struct EncoderMetadata {
    /// ICC color profile bytes.
    pub icc_profile: Vec<u8>,
    /// EXIF metadata bytes.
    pub exif_metadata: Vec<u8>,
    /// XMP metadata bytes.
    pub xmp_metadata: Vec<u8>,
}

impl EncoderMetadata {
    fn capacity(&self) -> Result<usize, EncodingError> {
        self.icc_profile.capacity().checked_add(self.exif_metadata.capacity())
            .and_then(|bytes| bytes.checked_add(self.xmp_metadata.capacity()))
            .ok_or(EncodingError::MemoryLimitExceeded)
    }

    fn is_empty(&self) -> bool {
        self.icc_profile.is_empty() && self.exif_metadata.is_empty() && self.xmp_metadata.is_empty()
    }
}

struct FixedBytes<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Default for FixedBytes<N> {
    fn default() -> Self { Self { bytes: [0; N], len: 0 } }
}

impl<const N: usize> Write for FixedBytes<N> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self.len.checked_add(bytes.len())
            .filter(|&end| end <= N).ok_or_else(|| io::Error::from(io::ErrorKind::WriteZero))?;
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

struct PreparedFrame {
    pixels: Vec<u8>,
    lengths: [[u8; 280]; 4],
    codes: [[u16; 280]; 4],
    header: FixedBytes<HEADER_BYTES>,
    header_bits: u64,
    header_nbits: u8,
    color: ColorType,
}

impl PreparedFrame {
    fn new(mut pixels: Vec<u8>, width: u32, height: u32, color: ColorType,
        params: EncoderParams, budget: &MemoryBudget) -> Result<Self, EncodingError>
    {
        let is_color = matches!(color, ColorType::Rgb8 | ColorType::Rgba8);
        let is_alpha = matches!(color, ColorType::La8 | ColorType::Rgba8);
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[0] = pixel[0].wrapping_sub(pixel[1]);
            pixel[2] = pixel[2].wrapping_sub(pixel[1]);
        }
        if params.use_predictor_transform {
            let row_bytes = width as usize * 4;
            for y in (1..height as usize).rev() {
                let (prev, current) = pixels[(y - 1) * row_bytes..][..row_bytes * 2].split_at_mut(row_bytes);
                for (c, p) in current.iter_mut().zip(prev) { *c = c.wrapping_sub(*p); }
            }
            for i in (4..row_bytes).rev() { pixels[i] = pixels[i].wrapping_sub(pixels[i - 4]); }
            pixels[3] = pixels[3].wrapping_sub(255);
        }

        let mut frequencies = [[0u32; 280]; 4];
        if !is_color { frequencies[0][0] = 1; frequencies[2][0] = 1; }
        if !is_alpha { frequencies[3][0] = 1; }
        let mut it = pixels.chunks_exact(4).peekable();
        while let Some(pixel) = it.next() {
            frequencies[1][pixel[1] as usize] += 1;
            if is_color {
                frequencies[0][pixel[0] as usize] += 1;
                frequencies[2][pixel[2] as usize] += 1;
            }
            if is_alpha { frequencies[3][pixel[3] as usize] += 1; }
            count_run(pixel, &mut it, &mut frequencies[1]);
        }

        let mut plan = Self {
            pixels, lengths: [[0; 280]; 4], codes: [[0; 280]; 4],
            header: FixedBytes::default(), header_bits: 0, header_nbits: 0, color,
        };
        let mut w = BitWriter { writer: &mut plan.header, buffer: 0, nbits: 0 };
        w.write_bits(0x2f, 8)?;
        w.write_bits(u64::from(width) - 1, 14)?;
        w.write_bits(u64::from(height) - 1, 14)?;
        w.write_bits(u64::from(is_alpha), 1)?;
        w.write_bits(0, 3)?;
        w.write_bits(0b101, 3)?; // subtract green
        if params.use_predictor_transform {
            w.write_bits(0b111001, 6)?;
            w.write_bits(0, 1)?;
            write_single_entry_huffman_tree(&mut w, 2)?;
            for _ in 0..4 { write_single_entry_huffman_tree(&mut w, 0)?; }
        }
        w.write_bits(0, 3)?; // end transforms; no cache or meta-Huffman image
        write_huffman_tree(&mut w, &frequencies[1], &mut plan.lengths[1], &mut plan.codes[1], budget)?;
        for channel in [0, 2] {
            if is_color {
                write_huffman_tree(&mut w, &frequencies[channel][..256],
                    &mut plan.lengths[channel][..256], &mut plan.codes[channel][..256], budget)?;
            } else { write_single_entry_huffman_tree(&mut w, 0)?; }
        }
        if is_alpha {
            write_huffman_tree(&mut w, &frequencies[3][..256],
                &mut plan.lengths[3][..256], &mut plan.codes[3][..256], budget)?;
        } else {
            write_single_entry_huffman_tree(&mut w, if params.use_predictor_transform { 0 } else { 255 })?;
        }
        write_single_entry_huffman_tree(&mut w, 1)?;
        plan.header_bits = w.buffer;
        plan.header_nbits = w.nbits;
        Ok(plan)
    }

    fn write_next<W: Write>(&self, w: &mut BitWriter<W>, next_pixel: &mut usize) -> io::Result<bool> {
        let mut it = self.pixels[*next_pixel * 4..].chunks_exact(4).peekable();
        let Some(pixel) = it.next() else { return Ok(false); };
        let mut code = u64::from(self.codes[1][pixel[1] as usize]);
        let mut length = self.lengths[1][pixel[1] as usize];
        if matches!(self.color, ColorType::Rgb8 | ColorType::Rgba8) {
            for channel in [0, 2] {
                code |= u64::from(self.codes[channel][pixel[channel] as usize]) << length;
                length += self.lengths[channel][pixel[channel] as usize];
            }
        }
        if matches!(self.color, ColorType::La8 | ColorType::Rgba8) {
            code |= u64::from(self.codes[3][pixel[3] as usize]) << length;
            length += self.lengths[3][pixel[3] as usize];
        }
        w.write_bits(code, length)?;
        write_run(w, pixel, &mut it, &self.codes[1], &self.lengths[1])?;
        *next_pixel = self.pixels.len() / 4 - it.len();
        Ok(true)
    }

    fn encoded_len(&self) -> Result<u32, EncodingError> {
        let mut w = BitWriter { writer: Counter(self.header.len as u64),
            buffer: self.header_bits, nbits: self.header_nbits };
        let mut pixel = 0;
        while self.write_next(&mut w, &mut pixel)? {}
        w.flush()?;
        u32::try_from(w.writer.0).map_err(|_| EncodingError::FileTooLarge)
    }
}

struct FrameCursor {
    header_position: usize,
    next_pixel: usize,
    bits: u64,
    nbits: u8,
    pending: FixedBytes<16>,
    pending_position: usize,
    finished: bool,
}

impl FrameCursor {
    fn new(plan: &PreparedFrame) -> Self {
        Self { header_position: 0, next_pixel: 0, bits: plan.header_bits,
            nbits: plan.header_nbits, pending: FixedBytes::default(), pending_position: 0, finished: false }
    }

    fn read(&mut self, plan: &PreparedFrame, output: &mut [u8]) -> Result<usize, EncodingError> {
        let mut written = 0;
        while written < output.len() {
            if self.header_position < plan.header.len {
                let count = (plan.header.len - self.header_position).min(output.len() - written);
                output[written..written + count].copy_from_slice(
                    &plan.header.bytes[self.header_position..self.header_position + count]);
                self.header_position += count;
                written += count;
            } else if self.pending_position < self.pending.len {
                let count = (self.pending.len - self.pending_position).min(output.len() - written);
                output[written..written + count].copy_from_slice(
                    &self.pending.bytes[self.pending_position..self.pending_position + count]);
                self.pending_position += count;
                written += count;
            } else if self.finished { break; }
            else {
                self.pending.len = 0;
                self.pending_position = 0;
                let mut w = BitWriter { writer: &mut self.pending, buffer: self.bits, nbits: self.nbits };
                if !plan.write_next(&mut w, &mut self.next_pixel)? {
                    w.flush()?;
                    self.finished = true;
                }
                self.bits = w.buffer;
                self.nbits = w.nbits;
            }
        }
        Ok(written)
    }
}

/// Pull-based lossless WebP encoder owning one source frame, not the encoded frame.
///
/// Construction transforms the owned pixel allocation in place, builds Huffman tables once,
/// and counts the encoded bytes. Iteration replays that plan into chunks of at most 64 KiB.
/// The budget includes pixel and metadata **capacities**, coding state, preparation workspace,
/// and the current output chunk. Chunks already returned to the caller are caller-owned.
/// Dropping this iterator immediately releases its source frame and metadata.
pub struct OwnedWebPEncoder {
    plan: PreparedFrame,
    cursor: FrameCursor,
    metadata: EncoderMetadata,
    width: u32,
    height: u32,
    frame_len: u32,
    total_len: u64,
    memory_limit: usize,
    retained_bytes: usize,
    stage: u8,
    stage_position: usize,
    emitted: u64,
    raw: bool,
    failed: bool,
}

fn channels(color: ColorType) -> usize {
    match color { ColorType::L8 => 1, ColorType::La8 => 2, ColorType::Rgb8 => 3, ColorType::Rgba8 => 4 }
}

fn validate(len: usize, width: u32, height: u32, color: ColorType) -> Result<(), EncodingError> {
    if width == 0 || width > 16384 || height == 0 || height > 16384 {
        return Err(EncodingError::InvalidDimensions);
    }
    let required = image_size(width, height, channels(color)).map_err(|_| EncodingError::MemoryLimitExceeded)?;
    if len != required { return Err(EncodingError::InvalidDimensions); }
    Ok(())
}

fn chunk_size(len: usize) -> Result<u64, EncodingError> {
    let len = u32::try_from(len).map_err(|_| EncodingError::FileTooLarge)?;
    Ok(u64::from(len) + u64::from(len & 1) + 8)
}

impl OwnedWebPEncoder {
    /// Takes ownership of RGBA8 pixels without cloning their allocation.
    pub fn new_rgba8(pixels: Vec<u8>, width: u32, height: u32, memory_limit: usize) -> Result<Self, EncodingError> {
        Self::new_rgba8_with_options(pixels, width, height, memory_limit,
            EncoderParams::default(), EncoderMetadata::default())
    }

    /// Takes ownership of RGB8 pixels, expanding them in place rather than cloning to RGBA.
    /// Reallocation, if needed, is fallible and accounts for both old and new capacities.
    pub fn new_rgb8(pixels: Vec<u8>, width: u32, height: u32, memory_limit: usize) -> Result<Self, EncodingError> {
        Self::from_owned(pixels, width, height, ColorType::Rgb8, memory_limit,
            EncoderParams::default(), EncoderMetadata::default(), false)
    }

    /// Takes owned RGBA8 pixels, encoder settings, and metadata under one memory budget.
    pub fn new_rgba8_with_options(pixels: Vec<u8>, width: u32, height: u32, memory_limit: usize,
        params: EncoderParams, metadata: EncoderMetadata) -> Result<Self, EncodingError>
    {
        Self::from_owned(pixels, width, height, ColorType::Rgba8, memory_limit, params, metadata, false)
    }

    fn from_owned(mut pixels: Vec<u8>, width: u32, height: u32, color: ColorType, memory_limit: usize,
        params: EncoderParams, metadata: EncoderMetadata, expanded: bool) -> Result<Self, EncodingError>
    {
        validate(pixels.len(), width, height, if expanded { ColorType::Rgba8 } else { color })?;
        let base = pixels.capacity().checked_add(metadata.capacity()?)
            .and_then(|bytes| bytes.checked_add(size_of::<Self>())).ok_or(EncodingError::MemoryLimitExceeded)?;
        let budget = MemoryBudget::new(memory_limit);
        let mut storage = budget.reserve(base).map_err(|_| EncodingError::MemoryLimitExceeded)?;
        let _frequencies = budget.reserve(4 * 280 * size_of::<u32>())
            .map_err(|_| EncodingError::MemoryLimitExceeded)?;
        if !expanded && channels(color) != 4 {
            let rgba_len = image_size(width, height, 4).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            let additional = rgba_len - pixels.len();
            storage.reserve_vec(&mut pixels, additional).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            pixels.resize(rgba_len, 0);
            for pixel in (0..rgba_len / 4).rev() {
                let source = pixel * channels(color);
                let rgba = match color {
                    ColorType::L8 => [pixels[source], pixels[source], pixels[source], 255],
                    ColorType::La8 => [pixels[source], pixels[source], pixels[source], pixels[source + 1]],
                    ColorType::Rgb8 => [pixels[source], pixels[source + 1], pixels[source + 2], 255],
                    ColorType::Rgba8 => unreachable!(),
                };
                pixels[pixel * 4..][..4].copy_from_slice(&rgba);
            }
        }
        let plan = PreparedFrame::new(pixels, width, height, color, params, &budget)?;
        let frame_len = plan.encoded_len()?;
        let mut riff_len = 4 + chunk_size(frame_len as usize)?;
        if !metadata.is_empty() {
            riff_len += 18;
            for data in [&metadata.icc_profile, &metadata.exif_metadata, &metadata.xmp_metadata] {
                if !data.is_empty() { riff_len += chunk_size(data.len())?; }
            }
        }
        if riff_len > u64::from(u32::MAX) { return Err(EncodingError::FileTooLarge); }
        let retained_bytes = plan.pixels.capacity().checked_add(metadata.capacity()?)
            .and_then(|bytes| bytes.checked_add(size_of::<Self>())).ok_or(EncodingError::MemoryLimitExceeded)?;
        let cursor = FrameCursor::new(&plan);
        Ok(Self { plan, cursor, metadata, width, height, frame_len, total_len: riff_len + 8,
            memory_limit, retained_bytes, stage: 0, stage_position: 0, emitted: 0, raw: false, failed: false })
    }

    pub(crate) fn from_borrowed(data: &[u8], width: u32, height: u32, color: ColorType, memory_limit: usize,
        params: EncoderParams, metadata: EncoderMetadata) -> Result<Self, EncodingError>
    {
        assert_eq!((u64::from(width) * u64::from(height)).saturating_mul(channels(color) as u64), data.len() as u64);
        validate(data.len(), width, height, color)?;
        // Borrowed APIs must own their working pixels, but allocate only the final RGBA buffer.
        let pixels = {
            let budget = MemoryBudget::new(memory_limit);
            let _metadata = budget.reserve(metadata.capacity()?).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            let len = image_size(width, height, 4).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            let mut pixels = BudgetedVec::filled(&budget, len, 0).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            for (source, target) in data.chunks_exact(channels(color)).zip(pixels.chunks_exact_mut(4)) {
                let rgba = match color {
                    ColorType::L8 => [source[0], source[0], source[0], 255],
                    ColorType::La8 => [source[0], source[0], source[0], source[1]],
                    ColorType::Rgb8 => [source[0], source[1], source[2], 255],
                    ColorType::Rgba8 => [source[0], source[1], source[2], source[3]],
                };
                target.copy_from_slice(&rgba);
            }
            pixels.into_vec()
        };
        // Pixels are already expanded; retain the original color's coding behavior.
        Self::from_owned(pixels, width, height, color, memory_limit, params, metadata, true)
    }

    /// Total bytes emitted in the selected mode, including RIFF/chunk headers and padding by default.
    pub fn encoded_len(&self) -> u64 { if self.raw { u64::from(self.frame_len) } else { self.total_len } }

    /// Length of the raw VP8L payload, excluding its chunk header and optional padding byte.
    pub fn vp8l_len(&self) -> u32 { self.frame_len }

    /// Restarts output as raw VP8L payload bytes for wrapping in an ANMF frame.
    /// Metadata and the RIFF/VP8L chunk headers are not emitted in this mode.
    pub fn into_vp8l(mut self) -> Self {
        self.cursor = FrameCursor::new(&self.plan);
        self.stage = 6;
        self.stage_position = 0;
        self.emitted = 0;
        self.raw = true;
        self.failed = false;
        self.retained_bytes -= self.metadata.icc_profile.capacity()
            + self.metadata.exif_metadata.capacity() + self.metadata.xmp_metadata.capacity();
        self.metadata = EncoderMetadata::default();
        self
    }

    fn read_output(&mut self, output: &mut [u8]) -> Result<usize, EncodingError> {
        let mut written = 0;
        while written < output.len() && self.stage < 14 {
            if self.stage == 6 {
                let count = self.cursor.read(&self.plan, &mut output[written..])?;
                written += count;
                if count == 0 { self.stage = if self.raw { 14 } else { 7 }; self.stage_position = 0; }
                continue;
            }
            let mut header = [0; 18];
            let bytes: &[u8] = match self.stage {
                0 => {
                    header[..4].copy_from_slice(b"RIFF");
                    header[4..8].copy_from_slice(&((self.total_len - 8) as u32).to_le_bytes());
                    header[8..12].copy_from_slice(b"WEBP");
                    &header[..12]
                }
                1 if !self.metadata.is_empty() => {
                    header[..4].copy_from_slice(b"VP8X");
                    header[4..8].copy_from_slice(&10u32.to_le_bytes());
                    header[8] = (if self.metadata.xmp_metadata.is_empty() { 0 } else { 4 })
                        | (if self.metadata.exif_metadata.is_empty() { 0 } else { 8 })
                        | (if matches!(self.plan.color, ColorType::La8 | ColorType::Rgba8) { 16 } else { 0 })
                        | (if self.metadata.icc_profile.is_empty() { 0 } else { 32 });
                    header[12..15].copy_from_slice(&(self.width - 1).to_le_bytes()[..3]);
                    header[15..18].copy_from_slice(&(self.height - 1).to_le_bytes()[..3]);
                    &header
                }
                2 | 5 | 8 | 11 => {
                    let (tag, len) = match self.stage {
                        2 => (b"ICCP", self.metadata.icc_profile.len()),
                        5 => (b"VP8L", self.frame_len as usize),
                        8 => (b"EXIF", self.metadata.exif_metadata.len()),
                        _ => (b"XMP ", self.metadata.xmp_metadata.len()),
                    };
                    if len == 0 { &[] } else {
                        header[..4].copy_from_slice(tag);
                        header[4..8].copy_from_slice(&(len as u32).to_le_bytes());
                        &header[..8]
                    }
                }
                3 => &self.metadata.icc_profile,
                9 => &self.metadata.exif_metadata,
                12 => &self.metadata.xmp_metadata,
                4 | 7 | 10 | 13 => {
                    let len = match self.stage {
                        4 => self.metadata.icc_profile.len(), 7 => self.frame_len as usize,
                        10 => self.metadata.exif_metadata.len(), _ => self.metadata.xmp_metadata.len(),
                    };
                    &header[..len & 1]
                }
                _ => &[],
            };
            let count = (bytes.len() - self.stage_position).min(output.len() - written);
            output[written..written + count].copy_from_slice(&bytes[self.stage_position..self.stage_position + count]);
            written += count;
            self.stage_position += count;
            if self.stage_position == bytes.len() { self.stage += 1; self.stage_position = 0; }
        }
        self.emitted += written as u64;
        Ok(written)
    }

    /// Streams this encoder into a writer without collecting its encoded output.
    pub fn write_to<W: Write>(mut self, mut writer: W) -> Result<(), EncodingError> {
        let mut bytes = [0; 8192];
        let budget = MemoryBudget::new(self.memory_limit);
        let _retained = budget.reserve(self.retained_bytes).map_err(|_| EncodingError::MemoryLimitExceeded)?;
        let _output = budget.reserve(bytes.len()).map_err(|_| EncodingError::MemoryLimitExceeded)?;
        loop {
            let count = self.read_output(&mut bytes)?;
            if count == 0 { break; }
            writer.write_all(&bytes[..count])?;
        }
        Ok(())
    }
}

impl Iterator for OwnedWebPEncoder {
    type Item = Result<Vec<u8>, EncodingError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.emitted == self.encoded_len() { return None; }
        let remaining = self.encoded_len() - self.emitted;
        let available = self.memory_limit.saturating_sub(self.retained_bytes);
        let len = remaining.min(CHUNK_BYTES as u64).min(available as u64) as usize;
        let result = (|| {
            if len == 0 { return Err(EncodingError::MemoryLimitExceeded); }
            let budget = MemoryBudget::new(self.memory_limit);
            let _retained = budget.reserve(self.retained_bytes).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            let mut bytes = BudgetedVec::filled(&budget, len, 0).map_err(|_| EncodingError::MemoryLimitExceeded)?;
            let count = self.read_output(&mut bytes)?;
            if count != len { return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()); }
            Ok(bytes.into_vec())
        })();
        if result.is_err() { self.failed = true; }
        Some(result)
    }
}

impl std::iter::FusedIterator for OwnedWebPEncoder {}

#[cfg(test)]
#[path = "encoder_stream_tests.rs"]
mod tests;
