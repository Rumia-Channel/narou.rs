//! `/api/transfer/*` — export and import portable novel archives.
//!
//! `GET  /api/transfer/export?ids=1,2` returns `application/zip`.
//! `POST /api/transfer/import` accepts a raw ZIP body (`Content-Type:
//! application/zip`) and returns a JSON summary.
//!
//! Both endpoints delegate to [`crate::native::transfer`], the same helper the
//! `narou export` / `narou import` CLI uses, so the two front ends cannot drift
//! apart. Authentication is the normal native Web middleware (loopback UI token
//! or Basic auth) — these routes are registered without a bypass.

use axum::Json;
use axum::body::{Body, Bytes};
use futures::StreamExt;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use crate::native::transfer;
use crate::transfer::{
    flag_value, import_payload, is_zip_content_type, parse_ids, ConflictPolicy,
};

use super::AppState;

/// Maximum request body accepted by `POST /api/transfer/import`.
///
/// The archive core enforces the same cap after decode; raising axum's global
/// 2 MiB body limit on this route is what lets a real archive arrive at all.
pub const MAX_TRANSFER_IMPORT_BODY_BYTES: usize = crate::transfer::MAX_ARCHIVE_BYTES as usize;

#[derive(Debug, Deserialize)]
pub struct ExportParams {
    /// Comma-separated library ids (`ids=1,2,3`).
    pub ids: Option<String>,
    pub progress: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ImportParams {
    /// `true` replaces novels that already exist instead of skipping them.
    pub overwrite: Option<String>,
    pub progress: Option<String>,
}

/// `GET /api/transfer/export?ids=1,2` — download a ZIP of the selected novels.
pub async fn export_novels(
    State(_state): State<AppState>,
    Query(params): Query<ExportParams>,
) -> Response {
    let ids = match parse_ids(params.ids.as_deref()) {
        Ok(ids) => ids,
        Err(message) => return export_error(StatusCode::BAD_REQUEST, message),
    };
    if flag_value(params.progress.as_deref()) {
        return progress_response(move |emit, _| transfer::export_archive_stream(&ids, emit));
    }

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(1);
    tokio::task::spawn_blocking(move || {
        struct Sink(tokio::sync::mpsc::Sender<std::io::Result<Bytes>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                for chunk in bytes.chunks(64 * 1024) {
                    self.0.blocking_send(Ok(Bytes::copy_from_slice(chunk)))
                        .map_err(|_| std::io::Error::other("transfer cancelled"))?;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let mut sink = std::io::BufWriter::with_capacity(64 * 1024, Sink(sender.clone()));
        let result = transfer::export_archive_to(&ids, &mut sink)
            .and_then(|_| std::io::Write::flush(&mut sink).map_err(Into::into));
        if let Err(error) = result { let _ = sender.blocking_send(Err(std::io::Error::other(error.to_string()))); }
    });
    // The encoder resolves every requested ID before writing. Preserve the
    // ordinary JSON error response if preflight fails before the first chunk.
    let first = match receiver.recv().await {
        Some(Ok(bytes)) => bytes,
        Some(Err(error)) => return export_error(StatusCode::BAD_REQUEST, error.to_string()),
        None => return export_error(StatusCode::INTERNAL_SERVER_ERROR, "export ended before producing a ZIP".into()),
    };
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(first) })
        .chain(futures::stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|chunk| (chunk, receiver))
        }));

    let filename = format!(
        "narou-export-{}.zip",
        chrono::Local::now().format("%Y%m%d%H%M%S")
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    match HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        Ok(value) => {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        Err(error) => {
            return export_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("invalid Content-Disposition: {error}"),
            );
        }
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (headers, Body::from_stream(stream)).into_response()
}

/// `POST /api/transfer/import[?overwrite=true]` — import a raw ZIP body.
pub async fn import_archive(
    State(_state): State<AppState>,
    Query(params): Query<ImportParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !request_is_zip(&headers) {
        return import_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type は application/zip を指定してください".to_string(),
        );
    }
    // Receive into a private file under OS backpressure. TempPath removes the
    // upload if the request is cancelled, including during validation.
    let temp = match tempfile::NamedTempFile::new() {
        Ok(temp) => temp,
        Err(error) => return import_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let (file, path) = temp.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut body = body.into_data_stream();
    let mut size = 0usize;
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return import_error(StatusCode::BAD_REQUEST, error.to_string()),
        };
        size = match size.checked_add(chunk.len()).filter(|size| *size <= MAX_TRANSFER_IMPORT_BODY_BYTES) {
            Some(size) => size,
            None => return import_error(StatusCode::PAYLOAD_TOO_LARGE, "アーカイブが大きすぎます".into()),
        };
        if let Err(error) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await {
            return import_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    }
    if size == 0 { return import_error(StatusCode::BAD_REQUEST, "空のリクエストです".into()); }
    if let Err(error) = file.sync_all().await {
        return import_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    drop(file);
    let policy = conflict_policy(flag_value(params.overwrite.as_deref()));
    if flag_value(params.progress.as_deref()) {
        return progress_response(move |emit, cancelled| {
            emit(serde_json::json!({"type":"progress","phase":"validate","completed":0,"total":0}))?;
            let mut progress = |completed, total, title: &str| {
                emit(serde_json::json!({"type":"progress","phase":"import",
                    "completed":completed,"total":total,"title":title}))
            };
            let outcome = transfer::import_archive_reader(std::fs::File::open(&path)?, policy, Some(&mut progress), Some(cancelled))?;
            emit(serde_json::json!({"type":"complete","result":import_payload(
                outcome.success(), outcome.created, outcome.overwritten, outcome.skipped, outcome.errors,
            )}))
        });
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    match tokio::task::spawn_blocking(move || transfer::import_archive_reader(std::fs::File::open(&path)?, policy, None, Some(cancelled))).await
    {
        Ok(Ok(outcome)) => Json(import_payload(
            outcome.success(),
            outcome.created,
            outcome.overwritten,
            outcome.skipped,
            outcome.errors,
        ))
        .into_response(),
        Ok(Err(error)) => import_error(StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        Err(error) => import_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("import task failed: {error}"),
        )
        .into_response(),
    }
}

type ProgressEmitter<'a> = dyn FnMut(serde_json::Value) -> crate::error::Result<()> + 'a;

fn progress_response(
    task: impl FnOnce(&mut ProgressEmitter<'_>, Arc<AtomicBool>) -> crate::error::Result<()> + Send + 'static,
) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = CancelOnDrop(cancelled.clone());
    tokio::task::spawn_blocking(move || {
        let mut emit = |value: serde_json::Value| -> crate::error::Result<()> {
            let mut bytes = serde_json::to_vec(&value).map_err(|error| {
                crate::error::NarouError::Platform(format!("転送応答を作成できません: {error}"))
            })?;
            bytes.push(b'\n');
            sender.blocking_send(Bytes::from(bytes)).map_err(|_| {
                crate::error::NarouError::Platform("転送の接続が閉じられました".into())
            })
        };
        if let Err(error) = task(&mut emit, cancelled) {
            let _ = emit(serde_json::json!({"type":"error","message":error.to_string()}));
        }
    });
    let stream = futures::stream::unfold((receiver, guard), |(mut receiver, guard)| async {
        receiver.recv().await.map(|bytes| (Ok::<_, std::convert::Infallible>(bytes), (receiver, guard)))
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store")],
        axum::body::Body::from_stream(stream),
    ).into_response()
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) { self.0.store(true, Ordering::Relaxed); }
}

fn conflict_policy(overwrite: bool) -> ConflictPolicy {
    if overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Skip
    }
}

/// リクエストの `Content-Type` が ZIP として受け付けるメディア型か。
fn request_is_zip(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    is_zip_content_type(value)
}

fn import_error(status: StatusCode, message: String) -> Response {
    (status, Json(import_payload(false, 0, 0, 0, vec![message]))).into_response()
}

fn export_error(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({ "success": false, "message": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// リクエストの `Content-Type` 判定は共有の規則を使う。
    #[test]
    fn transfer_archive_import_requires_a_zip_media_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/zip"),
        );
        assert!(request_is_zip(&headers));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(!request_is_zip(&headers));

        assert!(!request_is_zip(&HeaderMap::new()));
    }
}
