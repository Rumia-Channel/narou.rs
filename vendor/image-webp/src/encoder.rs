//! Encoding of WebP images.
use std::collections::BinaryHeap;
use std::io::{self, Write};
use std::slice::ChunksExact;

use quick_error::quick_error;
use crate::encoder_stream::{EncoderMetadata, OwnedWebPEncoder};
use crate::memory::MemoryBudget;

/// Color type of the image.
///
/// Note that the WebP format doesn't have a concept of color type. All images are encoded as RGBA
/// and some decoders may treat them as such. This enum is used to indicate the color type of the
/// input data provided to the encoder, which can help improve compression ratio.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorType {
    /// Opaque image with a single luminance byte per pixel.
    L8,
    /// Image with a luminance and alpha byte per pixel.
    La8,
    /// Opaque image with a red, green, and blue byte per pixel.
    Rgb8,
    /// Image with a red, green, blue, and alpha byte per pixel.
    Rgba8,
}

quick_error! {
    /// Error that can occur during encoding.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum EncodingError {
        /// An IO error occurred.
        IoError(err: io::Error) {
            from()
            display("IO error: {}", err)
            source(err)
        }

        /// The image dimensions are not allowed by the WebP format.
        InvalidDimensions {
            display("Invalid dimensions")
        }

        /// The configured working-memory budget or the allocator's capacity was exceeded.
        MemoryLimitExceeded {
            display("Memory limit exceeded")
        }

        /// The encoded container or a metadata chunk exceeds WebP's 32-bit length field.
        FileTooLarge {
            display("Encoded WebP is too large")
        }
    }
}

pub(crate) struct BitWriter<W> {
    pub(crate) writer: W,
    pub(crate) buffer: u64,
    pub(crate) nbits: u8,
}

impl<W: Write> BitWriter<W> {
    pub(crate) fn write_bits(&mut self, bits: u64, nbits: u8) -> io::Result<()> {
        debug_assert!(nbits <= 64);

        self.buffer |= bits << self.nbits;
        self.nbits += nbits;

        if self.nbits >= 64 {
            self.writer.write_all(&self.buffer.to_le_bytes())?;
            self.nbits -= 64;
            self.buffer = bits.checked_shr(u32::from(nbits - self.nbits)).unwrap_or(0);
        }
        debug_assert!(self.nbits < 64);
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if self.nbits % 8 != 0 {
            self.write_bits(0, 8 - self.nbits % 8)?;
        }
        if self.nbits > 0 {
            self.writer
                .write_all(&self.buffer.to_le_bytes()[..self.nbits as usize / 8])?;
            self.buffer = 0;
            self.nbits = 0;
        }
        Ok(())
    }
}

pub(crate) fn write_single_entry_huffman_tree<W: Write>(w: &mut BitWriter<W>, symbol: u8) -> io::Result<()> {
    w.write_bits(1, 2)?;
    if symbol <= 1 {
        w.write_bits(0, 1)?;
        w.write_bits(u64::from(symbol), 1)?;
    } else {
        w.write_bits(1, 1)?;
        w.write_bits(u64::from(symbol), 8)?;
    }
    Ok(())
}

fn build_huffman_tree(
    frequencies: &[u32],
    lengths: &mut [u8],
    codes: &mut [u16],
    length_limit: u8,
    budget: &MemoryBudget,
) -> Result<bool, EncodingError> {
    assert_eq!(frequencies.len(), lengths.len());
    assert_eq!(frequencies.len(), codes.len());

    if frequencies.iter().filter(|&&f| f > 0).count() <= 1 {
        lengths.fill(0);
        codes.fill(0);
        return Ok(false);
    }

    #[derive(Eq, PartialEq, Copy, Clone, Debug)]
    struct Item(u32, u16);
    impl Ord for Item {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            other.0.cmp(&self.0)
        }
    }
    impl PartialOrd for Item {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    // Build a huffman tree
    let mut allocation = budget.reserve(0).map_err(|_| EncodingError::MemoryLimitExceeded)?;
    let mut internal_nodes = Vec::new();
    allocation.reserve_vec(&mut internal_nodes, frequencies.len())
        .map_err(|_| EncodingError::MemoryLimitExceeded)?;
    let mut node_storage = Vec::new();
    allocation.reserve_vec(&mut node_storage, frequencies.len())
        .map_err(|_| EncodingError::MemoryLimitExceeded)?;
    for (i, &frequency) in frequencies.iter().enumerate() {
        if frequency > 0 {
            node_storage.push(Item(frequency, i as u16));
        }
    }
    let mut nodes = BinaryHeap::from(node_storage);
    while nodes.len() > 1 {
        let Item(frequency1, index1) = nodes.pop().unwrap();
        let mut root = nodes.peek_mut().unwrap();
        internal_nodes.push((index1, root.1));
        *root = Item(
            frequency1 + root.0,
            internal_nodes.len() as u16 + frequencies.len() as u16 - 1,
        );
    }

    // Walk the tree to assign code lengths
    lengths.fill(0);
    let mut stack = Vec::new();
    allocation.reserve_vec(&mut stack, frequencies.len())
        .map_err(|_| EncodingError::MemoryLimitExceeded)?;
    stack.push((nodes.pop().unwrap().1, 0));
    while let Some((node, depth)) = stack.pop() {
        let node = node as usize;
        if node < frequencies.len() {
            lengths[node] = depth as u8;
        } else {
            let (left, right) = internal_nodes[node - frequencies.len()];
            stack.push((left, depth + 1));
            stack.push((right, depth + 1));
        }
    }

    // Limit the codes to length length_limit
    let mut max_length = 0;
    for &length in lengths.iter() {
        max_length = max_length.max(length);
    }
    if max_length > length_limit {
        let mut counts = [0u32; 16];
        for &length in lengths.iter() {
            counts[length.min(length_limit) as usize] += 1;
        }

        let mut total = 0;
        for (i, count) in counts
            .iter()
            .enumerate()
            .skip(1)
            .take(length_limit as usize)
        {
            total += count << (length_limit as usize - i);
        }

        while total > 1u32 << length_limit {
            let mut i = length_limit as usize - 1;
            while counts[i] == 0 {
                i -= 1;
            }
            counts[i] -= 1;
            counts[length_limit as usize] -= 1;
            counts[i + 1] += 2;
            total -= 1;
        }

        // assign new lengths
        let mut len = length_limit;
        let mut indexes = Vec::new();
        allocation.reserve_vec(&mut indexes, frequencies.len())
            .map_err(|_| EncodingError::MemoryLimitExceeded)?;
        indexes.extend(frequencies.iter().copied().enumerate());
        indexes.sort_unstable_by_key(|&(_, frequency)| frequency);
        for &(i, frequency) in &indexes {
            if frequency > 0 {
                while counts[len as usize] == 0 {
                    len -= 1;
                }
                lengths[i] = len;
                counts[len as usize] -= 1;
            }
        }
    }

    // Assign codes
    codes.fill(0);
    let mut code = 0u32;
    for len in 1..=length_limit {
        for (i, &length) in lengths.iter().enumerate() {
            if length == len {
                codes[i] = (code as u16).reverse_bits() >> (16 - len);
                code += 1;
            }
        }
        code <<= 1;
    }
    assert_eq!(code, 2 << length_limit);

    Ok(true)
}

pub(crate) fn write_huffman_tree<W: Write>(
    w: &mut BitWriter<W>,
    frequencies: &[u32],
    lengths: &mut [u8],
    codes: &mut [u16],
    budget: &MemoryBudget,
) -> Result<(), EncodingError> {
    if !build_huffman_tree(frequencies, lengths, codes, 15, budget)? {
        let symbol = frequencies
            .iter()
            .position(|&frequency| frequency > 0)
            .unwrap_or(0);
        return Ok(write_single_entry_huffman_tree(w, symbol as u8)?);
    }

    let mut code_length_lengths = [0u8; 16];
    let mut code_length_codes = [0u16; 16];
    let mut code_length_frequencies = [0u32; 16];
    for &length in lengths.iter() {
        code_length_frequencies[length as usize] += 1;
    }
    let single_code_length_length = !build_huffman_tree(
        &code_length_frequencies,
        &mut code_length_lengths,
        &mut code_length_codes,
        7,
        budget,
    )?;

    const CODE_LENGTH_ORDER: [usize; 19] = [
        17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    ];

    // Write the huffman tree
    w.write_bits(0, 1)?; // normal huffman tree
    w.write_bits(19 - 4, 4)?; // num_code_lengths - 4

    for i in CODE_LENGTH_ORDER {
        if i > 15 || code_length_frequencies[i] == 0 {
            w.write_bits(0, 3)?;
        } else if single_code_length_length {
            w.write_bits(1, 3)?;
        } else {
            w.write_bits(u64::from(code_length_lengths[i]), 3)?;
        }
    }

    match lengths.len() {
        256 => {
            w.write_bits(1, 1)?; // max_symbol is stored
            w.write_bits(3, 3)?; // max_symbol_nbits / 2 - 2
            w.write_bits(254, 8)?; // max_symbol - 2
        }
        280 => w.write_bits(0, 1)?,
        _ => unreachable!(),
    }

    // Write the huffman codes
    if !single_code_length_length {
        for &len in lengths.iter() {
            w.write_bits(
                u64::from(code_length_codes[len as usize]),
                code_length_lengths[len as usize],
            )?;
        }
    }

    Ok(())
}

const fn length_to_symbol(len: u16) -> (u16, u8) {
    let len = len - 1;
    let highest_bit = len.ilog2() as u16;
    let second_highest_bit = (len >> (highest_bit - 1)) & 1;
    let extra_bits = highest_bit - 1;
    let symbol = 2 * highest_bit + second_highest_bit;
    (symbol, extra_bits as u8)
}

#[inline(always)]
pub(crate) fn count_run(
    pixel: &[u8],
    it: &mut std::iter::Peekable<ChunksExact<u8>>,
    frequencies1: &mut [u32; 280],
) {
    let mut run_length = 0;
    while run_length < 4096 && it.peek() == Some(&pixel) {
        run_length += 1;
        it.next();
    }
    if run_length > 0 {
        if run_length <= 4 {
            let symbol = 256 + run_length - 1;
            frequencies1[symbol] += 1;
        } else {
            let (symbol, _extra_bits) = length_to_symbol(run_length as u16);
            frequencies1[256 + symbol as usize] += 1;
        }
    }
}

#[inline(always)]
pub(crate) fn write_run<W: Write>(
    w: &mut BitWriter<W>,
    pixel: &[u8],
    it: &mut std::iter::Peekable<ChunksExact<u8>>,
    codes1: &[u16; 280],
    lengths1: &[u8; 280],
) -> io::Result<()> {
    let mut run_length = 0;
    while run_length < 4096 && it.peek() == Some(&pixel) {
        run_length += 1;
        it.next();
    }
    if run_length > 0 {
        if run_length <= 4 {
            let symbol = 256 + run_length - 1;
            w.write_bits(u64::from(codes1[symbol]), lengths1[symbol])?;
        } else {
            let (symbol, extra_bits) = length_to_symbol(run_length as u16);
            w.write_bits(
                u64::from(codes1[256 + symbol as usize]),
                lengths1[256 + symbol as usize],
            )?;
            w.write_bits(
                (run_length as u64 - 1) & ((1 << extra_bits) - 1),
                extra_bits,
            )?;
        }
    }
    Ok(())
}

/// Allows fine-tuning some encoder parameters.
///
/// Pass to [`WebPEncoder::set_params()`].
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct EncoderParams {
    /// Use a predictor transform. Enabled by default.
    pub use_predictor_transform: bool,
}

impl Default for EncoderParams {
    fn default() -> Self {
        Self {
            use_predictor_transform: true,
        }
    }
}


/// WebP Encoder.
pub struct WebPEncoder<W> {
    writer: W,
    icc_profile: Vec<u8>,
    exif_metadata: Vec<u8>,
    xmp_metadata: Vec<u8>,
    params: EncoderParams,
    memory_limit: usize,
}

impl<W: Write> WebPEncoder<W> {
    /// Create a new encoder that writes its output to `w`.
    ///
    /// Only supports "VP8L" lossless encoding.
    pub fn new(w: W) -> Self {
        Self {
            writer: w,
            icc_profile: Vec::new(),
            exif_metadata: Vec::new(),
            xmp_metadata: Vec::new(),
            params: EncoderParams::default(),
            memory_limit: usize::MAX,
        }
    }

    /// Set the ICC profile to use for the image.
    pub fn set_icc_profile(&mut self, icc_profile: Vec<u8>) {
        self.icc_profile = icc_profile;
    }

    /// Set the EXIF metadata to use for the image.
    pub fn set_exif_metadata(&mut self, exif_metadata: Vec<u8>) {
        self.exif_metadata = exif_metadata;
    }

    /// Set the XMP metadata to use for the image.
    pub fn set_xmp_metadata(&mut self, xmp_metadata: Vec<u8>) {
        self.xmp_metadata = xmp_metadata;
    }

    /// Set the `EncoderParams` to use.
    pub fn set_params(&mut self, params: EncoderParams) {
        self.params = params;
    }

    /// Sets the maximum working memory used by encoding, including owned metadata and tables.
    /// The borrowed input and the caller's writer are excluded. Checked when encoding starts.
    pub fn set_memory_limit(&mut self, limit: usize) {
        self.memory_limit = limit;
    }

    /// Encode image data with the indicated color type.
    ///
    /// # Panics
    ///
    /// Panics if the image data is not of the indicated dimensions.
    pub fn encode(
        self,
        data: &[u8],
        width: u32,
        height: u32,
        color: ColorType,
    ) -> Result<(), EncodingError> {
        let metadata = EncoderMetadata {
            icc_profile: self.icc_profile,
            exif_metadata: self.exif_metadata,
            xmp_metadata: self.xmp_metadata,
        };
        let stream = OwnedWebPEncoder::from_borrowed(
            data, width, height, color, self.memory_limit, self.params, metadata,
        )?;
        stream.write_to(self.writer)
    }
}

#[cfg(test)]
mod tests {
    use rand::RngCore;

    use super::*;

    #[test]
    fn write_webp() {
        let mut img = vec![0; 256 * 256 * 4];
        rand::thread_rng().fill_bytes(&mut img);

        let mut output = Vec::new();
        WebPEncoder::new(&mut output)
            .encode(&img, 256, 256, crate::ColorType::Rgba8)
            .unwrap();

        let mut decoder = crate::WebPDecoder::new(std::io::Cursor::new(output)).unwrap();
        let mut img2 = vec![0; 256 * 256 * 4];
        decoder.read_image(&mut img2).unwrap();
        assert_eq!(img, img2);
    }

    #[test]
    fn write_webp_exif() {
        let mut img = vec![0; 256 * 256 * 3];
        rand::thread_rng().fill_bytes(&mut img);

        let mut exif = vec![0; 10];
        rand::thread_rng().fill_bytes(&mut exif);

        let mut output = Vec::new();
        let mut encoder = WebPEncoder::new(&mut output);
        encoder.set_exif_metadata(exif.clone());
        encoder
            .encode(&img, 256, 256, crate::ColorType::Rgb8)
            .unwrap();

        let mut decoder = crate::WebPDecoder::new(std::io::Cursor::new(output)).unwrap();

        let mut img2 = vec![0; 256 * 256 * 3];
        decoder.read_image(&mut img2).unwrap();
        assert_eq!(img, img2);

        let exif2 = decoder.exif_metadata().unwrap();
        assert_eq!(Some(exif), exif2);
    }

    #[test]
    fn roundtrip_libwebp() {
        roundtrip_libwebp_params(EncoderParams::default());
        roundtrip_libwebp_params(EncoderParams {
            use_predictor_transform: false,
            ..Default::default()
        });
    }

    fn roundtrip_libwebp_params(params: EncoderParams) {
        println!("Testing {params:?}");

        let mut img = vec![0; 256 * 256 * 4];
        rand::thread_rng().fill_bytes(&mut img);

        let mut output = Vec::new();
        let mut encoder = WebPEncoder::new(&mut output);
        encoder.set_params(params.clone());
        encoder
            .encode(&img[..256 * 256 * 3], 256, 256, crate::ColorType::Rgb8)
            .unwrap();
        let decoded = webp::Decoder::new(&output).decode().unwrap();
        assert_eq!(img[..256 * 256 * 3], *decoded);

        let mut output = Vec::new();
        let mut encoder = WebPEncoder::new(&mut output);
        encoder.set_params(params.clone());
        encoder
            .encode(&img, 256, 256, crate::ColorType::Rgba8)
            .unwrap();
        let decoded = webp::Decoder::new(&output).decode().unwrap();
        assert_eq!(img, *decoded);

        let mut output = Vec::new();
        let mut encoder = WebPEncoder::new(&mut output);
        encoder.set_params(params.clone());
        encoder.set_icc_profile(vec![0; 10]);
        encoder
            .encode(&img, 256, 256, crate::ColorType::Rgba8)
            .unwrap();
        let decoded = webp::Decoder::new(&output).decode().unwrap();
        assert_eq!(img, *decoded);

        let mut output = Vec::new();
        let mut encoder = WebPEncoder::new(&mut output);
        encoder.set_params(params.clone());
        encoder.set_exif_metadata(vec![0; 10]);
        encoder
            .encode(&img, 256, 256, crate::ColorType::Rgba8)
            .unwrap();
        let decoded = webp::Decoder::new(&output).decode().unwrap();
        assert_eq!(img, *decoded);

        let mut output = Vec::new();
        let mut encoder = WebPEncoder::new(&mut output);
        encoder.set_params(params);
        encoder.set_xmp_metadata(vec![0; 7]);
        encoder.set_icc_profile(vec![0; 8]);
        encoder.set_icc_profile(vec![0; 9]);
        encoder
            .encode(&img, 256, 256, crate::ColorType::Rgba8)
            .unwrap();
        let decoded = webp::Decoder::new(&output).decode().unwrap();
        assert_eq!(img, *decoded);
    }
}
