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
use narou_rs::application::settings::SettingsStore;
use narou_rs::downloader::Downloader;
use narou_rs::downloader::settings::{DownloaderSettings, SnapshotDownloaderSettings};
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::Result;
use narou_rs::platform::{
    AssetStore, Clock, HttpClient, NovelRepository, ObjectStore, RateLimiter, SystemClock,
};
use narou_rs::setting_core::SettingScope;
use serde::Deserialize;
use worker::{D1Database, Env, Queue};

use crate::bundled_sites::load_bundled_site_settings;
use crate::d1_repository::{D1FreezeStore, D1NovelRepository, D1SettingsStore, D1TagColorStore};
use crate::http::WorkerHttpClient;
use crate::ledger::{D1JobLedger, D1SchedulerCheckpoint};
use crate::rate_limiter::WorkerRateLimiter;
use crate::d1_object_store::D1ObjectStore;


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
    /// D1 ハンドル（資格情報ストアと設定スナップショットが使う）。
    db: Arc<D1Database>,
    /// 保存済み資格情報の復号鍵（secret 未設定なら `None`）。
    login_key: Option<[u8; 32]>,
    /// Shared subrequest counter for this invocation; the executor reads it
    /// at section boundaries via `WorkerBudget`.
    pub subrequests: crate::budget::SubrequestBudget,
}

/// 現在の保存先を読む。未設定・読み取り失敗は `None` (= D1 扱い)。
///
/// 保存先は `app_state('inv','object_backend')` の 1 行で決める。`s3` 以外は
/// すべて D1 として扱うので、移行前に戻すときはこの行を書き換えるだけでよい。
async fn read_object_backend(db: &D1Database) -> Option<String> {
    let statement = db
        .prepare("SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'object_backend'");
    let value = statement
        .first::<serde_json::Value>(Some("value_json"))
        .await
        .ok()
        .flatten()?;
    value.as_str().map(str::to_string)
}

/// オブジェクトの保存先を組み立てる。
///
/// 移行期間中は `app_state` の 1 行で D1 と S3 を切り替える。S3 が選ばれて
/// いて資格情報が欠けている場合は起動を失敗させ、黙って D1 へ落とさない
/// (fail-closed)。
async fn build_object_stores(
    env: &Env,
    db: &Arc<D1Database>,
) -> worker::Result<(Arc<dyn ObjectStore>, Arc<dyn AssetStore>)> {
    match read_object_backend(db).await.as_deref() {
        Some("s3") => {
            let store: Arc<crate::s3_object_store::S3ObjectStore> =
                Arc::new(crate::s3_object_store::S3ObjectStore::from_env(env)?);
            Ok((store.clone(), store))
        }
        _ => {
            let store: Arc<D1ObjectStore> = Arc::new(D1ObjectStore::new(db.clone()));
            Ok((store.clone(), store))
        }
    }
}

/// `app_state(scope='inv', key='section_hash_cache')` を読む。
///
/// native が Inventory に保存する強更新用ハッシュキャッシュ
/// (`SECTION_HASH_CACHE_NAME`) と同じキー名。`Downloader` の起動時
/// スナップショットとして渡すだけなので、行が無い・値が壊れている場合は
/// 空 map にする（キャッシュは fail-open で良い）。
async fn load_section_hash_cache(db: &D1Database) -> HashMap<String, HashMap<String, String>> {
    let row: Option<(Option<String>, Option<String>)> = db
        .prepare(
            "SELECT value_yaml, value_json FROM app_state WHERE scope = 'inv' AND key = 'section_hash_cache'",
        )
        .first(None)
        .await
        .unwrap_or_default();
    let Some((yaml, json)) = row else {
        return HashMap::new();
    };
    let payload = yaml.filter(|value| !value.trim().is_empty()).or(json);
    payload
        .and_then(|value| serde_yaml::from_str(&value).ok())
        .unwrap_or_default()
}

/// `app_state` の値を bool として解釈する。YAML/JSON の真偽値と
/// `"true"` / `"false"` 文字列を受け付け、それ以外は未設定として `None`。
fn setting_bool(value: &serde_yaml::Value) -> Option<bool> {
    match value {
        serde_yaml::Value::Bool(value) => Some(*value),
        serde_yaml::Value::String(value) => match value.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

impl WorkerRuntime {
    /// Build the runtime from the production bindings. Missing bindings fail
    /// composition (readiness 503), never an empty/partial service set.
    pub async fn build(env: &Env) -> worker::Result<Self> {
        let db = Arc::new(env.d1("DB")?);
        let subrequests = crate::budget::SubrequestBudget::new();
        let (objects, assets) = build_object_stores(env, &db).await?;
        let novels: Arc<dyn NovelRepository> = Arc::new(D1NovelRepository::new(db.clone()));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let freeze = Arc::new(D1FreezeStore::new(db.clone()));
        let http: Arc<dyn HttpClient> =
            Arc::new(WorkerHttpClient::new(subrequests.clone()));
        let rate_limiter: Arc<dyn RateLimiter> = Arc::new(
            WorkerRateLimiter::new(env, subrequests.clone())
                .map_err(|error| worker::Error::RustError(error.to_string()))?,
        );
        let site_settings = load_bundled_site_settings()
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let login_key = crate::d1_cookie_store::D1CookieStore::key_from_env(env);
        let ledger = Arc::new(D1JobLedger::new(db.clone(), clock.clone()));
        let checkpoint = D1SchedulerCheckpoint::new(db.clone());
        let queue = env.queue(JOB_QUEUE_BINDING)?;

        let services = services_from(
            novels.clone(),
            objects.clone(),
            freeze.clone(),
            db.clone(),
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
            subrequests,
            clock,
            site_settings,
            db,
            login_key,
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
    /// Bundled site settings are compiled at composition time. Downloader
    /// settings are a per-invocation snapshot of `app_state`: local scope
    /// (`update.strong` / `guard-spoiler` / `auto-add-tags` /
    /// `download.use-subdirectory`), global scope (`over18`), and the
    /// `inv`-scope section hash cache. Unset values keep the same defaults
    /// as native (`false` / `None` — `None` leaves the age-confirmation path
    /// intact and the executor reports `Blocked`).
    pub async fn new_downloader(&self) -> Result<Downloader> {
        let settings = self.downloader_settings().await?;
        let section_hash_cache = settings.load_section_hash_cache();
        let cookies = Arc::new(crate::d1_cookie_store::D1CookieStore::new(
            self.db.clone(),
            self.login_key,
        ));
        let downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            self.http.clone(),
            self.rate_limiter.clone(),
            self.novels.clone(),
            self.objects.clone(),
            self.assets.clone(),
            self.clock.clone(),
            self.site_settings.clone(),
            section_hash_cache,
            settings,
        )?;
        Ok(downloader.with_cookie_store(cookies))
    }

    /// `Downloader` に渡す設定スナップショットを D1 (`app_state`) から組み立てる。
    ///
    /// `DownloaderSettings` は同期 API だが D1 は非同期なので、呼び出し単位で
    /// 値を読み切り `SnapshotDownloaderSettings` に固めて渡す。
    async fn downloader_settings(&self) -> Result<Arc<dyn DownloaderSettings>> {
        const LOCAL_BOOL_KEYS: [&str; 4] = [
            "update.strong",
            "guard-spoiler",
            "auto-add-tags",
            "download.use-subdirectory",
        ];
        let store = D1SettingsStore::new(self.db.clone());
        let local_values = store.load(SettingScope::Local).await?;
        let local: HashMap<String, bool> = LOCAL_BOOL_KEYS
            .iter()
            .filter_map(|key| {
                local_values
                    .get(*key)
                    .and_then(setting_bool)
                    .map(|value| ((*key).to_string(), value))
            })
            .collect();
        let global_values = store.load(SettingScope::Global).await?;
        let over18 = global_values.get("over18").and_then(setting_bool);
        let section_hash_cache = load_section_hash_cache(&self.db).await;
        Ok(Arc::new(SnapshotDownloaderSettings::new(
            local,
            over18,
            section_hash_cache,
        )))
    }

    /// 保存済み設定の読み出し (D1)。`ConvertService` が `default.*` / `force.*`
    /// を適用するのに使う。
    pub fn settings_store(&self) -> Arc<dyn narou_rs::application::settings::SettingsStore> {
        Arc::new(D1SettingsStore::new(self.db.clone()))
    }

    /// 変換 (`ConvertService`) と HTTP 系が共有する平台能力。
    pub fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    pub fn rate_limiter(&self) -> Arc<dyn RateLimiter> {
        self.rate_limiter.clone()
    }

    pub fn objects(&self) -> Arc<dyn ObjectStore> {
        self.objects.clone()
    }

    pub fn assets(&self) -> Arc<dyn AssetStore> {
        self.assets.clone()
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
                JobClaim::Claimed {
                    execution_token,
                    ..
                } => {
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
                JobClaim::Busy { retry_after } => {
                    return Err(narou_rs::error::NarouError::Platform(format!(
                        "cannot block a job with a live execution lease; retry after {}s",
                        retry_after.as_secs()
                    )));
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

/// Application services plus direct access to the novel object store, for
/// handlers that stream content (download-time EPUB).
pub struct ReadServices {
    pub app: AppServices,
    pub objects: Arc<dyn ObjectStore>,
}

/// Build the application services (read-only API surface).
pub async fn build_services(env: &Env) -> worker::Result<AppServices> {
    Ok(build_read_services(env).await?.app)
}

pub async fn build_read_services(env: &Env) -> worker::Result<ReadServices> {
    let db = Arc::new(env.d1("DB")?);
    let (objects, _assets) = build_object_stores(env, &db).await?;
    let novels: Arc<dyn NovelRepository> = Arc::new(D1NovelRepository::new(db.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let freeze = Arc::new(D1FreezeStore::new(db.clone()));
    Ok(ReadServices {
        app: services_from(novels, objects.clone(), freeze, db, clock),
        objects,
    })
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
    env.queue(JOB_QUEUE_BINDING)?;
    env.durable_object("RATE_LIMITER")?;
    // Full composition validates bundled sites and all adapters.
    WorkerRuntime::build(env).await.map(|_| ())
}

#[derive(Debug, Deserialize)]
struct OneRow {
    one: i64,
}
