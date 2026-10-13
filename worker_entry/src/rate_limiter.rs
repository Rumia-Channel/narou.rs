//! Worker pacing adapter. Only actual grants consume a slot; delayed callers
//! recheck the per-site Durable Object instead of spending cached reservations.

use std::time::Duration;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::rate_limiter::{PacingDecision, duration_millis_ceil};
use narou_rs::platform::{normalize_site_key, PlatformFuture, RateLimitScope, RateLimiter};
use wasm_bindgen::JsValue;
use worker::{Env, Method, ObjectNamespace, Request, RequestInit};

use crate::budget::SubrequestBudget;
use crate::site_rate_limiter::{PACING_REFRESH_TTL_MS, PermitRequest};

pub struct WorkerRateLimiter {
    namespace: ObjectNamespace,
    subrequests: SubrequestBudget,
}

impl WorkerRateLimiter {
    pub fn new(env: &Env, subrequests: SubrequestBudget) -> Result<Self> {
        Ok(Self {
            namespace: env.durable_object("RATE_LIMITER").map_err(worker_error)?,
            subrequests,
        })
    }

    async fn acquire_permit(&self, site: &str, permit: PermitRequest) -> Result<()> {
        let stub = self.namespace.get_by_name(site).map_err(worker_error)?;
        let body = serde_json::to_string(&permit)
            .map_err(|error| NarouError::Platform(format!("permit request serialization: {error}")))?;
        loop {
            let mut init = RequestInit::new();
            init.with_method(Method::Post);
            init.with_body(Some(JsValue::from_str(&body)));
            let request = Request::new_with_init("https://rate-limiter.local/acquire", &init)
                .map_err(worker_error)?;
            self.subrequests.record();
            let mut response = stub.fetch_with_request(request).await.map_err(worker_error)?;
            if response.status_code() != 200 {
                return Err(NarouError::Platform(format!(
                    "rate limiter DO returned status {}", response.status_code()
                )));
            }
            match response.json::<PacingDecision>().await.map_err(worker_error)? {
                PacingDecision::Granted => return Ok(()),
                PacingDecision::Wait { wait_ms } => {
                    // Recheck even a long wait so a settings decrease reaches an
                    // active acquisition within the same configuration TTL.
                    worker::Delay::from(Duration::from_millis(wait_ms.min(PACING_REFRESH_TTL_MS))).await;
                }
            }
        }
    }

    async fn acquire_scope(&self, scope: &RateLimitScope) -> Result<()> {
        let site = normalize_site_key(&scope.site).ok_or_else(|| {
            NarouError::Platform("rate-limit site key is empty".to_string())
        })?;
        self.acquire_permit(&site, PermitRequest::Http {
            narou: scope.narou,
            min_interval_ms: scope.min_interval.map(duration_millis_ceil).unwrap_or(0),
        }).await
    }

    pub(crate) async fn acquire_work_start(&self, domain: &str) -> Result<()> {
        let domain = normalize_site_key(domain).ok_or_else(|| {
            NarouError::Platform("update-start domain is empty".to_string())
        })?;
        self.acquire_permit(&format!("update-start:{domain}"), PermitRequest::WorkStart).await
    }
}

impl RateLimiter for WorkerRateLimiter {
    fn acquire<'a>(&'a self, scope: &'a RateLimitScope) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.acquire_scope(scope))
    }
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker rate limiter error: {error}"))
}
