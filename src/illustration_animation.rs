//! Assembly of animated illustrations from a frame archive.
//!
//! Some sites publish an animated work as a ZIP of frames plus per-frame
//! timings instead of an animated image (Pixiv calls these うごイラ). The site
//! definition keeps that declarative: it appends the timings to the archive
//! URL as a query parameter —
//!
//! ```text
//! https://…/69642452_ugoira1920x1080.zip?ugoira=120,120,120
//! ```
//!
//! — and this module turns the archive into an APNG (the same output the
//! Python bridge produced with Pillow). Decoding handles JPEG and PNG frames;
//! frames of other formats fall back to the first decodable frame so the
//! illustration is still usable.
//!
//! Decoding needs the `image` and `zip` crates. Both are pure Rust (`zip` は
//! deflate/flate2(zlib-rs) のみを有効にしている)、ので portable (Worker / wasm)
//! ビルドでも `worker-runtime` 経由で有効になっている。`illustration-animation`
//! を外したビルドでは `assemble_animation` が未対応を返し、呼び出し側は取得した
//! バイト列をそのまま保存する。

#[cfg(feature = "illustration-animation")]
use std::io::Read;

use crate::error::{NarouError, Result};

/// Query parameter carrying per-frame delays in milliseconds.
const DELAY_PARAM: &str = "ugoira";

/// Frame delays declared on an illustration URL, if any.
pub fn frame_delays(url: &str) -> Option<Vec<u16>> {
    let query = url.split_once('?')?.1;
    let value = query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{DELAY_PARAM}=")))?;
    let delays: Vec<u16> = value
        .split(',')
        .filter_map(|delay| delay.trim().parse::<u16>().ok())
        .collect();
    (!delays.is_empty()).then_some(delays)
}

/// Assemble an animated image when `bytes` is a frame archive that the URL
/// declared timings for. Returns `None` for ordinary images, so callers can
/// keep their normal path.
pub fn assemble_animation(url: &str, bytes: &[u8]) -> Option<Result<Vec<u8>>> {
    if !is_zip(bytes) {
        return None;
    }
    let delays = frame_delays(url)?;
    Some(build_apng(bytes, &delays))
}

fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

/// Frame decoding and APNG assembly need the `zip` and `image` crates.
/// `illustration-animation` を外したビルドでは、呼び出し側が取得したアーカイブを
/// そのまま保存し、警告経路でこの未対応を報告する。
#[cfg(not(feature = "illustration-animation"))]
fn build_apng(_archive: &[u8], _delays: &[u16]) -> Result<Vec<u8>> {
    Err(NarouError::Platform(
        "assembling an animated illustration needs the zip and image crates".into(),
    ))
}

#[cfg(feature = "illustration-animation")]
fn build_apng(archive: &[u8], delays: &[u16]) -> Result<Vec<u8>> {
    let frames = decode_frames(archive)?;
    let first = frames
        .first()
        .ok_or_else(|| NarouError::Platform("animation archive has no frames".into()))?;
    let (width, height) = (first.width(), first.height());

    if frames.len() == 1 {
        // Nothing to animate; a plain PNG is the honest output.
        return encode_png(first);
    }

    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut sequence = 0u32;
    for (index, frame) in frames.iter().enumerate() {
        if frame.width() != width || frame.height() != height {
            return Err(NarouError::Platform(format!(
                "animation frame {} is {}x{}, expected {width}x{height}",
                index,
                frame.width(),
                frame.height()
            )));
        }
        let delay = u16::from(*delays.get(index).unwrap_or(&delays[delays.len() - 1]));
        let png = encode_png(frame)?;
        let (ihdr, idat) = split_png(&png)?;

        if index == 0 {
            chunks.push(png_chunk(b"IHDR", &ihdr));
            // acTL: num_frames (u32) then num_plays (u32, 0 = loop forever).
            let mut actl = Vec::with_capacity(8);
            actl.extend_from_slice(&(frames.len() as u32).to_be_bytes());
            actl.extend_from_slice(&0u32.to_be_bytes());
            chunks.push(png_chunk(b"acTL", &actl));
            chunks.push(frame_control(sequence, width, height, delay));
            sequence += 1;
            chunks.push(png_chunk(b"IDAT", &idat));
            continue;
        }

        chunks.push(frame_control(sequence, width, height, delay));
        sequence += 1;
        let mut payload = sequence.to_be_bytes().to_vec();
        payload.extend_from_slice(&idat);
        chunks.push(png_chunk(b"fdAT", &payload));
        sequence += 1;
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    for chunk in chunks {
        out.extend_from_slice(&chunk);
    }
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    Ok(out)
}

#[cfg(feature = "illustration-animation")]
fn frame_control(sequence: u32, width: u32, height: u32, delay_ms: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(26);
    payload.extend_from_slice(&sequence.to_be_bytes());
    payload.extend_from_slice(&width.to_be_bytes());
    payload.extend_from_slice(&height.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes()); // x offset
    payload.extend_from_slice(&0u32.to_be_bytes()); // y offset
    payload.extend_from_slice(&delay_ms.to_be_bytes());
    payload.extend_from_slice(&1000u16.to_be_bytes());
    payload.push(0); // dispose op: none
    payload.push(0); // blend op: source
    png_chunk(b"fcTL", &payload)
}

#[cfg(feature = "illustration-animation")]
fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(payload.len() + 12);
    chunk.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(payload);
    let mut crc_input = Vec::with_capacity(payload.len() + 4);
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(payload);
    chunk.extend_from_slice(&crc32fast::hash(&crc_input).to_be_bytes());
    chunk
}

#[cfg(feature = "illustration-animation")]
fn decode_frames(archive: &[u8]) -> Result<Vec<image::RgbaImage>> {
    let cursor = std::io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|error| NarouError::Platform(format!("animation archive: {error}")))?;

    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|index| zip.by_index(index).ok().map(|file| file.name().to_string()))
        .filter(|name| !name.ends_with('/'))
        .collect();
    names.sort();

    let mut frames = Vec::with_capacity(names.len());
    for name in names {
        let Ok(mut file) = zip.by_name(&name) else {
            continue;
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .map_err(|error| NarouError::Io(error))?;
        match image::load_from_memory(&data) {
            Ok(decoded) => frames.push(decoded.to_rgba8()),
            Err(error) => {
                tracing::debug!("animation frame {name} could not be decoded: {error}");
            }
        }
    }

    if frames.is_empty() {
        return Err(NarouError::Platform(
            "animation archive has no decodable frames".into(),
        ));
    }
    Ok(frames)
}

/// Encode one frame. Frames stay RGBA: an animated work may carry
/// transparency, and keeping one colour type for every frame is what the APNG
/// requires anyway.
#[cfg(feature = "illustration-animation")]
fn encode_png(frame: &image::RgbaImage) -> Result<Vec<u8>> {
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(frame.clone())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| NarouError::Platform(format!("animation frame encode: {error}")))?;
    Ok(png)
}

/// Split a PNG into its `IHDR` payload and the concatenated `IDAT` payload.
#[cfg(feature = "illustration-animation")]
fn split_png(png: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut cursor = 8usize; // skip the signature
    let mut ihdr = Vec::new();
    let mut idat = Vec::new();
    while cursor + 8 <= png.len() {
        let length = u32::from_be_bytes(
            png[cursor..cursor + 4]
                .try_into()
                .map_err(|_| NarouError::Platform("truncated PNG chunk".into()))?,
        ) as usize;
        let kind = &png[cursor + 4..cursor + 8];
        let payload_start = cursor + 8;
        let payload_end = payload_start + length;
        if payload_end > png.len() {
            break;
        }
        match kind {
            b"IHDR" => ihdr = png[payload_start..payload_end].to_vec(),
            b"IDAT" => idat.extend_from_slice(&png[payload_start..payload_end]),
            _ => {}
        }
        cursor = payload_end + 4; // skip the CRC
    }
    if ihdr.is_empty() || idat.is_empty() {
        return Err(NarouError::Platform("PNG encode produced no data".into()));
    }
    Ok((ihdr, idat))
}

#[cfg(all(test, feature = "illustration-animation"))]
mod tests {
    use super::*;
    use std::io::Write;

    fn png_frame(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba(color));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn zip_of(frames: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in frames {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer
    }

    /// Walk the chunk stream: `(kind, payload)` pairs, CRC skipped.
    fn chunks(apng: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut cursor = 8;
        while cursor + 8 <= apng.len() {
            let length =
                u32::from_be_bytes(apng[cursor..cursor + 4].try_into().unwrap()) as usize;
            let kind = String::from_utf8_lossy(&apng[cursor + 4..cursor + 8]).to_string();
            let start = cursor + 8;
            if start + length > apng.len() {
                break;
            }
            out.push((kind, apng[start..start + length].to_vec()));
            cursor = start + length + 4;
        }
        out
    }

    fn chunk_kinds(apng: &[u8]) -> Vec<String> {
        chunks(apng).into_iter().map(|(kind, _)| kind).collect()
    }

    #[test]
    fn delays_are_read_from_the_url_query() {
        assert_eq!(
            frame_delays("https://i.pximg.net/x.zip?ugoira=120,80,80"),
            Some(vec![120, 80, 80])
        );
        assert_eq!(frame_delays("https://i.pximg.net/x.zip"), None);
        assert_eq!(frame_delays("https://i.pximg.net/x.zip?ugoira="), None);
        assert_eq!(frame_delays("https://i.pximg.net/x.png"), None);
    }

    #[test]
    fn a_frame_archive_becomes_an_apng() {
        let archive = zip_of(&[
            ("000001.png", png_frame(2, 2, [255, 0, 0, 255])),
            ("000000.png", png_frame(2, 2, [0, 255, 0, 255])),
        ]);
        let apng = assemble_animation("https://i.pximg.net/x.zip?ugoira=120,80", &archive)
            .expect("archive should be recognised")
            .expect("assembly should succeed");

        assert_eq!(&apng[..8], b"\x89PNG\r\n\x1a\n");
        let kinds = chunk_kinds(&apng);
        assert_eq!(kinds[0], "IHDR", "{kinds:?}");
        assert_eq!(kinds[1], "acTL", "{kinds:?}");
        // 2 frames: fcTL+IDAT, then fcTL+fdAT
        assert_eq!(kinds[2..], ["fcTL", "IDAT", "fcTL", "fdAT", "IEND"]);

        // The frame count and the first delay live in acTL / the first fcTL.
        let parsed = chunks(&apng);
        let actl = parsed
            .iter()
            .find(|(kind, _)| kind == "acTL")
            .expect("acTL chunk");
        assert_eq!(u32::from_be_bytes(actl.1[..4].try_into().unwrap()), 2);
        let fctl = parsed
            .iter()
            .find(|(kind, _)| kind == "fcTL")
            .expect("fcTL chunk");
        // sequence(4) + width(4) + height(4) + x(4) + y(4) + delay_num(2)
        assert_eq!(u16::from_be_bytes(fctl.1[20..22].try_into().unwrap()), 120);
        assert_eq!(u16::from_be_bytes(fctl.1[22..24].try_into().unwrap()), 1000);
    }

    #[test]
    fn frames_are_ordered_by_name_not_by_archive_order() {
        let archive = zip_of(&[
            ("000002.png", png_frame(2, 2, [0, 0, 255, 255])),
            ("000000.png", png_frame(2, 2, [255, 0, 0, 255])),
            ("000001.png", png_frame(2, 2, [0, 255, 0, 255])),
        ]);
        let apng = assemble_animation("https://i.pximg.net/x.zip?ugoira=10,20,30", &archive)
            .unwrap()
            .unwrap();
        let kinds = chunk_kinds(&apng);
        assert_eq!(
            kinds.iter().filter(|kind| kind.as_str() == "fcTL").count(),
            3
        );
    }

    #[test]
    fn a_single_frame_archive_stays_a_plain_png() {
        let archive = zip_of(&[("000000.png", png_frame(2, 2, [1, 2, 3, 255]))]);
        let png = assemble_animation("https://i.pximg.net/x.zip?ugoira=100", &archive)
            .unwrap()
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(!chunk_kinds(&png).contains(&"acTL".to_string()));
    }

    #[test]
    fn frames_keep_their_alpha_channel() {
        // アニメ作品は半透明を含みうるので、全フレームを RGBA で揃える。
        for color in [[10, 20, 30, 255], [1, 2, 3, 128]] {
            let archive = zip_of(&[
                ("000000.png", png_frame(2, 2, color)),
                ("000001.png", png_frame(2, 2, [40, 50, 60, 255])),
            ]);
            let apng = assemble_animation("https://i.pximg.net/x.zip?ugoira=50,50", &archive)
                .unwrap()
                .unwrap();
            let ihdr = chunks(&apng)
                .into_iter()
                .find(|(kind, _)| kind == "IHDR")
                .expect("IHDR");
            assert_eq!(ihdr.1[8], 8, "bit depth");
            assert_eq!(ihdr.1[9], 6, "colour type 6 = RGBA (frame {color:?})");
        }
    }

    #[test]
    fn ordinary_images_are_not_treated_as_archives() {
        let png = png_frame(2, 2, [0, 0, 0, 255]);
        assert!(assemble_animation("https://i.pximg.net/x.png", &png).is_none());
        // A zip without declared delays is left to the caller as well.
        let archive = zip_of(&[("000000.png", png_frame(2, 2, [0, 0, 0, 255]))]);
        assert!(assemble_animation("https://i.pximg.net/x.zip", &archive).is_none());
    }
}
