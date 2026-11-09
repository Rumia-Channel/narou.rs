//! Streaming legacy payload decoding with uncompressed size/CRC verification.

use futures::{StreamExt, TryStreamExt, io::{AsyncRead, AsyncReadExt, BufReader}};
use std::pin::Pin;
use crate::error::{NarouError, Result};
use super::{AssetStream, ObjectEncoding};
use super::staging::{ASSET_CHUNK_BYTES, validate_range};


pub use crc32fast::Hasher as PayloadCrc32;
#[cfg(not(target_arch = "wasm32"))]
type Reader = Pin<Box<dyn AsyncRead + Send>>;
#[cfg(target_arch = "wasm32")]
type Reader = Pin<Box<dyn AsyncRead>>;

pub fn decode_stream(raw: AssetStream, encoding: ObjectEncoding, size: u64, crc32: u32) -> AssetStream {
    let input = BufReader::with_capacity(ASSET_CHUNK_BYTES,
        raw.map_err(|error| std::io::Error::other(error.to_string())).into_async_read());
    let reader: Reader = match encoding {
        ObjectEncoding::None => Box::pin(input),
        ObjectEncoding::Brotli => Box::pin(async_compression::futures::bufread::BrotliDecoder::new(input)),
        ObjectEncoding::Deflate => Box::pin(async_compression::futures::bufread::DeflateDecoder::new(input)),
    };
    Box::pin(futures::stream::try_unfold((reader, 0u64, crc32fast::Hasher::new()), move |(mut reader, total, mut hash)| async move {
        let mut bytes = vec![0; ASSET_CHUNK_BYTES];
        let length = reader.read(&mut bytes).await?;
        if length == 0 {
            if total != size || hash.finalize() != crc32 {
                return Err(NarouError::Platform("asset payload size/CRC mismatch".into()));
            }
            return Ok(None);
        }
        let next = total.checked_add(length as u64).filter(|&value| value <= size)
            .ok_or_else(|| NarouError::Platform("asset exceeds stored size".into()))?;
        bytes.truncate(length);
        hash.update(&bytes);
        Ok(Some((bytes, (reader, next, hash))))
    }))
}

/// Legacy compressed rows require decoding up to the requested offset, but
/// never retaining the prefix or allocating beyond a single read window.
pub async fn range_from_stream(mut stream: AssetStream, range: std::ops::Range<u64>) -> Result<Vec<u8>> {
    let length = validate_range(&range)?;
    let mut bytes = Vec::with_capacity(length);
    let mut position = 0u64;
    while position < range.end {
        let chunk = stream.next().await.ok_or_else(|| NarouError::Platform("asset range exceeds object".into()))??;
        let next = position.checked_add(chunk.len() as u64).ok_or_else(|| NarouError::Platform("asset position overflow".into()))?;
        let start = range.start.saturating_sub(position).min(chunk.len() as u64) as usize;
        let end = range.end.saturating_sub(position).min(chunk.len() as u64) as usize;
        if start < end { bytes.extend_from_slice(&chunk[start..end]); }
        position = next;
    }
    if bytes.len() != length { return Err(NarouError::Platform("asset range truncated".into())); }
    Ok(bytes)
}
