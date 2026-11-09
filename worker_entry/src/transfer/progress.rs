//! ZIP and NDJSON responses share a bounded pull-driven producer. Dropping the
//! response drops the producer future, input body, staging guards and permit.
use std::{cell::Cell, future::Future, pin::Pin, rc::Rc, task::{Context, Poll}};

use base64::Engine;
use futures::{Sink, SinkExt, Stream, io::AsyncWrite};
use narou_rs::transfer::AsyncArchiveStream;
use serde_json::{Value, json};

use super::*;

pub(super) type EventSender = futures::channel::mpsc::Sender<worker::Result<Vec<u8>>>;

type Producer = Pin<Box<dyn Future<Output = worker::Result<()>>>>;
struct PullResponse {
    producer: Option<Producer>,
    receiver: futures::channel::mpsc::Receiver<worker::Result<Vec<u8>>>,
    permit: Option<crate::memory_budget::MemoryPermit>,
}
impl Stream for PullResponse {
    type Item = worker::Result<Vec<u8>>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Poll::Ready(Some(chunk)) = Pin::new(&mut this.receiver).poll_next(cx) {
            return Poll::Ready(Some(chunk));
        }
        if let Some(producer) = this.producer.as_mut() {
            match producer.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    this.producer = None;
                    if let Err(error) = result {
                        this.permit = None;
                        return Poll::Ready(Some(Err(error)));
                    }
                }
                Poll::Pending => {}
            }
        }
        match Pin::new(&mut this.receiver).poll_next(cx) {
            Poll::Ready(None) if this.producer.is_none() => {
                this.permit = None;
                Poll::Ready(None)
            }
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(chunk)),
            _ => Poll::Pending,
        }
    }
}

struct DataWriter { sender: EventSender, events: bool, bytes: Rc<Cell<u64>> }
impl AsyncWrite for DataWriter {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.sender).poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_)) => return Poll::Ready(Err(std::io::Error::other("transfer cancelled"))),
            Poll::Ready(Ok(())) => {}
        }
        let count = bytes.len().min(64 * 1024);
        let chunk = if self.events {
            event_bytes(json!({"type":"data","data":base64::engine::general_purpose::STANDARD.encode(&bytes[..count])}))
                .map_err(|error| std::io::Error::other(error.to_string()))?
        } else { bytes[..count].to_vec() };
        Pin::new(&mut self.sender).start_send(Ok(chunk))
            .map_err(|_| std::io::Error::other("transfer cancelled"))?;
        self.bytes.set(self.bytes.get() + count as u64);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.sender).poll_flush(cx).map_err(|_| std::io::Error::other("transfer cancelled"))
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> { self.poll_flush(cx) }
}

fn event_bytes(event: Value) -> worker::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&event).map_err(|error| worker::Error::RustError(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) async fn send_event(sender: &mut EventSender, event: Value) -> worker::Result<()> {
    sender.send(event_bytes(event)).await.map_err(|_| worker::Error::RustError("転送の接続が閉じられました".into()))
}

pub(super) fn export(
    assets: Arc<dyn AssetStore>, novels: Vec<(NovelRecord, Vec<(String, ObjectKey, u64)>)>,
    permit: crate::memory_budget::MemoryPermit, events: bool,
) -> worker::Result<Response> {
    let exported_at = Utc::now();
    let filename = format!("narou-export-{}.zip", exported_at.format("%Y%m%d%H%M%S"));
    let disposition = format!("attachment; filename=\"{filename}\"");
    let (mut sender, receiver) = futures::channel::mpsc::channel(0);
    let bytes = Rc::new(Cell::new(0));
    let sink = futures::io::BufWriter::with_capacity(64 * 1024,
        DataWriter { sender: sender.clone(), events, bytes: bytes.clone() });
    let producer: Producer = Box::pin(async move {
        let result = async {
            let mut writer = AsyncArchiveStream::new(sink, env!("CARGO_PKG_VERSION").into(),
                exported_at.to_rfc3339_opts(SecondsFormat::Secs, true)).map_err(worker_error)?;
            let total = novels.len();
            for (completed, (record, files)) in novels.into_iter().enumerate() {
                let mut toc = match files.iter().find(|(path, _, _)| path == TOC_FILE) {
                    Some((_, key, size)) => {
                        if *size > MAX_TOC_METADATA_BYTES {
                            return Err(worker_error("toc.yaml exceeds the 16 MiB metadata limit"));
                        }
                        let metadata = assets.read_stream(key).await.map_err(worker_error)?
                            .ok_or_else(|| worker_error(format!("{key} がありません")))?;
                        let mut metadata = metadata;
                        let mut toc = Vec::new();
                        while let Some(chunk) = metadata.next().await {
                            let chunk = chunk.map_err(worker_error)?;
                            if toc.len() as u64 + chunk.len() as u64 > MAX_TOC_METADATA_BYTES {
                                return Err(worker_error("toc.yaml exceeds the 16 MiB metadata limit"));
                            }
                            toc.extend_from_slice(&chunk);
                        }
                        Some(toc)
                    }
                    None => None,
                };
                let subtitle = toc.as_deref().and_then(parse_toc_story);
                writer.begin_novel(archive_novel_from_record(&record, Vec::new(), subtitle)).map_err(worker_error)?;
                if events { send_event(&mut sender, json!({"type":"progress","phase":"export",
                    "completed":completed,"total":total,"title":record.title})).await?; }
                for (relative, key, _) in files {
                    if relative == TOC_FILE {
                        let mut reader = futures::io::Cursor::new(toc.take().expect("listed TOC was read"));
                        writer.add_reader(&relative, &mut reader).await.map_err(worker_error)?;
                        continue;
                    }
                    let stream = assets.read_stream(&key).await.map_err(worker_error)?
                        .ok_or_else(|| worker_error(format!("{key} がありません")))?;
                    use futures::TryStreamExt;
                    let mut reader = stream.map_err(|error| std::io::Error::other(error.to_string())).into_async_read();
                    writer.add_reader(&relative, &mut reader).await.map_err(worker_error)?;
                }
                if events { send_event(&mut sender, json!({"type":"progress","phase":"export",
                    "completed":completed + 1,"total":total,"title":record.title})).await?; }
            }
            writer.finish().await.map_err(worker_error)?;
            if events { send_event(&mut sender, json!({"type":"complete","filename":filename,"bytes":bytes.get()})).await?; }
            Ok(())
        }.await;
        match result {
            Err(error) if events => send_event(&mut sender, json!({"type":"error","message":error.to_string()})).await,
            result => result,
        }
    });
    let stream = PullResponse { producer: Some(producer), receiver, permit: Some(permit) };
    let builder = worker::ResponseBuilder::new().with_header("Cache-Control", "no-store")?;
    if events {
        builder.with_header("Content-Type", "application/x-ndjson; charset=utf-8")?.from_stream(stream)
    } else {
        builder.with_header("Content-Type", "application/zip")?
            .with_header("Content-Disposition", &disposition)?.from_stream(stream)
    }
}

pub(super) fn import(
    body: AssetStream, policy: ConflictPolicy, runtime: WorkerRuntime,
    permit: crate::memory_budget::MemoryPermit,
) -> worker::Result<Response> {
    let (mut sender, receiver) = futures::channel::mpsc::channel(0);
    let producer: Producer = Box::pin(async move {
        send_event(&mut sender, json!({"type":"progress","phase":"validate","completed":0,"total":0})).await?;
        let result = super::import_body(body, policy, runtime, Some(&mut sender)).await;
        let event = match result {
            Ok(mut response) => match response.json::<Value>().await {
                Ok(result) => json!({"type":"complete","result":result}),
                Err(error) => json!({"type":"error","message":error.to_string()}),
            },
            Err(error) => json!({"type":"error","message":error.to_string()}),
        };
        send_event(&mut sender, event).await
    });
    worker::ResponseBuilder::new().with_header("Content-Type", "application/x-ndjson; charset=utf-8")?
        .with_header("Cache-Control", "no-store")?.from_stream(PullResponse { producer: Some(producer), receiver, permit: Some(permit) })
}

fn worker_error(error: impl std::fmt::Display) -> worker::Error { worker::Error::RustError(error.to_string()) }

pub(super) fn export_error(events: bool, status: u16, message: String) -> worker::Result<Response> {
    if !events { return super::export_error(status, message); }
    let bytes = event_bytes(json!({"type":"error","message":message}))?;
    worker::ResponseBuilder::new().with_header("Content-Type", "application/x-ndjson; charset=utf-8")?
        .with_header("Cache-Control", "no-store")?
        .from_stream(futures::stream::once(async move { Ok::<_, worker::Error>(bytes) }))
}
