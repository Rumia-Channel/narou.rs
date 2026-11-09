use std::io::{self, Cursor, Read};

use crate::huffman::HuffmanTree;
use crate::memory::MemoryBudget;
use crate::vp8::Vp8Decoder;
use crate::{ColorType, DecodingError, WebPDecoder, WebPEncoder};

// The original 2x2 fixture from decoder.rs, unchanged.
const LOSSY: &[u8] = &[
    0x52, 0x49, 0x46, 0x46, 0x3c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
    0x38, 0x20, 0x30, 0x00, 0x00, 0x00, 0xd0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x02, 0x00,
    0x02, 0x00, 0x02, 0x00, 0x34, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x01, 0xf8, 0x00, 0x03,
    0xb0, 0x00, 0xfe, 0xf0, 0xc4, 0x0b, 0xff, 0x20, 0xb9, 0x61, 0x75, 0xc8, 0xd7, 0xff,
    0x20, 0x3f, 0xe4, 0x07, 0xfc, 0x80, 0xff, 0xf8, 0xf2, 0x00, 0x00, 0x00,
];

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    fn put(&mut self, value: u32, count: usize) {
        for bit in 0..count {
            if self.len % 8 == 0 { self.bytes.push(0); }
            self.bytes[self.len / 8] |= (((value >> bit) & 1) as u8) << (self.len % 8);
            self.len += 1;
        }
    }

    fn group(&mut self, rgba: [u8; 4]) {
        for symbol in [rgba[1], rgba[0], rgba[2], rgba[3], 0] {
            self.put(1, 1); // simple code
            self.put(0, 1); // one symbol
            self.put(1, 1); // eight-bit first symbol
            self.put(u32::from(symbol), 8);
        }
    }

    fn subimage(&mut self, rgba: [u8; 4]) {
        self.put(0, 1); // no color cache; subimages cannot contain meta-Huffman images
        self.group(rgba);
    }

    fn header(width: u32, height: u32, alpha: bool) -> Self {
        let mut bits = Self::default();
        bits.put(0x2f, 8);
        bits.put(width - 1, 14);
        bits.put(height - 1, 14);
        bits.put(u32::from(alpha), 1);
        bits.put(0, 3);
        bits
    }

    fn finish(mut self) -> Vec<u8> {
        // Legal trailing padding also allows all fast-path bit-reader lookahead.
        self.bytes.extend_from_slice(&[0; 8]);
        self.bytes
    }
}

fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = tag.to_vec();
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    if data.len() % 2 != 0 { bytes.push(0); }
    bytes
}

fn riff(chunks: &[u8]) -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(chunks.len() as u32 + 4).to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(chunks);
    bytes
}

fn flat_lossless(width: u32, height: u32, alpha: bool, rgba: [u8; 4]) -> Vec<u8> {
    let mut bits = Bits::header(width, height, alpha);
    bits.put(0, 1); // no transforms
    bits.put(0, 1); // no color cache
    bits.put(0, 1); // no entropy image
    bits.group(rgba);
    bits.finish()
}

fn extended_header(width: u32, height: u32, flags: u8) -> Vec<u8> {
    let mut data = vec![flags, 0, 0, 0];
    data.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    data.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    chunk(b"VP8X", &data)
}

fn animated(frame_chunks: &[Vec<u8>], metadata: bool) -> Vec<u8> {
    let mut chunks = extended_header(2, 2, 0x12 | if metadata { 0x20 } else { 0 });
    if metadata { chunks.extend(chunk(b"ICCP", &[42; 8])); }
    chunks.extend(chunk(b"ANIM", &[0, 0, 0, 0, 3, 0]));
    for (index, frame) in frame_chunks.iter().enumerate() {
        let mut data = vec![0; 6]; // x/y offset
        data.extend_from_slice(&[1, 0, 0, 1, 0, 0]); // 2x2 frame
        data.extend_from_slice(&((index as u32 % 17) + 1).to_le_bytes()[..3]);
        data.push(2 | (index as u8 & 1)); // replace, alternating disposal
        data.extend_from_slice(frame);
        chunks.extend(chunk(b"ANMF", &data));
    }
    riff(&chunks)
}

fn decoded(bytes: &[u8], limit: usize) -> Result<Vec<u8>, DecodingError> {
    let mut decoder = WebPDecoder::new(Cursor::new(bytes))?;
    decoder.set_memory_limit(limit);
    let mut pixels = vec![0; decoder.output_buffer_size().unwrap()];
    decoder.read_image(&mut pixels)?;
    Ok(pixels)
}

fn minimum_limit(bytes: &[u8], animation: bool) -> usize {
    let accepts = |limit| {
        let mut decoder = WebPDecoder::new(Cursor::new(bytes)).unwrap();
        decoder.set_memory_limit(limit);
        let mut pixels = vec![0; decoder.output_buffer_size().unwrap()];
        let result = if animation { decoder.read_frame(&mut pixels).map(|_| ()) }
            else { decoder.read_image(&mut pixels) };
        match result {
            Ok(()) => true,
            Err(DecodingError::MemoryLimitExceeded) => false,
            Err(error) => panic!("unexpected decode error: {error:?}"),
        }
    };
    let (mut low, mut high) = (0, 1024 * 1024);
    assert!(accepts(high));
    while low < high {
        let middle = low + (high - low) / 2;
        if accepts(middle) { high = middle; } else { low = middle + 1; }
    }
    low
}

#[test]
fn lossless_rgb_rgba_and_repeated_reads_respect_limit() {
    for alpha in [false, true] {
        let color = [19, 73, 201, if alpha { 63 } else { 255 }];
        let bytes = riff(&chunk(b"VP8L", &flat_lossless(2, 2, alpha, color)));
        let expected = if alpha { color.to_vec().repeat(4) } else { color[..3].repeat(4) };
        let limit = minimum_limit(&bytes, false);
        assert!(limit > 0);
        assert!(matches!(decoded(&bytes, limit - 1), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(decoded(&bytes, limit).unwrap(), expected);
        let mut decoder = WebPDecoder::new(Cursor::new(&bytes)).unwrap();
        let mut pixels = vec![0; expected.len()];
        decoder.set_memory_limit(0);
        assert!(matches!(decoder.read_image(&mut pixels), Err(DecodingError::MemoryLimitExceeded)));
        decoder.set_memory_limit(limit);
        for _ in 0..64 {
            decoder.read_image(&mut pixels).unwrap();
            assert_eq!(pixels, expected);
        }
    }
}

#[test]
fn encoder_generated_lossless_pixels_are_unchanged() {
    for color_type in [ColorType::Rgb8, ColorType::Rgba8] {
        let channels = if matches!(color_type, ColorType::Rgb8) { 3 } else { 4 };
        let pixels: Vec<u8> = (0..19 * 17 * channels).map(|i| ((i * 37 + i / 7) % 256) as u8).collect();
        let mut bytes = Vec::new();
        WebPEncoder::new(&mut bytes).encode(&pixels, 19, 17, color_type).unwrap();
        assert!(matches!(decoded(&bytes, 1), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(decoded(&bytes, 1024 * 1024).unwrap(), pixels);
    }
}

#[test]
fn tiny_image_with_maximum_entropy_group_index_is_budgeted() {
    let mut bits = Bits::header(1, 1, true);
    bits.put(0, 1); // transforms
    bits.put(0, 1); // cache
    bits.put(1, 1); // entropy image
    bits.put(0, 3); // one 4x4 block, hence one metadata pixel
    bits.subimage([255, 255, 0, 255]); // group index 65535, despite a single output pixel
    for _ in 0..65536 { bits.group([19, 73, 201, 63]); }
    let bytes = riff(&chunk(b"VP8L", &bits.finish()));
    assert!(matches!(decoded(&bytes, 64 * 1024), Err(DecodingError::MemoryLimitExceeded)));
    assert_eq!(decoded(&bytes, 128 * 1024 * 1024).unwrap(), [19, 73, 201, 63]);
}

#[test]
fn huffman_primary_and_secondary_tables_share_budget() {
    let budget = MemoryBudget::new(1023);
    assert!(matches!(HuffmanTree::build_implicit(&[8; 256], &budget), Err(DecodingError::MemoryLimitExceeded)));
    let budget = MemoryBudget::new(1024);
    let tree = HuffmanTree::build_implicit(&[8; 256], &budget).unwrap();
    assert!(matches!(HuffmanTree::build_two_node(0, 1, &budget), Err(DecodingError::MemoryLimitExceeded)));
    drop(tree);
    assert!(HuffmanTree::build_two_node(0, 1, &budget).is_ok());

    // A complete prefix tree with codes longer than the ten-bit primary table.
    let lengths = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 11];
    let budget = MemoryBudget::new(4096);
    assert!(matches!(HuffmanTree::build_implicit(&lengths, &budget), Err(DecodingError::MemoryLimitExceeded)));
    let budget = MemoryBudget::new(8192);
    assert!(HuffmanTree::build_implicit(&lengths, &budget).is_ok());
}

#[test]
fn recursive_transform_images_and_color_cache_share_budget() {
    for transform in [0, 1] {
        let mut bits = Bits::header(8, 8, true);
        bits.put(1, 1);
        bits.put(transform, 2);
        bits.put(0, 3); // 4x4 transform blocks
        bits.subimage([0, 0, 0, 0]);
        bits.put(0, 1); // end transforms
        bits.put(1, 1); // maximum color cache
        bits.put(11, 4);
        bits.put(0, 1); // no entropy image
        bits.group([19, 73, 201, 63]);
        let bytes = riff(&chunk(b"VP8L", &bits.finish()));
        let expected = decoded(&bytes, 1024 * 1024).unwrap();
        let limit = minimum_limit(&bytes, false);
        assert!(limit > 8192);
        assert!(matches!(decoded(&bytes, limit - 1), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(decoded(&bytes, limit).unwrap(), expected);
    }
}

#[test]
fn palette_expansion_tables_and_rows_share_budget() {
    for colors in [2, 4, 16, 17, 256] {
        let mut bits = Bits::header(8, 8, true);
        bits.put(1, 1);
        bits.put(3, 2); // color indexing
        bits.put(colors - 1, 8);
        bits.subimage([19, 73, 201, 63]);
        bits.put(0, 1);
        bits.put(0, 1);
        bits.put(0, 1);
        bits.group([0; 4]); // all palette indices zero
        let bytes = riff(&chunk(b"VP8L", &bits.finish()));
        let expected = [19, 73, 201, 63].repeat(64);
        let limit = minimum_limit(&bytes, false);
        assert!(matches!(decoded(&bytes, limit - 1), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(decoded(&bytes, limit).unwrap(), expected);
    }
}

#[test]
fn lossy_planes_and_large_compressed_partitions_are_budgeted() {
    let expected = decoded(LOSSY, usize::MAX).unwrap();
    let limit = minimum_limit(LOSSY, false);
    assert!(matches!(decoded(LOSSY, limit - 1), Err(DecodingError::MemoryLimitExceeded)));
    assert_eq!(decoded(LOSSY, limit).unwrap(), expected);

    let mut raw = LOSSY[20..].to_vec();
    raw.extend_from_slice(&vec![0; 128 * 1024]);
    let bytes = riff(&chunk(b"VP8 ", &raw));
    assert!(matches!(decoded(&bytes, 16 * 1024), Err(DecodingError::MemoryLimitExceeded)));
    assert_eq!(decoded(&bytes, 1024 * 1024).unwrap(), expected);

    // The declared first partition must be rejected before trying to read its missing payload.
    let tag = (0x7ffffu32 << 5) | 0x10;
    let mut raw = LOSSY[20..].to_vec();
    raw[..3].copy_from_slice(&tag.to_le_bytes()[..3]);
    let bytes = riff(&chunk(b"VP8 ", &raw));
    assert!(matches!(decoded(&bytes, 16 * 1024), Err(DecodingError::MemoryLimitExceeded)));
}

#[test]
fn lossy_partition_short_reads_preserve_pixels() {
    struct ShortReads<'a>(&'a [u8]);
    impl Read for ShortReads<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let len = buffer.len().min(3);
            self.0.read(&mut buffer[..len])
        }
    }
    let expected = Vp8Decoder::decode_frame(&LOSSY[20..]).unwrap();
    let budget = MemoryBudget::new(64 * 1024);
    let actual = Vp8Decoder::decode_frame_with_budget(ShortReads(&LOSSY[20..]), &budget).unwrap();
    assert_eq!(actual.ybuf, expected.ybuf);
    assert_eq!(actual.ubuf, expected.ubuf);
    assert_eq!(actual.vbuf, expected.vbuf);
}

fn alpha_chunks(compressed: bool) -> Vec<u8> {
    let mut alpha = if compressed {
        let mut data = vec![1];
        data.extend_from_slice(&flat_lossless(2, 2, false, [0, 63, 0, 255])[5..]);
        data
    } else {
        vec![0, 63, 63, 63, 63]
    };
    let mut chunks = chunk(b"ALPH", &alpha);
    alpha.clear();
    chunks.extend_from_slice(&LOSSY[12..]);
    chunks
}

#[test]
fn compressed_and_raw_alpha_keep_simultaneous_buffers_budgeted() {
    let rgb = decoded(LOSSY, usize::MAX).unwrap();
    let expected: Vec<u8> = rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 63]).collect();
    for compressed in [false, true] {
        let mut chunks = extended_header(2, 2, 0x10);
        chunks.extend(alpha_chunks(compressed));
        let bytes = riff(&chunks);
        let limit = minimum_limit(&bytes, false);
        assert!(matches!(decoded(&bytes, limit - 1), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(decoded(&bytes, limit).unwrap(), expected);
    }
}

#[test]
fn successive_animation_frames_reuse_budget_and_preserve_timing() {
    let lossless = chunk(b"VP8L", &flat_lossless(2, 2, true, [19, 73, 201, 63]));
    let variants = [lossless, LOSSY[12..].to_vec(), alpha_chunks(false), alpha_chunks(true)];
    let frames: Vec<Vec<u8>> = (0..256).map(|i| variants[i % variants.len()].clone()).collect();
    let bytes = animated(&frames, false);
    let mut limited = WebPDecoder::new(Cursor::new(&bytes)).unwrap();
    let mut unlimited = WebPDecoder::new(Cursor::new(&bytes)).unwrap();
    limited.set_memory_limit(64 * 1024);
    assert_eq!(limited.num_frames(), 256);
    assert_eq!(limited.loop_count(), unlimited.loop_count());
    let mut actual = [0; 16];
    let mut expected = [0; 16];
    for index in 0..256 {
        let duration = limited.read_frame(&mut actual).unwrap();
        assert_eq!(duration, index % 17 + 1);
        assert_eq!(duration, unlimited.read_frame(&mut expected).unwrap());
        assert_eq!(actual, expected);
    }
    limited.set_memory_limit(0);
    assert!(matches!(limited.read_frame(&mut actual), Err(DecodingError::NoMoreFrames)));
    limited.set_memory_limit(64 * 1024);
    limited.reset_animation();
    limited.read_frame(&mut actual).unwrap();
    assert_eq!(actual, [19, 73, 201, 63].repeat(4).as_slice());
}

#[test]
fn retained_canvas_counts_on_next_frame_metadata_and_read_image() {
    let frame = chunk(b"VP8L", &flat_lossless(2, 2, true, [19, 73, 201, 63]));
    let bytes = animated(&[frame.clone(), frame.clone(), frame], true);
    let first_limit = minimum_limit(&bytes, true);
    let mut decoder = WebPDecoder::new(Cursor::new(&bytes)).unwrap();
    let mut pixels = [0; 16];
    decoder.set_memory_limit(first_limit);
    assert_eq!(decoder.read_frame(&mut pixels).unwrap(), 1);
    assert!(matches!(decoder.read_frame(&mut pixels), Err(DecodingError::MemoryLimitExceeded)));
    // read_image must count the saved canvas while independently decoding the first frame.
    assert!(matches!(decoder.read_image(&mut pixels), Err(DecodingError::MemoryLimitExceeded)));
    decoder.set_memory_limit(first_limit + 16);
    decoder.read_image(&mut pixels).unwrap();
    assert_eq!(decoder.read_frame(&mut pixels).unwrap(), 2);
    decoder.set_memory_limit(23);
    assert!(matches!(decoder.icc_profile(), Err(DecodingError::MemoryLimitExceeded)));
    decoder.set_memory_limit(24);
    assert_eq!(decoder.icc_profile().unwrap().unwrap(), [42; 8]);
    assert_eq!(decoder.icc_profile().unwrap().unwrap(), [42; 8]);
    decoder.set_memory_limit(first_limit + 16);
    assert_eq!(decoder.read_frame(&mut pixels).unwrap(), 3);
}

#[test]
fn metadata_allocation_failure_is_retryable() {
    let pixels = [19, 73, 201];
    let mut bytes = Vec::new();
    let mut encoder = WebPEncoder::new(&mut bytes);
    encoder.set_icc_profile(vec![42; 32]);
    encoder.encode(&pixels, 1, 1, ColorType::Rgb8).unwrap();
    let mut decoder = WebPDecoder::new(Cursor::new(bytes)).unwrap();
    decoder.set_memory_limit(31);
    assert!(matches!(decoder.icc_profile(), Err(DecodingError::MemoryLimitExceeded)));
    decoder.set_memory_limit(32);
    assert_eq!(decoder.icc_profile().unwrap().unwrap(), [42; 32]);
}
