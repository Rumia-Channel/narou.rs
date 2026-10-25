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
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::native::transfer;
use crate::transfer::ConflictPolicy;

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
}

#[derive(Debug, Deserialize)]
pub struct ImportParams {
    /// `true` replaces novels that already exist instead of skipping them.
    pub overwrite: Option<String>,
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

    let exported =
        match tokio::task::spawn_blocking(move || transfer::export_archive_bytes(&ids)).await {
            Ok(Ok(exported)) => exported,
            Ok(Err(error)) => return export_error(StatusCode::BAD_REQUEST, error.to_string()),
            Err(error) => {
                return export_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("export task failed: {error}"),
                );
            }
        };

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
    (headers, exported.bytes).into_response()
}

/// `POST /api/transfer/import[?overwrite=true]` — import a raw ZIP body.
pub async fn import_archive(
    State(_state): State<AppState>,
    Query(params): Query<ImportParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_zip_content_type(&headers) {
        return import_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type は application/zip を指定してください".to_string(),
        );
    }
    if body.len() > MAX_TRANSFER_IMPORT_BODY_BYTES {
        return import_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "アーカイブが大きすぎます ({} bytes、上限 {} bytes)",
                body.len(),
                MAX_TRANSFER_IMPORT_BODY_BYTES
            ),
        );
    }
    if body.is_empty() {
        return import_error(StatusCode::BAD_REQUEST, "空のリクエストです".to_string());
    }

    let policy = conflict_policy(flag_value(params.overwrite.as_deref()));
    let bytes = body.to_vec();
    match tokio::task::spawn_blocking(move || transfer::import_archive_bytes(&bytes, policy)).await
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

fn conflict_policy(overwrite: bool) -> ConflictPolicy {
    if overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Skip
    }
}

/// Parses `ids=1,2,3`. An omitted or empty value is rejected: exporting the
/// whole library by accident is not a useful default for a download endpoint.
fn parse_ids(value: Option<&str>) -> Result<Vec<i64>, String> {
    let Some(value) = value else {
        return Err("ids を指定してください (例: /api/transfer/export?ids=1,2)".to_string());
    };
    let mut ids = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id = part
            .parse::<i64>()
            .map_err(|_| format!("不正な ID です: {part:?}"))?;
        if id < 0 {
            return Err(format!("不正な ID です: {part:?}"));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("ids を指定してください (例: /api/transfer/export?ids=1,2)".to_string());
    }
    Ok(ids)
}

fn flag_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim),
        Some("1" | "true" | "TRUE" | "True" | "yes" | "on")
    )
}

fn is_zip_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let value = value.split(';').next().unwrap_or("").trim();
    value.eq_ignore_ascii_case("application/zip")
        || value.eq_ignore_ascii_case("application/x-zip-compressed")
        || value.eq_ignore_ascii_case("application/octet-stream")
}

fn import_payload(
    success: bool,
    created: usize,
    overwritten: usize,
    skipped: usize,
    errors: Vec<String>,
) -> serde_json::Value {
    serde_json::json!({
        "success": success,
        "created": created,
        "overwritten": overwritten,
        "skipped": skipped,
        "errors": errors,
    })
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

    #[test]
    fn transfer_archive_export_ids_parse_comma_separated() {
        assert_eq!(parse_ids(Some("1,2, 3")).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_ids(Some("2,2")).unwrap(), vec![2]);
        assert!(parse_ids(None).is_err());
        assert!(parse_ids(Some("")).is_err());
        assert!(parse_ids(Some("1,x")).is_err());
        assert!(parse_ids(Some("-1")).is_err());
    }

    #[test]
    fn transfer_archive_overwrite_flag_accepts_true_like_values() {
        assert!(flag_value(Some("true")));
        assert!(flag_value(Some("1")));
        assert!(!flag_value(Some("false")));
        assert!(!flag_value(None));
    }

    #[test]
    fn transfer_archive_import_payload_has_the_documented_shape() {
        let payload = import_payload(true, 1, 2, 3, Vec::new());
        assert_eq!(payload["success"], true);
        assert_eq!(payload["created"], 1);
        assert_eq!(payload["overwritten"], 2);
        assert_eq!(payload["skipped"], 3);
        assert_eq!(payload["errors"], serde_json::json!([]));
    }
}
