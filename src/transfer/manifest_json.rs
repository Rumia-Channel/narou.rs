//! Decode only schema metadata, directly into its final representation. Unknown
//! values are syntax-checked and discarded; even a large ignored array never
//! becomes a raw JSON buffer or a `serde_json::Value` tree.
use futures::io::{AsyncRead, AsyncReadExt};

use super::{ArchiveFile, ArchiveManifest, ArchiveNovel, MAX_ENTRIES, MAX_MANIFEST_BYTES,
    MAX_NOVELS, Result, TransferError};
use super::memory::{MetadataBudget, default_limit};

/// Decode metadata from bounded reads using the same schema as archive import.
/// Wasm admission applies before metadata growth; unknown values are discarded.
pub async fn read_manifest<R: AsyncRead + Unpin>(reader: R) -> Result<ArchiveManifest> {
    read_with_budget(reader, default_limit()).await
}

async fn read_with_budget<R: AsyncRead + Unpin>(reader: R, limit: usize) -> Result<ArchiveManifest> {
    let mut json = Json {
        reader, bytes: vec![0; 64 * 1024], position: 0, length: 0, read: 0,
        memory: MetadataBudget { used: 0, limit },
    };
    let mut members = json.members().await?;
    let mut manifest = ArchiveManifest::default();
    let mut seen = 0u16;
    let mut file_count = 0usize;
    while let Some(name) = json.member(&mut members, &[b"format_version", b"app_version", b"exported_at", b"novels"]).await? {
        match name.as_bytes() {
            b"format_version" => {
                mark(&mut seen, 0)?;
                manifest.format_version = json.unsigned(u32::MAX as u64).await? as u32;
            }
            b"app_version" => { mark(&mut seen, 1)?; manifest.app_version = json.optional_string().await?; }
            b"exported_at" => { mark(&mut seen, 2)?; manifest.exported_at = json.optional_string().await?; }
            b"novels" => {
                mark(&mut seen, 3)?;
                json.expect(b'[').await?;
                let mut first = true;
                while json.item(&mut first, b']').await? {
                    if manifest.novels.len() >= MAX_NOVELS {
                        return Err(TransferError::TooLarge("too many novels".into()));
                    }
                    json.memory.reserve(&mut manifest.novels, 1)?;
                    manifest.novels.push(json.novel(&mut file_count).await?);
                }
            }
            _ => json.skip_value().await?,
        }
    }
    required(seen, 1)?;
    if json.peek().await?.is_some() { return Err(invalid("trailing manifest data")); }
    Ok(manifest)
}


/// Schema names fit in 16 UTF-8 bytes. Longer keys are still fully decoded and
/// validated, but cannot match a schema field and need no heap allocation.
struct FieldName { bytes: [u8; 16], length: usize }
impl FieldName {
    fn as_bytes(&self) -> &[u8] {
        if self.length > self.bytes.len() { &[] } else { &self.bytes[..self.length] }
    }
}

// Derived serde structs accept both named objects and positional sequences.
// Keep that compatibility without constructing an intermediate JSON model.
struct Members { end: u8, first: bool, position: Option<usize> }

struct Json<R> {
    reader: R, bytes: Vec<u8>, position: usize, length: usize, read: u64,
    memory: MetadataBudget,
}
impl<R: AsyncRead + Unpin> Json<R> {
    async fn members(&mut self) -> Result<Members> {
        let (end, position) = match self.peek().await? {
            Some(b'{') => (b'}', None),
            Some(b'[') => (b']', Some(0)),
            _ => return Err(invalid("expected manifest structure")),
        };
        self.position += 1;
        Ok(Members { end, first: true, position })
    }

    async fn member(&mut self, members: &mut Members, fields: &[&[u8]]) -> Result<Option<FieldName>> {
        if !self.item(&mut members.first, members.end).await? { return Ok(None); }
        if let Some(position) = &mut members.position {
            let name = fields.get(*position).ok_or_else(|| invalid("too many positional manifest fields"))?;
            *position += 1;
            let mut field = FieldName { bytes: [0; 16], length: name.len() };
            field.bytes[..name.len()].copy_from_slice(name);
            Ok(Some(field))
        } else {
            let name = self.field().await?;
            self.expect(b':').await?;
            Ok(Some(name))
        }
    }

    async fn byte(&mut self) -> Result<Option<u8>> {
        if self.position == self.length {
            self.length = self.reader.read(&mut self.bytes).await?;
            self.position = 0;
            self.read += self.length as u64;
            if self.read > MAX_MANIFEST_BYTES { return Err(TransferError::TooLarge("manifest too large".into())); }
            if self.length == 0 { return Ok(None); }
        }
        Ok(Some(self.bytes[self.position]))
    }

    async fn take(&mut self) -> Result<u8> {
        let byte = self.byte().await?.ok_or_else(|| invalid("truncated JSON value"))?;
        self.position += 1;
        Ok(byte)
    }

    async fn peek(&mut self) -> Result<Option<u8>> {
        while let Some(byte) = self.byte().await? {
            if !matches!(byte, b' ' | b'\n' | b'\r' | b'\t') { return Ok(Some(byte)); }
            self.position += 1;
        }
        Ok(None)
    }

    async fn expect(&mut self, expected: u8) -> Result<()> {
        if self.peek().await? != Some(expected) { return Err(invalid("unexpected JSON delimiter")); }
        self.position += 1;
        Ok(())
    }

    async fn item(&mut self, first: &mut bool, end: u8) -> Result<bool> {
        if self.peek().await? == Some(end) { self.position += 1; return Ok(false); }
        if !*first { self.expect(b',').await?; }
        *first = false;
        Ok(true)
    }

    async fn literal(&mut self, literal: &[u8]) -> Result<()> {
        let _ = self.peek().await?;
        for expected in literal {
            if self.take().await? != *expected { return Err(invalid("invalid JSON literal")); }
        }
        Ok(())
    }

    async fn hex_escape(&mut self) -> Result<u16> {
        let mut code = 0u16;
        for _ in 0..4 {
            let digit = match self.take().await? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(invalid("invalid JSON Unicode escape")),
            };
            code = (code << 4) | digit as u16;
        }
        Ok(code)
    }

    /// One decoded character, without retaining its JSON spelling. Field names
    /// and known strings have the same UTF-8/surrogate requirements as String.
    async fn string_char(&mut self) -> Result<Option<char>> {
        let byte = self.take().await?;
        let character = match byte {
            b'"' => return Ok(None),
            0..=0x1f => return Err(invalid("control character in JSON string")),
            b'\\' => match self.take().await? {
                b'"' => '"', b'\\' => '\\', b'/' => '/', b'b' => '\u{08}',
                b'f' => '\u{0c}', b'n' => '\n', b'r' => '\r', b't' => '\t',
                b'u' => {
                    let first = self.hex_escape().await?;
                    let code = if (0xd800..=0xdbff).contains(&first) {
                        if self.take().await? != b'\\' || self.take().await? != b'u' {
                            return Err(invalid("unpaired JSON surrogate"));
                        }
                        let second = self.hex_escape().await?;
                        if !(0xdc00..=0xdfff).contains(&second) { return Err(invalid("unpaired JSON surrogate")); }
                        0x10000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
                    } else { first as u32 };
                    char::from_u32(code).ok_or_else(|| invalid("unpaired JSON surrogate"))?
                }
                _ => return Err(invalid("invalid JSON escape")),
            },
            0x20..=0x7f => byte as char,
            _ => {
                let length = match byte {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(invalid("invalid UTF-8 JSON string")),
                };
                let mut bytes = [byte, 0, 0, 0];
                for next in &mut bytes[1..length] { *next = self.take().await?; }
                std::str::from_utf8(&bytes[..length]).map_err(|_| invalid("invalid UTF-8 JSON string"))?
                    .chars().next().expect("nonempty UTF-8 character")
            }
        };
        Ok(Some(character))
    }

    async fn field(&mut self) -> Result<FieldName> {
        self.expect(b'"').await?;
        let mut field = FieldName { bytes: [0; 16], length: 0 };
        while let Some(character) = self.string_char().await? {
            if field.length <= field.bytes.len() {
                let end = field.length + character.len_utf8();
                if end <= field.bytes.len() {
                    character.encode_utf8(&mut field.bytes[field.length..end]);
                    field.length = end;
                } else { field.length = field.bytes.len() + 1; }
            }
        }
        Ok(field)
    }

    async fn string(&mut self) -> Result<String> {
        self.expect(b'"').await?;
        let mut decoded = Vec::new();
        while let Some(character) = self.string_char().await? {
            let mut bytes = [0u8; 4];
            let bytes = character.encode_utf8(&mut bytes).as_bytes();
            self.memory.reserve(&mut decoded, bytes.len())?;
            decoded.extend_from_slice(bytes);
        }
        String::from_utf8(decoded).map_err(|_| invalid("invalid UTF-8 JSON string"))
    }

    async fn optional_string(&mut self) -> Result<Option<String>> {
        if self.peek().await? == Some(b'n') { self.literal(b"null").await?; Ok(None) }
        else { self.string().await.map(Some) }
    }

    async fn unsigned(&mut self, maximum: u64) -> Result<u64> {
        let first = self.peek().await?.ok_or_else(|| invalid("missing JSON integer"))?;
        if !first.is_ascii_digit() { return Err(invalid("expected unsigned JSON integer")); }
        self.position += 1;
        let mut value = (first - b'0') as u64;
        while let Some(byte @ b'0'..=b'9') = self.byte().await? {
            if first == b'0' { return Err(invalid("leading zero in JSON integer")); }
            self.position += 1;
            value = value.checked_mul(10).and_then(|value| value.checked_add((byte - b'0') as u64))
                .filter(|value| *value <= maximum).ok_or_else(|| invalid("JSON integer out of range"))?;
        }
        if matches!(self.byte().await?, Some(b'.' | b'e' | b'E')) {
            return Err(invalid("expected unsigned JSON integer"));
        }
        if value > maximum { return Err(invalid("JSON integer out of range")); }
        Ok(value)
    }

    async fn optional_unsigned(&mut self) -> Result<Option<u64>> {
        if self.peek().await? == Some(b'n') { self.literal(b"null").await?; Ok(None) }
        else { self.unsigned(u64::MAX).await.map(Some) }
    }

    async fn novel(&mut self, file_count: &mut usize) -> Result<ArchiveNovel> {
        let mut members = self.members().await?;
        let mut novel = ArchiveNovel::default();
        let mut seen = 0u16;
        let fields: &[&[u8]] = &[b"toc_url", b"title", b"author", b"sitename", b"domain", b"tags",
            b"attribute_tag", b"novel_type", b"files", b"ncode", b"subtitle", b"general_lastup", b"last_update",
            b"virtual_members"];
        while let Some(name) = self.member(&mut members, fields).await? {
            match name.as_bytes() {
                b"toc_url" => { mark(&mut seen, 0)?; novel.toc_url = self.string().await?; }
                b"title" => { mark(&mut seen, 1)?; novel.title = self.string().await?; }
                b"author" => { mark(&mut seen, 2)?; novel.author = self.string().await?; }
                b"sitename" => { mark(&mut seen, 3)?; novel.sitename = self.string().await?; }
                b"novel_type" => { mark(&mut seen, 4)?; novel.novel_type = self.unsigned(u8::MAX as u64).await? as u8; }
                b"domain" => { mark(&mut seen, 5)?; novel.domain = self.optional_string().await?; }
                b"attribute_tag" => { mark(&mut seen, 6)?; novel.attribute_tag = self.optional_string().await?; }
                b"ncode" => { mark(&mut seen, 7)?; novel.ncode = self.optional_string().await?; }
                b"subtitle" => { mark(&mut seen, 8)?; novel.subtitle = self.optional_string().await?; }
                b"general_lastup" => { mark(&mut seen, 9)?; novel.general_lastup = self.optional_string().await?; }
                b"last_update" => { mark(&mut seen, 10)?; novel.last_update = self.optional_string().await?; }
                b"tags" => {
                    mark(&mut seen, 11)?;
                    self.expect(b'[').await?;
                    let mut first = true;
                    while self.item(&mut first, b']').await? {
                        self.memory.reserve(&mut novel.tags, 1)?;
                        novel.tags.push(self.string().await?);
                    }
                }
                b"files" => {
                    mark(&mut seen, 12)?;
                    self.expect(b'[').await?;
                    let mut first = true;
                    while self.item(&mut first, b']').await? {
                        *file_count += 1;
                        if *file_count >= MAX_ENTRIES { return Err(TransferError::TooLarge("too many files".into())); }
                        self.memory.reserve(&mut novel.files, 1)?;
                        novel.files.push(self.file().await?);
                    }
                }
                b"virtual_members" => {
                    mark(&mut seen, 13)?;
                    if self.peek().await? == Some(b'n') {
                        self.literal(b"null").await?;
                    } else {
                        self.expect(b'[').await?;
                        let mut values = Vec::new();
                        let mut first = true;
                        while self.item(&mut first, b']').await? {
                            if values.len() >= MAX_NOVELS { return Err(TransferError::TooLarge("too many virtual members".into())); }
                            self.memory.reserve(&mut values, 1)?;
                            values.push(self.string().await?);
                        }
                        novel.virtual_members = Some(values);
                    }
                }
                _ => self.skip_value().await?,
            }
        }
        required(seen, 0b1_1111)?;
        Ok(novel)
    }

    async fn file(&mut self) -> Result<ArchiveFile> {
        let mut members = self.members().await?;
        let mut file = ArchiveFile::default();
        let mut seen = 0u16;
        while let Some(name) = self.member(&mut members, &[b"path", b"sha256", b"size"]).await? {
            match name.as_bytes() {
                b"path" => { mark(&mut seen, 0)?; file.path = self.string().await?; }
                b"sha256" => { mark(&mut seen, 1)?; file.sha256 = self.optional_string().await?; }
                b"size" => { mark(&mut seen, 2)?; file.size = self.optional_unsigned().await?; }
                // `bytes` is serde(skip), and therefore behaves like any other
                // unknown field, including when it occurs more than once.
                _ => self.skip_value().await?,
            }
        }
        required(seen, 1)?;
        Ok(file)
    }

    /// Match serde's IgnoredAny string handling: validate control characters,
    /// escapes, and hex digits, but don't interpret Unicode or pair surrogates
    /// in data that will never become a Rust String.
    async fn skip_string(&mut self) -> Result<()> {
        self.expect(b'"').await?;
        loop {
            match self.take().await? {
                b'"' => return Ok(()),
                b'\\' => match self.take().await? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                    b'u' => { self.hex_escape().await?; }
                    _ => return Err(invalid("invalid JSON escape")),
                },
                0..=0x1f => return Err(invalid("control character in JSON string")),
                _ => {}
            }
        }
    }

    async fn digits(&mut self) -> Result<()> {
        if !self.byte().await?.is_some_and(|byte| byte.is_ascii_digit()) {
            return Err(invalid("invalid JSON number"));
        }
        while self.byte().await?.is_some_and(|byte| byte.is_ascii_digit()) { self.position += 1; }
        Ok(())
    }

    async fn skip_number(&mut self) -> Result<()> {
        if self.byte().await? == Some(b'-') { self.position += 1; }
        match self.byte().await? {
            Some(b'0') => {
                self.position += 1;
                if self.byte().await?.is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(invalid("leading zero in JSON number"));
                }
            }
            Some(b'1'..=b'9') => self.digits().await?,
            _ => return Err(invalid("invalid JSON number")),
        }
        if self.byte().await? == Some(b'.') { self.position += 1; self.digits().await?; }
        if matches!(self.byte().await?, Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.byte().await?, Some(b'+' | b'-')) { self.position += 1; }
            self.digits().await?;
        }
        Ok(())
    }

    async fn skip_value(&mut self) -> Result<()> {
        let mut frames = Vec::new();
        let result = self.skip_value_inner(&mut frames).await;
        self.memory.release(&frames);
        result
    }

    async fn skip_value_inner(&mut self, frames: &mut Vec<u8>) -> Result<()> {
        const ARRAY_FIRST: u8 = 0;
        const ARRAY_NEXT: u8 = 1;
        const OBJECT_FIRST: u8 = 2;
        const OBJECT_NEXT: u8 = 3;
        loop {
            match self.peek().await? {
                Some(b'n') => self.literal(b"null").await?,
                Some(b't') => self.literal(b"true").await?,
                Some(b'f') => self.literal(b"false").await?,
                Some(b'"') => self.skip_string().await?,
                Some(b'-' | b'0'..=b'9') => self.skip_number().await?,
                Some(byte @ (b'[' | b'{')) => {
                    self.position += 1;
                    self.memory.reserve(frames, 1)?;
                    frames.push(if byte == b'[' { ARRAY_FIRST } else { OBJECT_FIRST });
                }
                _ => return Err(invalid("expected JSON value")),
            }
            loop {
                let Some(&frame) = frames.last() else { return Ok(()); };
                let array = frame == ARRAY_FIRST || frame == ARRAY_NEXT;
                let end = if array { b']' } else { b'}' };
                if self.peek().await? == Some(end) {
                    self.position += 1;
                    frames.pop();
                    continue;
                }
                if frame == ARRAY_NEXT || frame == OBJECT_NEXT { self.expect(b',').await?; }
                if !array {
                    self.skip_string().await?;
                    self.expect(b':').await?;
                }
                *frames.last_mut().expect("current container") = if array { ARRAY_NEXT } else { OBJECT_NEXT };
                break;
            }
        }
    }
}

fn mark(seen: &mut u16, field: u32) -> Result<()> {
    let bit = 1u16 << field;
    if *seen & bit != 0 { return Err(invalid("duplicate known manifest field")); }
    *seen |= bit;
    Ok(())
}
fn required(seen: u16, fields: u16) -> Result<()> {
    if seen & fields != fields { return Err(invalid("missing required manifest field")); }
    Ok(())
}
fn invalid(message: &str) -> TransferError { TransferError::InvalidArchive(message.into()) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::{pin::Pin, task::{Context, Poll}};

    /// Supplies millions of array elements without allocating their input text.
    struct Pairs { remaining: usize, position: usize }
    impl AsyncRead for Pairs {
        fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &mut [u8]) -> Poll<std::io::Result<usize>> {
            let count = bytes.len().min(self.remaining);
            for byte in &mut bytes[..count] {
                *byte = b"0,"[self.position % 2];
                self.position += 1;
            }
            self.remaining -= count;
            Poll::Ready(Ok(count))
        }
    }

    #[test]
    fn ignored_five_million_element_array_needs_only_a_container_stack() {
        futures::executor::block_on(async {
            let reader = futures::io::Cursor::new(br#"{"format_version":1,"unknown":["#)
                .chain(Pairs { remaining: 4_999_999 * 2, position: 0 })
                .chain(futures::io::Cursor::new(br#"0],"unknown":{"nested":[null,true,false]},"novels":[]}"#));
            // Decoded metadata is empty. No number/string/array models may be
            // allocated, and each ignored field must release its stack charge.
            let parsed = read_with_budget(reader, 64).await.unwrap();
            assert_eq!(parsed.format_version, 1);
            assert!(parsed.novels.is_empty());
        });
    }

    #[test]
    fn streaming_metadata_matches_typed_serde() {
        futures::executor::block_on(async {
            let novel = r#"{"toc_url":"https://e/","title":"title } \\\" \ud83d\ude00","author":"あ","sitename":"s","novel_type":255,"tags":["", "a\nb", "\u0000"],"files":[{"path":"toc.yaml","size":18446744073709551615,"bytes":[1,2],"bytes":null,"sha256":null}],"domain":null,"attribute_tag":null,"ncode":"n","subtitle":"x","general_lastup":"y","last_update":"z"}"#;
            let valid = [
                format!(r#"{{"format_version":1,"app_version":null,"exported_at":"now","novels":[{novel}],"unknown":[1e999999,{{"x":"\ud800"}}],"unknown":false}}"#),
                r#"{"format_version":1,"novels":[]}"#.into(),
                r#"{"format_version":1,"very_long_unrecognized_field_name":[[],{}],"\u0061pp_version":"ok"}"#.into(),
                r#"[1]"#.into(),
                r#"[1,null,null,[["u","t","a","s",null,[],null,1,[["toc.yaml",null,4]]]]]"#.into(),
                r#"{"format_version":2,"novels":[{"toc_url":"https://narou.rs/virtual/0123456789abcdef0123456789abcdef","title":"parent","author":"a","sitename":"s","novel_type":0,"virtual_members":["https://e/second/","https://e/first/"]}]}"#.into(),
                r#"{"format_version":1,"novels":[{"toc_url":"u","title":"t","author":"a","sitename":"s","novel_type":1,"virtual_members":null}]}"#.into(),
            ];
            for bytes in valid {
                let expected: ArchiveManifest = serde_json::from_slice(bytes.as_bytes()).unwrap();
                let actual = read_manifest(futures::io::Cursor::new(bytes.as_bytes())).await.unwrap();
                assert_eq!(actual, expected, "{bytes}");
            }
        });
    }

    #[test]
    fn invalid_syntax_and_duplicate_known_fields_match_typed_serde() {
        futures::executor::block_on(async {
            let invalid = [
                r#"[]"#, r#"[1,null,null,[],false]"#, r#"[1,null,null,[["u","t","a","s"]]]"#,
                r#"{}"#, r#"{"format_version":1,"format_version":1}"#,
                r#"{"format_version":1,"app_version":null,"app_version":null}"#,
                r#"{"format_version":1,"novels":[],"\u006eovels":[]}"#,
                r#"{"format_version":1,}"#, r#"{"format_version":1,"x":[0,]}"#,
                r#"{"format_version":1,"x":{"a":1,}}"#, r#"{"format_version":1,"x":[0 1]}"#,
                r#"{"format_version":1,"x":{"a" 1}}"#, r#"{"format_version":1,"x":01}"#,
                r#"{"format_version":1,"x":1.}"#, r#"{"format_version":1,"x":1e+}"#,
                r#"{"format_version":1,"x":tru}"#, r#"{"format_version":1,"x":"\q"}"#,
                r#"{"format_version":1,"x":"\u000x"}"#, r#"{"format_version":1,"app_version":"\ud800"}"#,
                r#"{"format_version":1,"app_version":"\udc00"}"#, r#"{"format_version":1,"\ud800":0}"#,
                r#"{"format_version":1.0}"#, r#"{"format_version":-0}"#, r#"{"format_version":4294967296}"#,
                r#"{"format_version":1,"novels":[{}]}"#,
                r#"{"format_version":1,"novels":[{"toc_url":"u","title":"t","title":"t","author":"a","sitename":"s","novel_type":1}]}"#,
                r#"{"format_version":1,"novels":[{"toc_url":"u","title":"t","author":"a","sitename":"s","novel_type":256}]}"#,
                r#"{"format_version":1,"novels":[{"toc_url":"u","title":"t","author":"a","sitename":"s","novel_type":1,"files":[{"path":"a","size":null,"size":null}]}]}"#,
                r#"{"format_version":1,"novels":[{"toc_url":"u","title":"t","author":"a","sitename":"s","novel_type":1,"files":[{"path":"a","size":18446744073709551616}]}]}"#,
                r#"{"format_version":2,"novels":[{"toc_url":"u","title":"t","author":"a","sitename":"s","novel_type":0,"virtual_members":[],"virtual_members":[]}]}"#,
                r#"{"format_version":2,"novels":[{"toc_url":"u","title":"t","author":"a","sitename":"s","novel_type":0,"virtual_members":[1]}]}"#,
            ];
            for bytes in invalid {
                assert!(serde_json::from_slice::<ArchiveManifest>(bytes.as_bytes()).is_err(), "reference accepted {bytes}");
                assert!(read_manifest(futures::io::Cursor::new(bytes.as_bytes())).await.is_err(), "stream accepted {bytes}");
            }
        });
    }

    #[test]
    fn utf8_and_surrogates_remain_correct_across_read_boundaries() {
        futures::executor::block_on(async {
            struct OneByte<'a>(&'a [u8]);
            impl AsyncRead for OneByte<'_> {
                fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &mut [u8]) -> Poll<std::io::Result<usize>> {
                    if bytes.is_empty() || self.0.is_empty() { return Poll::Ready(Ok(0)); }
                    bytes[0] = self.0[0]; self.0 = &self.0[1..]; Poll::Ready(Ok(1))
                }
            }
            let valid = r#"{"format_version":1,"app_version":"日本語 \ud83d\ude00 \\ \""}"#.as_bytes();
            assert_eq!(read_manifest(OneByte(valid)).await.unwrap(), serde_json::from_slice(valid).unwrap());
            for bytes in [
                b"{\"format_version\":1,\"app_version\":\"\xff\"}".as_slice(),
                b"{\"format_version\":1,\"app_version\":\"\xed\xa0\x80\"}".as_slice(),
                b"{\"format_version\":1,\"app_version\":\"\xe3\x81\"}".as_slice(),
                b"{\"format_version\":1,\"app_version\":\"\x00\"}".as_slice(),
            ] {
                assert!(read_manifest(OneByte(bytes)).await.is_err());
                assert!(serde_json::from_slice::<ArchiveManifest>(bytes).is_err());
            }
            // serde IgnoredAny deliberately doesn't decode unknown strings.
            let ignored = b"{\"format_version\":1,\"unknown\":\"\xff\"}";
            assert_eq!(read_manifest(OneByte(ignored)).await.unwrap(), serde_json::from_slice(ignored).unwrap());
            assert!(read_manifest(OneByte(&valid[..valid.len() - 1])).await.is_err());
        });
    }

    #[test]
    fn metadata_growth_is_rejected_before_reserving() {
        let mut memory = MetadataBudget { used: 0, limit: MetadataBudget::allocation::<String>(1).unwrap() };
        let mut strings: Vec<String> = Vec::new();
        memory.reserve(&mut strings, 1).unwrap();
        strings.push(String::new());
        let before = (memory.used, strings.capacity());
        assert!(matches!(memory.reserve(&mut strings, 1), Err(TransferError::TooLarge(_))));
        assert_eq!((memory.used, strings.capacity()), before);
        futures::executor::block_on(async {
            let bytes = br#"{"format_version":1,"app_version":"this retained metadata exceeds a deliberately tiny allocation budget"}"#;
            assert!(matches!(read_with_budget(futures::io::Cursor::new(bytes), 32).await, Err(TransferError::TooLarge(_))));
            let tags = br#"{"format_version":1,"novels":[{"toc_url":"","title":"","author":"","sitename":"","novel_type":1,"tags":["","","","","","","","","","","","","","","",""]}]}"#;
            assert!(matches!(read_with_budget(futures::io::Cursor::new(tags), std::mem::size_of::<ArchiveNovel>() + 128).await, Err(TransferError::TooLarge(_))));
            let members = br#"{"format_version":2,"novels":[{"toc_url":"","title":"","author":"","sitename":"","novel_type":0,"virtual_members":["","","","","","","","","","","","","","","",""]}]}"#;
            assert!(matches!(read_with_budget(futures::io::Cursor::new(members), std::mem::size_of::<ArchiveNovel>() + 128).await, Err(TransferError::TooLarge(_))));
        });
    }

    #[test]
    fn ignored_strings_keys_and_deep_nesting_do_not_build_values() {
        futures::executor::block_on(async {
            let string = futures::io::Cursor::new(br#"{"format_version":1,"unknown":""#)
                .chain(Pairs { remaining: 1024 * 1024, position: 0 })
                .chain(futures::io::Cursor::new(br#""}"#));
            assert!(read_with_budget(string, 0).await.is_ok());
            let key = futures::io::Cursor::new(br#"{"format_version":1,""#)
                .chain(Pairs { remaining: 128 * 1024, position: 0 })
                .chain(futures::io::Cursor::new(br#"":null}"#));
            assert!(read_with_budget(key, 0).await.is_ok());
            let nested = [r#"{"format_version":1,"unknown":"#.to_owned(),
                "[".repeat(1024), "null".into(), "]".repeat(1024), "}".into()].concat();
            assert_eq!(read_with_budget(futures::io::Cursor::new(nested.as_bytes()), 4096).await.unwrap(),
                serde_json::from_str(&nested).unwrap());
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_retains_the_existing_manifest_capacity() {
        futures::executor::block_on(async {
            let length = super::super::MAX_MANIFEST_METADATA_BYTES + 1;
            let reader = futures::io::Cursor::new(br#"{"format_version":1,"app_version":""#)
                .chain(Pairs { remaining: length, position: 0 })
                .chain(futures::io::Cursor::new(br#""}"#));
            let manifest = read_manifest(reader).await.unwrap();
            assert_eq!(manifest.app_version.unwrap().len(), length);
        });
    }
}
