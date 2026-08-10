use std::sync::Arc;

use narou_rs::application::{
    AppServiceDependencies, AppServices, EmptySiteDefinitionProvider, EmptySiteTimezoneProvider,
    EmptyWebActionService, JobService, MemorySettingsStore, MemoryTagColorStore,
    NoopFreezeMutationStore, NoopSelfUpdateService, NovelActionService, NovelContentService,
    NovelSettingsService, SchedulerService, SystemFreezeStore, TagColorService,
};
use narou_rs::platform::mocks::{MemoryNovelRepository, MemoryObjectStore};
use narou_rs::platform::{NovelRepository, ObjectStore, SystemClock};

/// Worker composition root.
///
/// The adapters here are deliberately in-memory until the D1/ObjectStore
/// adapters are introduced. Keeping construction in this crate makes the
/// Worker boundary explicit without coupling the core services to `worker`.
pub fn build_services() -> AppServices {
    let novels: Arc<dyn NovelRepository> = Arc::new(MemoryNovelRepository::new());
    let objects: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    let clock = Arc::new(SystemClock);
    let freeze = Arc::new(SystemFreezeStore);

    let library = Arc::new(narou_rs::application::LibraryService::new(
        novels.clone(),
        clock.clone(),
        freeze.clone(),
        Arc::new(EmptySiteTimezoneProvider),
    ));
    let novel_actions = Arc::new(NovelActionService::new(
        novels.clone(),
        freeze,
        Arc::new(NoopFreezeMutationStore),
        Some(objects.clone()),
    ));

    AppServices::new(AppServiceDependencies {
        library,
        novel_actions,
        novel_settings: Arc::new(NovelSettingsService::new(novels.clone(), objects.clone())),
        content: Arc::new(NovelContentService::new(novels, objects)),
        settings: Arc::new(narou_rs::application::SettingsService::new(Arc::new(
            MemorySettingsStore::new(),
        ))),
        tag_colors: Arc::new(TagColorService::new(Arc::new(
            MemoryTagColorStore::default(),
        ))),
        jobs: Arc::new(JobService),
        scheduler: Arc::new(SchedulerService::new(clock)),
        site_definitions: Arc::new(EmptySiteDefinitionProvider),
        self_update: Arc::new(NoopSelfUpdateService),
        web_actions: Arc::new(EmptyWebActionService),
    })
}
