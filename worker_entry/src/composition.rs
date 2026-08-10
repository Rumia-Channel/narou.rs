use std::sync::Arc;

use narou_rs::application::{
    AppServiceDependencies, AppServices, EmptySiteDefinitionProvider, EmptySiteTimezoneProvider,
    EmptyWebActionService, JobService, NoopSelfUpdateService, NovelActionService,
    NovelContentService, NovelSettingsService, SchedulerService, SettingsService, TagColorService,
};
use narou_rs::platform::{NovelRepository, ObjectStore, SystemClock};
use worker::Env;

use crate::d1_repository::{D1FreezeStore, D1NovelRepository, D1SettingsStore, D1TagColorStore};
use crate::wasabi::{WasabiConfig, WasabiObjectStore};

/// Worker composition root.
///
/// Production bindings are required. Memory adapters are intentionally absent:
/// a missing D1 or Wasabi binding fails readiness rather than serving an empty
/// library and silently accepting writes.
pub fn build_services(env: &Env) -> worker::Result<AppServices> {
    let db = Arc::new(env.d1("DB")?);
    let objects: Arc<dyn ObjectStore> = Arc::new(WasabiObjectStore::new(
        WasabiConfig::from_env(env)
            .map_err(|error| worker::Error::RustError(error.to_string()))?,
    ));
    let novels: Arc<dyn NovelRepository> = Arc::new(D1NovelRepository::new(db.clone()));
    let clock = Arc::new(SystemClock);
    let freeze = Arc::new(D1FreezeStore::new(db.clone()));

    let library = Arc::new(narou_rs::application::LibraryService::new(
        novels.clone(),
        clock.clone(),
        freeze.clone(),
        Arc::new(EmptySiteTimezoneProvider),
    ));
    let novel_actions = Arc::new(NovelActionService::new(
        novels.clone(),
        freeze.clone(),
        freeze,
        Some(objects.clone()),
    ));

    Ok(AppServices::new(AppServiceDependencies {
        library,
        novel_actions,
        novel_settings: Arc::new(NovelSettingsService::new(novels.clone(), objects.clone())),
        content: Arc::new(NovelContentService::new(novels, objects)),
        settings: Arc::new(SettingsService::new(Arc::new(D1SettingsStore::new(db.clone())))),
        tag_colors: Arc::new(TagColorService::new(Arc::new(D1TagColorStore::new(
            db,
        )))),
        jobs: Arc::new(JobService),
        scheduler: Arc::new(SchedulerService::new(clock)),
        site_definitions: Arc::new(EmptySiteDefinitionProvider),
        self_update: Arc::new(NoopSelfUpdateService),
        web_actions: Arc::new(EmptyWebActionService),
    }))
}
