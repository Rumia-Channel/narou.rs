//! Deployment roles of one Worker artifact.
//!
//! The same Wasm artifact is deployed as several scripts:
//!
//! - `writer` — Web UI, API, cron, the conversion queue and the legacy
//!   acquisition ingress. It never executes an acquisition itself.
//! - `reader` — read-only delivery of public content.
//! - `acquisition` — one acquisition lane. It consumes exactly one
//!   acquisition queue and runs at most one heavy operation per isolate.

use narou_rs::application::acquisition_lanes::LANE_COUNT;
use worker::{Env, Error, Method, Result};

/// Deployment role of this script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerRole {
    /// Web UI, API, cron, conversion queue and the legacy acquisition ingress.
    Writer,
    /// Read-only delivery of public content.
    Reader,
    /// One acquisition lane: consumes exactly one acquisition queue.
    Acquisition,
}

const ROLE_VAR: &str = "NAROU_WORKER_ROLE";
const LANE_VAR: &str = "NAROU_ACQUISITION_LANE";

/// The role of this script. An unset variable means the writer.
pub(crate) fn role(env: &Env) -> Result<WorkerRole> {
    let value = js_sys::Reflect::get(env, &wasm_bindgen::JsValue::from_str(ROLE_VAR))
        .map_err(|_| Error::RustError("failed to read NAROU_WORKER_ROLE".into()))?;
    if value.is_undefined() {
        return Ok(WorkerRole::Writer);
    }
    match value.as_string().as_deref() {
        Some("writer") => Ok(WorkerRole::Writer),
        Some("reader") => Ok(WorkerRole::Reader),
        Some("acquisition") => Ok(WorkerRole::Acquisition),
        _ => Err(Error::RustError(
            "NAROU_WORKER_ROLE must be writer, reader or acquisition".into(),
        )),
    }
}

/// Whether this script is the read-only delivery Worker.
pub(crate) fn is_reader(env: &Env) -> Result<bool> {
    Ok(role(env)? == WorkerRole::Reader)
}

/// The acquisition lane this script owns. `None` for every other role.
///
/// The lane index is part of the deployment identity: a lane script may only
/// execute an assignment that names its own lane.
pub(crate) fn acquisition_lane(env: &Env) -> Result<Option<u8>> {
    if role(env)? != WorkerRole::Acquisition {
        return Ok(None);
    }
    let value = js_sys::Reflect::get(env, &wasm_bindgen::JsValue::from_str(LANE_VAR))
        .map_err(|_| Error::RustError("failed to read NAROU_ACQUISITION_LANE".into()))?;
    let text = value.as_string().ok_or_else(|| {
        Error::RustError("NAROU_ACQUISITION_LANE is required for the acquisition role".into())
    })?;
    let lane: u8 = text.trim().parse().map_err(|_| {
        Error::RustError(format!(
            "NAROU_ACQUISITION_LANE must be 0..{}, got {text:?}",
            LANE_COUNT - 1
        ))
    })?;
    if lane >= LANE_COUNT {
        return Err(Error::RustError(format!(
            "NAROU_ACQUISITION_LANE {lane} is out of range"
        )));
    }
    Ok(Some(lane))
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
