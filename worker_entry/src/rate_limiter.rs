//! Worker [`RateLimiter`] adapter (Phase 8).
//!
//! Serializes per-site permit allocation through the `SiteRateLimiter`
//! Durable Object (binding `RATE_LIMITER`), then awaits `worker::Delay`
//! until the returned permit timestamp. Site keys are normalized before
//! they become DO ids, so alternate spellings of a site share one rate
//! bucket.
//!
//! Permits are claimed in batches of [`PERMIT_BATCH_SIZE`]: one DO
//! subrequest returns a run of consecutive permit timestamps, which are
//! cached per site and consumed by later `acquire` calls. This keeps the
//! per-site pacing identical while cutting the DO subrequest count — and
//! its billed request/duration — by the batch factor.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    normalize_site_key, PlatformFuture, RateLimitScope, RateLimiter,
};
use wasm_bindgen::JsValue;
use worker::{Env, Method, ObjectNamespace, Request, RequestInit};

use crate::budget::SubrequestBudget;
use crate::site_rate_limiter::{PermitRequest, PermitResponse};

/// Pacing defaults mirroring the native limiter
/// (`DEFAULT_INTERVAL_SECS = 0.7`, なろう wait-steps default 10,
/// `STEPS_WAIT_TIME = 5s`).
const DEFAULT_INTERVAL_MS: u64 = 700;
const MAX_STEPS_WAIT_TIME_MS: u64 = 5_000;

/// Permits claimed per Durable Object call. Small enough that an abandoned
/// remainder only delays the site by a few seconds; large enough to keep DO
/// subrequests negligible over a long download.
const PERMIT_BATCH_SIZE: u32 = 16;

/// Rate limiter that delegates each site to a per-site Durable Object.
#[derive(Debug)]
pub struct WorkerRateLimiter {
    namespace: ObjectNamespace,
    subrequests: SubrequestBudget,
    /// Cached permit timestamps per normalized site key, in claim order.
    permits: Mutex<HashMap<String, VecDeque<u64>>>,
}

impl WorkerRateLimiter {
    pub fn new(env: &Env, subrequests: SubrequestBudget) -> Result<Self> {
        let namespace = env
            .durable_object("RATE_LIMITER")
            .map_err(worker_error)?;
        Ok(Self {
            namespace,
            subrequests,
            permits: Mutex::new(HashMap::new()),
        })
    }

    fn pop_cached_permit(&self, site: &str) -> Option<u64> {
        let mut permits = self.permits.lock().expect("permit cache poisoned");
        let queue = permits.get_mut(site)?;
        let permit = queue.pop_front();
        if queue.is_empty() {
            permits.remove(site);
        }
        permit
    }

    fn cache_permits(&self, site: &str, permit_ats: Vec<u64>) {
        let mut permits = self.permits.lock().expect("permit cache poisoned");
        permits
            .entry(site.to_string())
            .or_default()
            .extend(permit_ats);
    }

    async fn claim_batch(&self, site: &str, scope: &RateLimitScope) -> Result<Vec<u64>> {
        let stub = self
            .namespace
            .get_by_name(site)
            .map_err(worker_error)?;

        let permit = PermitRequest {
            interval_ms: DEFAULT_INTERVAL_MS,
            wait_steps: if scope.narou { 10 } else { 0 },
            max_steps_wait_time_ms: MAX_STEPS_WAIT_TIME_MS,
            count: PERMIT_BATCH_SIZE,
        };
        let body = serde_json::to_string(&permit)
            .map_err(|error| NarouError::Platform(format!("permit request serialization: {error}")))?;

        let mut init = RequestInit::new();
        init.with_method(Method::Post);
        init.with_body(Some(JsValue::from_str(&body)));
        let request = Request::new_with_init(&format!("https://rate-limiter.local/{site}"), &init)
            .map_err(worker_error)?;
        self.subrequests.record();
        let mut response = stub
            .fetch_with_request(request)
            .await
            .map_err(worker_error)?;
        if response.status_code() != 200 {
            return Err(NarouError::Platform(format!(
                "rate limiter DO returned status {}",
                response.status_code()
            )));
        }
        let response = response.json::<PermitResponse>().await.map_err(worker_error)?;
        if response.permit_ats_ms.is_empty() {
            return Err(NarouError::Platform(
                "rate limiter DO returned no permits".to_string(),
            ));
        }
        Ok(response.permit_ats_ms)
    }

    async fn acquire_scope(&self, scope: &RateLimitScope) -> Result<()> {
        let site = normalize_site_key(&scope.site).ok_or_else(|| {
            NarouError::Platform("rate-limit site key is empty".to_string())
        })?;
        let permit_at_ms = match self.pop_cached_permit(&site) {
            Some(permit) => permit,
            None => {
                let batch = self.claim_batch(&site, scope).await?;
                let mut iter = batch.into_iter();
                let first = iter.next().expect("non-empty permit batch");
                self.cache_permits(&site, iter.collect());
                first
            }
        };
        let now_ms = js_sys::Date::now() as u64;
        let wait_ms = permit_at_ms.saturating_sub(now_ms);
        if wait_ms > 0 {
            worker::Delay::from(Duration::from_millis(wait_ms)).await;
        }
        Ok(())
    }
}

impl RateLimiter for WorkerRateLimiter {
    fn acquire<'a>(
        &'a self,
        scope: &'a RateLimitScope,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.acquire_scope(scope))
    }
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker rate limiter error: {error}"))
}
