//! Progress responses keep ZIP bodies out of the Worker-wide aggregate buffer.
use std::collections::VecDeque;

use base64::Engine;
use futures::SinkExt;
use narou_rs::transfer::{ArchiveStream, ConflictPolicy, TransferBuffer};
use serde_json::{Value, json};
use worker::{Env, Response};

use super::*;

struct ExportState {
    runtime: WorkerRuntime,
    ids: Vec<i64>,
    next: usize,
    completed: usize,
    current: String,
    active: bool,
    files: VecDeque<(String, ObjectKey)>,
    writer: Option<ArchiveStream<TransferBuffer>>,
    sink: TransferBuffer,
    pending: Vec<u8>,
    offset: usize,
    bytes: u64,
    final_event: Option<Value>,
    finished: bool,
}

impl ExportState {
    async fn next_event(&mut self) -> Result<Option<Value>, String> {
        loop {
            if self.offset < self.pending.len() {
                let end = (self.offset + 64 * 1024).min(self.pending.len());
                let data = base64::engine::general_purpose::STANDARD.encode(&self.pending[self.offset..end]);
                self.offset = end;
                return Ok(Some(json!({"type":"data", "data":data})));
            }
            self.pending.clear(); self.offset = 0;
            if let Some(event) = self.final_event.take() { return Ok(Some(event)); }
            if self.finished { return Ok(None); }
            if let Some((relative, key)) = self.files.pop_front() {
                let objects = self.runtime.write_objects();
                if let Some(bytes) = objects.read_small(&key).await.map_err(|error| format!("{relative}: {error}"))? {
                    let writer = self.writer.as_mut().expect("active ZIP writer");
                    if relative == TOC_FILE { writer.set_subtitle(parse_toc_story(&bytes)); }
                    writer.add_file(&relative, &bytes).map_err(|error| error.to_string())?;
                    self.pending = self.sink.take();
                    self.bytes += self.pending.len() as u64;
                }
                continue;
            }
            if self.active {
                self.active = false;
                self.completed += 1;
                return Ok(Some(json!({"type":"progress", "phase":"export", "completed":self.completed,
                    "total":self.ids.len(), "title":self.current})));
            }
            if let Some(id) = self.ids.get(self.next).copied() {
                let record = self.runtime.novels.get(NovelId(id)).await
                    .map_err(|error| format!("ID: {id}: {error}"))?
                    .ok_or_else(|| format!("ID: {id} が見つかりません"))?;
                let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)
                    .map_err(|error| error.to_string())?;
                let objects = self.runtime.write_objects();
                let prefix = keys.prefix();
                let mut listed = BTreeMap::new();
                list_prefix(objects.as_ref(), prefix.as_ref(), &mut listed).await?;
                list_prefix(objects.as_ref(), &format!("{}/挿絵", prefix.as_ref()), &mut listed).await?;
                let root = format!("{}/", prefix.as_ref());
                for (key, size) in listed {
                    let Some(relative) = key.strip_prefix(&root) else { continue; };
                    if is_excluded_top_level(relative) || is_excluded_file_name(relative)
                        || !is_archivable_relative_path(relative) { continue; }
                    if size > MAX_OBJECT_BYTES {
                        return Err(format!("{}: {relative} は Worker のファイル上限 ({MAX_OBJECT_BYTES} bytes) を超えています", record.title));
                    }
                    self.files.push_back((relative.to_string(), ObjectKey::try_new(key).map_err(|error| error.to_string())?));
                }
                self.writer.as_mut().expect("active ZIP writer")
                    .begin_novel(archive_novel_from_record(&record, Vec::new(), None))
                    .map_err(|error| error.to_string())?;
                self.current = record.title;
                self.next += 1; self.active = true;
                return Ok(Some(json!({"type":"progress", "phase":"export", "completed":self.completed,
                    "total":self.ids.len(), "title":self.current})));
            }
            self.writer.take().expect("active ZIP writer").finish().map_err(|error| error.to_string())?;
            self.pending = self.sink.take();
            self.bytes += self.pending.len() as u64;
            self.finished = true;
            self.final_event = Some(json!({"type":"complete", "filename":"narou-export.zip", "bytes":self.bytes}));
        }
    }
}

fn event_bytes(event: Value) -> worker::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&event).map_err(|error| worker::Error::RustError(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn response<S>(stream: S) -> worker::Result<Response>
where S: futures::Stream<Item = worker::Result<Vec<u8>>> + 'static {
    worker::ResponseBuilder::new().with_header("Content-Type", "application/x-ndjson; charset=utf-8")?
        .with_header("Cache-Control", "no-store")?.from_stream(stream)
}

pub(super) async fn export(ids: Vec<i64>, env: Env) -> worker::Result<Response> {
    let runtime = WorkerRuntime::build_ui(&env).await
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    let sink = TransferBuffer::default();
    let writer = ArchiveStream::new(sink.clone(), env!("CARGO_PKG_VERSION").into(),
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    let state = ExportState { runtime, ids, next: 0, completed: 0, current: String::new(), active: false,
        files: VecDeque::new(), writer: Some(writer), sink, pending: Vec::new(), offset: 0,
        bytes: 0, final_event: None, finished: false };
    response(futures::stream::unfold(state, |mut state| async move {
        let event = match state.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => return None,
            Err(message) => {
                state.finished = true;
                state.writer = None;
                json!({"type":"error", "message":message})
            }
        };
        Some((event_bytes(event), state))
    }))
}

pub(super) type EventSender = futures::channel::mpsc::Sender<worker::Result<Vec<u8>>>;

pub(super) async fn send_event(sender: &mut EventSender, event: Value) -> worker::Result<()> {
    sender.send(event_bytes(event)).await.map_err(|_| worker::Error::RustError("転送の接続が閉じられました".into()))
}

pub(super) fn import(body: Vec<u8>, policy: ConflictPolicy, env: Env) -> worker::Result<Response> {
    let (mut sender, receiver) = futures::channel::mpsc::channel(4);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = send_event(&mut sender, json!({"type":"progress","phase":"validate","completed":0,"total":0})).await;
        let result = super::import_body(body, policy, env, Some(&mut sender)).await;
        let event = match result {
            Ok(mut response) => match response.json::<Value>().await {
                Ok(result) => json!({"type":"complete","result":result}),
                Err(error) => json!({"type":"error","message":error.to_string()}),
            },
            Err(error) => json!({"type":"error","message":error.to_string()}),
        };
        let _ = send_event(&mut sender, event).await;
    });
    response(receiver)
}
