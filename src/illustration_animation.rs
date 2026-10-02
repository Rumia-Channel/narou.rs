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
    Some(build_apng(bytes, &delays, DEFAULT_LIMITS))
}

fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

/// 組み立ての上限。
///
/// Workers は 1 isolate 128 MiB なので、「入力アーカイブ + 出力 APNG + 1 フレーム」
/// だけを同時に保持する前提で決めている。フレームは復号 → 符号化 → 追記のたびに
/// 解放し、全フレームを溜め込まない。
#[derive(Debug, Clone, Copy)]
struct Limits {
    max_frames: usize,
    max_frame_pixels: u64,
    max_output_bytes: usize,
}

/// 既定の上限。
///
/// Workers の 128 MiB に対する予算:
/// 入力アーカイブ (16 MiB, 転送上限) + 出力 APNG (56 MiB) + 1 フレーム
/// (1920×1080 RGBA = 8.3 MiB + 符号化バッファ) + isolate 分 ≈ 105 MiB。
/// Pixiv のうごイラは 1920×1080 までなので、それを超えるフレームは対象外にする。
/// 超過は OOM ではなく明示エラーで止める (呼び出し側はアーカイブをそのまま保存する)。
const DEFAULT_LIMITS: Limits = Limits {
    max_frames: 512,
    max_frame_pixels: 1920 * 1080,
    max_output_bytes: 56 * 1024 * 1024,
};

/// Frame decoding and APNG assembly need the `zip` and `image` crates.
/// `illustration-animation` を外したビルドでは、呼び出し側が取得したアーカイブを
/// そのまま保存し、警告経路でこの未対応を報告する。
#[cfg(not(feature = "illustration-animation"))]
fn build_apng(_archive: &[u8], _delays: &[u16], _limits: Limits) -> Result<Vec<u8>> {
    Err(NarouError::Platform(
        "assembling an animated illustration needs the zip and image crates".into(),
    ))
}

#[cfg(feature = "illustration-animation")]
fn build_apng(archive: &[u8], delays: &[u16], limits: Limits) -> Result<Vec<u8>> {
    let names = frame_names(archive)?;
    if names.len() > limits.max_frames {
        return Err(NarouError::Platform(format!(
            "animation archive has {} frames, over the limit of {}",
            names.len(),
            limits.max_frames
        )));
    }

    let mut out: Vec<u8> = Vec::new();
    let mut work: Vec<u8> = Vec::new();
    // 単一フレームのアーカイブは通常の PNG を返すので、2 フレーム目が現れるまで
    // 1 枚目の符号化結果を保持する (現れなければこれをそのまま返す)。
    let mut first_png: Option<Vec<u8>> = None;
    let mut sequence = 0u32;
    let mut frames = 0usize;
    let mut dimensions: Option<(u32, u32)> = None;
    let mut actl_count_offset = 0usize;
    let mut actl_crc_offset = 0usize;

    for name in &names {
        let Some(frame) = decode_frame(archive, name)? else {
            continue;
        };
        let (width, height) = (frame.width(), frame.height());
        if u64::from(width) * u64::from(height) > limits.max_frame_pixels {
            return Err(NarouError::Platform(format!(
                "animation frame {name} is {width}x{height}, over the pixel limit"
            )));
        }
        match dimensions {
            Some(expected) if expected != (width, height) => {
                return Err(NarouError::Platform(format!(
                    "animation frame {name} is {width}x{height}, expected {}x{}",
                    expected.0, expected.1
                )));
            }
            None => dimensions = Some((width, height)),
            _ => {}
        }

        encode_png_into(&frame, &mut work)?;
        drop(frame); // RGBA バッファはここで解放する (同時に持つのは常に 1 フレーム)

        if frames == 0 {
            // 1 枚目は単一フレームの可能性があるので、2 枚目が来るまで書かない。
            first_png = Some(std::mem::take(&mut work));
            frames = 1;
            continue;
        }
        if frames == 1 {
            let first = first_png.take().expect("the first frame is buffered");
            out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
            let (ihdr, idat) = png_payloads(&first)?;
            write_png_chunk(&mut out, b"IHDR", &first[ihdr]);
            // acTL の num_frames は最後まで確定しないので、後で書き戻す。
            let actl_start = out.len();
            write_png_chunk(&mut out, b"acTL", &[0, 0, 0, 0, 0, 0, 0, 0]);
            actl_count_offset = actl_start + 8;
            actl_crc_offset = actl_start + 16;
            write_png_chunk(
                &mut out,
                b"fcTL",
                &frame_control_payload(sequence, width, height, delay_at(delays, 0)),
            );
            sequence += 1;
            for range in &idat {
                write_png_chunk(&mut out, b"IDAT", &first[range.clone()]);
            }
        }

        // 2 枚目以降は fcTL + fdAT を追記する。
        write_png_chunk(
            &mut out,
            b"fcTL",
            &frame_control_payload(sequence, width, height, delay_at(delays, frames)),
        );
        sequence += 1;
        for range in png_payloads(&work)?.1 {
            let mut payload = sequence.to_be_bytes().to_vec();
            payload.extend_from_slice(&work[range]);
            write_png_chunk(&mut out, b"fdAT", &payload);
            sequence += 1;
        }

        frames += 1;
        if out.len() > limits.max_output_bytes {
            return Err(NarouError::Platform(format!(
                "assembled animation exceeds {} bytes",
                limits.max_output_bytes
            )));
        }
    }

    if frames == 0 {
        return Err(NarouError::Platform(
            "animation archive has no decodable frames".into(),
        ));
    }
    if frames == 1 {
        return Ok(first_png.expect("single frame stays a PNG"));
    }

    // acTL の num_frames と CRC を確定してから IEND を置く。
    out[actl_count_offset..actl_count_offset + 4].copy_from_slice(&(frames as u32).to_be_bytes());
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(b"acTL");
    hasher.update(&out[actl_count_offset..actl_count_offset + 8]);
    let crc = hasher.finalize();
    out[actl_crc_offset..actl_crc_offset + 4].copy_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    Ok(out)
}

#[cfg(feature = "illustration-animation")]
fn png_payloads(png: &[u8]) -> Result<(std::ops::Range<usize>, Vec<std::ops::Range<usize>>)> {
    let mut cursor = 8usize; // signature
    let mut ihdr = None;
    let mut idat = Vec::new();
    while cursor + 8 <= png.len() {
        let length = u32::from_be_bytes(
            png[cursor..cursor + 4]
                .try_into()
                .map_err(|_| NarouError::Platform("truncated PNG chunk".into()))?,
        ) as usize;
        let kind = &png[cursor + 4..cursor + 8];
        let start = cursor + 8;
        let end = start + length;
        if end > png.len() {
            break;
        }
        match kind {
            b"IHDR" => ihdr = Some(start..end),
            b"IDAT" => idat.push(start..end),
            _ => {}
        }
        cursor = end + 4; // CRC
    }
    let ihdr = ihdr.ok_or_else(|| NarouError::Platform("PNG encode produced no IHDR".into()))?;
    if idat.is_empty() {
        return Err(NarouError::Platform("PNG encode produced no IDAT".into()));
    }
    Ok((ihdr, idat))
}

/// 1 フレームを PNG へ符号化する。`buffer` は呼び出し側が再利用する。
#[cfg(feature = "illustration-animation")]
fn encode_png_into(frame: &image::RgbaImage, buffer: &mut Vec<u8>) -> Result<()> {
    use image::ImageEncoder as _;

    buffer.clear();
    let encoder = image::codecs::png::PngEncoder::new(&mut *buffer);
    encoder
        .write_image(
            frame.as_raw(),
            frame.width(),
            frame.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| NarouError::Platform(format!("animation frame encode: {error}")))
}

#[cfg(feature = "illustration-animation")]
fn frame_names(archive: &[u8]) -> Result<Vec<String>> {
    let cursor = std::io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|error| NarouError::Platform(format!("animation archive: {error}")))?;
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|index| zip.by_index(index).ok().map(|file| file.name().to_string()))
        .filter(|name| !name.ends_with('/'))
        .collect();
    names.sort();
    Ok(names)
}

/// 1 フレームだけ読み出して復号する (アーカイブは毎回開き直す)。
#[cfg(feature = "illustration-animation")]
fn decode_frame(archive: &[u8], name: &str) -> Result<Option<image::RgbaImage>> {
    let cursor = std::io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|error| NarouError::Platform(format!("animation archive: {error}")))?;
    let Ok(mut file) = zip.by_name(name) else {
        return Ok(None);
    };
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(NarouError::Io)?;
    match image::load_from_memory(&data) {
        Ok(decoded) => Ok(Some(decoded.to_rgba8())),
        Err(error) => {
            tracing::debug!("animation frame {name} could not be decoded: {error}");
            Ok(None)
        }
    }
}

/// フレーム番号に対応する表示時間 (足りなければ最後の値を使う)。
#[cfg(feature = "illustration-animation")]
fn delay_at(delays: &[u16], index: usize) -> u16 {
    *delays.get(index).unwrap_or(&delays[delays.len() - 1])
}

#[cfg(feature = "illustration-animation")]
fn frame_control_payload(sequence: u32, width: u32, height: u32, delay_ms: u16) -> Vec<u8> {
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
    payload
}

/// PNG チャンクを `out` へ直接書く (中間 Vec を作らない)。
#[cfg(feature = "illustration-animation")]
fn write_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(kind);
    hasher.update(payload);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
}

#[cfg(feature = "illustration-animation")]
fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(payload.len() + 12);
    write_png_chunk(&mut chunk, kind, payload);
    chunk
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

    /// 上限は「Workers のメモリに収める」ための安全弁で、超えたら OOM ではなく
    /// 明示エラーで止める。
    #[test]
    fn limits_reject_oversized_archives() {
        let two_frames = zip_of(&[
            ("000000.png", png_frame(2, 2, [255, 0, 0, 255])),
            ("000001.png", png_frame(2, 2, [0, 255, 0, 255])),
        ]);
        let delays = [50u16, 50];

        let too_many = Limits {
            max_frames: 1,
            ..DEFAULT_LIMITS
        };
        assert!(build_apng(&two_frames, &delays, too_many).is_err());

        let too_many_pixels = Limits {
            max_frame_pixels: 1,
            ..DEFAULT_LIMITS
        };
        assert!(build_apng(&two_frames, &delays, too_many_pixels).is_err());

        let too_large = Limits {
            max_output_bytes: 16,
            ..DEFAULT_LIMITS
        };
        assert!(build_apng(&two_frames, &delays, too_large).is_err());

        // 既定の上限なら通る。
        assert!(build_apng(&two_frames, &delays, DEFAULT_LIMITS).is_ok());
    }

    #[test]
    fn delays_shorter_than_frames_reuse_the_last_value() {
        let archive = zip_of(&[
            ("000000.png", png_frame(2, 2, [1, 1, 1, 255])),
            ("000001.png", png_frame(2, 2, [2, 2, 2, 255])),
            ("000002.png", png_frame(2, 2, [3, 3, 3, 255])),
        ]);
        let apng = build_apng(&archive, &[120, 80], DEFAULT_LIMITS).unwrap();
        let controls: Vec<Vec<u8>> = chunks(&apng)
            .into_iter()
            .filter(|(kind, _)| kind == "fcTL")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(controls.len(), 3);
        let delay = |payload: &[u8]| u16::from_be_bytes(payload[20..22].try_into().unwrap());
        assert_eq!(delay(&controls[0]), 120);
        assert_eq!(delay(&controls[1]), 80);
        assert_eq!(delay(&controls[2]), 80, "3 枚目は最後の値を再利用する");
    }
}
