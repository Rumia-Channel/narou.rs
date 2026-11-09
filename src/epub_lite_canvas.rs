//! Shared animation canvas with row decoding and spillable restore-previous state.

use std::sync::{Arc, Mutex};
use futures::{StreamExt, TryStreamExt};
use futures::io::{AsyncReadExt, BufReader};
use image::{DynamicImage, GenericImageView, Pixel, Rgba, RgbaImage};
use super::{Control, CHUNK, conversion_error};
use crate::error::Result;
use crate::platform::{AssetStore, AssetStream, ObjectKey, StagedAsset};

const MEMORY_SNAPSHOT: usize = 8 * 1024 * 1024;
const DECODE_WORKING_SET: u64 = 80 * 1024 * 1024;

/// Request-owned scratch identities survive cancellation of an image future.
/// The HTTP guard deletes these on drop; durable stage expiry covers isolate loss.
#[derive(Clone, Default)]
pub struct ImageScratch(Arc<Mutex<Vec<ObjectKey>>>);
impl ImageScratch {
    pub fn take_keys(&self) -> Vec<ObjectKey> { std::mem::take(&mut *self.0.lock().expect("image scratch poisoned")) }
    pub(super) fn remember(&self, key: ObjectKey) { self.0.lock().expect("image scratch poisoned").push(key); }
    pub(super) fn forget(&self, key: &ObjectKey) { self.0.lock().expect("image scratch poisoned").retain(|value| value != key); }
    pub async fn cleanup(&self, store: &dyn AssetStore, except: Option<&ObjectKey>) -> Result<()> {
        let keys = self.0.lock().expect("image scratch poisoned").clone();
        let mut failed = None;
        for key in keys {
            if except == Some(&key) { continue; }
            match store.delete(&key).await { Ok(()) => self.forget(&key), Err(error) => failed = Some(error) }
        }
        failed.map_or(Ok(()), Err)
    }
}

enum Saved { Memory(Vec<u8>), Asset(StagedAsset) }
struct Previous { rect: Control, data: Saved }
struct Parked { stage: StagedAsset, width: u32, height: u32, sixteen: bool }

pub(super) struct Canvas {
    pub image: Arc<DynamicImage>,
    pub background: Rgba<u8>,
    previous: Option<Previous>,
    last: Option<Control>,
    parked: Option<Parked>,
    store: Arc<dyn AssetStore>,
    scratch: ImageScratch,
}
impl Canvas {
    pub fn new(store: Arc<dyn AssetStore>, scratch: ImageScratch, width: u32, height: u32, background: Rgba<u8>, sixteen: bool) -> Result<Self> {
        super::dimensions(width, height)?;
        let bytes = u64::from(width) * u64::from(height) * if sixteen { 8 } else { 4 };
        // Encoded-frame growth can momentarily retain its old 8 MiB and new
        // 16 MiB allocation. Storage's part overlaps frame decoding, not growth.
        super::admit_image_memory(bytes + 24 * 1024 * 1024 + super::CODEC_SCRATCH_BYTES)?;
        let image = if sixteen {
            DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(width, height, Rgba(background.0.map(|value| u16::from(value) * 257))))
        } else { DynamicImage::ImageRgba8(RgbaImage::from_pixel(width, height, background)) };
        Ok(Self { image: Arc::new(image), background, previous: None, last: None, parked: None, store, scratch })
    }
    pub fn live_bytes(&self) -> u64 {
        self.image.as_bytes().len() as u64 + match &self.previous { Some(Previous { data: Saved::Memory(bytes), .. }) => bytes.len() as u64, _ => 0 }
    }
    fn mutable(&mut self) -> Result<&mut DynamicImage> {
        Arc::get_mut(&mut self.image).ok_or_else(|| conversion_error("previous animation frame still borrowed"))
    }
    fn validate(&self, control: Control) -> Result<()> {
        if control.x.checked_add(control.width).is_none_or(|value| value > self.image.width())
            || control.y.checked_add(control.height).is_none_or(|value| value > self.image.height()) {
            return Err(conversion_error("animation frame outside canvas"));
        }
        Ok(())
    }
    pub async fn park(&mut self) -> Result<()> {
        // Non-row codecs need ownership for in-place transforms. Preserve a
        // large compositing canvas in the staged tier instead of cloning it.
        if self.image.as_bytes().len() <= 32 * 1024 * 1024 || self.parked.is_some() { return Ok(()); }
        let width = self.image.width(); let height = self.image.height();
        let sixteen = self.image.color() == image::ColorType::Rgba16;
        let rect = Control { x: 0, y: 0, width, height, dispose: 0, blend: false, delay_num: 0, delay_den: 1, delay_ms: 0 };
        let stage = self.stage_region(rect).await?;
        self.image = Arc::new(DynamicImage::new_rgba8(0, 0));
        self.parked = Some(Parked { stage, width, height, sixteen });
        Ok(())
    }
    pub fn clear_background(&mut self) -> Result<()> {
        let background = self.background;
        let image = self.mutable()?.as_mut_rgba8().ok_or_else(|| conversion_error("WebP canvas format changed"))?;
        image.pixels_mut().for_each(|pixel| *pixel = background);
        Ok(())
    }
    pub async fn begin(&mut self, control: Control) -> Result<()> {
        if let Some(parked) = self.parked.take() {
            let image = if parked.sixteen { DynamicImage::new_rgba16(parked.width, parked.height) }
                else { DynamicImage::new_rgba8(parked.width, parked.height) };
            self.image = Arc::new(image);
            let rect = Control { x: 0, y: 0, width: parked.width, height: parked.height, dispose: 0, blend: false, delay_num: 0, delay_den: 1, delay_ms: 0 };
            self.restore(Previous { rect, data: Saved::Asset(parked.stage) }).await?;
        }
        self.validate(control)?;
        if let Some(last) = self.last.take() {
            match last.dispose {
                1 => {
                    let background = self.background;
                    let image = self.mutable()?;
                    for y in last.y..last.y + last.height { for x in last.x..last.x + last.width {
                        if let Some(image) = image.as_mut_rgba8() { image.put_pixel(x, y, background); }
                        else if let Some(image) = image.as_mut_rgba16() { image.put_pixel(x, y, Rgba(background.0.map(|value| u16::from(value) * 257))); }
                    } }
                }
                2 => {
                    let previous = self.previous.take().ok_or_else(|| conversion_error("restore-previous snapshot missing"))?;
                    self.restore(previous).await?;
                }
                _ => {}
            }
        }
        if control.dispose == 2 {
            let bytes = region_size(&self.image, control)?;
            let data = if bytes <= MEMORY_SNAPSHOT && self.image.as_bytes().len() <= 32 * 1024 * 1024 {
                let mut result = Vec::with_capacity(bytes);
                let mut stream = region_stream(self.image.clone(), control)?;
                while let Some(chunk) = stream.next().await { result.extend(chunk?); }
                Saved::Memory(result)
            } else { Saved::Asset(self.stage_region(control).await?) };
            self.previous = Some(Previous { rect: control, data });
        }
        self.last = Some(control);
        Ok(())
    }
    async fn stage_region(&self, control: Control) -> Result<StagedAsset> {
        let stage = self.store.stage_stream(region_stream(self.image.clone(), control)?).await?;
        self.scratch.remember(stage.key.clone());
        Ok(stage)
    }
    async fn restore(&mut self, previous: Previous) -> Result<()> {
        match previous.data {
            Saved::Memory(bytes) => self.write_region(previous.rect, 0, &bytes),
            Saved::Asset(stage) => {
                let stream = self.store.read_stream(&stage.key).await?.ok_or_else(|| conversion_error("canvas snapshot missing"))?;
                let mut reader = BufReader::with_capacity(CHUNK, stream.map_err(|error| std::io::Error::other(error.to_string())).into_async_read());
                let mut offset = 0usize;
                let total = usize::try_from(stage.size).map_err(conversion_error)?;
                while offset < total {
                    let mut bytes = vec![0; (total - offset).min(CHUNK)];
                    reader.read_exact(&mut bytes).await?;
                    self.write_region(previous.rect, offset, &bytes)?;
                    offset += bytes.len();
                }
                if reader.read(&mut [0]).await? != 0 { return Err(conversion_error("canvas snapshot length mismatch")); }
                self.store.delete(&stage.key).await?;
                self.scratch.forget(&stage.key);
                Ok(())
            }
        }
    }
    fn write_region(&mut self, rect: Control, mut offset: usize, mut bytes: &[u8]) -> Result<()> {
        let width = self.image.width() as usize;
        let bpp = self.image.color().bytes_per_pixel() as usize;
        let row = rect.width as usize * bpp;
        let image = self.mutable()?;
        while !bytes.is_empty() {
            let y = offset / row;
            let x = offset % row;
            let count = bytes.len().min(row - x);
            let target = ((rect.y as usize + y) * width + rect.x as usize) * bpp + x;
            if let Some(image) = image.as_mut_rgba8() { image.as_mut()[target..target + count].copy_from_slice(&bytes[..count]); }
            else if let Some(image) = image.as_mut_rgba16() {
                if !target.is_multiple_of(2) || !count.is_multiple_of(2) { return Err(conversion_error("unaligned 16-bit canvas snapshot")); }
                for (value, pair) in image.as_mut()[target / 2..(target + count) / 2].iter_mut().zip(bytes[..count].as_chunks::<2>().0) { *value = u16::from_ne_bytes(*pair); }
            } else { return Err(conversion_error("invalid canvas pixel format")); }
            bytes = &bytes[count..]; offset += count;
        }
        Ok(())
    }
    pub async fn png(&mut self, encoded: &[u8], capacity: usize, control: Control) -> Result<Arc<DynamicImage>> {
        let row_scratch = u64::from(control.width) * 8 * 3 + super::CODEC_SCRATCH_BYTES;
        super::admit_image_memory(self.live_bytes() + capacity as u64 + row_scratch + super::STORAGE_PART_BYTES)?;
        self.begin(control).await?;
        let mut decoder = png::Decoder::new(std::io::Cursor::new(encoded));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().map_err(conversion_error)?;
        let (color, depth) = reader.output_color_type();
        let channels = color.samples();
        let sixteen = depth == png::BitDepth::Sixteen;
        let sample_bytes = if sixteen { 2 } else { 1 };
        let passes: &[(u32, u32, u32, u32)] = if reader.info().interlaced {
            &[(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
        } else { &[(0, 0, 1, 1)] };
        let image = self.mutable()?;
        for &(start_x, start_y, step_x, step_y) in passes {
            if start_x >= control.width || start_y >= control.height { continue; }
            for y in (start_y..control.height).step_by(step_y as usize) {
                let row = reader.next_interlaced_row().map_err(conversion_error)?.ok_or_else(|| conversion_error("PNG row missing"))?;
                let row = row.data();
                let count = (control.width - start_x).div_ceil(step_x) as usize;
                if row.len() != count * channels * sample_bytes { return Err(conversion_error("PNG row length mismatch")); }
                for (index, x) in (start_x..control.width).step_by(step_x as usize).enumerate() {
                    let input = &row[index * channels * sample_bytes..];
                    if sixteen {
                        let value = |index: usize| u16::from_be_bytes([input[index * 2], input[index * 2 + 1]]);
                        let pixel = match color {
                            png::ColorType::Rgba => Rgba([value(0), value(1), value(2), value(3)]),
                            png::ColorType::Rgb => Rgba([value(0), value(1), value(2), u16::MAX]),
                            png::ColorType::GrayscaleAlpha => Rgba([value(0), value(0), value(0), value(1)]),
                            png::ColorType::Grayscale => Rgba([value(0), value(0), value(0), u16::MAX]),
                            _ => return Err(conversion_error("PNG palette was not expanded")),
                        };
                        let target = image.as_mut_rgba16().ok_or_else(|| conversion_error("PNG canvas depth mismatch"))?.get_pixel_mut(control.x + x, control.y + y);
                        if control.blend { target.blend(&pixel); } else { *target = pixel; }
                    } else {
                        let pixel = match color {
                            png::ColorType::Rgba => Rgba([input[0], input[1], input[2], input[3]]),
                            png::ColorType::Rgb => Rgba([input[0], input[1], input[2], 255]),
                            png::ColorType::GrayscaleAlpha => Rgba([input[0], input[0], input[0], input[1]]),
                            png::ColorType::Grayscale => Rgba([input[0], input[0], input[0], 255]),
                            _ => return Err(conversion_error("PNG palette was not expanded")),
                        };
                        let target = image.as_mut_rgba8().ok_or_else(|| conversion_error("PNG canvas depth mismatch"))?.get_pixel_mut(control.x + x, control.y + y);
                        if control.blend { target.blend(&pixel); } else { *target = pixel; }
                    }
                }
            }
        }
        if reader.next_interlaced_row().map_err(conversion_error)?.is_some() { return Err(conversion_error("extra PNG rows")); }
        Ok(self.image.clone())
    }
    pub async fn gif(&mut self, frame: &gif::Frame<'_>, palette: &[u8], control: Control) -> Result<Arc<DynamicImage>> {
        self.begin(control).await?;
        let target = self.mutable()?.as_mut_rgba8().ok_or_else(|| conversion_error("GIF canvas format"))?;
        for (index, &color) in frame.buffer.iter().enumerate() {
            if frame.transparent == Some(color) { continue; }
            let entry = palette.get(color as usize * 3..color as usize * 3 + 3).ok_or_else(|| conversion_error("GIF palette index out of bounds"))?;
            target.put_pixel(control.x + (index as u32 % control.width), control.y + (index as u32 / control.width), Rgba([entry[0], entry[1], entry[2], 255]));
        }
        Ok(self.image.clone())
    }
    pub async fn webp(&mut self, chunks: Vec<u8>, control: Control) -> Result<Arc<DynamicImage>> {
        self.begin(control).await?;
        let frame_bytes = u64::from(control.width) * u64::from(control.height) * 4;
        let spill = self.image.as_bytes().len() as u64 + frame_bytes > DECODE_WORKING_SET;
        let full = Control { x: 0, y: 0, width: self.image.width(), height: self.image.height(), ..control };
        let saved = if spill {
            let stage = self.stage_region(full).await?;
            self.image = Arc::new(DynamicImage::new_rgba8(0, 0));
            Some(stage)
        } else { None };
        let prepend = if chunks.capacity() - chunks.len() < 12 { chunks.len() as u64 + 12 } else { 0 };
        super::admit_image_memory(self.live_bytes() + chunks.capacity() as u64 + prepend + super::CODEC_SCRATCH_BYTES)?;
        let encoded = super::riff(chunks)?;
        let pixels = super::decode_pixels(&encoded, image::ImageFormat::WebP, self.live_bytes() + encoded.capacity() as u64)?;
        if pixels.dimensions() != (control.width, control.height) { return Err(conversion_error("WebP frame dimensions mismatch")); }
        if let Some(saved) = saved {
            let pixels = Arc::new(pixels);
            let frame = self.store.stage_stream(rgba_stream(pixels)).await?;
            self.scratch.remember(frame.key.clone());
            self.image = Arc::new(DynamicImage::new_rgba8(full.width, full.height));
            self.restore(Previous { rect: full, data: Saved::Asset(saved) }).await?;
            let stream = self.store.read_stream(&frame.key).await?.ok_or_else(|| conversion_error("WebP frame scratch missing"))?;
            let mut reader = stream.map_err(|error| std::io::Error::other(error.to_string())).into_async_read();
            let mut row = vec![0; control.width as usize * 4];
            for y in 0..control.height {
                reader.read_exact(&mut row).await?;
                self.draw_rgba_row(&row, y, control)?;
            }
            if reader.read(&mut [0]).await? != 0 { return Err(conversion_error("WebP frame scratch length mismatch")); }
            self.store.delete(&frame.key).await?; self.scratch.forget(&frame.key);
        } else {
            let target = self.mutable()?.as_mut_rgba8().ok_or_else(|| conversion_error("WebP canvas format"))?;
            for (x, y, pixel) in pixels.pixels() {
                let output = target.get_pixel_mut(control.x + x, control.y + y);
                if control.blend { output.blend(&pixel); } else { *output = pixel; }
            }
        }
        Ok(self.image.clone())
    }
    fn draw_rgba_row(&mut self, row: &[u8], y: u32, control: Control) -> Result<()> {
        let target = self.mutable()?.as_mut_rgba8().ok_or_else(|| conversion_error("canvas format"))?;
        for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
            let pixel = Rgba([pixel[0], pixel[1], pixel[2], pixel[3]]);
            let output = target.get_pixel_mut(control.x + x as u32, control.y + y);
            if control.blend { output.blend(&pixel); } else { *output = pixel; }
        }
        Ok(())
    }
    pub async fn finish(&mut self) -> Result<()> {
        if let Some(parked) = self.parked.take() {
            self.store.delete(&parked.stage.key).await?; self.scratch.forget(&parked.stage.key);
        }
        if let Some(Previous { data: Saved::Asset(stage), .. }) = self.previous.take() {
            self.store.delete(&stage.key).await?; self.scratch.forget(&stage.key);
        }
        Ok(())
    }
}
fn region_size(image: &DynamicImage, rect: Control) -> Result<usize> {
    usize::try_from(u64::from(rect.width) * u64::from(rect.height) * u64::from(image.color().bytes_per_pixel())).map_err(conversion_error)
}
fn region_stream(image: Arc<DynamicImage>, rect: Control) -> Result<AssetStream> {
    let size = region_size(&image, rect)?;
    let bpp = image.color().bytes_per_pixel() as usize;
    let row = rect.width as usize * bpp;
    let stride = image.width() as usize * bpp;
    Ok(Box::pin(futures::stream::unfold((image, 0usize), move |(image, offset)| async move {
        if offset == size { return None; }
        let y = offset / row; let x = offset % row;
        let count = (row - x).min(CHUNK);
        let start = (rect.y as usize + y) * stride + rect.x as usize * bpp + x;
        let bytes = image.as_bytes()[start..start + count].to_vec();
        Some((Ok(bytes), (image, offset + count)))
    })))
}

fn rgba_stream(image: Arc<DynamicImage>) -> AssetStream {
    Box::pin(futures::stream::unfold((image, 0u32, 0u32), |(image, mut x, mut y)| async move {
        if y == image.height() { return None; }
        let count = (image.width() - x).min((CHUNK / 4) as u32);
        let mut bytes = Vec::with_capacity(count as usize * 4);
        for pixel_x in x..x + count { bytes.extend_from_slice(&image.get_pixel(pixel_x, y).0); }
        x += count;
        if x == image.width() { x = 0; y += 1; }
        Some((Ok(bytes), (image, x, y)))
    }))
}
