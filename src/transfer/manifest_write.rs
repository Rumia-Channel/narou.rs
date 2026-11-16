//! Serialize the existing manifest schema without cloning metadata or building
//! a Value tree / serialized manifest. The caller supplies one 64 KiB buffer.
use futures::io::{AsyncWrite, AsyncWriteExt};
use super::{ArchiveManifest, Result};

pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, manifest: &ArchiveManifest) -> Result<()> {
    writer.write_all(b"{\"format_version\":").await?;
    number(writer, manifest.format_version as u64).await?;
    optional(writer, "app_version", manifest.app_version.as_deref()).await?;
    optional(writer, "exported_at", manifest.exported_at.as_deref()).await?;
    writer.write_all(b",\"novels\":[").await?;
    for (index, novel) in manifest.novels.iter().enumerate() {
        if index != 0 { writer.write_all(b",").await?; }
        writer.write_all(b"{\"toc_url\":").await?;
        string(writer, &novel.toc_url).await?;
        for (name, value) in [("title", &novel.title), ("author", &novel.author), ("sitename", &novel.sitename)] {
            field(writer, name, value).await?;
        }
        optional(writer, "domain", novel.domain.as_deref()).await?;
        writer.write_all(b",\"tags\":[").await?;
        for (position, tag) in novel.tags.iter().enumerate() {
            if position != 0 { writer.write_all(b",").await?; }
            string(writer, tag).await?;
        }
        writer.write_all(b"]").await?;
        optional(writer, "attribute_tag", novel.attribute_tag.as_deref()).await?;
        writer.write_all(b",\"novel_type\":").await?;
        number(writer, novel.novel_type as u64).await?;
        writer.write_all(b",\"files\":[").await?;
        for (position, file) in novel.files.iter().enumerate() {
            if position != 0 { writer.write_all(b",").await?; }
            writer.write_all(b"{\"path\":").await?;
            string(writer, &file.path).await?;
            optional(writer, "sha256", file.sha256.as_deref()).await?;
            if let Some(size) = file.size {
                writer.write_all(b",\"size\":").await?;
                number(writer, size).await?;
            }
            writer.write_all(b"}").await?;
        }
        writer.write_all(b"]").await?;
        for (name, value) in [("ncode", novel.ncode.as_deref()), ("subtitle", novel.subtitle.as_deref()),
            ("general_lastup", novel.general_lastup.as_deref()), ("last_update", novel.last_update.as_deref())] {
            optional(writer, name, value).await?;
        }
        if let Some(members) = &novel.virtual_members {
            writer.write_all(b",\"virtual_members\":[").await?;
            for (position, url) in members.iter().enumerate() {
                if position != 0 { writer.write_all(b",").await?; }
                string(writer, url).await?;
            }
            writer.write_all(b"]").await?;
        }
        writer.write_all(b"}").await?;
    }
    writer.write_all(b"]}").await?;
    Ok(())
}

async fn optional<W: AsyncWrite + Unpin>(writer: &mut W, name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value { field(writer, name, value).await?; }
    Ok(())
}
async fn field<W: AsyncWrite + Unpin>(writer: &mut W, name: &str, value: &str) -> Result<()> {
    writer.write_all(b",").await?;
    string(writer, name).await?;
    writer.write_all(b":").await?;
    string(writer, value).await
}
async fn raw<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    for chunk in bytes.chunks(64 * 1024) { writer.write_all(chunk).await?; }
    Ok(())
}
async fn string<W: AsyncWrite + Unpin>(writer: &mut W, value: &str) -> Result<()> {
    writer.write_all(b"\"").await?;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        let mut encoded = [0u8; 6];
        let Some(escaped) = json_escape(byte, &mut encoded) else { continue };
        raw(writer, &value.as_bytes()[start..index]).await?;
        writer.write_all(escaped).await?;
        start = index + 1;
    }
    raw(writer, &value.as_bytes()[start..]).await?;
    writer.write_all(b"\"").await?;
    Ok(())
}
async fn number<W: AsyncWrite + Unpin>(writer: &mut W, value: u64) -> Result<()> {
    let mut bytes = [0u8; 20];
    writer.write_all(decimal(value, &mut bytes)).await?;
    Ok(())
}
pub(super) fn decimal(mut value: u64, bytes: &mut [u8; 20]) -> &[u8] {
    let mut start = bytes.len();
    loop {
        start -= 1;
        bytes[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 { return &bytes[start..]; }
    }
}

/// Durable cursor for metadata-only novel JSON. It contains offsets and
/// structural state only, never a clone of a title, tag or member URL.
/// Callers must supply the same immutable metadata snapshot on every call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NovelJsonCursor {
    version: u8,
    field: u8,
    phase: NovelJsonPhase,
    item: u32,
    byte_offset: u64,
    token_offset: u8,
    escape_offset: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum NovelJsonPhase {
    Start, Field, Prefix, StringOpen, StringBody, StringClose,
    ArrayItem, ArrayEnd, Number, End, Done,
}

impl Default for NovelJsonCursor {
    fn default() -> Self {
        Self { version: 1, field: 0, phase: NovelJsonPhase::Start, item: 0,
            byte_offset: 0, token_offset: 0, escape_offset: 0 }
    }
}

enum NovelJsonValue<'a> { Text(&'a str), Array(&'a [String]), Number(u8) }

fn novel_json_value(novel: &super::ArchiveNovel, field: u8) -> Option<NovelJsonValue<'_>> {
    use NovelJsonValue::*;
    match field {
        0 => Some(Text(&novel.toc_url)), 1 => Some(Text(&novel.title)),
        2 => Some(Text(&novel.author)), 3 => Some(Text(&novel.sitename)),
        4 => novel.domain.as_deref().map(Text), 5 => Some(Array(&novel.tags)),
        6 => novel.attribute_tag.as_deref().map(Text), 7 => Some(Number(novel.novel_type)),
        8 => novel.ncode.as_deref().map(Text), 9 => novel.subtitle.as_deref().map(Text),
        10 => novel.general_lastup.as_deref().map(Text), 11 => novel.last_update.as_deref().map(Text),
        12 => novel.virtual_members.as_deref().map(Array), _ => None,
    }
}

fn novel_json_prefix(field: u8) -> &'static [u8] {
    match field {
        0 => b"\"toc_url\":", 1 => b",\"title\":",
        2 => b",\"author\":", 3 => b",\"sitename\":",
        4 => b",\"domain\":", 5 => b",\"tags\":[",
        6 => b",\"attribute_tag\":", 7 => b",\"novel_type\":",
        8 => b",\"ncode\":", 9 => b",\"subtitle\":",
        10 => b",\"general_lastup\":", 11 => b",\"last_update\":",
        12 => b",\"virtual_members\":[", _ => b"",
    }
}

fn invalid_novel_cursor() -> super::TransferError {
    super::TransferError::Protocol("invalid novel JSON cursor or changed metadata snapshot".into())
}

/// Append a small structural token, preserving its byte offset on suspension.
fn novel_json_token(output: &mut Vec<u8>, limit: usize, offset: &mut u8, token: &[u8]) -> Result<bool> {
    let start = *offset as usize;
    if start > token.len() { return Err(invalid_novel_cursor()); }
    let count = (limit - output.len()).min(token.len() - start);
    output.extend_from_slice(&token[start..start + count]);
    *offset += count as u8;
    if *offset as usize == token.len() { *offset = 0; Ok(true) } else { Ok(false) }
}

/// Produce at most `max_bytes` (1..=64 KiB) of one JSON object containing all
/// ArchiveNovel metadata EXCEPT its `files` property. Optional fields, tags
/// and virtual member ordering follow the archive schema exactly. `done` is
/// true only after the closing brace, and repeated completed calls are empty.
///
/// The cursor resumes at the exact field/item/UTF-8 byte/escape offset: it
/// never serializes a whole value, scans an already emitted prefix, or builds
/// a serde Value tree. Chunks are raw byte segments and may split a UTF-8
/// character or JSON escape; concatenate them before interpreting JSON.
/// A manifest producer may prepend `{"files":[...],` and discard only the
/// first `{` byte emitted by this helper to insert independently paged files.
pub fn novel_json_chunk(
    novel: &super::ArchiveNovel, cursor: &mut NovelJsonCursor, max_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    use NovelJsonPhase::*;
    if max_bytes == 0 || max_bytes > super::protocol::IO_CHUNK_BYTES {
        return Err(super::TransferError::Protocol("novel JSON chunk must be 1..=64 KiB".into()));
    }
    if cursor.version != 1 || cursor.field > 13 || cursor.escape_offset > 5 {
        return Err(invalid_novel_cursor());
    }
    if cursor.phase == Done {
        if cursor.field != 13 { return Err(invalid_novel_cursor()); }
        return Ok((Vec::new(), true));
    }
    let mut output = Vec::with_capacity(max_bytes);
    while output.len() < max_bytes && cursor.phase != Done {
        match cursor.phase {
            Start => {
                if cursor.field != 0 { return Err(invalid_novel_cursor()); }
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, b"{")? { cursor.phase = Field; }
            }
            Field => {
                if cursor.field == 13 { cursor.phase = End; continue; }
                if novel_json_value(novel, cursor.field).is_none() { cursor.field += 1; continue; }
                cursor.item = 0; cursor.byte_offset = 0; cursor.escape_offset = 0;
                cursor.phase = Prefix;
            }
            Prefix => {
                let value = novel_json_value(novel, cursor.field).ok_or_else(invalid_novel_cursor)?;
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, novel_json_prefix(cursor.field))? {
                    cursor.phase = match value { NovelJsonValue::Text(_) => StringOpen, NovelJsonValue::Array(_) => ArrayItem, NovelJsonValue::Number(_) => Number };
                }
            }
            StringOpen => {
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, b"\"")? { cursor.phase = StringBody; }
            }
            StringBody => {
                let value = match novel_json_value(novel, cursor.field).ok_or_else(invalid_novel_cursor)? {
                    NovelJsonValue::Text(value) => value,
                    NovelJsonValue::Array(values) => values.get(cursor.item as usize).ok_or_else(invalid_novel_cursor)?.as_str(),
                    NovelJsonValue::Number(_) => return Err(invalid_novel_cursor()),
                };
                let start = usize::try_from(cursor.byte_offset).map_err(|_| invalid_novel_cursor())?;
                if start > value.len() { return Err(invalid_novel_cursor()); }
                if start == value.len() {
                    if cursor.escape_offset != 0 { return Err(invalid_novel_cursor()); }
                    cursor.phase = StringClose; continue;
                }
                let mut encoded = [0u8; 6];
                if let Some(escape) = json_escape(value.as_bytes()[start], &mut encoded) {
                    let escaped = cursor.escape_offset as usize;
                    if escaped >= escape.len() { return Err(invalid_novel_cursor()); }
                    let count = (max_bytes - output.len()).min(escape.len() - escaped);
                    output.extend_from_slice(&escape[escaped..escaped + count]);
                    cursor.escape_offset += count as u8;
                    if cursor.escape_offset as usize == escape.len() {
                        cursor.escape_offset = 0; cursor.byte_offset += 1;
                    }
                } else {
                    if cursor.escape_offset != 0 { return Err(invalid_novel_cursor()); }
                    // Scan only the current output window, not the remaining
                    // large string on every call (which would be quadratic).
                    let available = (max_bytes - output.len()).min(value.len() - start);
                    let raw = &value.as_bytes()[start..start + available];
                    let count = raw.iter().position(|byte| matches!(*byte, 0..=31 | b'"' | b'\\')).unwrap_or(raw.len());
                    output.extend_from_slice(&raw[..count]); cursor.byte_offset += count as u64;
                }
            }
            StringClose => {
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, b"\"")? {
                    cursor.byte_offset = 0; cursor.escape_offset = 0;
                    if matches!(novel_json_value(novel, cursor.field), Some(NovelJsonValue::Array(_))) {
                        cursor.item = cursor.item.checked_add(1).ok_or_else(invalid_novel_cursor)?; cursor.phase = ArrayItem;
                    } else { cursor.field += 1; cursor.phase = Field; }
                }
            }
            ArrayItem => {
                let Some(NovelJsonValue::Array(values)) = novel_json_value(novel, cursor.field) else { return Err(invalid_novel_cursor()); };
                if cursor.item as usize > values.len() { return Err(invalid_novel_cursor()); }
                if cursor.item as usize == values.len() { cursor.phase = ArrayEnd; continue; }
                if cursor.item == 0 || novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, b",")? { cursor.phase = StringOpen; }
            }
            ArrayEnd => {
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, b"]")? { cursor.field += 1; cursor.phase = Field; }
            }
            Number => {
                let Some(NovelJsonValue::Number(value)) = novel_json_value(novel, cursor.field) else { return Err(invalid_novel_cursor()); };
                let mut bytes = [0u8; 20];
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, decimal(value as u64, &mut bytes))? { cursor.field += 1; cursor.phase = Field; }
            }
            End => {
                if cursor.field != 13 { return Err(invalid_novel_cursor()); }
                if novel_json_token(&mut output, max_bytes, &mut cursor.token_offset, b"}")? { cursor.phase = Done; }
            }
            Done => {}
        }
    }
    Ok((output, cursor.phase == Done))
}

/// Exact escaping shared by the durable cursor and asynchronous serializer.
pub(super) fn json_escape(byte: u8, encoded: &mut [u8; 6]) -> Option<&[u8]> {
    Some(match byte {
        b'"' => b"\\\"", b'\\' => b"\\\\", b'\n' => b"\\n", b'\r' => b"\\r",
        b'\t' => b"\\t", 8 => b"\\b", 12 => b"\\f",
        0..=31 => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            *encoded = [b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]];
            encoded
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{ArchiveNovel, ArchiveFile};
    #[test]
    fn durable_novel_cursor_preserves_every_field_at_all_small_boundaries() {
        let novel = ArchiveNovel {
            toc_url: "https://example.test/日本語?x=\"quoted\"".into(),
            title: "\0\u{01}\u{08}\u{0c}\n\r\t\\\" 日本語 😀".into(),
            author: "a".into(), sitename: "s".into(), domain: Some("".into()),
            tags: vec!["".into(), "日本語\\tag".into()], attribute_tag: Some("小説".into()),
            novel_type: u8::MAX, ncode: Some("n".into()), subtitle: Some("s".into()),
            general_lastup: Some("g".into()), last_update: Some("l".into()),
            virtual_members: Some(vec!["first".into(), "日本語😀".into(), "".into()]),
            files: vec![ArchiveFile::new("not-emitted.yaml", vec![1, 2, 3])],
        };
        let mut expected = serde_json::to_value(&novel).unwrap();
        expected.as_object_mut().unwrap().remove("files");
        for limit in (1..=96).chain([256, 65536]) {
            let mut cursor = NovelJsonCursor::default();
            let mut bytes = Vec::new();
            loop {
                // Simulate a fresh process/invocation for every output chunk.
                cursor = serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
                let (chunk, done) = novel_json_chunk(&novel, &mut cursor, limit).unwrap();
                assert!(!chunk.is_empty()); assert!(chunk.len() <= limit);
                bytes.extend(chunk);
                if done { break; }
            }
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(), expected);
            assert_eq!(novel_json_chunk(&novel, &mut cursor, limit).unwrap(), (Vec::new(), true));
        }
    }

    #[test]
    fn durable_novel_cursor_omits_absent_fields_but_keeps_empty_arrays_and_strings() {
        for members in [None, Some(Vec::new())] {
            let novel = ArchiveNovel { virtual_members: members, ..Default::default() };
            let mut expected = serde_json::to_value(&novel).unwrap(); expected.as_object_mut().unwrap().remove("files");
            let mut cursor = NovelJsonCursor::default();
            let (bytes, done) = novel_json_chunk(&novel, &mut cursor, 65536).unwrap();
            assert!(done); assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(), expected);
            let mut with_files = b"{\"files\":[],".to_vec(); with_files.extend_from_slice(&bytes[1..]);
            let decoded: ArchiveNovel = serde_json::from_slice(&with_files).unwrap();
            assert_eq!(decoded, novel);
        }
    }

    #[test]
    fn durable_novel_cursor_streams_six_mib_escaped_title_without_a_json_value() {
        use sha2::{Digest, Sha256};
        let novel = ArchiveNovel { title: "\0".repeat(6 * 1024 * 1024), ..Default::default() };
        let mut expected = Sha256::new(); expected.update(b"{\"toc_url\":\"\",\"title\":\"");
        let escaped = b"\\u0000".repeat(4096);
        for _ in 0..6 * 1024 * 1024 / 4096 { expected.update(&escaped); }
        expected.update(b"\",\"author\":\"\",\"sitename\":\"\",\"tags\":[],\"novel_type\":0}");
        let mut actual = Sha256::new();
        let mut cursor = NovelJsonCursor::default();
        let mut count = 0usize;
        loop {
            let checkpoint = serde_json::to_vec(&cursor).unwrap(); assert!(checkpoint.len() < 256);
            cursor = serde_json::from_slice(&checkpoint).unwrap();
            let (bytes, done) = novel_json_chunk(&novel, &mut cursor, 65536).unwrap();
            assert!(bytes.len() <= 65536 && !bytes.is_empty()); count += bytes.len(); actual.update(&bytes);
            if done { break; }
        }
        assert!(count > 36 * 1024 * 1024); assert_eq!(actual.finalize(), expected.finalize());
    }

    #[test]
    fn durable_novel_cursor_rejects_invalid_bounds_and_corrupt_versions() {
        let novel = ArchiveNovel::default();
        assert!(novel_json_chunk(&novel, &mut NovelJsonCursor::default(), 0).is_err());
        assert!(novel_json_chunk(&novel, &mut NovelJsonCursor::default(), 65537).is_err());
        let mut cursor = NovelJsonCursor { version: 2, ..Default::default() };
        assert!(novel_json_chunk(&novel, &mut cursor, 32).is_err());
        let mut cursor = NovelJsonCursor { phase: NovelJsonPhase::StringBody, byte_offset: u64::MAX, ..Default::default() };
        assert!(novel_json_chunk(&novel, &mut cursor, 32).is_err());
    }

    #[test]
    fn streamed_json_matches_the_existing_serde_schema_exactly() {
        futures::executor::block_on(async {
            let manifest = ArchiveManifest {
                app_version: Some("\0\u{01}\u{08}\u{0c}\n\r\t\\\" 日本語".into()), exported_at: None,
                novels: vec![ArchiveNovel {
                    toc_url: "url".into(), title: "title".into(), author: "author".into(), sitename: "site".into(),
                    domain: Some("d".into()), tags: vec!["first".into(), "".into()], attribute_tag: Some("小説".into()),
                    novel_type: u8::MAX, ncode: Some("n".into()), subtitle: Some("s".into()),
                    general_lastup: Some("g".into()), last_update: Some("l".into()),
                    files: vec![ArchiveFile { path: "toc.yaml".into(), size: Some(u64::MAX), sha256: Some("hash".into()), bytes: vec![1] }],
                    virtual_members: Some(vec!["https://example.test/日本語?x=\"quoted\"".into()]),
                }, ArchiveNovel::default()], ..Default::default()
            };
            let mut output = futures::io::Cursor::new(Vec::new());
            write(&mut output, &manifest).await.unwrap();
            assert_eq!(output.into_inner(), serde_json::to_vec(&manifest).unwrap());
        });
    }
}
