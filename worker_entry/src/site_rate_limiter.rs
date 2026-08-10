//! Per-site rate limiting Durable Object (Phase 8).
//!
//! One DO instance per normalized site key. The Workers runtime serializes
//! requests to a DO, which makes the permit allocation atomic per site:
//! read the counter / next-allowed timestamp from [`State`] storage, apply
//! the same pacing algorithm as the native limiter (interval spacing, with
//! the なろう wait-steps default of 10 and a 5-second max-step wait), write
//! back, and return the permit timestamp. The caller awaits
//! `worker::Delay` until that timestamp.
//!
//! No alarm is used: allocation happens synchronously inside `fetch`.

use serde::{Deserialize, Serialize};
use worker::*;

/// Permit request body sent by [`crate::rate_limiter::WorkerRateLimiter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermitRequest {
    /// Minimum spacing between consecutive requests, in milliseconds.
    pub interval_ms: u64,
    /// Every `wait_steps`-th request waits `max_steps_wait_time_ms` instead
    /// of `interval_ms` (native `delay_after_request` semantics).
    pub wait_steps: u32,
    pub max_steps_wait_time_ms: u64,
}

/// Permit response: the epoch-millis timestamp at which the caller may
/// proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermitResponse {
    pub permit_at_ms: u64,
}

#[durable_object]
pub struct SiteRateLimiter {
    state: State,
}

impl DurableObject for SiteRateLimiter {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        if req.method() != Method::Post {
            return Response::error("Method Not Allowed", 405);
        }
        let permit: PermitRequest = req
            .json()
            .await
            .map_err(|error| Error::RustError(format!("invalid permit request: {error}")))?;
        if permit.interval_ms == 0 {
            return Response::error("interval_ms must be positive", 400);
        }
        let storage = self.state.storage();
        let now_ms = js_sys::Date::now() as u64;

        let mut counter: u64 = storage.get("counter").await?.unwrap_or(0);
        let next_allowed_at_ms: u64 = storage.get("next_allowed_at_ms").await?.unwrap_or(0);

        // Idle reset mirrors the native limiter: after a long quiet period
        // the site's burst counter restarts so the next request goes out
        // immediately.
        if next_allowed_at_ms > 0
            && now_ms > next_allowed_at_ms.saturating_add(permit.max_steps_wait_time_ms)
        {
            counter = 0;
        }

        let allowed_at_ms = next_allowed_at_ms.max(now_ms);
        counter += 1;

        // Native `delay_after_request`: every `wait_steps`-th request waits
        // the max-step duration; all others wait the base interval.
        let wait_steps = u64::from(permit.wait_steps);
        let delay_ms = if wait_steps > 0 && counter % wait_steps == 0 && counter >= wait_steps {
            permit.max_steps_wait_time_ms
        } else if counter > 0 {
            permit.interval_ms
        } else {
            0
        };

        let new_next_allowed_at_ms = allowed_at_ms.saturating_add(delay_ms);
        storage.put("counter", counter).await?;
        storage
            .put("next_allowed_at_ms", new_next_allowed_at_ms)
            .await?;

        Response::from_json(&PermitResponse {
            permit_at_ms: allowed_at_ms,
        })
    }
}
