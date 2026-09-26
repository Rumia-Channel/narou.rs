//! `POST /api/download` — Web UI のダウンロード要求 (native: `src/web/jobs.rs`
//! `api_download` → `queue_download_jobs`)。
//!
//! JSON parity with the native handler:
//! - body: `{"targets": ["n1234ab" | id | URL | タイトル | 別名, ...], "force"?,
//!   "mail"?}` (`DownloadBody`)。
//! - success: HTTP 200 `{"success": true, "results": [{"target", "job_id",
//!   "status": "queued"}]}` — one entry per input target, in request order.
//! - validation / enqueue failure: HTTP 200 `{"success": false, "message",
//!   "results": []}` — the native handler reports every request-level failure
//!   this way (never a 4xx/5xx for a well-formed request body).
//!
//! Native pushes the raw target string onto `PersistentQueue` and resolves
//! aliases/titles lazily inside the downloader subprocess. The worker ledger
//! must store a canonical `JobTarget` (the ledger row is decoded through
//! `JobTarget::parse`, which only knows id / ncode / URL), so this handler
//! resolves up front instead:
//!   1. `app_state('inv','alias')` — the same inventory row native's
//!      `alias_to_target` reads (`alias.yaml`, Local scope).
//!   2. `JobService::validate_target` — id / ncode / URL normalization
//!      (lowercases ncodes, strips URL fragments; the executor then takes the
//!      same `Downloader::get_target_type` path native takes).
//!   3. Otherwise the downloader would classify it as `TargetType::Other` and
//!      try `find_by_title` → `find_by_ncode`; we do that lookup here and store
//!      the resolved `Id`. A target that survives none of these can never run
//!      on the worker, so the whole request fails atomically — same
//!      all-or-nothing shape as native `validate_download_targets` (the job
//!      that native would have queued would fail with NotFound anyway).
//!
//! Enqueueing goes through `WorkerRuntime::enqueue_plan` — the identical
//! ledger + Queue-binding path `api_jobs` (`worker_entry/src/lib.rs`) uses.
//! The ledger's active dedupe (`kind:target:effective options`) is the same
//! guarantee native `find_active_job_id` gives: a second request for a target
//! that is already queued/running returns the existing job id instead of
//! stacking a duplicate job.

use std::collections::HashMap;

use narou_rs::application::aliases::{
    ALIAS_INVENTORY_KEY, ALIAS_INVENTORY_SCOPE, parse_alias_map, resolve_alias_target,
};
use narou_rs::application::{JobKind, JobPlan, JobTarget};
use narou_rs::platform::NovelId;
use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{console_log, Env, Method, Request, Response};

use crate::composition::WorkerRuntime;

/// native `MAX_WEB_TARGETS_PER_REQUEST` — the fallback when the
/// `server-max-targets-per-request` setting is absent or invalid
/// (`SettingsService::web_target_limit`).
const MAX_WEB_TARGETS_PER_REQUEST: usize = 100_000;
/// native `MAX_WEB_TARGET_LENGTH` (`validate_web_target_value`).
const MAX_WEB_TARGET_LENGTH: usize = 4096;

#[derive(Debug, Deserialize)]
struct DownloadBody {
    targets: Vec<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    mail: bool,
}

/// Entry point; `lib.rs` routes `POST /api/download` here.
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    match req.path().as_str() {
        "/api/download" => {}
        _ => return Response::error("Not Found", 404),
    }
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let body: DownloadBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };

    // `super::max_web_targets_per_request` parity: same setting key, same
    // fallback.
    let max_targets = runtime
        .services
        .settings
        .web_target_limit(MAX_WEB_TARGETS_PER_REQUEST)
        .await;

    // `queue_download_jobs` → `validate_download_targets` parity.
    if let Err(message) = validate_download_targets(&body.targets, max_targets) {
        return download_failure(&message);
    }

    let mut options = Vec::new();
    if body.force {
        options.push("--force".to_string());
    }
    if body.mail {
        options.push("--mail".to_string());
    }

    let aliases = load_aliases(&env).await;
    let mut plans = Vec::with_capacity(body.targets.len());
    for target in &body.targets {
        match resolve_plan_target(&runtime, &aliases, target).await {
            Ok(job_target) => plans.push(JobPlan {
                kind: JobKind::Download,
                target: job_target,
                options: options.clone(),
            }),
            // A target that cannot be planned (or resolved to a known novel)
            // aborts the request exactly like native's validation errors.
            Err(message) => return download_failure(&message),
        }
    }

    // `push_batch` parity: enqueue through the shared ledger + Queue binding;
    // an enqueue failure reports the error string as the message (native does
    // `map_err(|e| e.to_string())` into the same response shape).
    let mut results = Vec::with_capacity(plans.len());
    for (target, plan) in body.targets.iter().zip(plans.iter()) {
        let outcome = match runtime.enqueue_plan(plan.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => return download_failure(&error.to_string()),
        };
        results.push(json!({
            "target": target,
            "job_id": outcome.job_id,
            "status": "queued",
        }));
    }

    Response::from_json(&json!({ "success": true, "results": results }))
}

/// `lib.rs::json_error` と同じ JSON 形 (`{error: {code, message?}}`)。あちらは
/// private なので形だけ合わせてここに持つ (このファイルは HTTP 層のエラーに
/// のみ使い、native が 200 で返す API 失敗には `download_failure` を使う)。
fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    let payload = match message {
        Some(message) => json!({ "error": { "code": code, "message": message } }),
        None => json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}

/// native `api_download` の失敗応答: HTTP 200 + `{success:false, message,
/// results:[]}`。
fn download_failure(message: &str) -> worker::Result<Response> {
    Response::from_json(&json!({
        "success": false,
        "message": message,
        "results": [],
    }))
}

/// native `validate_download_targets` + `validate_web_target_value` parity:
/// reject over the configured cap, empty/whitespace-only, overlong,
/// flag-like (`-` prefix), or control-character targets. The raw string is
/// what native enqueues; validation only gates it.
fn validate_download_targets(targets: &[String], max_targets: usize) -> Result<(), String> {
    if targets.len() > max_targets {
        return Err("too many targets".to_string());
    }
    for target in targets {
        validate_web_target_value(target)?;
    }
    Ok(())
}

/// `src/web/mod.rs::validate_web_target_value`, error strings mapped to the
/// download handler's single message ("invalid download target").
fn validate_web_target_value(value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_WEB_TARGET_LENGTH
        || trimmed.starts_with('-')
        || trimmed.chars().any(|ch| ch.is_control())
    {
        return Err("invalid download target".to_string());
    }
    Ok(())
}

/// `app_state('inv','alias')` の行 (YAML 優先、旧 `value_json` はフォールバック)。
#[derive(Debug, Deserialize)]
struct AliasRow {
    #[serde(default)]
    value_yaml: Option<String>,
    #[serde(default)]
    value_json: Option<String>,
}

/// native `alias_to_target` が読むエイリアス表。読めない/行が無いときは
/// 空表 (= native が load に失敗したときと同じく別名なしとして扱う)。
pub(crate) async fn load_aliases(env: &Env) -> HashMap<String, String> {
    let Ok(db) = env.d1("DB") else {
        return Default::default();
    };
    let statement = match db
        .prepare("SELECT value_yaml, value_json FROM app_state WHERE scope = ? AND key = ?")
        .bind(&[
            JsValue::from_str(ALIAS_INVENTORY_SCOPE),
            JsValue::from_str(ALIAS_INVENTORY_KEY),
        ]) {
        Ok(statement) => statement,
        Err(_) => return Default::default(),
    };
    let row: Option<AliasRow> = statement.first::<AliasRow>(None).await.unwrap_or_default();
    let Some(row) = row else {
        return Default::default();
    };
    // `value_yaml` を正とし、移行前の行 (`value_json` のみ) も読めるようにする
    // (`D1CookieStore::load_payload` と同じ規則)。
    let payload = match row.value_yaml.as_deref() {
        Some(yaml) if !yaml.trim().is_empty() && yaml.trim() != "{}" => yaml.to_string(),
        _ => row.value_json.unwrap_or_else(|| "{}".to_string()),
    };
    parse_alias_map(&payload)
}

/// One raw target → a ledger-safe `JobTarget`, following native's resolution
/// order (`alias → target type → title/ncode record lookup`).
async fn resolve_plan_target(
    runtime: &WorkerRuntime,
    aliases: &HashMap<String, String>,
    raw: &str,
) -> Result<JobTarget, String> {
    // `alias_to_target`: exact-key lookup, unmapped names pass through.
    let effective = resolve_alias_target(aliases, raw);
    let effective = effective.trim();
    if effective.is_empty() {
        return Err("invalid download target".to_string());
    }

    // id / ncode / URL — the planner's normalization produces canonical,
    // round-trippable ledger strings (`JobTarget::parse` must accept what
    // `as_str` writes or the ledger row becomes unreadable).
    if let Ok(target) = runtime.services.jobs.validate_target(effective) {
        if matches!(target, JobTarget::All) {
            return Err("invalid download target".to_string());
        }
        return Ok(target);
    }

    // `TargetType::Other` parity: the downloader would try title first, then
    // ncode, against the library. Resolve to the record id now since the
    // ledger cannot store a title target.
    let library = &runtime.services.library;
    let existing = match library
        .find_by_title(effective)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(record) => Some(record.id),
        None => library
            .find_by_ncode(effective)
            .await
            .map_err(|error| error.to_string())?
            .map(|record| record.id),
    };
    match existing {
        Some(id) => Ok(JobTarget::Id(NovelId(id))),
        None => Err("invalid download target".to_string()),
    }
}
