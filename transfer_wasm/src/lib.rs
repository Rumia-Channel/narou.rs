//! Streaming Dedicated Worker bindings for the portable transfer codec.
//! No ZIP/file body is materialized as a whole JavaScript value.
use std::{cell::RefCell, collections::HashMap, io::{self, SeekFrom}, pin::Pin, task::{Context, Poll}};
use futures::{FutureExt, future::LocalBoxFuture, io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite}};
use js_sys::{Function, Promise, Uint8Array};
use narou_rs::transfer::{ArchiveLimits, AsyncArchiveStream, ValidatedArchive};
use narou_rs::transfer::projection::{TransferProjection, TransferSelection};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

fn js_error(error: impl std::fmt::Display) -> JsValue { JsValue::from_str(&error.to_string()) }
fn io_error(error: JsValue) -> io::Error { io::Error::other(format!("{error:?}")) }

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
fn decimal(value: &str) -> Result<u64, JsValue> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(js_error("invalid decimal byte offset"));
    }
    value.parse().map_err(js_error)
}

struct ArchiveHandle {
    archive: ValidatedArchive<RangeReader>,
    projection: Option<TransferProjection>,
}
#[derive(Default)]
struct Handles { next: u32, values: HashMap<u32, Option<ArchiveHandle>> }
thread_local! { static HANDLES: RefCell<Handles> = RefCell::new(Handles::default()); }

// Take an exclusive operation lease without holding RefCell borrows across
// await. A failed/cancelled callback always returns the handle to its registry.
struct ActiveHandle { id: u32, value: Option<ArchiveHandle> }
impl ActiveHandle {
    fn take(id: u32) -> Result<Self, JsValue> {
        HANDLES.with(|handles| {
            let mut handles = handles.borrow_mut();
            let slot = handles.values.get_mut(&id).ok_or_else(|| js_error("unknown archive handle"))?;
            let value = slot.take().ok_or_else(|| js_error("archive handle is busy"))?;
            Ok(Self { id, value: Some(value) })
        })
    }
    fn value(&mut self) -> &mut ArchiveHandle { self.value.as_mut().expect("active handle") }
}
impl Drop for ActiveHandle {
    fn drop(&mut self) {
        HANDLES.with(|handles| {
            if let Some(slot) = handles.borrow_mut().values.get_mut(&self.id) { *slot = self.value.take(); }
        });
    }
}

/// Actual precompiled binding identity, independently compared with assets.
#[wasm_bindgen]
pub fn codec_info() -> String {
    serde_json::json!({
        "protocol_version": narou_rs::transfer::protocol::PROTOCOL_VERSION,
        "abi_version": narou_rs::transfer::protocol::ABI_VERSION,
        "build_id": option_env!("NAROU_TRANSFER_BUILD_ID").unwrap_or(env!("CARGO_PKG_VERSION")),
    }).to_string()
}

/// Index an immutable source. `size` is decimal, range offsets are checked
/// safe JS numbers and every requested/output range is at most 64 KiB.
#[wasm_bindgen]
pub async fn open_archive(read_range: Function, size: String, limits: Option<String>) -> Result<u32, JsValue> {
    let size = decimal(&size)?;
    if size == 0 || size > MAX_SAFE_INTEGER { return Err(js_error("archive size is not a safe File slice offset")); }
    let limits = ArchiveLimits::from_json(limits.as_deref()).map_err(js_error)?;
    let archive = ValidatedArchive::open(move || Ok(RangeReader::new(read_range.clone(), size, None)), size, limits)
        .await.map_err(js_error)?;
    HANDLES.with(|handles| {
        let mut handles = handles.borrow_mut();
        let id = handles.next.checked_add(1).ok_or_else(|| js_error("archive handle ids exhausted"))?;
        handles.next = id;
        handles.values.insert(id, Some(ArchiveHandle { archive, projection: None }));
        Ok(id)
    })
}

/// Validate the entire source before its manifest or any entry can stream.
/// Progress is synchronous `(completed, total)`; throwing cancels validation.
#[wasm_bindgen]
pub async fn validate_archive(handle: u32, progress: Function) -> Result<String, JsValue> {
    let mut active = ActiveHandle::take(handle)?;
    let value = active.value();
    value.archive.validate(|done, total| {
        progress.call2(&JsValue::NULL, &JsValue::from_f64(done as f64), &JsValue::from_f64(total as f64))
            .map_err(io_error)?;
        Ok(())
    }).await.map_err(js_error)?;
    Ok(serde_json::json!({
        "fingerprint": value.archive.fingerprint().expect("validated"),
        "files": value.archive.matches().len(),
        "novels": value.archive.manifest().novels.len(),
        "total_uncompressed": value.archive.total_uncompressed().to_string(),
    }).to_string())
}

/// Display-only page, never a second full manifest/metadata tree.
#[wasm_bindgen]
pub fn catalog_page(handle: u32, cursor: Option<String>) -> Result<String, JsValue> {
    let mut active = ActiveHandle::take(handle)?;
    let (items, next) = active.value().archive.catalog_page(cursor.as_deref(), &Default::default()).map_err(js_error)?;
    serde_json::to_string(&serde_json::json!({
        "items": items, "next_cursor": next.map(|value| value.to_string()),
    })).map_err(js_error)
}

/// Write projected NDJSON, then retain the dense wire-file mapping in the
/// handle only after every awaited output callback succeeds.
#[wasm_bindgen]
pub async fn write_transfer_manifest(handle: u32, selection: String, write_chunk: Function) -> Result<String, JsValue> {
    let selection = TransferSelection::from_json(&selection).map_err(js_error)?;
    let mut active = ActiveHandle::take(handle)?;
    let value = active.value();
    value.projection = None;
    let projection = value.archive.project(&selection).map_err(js_error)?;
    let mut sink = CallbackWriter { callback: write_chunk, pending: None };
    let summary = value.archive.write_wire_manifest(&selection, &projection, &mut sink).await.map_err(js_error)?;
    value.projection = Some(projection);
    serde_json::to_string(&summary).map_err(js_error)
}

/// `entry_id` is the projected file_id, not a catalog archive-entry ordinal.
/// Empty files invoke no byte callback; the caller emits their empty final
/// upload record. Confirmed prefix bytes are hashed/discarded, never emitted.
#[wasm_bindgen]
pub async fn stream_entry(handle: u32, entry_id: u32, confirmed_offset: String,
    expected_prefix_sha256: Option<String>, write_chunk: Function) -> Result<String, JsValue> {
    let offset = decimal(&confirmed_offset)?;
    let mut active = ActiveHandle::take(handle)?;
    let value = active.value();
    let projection = value.projection.as_ref().ok_or_else(|| js_error("write_transfer_manifest must succeed before streaming entries"))?;
    let mut sink = CallbackWriter { callback: write_chunk, pending: None };
    let summary = value.archive.stream_entry(entry_id, &projection.wire_files, offset,
        expected_prefix_sha256.as_deref(), &mut sink).await.map_err(js_error)?;
    serde_json::to_string(&summary).map_err(js_error)
}

#[wasm_bindgen]
pub fn close_archive(handle: u32) -> Result<(), JsValue> {
    HANDLES.with(|handles| {
        let mut handles = handles.borrow_mut();
        match handles.values.get(&handle) {
            Some(Some(_)) => { handles.values.remove(&handle); Ok(()) }
            Some(None) => Err(js_error("archive handle is busy")),
            None => Err(js_error("unknown archive handle")),
        }
    })
}

/// Stream a snapshot to ZIP64. read_chunk receives
/// `(novel_index, path, offset, length, global_file_id)`; expected size/SHA are
/// mandatory in the metadata and any changed bytes fail rather than rewrite
/// the expected manifest. Callback output is owned, never a Wasm memory view.
#[wasm_bindgen]
pub async fn build_zip(read_manifest: Function, manifest_size: String, read_chunk: Function, write_chunk: Function) -> Result<(), JsValue> {
    let size = decimal(&manifest_size)?;
    if size == 0 || size > narou_rs::transfer::MAX_MANIFEST_BYTES { return Err(js_error("invalid manifest size")); }
    let manifest = narou_rs::transfer::read_manifest(RangeReader::new(read_manifest, size, None)).await.map_err(js_error)?;
    let sink = CallbackWriter { callback: write_chunk, pending: None };
    let mut file_id = 0u32;
    AsyncArchiveStream::write_manifest(sink, manifest, |index, path, size| {
        if size > MAX_SAFE_INTEGER {
            return Err(narou_rs::transfer::TransferError::TooLarge("file size is not a safe callback offset".into()));
        }
        let id = file_id;
        file_id = file_id.checked_add(1).expect("entry count bounded by codec");
        Ok(RangeReader::new(read_chunk.clone(), size, Some((index as u32, path.to_owned(), id))))
    }).await.map_err(js_error)
}

struct CallbackWriter { callback: Function, pending: Option<LocalBoxFuture<'static, io::Result<usize>>> }
impl AsyncWrite for CallbackWriter {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if bytes.is_empty() { return Poll::Ready(Ok(0)); }
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
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(pending) = self.pending.as_mut() {
            match pending.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => { self.pending = None; result?; }
            }
        }
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { self.poll_flush(cx) }
}

struct RangeReader {
    callback: Function, size: u64, position: u64, file: Option<(u32, String, u32)>,
    bytes: Vec<u8>, offset: usize, pending: Option<LocalBoxFuture<'static, io::Result<Uint8Array>>>,
}
impl RangeReader {
    fn new(callback: Function, size: u64, file: Option<(u32, String, u32)>) -> Self {
        Self { callback, size, position: 0, file, bytes: Vec::new(), offset: 0, pending: None }
    }
    fn fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.offset < self.bytes.len() || self.position == self.size { return Poll::Ready(Ok(())); }
        if self.pending.is_none() {
            let count = (self.size - self.position).min(64 * 1024) as u32;
            let offset = JsValue::from_f64(self.position as f64);
            let length = JsValue::from(count);
            let value = match &self.file {
                Some((index, path, file_id)) => self.callback.call5(&JsValue::NULL, &JsValue::from(*index), &JsValue::from_str(path), &offset, &length, &JsValue::from(*file_id)),
                None => self.callback.call2(&JsValue::NULL, &offset, &length),
            }.map_err(io_error)?;
            self.pending = Some(async move {
                let value = JsFuture::from(Promise::resolve(&value)).await.map_err(io_error)?;
                let bytes = value.dyn_into::<Uint8Array>().map_err(|_| io::Error::other("range callback must return Uint8Array"))?;
                if bytes.length() != count { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "range callback returned incorrect length")); }
                Ok(bytes)
            }.boxed_local());
        }
        match self.pending.as_mut().expect("pending range").as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                let bytes = result?;
                self.bytes.resize(bytes.length() as usize, 0);
                bytes.copy_to(&mut self.bytes);
                self.offset = 0;
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
