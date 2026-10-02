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
//!
//! The pacing parameters come from the `download.interval` /
//! `download.wait-steps` settings (read once per `WorkerRuntime::build`)
//! via the shared [`DownloadPacing`] rules; the site definition's
//! `min_interval` floor is applied per scope exactly like the native
//! limiter's `reserve_wait_duration_for_scope`.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use parking_lot::Mutex;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    normalize_site_key, DownloadPacing, PlatformFuture, RateLimitScope, RateLimiter,
};
use wasm_bindgen::JsValue;
use worker::{Env, Method, ObjectNamespace, Request, RequestInit};

use crate::budget::SubrequestBudget;
use crate::site_rate_limiter::{PermitRequest, PermitResponse};

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
    /// `download.interval` / `download.wait-steps` の展開済み設定。
    pacing: DownloadPacing,
}

impl WorkerRateLimiter {
    /// `interval_secs` / `wait_steps` は `download.interval` /
    /// `download.wait-steps` の設定値 (`None` で既定値)。値はジョブ単位で
    /// 読み直されるため、設定変更は次の実行から効く (D1SettingsStore の
    /// TTL 30 秒を上限とする遅延を除く)。
    pub fn new(
        env: &Env,
        subrequests: SubrequestBudget,
        interval_secs: Option<f64>,
        wait_steps: Option<i64>,
    ) -> Result<Self> {
        let namespace = env
            .durable_object("RATE_LIMITER")
            .map_err(worker_error)?;
        Ok(Self {
            namespace,
            subrequests,
            permits: Mutex::new(HashMap::new()),
            pacing: DownloadPacing::new(interval_secs, wait_steps),
        })
    }

    fn pop_cached_permit(&self, site: &str) -> Option<u64> {
        let mut permits = self.permits.lock();
        let queue = permits.get_mut(site)?;
        let permit = queue.pop_front();
        if queue.is_empty() {
            permits.remove(site);
        }
        permit
    }

    fn cache_permits(&self, site: &str, permit_ats: Vec<u64>) {
        let mut permits = self.permits.lock();
        permits
            .entry(site.to_string())
            .or_default()
            .extend(permit_ats);
    }

    /// スコープと設定値から DO へ送る `PermitRequest` を組み立てる。
    fn permit_request(&self, scope: &RateLimitScope) -> PermitRequest {
        let scoped = self.pacing.for_scope(scope);
        PermitRequest {
            interval_ms: u64::try_from(scoped.interval.as_millis()).unwrap_or(u64::MAX),
            wait_steps: scoped.wait_steps,
            max_steps_wait_time_ms: u64::try_from(scoped.max_steps_wait_time.as_millis())
                .unwrap_or(u64::MAX),
            count: PERMIT_BATCH_SIZE,
        }
    }

    async fn claim_batch(&self, site: &str, scope: &RateLimitScope) -> Result<Vec<u64>> {
        let permit = self.permit_request(scope);
        self.claim(site, &permit).await
    }

    /// 1 回分の permit 要求を DO へ送り、許可時刻の一覧を受け取る。
    async fn claim(&self, site: &str, permit: &PermitRequest) -> Result<Vec<u64>> {
        let stub = self
            .namespace
            .get_by_name(site)
            .map_err(worker_error)?;

        let body = serde_json::to_string(permit)
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
        wait_until_ms(permit_at_ms, now_ms).await;
        Ok(())
    }

    /// native `commands::update` の `update.interval`（同じドメインの作品開始を
    /// 一定時間空ける）を DO で数える。HTTP リクエストの間隔 (`download.interval`)
    /// とは別のバケット (`update-start:<domain>`) を使うので、本文取得のペーシング
    /// には影響しない。DO が state を永続化するため、別 isolate で走るジョブ間でも
    /// 間隔が保たれる。
    pub(crate) async fn acquire_work_start(
        &self,
        domain: &str,
        interval: Duration,
    ) -> Result<()> {
        if interval.is_zero() {
            return Ok(());
        }
        let site = normalize_site_key(&format!("update-start:{domain}")).ok_or_else(|| {
            NarouError::Platform("update-start domain is empty".to_string())
        })?;
        let interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
        // 作品開始の間隔だけを数えるので、wait-steps の段差もまとめ取りもしない
        // (`count = 1` が 1 作品の開始時刻そのものを表す)。
        let permit = PermitRequest {
            interval_ms,
            wait_steps: 0,
            max_steps_wait_time_ms: interval_ms,
            count: 1,
        };
        let permit_at_ms = self
            .claim(&site, &permit)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| NarouError::Platform("rate limiter DO returned no permits".to_string()))?;
        let now_ms = js_sys::Date::now() as u64;
        wait_until_ms(permit_at_ms, now_ms).await;
        Ok(())
    }
}

/// 許可時刻まで待つ (既に過ぎていれば待たない)。
async fn wait_until_ms(permit_at_ms: u64, now_ms: u64) {
    let wait_ms = permit_at_ms.saturating_sub(now_ms);
    if wait_ms > 0 {
        worker::Delay::from(Duration::from_millis(wait_ms)).await;
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
