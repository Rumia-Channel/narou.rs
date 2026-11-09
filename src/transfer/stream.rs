//! Incremental ZIP output. Bodies are read and written in 64 KiB chunks; only
//! manifest metadata and the ZIP directory remain resident until completion.
use std::collections::HashSet;
use std::io::{Read, Write};

use sha2::{Digest, Sha256};
use zip::write::{SimpleFileOptions, StreamWriter};
use zip::{CompressionMethod, ZipWriter};

use super::{ArchiveFile, ArchiveManifest, ArchiveNovel, FORMAT_VERSION, MANIFEST_PATH,
    MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_FILE_BYTES, MAX_MANIFEST_BYTES, MAX_NOVELS,
    MAX_TOTAL_UNCOMPRESSED_BYTES, Result, TransferError};
use super::archive::{reject_file_directory_conflicts, validate_relative_path};

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
    paths: HashSet<[u8; 32]>,
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
        self.add_reader(path, &mut std::io::Cursor::new(bytes))
    }

    /// Copy one entry without retaining its body. The destination's `Write`
    /// implementation provides backpressure and cancellation.
    pub fn add_reader(&mut self, path: &str, reader: &mut dyn Read) -> Result<()> {
        let path = validate_relative_path(path)?;
        let index = self.manifest.novels.len().checked_sub(1)
            .ok_or_else(|| TransferError::InvalidArchive("作品が指定されていません".into()))?;
        if !self.paths.insert(Sha256::digest(path.to_lowercase().as_bytes()).into()) {
            return Err(TransferError::InvalidArchive(format!("duplicate file path {path:?}")));
        }
        self.files += 1;
        if self.files > MAX_ENTRIES {
            return Err(TransferError::TooLarge("転送データの件数が上限を超えています".into()));
        }
        self.writer.start_file(format!("novels/{index}/{path}"), options())?;
        let mut buffer = [0u8; 64 * 1024];
        let mut hash = Sha256::new();
        let mut size = 0u64;
        loop {
            let count = reader.read(&mut buffer).map_err(zip_io)?;
            if count == 0 { break; }
            size += count as u64;
            self.uncompressed += count as u64;
            if size > MAX_FILE_BYTES || self.uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES {
                return Err(TransferError::TooLarge(format!("{path}: 展開後サイズが上限を超えています")));
            }
            hash.update(&buffer[..count]);
            self.writer.write_all(&buffer[..count]).map_err(zip_io)?;
        }
        self.manifest.novels[index].files.push(ArchiveFile {
            path, bytes: Vec::new(), sha256: Some(hex::encode(hash.finalize())), size: Some(size),
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
        // Count without allocating a second manifest, then serialize directly
        // into the compressor. Even the directory is written to the sink.
        let mut counter = CountingWriter(0);
        serde_json::to_writer(&mut counter, &self.manifest)?;
        if counter.0 > MAX_MANIFEST_BYTES
            || self.uncompressed.saturating_add(counter.0) > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(TransferError::TooLarge("manifest を含む転送データが上限を超えています".into()));
        }
        self.writer.start_file(MANIFEST_PATH, options())?;
        serde_json::to_writer(&mut self.writer, &self.manifest)?;
        self.writer.finish()?;
        Ok(())
    }
}

pub(super) struct CountingWriter(pub u64);

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("manifest size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
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
