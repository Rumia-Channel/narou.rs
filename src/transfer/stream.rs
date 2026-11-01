//! Incremental ZIP output. File bodies are released after each entry; only the
//! manifest and ZIP directory remain resident until the archive is finished.
use std::collections::HashSet;
use std::io::Write;

use sha2::{Digest, Sha256};
use zip::write::{SimpleFileOptions, StreamWriter};
use zip::{CompressionMethod, ZipWriter};

use super::{ArchiveFile, ArchiveManifest, ArchiveNovel, FORMAT_VERSION, MANIFEST_PATH,
    MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_FILE_BYTES, MAX_MANIFEST_BYTES, MAX_NOVELS,
    MAX_TOTAL_UNCOMPRESSED_BYTES, Result, TransferError};
use super::archive::{reject_file_directory_conflicts, validate_relative_path};
/// One-entry output buffer, drained by the transport before reading another file.
#[derive(Clone, Default)]
pub struct TransferBuffer(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

impl TransferBuffer {
    pub fn take(&self) -> Vec<u8> { std::mem::take(&mut *self.0.lock()) }
}

impl Write for TransferBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}


struct LimitedWriter<W> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(bytes.len() as u64) > MAX_ARCHIVE_BYTES {
            return Err(std::io::Error::other("転送 ZIP が上限の 512 MiB を超えています"));
        }
        let count = self.inner.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}

pub struct ArchiveStream<W: Write> {
    writer: ZipWriter<StreamWriter<LimitedWriter<W>>>,
    manifest: ArchiveManifest,
    paths: HashSet<String>,
    files: usize,
    uncompressed: u64,
}

impl<W: Write> ArchiveStream<W> {
    pub fn new(writer: W, app_version: String, exported_at: String) -> Self {
        Self {
            writer: ZipWriter::new_stream(LimitedWriter { inner: writer, written: 0 }),
            manifest: ArchiveManifest { format_version: FORMAT_VERSION,
                app_version: Some(app_version), exported_at: Some(exported_at), novels: Vec::new() },
            paths: HashSet::new(), files: 1, uncompressed: 0,
        }
    }

    pub fn begin_novel(&mut self, mut novel: ArchiveNovel) -> Result<()> {
        self.end_novel()?;
        if self.manifest.novels.len() >= MAX_NOVELS {
            return Err(TransferError::TooLarge(format!("作品数の上限は {MAX_NOVELS} 件です")));
        }
        novel.files.clear();
        super::attribute::normalize(&mut novel)?;
        self.manifest.novels.push(novel);
        self.paths.clear();
        Ok(())
    }

    pub fn set_subtitle(&mut self, subtitle: Option<String>) {
        if let Some(novel) = self.manifest.novels.last_mut() {
            novel.subtitle = subtitle;
        }
    }

    pub fn add_file(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        let path = validate_relative_path(path)?;
        let index = self.manifest.novels.len().checked_sub(1)
            .ok_or_else(|| TransferError::InvalidArchive("作品が指定されていません".into()))?;
        if !self.paths.insert(path.to_lowercase()) {
            return Err(TransferError::InvalidArchive(format!("duplicate file path {path:?}")));
        }
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(TransferError::TooLarge(format!("{path}: ファイルが上限を超えています")));
        }
        self.files += 1;
        self.uncompressed = self.uncompressed.saturating_add(bytes.len() as u64);
        if self.files > MAX_ENTRIES || self.uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(TransferError::TooLarge("転送データの件数・展開後サイズが上限を超えています".into()));
        }
        let sha256 = hex::encode(Sha256::digest(bytes));
        self.writer.start_file(format!("novels/{index}/{path}"), options())?;
        self.writer.write_all(bytes).map_err(zip_io)?;
        self.manifest.novels[index].files.push(ArchiveFile {
            path, bytes: Vec::new(), sha256: Some(sha256), size: Some(bytes.len() as u64),
        });
        Ok(())
    }

    fn end_novel(&self) -> Result<()> {
        if let Some(novel) = self.manifest.novels.last() {
            let index = self.manifest.novels.len() - 1;
            reject_file_directory_conflicts(novel.files.iter().map(|file| file.path.as_str()), index)?;
            if !novel.files.iter().any(|file| file.path == "toc.yaml") {
                return Err(TransferError::InvalidArchive(format!("{} に toc.yaml がありません", novel.title)));
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.end_novel()?;
        let bytes = serde_json::to_vec(&self.manifest)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES
            || self.uncompressed.saturating_add(bytes.len() as u64) > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(TransferError::TooLarge("manifest を含む転送データが上限を超えています".into()));
        }
        self.writer.start_file(MANIFEST_PATH, options())?;
        self.writer.write_all(&bytes).map_err(zip_io)?;
        self.writer.finish()?;
        Ok(())
    }
}

fn options() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
}

fn zip_io(error: std::io::Error) -> TransferError {
    TransferError::Zip(zip::result::ZipError::Io(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    #[test]
    fn streamed_archive_roundtrips_all_158_novels_and_their_hashes() {
        let sink = Sink::default();
        let mut stream = ArchiveStream::new(sink.clone(), "test".into(), "fixed".into());
        for index in 0..158 {
            stream.begin_novel(ArchiveNovel { toc_url: format!("https://example.test/{index}/"),
                title: format!("作品 {index}"), author: "作者".into(), sitename: "site".into(),
                novel_type: 1, ..Default::default() }).unwrap();
            stream.add_file("toc.yaml", format!("title: 作品 {index}\n").as_bytes()).unwrap();
            stream.add_file("本文/1 話.yaml", format!("body: 本文 {index}\n").as_bytes()).unwrap();
        }
        stream.finish().unwrap();
        let data = sink.0.lock().unwrap();
        let parsed = super::super::parse_archive(&data).unwrap();
        assert_eq!(parsed.novels.len(), 158);
        for (index, novel) in parsed.novels.iter().enumerate() {
            assert_eq!(novel.files.iter().find(|(path, _)| path == "本文/1 話.yaml").unwrap().1,
                format!("body: 本文 {index}\n").as_bytes());
        }
    }

    #[test]
    fn stream_rejects_missing_toc_and_unsafe_or_conflicting_paths() {
        let mut writer = ArchiveStream::new(Vec::new(), "test".into(), "fixed".into());
        writer.begin_novel(ArchiveNovel::default()).unwrap();
        assert!(writer.add_file("../escape", b"bad").is_err());
        writer.add_file("toc.yaml", b"toc").unwrap();
        writer.add_file("a", b"file").unwrap();
        writer.add_file("a/b", b"child").unwrap();
        assert!(writer.finish().is_err());
        let mut writer = ArchiveStream::new(Vec::new(), "test".into(), "fixed".into());
        writer.begin_novel(ArchiveNovel::default()).unwrap();
        writer.add_file("body.yaml", b"body").unwrap();
        assert!(writer.finish().is_err());
    }
}
