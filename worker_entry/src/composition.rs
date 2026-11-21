use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::SecondsFormat;
use narou_rs::application::acquisition_lanes::LANE_COUNT;
use narou_rs::application::settings::SettingsStore;
use narou_rs::application::{
    AppServiceDependencies, AppServices,
    EmptyWebActionService, FreezeStore, JobClaim, JobId, JobKind, JobLedgerStatus, JobPlan, JobQueue, JobService,
    JobTarget, NoopSelfUpdateService, NovelActionService, NovelContentService,
    NovelSettingsService, SchedulerService, SettingsService, TagColorService, WorkerJobEnvelope,
    envelope_bytes, job_limits,
};
use narou_rs::downloader::Downloader;
use narou_rs::downloader::settings::{DownloaderSettings, SnapshotDownloaderSettings};
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, Clock, HttpClient, NovelFilter, NovelRepository, ObjectStore, S3Store,
    SplitStore, SystemClock,
};
use narou_rs::setting_core::SettingScope;
use serde::Deserialize;
use worker::{Env, Queue, console_log};

use crate::db_handle::DbHandle;
use crate::d1_object_store::D1ObjectStore;
use crate::d1_repository::{D1FreezeStore, D1NovelRepository, D1SettingsStore, D1TagColorStore};
use crate::http::WorkerHttpClient;
use crate::ledger::{D1JobLedger, D1SchedulerCheckpoint};
use crate::push_hub::PushHubClient;
use crate::rate_limiter::WorkerRateLimiter;
use crate::worker_role::WorkerRole;

#[path = "asset_backend.rs"]
mod asset_backend;
use asset_backend::AssetBackend;

#[path = "reader_storage.rs"]
mod reader_storage;
use reader_storage::ReaderStores;

/// Result of the complete ledger + Queue producer operation for one plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub job_id: String,
    pub sent: bool,
    pub blocked: Option<String>,
}
/// Queue producer binding that carries version-2 job envelopes.
pub const JOB_QUEUE_BINDING: &str = "NAROU_JOBS";
const CONVERT_QUEUE_BINDING: &str = "CONVERT_QUEUE";
/// Producer binding prefix of the four acquisition lane queues.
pub const ACQUISITION_QUEUE_BINDING_PREFIX: &str = "ACQUISITION_QUEUE_";
/// Service binding of the writer script. 配信 Worker は捕捉した変更系を
/// ここへ転送する (同一ドメインの更新 Worker)。
pub const WRITER_SERVICE_BINDING: &str = "NAROU_WRITER";

/// Only the current job's novel hashes await persistence; never a library snapshot.
pub type PendingSectionHashCache =
    Arc<parking_lot::Mutex<Option<HashMap<String, HashMap<String, String>>>>>;


/// Cloudflare Queues `sendBatch` accepts at most 100 messages. Page dispatch
/// chunks sends to this bound and falls back to per-message sends when a
/// chunk fails, so one rejected envelope cannot fail the rest of the page.
const QUEUE_SEND_BATCH_SIZE: usize = 100;

/// Everything the Worker event handlers need: the application services plus
/// the Phase 8 queue/ledger/checkpoint/rate-limiter wiring.
///
/// `WorkerRuntime` holds *capabilities* (cheap `Arc`s and values). The
/// mutable `Downloader` is deliberately not stored here: each invocation
/// builds its own via [`Self::new_downloader`], so a fetch, a cron, and a
/// queue batch running concurrently in one isolate never share a mutable
/// downloader.
pub struct WorkerRuntime {
    /// UI 読み取り系のサービス (`DbHandle::ui` = first-unconstrained
    /// セッション + isolate キャッシュありのストア)。GET 系はこちらだけを
    /// 使い、read replica へ逃がす。
    pub services: AppServices,
    /// 更新系のサービス (primary 直行 + キャッシュを介さないストア)。
    /// 書き込みを伴うハンドラは、判断材料の読み取りも含めてこちらを使う
    /// — `services` 側はキャッシュ済み・replica 遅延のスナップショットを
    /// 返し得るため、read-modify-write には使えない。
    pub write_services: AppServices,
    pub ledger: Arc<D1JobLedger>,
    pub checkpoint: D1SchedulerCheckpoint,
    /// Producer handles of the four acquisition lane queues.
    acquisition_lane_queues: Vec<Queue>,
    /// Conversion producer for writer and acquisition lanes; reader has none.
    convert_queue: Option<Queue>,
    /// Live event client bound to this invocation's subrequest budget.
    push: PushHubClient,
    pub freeze: Arc<D1FreezeStore>,
    pub novels: Arc<dyn NovelRepository>,
    http: Arc<dyn HttpClient>,
    rate_limiter: Arc<WorkerRateLimiter>,
    /// UI 読み取り系のオブジェクトストア (セッション / キャッシュ済みの
    /// asset_backend 解決)。
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    /// 更新系のオブジェクトストア (primary 直行 + キャッシュを介さない
    /// asset_backend 解決)。オブジェクトの削除・PUT とその直後の再読み込み
    /// (read-your-writes) はこちらを使う。
    write_objects: Arc<dyn ObjectStore>,
    pub clock: Arc<dyn Clock>,
    site_settings: Arc<Vec<SiteSetting>>,
    /// Job snapshots bypass the cross-isolate YAML TTL; UI snapshots do not.
    site_settings_fresh: bool,
    /// 更新系の D1 ハンドル。常に primary 直行 — read replica の遅延中に
    /// 書き込み判断の読み取りを行わせない。
    write_db: DbHandle,
    /// 保存済みログイン資格情報。Downloader・HTTP クライアント・管理 API が
    /// 同じ実体を共有する。`D1CookieStore` は読み取り自体が旧形式の畳み直し
    /// 書き込みを含み得るため、常に `write_db` (primary) に結び付ける。
    cookie_store: Arc<crate::d1_cookie_store::D1CookieStore>,
    /// Shared subrequest counter for this invocation; the executor reads it
    /// at section boundaries via `WorkerBudget`.
    pub subrequests: crate::budget::SubrequestBudget,
    stage_store: Option<Arc<S3Store>>,
    /// Invocation-local hashes, flushed to per-novel D1 rows after the job.
    pub pending_section_hash_cache: PendingSectionHashCache,
}

/// `WorkerRuntime::build_with` が行う重い読み込みのプロファイル。
///
/// Both profiles share compiled site rules. `Ui` retains replica-backed
/// content reads; mutation inputs and Downloader settings use primary D1.
enum CompositionProfile {
    Job,
    Ui,
}

/// isolate ごとの `asset_backend` キャッシュ。設定の切替は TTL で
/// 他 isolate に伝わる。切替直後に「D1 に書き・S3 から読む」混在が起きる
/// 可能性があるため、他キャッシュ (30 秒) より短い 5 秒にしておく
/// (`/api/storage/mode` は 501 で Worker からは書き換わらないが、手動 SQL
/// などで切り替えた直後のずれを最小化する)。
static ASSET_BACKEND_CACHE: std::sync::LazyLock<crate::isolate_cache::TtlMap<AssetBackend>> =
    std::sync::LazyLock::new(|| crate::isolate_cache::TtlMap::new(5_000.0, 1024, |_| 16));

/// 挿絵の保存先を読む。D1 の TEXT は JSON を含む文字列なので明示的に
/// decode する。旧構成の raw TEXT `d1` / `s3` も明示的に受け付ける。
/// 読み取り失敗・不正値は起動失敗で、D1 に落とさない。
///
/// 行が無い standalone の旧構成だけは D1 とする。CI 構成では後段の
/// `NAROU_REQUIRE_S3=true` が D1・未設定を含む有効な選択を S3 に解決する。
///
/// `fresh = true` (更新系の組立) では isolate キャッシュを読まない。
/// 保存先の読み違いはオブジェクトを誤ったバックエンドへ書く直結の問題
/// なので、書き込み側はキャッシュ値を信じない。
async fn read_asset_backend(db: &DbHandle, fresh: bool) -> worker::Result<AssetBackend> {
    if !fresh && let Some(cached) = ASSET_BACKEND_CACHE.get("asset_backend") {
        return Ok(cached);
    }
    let statement = db
        .prepare("SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'asset_backend'");
    let row = statement
        .first::<asset_backend::AssetBackendRow>(None)
        .await;
    let resolved = asset_backend::parse_asset_backend(row.map_err(|_| ()))
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    ASSET_BACKEND_CACHE.put("asset_backend", resolved);
    Ok(resolved)
}

/// Read the non-secret deployment guard without treating invalid bindings as
/// absent. Only an undefined binding preserves the standalone legacy policy.
pub(crate) fn required_s3(env: &Env) -> worker::Result<bool> {
    let value = js_sys::Reflect::get(env, &wasm_bindgen::JsValue::from_str("NAROU_REQUIRE_S3"))
        .map_err(|_| worker::Error::RustError("failed to read NAROU_REQUIRE_S3".to_string()))?;
    let value = if value.is_undefined() {
        None
    } else {
        Some(value.as_string().ok_or_else(|| {
            worker::Error::RustError(asset_backend::BackendError::InvalidRequirement.to_string())
        })?)
    };
    asset_backend::required_s3(value.as_deref())
        .map_err(|error| worker::Error::RustError(error.to_string()))
}

struct WorkerObjectStores {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
    s3_illustrations: Option<Arc<S3Store>>,
    stage_store: Option<Arc<S3Store>>,
}

/// オブジェクトの保存先を組み立てる。
///
/// 本文は未設定なら S3 を選び、既存の D1 本文は migrating で保持する。
/// `NAROU_REQUIRE_S3=true` は挿絵・本文とも S3 を必須とする。
/// S3 設定に失敗した場合はマーカーを変えず、黙って D1 へ落とさない。
///
/// `reader = true` は配信 Worker (読み取り専用) の組立。sources への
/// 書き込み (本文マーカーの確定・属性の backfill) は一切行わず、
/// 配信は private staging だけを書ける保存層で組み立てる。
async fn build_object_stores(
    env: &Env,
    db: &DbHandle,
    subrequests: crate::budget::SubrequestBudget,
    fresh: bool,
    reader: bool,
) -> worker::Result<WorkerObjectStores> {
    let required = required_s3(env)?;
    let backend = read_asset_backend(db, fresh).await?.resolve(required);
    // Initialize the S3 default only after S3 configuration has been resolved.
    // Existing D1 bodies stay readable in migrating; new writes use S3.
    let primary_db = db.primary_view();
    let content_marker = crate::content_store::stored_backend(&primary_db).await
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    let content_s3 = required || content_marker != Some(crate::content_store::ContentBackend::D1);
    let d1: Arc<D1ObjectStore> = Arc::new(D1ObjectStore::new(db.clone()));
    let s3 = if backend == AssetBackend::S3 || content_s3 {
        Some(crate::s3_store::store_from_env(env, subrequests).await?)
    } else {
        None
    };
    let content_backend = match content_marker {
        Some(mode) if mode != crate::content_store::ContentBackend::D1 || !required => mode,
        // 配信はマーカーを確定しない。writer が適用する遷移を推測して
        // 同じ保存先 (S3 本文 + 旧 D1 本文) から読むだけにする。
        _ if reader => crate::content_store::inferred_backend(&primary_db).await
            .map_err(|error| worker::Error::RustError(error.to_string()))?,
        _ => crate::content_store::initialize_s3_default(&primary_db, required).await
            .map_err(|error| worker::Error::RustError(error.to_string()))?,
    };
    let s3_handle = if backend == AssetBackend::S3 { s3.clone() } else { None };
    let illustrations: (Arc<dyn ObjectStore>, Arc<dyn AssetStore>) = match &s3_handle {
        Some(store) => (store.clone(), store.clone()),
        None => (d1.clone(), d1.clone()),
    };
    let stage_store = s3.clone();
    let primary: (Arc<dyn ObjectStore>, Arc<dyn AssetStore>) = match (content_backend, s3) {
        (crate::content_store::ContentBackend::D1, _) => (d1.clone(), d1),
        (_, Some(store)) => {
            let content = Arc::new(crate::content_store::ContentStore::new(db.clone(), store.clone(), store));
            (content.clone(), content)
        }
        _ => return Err(worker::Error::RustError("content S3 storage is unavailable".into())),
    };
    let mut split = SplitStore::new(primary, illustrations);
    if let Some(store) = &stage_store {
        split = split.with_staging((store.clone(), store.clone()));
    }
    let split = Arc::new(split);
    let refs=Arc::new(crate::content_store::TransferReferenceStore::new(db.primary_view(),split.clone(),split,Arc::new(D1ObjectStore::new(db.primary_view())),stage_store.clone().map(|store|store as Arc<dyn AssetStore>)));
    let (objects, assets): (Arc<dyn ObjectStore>, Arc<dyn AssetStore>) = if reader {
        // 配信は source を書き換えない。private staging (S3) だけを書ける。
        let staging = stage_store
            .clone()
            .map(|store| reader_storage::StagingTarget::new(store.clone(), store));
        // Lease 行だけは共有 D1 の durable mutation なので、配信 isolate
        // 自身ではなく writer の認証済み制御路へ委譲する。
        let fence = Some(crate::reader_control::remote_access(env).await?);
        let guarded = ReaderStores::new(refs.clone(), refs, staging, fence);
        (guarded.objects(), guarded.assets())
    } else {
        (refs.clone(), refs)
    };
    Ok(WorkerObjectStores {
        objects,
        assets,
        s3_illustrations: s3_handle,
        stage_store,
    })
}

/// Hash hints belong to the primary generation admitted by the job's origin.
type SectionHashes = HashMap<String, String>;

async fn load_section_hash_cache(db: &DbHandle, id: i64, fence: &narou_rs::platform::WriteFence) -> Result<HashMap<String, SectionHashes>> {
    #[derive(serde::Deserialize)]struct HashRow{digests_json:String}
    let db=db.primary_view();let store=D1ObjectStore::new(db.clone());let token=crate::content_store::nonce()?;
    let check=db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=? AND prefix=?)").bind(&[(id as f64).into(),fence.prefix.as_ref().trim_end_matches('/').into()]).map_err(|e|NarouError::Platform(e.to_string()))?;
    let select=db.prepare("SELECT digests_json FROM novel_section_hashes WHERE novel_id=?").bind(&[(id as f64).into()]).map_err(|e|NarouError::Platform(e.to_string()))?;
    let rows=db.batch(vec![store.open_guard(fence,&token)?,check,select,store.finish_publication_guard(&token)?]).await.map_err(|e|NarouError::Platform(e.to_string()))?;
    crate::d1_repository::ensure_batch_success(&rows)?;
    let value=rows[2].results::<HashRow>().map_err(|e|NarouError::Platform(e.to_string()))?.into_iter().next();
    let hashes=value.and_then(|row|serde_json::from_str(&row.digests_json).ok()).unwrap_or_default();
    Ok(HashMap::from([(id.to_string(),hashes)]))
}

async fn persist_section_hash_cache(db: &DbHandle, pending: HashMap<String, SectionHashes>, fence: &narou_rs::platform::WriteFence) -> Result<()> {
    let db=db.primary_view();let store=D1ObjectStore::new(db.clone());let token=crate::content_store::nonce()?;
    let mut statements=vec![store.open_guard(fence,&token)?];
    for (id, hashes) in pending {
        let json=serde_json::to_string(&hashes).map_err(|error|NarouError::Platform(error.to_string()))?;
        statements.push(db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=? AND prefix=?)")
            .bind(&[id.as_str().into(),fence.prefix.as_ref().trim_end_matches('/').into()]).map_err(|error|NarouError::Platform(error.to_string()))?);
        statements.push(db.prepare("INSERT INTO novel_section_hashes(novel_id,digests_json) VALUES(?,?) ON CONFLICT(novel_id) DO UPDATE SET digests_json=excluded.digests_json")
            .bind(&[id.into(),json.into()]).map_err(|error|NarouError::Platform(error.to_string()))?);
    }
    statements.push(store.finish_publication_guard(&token)?);
    let results=db.batch(statements).await.map_err(|error|NarouError::Platform(error.to_string()))?;
    crate::d1_repository::ensure_batch_success(&results)
}

impl WorkerRuntime {
    /// Build the runtime from the production bindings. Missing bindings fail
    /// composition (readiness 503), never an empty/partial service set.
    ///
    /// ジョブ実行系 (consumer / executor / scheduler / ledger) 用。
    /// すべてのクエリは primary 直行になる `DbHandle::Primary` で組み立てる
    /// — claim の UPDATE やリトライ判定が read-your-writes を前提にするため、
    /// ここはセッションを通さない。
    pub async fn build(env: &Env) -> worker::Result<Self> {
        let db = DbHandle::primary(Arc::new(env.d1("DB")?));
        Self::build_with(env, db.clone(), db, CompositionProfile::Job).await
    }

    /// UI (fetch) 経路用のビルド。D1 は `first-unconstrained` セッション経由
    /// (`DbHandle::ui`) になり、読み取りは read replica に逃がせる。
    /// セッション生成に失敗した場合 (`wrangler dev` のローカル D1 など
    /// セッション API 未対応の環境) は primary にフォールバックする。
    ///
    /// UI composition reuses the bounded compiled-site cache. Section hashes
    /// are fetched only when `new_downloader` resolves its one target novel.
    pub async fn build_ui(env: &Env) -> worker::Result<Self> {
        let db = DbHandle::ui(Arc::new(env.d1("DB")?));
        let write_db = DbHandle::primary(Arc::new(env.d1("DB")?));
        Self::build_with(env, db, write_db, CompositionProfile::Ui).await
    }

    /// `db` (読み取り系) と `write_db` (更新系、常に primary 直行) の 2 系統の
    /// ハンドルをストアへ配線する共通ビルド。ジョブ経路では両者に同じ
    /// primary ハンドルが渡る。
    async fn build_with(
        env: &Env,
        db: DbHandle,
        write_db: DbHandle,
        profile: CompositionProfile,
    ) -> worker::Result<Self> {
        // 配信 Worker では source を変更しない組立に落とす。
        let role = crate::worker_role::role(env)?;
        // The lane index is validated here so a misconfigured lane script
        // fails composition instead of consuming another lane's deliveries.
        let _lane = crate::worker_role::acquisition_lane(env)?;
        let reader = role == WorkerRole::Reader;
        let subrequests = crate::budget::SubrequestBudget::new();
        let WorkerObjectStores {
            objects: write_objects, assets: write_assets, stage_store, ..
        } = build_object_stores(env, &write_db, subrequests.clone(), true, reader).await?;
        let (objects, assets) = match profile {
            CompositionProfile::Job => (write_objects.clone(), write_assets),
            CompositionProfile::Ui => {
                let WorkerObjectStores { objects, assets, .. } =
                    build_object_stores(env, &db, subrequests.clone(), false, reader).await?;
                (objects, assets)
            }
        };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        // 一覧・描画系の凍結集合は isolate キャッシュ込みの読み取り系、
        // `runtime.freeze` (freeze 判定/更新前の読み取り) はキャッシュを
        // 介さない更新系に分ける。
        let read_freeze = Arc::new(D1FreezeStore::new(db.clone()));
        let freeze = Arc::new(D1FreezeStore::for_writes(write_db.clone()));
        let cookie_store: Arc<crate::d1_cookie_store::D1CookieStore> =
            Arc::new(crate::d1_cookie_store::D1CookieStore::new(
                write_db.clone(),
                crate::d1_cookie_store::D1CookieStore::key_from_env(env).await,
                // 旧ホストキーの畳み (key_site_settings) が Worker runtime と
                // 同じ effective 定義 (bundle + ユーザー、version gate 済み) を
                // 読めるように注入する。
                write_objects.clone(),
            ));
        // ジョブの設定は primary から現在値を読み、UI の GET はキャッシュを維持する。
        let local_store: Arc<dyn SettingsStore> = Arc::new(match profile {
            CompositionProfile::Job => D1SettingsStore::for_writes(write_db.clone()),
            CompositionProfile::Ui => D1SettingsStore::new(db.clone()),
        });
        let local_values = local_store
            .load(SettingScope::Local)
            .await
            .unwrap_or_default();
        // User-Agent は保存済み設定から解決する (native の `with_user_agent` と同じ規則)。
        // 送らないと多くのサイトが 403 を返すため、必ず何かしら送る。
        let saved_user_agent = local_values
            .get("user-agent")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let user_agent = narou_rs::downloader::resolve_user_agent(None, saved_user_agent);
        // 外部リレーは `NAROU_RELAY_ENDPOINT` と
        // `NAROU_RELAY_KEY` が揃ったときだけ有効にする。公開先は https か、
        // ループバックの http だけを許可する (http.rs の `with_relay` が検証し、
        // それ以外は fail closed で無効化する)。
        let relay_base = crate::secrets::value(env, "NAROU_RELAY_ENDPOINT").await;
        let relay_token = crate::secrets::value(env, "NAROU_RELAY_KEY").await;
        // `download.relay-sites` は local 設定から、リレー設定と同じ
        // 組み立てタイミングで読む (`narou setting` / Web UI の変更は次の
        // ジョブで反映される)。
        let relay_sites = local_values
            .get("download.relay-sites")
            .and_then(|value| value.as_str());
        let mut http_client =
            WorkerHttpClient::new(subrequests.clone()).with_user_agent(user_agent);
        if let (Some(base), Some(token)) = (relay_base, relay_token) {
            http_client = http_client
                .with_relay(base, token)
                .with_relay_sites(relay_sites);
        }
        let http: Arc<dyn HttpClient> =
            Arc::new(http_client.with_cookie_store(cookie_store.clone()));
        let rate_limiter = Arc::new(
            WorkerRateLimiter::new(env, subrequests.clone())
                .map_err(|error| worker::Error::RustError(error.to_string()))?,
        );
        let site_settings_fresh = matches!(profile, CompositionProfile::Job);
        let site_settings = if site_settings_fresh {
            crate::bundled_sites::load_site_settings_uncached(&write_objects).await
        } else {
            crate::bundled_sites::load_site_settings(&write_objects).await
        }.map_err(|error| worker::Error::RustError(error.to_string()))?;
        crate::webui::metadata::initialize_attributes(env).await?;
        let novels: Arc<dyn NovelRepository> =
            Arc::new(D1NovelRepository::new(db.clone()).with_sites(site_settings.clone()).with_objects(write_objects.clone()));
        let write_novels: Arc<dyn NovelRepository> =
            Arc::new(D1NovelRepository::new(write_db.clone()).with_sites(site_settings.clone()).with_objects(write_objects.clone()));
        let site_rules = Arc::new(
            narou_rs::application::site_definitions::SiteSettingsSnapshot::new(site_settings.clone()),
        );
        // 台帳・チェックポイントは更新系 (claim/状態遷移の read-your-writes)
        // なので常に primary 直行。`/api/queue/*` の読み取りは webui/queue.rs
        // 側がセッションハンドルを使う。
        let ledger = Arc::new(D1JobLedger::new(write_db.clone(), clock.clone()));
        let checkpoint = D1SchedulerCheckpoint::new(write_db.clone());
        // 取得レーンも取得後の変換を writer のキューへ送る。実行は writer だけ。
        // キュー名は配備の識別子なので、空のままでは組立させない。
        let convert_queue = match role {
            WorkerRole::Writer | WorkerRole::Acquisition => {
                let name = env.var("CONVERT_QUEUE_NAME")?.to_string();
                if name.is_empty() {
                    return Err(worker::Error::RustError("CONVERT_QUEUE_NAME is empty".into()));
                }
                Some(env.queue(CONVERT_QUEUE_BINDING)?)
            }
            WorkerRole::Reader => None,
        };
        // 取得レーンの送信先。4 本すべての binding が揃っている脚本だけが
        // 組立できる (binding が欠けた脚本は、配信できない job を受け取る
        // ので fail closed する)。
        let mut acquisition_lane_queues: Vec<Queue> = Vec::with_capacity(LANE_COUNT as usize);
        for index in 0..LANE_COUNT {
            let binding = format!("{ACQUISITION_QUEUE_BINDING_PREFIX}{index}");
            let queue = env.queue(&binding).map_err(|error| {
                worker::Error::RustError(format!(
                    "acquisition lane queue binding {binding} is missing: {error}"
                ))
            })?;
            acquisition_lane_queues.push(queue);
        }
        let push = PushHubClient::new(env, subrequests.clone());

        let services = services_from(ServiceDependencies {
            novels: novels.clone(),
            objects: objects.clone(),
            freeze: read_freeze,
            settings: Arc::new(D1SettingsStore::new(db.clone())),
            tag_colors: Arc::new(D1TagColorStore::new(db.clone())),
            clock: clock.clone(),
            site_rules: site_rules.clone(),
            db,
            env,
        });
        let write_services = services_from(ServiceDependencies {
            novels: write_novels.clone(),
            objects: write_objects.clone(),
            freeze: freeze.clone(),
            settings: Arc::new(D1SettingsStore::for_writes(write_db.clone())),
            tag_colors: Arc::new(D1TagColorStore::new(write_db.clone())),
            clock: clock.clone(),
            site_rules,
            db: write_db.clone(),
            env,
        });
        Ok(Self {
            services,
            write_services,
            ledger,
            checkpoint,
            acquisition_lane_queues,
            convert_queue,
            push,
            freeze,
            novels: write_novels,
            http,
            rate_limiter,
            objects,
            assets,
            write_objects,
            subrequests,
            stage_store,
            clock,
            site_settings,
            site_settings_fresh,
            write_db,
            cookie_store,
            pending_section_hash_cache: Arc::new(parking_lot::Mutex::new(None)),
        })
    }

    /// Current UTC time in canonical RFC3339 form.
    pub fn now_rfc3339(&self) -> String {
        self.clock
            .now_utc()
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    /// The producer handle of one acquisition lane queue.
    pub fn lane_queue(&self, lane: u8) -> Option<&Queue> {
        self.acquisition_lane_queues.get(lane as usize)
    }

    /// The live event client of this invocation.
    pub fn push(&self) -> &PushHubClient {
        &self.push
    }

    /// Assign ready acquisition rows to free lanes.
    ///
    /// The dispatcher only moves durable rows into a lane queue; it never
    /// executes an acquisition itself.
    pub async fn dispatch_acquisition(&self) -> Result<()> {
        crate::acquisition::dispatch(self).await
    }

    /// Admission uses current primary definitions, sharing the fresh job
    /// snapshot when one was already loaded during composition.
    pub(crate) async fn effective_site_settings(&self) -> Result<Arc<Vec<SiteSetting>>> {
        if self.site_settings_fresh {
            return Ok(self.site_settings.clone());
        }
        crate::bundled_sites::load_site_settings_uncached(&self.write_objects).await
    }

    /// Give one acquisition row the identity the lane dispatcher needs.
    ///
    /// The identity is resolved from the effective YAML definitions, without
    /// building a heavy downloader and without an upstream request.
    pub(crate) async fn prepare_acquisition_identity(
        &self,
        job_id: &JobId,
        plan: &JobPlan,
        settings: &[SiteSetting],
    ) -> Result<()> {
        let identity =
            crate::acquisition::resolve_identity_with(settings, self, &plan.target).await?;
        self.ledger
            .set_acquisition_identity(
                job_id,
                &identity.normalized_target,
                &identity.site_key,
            )
            .await
    }

    /// A fresh, exclusively-owned shared Downloader for one event invocation.
    /// Mutable downloader state is owned; compiled definitions are shared.
    ///
    /// Bundled site settings are compiled at composition time. Downloader
    /// settings are a per-invocation snapshot of `app_state`: local scope
    /// (`update.strong` / `guard-spoiler` / `auto-add-tags` /
    /// `download.use-subdirectory` / `economy`), global scope (`over18`), and the
    /// selected novel's section hashes. Unset booleans and age confirmation keep
    /// native's `false` / `None` defaults; `None` leaves the confirmation path
    /// intact and the executor reports `Blocked`. Diff snapshots are saved
    /// unless economy contains `nosave_diff`; raw HTML stays unsaved on Worker.
    pub async fn new_downloader(&self, target: &JobTarget, origin: Option<&narou_rs::application::novel_access::AccessOrigin>) -> Result<(Downloader,Option<narou_rs::application::novel_access::NovelWriteGuard>)> {
        let repository=D1NovelRepository::new(self.write_db.primary_view()).with_sites(self.site_settings.clone()).with_objects(self.write_objects.clone());
        let record=match target{
            JobTarget::Id(id)=>repository.get(*id).await?,
            JobTarget::Ncode(ncode)=>repository.find_by_ncode(ncode).await?,
            JobTarget::Url(url)=>repository.find_by_toc_url(url).await?,
            JobTarget::All=>None,
        };
        let prepared_writer=if let Some(origin)=origin{
            Some(narou_rs::application::novel_access::NovelWriteGuard::resume(self.write_objects.clone(),Some(self.assets.clone()),origin).await?)
        }else if let Some(record)=record.as_ref(){
            repository.bind_record_origin(record).await?;
            let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;
            Some(narou_rs::application::novel_access::NovelWriteGuard::capture_for(&self.write_objects,Some(&self.assets),&keys).await?)
        }else{None};
        let section_hash_cache=match(record.as_ref(),prepared_writer.as_ref()){
            (Some(record),Some(writer))=>{
                let keys=narou_rs::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;
                if writer.prefix().as_ref()!=keys.prefix().as_ref(){return Err(NarouError::Platform("novel_epoch_conflict: resumed writer namespace differs from canonical record".into()));}
                repository.bind_record_origin(record).await?;
                load_section_hash_cache(&self.write_db,record.id,writer.fence()).await?
            },
            _=>HashMap::new(),
        };
        let settings = self.downloader_settings().await?;
        let downloader = Downloader::with_platform_and_storage_and_settings_and_support(
            narou_rs::downloader::DownloaderPlatform {
                http: self.http.clone(),
                rate_limiter: self.rate_limiter.clone(),
                novels: Arc::new(repository),
                objects: self.write_objects.clone(),
                assets: self.assets.clone(),
                clock: self.clock.clone(),
            },
            self.site_settings.clone(),
            section_hash_cache,
            settings,
        )?;
        Ok((downloader.with_cookie_store(self.cookie_store.clone()),prepared_writer))
    }

    /// Use the same effective YAML URL resolution as acquisition. The original
    /// plan stays unchanged; this key also resolves newly registered novels.
    pub(crate) fn normalized_job_target<'a>(&self, target: &'a str) -> std::borrow::Cow<'a, str> {
        if (target.starts_with("https://") || target.starts_with("http://"))
            && let Some(url) = narou_rs::application::target_lookup::first_matching_toc_url(&self.site_settings, target)
        {
            return std::borrow::Cow::Owned(url);
        }
        std::borrow::Cow::Borrowed(target)
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
        // ジョブの実行パラメータはキャッシュ済みスナップショットではなく
        // 現在値を読む (job 経路のハンドル自体は primary)。
        let store = D1SettingsStore::for_writes(self.write_db.clone());
        let local_values = store.load(SettingScope::Local).await?;
        let local: HashMap<String, bool> = LOCAL_BOOL_KEYS
            .iter()
            .filter_map(|key| {
                local_values
                    .get(*key)
                    .and_then(narou_rs::setting_core::setting_bool_value)
                    .map(|value| ((*key).to_string(), value))
            })
            .collect();
        let global_values = store.load(SettingScope::Global).await?;
        let over18 = global_values
            .get("over18")
            .and_then(narou_rs::setting_core::setting_bool_value);
        Ok(Arc::new(
            SnapshotDownloaderSettings::new(local, over18, HashMap::new())
                .with_economy(local_values.get("economy"))
                .with_pending_section_hash_save(self.pending_section_hash_cache.clone()),
        ))
    }

    /// Flush this invocation's current-novel hints without reading other books.
    pub async fn persist_pending_section_hash_cache(&self, fence: &narou_rs::platform::WriteFence) -> Result<()> {
        let pending = self.pending_section_hash_cache.lock().take();
        let Some(pending) = pending else {
            return Ok(());
        };
        // Each novel owns an independent row: no library-wide read/merge/write.
        persist_section_hash_cache(&self.write_db, pending, fence).await
    }

    pub(crate) async fn collect_expired_assets(&self) -> Result<()> {
        D1ObjectStore::new(self.write_db.clone()).collect_expired_stages(16).await?;
        if let Some(store) = &self.stage_store {
            store.collect_expired_stages(16).await?;
        }
        Ok(())
    }

    /// 管理 API 用の具体ハンドル（診断用メソッドを持つ）。
    pub fn cookie_store(&self) -> &Arc<crate::d1_cookie_store::D1CookieStore> {
        &self.cookie_store
    }

    /// 保存済み設定の読み出し (D1)。`ConvertService` が `default.*` / `force.*`
    /// を適用するのに使う。
    pub fn settings_store(&self) -> Arc<dyn narou_rs::application::settings::SettingsStore> {
        Arc::new(D1SettingsStore::for_writes(self.write_db.clone()))
    }

    /// コンパイル済みサイト定義のスナップショット。`build` (Job) 経路では
    /// bundle + ユーザー定義が読み込まれ、`build_ui` では空。
    /// `new_downloader` と同じ一覧を、小説単位の `FetchPolicy` 解決
    /// (挿絵取得のサイトヘッダ) が共有する。
    pub fn site_settings(&self) -> &[SiteSetting] {
        &self.site_settings
    }

    /// native `commands::update` の `update.interval` を、同じドメインの Update
    /// ジョブの開始間隔として適用する。native は update コマンドのプロセス内で
    /// 数えるが、Worker ではジョブが別 isolate で走りうるので、サイト別 DO と
    /// 同じ仕組み (`update-start:` バケット) に永続化して数える。
    pub async fn pace_update_start(&self, domain: &str) -> Result<()> {
        self.rate_limiter
            .acquire_work_start(domain)
            .await
    }

    pub fn objects(&self) -> Arc<dyn ObjectStore> {
        self.objects.clone()
    }

    pub fn assets(&self) -> Arc<dyn AssetStore> {
        self.assets.clone()
    }
    /// 更新系サービスへのアクセサ。UI ハンドラは書き込み判断に必要な
    /// 読み取りも含めてこちらを使う (`self.services` は read replica /
    /// isolate キャッシュ系)。
    pub fn write_services(&self) -> &AppServices {
        &self.write_services
    }

    /// 更新系のオブジェクトストア (primary 直行)。削除・PUT 系の
    /// read-your-writes と保存先解決はこちらを使う。
    pub fn write_objects(&self) -> Arc<dyn ObjectStore> {
        self.write_objects.clone()
    }

    pub fn author_service(&self) -> narou_rs::application::authors::AuthorService {
        crate::authors::service(self.write_db.clone())
    }

    /// 更新系の primary D1 ハンドル。非表示設定など `app_state` を直接
    /// 読み書きする小さなストア用。
    pub fn write_db(&self) -> &DbHandle {
        &self.write_db
    }

    pub async fn author_work_urls(&self, url: &str) -> Result<Vec<String>> {
        let sites = crate::bundled_sites::load_site_settings_uncached(&self.write_objects).await?;
        let expander = narou_rs::application::download_targets::DownloadTargetExpander::new(
            self.http.as_ref(), self.rate_limiter.as_ref(), &sites,
        ).with_cookie_store(Some(&*self.cookie_store));
        narou_rs::application::authors::new_work_urls(url, &expander, &sites, self.novels.as_ref()).await
    }

    pub async fn enqueue_author_checks(&self, automatic: bool) -> Result<Vec<DispatchOutcome>> {
        if automatic {
            let values = self.write_services.settings.get_many(&["update.disabled-author-tracking"]).await
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            if values.first().and_then(Option::as_ref).and_then(narou_rs::setting_core::setting_bool_value).unwrap_or(false) {
                return Ok(Vec::new());
            }
        }
        let plans = self.author_service().list().await?.into_iter().map(|author| {
            let target = JobTarget::parse(&author.url).filter(|target| matches!(target, JobTarget::Url(_)))
                .ok_or_else(|| NarouError::InvalidTarget(format!("登録中の作者 URL が不正です: {}", author.url)))?;
            Ok(JobPlan { kind: JobKind::AuthorCheck, target, options: Vec::new() })
        }).collect::<Result<Vec<_>>>()?;
        self.enqueue_batch(&plans).await
    }


    /// Expand before writing any ledger rows, preserving input groups for the
    /// Web UI response. UI composition skips site loading, so load the current
    /// primary definitions only for requests containing download URLs.
    ///
    /// Author pages can be login-walled, so the expander gets the same D1
    /// credential store the downloader/HTTP client use. `&*self.cookie_store`
    /// borrows the runtime field for the expander's lifetime — never a
    /// temporary clone.
    pub async fn expand_download_plans(&self, plans: Vec<JobPlan>) -> Result<Vec<Vec<JobPlan>>> {
        if !plans.iter().any(|plan| plan.kind == JobKind::Download && matches!(plan.target, JobTarget::Url(_))) {
            return Ok(plans.into_iter().map(|plan| vec![plan]).collect());
        }
        let sites = crate::bundled_sites::load_site_settings_uncached(&self.write_objects).await?;
        let expander = narou_rs::application::download_targets::DownloadTargetExpander::new(
            self.http.as_ref(), self.rate_limiter.as_ref(), &sites,
        )
        .with_cookie_store(Some(&*self.cookie_store));
        let mut expanded = Vec::with_capacity(plans.len());
        for plan in plans {
            expanded.push(expander.expand_plan(&plan).await?.unwrap_or_else(|| vec![plan]));
        }
        Ok(expanded)
    }

    /// Redispatch the same durable job.
    ///
    /// Conversion keeps the version-2 protocol on the conversion queue.
    /// Acquisition work is only ever delivered by the lane dispatcher: the
    /// writer never executes an acquisition itself.
    pub async fn send_existing_job(&self, job_id: &JobId, kind: &str) -> Result<()> {
        if kind == JobKind::Convert.as_str() {
            let queue = self.convert_queue.as_ref().ok_or_else(|| {
                NarouError::Platform("conversion queue binding is unavailable in this script".into())
            })?;
            return queue
                .send(WorkerJobEnvelope::v2(job_id.clone()))
                .await
                .map_err(|error| NarouError::Platform(format!("queue send failed: {error}")));
        }
        self.dispatch_acquisition().await
    }

    /// Completion and cron wake durable waiting work without provider retries.
    ///
    /// Only conversion rows wait in the queue's waiting set; acquisition rows
    /// are woken by the lane dispatcher.
    pub async fn wake_waiting_jobs(&self) -> Result<()> {
        for job in self.ledger.waiting_jobs().await? {
            if !self.ledger.mark_waiting_dispatched(&job.job_id).await? {
                continue;
            }
            self.send_existing_job(&JobId(job.job_id), &job.kind).await?;
        }
        Ok(())
    }

    /// Enqueue one plan through the ledger and Queue binding as one operation.
    /// Pending/retryable rows are sent; a live running row is not duplicated.

    pub async fn enqueue_plan_with_origin(&self, plan: JobPlan, origin: &narou_rs::application::novel_access::AccessOrigin) -> Result<DispatchOutcome> {
        let mut outcomes=self.dispatch_plans(std::slice::from_ref(&plan),Some(origin)).await?;
        outcomes.pop().ok_or_else(||NarouError::Platform("enqueue produced no outcome".into()))?.map_err(NarouError::Platform)
    }

    /// Enqueue a bounded page as one batch operation: one ledger dedupe
    /// SELECT + one INSERT batch per 100 plans, one frozen snapshot for the
    /// whole page, and Queue sends in bounded `send_batch` chunks. Every plan
    /// is attempted before a partial failure is reported; failed sends stay
    /// active in the ledger for the reaper to repair.
    pub async fn enqueue_batch(&self, plans: &[JobPlan]) -> Result<Vec<DispatchOutcome>> {
        if plans.is_empty() {
            return Ok(Vec::new());
        }
        let results = self.dispatch_plans(plans,None).await?;
        let mut outcomes = Vec::with_capacity(results.len());
        let mut failures = Vec::new();
        for result in results {
            match result {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => failures.push(error),
            }
        }
        if failures.is_empty() {
            Ok(outcomes)
        } else {
            Err(NarouError::Platform(format!(
                "queue page partially failed ({}): {}",
                failures.len(),
                failures.join("; ")
            )))
        }
    }

    /// Shared bounded dispatch used by both the single-plan and page paths.
    ///
    /// Per-plan failures are returned in place so the caller can either
    /// surface one error or aggregate a partial page. Ordering is the input
    /// order: `results[i]` always belongs to `plans[i]`, and duplicate plans
    /// that dedupe onto one ledger row share that row's job id.
    async fn dispatch_plans(
        &self,
        plans: &[JobPlan],
        origin: Option<&narou_rs::application::novel_access::AccessOrigin>,
    ) -> Result<Vec<std::result::Result<DispatchOutcome, String>>> {
        // One ledger round trip per chunk: dedupe lookup, bounded inserts,
        // and the status of every winning row (no per-plan re-read).
        let enqueued=if let Some(origin)=origin{
            let [plan]=plans else{return Err(NarouError::Platform("origin-bound dispatch requires one logical job".into()))};
            vec![self.ledger.enqueue_with_origin(plan,origin).await?]
        }else{self.ledger.enqueue_batch(plans).await?};
        let frozen = self.frozen_skip_reasons(plans).await;
        let mut results: Vec<std::result::Result<DispatchOutcome, String>> =
            Vec::with_capacity(plans.len());
        let mut converts: Vec<usize> = Vec::new();
        let mut acquisitions = 0usize;
        // 取得行の identity はページ全体で同じ定義スナップショットを使う。
        let settings = if plans
            .iter()
            .any(|plan| plan.kind != JobKind::Convert)
        {
            match self.effective_site_settings().await {
                Ok(settings) => Some(settings),
                Err(error) => {
                    console_log!("site definitions are unavailable: {error}");
                    None
                }
            }
        } else {
            None
        };
        for (index, plan) in plans.iter().enumerate() {
            let job_id = enqueued[index].queued.job_id.clone();
            // 凍結小説の除外はここでも行う (native の update/download 拒否と
            // 同じ規則、`--force` 指定時のみ通す)。キューへ流さず、代わりに
            // 台帳行を `partial` で閉じて理由を残す — native の単発 mistook=1
            // → queue_partial と同じ終端。bulk 呼び出し (`job_actions`) が
            // `blocked` を致命的に扱うため、凍結スキップは `blocked` ではなく
            // `sent: false` + 終端行として報告する。
            if let Some(reason) = &frozen[index] {
                if let Err(error) = self.ledger.settle_skipped(&job_id, reason).await {
                    results.push(Err(error.to_string()));
                    continue;
                }
                results.push(Ok(DispatchOutcome {
                    job_id: job_id.as_str().to_string(),
                    sent: false,
                    blocked: None,
                }));
                continue;
            }
            if let Some(reason) = self.blocked_plan_reason(plan, &job_id) {
                let target = plan.target.as_str();
                let normalized_target = self.normalized_job_target(&target);
                match self.ledger.claim(&job_id, &normalized_target, None).await {
                    Ok(JobClaim::Claimed { execution_token, .. }) => {
                        if let Err(error) = self
                            .ledger
                            .mark_terminal(
                                &job_id,
                                &execution_token,
                                JobLedgerStatus::Blocked,
                                Some(&reason),
                            )
                            .await
                        {
                            results.push(Err(error.to_string()));
                            continue;
                        }
                    }
                    Ok(JobClaim::AlreadyTerminal | JobClaim::Unknown) => {}
                    Ok(JobClaim::Busy) => {
                        results.push(Err("cannot block a job with a live execution lease".to_string()));
                        continue;
                    }
                    Err(error) => {
                        results.push(Err(error.to_string()));
                        continue;
                    }
                }
                results.push(Ok(DispatchOutcome {
                    job_id: job_id.as_str().to_string(),
                    sent: false,
                    blocked: Some(reason),
                }));
                continue;
            }
            if !matches!(
                enqueued[index].status,
                JobLedgerStatus::Pending | JobLedgerStatus::Retryable
            ) {
                results.push(Ok(DispatchOutcome {
                    job_id: job_id.as_str().to_string(),
                    sent: false,
                    blocked: None,
                }));
                continue;
            }
            if plan.kind == JobKind::Convert {
                converts.push(index);
                results.push(Ok(DispatchOutcome {
                    job_id: job_id.as_str().to_string(),
                    sent: false,
                    blocked: None,
                }));
                continue;
            }
            // 取得は「識別子のある行」としてだけ予約できる。effective YAML から
            // 正規の target と site family を先に確定し (重い Downloader は
            // 作らない)、そのあとレーンへ割り当ててもらう。
            if let Some(settings) = &settings
                && let Err(error) = self
                    .prepare_acquisition_identity(&job_id, plan, settings)
                    .await
            {
                // 対象が解決できない行は identity なしで残る。ディスパッチャの
                // 取り込み処理が、実行時と同じ恒久エラーとして終端する。
                console_log!("job {job_id} has no effective site identity yet: {error}");
            }
            acquisitions += 1;
            results.push(Ok(DispatchOutcome {
                job_id: job_id.as_str().to_string(),
                sent: true,
                blocked: None,
            }));
        }
        // 変換だけは version-2 のまま変換キューへ送る。
        // Bounded Queue batches. A failed `send_batch` is retried per message
        // so the failure stays attached to its own plan and the rest of the
        // page (and later chunks) still goes out.
        if !converts.is_empty() {
            let queue = self.convert_queue.as_ref().ok_or_else(|| {
                NarouError::Platform("conversion queue binding is unavailable in this script".into())
            })?;
            for chunk in converts.chunks(QUEUE_SEND_BATCH_SIZE) {
                let envelopes: Vec<WorkerJobEnvelope> = chunk
                    .iter()
                    .map(|index| WorkerJobEnvelope::v2(enqueued[*index].queued.job_id.clone()))
                    .collect();
                match queue.send_batch(envelopes).await {
                    Ok(()) => {
                        for index in chunk {
                            if let Ok(outcome) = &mut results[*index] {
                                outcome.sent = true;
                            }
                        }
                    }
                    Err(batch_error) => {
                        console_log!(
                            "queue send_batch of {} messages failed ({batch_error}); retrying per message",
                            chunk.len()
                        );
                        for index in chunk {
                            let envelope =
                                WorkerJobEnvelope::v2(enqueued[*index].queued.job_id.clone());
                            match queue.send(&envelope).await {
                                Ok(()) => {
                                    if let Ok(outcome) = &mut results[*index] {
                                        outcome.sent = true;
                                    }
                                }
                                Err(error) => {
                                    results[*index] = Err(format!("queue send failed: {error}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        // 取得はレーンへ割り当てる。割り当てに失敗しても行は台帳に残るので、
        // 次のディスパッチ/クロンが回収する (ジョブは失われない)。
        if acquisitions > 0 {
            if let Err(error) = self.dispatch_acquisition().await {
                console_log!("acquisition dispatch failed: {error}");
            }
        }
        Ok(results)
    }

    /// Frozen-target exclusion for a whole page, loaded once: without
    /// `--force`, a Download/Update plan whose target resolves to a frozen
    /// novel is folded into a terminal `partial` row instead of entering the
    /// runnable queue (native `commands::{update,download}` skip parity — a
    /// queued job that must not run). The native notice text is recorded as
    /// `last_error` so the queue row shows why it settled.
    ///
    /// The set is the same union the single-plan path uses —
    /// `FreezeStore::frozen_ids` (primary, cache-bypassing) plus the legacy
    /// `frozen` record tag — read once for the page instead of once per plan.
    /// Read failures stay fail-open, exactly like `NovelActionService::is_frozen`.
    /// Returns `None` per plan when it is not frozen-skippable (force, other
    /// kinds, unresolved/new targets — all proceed as before). The executor
    /// keeps an identical check as the last line of defense for envelopes
    /// that bypass dispatch (legacy v1, direct ledger writes).
    async fn frozen_skip_reasons(&self, plans: &[JobPlan]) -> Vec<Option<String>> {
        use narou_rs::application::messages;
        let mut reasons: Vec<Option<String>> = std::iter::repeat_with(|| None)
            .take(plans.len())
            .collect();
        let candidates: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter(|(_, plan)| {
                matches!(plan.kind, JobKind::Download | JobKind::Update)
                    && !plan
                        .options
                        .iter()
                        .any(|option| option == "--force" || option == "-f")
            })
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            return reasons;
        }
        // 凍結判定は更新系 (primary + キャッシュ無し) で行う — 読み取り系の
        // `services` は isolate キャッシュ・replica 遅延の凍結集合を返し得て、
        // 直前の凍結/解除を見落とす。
        let tagged = NovelFilter {
            tag: Some("frozen".to_string()),
            ..NovelFilter::default()
        };
        let (store_ids, tagged_ids) = futures::join!(
            self.freeze.frozen_ids(),
            self.write_services().library.all_ids(&tagged),
        );
        let mut frozen: HashSet<i64> = store_ids.unwrap_or_default();
        frozen.extend(
            tagged_ids
                .unwrap_or_default()
                .into_iter()
                .map(|id| id.0),
        );
        if frozen.is_empty() {
            return reasons;
        }
        let mut resolved: HashMap<String, Option<i64>> = HashMap::new();
        for index in candidates {
            let plan = &plans[index];
            let target = plan.target.as_str();
            let id = match resolved.get(&target) {
                Some(id) => *id,
                None => {
                    let id = crate::executor::resolve_novel_id(self.novels.as_ref(), &plan.target)
                        .await
                        .map(|id| id.0);
                    resolved.insert(target, id);
                    id
                }
            };
            let Some(id) = id else {
                continue;
            };
            if !frozen.contains(&id) {
                continue;
            }
            let title = self
                .novels
                .get(narou_rs::platform::NovelId(id))
                .await
                .ok()
                .flatten()
                .map(|record| record.title)
                .unwrap_or_default();
            reasons[index] = Some(match plan.kind {
                JobKind::Update => messages::update::frozen_id(id, title),
                _ => messages::download::frozen_abort(title),
            });
        }
        reasons
    }

    /// Queue-refused plans: kinds without a worker execution path, the
    /// library-wide auto-update placeholder, and envelopes over the payload
    /// limit. Each is settled `blocked` in the ledger instead of being sent.
    fn blocked_plan_reason(&self, plan: &JobPlan, job_id: &JobId) -> Option<String> {
        let unsupported = if !plan.kind.is_worker_executable() {
            Some(format!(
                "unsupported job kind {:?} on the worker (no subprocess support)",
                plan.kind
            ))
        } else if plan.target == JobTarget::All {
            Some("auto-update must be planned into discrete per-novel jobs".to_string())
        } else {
            None
        };
        let oversized = envelope_bytes(&WorkerJobEnvelope::v2(job_id.clone()))
            .map(|size| size > job_limits::MAX_ENVELOPE_BYTES)
            .unwrap_or(true);
        unsupported.or_else(|| {
            oversized.then(|| {
                format!(
                    "envelope exceeds queue payload limit (max {} bytes)",
                    job_limits::MAX_ENVELOPE_BYTES
                )
            })
        })
    }
}

/// Transfer decisions and byte reads always use primary-bound, uncached stores.
/// This lightweight composition does not require queue bindings.
pub(crate) async fn build_transfer_service(env: &Env) -> std::result::Result<(narou_rs::application::transfer::TransferService,narou_rs::application::transfer::AuthScope,Arc<crate::transfer::journal::D1TransferJournal>),narou_rs::application::transfer::TransferError> {
    use narou_rs::application::transfer::{AuthScope,TransferBuildInfo,TransferError,TransferService};
    use narou_rs::platform::TransferStageStore;
    let error=|e:worker::Error|TransferError::Platform{message:e.to_string()};
    let db=DbHandle::primary(Arc::new(env.d1("DB").map_err(error)?));
    let stores=build_object_stores(env,&db,crate::budget::SubrequestBudget::new(),true,false).await.map_err(error)?;
    let d1=Arc::new(D1ObjectStore::new(db.clone()));
    let (stage,s3):(Arc<dyn TransferStageStore>,bool)=match stores.stage_store {Some(store)=>(store,true),None=>(d1.clone(),false)};
    let journal=Arc::new(crate::transfer::journal::D1TransferJournal::new(db.clone()));
    let candidate=crate::content_store::nonce().map_err(|e|TransferError::Platform{message:e.to_string()})?;
    db.prepare("INSERT OR IGNORE INTO app_state(scope,key,value_json,value_yaml) VALUES('inv','transfer_library_id',?,?)").bind(&[serde_json::to_string(&candidate).map_err(|e|TransferError::Platform{message:e.to_string()})?.into(),candidate.into()]).map_err(error)?.run().await.map_err(error)?;
    let id=db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key='transfer_library_id'").first::<String>(Some("value_json")).await.map_err(error)?.ok_or_else(||TransferError::Platform{message:"library transfer identity is missing".into()})?;
    let scope=AuthScope{library_id:serde_json::from_str(&id).map_err(|e|TransferError::Platform{message:e.to_string()})?,principal:"admin".into()};
    let key=crate::secrets::require(env,"NAROU_RS_LOGIN_KEY").await.map_err(error)?;
    let sites=crate::bundled_sites::load_site_settings_uncached(&stores.objects).await.map_err(|e|TransferError::Platform{message:e.to_string()})?;
    let backend=Arc::new(crate::transfer::backend::WorkerTransferBackend::new(db,journal.clone(),stores.objects,stores.assets,stage,d1,s3,key.into_bytes(),sites));
    let build_id=option_env!("NAROU_TRANSFER_BUILD_ID").filter(|id|id.len()==64&&id.bytes().all(|b|b.is_ascii_hexdigit())).map(str::to_owned);
    let build=TransferBuildInfo{wasm_build_id:build_id,abi_version:option_env!("NAROU_TRANSFER_ABI_VERSION").filter(|id|*id=="2").map(str::to_owned)};
    Ok((TransferService::new(backend,journal.clone(),Arc::new(SystemClock),build),scope,journal))
}

/// Application services plus direct access to the novel object store, for
/// handlers that stream content (download-time EPUB).
pub struct ReadServices {
    pub app: AppServices,
    pub objects: Arc<dyn ObjectStore>,
    pub assets: Arc<dyn AssetStore>,
    pub novels: Arc<dyn NovelRepository>,
    pub conversion: narou_rs::application::convert::ConvertService,
    pub db: DbHandle,
    /// 挿絵を S3 に置く構成のときだけ入る（presigned URL を作るのに使う）。
    pub s3_illustrations: Option<Arc<S3Store>>,
}

/// Build the application services (read-only API surface).
pub async fn build_services(env: &Env) -> worker::Result<AppServices> {
    Ok(build_read_services(env).await?.app)
}

pub async fn build_read_services(env: &Env) -> worker::Result<ReadServices> {
    // Content metadata and leases must observe the same primary publication.
    // Metadata-only list queries retain their separate UI replica path.
    // Reader stores reject writes and never perform lazy source backfills.
    let reader = crate::worker_role::is_reader(env)?;
    let db = DbHandle::primary(Arc::new(env.d1("DB")?));
    let subrequests = crate::budget::SubrequestBudget::new();
    let WorkerObjectStores { objects, assets, s3_illustrations, .. } =
        build_object_stores(env, &db, subrequests, false, reader).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let freeze = Arc::new(D1FreezeStore::new(db.clone()));
    let site_settings = crate::bundled_sites::load_site_settings(&objects).await
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    // 属性の backfill は writer の責務 (配信では実行しない)。
    crate::webui::metadata::initialize_attributes(env).await?;
    let novels: Arc<dyn NovelRepository> =
        Arc::new(D1NovelRepository::new(db.clone()).with_sites(site_settings.clone()).with_objects(objects.clone()));
    let site_rules = Arc::new(
        narou_rs::application::site_definitions::SiteSettingsSnapshot::new(site_settings),
    );
    let settings = Arc::new(D1SettingsStore::new(db.clone()));
    let conversion = narou_rs::application::convert::ConvertService::new(
        objects.clone(), assets.clone(), settings.clone(),
    );
    Ok(ReadServices {
        app: services_from(ServiceDependencies {
            novels: novels.clone(),
            objects: objects.clone(),
            freeze,
            settings,
            tag_colors: Arc::new(D1TagColorStore::new(db.clone())),
            clock,
            site_rules,
            db: db.clone(),
            env,
        }),
        objects,
        assets,
        novels,
        conversion,
        db,
        s3_illustrations,
    })
}

/// Build the application services for mutation paths. すべての読み取りを
/// primary 直行・isolate キャッシュ無しで組み立てる (`WorkerRuntime::
/// write_services` と同じ組立) — 更新系ハンドラは「書き込み判断のための
/// 読み取り」も stale な replica/キャッシュ値を使えないため。
pub async fn build_mutation_services(env: &Env) -> worker::Result<AppServices> {
    // 配信 Worker は更新経路を実行しない (捕捉した変更系は writer へ
    // 転送される)。ここでも source を書き換えないことを保証する。
    if crate::worker_role::is_reader(env)? {
        return Err(worker::Error::RustError(
            "mutation services are writer-only".into(),
        ));
    }
    let db = DbHandle::primary(Arc::new(env.d1("DB")?));
    let subrequests = crate::budget::SubrequestBudget::new();
    let WorkerObjectStores { objects, .. } =
        build_object_stores(env, &db, subrequests, true, false).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let freeze = Arc::new(D1FreezeStore::for_writes(db.clone()));
    let site_settings = crate::bundled_sites::load_site_settings_uncached(&objects).await
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    crate::webui::metadata::initialize_attributes(env).await?;
    let novels: Arc<dyn NovelRepository> =
        Arc::new(D1NovelRepository::new(db.clone()).with_sites(site_settings.clone()));
    let site_rules = Arc::new(
        narou_rs::application::site_definitions::SiteSettingsSnapshot::new(site_settings),
    );
    Ok(services_from(ServiceDependencies {
        novels,
        objects,
        freeze,
        settings: Arc::new(D1SettingsStore::for_writes(db.clone())),
        tag_colors: Arc::new(D1TagColorStore::new(db.clone())),
        clock,
        site_rules,
        db,
        env,
    }))
}

struct ServiceDependencies<'a> {
    novels: Arc<dyn NovelRepository>,
    objects: Arc<dyn ObjectStore>,
    freeze: Arc<D1FreezeStore>,
    settings: Arc<D1SettingsStore>,
    tag_colors: Arc<D1TagColorStore>,
    clock: Arc<dyn Clock>,
    site_rules: Arc<narou_rs::application::site_definitions::SiteSettingsSnapshot>,
    db: DbHandle,
    env: &'a Env,
}

fn services_from(deps: ServiceDependencies<'_>) -> AppServices {
    let ServiceDependencies {
        novels, objects, freeze, settings, tag_colors, clock, site_rules, db, env,
    } = deps;
    let library = Arc::new(narou_rs::application::LibraryService::new(
        novels.clone(),
        clock.clone(),
        freeze.clone(),
        site_rules.clone(),
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
        settings: Arc::new(SettingsService::new(settings)),
        tag_colors: Arc::new(TagColorService::new(tag_colors)),
        jobs: Arc::new(JobService),
        scheduler: Arc::new(SchedulerService::new(clock)),
        site_definitions: site_rules,
        self_update: Arc::new(NoopSelfUpdateService),
        web_actions: Arc::new(EmptyWebActionService),
        virtual_novels: Arc::new(crate::virtual_novels::D1VirtualNovelDirectory::new(env)),
        // 書き込み元 (CAS で比べるスナップショット) は stale な replica を
        // 見ない primary 束縛で読む (作者一覧と同じ扱い)。
        api_tokens: Arc::new(crate::api_tokens::D1ApiTokenStore::new(db.primary_view())),
    })
}


/// Readiness probe: every required binding and configuration must be real.
///
/// - D1 answers `SELECT 1`
/// - acquisition / conversion queue producers, `RATE_LIMITER`, and `PUSH_HUB` are bound
/// - full composition (bundled site definitions parse and compile)
///
/// Nothing here performs network I/O; a missing binding or config fails
/// with `Err`, which the handler maps to 503.
///
/// 配信 Worker (reader) はキュー・cron・rate limiter を持たないので、
/// 読み取り専用組立 (source を変更しない) と writer への転送用 service
/// binding だけを確かめる。
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
    match crate::worker_role::role(env)? {
        WorkerRole::Reader => {
            // 捕捉した変更系は writer へ転送するので、その binding を要求する。
            env.service(WRITER_SERVICE_BINDING)?;
            return build_read_services(env).await.map(|_| ());
        }
        WorkerRole::Acquisition => {
            // consumer は自分の1本だけ。producer は系列再配信と取得後の変換用。
            let lane = crate::worker_role::acquisition_lane(env)?.ok_or_else(|| {
                worker::Error::RustError("acquisition role has no lane index".to_string())
            })?;
            env.queue(&format!("{ACQUISITION_QUEUE_BINDING_PREFIX}{lane}"))?;
            env.queue(CONVERT_QUEUE_BINDING)?;
            env.durable_object("RATE_LIMITER")?;
            env.durable_object("PUSH_HUB")?;
            // The lane dispatches acquisitions too, so every lane queue must
            // be bound.
            for index in 0..LANE_COUNT {
                env.queue(&format!("{ACQUISITION_QUEUE_BINDING_PREFIX}{index}"))?;
            }
            // Full composition validates bundled sites and all adapters.
            return WorkerRuntime::build(env).await.map(|_| ());
        }
        WorkerRole::Writer => {}
    }
    env.queue(JOB_QUEUE_BINDING)?;
    env.queue(CONVERT_QUEUE_BINDING)?;
    env.durable_object("RATE_LIMITER")?;
    env.durable_object("PUSH_HUB")?;
    // The writer dispatches acquisitions, so every lane queue must be bound.
    for index in 0..LANE_COUNT {
        env.queue(&format!("{ACQUISITION_QUEUE_BINDING_PREFIX}{index}"))?;
    }
    // Full composition validates bundled sites and all adapters.
    WorkerRuntime::build(env).await.map(|_| ())
}

/// The name of the conversion queue of this deployment, when it has one.
pub fn convert_queue_name(env: &Env) -> Option<String> {
    env.var("CONVERT_QUEUE_NAME")
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Deserialize)]
struct OneRow {
    one: i64,
}
