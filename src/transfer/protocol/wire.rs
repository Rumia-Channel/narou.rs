//! Normalized NDJSON wire manifest (`§6.5`).
//!
//! The wire manifest is **not** the ZIP's `manifest.json`: it is a stable,
//! bounded record stream the server can plan against without ever seeing the
//! archive. Record order is fixed: `header`, then for each selected novel a
//! `novel` record followed by its `text` and `file` records, then `end`.
//!
//! String fields of arbitrary length (title, author, subtitle, tags, URLs,
//! members) travel as decoded ≤32 KiB `text` fragments — never as huge
//! escaped JSON strings and never truncated for storage.

use serde::{Deserialize, Serialize};

use crate::transfer::projection::TransferProfile;
use super::{
    Result, TEXT_FRAGMENT_MAX_BYTES, TransferError, WIRE_MANIFEST_MAX_BYTES,
    WIRE_RECORD_MAX_BYTES,
};

/// Kind of a novel record on the wire (mirrors plan/catalog `kind`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NovelKind {
    /// Ordinary work.
    Normal,
    /// Virtual parent (metadata + member references only).
    VirtualParent,
}

impl NovelKind {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            NovelKind::Normal => "normal",
            NovelKind::VirtualParent => "virtual_parent",
        }
    }
}

/// `header` record: versions, selection policy, fingerprint and totals.
/// Serialized key order is fixed by field declaration order; producers build
/// the struct literal directly.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireHeader {
    /// Archive format version the client validated against (`2`).
    pub format_version: u32,
    /// Transfer protocol version (`2`).
    pub protocol_version: u32,
    /// Destination storage-policy version the projection was built for.
    pub policy_version: u32,
    /// `runtime` or `complete`.
    pub profile: TransferProfile,
    /// Additional roles explicitly selected (`["raw","history"]`).
    #[serde(default)]
    pub extra_roles: Vec<String>,
    /// Source fingerprint (validated archive identity, hex64).
    pub fingerprint: String,
    /// Confirmed profile, policy and selected file-set identity.
    pub selection_digest: String,
    /// Total selected novels.
    pub novels: u32,
    /// Total sent file records.
    pub files: u32,
    /// Total text fragment records.
    pub texts: u32,
}

/// `novel` record: position and kind only — every free-form string travels as
/// `text` fragments so no record can exceed its bound.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireNovel {
    /// Manifest novel index (stable across the whole stream).
    pub index: u32,
    pub kind: NovelKind,
    /// `novel_type` byte from the archive manifest.
    pub novel_type: u8,
    /// Number of `file` records belonging to this novel.
    pub files: u32,
}

/// `text` record: one decoded fragment of an arbitrary-length field.
///
/// `field` is a whitelisted name (`toc_url`, `title`, `author`, `sitename`,
/// `domain`, `attribute_tag`, `ncode`, `subtitle`, `general_lastup`,
/// `last_update`, `tag`, `member`). `item` is the array position for `tag`
/// and `member`; `part` is the fragment position; `last` terminates the
/// field/item pair.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireText {
    /// Owning novel index.
    pub novel: u32,
    /// Whitelisted field name.
    pub field: String,
    /// Array element position for `tag`/`member`; absent for scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<u32>,
    /// Fragment position within the field/item.
    pub part: u32,
    /// True on the final fragment.
    pub last: bool,
    /// Decoded fragment, at most [`TEXT_FRAGMENT_MAX_BYTES`] bytes.
    pub value: String,
}

/// `file` record: one sent file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireFile {
    /// Global file ordinal (`manifest_file_index`), dense in emission order.
    pub file_id: u32,
    /// Owning novel index.
    pub novel: u32,
    /// Path relative to the novel directory (validated form).
    pub path: String,
    /// Exact uncompressed size as a decimal string.
    #[serde(with = "super::decimal_u64")]
    pub size: u64,
    /// SHA-256 of the file bytes (hex64, verified by the client).
    pub sha256: String,
}

/// `end` record: totals, echoed for completeness checks.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireEnd {
    pub novels: u32,
    pub files: u32,
    pub texts: u32,
}

/// One wire-manifest record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireRecord {
    Header(WireHeader),
    Novel(WireNovel),
    Text(WireText),
    File(WireFile),
    End(WireEnd),
}

/// Field names allowed in `text` records.
pub const TEXT_FIELDS: [&str; 12] = [
    "toc_url", "title", "author", "sitename", "domain", "attribute_tag", "ncode", "subtitle",
    "general_lastup", "last_update", "tag", "member",
];

/// Check a field against the same ordered whitelist used by the decoder.
pub fn is_text_field(field: &str) -> bool {
    TEXT_FIELDS.contains(&field)
}

/// Split `value` into decoded fragments of at most
/// [`TEXT_FRAGMENT_MAX_BYTES`] on UTF-8 boundaries. An empty value yields one
/// empty fragment so every field/item has an explicit `last` record.
pub fn text_fragments(value: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(value);
    std::iter::from_fn(move || {
        let value = rest.take()?;
        if value.len() <= TEXT_FRAGMENT_MAX_BYTES { return Some(value); }
        let mut end = TEXT_FRAGMENT_MAX_BYTES;
        while !value.is_char_boundary(end) { end -= 1; }
        rest = Some(&value[end..]);
        Some(&value[..end])
    })
}

/// Serialize one record as an NDJSON line (trailing `\n`, ≤
/// [`WIRE_RECORD_MAX_BYTES`]). The emitted form is normalized: field order
/// comes from these types, and `text` values are always decoded fragments —
/// callers never hand-wrap arbitrary JSON.
pub fn serialize_record(record: &WireRecord) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(record).map_err(TransferError::Serde)?;
    bytes.push(b'\n');
    if bytes.len() > WIRE_RECORD_MAX_BYTES {
        return Err(TransferError::Protocol(format!(
            "wire record is {} bytes, limit {}",
            bytes.len(),
            WIRE_RECORD_MAX_BYTES
        )));
    }
    Ok(bytes)
}

/// Bounded incremental decoder for the wire manifest.
///
/// Feed arbitrary byte ranges (the server drives it per ≤8 MiB/100-record
/// `advance` step, persisting its own durable cursor between steps). The
/// decoder enforces record and total budgets while buffering and validates
/// record structure; sequence/fragment assembly checks belong to the
/// caller's normalized catalog.
#[derive(Debug)]
pub struct WireManifestDecoder {
    /// Incomplete line tail (bounded by WIRE_RECORD_MAX_BYTES).
    tail: Vec<u8>,
    /// Total bytes consumed.
    bytes_seen: u64,
    /// Records delivered.
    records_seen: u64,
    /// Whether `end` terminated the stream.
    finished: bool,
    header: Option<WireHeader>,
    novels: u32,
    files: u32,
    texts: u32,
    current: Option<(u32, u32, u32)>, // (novel index, declared files, seen files)
    fields: u16,
    next_tag: u32,
    next_member: u32,
    pending_text: Option<(usize, Option<u32>, u32)>,
}

impl Default for WireManifestDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl WireManifestDecoder {
    pub fn new() -> Self {
        Self { tail: Vec::new(), bytes_seen: 0, records_seen: 0, finished: false,
            header: None, novels: 0, files: 0, texts: 0, current: None, fields: 0,
            next_tag: 0, next_member: 0, pending_text: None }
    }

    /// Total input bytes consumed.
    pub fn bytes_seen(&self) -> u64 {
        self.bytes_seen
    }

    /// Total records delivered.
    pub fn records_seen(&self) -> u64 {
        self.records_seen
    }

    /// Whether the terminating `end` record has been seen.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Feed bytes; returns newly decoded records in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<WireRecord>> {
        self.bytes_seen = self.bytes_seen.checked_add(bytes.len() as u64)
            .filter(|total| *total <= WIRE_MANIFEST_MAX_BYTES)
            .ok_or_else(|| TransferError::TooLarge("wire manifest exceeds serialized budget".into()))?;
        let mut records = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.finished { return Err(super::protocol_error("data after wire manifest end")); }
            let end = rest.iter().position(|byte| *byte == b'\n');
            let length = end.map_or(rest.len(), |position| position + 1);
            if self.tail.len() + length > WIRE_RECORD_MAX_BYTES {
                return Err(TransferError::TooLarge("wire record exceeds limit".into()));
            }
            self.tail.extend_from_slice(&rest[..length]);
            rest = &rest[length..];
            if end.is_some() {
                let mut line = std::mem::take(&mut self.tail);
                line.pop();
                let record = self.decode_line(&line)?;
                line.clear();
                self.tail = line;
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Complete the stream: decode a final unterminated line or report a
    /// missing `end` record.
    pub fn finish(&mut self) -> Result<Vec<WireRecord>> {
        if self.finished {
            return Ok(Vec::new());
        }
        if !self.tail.is_empty() {
            if self.tail.len() > WIRE_RECORD_MAX_BYTES {
                return Err(TransferError::TooLarge(format!(
                    "wire record exceeds {} bytes",
                    WIRE_RECORD_MAX_BYTES
                )));
            }
            let line = std::mem::take(&mut self.tail);
            let record = self.decode_line(&line)?;
            if !self.finished {
                return Err(TransferError::Protocol(
                    "wire manifest ended without an end record".into(),
                ));
            }
            return Ok(vec![record]);
        }
        Err(TransferError::Protocol("wire manifest is empty or truncated".into()))
    }

    fn end_novel(&self) -> Result<()> {
        if let Some((_, expected, actual)) = self.current
            && (expected != actual || self.pending_text.is_some() || self.fields & 15 != 15)
        {
            return Err(super::protocol_error("incomplete novel files or text fields"));
        }
        Ok(())
    }

    fn decode_line(&mut self, line: &[u8]) -> Result<WireRecord> {
        if self.finished { return Err(super::protocol_error("data after wire manifest end")); }
        if line.len() >= WIRE_RECORD_MAX_BYTES { return Err(TransferError::TooLarge("wire record exceeds limit".into())); }
        let record: WireRecord = serde_json::from_slice(line)
            .map_err(|error| super::protocol_error(format!("invalid wire record: {error}")))?;
        if self.header.is_none() && !matches!(record, WireRecord::Header(_)) {
            return Err(super::protocol_error("header must be the first record"));
        }
        match &record {
            WireRecord::Header(header) => {
                if self.header.is_some() || self.records_seen != 0 { return Err(super::protocol_error("duplicate or late header")); }
                super::super::structure::version(header.format_version)?;
                if header.protocol_version != super::PROTOCOL_VERSION || header.policy_version == 0 {
                    return Err(super::protocol_error("unsupported protocol or policy version"));
                }
                if header.novels as usize > crate::transfer::MAX_NOVELS || header.files as usize >= crate::transfer::MAX_ENTRIES {
                    return Err(TransferError::TooLarge("wire manifest count limit".into()));
                }
                super::check_sha256_hex(&header.fingerprint)?;
                super::check_sha256_hex(&header.selection_digest)?;
                crate::transfer::projection::validate_extra_roles(header.profile, &header.extra_roles)?;
                self.header = Some(header.clone());
            }
            WireRecord::Novel(novel) => {
                self.end_novel()?;
                let header = self.header.as_ref().expect("header checked");
                if novel.index != self.novels || self.novels >= header.novels { return Err(super::protocol_error("non-contiguous novel index")); }
                if novel.kind == NovelKind::VirtualParent && header.format_version < 2 {
                    return Err(super::protocol_error("virtual parent requires format 2"));
                }
                self.current = Some((novel.index, novel.files, 0));
                self.novels += 1; self.fields = 0; self.next_tag = 0; self.next_member = 0;
            }
            WireRecord::Text(text) => {
                let (novel, _, files) = self.current.ok_or_else(|| super::protocol_error("text outside novel"))?;
                if text.novel != novel || files != 0 { return Err(super::protocol_error("text after files or wrong novel")); }
                let field = TEXT_FIELDS.iter().position(|field| *field == text.field)
                    .ok_or_else(|| super::protocol_error("unknown text field"))?;
                if text.value.len() > TEXT_FRAGMENT_MAX_BYTES { return Err(TransferError::TooLarge("text fragment exceeds decoded limit".into())); }
                let array = matches!(text.field.as_str(), "tag" | "member");
                if array != text.item.is_some() { return Err(super::protocol_error("incorrect text item shape")); }
                if let Some((expected_field, item, part)) = self.pending_text {
                    if field != expected_field || text.item != item || text.part != part { return Err(super::protocol_error("discontinuous text fragments")); }
                } else {
                    if text.part != 0 || (!array && self.fields & (1 << field) != 0) { return Err(super::protocol_error("duplicate or discontinuous text field")); }
                    let expected_item = if text.field == "tag" { self.next_tag } else { self.next_member };
                    if array && text.item != Some(expected_item) { return Err(super::protocol_error("discontinuous text array")); }
                }
                if text.last {
                    self.pending_text = None;
                    self.fields |= 1 << field;
                    if text.field == "tag" { self.next_tag += 1; }
                    if text.field == "member" { self.next_member += 1; }
                } else {
                    self.pending_text = Some((field, text.item, text.part.checked_add(1)
                        .ok_or_else(|| super::protocol_error("text part overflow"))?));
                }
                self.texts = self.texts.checked_add(1).ok_or_else(|| super::protocol_error("text count overflow"))?;
                if self.texts > self.header.as_ref().expect("header").texts { return Err(super::protocol_error("excess text records")); }
            }
            WireRecord::File(file) => {
                let current = self.current.as_mut().ok_or_else(|| super::protocol_error("file outside novel"))?;
                if file.novel != current.0 || current.2 >= current.1 || self.pending_text.is_some()
                    || self.fields & 15 != 15 || file.file_id != self.files {
                    return Err(super::protocol_error("unexpected file ordinal or incomplete text"));
                }
                if self.files >= self.header.as_ref().expect("header").files { return Err(super::protocol_error("excess file records")); }
                let path = crate::transfer::archive::validate_relative_path(&file.path)?;
                if path != file.path { return Err(super::protocol_error("non-normalized file path")); }
                super::check_sha256_hex(&file.sha256)?;
                current.2 += 1; self.files += 1;
            }
            WireRecord::End(end) => {
                self.end_novel()?;
                let header = self.header.as_ref().expect("header checked");
                if end.novels != self.novels || end.files != self.files || end.texts != self.texts
                    || end.novels != header.novels || end.files != header.files || end.texts != header.texts {
                    return Err(super::protocol_error("wire manifest totals differ"));
                }
                self.finished = true;
            }
        }
        self.records_seen += 1;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_validator_uses_the_decoder_whitelist() {
        for field in TEXT_FIELDS {
            assert!(is_text_field(field));
        }
        for field in ["", "Title", "body", "files", "novel_type", "tag[]"] {
            assert!(!is_text_field(field));
        }
    }

    fn records() -> Vec<WireRecord> {
        let mut records = vec![WireRecord::Header(WireHeader {
            format_version: 2, protocol_version: super::super::PROTOCOL_VERSION, policy_version: 1,
            profile: TransferProfile::Runtime, extra_roles: vec![],
            fingerprint: "a".repeat(64), selection_digest: "c".repeat(64), novels: 1, files: 1, texts: 4,
        }), WireRecord::Novel(WireNovel { index: 0, kind: NovelKind::Normal, novel_type: 1, files: 1 })];
        for field in ["toc_url", "title", "author", "sitename"] {
            records.push(WireRecord::Text(WireText { novel: 0, field: field.into(), item: None, part: 0, last: true, value: field.into() }));
        }
        records.push(WireRecord::File(WireFile { file_id: 0, novel: 0, path: "toc.yaml".into(), size: 12, sha256: "b".repeat(64) }));
        records.push(WireRecord::End(WireEnd { novels: 1, files: 1, texts: 4 }));
        records
    }

    fn encode(records: &[WireRecord]) -> Vec<u8> {
        records.iter().flat_map(|record| serialize_record(record).unwrap()).collect()
    }

    #[test]
    fn fragments_preserve_utf8_empty_and_escape_amplification() {
        for value in [String::new(), "あ".repeat(TEXT_FRAGMENT_MAX_BYTES), "\0".repeat(6 * 1024 * 1024)] {
            let parts: Vec<_> = text_fragments(&value).collect();
            assert!(!parts.is_empty());
            assert!(parts.iter().all(|part| part.len() <= TEXT_FRAGMENT_MAX_BYTES));
            assert_eq!(parts.concat(), value);
            for (index, part) in parts.iter().enumerate() {
                let line = serialize_record(&WireRecord::Text(WireText { novel: 0, field: "title".into(), item: None, part: index as u32, last: index + 1 == parts.len(), value: (*part).into() })).unwrap();
                assert!(line.len() <= WIRE_RECORD_MAX_BYTES);
            }
        }
    }

    #[test]
    fn decoder_accepts_every_split_and_complete_totals() {
        let source = records();
        let bytes = encode(&source);
        for split in 0..=bytes.len() {
            let mut decoder = WireManifestDecoder::new();
            let mut decoded = decoder.feed(&bytes[..split]).unwrap();
            decoded.extend(decoder.feed(&bytes[split..]).unwrap());
            decoded.extend(decoder.finish().unwrap());
            assert_eq!(decoded, source);
            assert_eq!(decoder.bytes_seen(), bytes.len() as u64);
        }
    }

    #[test]
    fn rejects_truncation_same_feed_after_end_and_wrong_totals() {
        let source = records();
        let bytes = encode(&source);
        let mut decoder = WireManifestDecoder::new();
        decoder.feed(&bytes[..bytes.len() - 2]).unwrap();
        assert!(decoder.finish().is_err());
        let mut doubled = bytes.clone(); doubled.extend_from_slice(&bytes);
        assert!(WireManifestDecoder::new().feed(&doubled).is_err());
        let mut wrong = source.clone();
        if let WireRecord::End(end) = wrong.last_mut().unwrap() { end.files += 1; }
        assert!(WireManifestDecoder::new().feed(&encode(&wrong)).is_err());
        assert!(WireManifestDecoder::new().feed(b"{\"type\":\"end\",\"novels\":0,\"files\":0,\"texts\":0}\n").is_err());
    }

    #[test]
    fn rejects_fragment_gaps_and_array_shape() {
        for field in ["title", "tag", "unknown"] {
            let mut source = records();
            if let WireRecord::Text(text) = &mut source[3] {
                text.field = field.into(); text.part = 2;
            }
            assert!(WireManifestDecoder::new().feed(&encode(&source)).is_err());
        }
        let mut source = records();
        if let WireRecord::Text(text) = &mut source[2] { text.last = false; }
        assert!(WireManifestDecoder::new().feed(&encode(&source)).is_err());
    }

    #[test]
    fn rejects_oversized_partial_line_before_buffering_it() {
        let mut decoder = WireManifestDecoder::new();
        assert!(decoder.feed(&vec![b' '; WIRE_RECORD_MAX_BYTES + 1]).is_err());
        assert!(decoder.tail.is_empty());
    }

    #[test]
    fn rejects_non_dense_ids_unsafe_paths_and_numeric_sizes() {
        for path in ["../escape", "toc.yaml"] {
            let mut source = records();
            if let WireRecord::File(file) = &mut source[6] { file.file_id = 3; file.path = path.into(); }
            assert!(WireManifestDecoder::new().feed(&encode(&source)).is_err());
        }
        let file = WireFile { file_id: 0, novel: 0, path: "a".into(), size: u64::MAX, sha256: "b".repeat(64) };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"size\":\"18446744073709551615\""));
        assert_eq!(serde_json::from_str::<WireFile>(&json).unwrap(), file);
        assert!(serde_json::from_str::<WireFile>(&json.replace("\"18446744073709551615\"", "12")).is_err());
    }
}
