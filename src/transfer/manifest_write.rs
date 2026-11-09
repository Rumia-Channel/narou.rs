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
        let escaped: &[u8] = match byte {
            b'"' => b"\\\"", b'\\' => b"\\\\", b'\n' => b"\\n", b'\r' => b"\\r",
            b'\t' => b"\\t", 8 => b"\\b", 12 => b"\\f", 0..=31 => b"", _ => continue,
        };
        raw(writer, &value.as_bytes()[start..index]).await?;
        if escaped.is_empty() {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            writer.write_all(&[b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]).await?;
        } else { writer.write_all(escaped).await?; }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{ArchiveNovel, ArchiveFile};
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
                }, ArchiveNovel::default()], ..Default::default()
            };
            let mut output = futures::io::Cursor::new(Vec::new());
            write(&mut output, &manifest).await.unwrap();
            assert_eq!(output.into_inner(), serde_json::to_vec(&manifest).unwrap());
        });
    }
}
