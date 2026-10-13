//! Per-site pacing authority. Settings come from primary D1, not a job's
//! potentially stale snapshot. The mutex covers external awaits as well as
//! storage: Durable Object input gates alone do not serialize D1 requests.

use std::sync::Arc;
use std::time::Duration;

use futures::lock::Mutex;
use narou_rs::application::settings::SettingsStore;
use narou_rs::platform::rate_limiter::{
    DownloadPacing, PacingSchedule, PacingState, duration_millis_ceil,
    setting_update_interval_secs, try_acquire,
};
use narou_rs::setting_core::SettingScope;
use serde::{Deserialize, Serialize};
use worker::*;

use crate::d1_repository::D1SettingsStore;
use crate::db_handle::DbHandle;

pub const PACING_REFRESH_TTL_MS: u64 = 30_000;
const GRANTS_KEY: &str = "pacing_state";
// A previous Queue invocation cannot retain a reservation beyond its 15-minute
// wall-clock lifetime. Preserve its fence, but not abandoned reservations for
// arbitrarily large configured intervals.
const LEGACY_QUEUE_LIFETIME_MS: u64 = 15 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermitRequest {
    Http { narou: bool, min_interval_ms: u64 },
    WorkStart,
}

struct SettingsSnapshot {
    download: DownloadPacing,
    update_interval_ms: u64,
    read_at_ms: u64,
}

#[derive(Default)]
struct Inner {
    settings: Option<SettingsSnapshot>,
    grants: Option<PacingState>,
}

#[durable_object]
pub struct SiteRateLimiter {
    state: State,
    env: Env,
    inner: Mutex<Inner>,
}

impl DurableObject for SiteRateLimiter {
    fn new(state: State, env: Env) -> Self {
        Self { state, env, inner: Mutex::new(Inner::default()) }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        if req.method() != Method::Post {
            return Response::error("Method Not Allowed", 405);
        }
        let permit = match req.json::<PermitRequest>().await {
            Ok(permit) => permit,
            Err(_) => return Response::error("Invalid permit request", 400),
        };
        let mut inner = self.inner.lock().await;
        let now_ms = js_sys::Date::now() as u64;
        if inner.settings.as_ref().is_none_or(|snapshot| {
            now_ms.saturating_sub(snapshot.read_at_ms) >= PACING_REFRESH_TTL_MS
        }) {
            let db = DbHandle::primary(Arc::new(self.env.d1("DB")?));
            let values = D1SettingsStore::for_writes(db).load(SettingScope::Local).await
                .map_err(|error| Error::RustError(error.to_string()))?;
            inner.settings = Some(SettingsSnapshot {
                download: DownloadPacing::from_settings(&values),
                update_interval_ms: duration_millis_ceil(Duration::from_secs_f64(
                    setting_update_interval_secs(values.get("update.interval")),
                )),
                read_at_ms: now_ms,
            });
        }
        let snapshot = inner.settings.as_ref().expect("loaded pacing settings");
        let settings_expire_ms = snapshot.read_at_ms.saturating_add(PACING_REFRESH_TTL_MS);
        let schedule = match permit {
            PermitRequest::Http { narou, min_interval_ms } => {
                PacingSchedule::from_scoped(&snapshot.download.for_site(
                    narou, Some(Duration::from_millis(min_interval_ms)),
                ))
            }
            PermitRequest::WorkStart => PacingSchedule {
                interval_ms: snapshot.update_interval_ms,
                wait_steps: 0,
                max_steps_wait_time_ms: snapshot.update_interval_ms,
                reset_window_ms: snapshot.update_interval_ms,
            },
        };
        let storage = self.state.storage();
        let mut grants = if let Some(grants) = inner.grants {
            grants
        } else if let Some(encoded) = storage.get::<String>(GRANTS_KEY).await? {
            serde_json::from_str::<PacingState>(&encoded)
                .map_err(|error| Error::RustError(format!("invalid stored pacing state: {error}")))?
        } else {
            // Old last_permit_ms represented a future reservation, not an
            // actual grant, so it must never be used for rebasing new policy.
            let next = storage.get::<Option<u64>>("next_allowed_at_ms").await?.flatten();
            PacingState {
                counter: storage.get::<u64>("counter").await?.unwrap_or(0),
                next_permit_ms: next.map(|next| next.min(
                    (js_sys::Date::now() as u64).saturating_add(LEGACY_QUEUE_LIFETIME_MS),
                )),
                ..PacingState::default()
            }
        };
        let acquired_at_ms = js_sys::Date::now() as u64;
        let decision = try_acquire(&mut grants, acquired_at_ms, schedule);
        if inner.grants != Some(grants) {
            let encoded = serde_json::to_string(&grants)
                .map_err(|error| Error::RustError(error.to_string()))?;
            storage.put(GRANTS_KEY, encoded).await?;
            inner.grants = Some(grants);
        }
        let decision = match decision {
            narou_rs::platform::rate_limiter::PacingDecision::Wait { wait_ms } => {
                let replied_at_ms = js_sys::Date::now() as u64;
                // A snapshot may already be 20 seconds old. Wake at its
                // expiry, not another full TTL after this denied attempt.
                narou_rs::platform::rate_limiter::PacingDecision::Wait {
                    wait_ms: wait_ms.saturating_sub(replied_at_ms.saturating_sub(acquired_at_ms))
                        .min(settings_expire_ms.saturating_sub(replied_at_ms)).max(1),
                }
            }
            granted => granted,
        };
        Response::from_json(&decision)
    }
}
