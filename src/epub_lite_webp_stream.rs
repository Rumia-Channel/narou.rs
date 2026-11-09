//! Owned lossless WebP frames are counted and replayed without encoded buffers.

use std::sync::Arc;
use aozora_epub3_lite::image::AnimationTransform;
use image::{DynamicImage, GenericImageView};
use super::{Decoder, Frame, apply_transform, conversion_error, put24, riff_chunk};
use crate::platform::AssetStream;


struct State {
    decoder: Decoder,
    first: Option<Frame>,
    transform: AnimationTransform,
    encoder: Option<image_webp::OwnedWebPEncoder>,
    padding: bool,
    animated: bool,
    repeat: u16,
    started: bool,
    dimensions: Option<(u32, u32)>,
}

pub(super) fn stream(decoder: Decoder, first: Frame, transform: AnimationTransform) -> AssetStream {
    let (animated, repeat) = match &decoder { Decoder::Webp(value) => (value.animated, value.repeat), _ => unreachable!("WebP decoder") };
    let state = State { decoder, first: Some(first), transform, encoder: None, padding: false, animated, repeat, started: false, dimensions: None };
    Box::pin(futures::stream::try_unfold(state, |mut state| async move {
        if let Some(encoder) = &mut state.encoder {
            if let Some(chunk) = encoder.next() {
                let chunk = chunk.map_err(conversion_error)?;
                if chunk.len() > super::CHUNK { return Err(conversion_error("WebP encoder exceeded chunk contract")); }
                return Ok(Some((chunk, state)));
            }
            state.encoder = None;
        }
        if state.padding { state.padding = false; return Ok(Some((vec![0], state))); }
        let frame = if let Some(frame) = state.first.take() { Some(frame) } else { state.decoder.next().await? };
        let Some(frame) = frame else { return Ok(None); };
        state.decoder.park().await?;
        let image = apply_transform(&state.transform, frame.image)?;
        let image = Arc::try_unwrap(image).map_err(|_| conversion_error("WebP output frame still borrowed"))?;
        let dimensions = image.dimensions();
        if state.dimensions.is_some_and(|expected| expected != dimensions) { return Err(conversion_error("WebP frame geometry changed")); }
        state.dimensions = Some(dimensions);
        let retained = state.decoder.live_bytes();
        let budget = if cfg!(target_arch = "wasm32") {
            usize::try_from(super::IMAGE_MEMORY_BUDGET.saturating_sub(retained + super::STORAGE_PART_BYTES + super::CODEC_SCRATCH_BYTES)).map_err(conversion_error)?
        } else { usize::MAX };
        let encoder = match image {
            DynamicImage::ImageRgba8(image) => image_webp::OwnedWebPEncoder::new_rgba8(image.into_raw(), dimensions.0, dimensions.1, budget),
            DynamicImage::ImageRgb8(image) => image_webp::OwnedWebPEncoder::new_rgb8(image.into_raw(), dimensions.0, dimensions.1, budget),
            _ => return Err(conversion_error("WebP decoder returned unsupported pixel format")),
        }.map_err(conversion_error)?;
        let length = encoder.vp8l_len();
        let mut prefix = Vec::new();
        if state.animated {
            if !state.started {
                let mut extended = vec![0x12, 0, 0, 0]; put24(&mut extended, dimensions.0 - 1); put24(&mut extended, dimensions.1 - 1);
                riff_chunk(&mut prefix, b"VP8X", &extended);
                let mut control = vec![0; 4]; control.extend_from_slice(&state.repeat.to_le_bytes()); riff_chunk(&mut prefix, b"ANIM", &control);
            }
            let frame_length = length.checked_add(24 + length % 2).ok_or_else(|| conversion_error("WebP frame length overflow"))?;
            prefix.extend_from_slice(b"ANMF"); prefix.extend_from_slice(&frame_length.to_le_bytes());
            prefix.extend_from_slice(&[0; 6]); put24(&mut prefix, dimensions.0 - 1); put24(&mut prefix, dimensions.1 - 1);
            put24(&mut prefix, frame.delay_ms); prefix.push(2);
        }
        prefix.extend_from_slice(b"VP8L"); prefix.extend_from_slice(&length.to_le_bytes());
        state.encoder = Some(encoder.into_vp8l());
        state.padding = !length.is_multiple_of(2);
        state.started = true;
        Ok(Some((prefix, state)))
    }))
}
