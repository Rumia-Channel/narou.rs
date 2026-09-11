//! Application services: platform-neutral business logic for the web UI.
//!
//! Phase 5: the web layer delegates to these services instead of touching
//! the database, inventory, or filesystem directly. Application code must
//! not import the web framework, the web/native modules, database globals,
//! the inventory type, filesystem/process, or HTTP clients; it depends only
//! on `serde`/`chrono`, the domain [`NovelRecord`], and the platform traits.
//!
//! Modules:
//! - [`error`]: the application-layer error type (no HTTP status).
//! - [`events`]: application-owned platform ports (`FreezeStore`,
//!   `SiteTimezoneProvider`) plus their no-op defaults.
//! - [`library`]: the novel library list service (search / filter /
//!   pagination / frozen status / new-arrival marker).

pub mod error;
pub mod events;
pub mod jobs;
pub mod novel_actions;
pub mod novel_settings;
pub mod novel_content;
pub mod scheduler;
pub mod self_update;
pub mod settings;
pub mod tag_colors;
pub mod web_actions;
pub use jobs::{
    CheckpointClaim, CheckpointState, ExecutionPhase, JobClaim, JobFailureClass, JobId, JobKind,
    JobLedgerStatus, JobPlan, JobPlanResult, JobQueue, JobRequest, JobService, JobTarget,
    LegacyEnvelopeOutcome, QueuedJob, QueuedJobView, SchedulerCheckpoint, UpdateScanPage,
    WorkerExecutionCheckpoint, WorkerJobEnvelope, WORKER_JOB_ENVELOPE_VERSION, classify_failure,
    decode_legacy_envelope, envelope_bytes, job_limits, validate_request_limits,
};
pub use novel_actions::{
    FileDeletionStatus, FreezeMutationStore, FreezeRequest, FreezeResult,
    MemoryFreezeMutationStore, NoopFreezeMutationStore, NovelActionService,
    RemoveRequest, RemoveResult, TagAction, TagChangeRequest, TagChangeResult,
};
pub use novel_settings::{
    NovelSettingsPatch, NovelSettingsService, NovelSettingsView, ReplacePattern,
};
pub use novel_content::NovelContentService;
pub use scheduler::{
    AutoUpdatePolicy, AutoUpdateSchedule, AutoUpdateTargets, Schedule,
    ScheduleDecision, SchedulerService,
};
pub use settings::{
    MemorySettingsStore, SettingEntry, SettingsEffect, SettingsService, SettingsStore,
};
pub use web_actions::{
    EmptyWebActionService, WebActionOutput, WebActionService,
};
pub use tag_colors::{MemoryTagColorStore, TagColorService, TagColorStore};
pub mod library;

pub use error::ApplicationError;
pub use events::{
    ApplicationEvent, EmptySiteDefinitionProvider, EmptySiteTimezoneProvider,
    EmptySiteUpdateCapabilityProvider, EventSink, FreezeStore, NoopEventSink,
    SiteDefinitionProvider, SiteTimezone, SiteTimezoneProvider,
    SiteUpdateCapabilityProvider, SystemFreezeStore,
};
pub use self_update::{NoopSelfUpdateService, SelfUpdateRequest, SelfUpdateResult, SelfUpdateService};
pub use library::{
    LibraryListRequest, LibraryPage, LibraryService, LibrarySortColumn, LibrarySortOrder,
    NovelSummary,
};


use std::sync::Arc;

/// Dependencies required to compose the application services.
///
/// Keeping the composition input as a named value makes native and Worker
/// roots readable and avoids an argument-count lint without suppressing it.
pub struct AppServiceDependencies {
    pub library: Arc<LibraryService>,
    pub novel_actions: Arc<NovelActionService>,
    pub novel_settings: Arc<NovelSettingsService>,
    pub content: Arc<NovelContentService>,
    pub settings: Arc<SettingsService>,
    pub tag_colors: Arc<TagColorService>,
    pub jobs: Arc<JobService>,
    pub scheduler: Arc<SchedulerService>,
    pub site_definitions: Arc<dyn SiteDefinitionProvider>,
    pub self_update: Arc<dyn SelfUpdateService>,
    pub web_actions: Arc<dyn WebActionService>,
}

/// Composition root for platform-neutral application use cases.
///
/// Native and Worker entrypoints construct this once with their adapters;
pub struct AppServices {
    pub library: Arc<LibraryService>,
    pub novel_actions: Arc<NovelActionService>,
    pub novel_settings: Arc<NovelSettingsService>,
    pub content: Arc<NovelContentService>,
    pub settings: Arc<SettingsService>,
    pub tag_colors: Arc<TagColorService>,
    pub jobs: Arc<JobService>,
    pub scheduler: Arc<SchedulerService>,
    pub site_definitions: Arc<dyn SiteDefinitionProvider>,
    pub self_update: Arc<dyn SelfUpdateService>,
    pub web_actions: Arc<dyn WebActionService>,
}

impl AppServices {
    pub fn new(deps: AppServiceDependencies) -> Self {
        Self {
            library: deps.library,
            novel_actions: deps.novel_actions,
            novel_settings: deps.novel_settings,
            content: deps.content,
            settings: deps.settings,
            tag_colors: deps.tag_colors,
            jobs: deps.jobs,
            scheduler: deps.scheduler,
            site_definitions: deps.site_definitions,
            self_update: deps.self_update,
            web_actions: deps.web_actions,
        }
    }
}