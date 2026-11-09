use super::*;
use crate::{DecodingError, WebPDecoder, WebPEncoder};
use std::io::Cursor;

fn pixels(len: usize) -> Vec<u8> {
    let mut state = 0x12345678u32;
    (0..len).map(|_| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as u8
    }).collect()
}

fn collect(mut stream: OwnedWebPEncoder) -> Vec<u8> {
    let len = stream.encoded_len();
    let mut result = Vec::new();
    while let Some(chunk) = stream.next() {
        let chunk = chunk.unwrap();
        assert!(!chunk.is_empty());
        assert!(chunk.len() <= CHUNK_BYTES);
        result.extend(chunk);
    }
    assert_eq!(result.len() as u64, len);
    assert!(stream.next().is_none());
    result
}

fn decode(bytes: &[u8]) -> Vec<u8> {
    let mut decoder = WebPDecoder::new(Cursor::new(bytes)).unwrap();
    let mut data = vec![0; decoder.output_buffer_size().unwrap()];
    decoder.read_image(&mut data).unwrap();
    data
}

#[test]
fn owned_rgba_keeps_allocation_and_streams_below_clone_budget() {
    let data = pixels(128 * 128 * 4);
    let expected = data.clone();
    let pointer = data.as_ptr();
    let limit = data.capacity() + 32 * 1024;
    let stream = OwnedWebPEncoder::new_rgba8(data, 128, 128, limit).unwrap();
    assert_eq!(stream.plan.pixels.as_ptr(), pointer);
    let encoded = collect(stream);
    assert_eq!(decode(&encoded), expected);
}

#[test]
fn owned_rgb_reuses_spare_capacity_and_preserves_opaque_pixels() {
    let mut data = Vec::with_capacity(128 * 128 * 4);
    data.extend(pixels(128 * 128 * 3));
    let expected = data.clone();
    let pointer = data.as_ptr();
    let stream = OwnedWebPEncoder::new_rgb8(data, 128, 128, 128 * 128 * 4 + 32 * 1024).unwrap();
    assert_eq!(stream.plan.pixels.as_ptr(), pointer);
    assert_eq!(decode(&collect(stream)), expected);
}

#[test]
fn budget_counts_source_and_metadata_capacities() {
    let mut data = Vec::with_capacity(64 * 1024);
    data.extend_from_slice(&[1, 2, 3, 4]);
    assert!(matches!(OwnedWebPEncoder::new_rgba8(data, 1, 1, 16 * 1024),
        Err(EncodingError::MemoryLimitExceeded)));
    let mut icc_profile = Vec::with_capacity(64 * 1024);
    icc_profile.push(42);
    let metadata = EncoderMetadata { icc_profile, ..Default::default() };
    assert!(matches!(OwnedWebPEncoder::new_rgba8_with_options(vec![1, 2, 3, 4], 1, 1,
        16 * 1024, EncoderParams::default(), metadata), Err(EncodingError::MemoryLimitExceeded)));
    let mut encoder = WebPEncoder::new(io::sink());
    encoder.set_memory_limit(1);
    assert!(matches!(encoder.encode(&[1, 2, 3, 4], 1, 1, ColorType::Rgba8),
        Err(EncodingError::MemoryLimitExceeded)));
}

#[test]
fn counted_payload_matches_riff_and_arbitrary_pull_boundaries() {
    let data = pixels(33 * 17 * 4);
    let stream = OwnedWebPEncoder::new_rgba8(data.clone(), 33, 17, 128 * 1024).unwrap();
    let payload_len = stream.vp8l_len() as usize;
    let expected = collect(stream);
    assert_eq!(u32::from_le_bytes(expected[4..8].try_into().unwrap()) as usize + 8, expected.len());
    assert_eq!(u32::from_le_bytes(expected[16..20].try_into().unwrap()) as usize, payload_len);
    let raw = OwnedWebPEncoder::new_rgba8(data.clone(), 33, 17, 128 * 1024).unwrap().into_vp8l();
    assert_eq!(raw.encoded_len(), payload_len as u64);
    assert_eq!(collect(raw), expected[20..20 + payload_len]);
    for chunk_size in [1, 7, 63, 8191, 65536] {
        let mut encoder = OwnedWebPEncoder::new_rgba8(data.clone(), 33, 17, 128 * 1024).unwrap();
        let mut buffer = vec![0; chunk_size];
        let mut actual = Vec::new();
        loop {
            let n = encoder.read_output(&mut buffer).unwrap();
            if n == 0 { break; }
            actual.extend_from_slice(&buffer[..n]);
        }
        assert_eq!(actual, expected);
    }
}

#[test]
fn metadata_streams_across_chunks_and_roundtrips() {
    let data = pixels(17 * 19 * 4);
    let metadata = EncoderMetadata {
        icc_profile: vec![42; 100003], exif_metadata: vec![43; 65539], xmp_metadata: vec![44; 7],
    };
    let stream = OwnedWebPEncoder::new_rgba8_with_options(data.clone(), 17, 19,
        256 * 1024, EncoderParams::default(), metadata).unwrap();
    let encoded = collect(stream);
    assert_eq!(decode(&encoded), data);
    let mut decoder = WebPDecoder::new(Cursor::new(encoded)).unwrap();
    assert_eq!(decoder.icc_profile().unwrap().unwrap(), vec![42; 100003]);
    assert_eq!(decoder.exif_metadata().unwrap().unwrap(), vec![43; 65539]);
    assert_eq!(decoder.xmp_metadata().unwrap().unwrap(), vec![44; 7]);
    decoder.set_memory_limit(100002);
    assert!(matches!(decoder.icc_profile(), Err(DecodingError::MemoryLimitExceeded)));
}

#[test]
fn borrowed_colors_and_predictor_options_use_same_streaming_engine() {
    for color in [ColorType::L8, ColorType::La8, ColorType::Rgb8, ColorType::Rgba8] {
        for predictor in [false, true] {
            let data = pixels(17 * 19 * channels(color));
            let params = EncoderParams { use_predictor_transform: predictor };
            let stream = OwnedWebPEncoder::from_borrowed(&data, 17, 19, color,
                128 * 1024, params.clone(), EncoderMetadata::default()).unwrap();
            let streamed = collect(stream);
            let mut encoded = Vec::new();
            let mut encoder = WebPEncoder::new(&mut encoded);
            encoder.set_params(params);
            encoder.encode(&data, 17, 19, color).unwrap();
            assert_eq!(encoded, streamed);
            let expected: Vec<u8> = data.chunks_exact(channels(color)).flat_map(|pixel| match color {
                ColorType::L8 => vec![pixel[0], pixel[0], pixel[0]],
                ColorType::La8 => vec![pixel[0], pixel[0], pixel[0], pixel[1]],
                ColorType::Rgb8 | ColorType::Rgba8 => pixel.to_vec(),
            }).collect();
            assert_eq!(decode(&encoded), expected);
        }
    }
}

#[test]
fn encoded_frames_larger_than_sixteen_mib_are_not_retained_or_capped() {
    let data = pixels(2048 * 2048 * 4);
    let limit = data.capacity() + 128 * 1024;
    let stream = OwnedWebPEncoder::new_rgba8(data, 2048, 2048, limit).unwrap();
    let expected = stream.encoded_len();
    assert!(expected > 16 * 1024 * 1024);
    let mut written = 0u64;
    for chunk in stream {
        let chunk = chunk.unwrap();
        assert!(chunk.len() <= CHUNK_BYTES);
        written += chunk.len() as u64;
    }
    assert_eq!(written, expected);
}

#[test]
fn bit_flush_and_writer_failures_propagate_without_panicking() {
    struct Refuse;
    impl Write for Refuse {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> { Err(io::ErrorKind::PermissionDenied.into()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut bits = BitWriter { writer: Refuse, buffer: 1, nbits: 1 };
    assert_eq!(bits.flush().unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    let error = WebPEncoder::new(Refuse).encode(&[1, 2, 3, 4], 1, 1, ColorType::Rgba8).unwrap_err();
    assert!(matches!(error, EncodingError::IoError(error) if error.kind() == io::ErrorKind::PermissionDenied));
}
