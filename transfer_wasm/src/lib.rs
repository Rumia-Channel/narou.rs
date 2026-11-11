//! Streaming browser bindings. Callbacks must read immutable Blob slices or
//! equivalent bounded ranges and await writes to a real streaming destination.
//! No archive or binary file crosses this boundary as one ArrayBuffer/base64.
use std::{io::{self, SeekFrom}, pin::Pin, task::{Context, Poll}};
use std::io::Write as _;
use futures::{FutureExt, future::LocalBoxFuture, io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite, AsyncWriteExt, BufWriter}};
use js_sys::{Function, Promise, Uint8Array};
use narou_rs::transfer::{ArchiveManifest, AsyncArchiveStream, ConflictPolicy, ImportAction, validate_archive, write_manifest_json};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

fn js_error(error: impl std::fmt::Display) -> JsValue { JsValue::from_str(&error.to_string()) }
fn io_error(error: JsValue) -> io::Error { io::Error::other(format!("{error:?}")) }

/// `read_manifest(offset, length) -> Promise<Uint8Array>` streams the immutable
/// UTF-8 manifest (at most 64 KiB per call); `manifest_size` is its byte length.
/// `read_chunk(novelIndex, path, offset, length) -> Promise<Uint8Array>` streams
/// each file, whose size is required in that metadata-only manifest.
/// `write_chunk(Uint8Array) -> Promise<void>` resolves only after accepting the
/// chunk. Metadata is decoded/admitted once and adopted by the ZIP writer.
#[wasm_bindgen]
pub async fn build_zip(read_manifest: Function, manifest_size: f64, read_chunk: Function, write_chunk: Function) -> Result<(), JsValue> {
    if !manifest_size.is_finite() || manifest_size < 0.0 || manifest_size.fract() != 0.0
        || manifest_size > narou_rs::transfer::MAX_MANIFEST_BYTES as f64 {
        return Err(js_error("invalid manifest size"));
    }
    let manifest = narou_rs::transfer::read_manifest(
        RangeReader::new(read_manifest, manifest_size as u64, None)).await.map_err(js_error)?;
    let sink = CallbackWriter { callback: write_chunk, pending: None };
    AsyncArchiveStream::write_manifest(sink, manifest, |index, path, size| {
        Ok(RangeReader::new(read_chunk.clone(), size, Some((index as u32, path.to_owned()))))
    }).await.map_err(js_error)
}

/// Validate the complete archive before emitting its metadata-only manifest.
/// Both range reads and `write_chunk(Uint8Array) -> Promise<void>` are bounded;
/// no duplicated summary tree or whole serialized JSON crosses into JavaScript.
#[wasm_bindgen]
pub async fn parse_zip(read_range: Function, size: f64, write_chunk: Function) -> Result<(), JsValue> {
    let manifest = parse(read_range, size).await?;
    let mut sink = BufWriter::with_capacity(64 * 1024, CallbackWriter { callback: write_chunk, pending: None });
    write_manifest_json(&mut sink, &manifest).await.map_err(js_error)?;
    sink.flush().await.map_err(js_error)
}

/// Validate first, then emit JSONL `{index, action}` decisions.
/// `has_novel(index, toc_url) -> Promise<boolean>` performs one exact URL lookup;
/// the caller need not serialize or copy its entire library into Wasm.
#[wasm_bindgen]
pub async fn plan_zip(read_range: Function, size: f64, has_novel: Function, overwrite: bool, write_chunk: Function) -> Result<(), JsValue> {
    let manifest = parse(read_range, size).await?;
    let conflict = if overwrite { ConflictPolicy::Overwrite } else { ConflictPolicy::Skip };
    let mut sink = BufWriter::with_capacity(64 * 1024, CallbackWriter { callback: write_chunk, pending: None });
    for (index, novel) in manifest.novels.iter().enumerate().filter(|(_, novel)| novel.virtual_members.is_none())
        .chain(manifest.novels.iter().enumerate().filter(|(_, novel)| novel.virtual_members.is_some()))
    {
        let value = has_novel.call2(&JsValue::NULL, &JsValue::from_f64(index as f64), &JsValue::from_str(&novel.toc_url))?;
        let exists = JsFuture::from(Promise::resolve(&value)).await?
            .as_bool().ok_or_else(|| js_error("has_novel must return a boolean"))?;
        let action = match conflict.action(exists) {
            ImportAction::Create => "Create",
            ImportAction::Overwrite => "Overwrite",
            ImportAction::Skip => "Skip",
        };
        let mut bytes = [0u8; 64];
        let mut cursor = io::Cursor::new(&mut bytes[..]);
        writeln!(cursor, "{{\"index\":{index},\"action\":\"{action}\"}}").map_err(js_error)?;
        let length = cursor.position() as usize;
        sink.write_all(&bytes[..length]).await.map_err(js_error)?;
    }
    sink.flush().await.map_err(js_error)
}

async fn parse(callback: Function, size: f64) -> Result<ArchiveManifest, JsValue> {
    if !size.is_finite() || size < 0.0 || size.fract() != 0.0 || size > narou_rs::transfer::MAX_ARCHIVE_BYTES as f64 {
        return Err(js_error("invalid archive size"));
    }
    validate_archive(RangeReader::new(callback, size as u64, None), size as u64).await.map_err(js_error)
}

struct CallbackWriter { callback: Function, pending: Option<LocalBoxFuture<'static, io::Result<usize>>> }
impl AsyncWrite for CallbackWriter {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if self.pending.is_none() {
            let count = bytes.len().min(64 * 1024);
            let value = Uint8Array::from(&bytes[..count]);
            let result = self.callback.call1(&JsValue::NULL, &value).map_err(io_error)?;
            self.pending = Some(async move {
                JsFuture::from(Promise::resolve(&result)).await.map_err(io_error)?;
                Ok(count)
            }.boxed_local());
        }
        let result = self.pending.as_mut().expect("pending write").as_mut().poll(cx);
        if result.is_ready() { self.pending = None; }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
}

struct RangeReader {
    callback: Function, size: u64, position: u64, file: Option<(u32, String)>,
    bytes: Vec<u8>, offset: usize, pending: Option<LocalBoxFuture<'static, io::Result<Vec<u8>>>>,
}
impl RangeReader {
    fn new(callback: Function, size: u64, file: Option<(u32, String)>) -> Self {
        Self { callback, size, position: 0, file, bytes: Vec::new(), offset: 0, pending: None }
    }
    fn fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.offset < self.bytes.len() || self.position == self.size { return Poll::Ready(Ok(())); }
        if self.pending.is_none() {
            let count = (self.size - self.position).min(64 * 1024) as u32;
            let offset = JsValue::from_f64(self.position as f64);
            let length = JsValue::from(count);
            let value = match &self.file {
                Some((index, path)) => self.callback.call4(&JsValue::NULL, &JsValue::from(*index), &JsValue::from_str(path), &offset, &length),
                None => self.callback.call2(&JsValue::NULL, &offset, &length),
            }.map_err(io_error)?;
            self.pending = Some(async move {
                let value = JsFuture::from(Promise::resolve(&value)).await.map_err(io_error)?;
                let bytes = value.dyn_into::<Uint8Array>().map_err(|_| io::Error::other("range callback must return Uint8Array"))?;
                if bytes.length() != count { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "range callback returned incorrect length")); }
                Ok(bytes.to_vec())
            }.boxed_local());
        }
        match self.pending.as_mut().expect("pending range").as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None; self.bytes = result?; self.offset = 0;
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl AsyncRead for RangeReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, target: &mut [u8]) -> Poll<io::Result<usize>> {
        if target.is_empty() { return Poll::Ready(Ok(0)); }
        match self.fill(cx) { Poll::Pending => return Poll::Pending, Poll::Ready(result) => result? }
        let count = target.len().min(self.bytes.len() - self.offset);
        target[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
        self.offset += count; self.position += count as u64;
        Poll::Ready(Ok(count))
    }
}
impl AsyncBufRead for RangeReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        match this.fill(cx) { Poll::Pending => Poll::Pending, Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(&this.bytes[this.offset..])) }
    }
    fn consume(mut self: Pin<&mut Self>, count: usize) { self.offset += count; self.position += count as u64; }
}
impl AsyncSeek for RangeReader {
    fn poll_seek(mut self: Pin<&mut Self>, _: &mut Context<'_>, from: SeekFrom) -> Poll<io::Result<u64>> {
        let position = match from { SeekFrom::Start(value) => value as i128,
            SeekFrom::Current(value) => self.position as i128 + value as i128,
            SeekFrom::End(value) => self.size as i128 + value as i128 };
        if position < 0 || position > self.size as i128 { return Poll::Ready(Err(io::Error::other("invalid seek"))); }
        self.position = position as u64; self.bytes.clear(); self.offset = 0; self.pending = None;
        Poll::Ready(Ok(self.position))
    }
}
