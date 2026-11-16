//! Binary batch framing: `[u32be header_len][utf8 json header][payload]`.
//!
//! One request/response is a bounded sequence of records: at most
//! [`BATCH_MAX_RECORDS`] frames and [`BINARY_BODY_MAX_BYTES`] total bytes
//! including the 4-byte length prefixes. Headers are ≤512-byte UTF-8 JSON;
//! payloads are raw bytes (never compressed, never base64).
//!
//! The decoder is incremental and safe to drive per ≤8 MiB HTTP step: it
//! reassembles frames across `feed` calls, enforces every limit while
//! buffering, and reports the offset consumed so partial-batch receipts map
//! to record positions.

use super::{
    BATCH_MAX_RECORDS, BINARY_BODY_MAX_BYTES, CHUNK_BYTES, BatchHeader,
    FRAME_HEADER_MAX_BYTES, Result, TransferError,
};

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Validated record header.
    pub header: BatchHeader,
    /// Raw payload bytes (exactly `header.payload_size`).
    pub payload: Vec<u8>,
}

/// Incremental binary batch encoder.
///
/// Enforces the same limits the decoder does so producers can never emit a
/// frame a conforming peer would reject.
#[derive(Debug, Default)]
pub struct FrameEncoder { records: usize }

impl FrameEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one record to `out`. `payload.len()` must equal
    /// `header.payload_size`; the header is normalized/validated here so the
    /// caller cannot hand-roll an inconsistent frame.
    pub fn encode_into(&mut self, header: &BatchHeader, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        if payload.len() as u64 != header.payload_size {
            return Err(TransferError::Protocol(format!(
                "payload is {} bytes but header declares {}",
                payload.len(),
                header.payload_size
            )));
        }
        if header.payload_size > CHUNK_BYTES {
            return Err(TransferError::Protocol(format!(
                "payload_size {} exceeds chunk_bytes {CHUNK_BYTES}",
                header.payload_size
            )));
        }
        if self.records >= BATCH_MAX_RECORDS { return Err(TransferError::Protocol("batch exceeds record limit".into())); }
        if let Some(sha256) = &header.sha256 { super::check_sha256_hex(sha256)?; }
        let json = header.encode()?;
        let additional = 4 + json.len() + payload.len();
        if out.len().checked_add(additional).is_none_or(|bytes| bytes as u64 > BINARY_BODY_MAX_BYTES) {
            return Err(TransferError::Protocol("batch exceeds wire byte limit".into()));
        }
        out.reserve(additional);
        self.records += 1;
        out.extend_from_slice(&(json.len() as u32).to_be_bytes());
        out.extend_from_slice(&json);
        out.extend_from_slice(payload);
        Ok(())
    }

    /// Serialized length of one record (length prefix + header + payload).
    pub fn frame_len(header: &BatchHeader, payload: &[u8]) -> Result<usize> {
        if header.payload_size != payload.len() as u64 { return Err(TransferError::Protocol("payload length mismatch".into())); }
        Ok(4 + header.encode()?.len() + payload.len())
    }
}

/// Incremental binary batch decoder.
///
/// `feed` may be called with arbitrary split points; each call returns the
/// frames that became complete. `finish` fails on trailing partial data
/// unless it is exactly empty.
///
/// Per-step accounting: `records_seen`/`bytes_seen` cover *all* input since
/// construction or the last [`Self::reset`]; callers compare them against the
/// session's negotiated batch limits.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    records_seen: usize,
    bytes_seen: u64,
    /// Whether the buffered parse position is mid-frame (for error detail).
    open: bool,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buffer: Vec::with_capacity(64 * 1024), records_seen: 0, bytes_seen: 0, open: false }
    }

    /// Total input bytes consumed since construction/`reset`.
    pub fn bytes_seen(&self) -> u64 {
        self.bytes_seen
    }

    /// Total frames completed since construction/`reset`.
    pub fn records_seen(&self) -> usize {
        self.records_seen
    }

    /// Feed bytes; returns newly completed frames in order.
    ///
    /// The step's cumulative input is capped at [`BINARY_BODY_MAX_BYTES`] and
    /// [`BATCH_MAX_RECORDS`] records — a larger stream must be decoded across
    /// multiple sessions or rejected by the caller's batch accounting (which
    /// may be smaller than these absolute maxima).
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>> {
        self.bytes_seen = self
            .bytes_seen
            .checked_add(bytes.len() as u64)
            .filter(|total| *total <= BINARY_BODY_MAX_BYTES)
            .ok_or_else(|| {
                TransferError::Protocol(format!(
                    "binary body exceeds {} bytes",
                    BINARY_BODY_MAX_BYTES
                ))
            })?;
        self.buffer.extend_from_slice(bytes);
        self.drain()
    }

    /// Complete the stream. Any buffered partial frame is an error.
    pub fn finish(&mut self) -> Result<Vec<Frame>> {
        let frames = self.drain()?;
        if !self.buffer.is_empty() {
            return Err(TransferError::Protocol(format!(
                "truncated binary batch: {} undelivered bytes",
                self.buffer.len()
            )));
        }
        Ok(frames)
    }

    /// Reset accounting and buffered input (new batch).
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.records_seen = 0;
        self.bytes_seen = 0;
        self.open = false;
    }

    fn drain(&mut self) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 4 {
                self.open = !self.buffer.is_empty();
                return Ok(frames);
            }
            let header_len =
                u32::from_be_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]])
                    as usize;
            if header_len > FRAME_HEADER_MAX_BYTES {
                return Err(TransferError::Protocol(format!(
                    "frame header length {header_len} exceeds {FRAME_HEADER_MAX_BYTES} bytes"
                )));
            }
            if header_len == 0 {
                return Err(TransferError::Protocol("empty frame header".into()));
            }
            if self.buffer.len() < 4 + header_len {
                self.open = true;
                return Ok(frames);
            }
            let header = BatchHeader::decode(&self.buffer[4..4 + header_len])?;
            let payload_len = header.payload_size as usize;
            if self.buffer.len() < 4 + header_len + payload_len {
                self.open = true;
                return Ok(frames);
            }
            let payload = self.buffer[4 + header_len..4 + header_len + payload_len].to_vec();
            self.buffer.drain(..4 + header_len + payload_len);
            self.records_seen += 1;
            if self.records_seen > BATCH_MAX_RECORDS {
                return Err(TransferError::Protocol(format!(
                    "more than {BATCH_MAX_RECORDS} records in one batch"
                )));
            }
            frames.push(Frame { header, payload });
            self.open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn header(file_id: u32, chunk_index: u32, payload: &[u8], is_final: bool, sha: bool) -> BatchHeader {
        BatchHeader {
            file_id,
            chunk_index,
            payload_size: payload.len() as u64,
            is_final,
            sha256: sha.then(|| hex::encode(Sha256::digest(payload))),
        }
    }

    #[test]
    fn encoder_enforces_wire_and_record_budgets_without_partial_append() {
        let mut encoder = FrameEncoder::new();
        let mut bytes = Vec::new();
        for index in 0..BATCH_MAX_RECORDS {
            encoder.encode_into(&header(0, index as u32, b"", true, true), b"", &mut bytes).unwrap();
        }
        let before = bytes.len();
        assert!(encoder.encode_into(&header(0, 100, b"", true, true), b"", &mut bytes).is_err());
        assert_eq!(bytes.len(), before);
        let mut bytes = vec![0; BINARY_BODY_MAX_BYTES as usize];
        assert!(FrameEncoder::new().encode_into(&header(0, 0, b"a", true, false), b"a", &mut bytes).is_err());
        assert_eq!(bytes.len() as u64, BINARY_BODY_MAX_BYTES);
    }

    #[test]
    fn roundtrip() {
        let payload = b"hello transfer".to_vec();
        let mut bytes = Vec::new();
        FrameEncoder::new()
            .encode_into(&header(0, 0, &payload, true, true), &payload, &mut bytes)
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let frames = decoder.feed(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.file_id, 0);
        assert_eq!(frames[0].header.payload_size, payload.len() as u64);
        assert_eq!(frames[0].payload, payload);
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn split_everywhere() {
        let payloads: [&[u8]; 3] = [b"a", b"bc", b"def"];
        let mut bytes = Vec::new();
        for (index, payload) in payloads.iter().enumerate() {
            FrameEncoder::new()
                .encode_into(&header(1, index as u32, payload, index == 2, false), payload, &mut bytes)
                .unwrap();
        }
        let mut decoder = FrameDecoder::new();
        let mut all = Vec::new();
        for byte in &bytes {
            all.extend(decoder.feed(&[*byte]).unwrap());
        }
        all.extend(decoder.finish().unwrap());
        assert_eq!(all.len(), 3);
        assert!(all[2].header.is_final);
        assert_eq!(all[1].header.chunk_index, 1);
    }

    #[test]
    fn rejects_oversized_header_length() {
        let mut decoder = FrameDecoder::new();
        let bytes = (513u32).to_be_bytes();
        assert!(decoder.feed(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated_stream() {
        let payload = b"abc".to_vec();
        let mut bytes = Vec::new();
        FrameEncoder::new()
            .encode_into(&header(0, 0, &payload, true, false), &payload, &mut bytes)
            .unwrap();
        let mut decoder = FrameDecoder::new();
        assert!(decoder.feed(&bytes[..bytes.len() - 1]).is_ok());
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn rejects_record_limit() {
        let mut bytes = Vec::new();
        for index in 0..=BATCH_MAX_RECORDS {
            FrameEncoder::new()
                .encode_into(&header(0, index as u32, b"x", false, false), b"x", &mut bytes)
                .unwrap();
        }
        let mut decoder = FrameDecoder::new();
        assert!(decoder.feed(&bytes).is_err());
    }

    #[test]
    fn rejects_mismatched_declared_size() {
        let payload = b"abc".to_vec();
        let mut bytes = Vec::new();
        let bad = BatchHeader { file_id: 0, chunk_index: 0, payload_size: 4, is_final: false, sha256: None };
        assert!(FrameEncoder::new().encode_into(&bad, &payload, &mut bytes).is_err());
    }
}
