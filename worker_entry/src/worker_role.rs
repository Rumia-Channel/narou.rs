//! Delivery runs in a separate script; captured management routes stay on the writer.

use worker::{Env, Error, Method, Result};

pub(crate) fn is_reader(env: &Env) -> Result<bool> {
    let value = js_sys::Reflect::get(env, &wasm_bindgen::JsValue::from_str("NAROU_WORKER_ROLE"))
        .map_err(|_| Error::RustError("failed to read NAROU_WORKER_ROLE".into()))?;
    if value.is_undefined() {
        return Ok(false);
    }
    match value.as_string().as_deref() {
        Some("writer") => Ok(false),
        Some("reader") => Ok(true),
        _ => Err(Error::RustError("NAROU_WORKER_ROLE must be writer or reader".into())),
    }
}

/// Only these routes may execute locally in a delivery isolate. Cloudflare's
/// prefix routes also capture edits; forward those without reading their body.
pub(crate) fn owns_request(path: &str, method: Method) -> bool {
    if path == "/api/v1" || path.starts_with("/api/v1/")
        || narou_rs::application::read_api::is_read_path(path)
    {
        // The existing API handlers reject unsupported methods; MCP uses POST.
        return true;
    }
    if !matches!(method, Method::Get | Method::Head) {
        return false;
    }
    if matches!(path, "/health/live" | "/health/ready" | "/api/novels"
        | "/api/novels/count" | "/api/novels/all_ids")
    {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/api/novels/") {
        let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
        return id.parse::<i64>().is_ok()
            && (matches!(tail, "" | "download.epub" | "author_comments")
                || tail.starts_with("illustrations/"));
    }
    if let Some(id) = path.strip_prefix("/api/virtual_novels/")
        .and_then(|rest| rest.strip_suffix("/download.epub"))
    {
        return id.parse::<i64>().is_ok();
    }
    path.strip_prefix("/novels/")
        .and_then(|rest| rest.strip_suffix("/download"))
        .is_some_and(|id| id.parse::<i64>().is_ok())
}
