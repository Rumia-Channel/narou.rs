//! D1-only composition for UI metadata reads and global settings.
//!
//! These routes do not use object payloads, S3, login cookies, an HTTP client,
//! a queue producer or a rate limiter. Building a full WorkerRuntime for them
//! added unrelated serial reads before the actual UI query. Keep the shared
//! application services (and their validation/cache invalidation) but only
//! construct their D1 dependencies. Handles are request-local, never global.
//! Authentication still happens in each route before calling these helpers.

use std::sync::Arc;

use narou_rs::application::{
    EmptySiteTimezoneProvider, LibraryService, SettingsService, TagColorService,
};
use narou_rs::platform::SystemClock;
use worker::{Env, Response};

use crate::d1_repository::{D1FreezeStore, D1NovelRepository, D1SettingsStore, D1TagColorStore};
use crate::db_handle::DbHandle;

pub(crate) struct MetadataServices {
    pub db: DbHandle,
    pub library: LibraryService,
    pub settings: SettingsService,
    pub tag_colors: TagColorService,
}

pub(crate) fn database(env: &Env) -> worker::Result<DbHandle> {
    Ok(DbHandle::ui(Arc::new(env.d1("DB")?)))
}

pub(crate) fn settings(env: &Env) -> worker::Result<SettingsService> {
    Ok(SettingsService::new(Arc::new(D1SettingsStore::new(database(env)?))))
}

impl MetadataServices {
    pub(crate) fn new(env: &Env) -> worker::Result<Self> {
        let db = database(env)?;
        let library = LibraryService::new(
            Arc::new(D1NovelRepository::new(db.clone())),
            Arc::new(SystemClock),
            Arc::new(D1FreezeStore::new(db.clone())),
            Arc::new(EmptySiteTimezoneProvider),
        );
        Ok(Self {
            library,
            settings: SettingsService::new(Arc::new(D1SettingsStore::new(db.clone()))),
            tag_colors: TagColorService::new(Arc::new(D1TagColorStore::new(db.clone()))),
            db,
        })
    }
}

/// Post-authentication wall time, including D1 waits, not a CPU measurement.
/// Workers' clock advances at I/O boundaries; use Workers CPU metrics for
/// synchronous work. No SQL, query values, titles or credentials are exposed.
pub(crate) fn timed_response(
    mut response: Response,
    metric: &'static str,
    started_ms: f64,
) -> worker::Result<Response> {
    let elapsed = (js_sys::Date::now() - started_ms).max(0.0);
    response.headers_mut().set("Server-Timing", &format!("{metric};dur={elapsed:.1}"))?;
    Ok(response)
}

pub(crate) async fn configured_tag_color(settings: &SettingsService) -> Option<String> {
    settings
        .get(narou_rs::application::tag_colors::NEW_TAG_COLOR_SETTING)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| narou_rs::application::tag_colors::is_valid_tag_color(value))
}
