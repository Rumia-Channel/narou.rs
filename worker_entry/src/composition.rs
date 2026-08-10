use std::collections::HashMap;
use std::sync::Arc;

use chrono::SecondsFormat;
use narou_rs::application::{
    envelope_bytes, job_limits, JobClaim, JobLedgerStatus, JobPlan, JobQueue,
    AppServiceDependencies, AppServices, EmptySiteDefinitionProvider, EmptySiteTimezoneProvider,
    EmptyWebActionService, JobService, NoopSelfUpdateService, NovelActionService,
    NovelContentService, NovelSettingsService, SchedulerService, SettingsService, TagColorService,
    WorkerJobEnvelope,
};
use narou_rs::downloader::Downloader;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::Result;
use narou_rs::platform::{
    AssetStore, Clock, HttpClient, NovelRepository, ObjectStore, RateLimiter, SystemClock,
};
use serde::Deserialize;
use worker::{D1Database, Env, Queue};

use crate::bundled_sites::load_bundled_site_settings;
use crate::d1_repository::{D1FreezeStore, D1NovelRepository, D1SettingsStore, D1TagColorStore};
use crate::http::WorkerHttpClient;
use crate::ledger::{D1JobLedger, D1SchedulerCheckpoint};
use crate::rate_limiter::WorkerRateLimiter;
use crate::wasabi::{WasabiConfig, WasabiObjectStore};


/// Result of the complete ledger + Queue producer operation for one plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub job_id: String,
    pub sent: bool,
    pub blocked: Option<String>,
}
/// Queue producer binding that carries version-2 job envelopes.
pub const JOB_QUEUE_BINDING: &str = "NAROU_JOBS";

/// Everything the Worker event handlers need: the application services plus
/// the Phase 8 queue/ledger/checkpoint/rate-limiter wiring.
///
/// `WorkerRuntime` holds *capabilities* (cheap `Arc`s and values). The
/// mutable `Downloader` is deliberately not stored here: each invocation
/// builds its own via [`Self::new_downloader`], so a fetch, a cron, and a
/// queue batch running concurrently in one isolate never share a mutable
/// downloader.
pub struct WorkerRuntime {
    pub services: AppServices,
    pub ledger: Arc<D1JobLedger>,
    pub checkpoint: D1SchedulerCheckpoint,
    pub queue: Queue,
    pub freeze: Arc<D1FreezeStore>,
    pub novels: Arc<dyn NovelRepository>,
    http: Arc<dyn HttpClient>,
    rate_limiter: Arc<dyn RateLimiter>,
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    clock: Arc<dyn Clock>,
    site_settings: Vec<SiteSetting>,
}

impl WorkerRuntime {
    /// Build the runtime from the production bindings. Missing bindings or
    /// an invalid Wasabi configuration fail composition (readiness 503),
    /// never an empty/partial service set.
    pub fn build(env: &Env) -> worker::Result<Self> {
        let db = Arc::new(env.d1("DB")?);
        let config = WasabiConfig::from_env(env)
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let store = Arc::new(WasabiObjectStore::new(config));
        let objects: Arc<dyn ObjectStore> = store.clone();
        let assets: Arc<dyn AssetStore> = store;
        let novels: Arc<dyn NovelRepository> = Arc::new(D1NovelRepository::new(db.clone()));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let freeze = Arc::new(D1FreezeStore::new(db.clone()));
        let http: Arc<dyn HttpClient> = Arc::new(WorkerHttpClient::new());
        let rate_limiter: Arc<dyn RateLimiter> = Arc::new(
            WorkerRateLimiter::new(env)
                .map_err(|error| worker::Error::RustError(error.to_string()))?,
        );
        let site_settings = load_bundled_site_settings()
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let ledger = Arc::new(D1JobLedger::new(db.clone(), clock.clone()));
        let checkpoint = D1SchedulerCheckpoint::new(db.clone());
        let queue = env.queue(JOB_QUEUE_BINDING)?;

        let services = services_from(
            novels.clone(),
            objects.clone(),
            freeze.clone(),
            db,
            clock.clone(),
        );
        Ok(Self {
            services,
            ledger,
            checkpoint,
            queue,
            freeze,
            novels,
            http,
            rate_limiter,
            objects,
            assets,
            clock,
            site_settings,
        })
    }

    /// Current UTC time in canonical RFC3339 form.
    pub fn now_rfc3339(&self) -> String {
        self.clock
            .now_utc()
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    /// A fresh, exclusively-owned shared Downloader for one event invocation.
    ///
    /// Bundled site settings are compiled at composition time; the section
    /// hash cache is the empty worker default (no Inventory on this
    /// platform — reruns overwrite persisted sections idempotently).
    pub fn new_downloader(&self) -> Result<Downloader> {
        Downloader::with_platform_and_storage_and_settings(
            self.http.clone(),
            self.rate_limiter.clone(),
            self.novels.clone(),
            self.objects.clone(),
            self.assets.clone(),
            self.clock.clone(),
            self.site_settings.clone(),
            HashMap::new(),
        )
    }

    /// Enqueue one plan through the ledger and Queue binding as one operation.
    /// Pending/retryable rows are sent; a live running row is not duplicated.
    pub async fn enqueue_plan(&self, plan: JobPlan) -> Result<DispatchOutcome> {
        let queued = self.ledger.enqueue(plan.clone()).await?;
        let view = self
            .ledger
            .get(&queued.job_id)
            .await?
            .ok_or_else(|| {
                narou_rs::error::NarouError::Platform("queued job disappeared".to_string())
            })?;
        let unsupported = if !plan.kind.is_worker_executable() {
            Some(format!(
                "unsupported job kind {:?} on the worker (no subprocess support)",
                plan.kind
            ))
        } else if plan.target == narou_rs::application::JobTarget::All {
            Some("auto-update must be planned into discrete per-novel jobs".to_string())
        } else {
            None
        };
        let oversized = envelope_bytes(&WorkerJobEnvelope::v2(
            queued.job_id.clone(),
            queued.job.clone(),
        ))
        .map(|size| size > job_limits::MAX_ENVELOPE_BYTES)
        .unwrap_or(true);
        let blocked_reason = unsupported.or_else(|| {
            oversized.then(|| {
                format!(
                    "envelope exceeds queue payload limit (max {} bytes)",
                    job_limits::MAX_ENVELOPE_BYTES
                )
            })
        });
        if let Some(reason) = blocked_reason {
            match self.ledger.claim(&queued.job_id).await? {
                JobClaim::Claimed { execution_token } => {
                    self.ledger
                        .mark_terminal(
                            &queued.job_id,
                            &execution_token,
                            JobLedgerStatus::Blocked,
                            Some(&reason),
                        )
                        .await?;
                }
                JobClaim::AlreadyTerminal | JobClaim::Unknown => {}
                JobClaim::Busy => {
                    return Err(narou_rs::error::NarouError::Platform(
                        "cannot block a job with a live execution lease".to_string(),
                    ));
                }
            }
            return Ok(DispatchOutcome {
                job_id: queued.job_id.as_str().to_string(),
                sent: false,
                blocked: Some(reason),
            });
        }
        if !matches!(
            view.status,
            narou_rs::application::JobLedgerStatus::Pending
                | narou_rs::application::JobLedgerStatus::Retryable
        ) {
            return Ok(DispatchOutcome {
                job_id: queued.job_id.as_str().to_string(),
                sent: false,
                blocked: None,
            });
        }
        let envelope = WorkerJobEnvelope::v2(queued.job_id.clone(), queued.job);
        self.queue.send(&envelope).await.map_err(|error| {
            narou_rs::error::NarouError::Platform(format!("queue send failed: {error}"))
        })?;
        Ok(DispatchOutcome {
            job_id: envelope.job_id.as_str().to_string(),
            sent: true,
            blocked: None,
        })
    }

    /// Dispatch a bounded page, attempting every plan before returning a
    /// partial failure. Failed sends remain active in the ledger for repair.
    pub async fn enqueue_batch(&self, plans: &[JobPlan]) -> Result<Vec<DispatchOutcome>> {
        let mut outcomes = Vec::with_capacity(plans.len());
        let mut failures = Vec::new();
        for plan in plans {
            match self.enqueue_plan(plan.clone()).await {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => failures.push(error.to_string()),
            }
        }
        if failures.is_empty() {
            Ok(outcomes)
        } else {
            Err(narou_rs::error::NarouError::Platform(format!(
                "queue page partially failed ({}): {}",
                failures.len(),
                failures.join("; ")
            )))
        }
    }
}

/// Build the application services (read-only API surface).
pub fn build_services(env: &Env) -> worker::Result<AppServices> {
    let db = Arc::new(env.d1("DB")?);
    let config = WasabiConfig::from_env(env)
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    let store = Arc::new(WasabiObjectStore::new(config));
    let objects: Arc<dyn ObjectStore> = store;
    let novels: Arc<dyn NovelRepository> = Arc::new(D1NovelRepository::new(db.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let freeze = Arc::new(D1FreezeStore::new(db.clone()));
    Ok(services_from(novels, objects, freeze, db, clock))
}

fn services_from(
    novels: Arc<dyn NovelRepository>,
    objects: Arc<dyn ObjectStore>,
    freeze: Arc<D1FreezeStore>,
    db: Arc<D1Database>,
    clock: Arc<dyn Clock>,
) -> AppServices {
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

    AppServices::new(AppServiceDependencies {
        library,
        novel_actions,
        novel_settings: Arc::new(NovelSettingsService::new(novels.clone(), objects.clone())),
        content: Arc::new(NovelContentService::new(novels, objects)),
        settings: Arc::new(SettingsService::new(Arc::new(D1SettingsStore::new(
            db.clone(),
        )))),
        tag_colors: Arc::new(TagColorService::new(Arc::new(D1TagColorStore::new(
            db,
        )))),
        jobs: Arc::new(JobService),
        scheduler: Arc::new(SchedulerService::new(clock)),
        site_definitions: Arc::new(EmptySiteDefinitionProvider),
        self_update: Arc::new(NoopSelfUpdateService),
        web_actions: Arc::new(EmptyWebActionService),
    })
}

/// Readiness probe: every required binding and configuration must be real.
///
/// - D1 answers `SELECT 1`
/// - Wasabi endpoint/bucket/region and credentials are present and the
///   endpoint parses as a URL
/// - `NAROU_JOBS` queue producer and `RATE_LIMITER` Durable Object
///   namespace are bound
/// - full composition (bundled site definitions parse and compile)
///
/// Nothing here performs network I/O; a missing binding or config fails
/// with `Err`, which the handler maps to 503.
pub async fn check_ready(env: &Env) -> worker::Result<()> {
    let db = env.d1("DB")?;
    let row = db
        .prepare("SELECT 1 AS one")
        .first::<OneRow>(None)
        .await
        .map_err(|error| worker::Error::RustError(format!("D1 readiness failed: {error}")))?;
    match row {
        Some(row) if row.one == 1 => {}
        _ => {
            return Err(worker::Error::RustError(
                "D1 readiness returned no row".to_string(),
            ));
        }
    }
    let config = WasabiConfig::from_env(env)
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    if url::Url::parse(&config.endpoint).is_err() {
        return Err(worker::Error::RustError(format!(
            "Wasabi endpoint is not a valid URL: {:?}",
            config.endpoint
        )));
    }
    env.queue(JOB_QUEUE_BINDING)?;
    env.durable_object("RATE_LIMITER")?;
    // Full composition validates bundled sites and all adapters.
    WorkerRuntime::build(env).map(|_| ())
}

#[derive(Debug, Deserialize)]
struct OneRow {
    one: i64,
}
