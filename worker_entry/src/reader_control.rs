//! Reader admission bridge between the delivery Worker and the writer.
//!
//! The delivery Worker must never write the shared source D1, but the
//! per-novel access fence (`novel_access_gates` / `novel_reader_leases`)
//! lives there: admit/renew/release are durable mutations. This module is
//! the control path both sides share:
//!
//! * [`handle`] runs on the writer. It is the *only* place the delivery
//!   Worker's lease rows are mutated, behind the same admin authentication
//!   as every other control route, and it delegates to the real
//!   `D1ObjectStore` fence — the same CAS authority the writer itself uses,
//!   not a second gate.
//! * [`RemoteNovelAccess`] is the reader-side `NovelAccessFence`. It
//!   exposes only the reader primitives (admit/renew/release plus
//!   `gate_info` for admission diagnostics) and forwards them over the
//!   `NAROU_WRITER` service binding with the shared admin credential.
//!   Writer-side primitives (capture/close/open/commit/remove) stay
//!   unsupported: a reader has no writer fence.
//!
//! Lease rows remain writer-visible so a closed gate drains exactly the
//! readers it must wait for, and expiry still happens by TTL — the bridge
//! never lets the reader fabricate a different admission authority.
//!
//! The wire contract is one JSON POST per call. Domain rejections
//! (`novel_recovering`, `novel_epoch_conflict`, …) travel as
//! `{ok:false,error}` in a 200 response so the reader reconstructs the
//! original `NarouError::Platform` message; protocol failures (auth,
//! malformed body, transport) stay ordinary non-200 responses.

use std::sync::Arc;

use worker::*;

use narou_rs::error::{NarouError, Result as NarouResult};
use narou_rs::platform::{
    GateInfo, GateLease, GateState, NovelAccessFence, ObjectPrefix, PlatformFuture,
};

/// The single control route (POST only). It is not a delivery route:
/// `worker_role::owns_request` does not own it, so a reader isolate that
/// receives it forwards it to the writer like every other non-read path.
pub(crate) const CONTROL_PATH: &str = "/api/control/novel-access";

fn platform_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(error.to_string())
}

fn lease_json(lease: &GateLease) -> serde_json::Value {
    serde_json::json!({
        "reader_id": lease.reader_id,
        "epoch": lease.epoch,
        "expires_at": lease.expires_at,
    })
}

fn parse_lease(value: &serde_json::Value) -> NarouResult<GateLease> {
    let field = |name: &str| value.get(name).ok_or_else(|| platform_error(format!("writer fence lease {name} missing")));
    Ok(GateLease {
        reader_id: field("reader_id")?
            .as_str()
            .ok_or_else(|| platform_error("writer fence lease reader_id invalid"))?
            .to_owned(),
        epoch: field("epoch")?
            .as_u64()
            .ok_or_else(|| platform_error("writer fence lease epoch invalid"))?,
        expires_at: field("expires_at")?
            .as_i64()
            .ok_or_else(|| platform_error("writer fence lease expiry invalid"))?,
    })
}

/// Reader-side `NovelAccessFence`: every lease mutation is delegated to the
/// writer over its service binding, authenticated by the deployment's admin
/// credential. No lease exists in a form the writer did not issue.
pub(crate) struct RemoteNovelAccess {
    service: Fetcher,
    /// Precomputed `Bearer …`; `None` only when the deployment runs with
    /// authentication disabled (the writer then treats every request as
    /// authenticated, the same as the rest of the API).
    authorization: Option<String>,
}

impl RemoteNovelAccess {
    /// One control call: `Err` carries the writer's original domain message
    /// (200 `{ok:false}`), or a transport/protocol failure for non-200 and
    /// unreadable responses.
    async fn call(&self, body: serde_json::Value) -> NarouResult<serde_json::Value> {
        let mut init = RequestInit::new();
        init.with_method(Method::Post);
        let headers = Headers::new();
        headers
            .set("content-type", "application/json")
            .map_err(|error| platform_error(format!("writer fence headers: {error}")))?;
        if let Some(authorization) = &self.authorization {
            headers
                .set("authorization", authorization)
                .map_err(|error| platform_error(format!("writer fence headers: {error}")))?;
        }
        init.with_headers(headers);
        init.with_body(Some(wasm_bindgen::JsValue::from_str(&body.to_string())));
        let request = Request::new_with_init(
            &format!("https://narou-writer.invalid{CONTROL_PATH}"),
            &init,
        )
        .map_err(|error| platform_error(format!("writer fence request: {error}")))?;
        let mut response = self
            .service
            .fetch_request(request)
            .await
            .map_err(|error| platform_error(format!("writer fence unreachable: {error}")))?;
        if response.status_code() != 200 {
            let detail = response.text().await.unwrap_or_default();
            return Err(platform_error(format!(
                "writer fence rejected ({}): {}",
                response.status_code(),
                detail.chars().take(2000).collect::<String>()
            )));
        }
        let mut payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| platform_error(format!("writer fence response unreadable: {error}")))?;
        match payload.get("ok").and_then(|ok| ok.as_bool()) {
            Some(true) => payload
                .get_mut("payload")
                .map(std::mem::take)
                .ok_or_else(|| platform_error("writer fence payload missing")),
            Some(false) => Err(platform_error(
                payload
                    .get("error")
                    .and_then(|error| error.as_str())
                    .unwrap_or("writer fence operation failed"),
            )),
            None => Err(platform_error("writer fence returned no verdict")),
        }
    }

    fn unsupported(name: &'static str) -> NarouError {
        platform_error(format!("reader fence does not expose writer primitive: {name}"))
    }
}

impl NovelAccessFence for RemoteNovelAccess {
    /// A reader never captures a writer fence; doing so would let a delivery
    /// isolate impersonate a writer epoch.
    fn capture_writer<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
    ) -> PlatformFuture<'a, NarouResult<narou_rs::platform::WriteFence>> {
        Box::pin(async move {
            Err(platform_error(format!(
                "reader fence does not expose writer capture: {}",
                prefix.as_ref()
            )))
        })
    }

    fn admit_reader<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        reader_id: &'a str,
        lease_secs: i64,
    ) -> PlatformFuture<'a, NarouResult<GateLease>> {
        Box::pin(async move {
            let reply = self
                .call(serde_json::json!({
                    "op": "admit",
                    "prefix": prefix.as_ref(),
                    "reader_id": reader_id,
                    "lease_secs": lease_secs,
                }))
                .await?;
            parse_lease(reply.get("lease").unwrap_or(&reply))
        })
    }

    fn renew_reader<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        lease: &'a GateLease,
        lease_secs: i64,
    ) -> PlatformFuture<'a, NarouResult<GateLease>> {
        Box::pin(async move {
            let reply = self
                .call(serde_json::json!({
                    "op": "renew",
                    "prefix": prefix.as_ref(),
                    "lease": lease_json(lease),
                    "lease_secs": lease_secs,
                }))
                .await?;
            parse_lease(reply.get("lease").unwrap_or(&reply))
        })
    }

    fn release_reader<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
        lease: &'a GateLease,
    ) -> PlatformFuture<'a, NarouResult<()>> {
        Box::pin(async move {
            self.call(serde_json::json!({
                "op": "release",
                "prefix": prefix.as_ref(),
                "lease": lease_json(lease),
            }))
            .await?;
            Ok(())
        })
    }

    fn gate_info<'a>(
        &'a self,
        prefix: &'a ObjectPrefix,
    ) -> PlatformFuture<'a, NarouResult<GateInfo>> {
        Box::pin(async move {
            let reply = self
                .call(serde_json::json!({
                    "op": "gate_info",
                    "prefix": prefix.as_ref(),
                }))
                .await?;
            let info = reply
                .get("info")
                .ok_or_else(|| platform_error("writer fence info missing"))?;
            let field = |name: &str| {
                info.get(name)
                    .ok_or_else(|| platform_error(format!("writer fence info {name} missing")))
            };
            let state = match field("state")?.as_str() {
                Some("open") => GateState::Open,
                Some("closed") => GateState::Closed,
                _ => return Err(platform_error("writer fence info state invalid")),
            };
            let u64_field = |name: &str| {
                field(name)?
                    .as_u64()
                    .ok_or_else(|| platform_error(format!("writer fence info {name} invalid")))
            };
            Ok(GateInfo {
                epoch: u64_field("epoch")?,
                content_revision: u64_field("content_revision")?,
                state,
                pending_readers: u32::try_from(u64_field("pending_readers")?)
                    .map_err(|_| platform_error("writer fence info pending_readers overflow"))?,
                owner: info.get("owner").and_then(|v| v.as_str()).map(str::to_owned),
                updated_at: field("updated_at")?
                    .as_i64()
                    .ok_or_else(|| platform_error("writer fence info updated_at invalid"))?,
            })
        })
    }

    fn close_gate<'a>(
        &'a self,
        _prefix: &'a ObjectPrefix,
        _owner: &'a str,
    ) -> PlatformFuture<'a, NarouResult<u64>> {
        Box::pin(async move { Err(Self::unsupported("close_gate")) })
    }

    fn renew_gate<'a>(
        &'a self,
        _prefix: &'a ObjectPrefix,
        _owner: &'a str,
        _epoch: u64,
    ) -> PlatformFuture<'a, NarouResult<()>> {
        Box::pin(async move { Err(Self::unsupported("renew_gate")) })
    }

    fn open_gate<'a>(
        &'a self,
        _prefix: &'a ObjectPrefix,
        _owner: &'a str,
        _epoch: u64,
    ) -> PlatformFuture<'a, NarouResult<()>> {
        Box::pin(async move { Err(Self::unsupported("open_gate")) })
    }

    fn commit_staged<'a>(
        &'a self,
        _prefix: &'a ObjectPrefix,
        _owner: &'a str,
        _epoch: u64,
        _stage: &'a narou_rs::platform::ObjectKey,
        _canonical: &'a narou_rs::platform::ObjectKey,
    ) -> PlatformFuture<'a, NarouResult<()>> {
        Box::pin(async move { Err(Self::unsupported("commit_staged")) })
    }

    fn remove_committed<'a>(
        &'a self,
        _prefix: &'a ObjectPrefix,
        _owner: &'a str,
        _epoch: u64,
        _canonical: &'a narou_rs::platform::ObjectKey,
        _expected_sha256: &'a str,
    ) -> PlatformFuture<'a, NarouResult<()>> {
        Box::pin(async move { Err(Self::unsupported("remove_committed")) })
    }
}

/// Build the reader-side fence from the deployment bindings.
///
/// `NAROU_WRITER` must point at the update Worker and the admin credential
/// must match it; both are enforced again on the writer side, so a
/// misconfigured reader fails closed with a remote rejection instead of
/// silently writing the shared D1 itself.
pub(crate) async fn remote_access(env: &Env) -> Result<Arc<dyn NovelAccessFence>> {
    let service = env.service(crate::composition::WRITER_SERVICE_BINDING)?;
    let required = env
        .var("NAROU_AUTH_REQUIRED")
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "true".to_string());
    let authorization = if required.eq_ignore_ascii_case("false") {
        None
    } else {
        crate::secrets::value(env, "NAROU_ADMIN_TOKEN")
            .await
            .map(|token| format!("Bearer {token}"))
    };
    Ok(Arc::new(RemoteNovelAccess {
        service,
        authorization,
    }))
}

/// Writer side of the bridge: `POST CONTROL_PATH`.
///
/// Every operation binds to the *actual* durable fence (`D1ObjectStore`),
/// never a reimplementation — admission/renewal semantics stay identical to
/// readers admitted inside the writer isolate itself. Only reader
/// primitives are reachable; there is no remote writer-fence handle.
pub(crate) async fn handle(mut req: Request, env: Env) -> Result<Response> {
    if crate::worker_role::is_reader(&env)? {
        return Response::error("novel access control runs on the writer", 403);
    }
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return Response::error("invalid novel-access control body", 400),
    };
    let text = |name: &str| body.get(name).and_then(|v| v.as_str());
    let prefix = match text("prefix").map(ObjectPrefix::new) {
        Some(Ok(prefix)) => prefix,
        _ => return Response::error("invalid novel-access prefix", 400),
    };
    let lease = || -> Option<GateLease> {
        let field = body.get("lease")?;
        Some(GateLease {
            reader_id: field.get("reader_id")?.as_str()?.to_owned(),
            epoch: field.get("epoch")?.as_u64()?,
            expires_at: field.get("expires_at")?.as_i64()?,
        })
    };
    let ttl = body
        .get("lease_secs")
        .and_then(|v| v.as_i64())
        .unwrap_or(narou_rs::platform::READER_LEASE_SECS);

    let db = crate::db_handle::DbHandle::primary(Arc::new(env.d1("DB")?));
    let store = crate::d1_object_store::D1ObjectStore::new(db);
    // Domain rejections keep 200 + {ok:false} so the reader rebuilds the
    // original error text (and its machine prefix); protocol failures are
    // ordinary non-200 responses.
    let result: NarouResult<serde_json::Value> = match text("op") {
        Some("admit") => match text("reader_id") {
            Some(reader_id) => store
                .admit_reader(&prefix, reader_id, ttl)
                .await
                .map(|lease| serde_json::json!({ "lease": lease_json(&lease) })),
            None => return Response::error("missing novel-access reader_id", 400),
        },
        Some("renew") => match lease() {
            Some(current) => store
                .renew_reader(&prefix, &current, ttl)
                .await
                .map(|lease| serde_json::json!({ "lease": lease_json(&lease) })),
            None => return Response::error("invalid novel-access lease", 400),
        },
        Some("release") => match lease() {
            Some(current) => store
                .release_reader(&prefix, &current)
                .await
                .map(|()| serde_json::json!({})),
            None => return Response::error("invalid novel-access lease", 400),
        },
        Some("gate_info") => store
            .gate_info(&prefix)
            .await
            .map(|info| {
                serde_json::json!({
                    "info": {
                        "epoch": info.epoch,
                        "content_revision": info.content_revision,
                        "state": match info.state {
                            GateState::Open => "open",
                            GateState::Closed => "closed",
                        },
                        "pending_readers": info.pending_readers,
                        "owner": info.owner,
                        "updated_at": info.updated_at,
                    }
                })
            }),
        _ => return Response::error("unknown novel-access operation", 400),
    };
    match result {
        Ok(payload) => Response::from_json(&serde_json::json!({
            "ok": true,
            "payload": payload,
        })),
        Err(error) => {
            let message = match error {
                NarouError::Platform(message) => message,
                error => error.to_string(),
            };
            Response::from_json(&serde_json::json!({
                "ok": false,
                "error": message,
            }))
        },
    }
}