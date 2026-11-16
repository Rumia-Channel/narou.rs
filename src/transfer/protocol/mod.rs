//! `/api/transfer/v2` wire contract: binary batch framing, the normalized
//! NDJSON wire manifest, and shared constants/DTOs.
//!
//! These types are the single source of truth for the Native and Workers
//! transfer backends and the browser WASM worker. All `u64` byte/offset/size
//! counters serialize as decimal strings on the wire.
//!
//! See `docs/client-transfer-design.md` §5–§6.
use serde::{Deserialize, Serialize};

use super::{Result, TransferError};

mod frame;
mod wire;

pub use frame::{Frame, FrameDecoder, FrameEncoder};
pub use wire::{
    NovelKind, WireEnd, WireFile, WireHeader, WireManifestDecoder, WireNovel, WireRecord,
    WireText, is_text_field, serialize_record, text_fragments,
};

/// Validate a 64-char hex SHA-256 string.
pub(crate) fn check_sha256_hex(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TransferError::Protocol(format!("invalid sha256 {value:?}")));
    }
    Ok(())
}

/// Transfer protocol version. Distinct from the archive `format_version`.
pub const PROTOCOL_VERSION: u32 = 2;

/// Browser binding ABI, independently checked against compiled codec_info.
pub const ABI_VERSION: u32 = 2;

/// Maximum binary HTTP body including framing (8 MiB).
pub const BINARY_BODY_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Codec I/O quantum: every JS↔WASM and server↔store read/write is bounded.
pub const IO_CHUNK_BYTES: usize = 64 * 1024;

/// Per-file payload chunk: `8 MiB - 64 KiB`, fixed at session creation.
pub const CHUNK_BYTES: u64 = BINARY_BODY_MAX_BYTES - IO_CHUNK_BYTES as u64;

/// Maximum records in one binary batch (upload or export).
pub const BATCH_MAX_RECORDS: usize = 100;

/// Maximum byte length of one framed record's JSON header.
pub const FRAME_HEADER_MAX_BYTES: usize = 512;

/// Maximum control-JSON body (capabilities, select, plan pages, ...).
pub const CONTROL_BODY_MAX_BYTES: u64 = 256 * 1024;

/// Maximum items in one paged response (files/plan/catalog/status).
pub const PAGE_MAX_ITEMS: usize = 100;

/// Maximum serialized size of one paged response.
pub const PAGE_MAX_BYTES: usize = 256 * 1024;

/// Serialized budget for the whole NDJSON wire manifest.
pub const WIRE_MANIFEST_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum serialized size of one wire-manifest record (line incl. `\n`).
pub const WIRE_RECORD_MAX_BYTES: usize = 256 * 1024;

/// Maximum decoded bytes in one `text` record fragment.
pub const TEXT_FRAGMENT_MAX_BYTES: usize = 32 * 1024;

/// Reserved `file_id` for wire-manifest chunks in the binary frame stream.
pub const MANIFEST_FILE_ID: u32 = u32::MAX;

/// A framed binary record header (`[u32be len][utf8 json][payload]`).
///
/// `file_id` is the global file ordinal (`manifest_file_index`); uploads
/// require `sha256`, export batches may omit it. `payload_size` travels as a
/// decimal string per the wire contract.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BatchHeader {
    /// Global file ordinal, or [`MANIFEST_FILE_ID`] for the manifest itself.
    pub file_id: u32,
    /// Chunk index inside the file (`offset = chunk_index * chunk_bytes`).
    pub chunk_index: u32,
    /// Exact payload byte length as a decimal string.
    #[serde(with = "decimal_u64")]
    pub payload_size: u64,
    /// Marks the last record of the file; folds the file seal on import.
    #[serde(rename = "final", default)]
    pub is_final: bool,
    /// SHA-256 (hex) of `payload`. Required on uploads, optional on export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl BatchHeader {
    /// Serialize as the JSON header (≤ [`FRAME_HEADER_MAX_BYTES`] enforced).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(TransferError::Serde)?;
        if bytes.len() > FRAME_HEADER_MAX_BYTES {
            return Err(TransferError::Protocol(format!(
                "frame header is {} bytes, limit {}",
                bytes.len(),
                FRAME_HEADER_MAX_BYTES
            )));
        }
        Ok(bytes)
    }

    /// Parse and validate a JSON header.
    pub fn decode(json: &[u8]) -> Result<BatchHeader> {
        if json.len() > FRAME_HEADER_MAX_BYTES {
            return Err(TransferError::Protocol(format!(
                "frame header is {} bytes, limit {}",
                json.len(),
                FRAME_HEADER_MAX_BYTES
            )));
        }
        let header: BatchHeader = serde_json::from_slice(json)
            .map_err(|error| TransferError::Protocol(format!("invalid frame header: {error}")))?;
        if header.payload_size > CHUNK_BYTES {
            return Err(TransferError::Protocol(format!(
                "payload_size {} exceeds chunk_bytes {}",
                header.payload_size, CHUNK_BYTES
            )));
        }
        if let Some(sha256) = &header.sha256 {
            check_sha256_hex(sha256)?;
        }
        Ok(header)
    }
}

/// u64 as a decimal string on the wire.
pub mod decimal_u64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() || text.len() > 20 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(serde::de::Error::custom("invalid decimal u64"));
        }
        text.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

/// Reject a `u64` that does not round-trip through the decimal-string form.
pub(crate) fn protocol_error(message: impl Into<String>) -> TransferError {
    TransferError::Protocol(message.into())
}
