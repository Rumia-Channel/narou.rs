use std::collections::HashMap;
use std::sync::Arc;

use crate::application::{
    FreezeMutationStore, FreezeStore, SiteDefinitionProvider, SiteTimezone,
    SiteTimezoneProvider,
};
use crate::application::settings::SettingsStore;
use crate::application::tag_colors::TagColorStore;
use crate::error::{NarouError, Result};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::platform::{ObjectStore, PlatformFuture};

/// Console used for local-only jobs when web concurrency is enabled.
///
/// The setting is native because it comes from the filesystem-backed local
/// compatibility settings; web presentation code only consumes the result.
pub fn non_external_console_target() -> &'static str {
    if crate::compat::load_local_setting_bool("concurrency") {
        "stdout2"
    } else {
        "stdout"
    }
}

/// 起動時に組んだオブジェクトストア (mirror / S3 構成) を組み替えないと
/// 反映できない設定名。
///
/// native Web はプロセス寿命ぶんの `NativeStore` / `SqliteObjectStore` /
/// `SplitStore` を保持する。ここに挙げた設定をその場で再適用すると、新しい
/// 設定を読む CLI の子プロセスと古いストアを持つサーバが別の保存先を指す
/// ため、変更を検知したら再起動を要求する
/// ([`STORAGE_RESTART_REQUIRED_NOTE`])。
pub const STORAGE_RESTART_SETTING_NAMES: &[&str] = &[
    "sqlite.mirror-files",
    "s3.asset-backend",
    "s3.endpoint",
    "s3.bucket",
    "s3.region",
    "s3.prefix",
    "s3.access-key-id",
    "s3.secret-access-key",
];

/// 保存先の構成が変わったときの再起動要求メッセージ (コンソール/UI 共通)。
pub const STORAGE_RESTART_REQUIRED_NOTE: &str =
    "保存先の構成 (sqlite.mirror-files / s3.*) が変更されました。反映にはサーバの再起動が必要です";

#[derive(Clone)]
pub struct NativeTagColorStore {
    inventory: Arc<Inventory>,
}

impl NativeTagColorStore {
    pub fn new(inventory: Arc<Inventory>) -> Self {
        Self { inventory }
    }
}

impl TagColorStore for NativeTagColorStore {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<crate::tag_colors::TagColors>> {
        let inventory = Arc::clone(&self.inventory);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::tag_colors::load_tag_colors(&inventory))
                .await
                .map_err(|error| NarouError::Platform(format!("tag color load task failed: {error}")))?
        })
    }

    fn update<'a>(
        &'a self,
        update: crate::tag_colors::TagColorUpdate,
    ) -> PlatformFuture<'a, Result<crate::tag_colors::TagColors>> {
        let inventory = Arc::clone(&self.inventory);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::tag_colors::update_tag_colors(&inventory, update))
                .await
                .map_err(|error| NarouError::Platform(format!("tag color update task failed: {error}")))?
        })
    }
}

/// Native adapter for the local freeze inventory.
#[derive(Clone)]
pub struct NativeFreezeStore {
    inventory: Arc<Inventory>,
}

impl NativeFreezeStore {
    pub fn new(inventory: Arc<Inventory>) -> Self {
        Self { inventory }
    }
}

impl FreezeStore for NativeFreezeStore {
    fn frozen_ids<'a>(&'a self) -> PlatformFuture<'a, Result<std::collections::HashSet<i64>>> {
        let inventory = Arc::clone(&self.inventory);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let frozen: HashMap<i64, serde_yaml::Value> =
                    inventory.load("freeze", InventoryScope::Local)?;
                Ok(frozen.into_keys().collect())
            })
            .await
            .map_err(|error| NarouError::Platform(format!("freeze inventory task failed: {error}")))?
        })
    }
}

impl FreezeMutationStore for NativeFreezeStore {
    fn set_frozen_guarded_many<'a>(
        &'a self, fences: &'a [crate::platform::WriteFence], ids: &'a [crate::platform::NovelId], frozen: bool,
    ) -> PlatformFuture<'a, Result<()>> {
        let inventory = self.inventory.clone();
        let fences = fences.to_vec();
        let ids = ids.to_vec();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let store = crate::native::object_store::NativeStore::for_narou_root(inventory.root_dir())?;
                store.with_open_writes(&fences, || {
                    let records = crate::native::novel_repository::NativeNovelRepository::new().get_many_current_sync(&ids)?;
                    for (id, record) in ids.iter().zip(records) {
                        let record = record.ok_or_else(|| NarouError::NotFound(format!("ID: {}", id.0)))?;
                        let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
                        if !fences.iter().any(|fence| fence.prefix.as_ref() == keys.prefix().as_ref()) {
                            return Err(NarouError::Platform("freeze requires every novel's original writer fence".into()));
                        }
                    }
                    inventory.update_yaml("freeze", InventoryScope::Local, |mut current: HashMap<i64, serde_yaml::Value>| {
                        for id in ids {
                            if frozen { current.insert(id.0, serde_yaml::Value::Bool(true)); }
                            else { current.remove(&id.0); }
                        }
                        Ok((current, ()))
                    })
                })
            }).await.map_err(|error| NarouError::Platform(format!("freeze inventory task failed: {error}")))?
        })
    }

    fn set_frozen<'a>(
        &'a self,
        ids: &'a [crate::platform::NovelId],
        frozen: bool,
    ) -> PlatformFuture<'a, Result<()>> {
        let _ = (ids, frozen);
        Box::pin(async { Err(NarouError::Platform("freeze requires original writer fences".into())) })
    }
}

/// Native adapter for local/global YAML settings.
#[derive(Clone)]
pub struct NativeSettingsStore {
    inventory: Arc<Inventory>,
}

impl NativeSettingsStore {
    pub fn new(inventory: Arc<Inventory>) -> Self {
        Self { inventory }
    }
}

impl SettingsStore for NativeSettingsStore {
    fn load<'a>(
        &'a self,
        scope: crate::setting_core::SettingScope,
    ) -> PlatformFuture<'a, Result<HashMap<String, serde_yaml::Value>>> {
        let inventory = Arc::clone(&self.inventory);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::db::settings::load_with_inventory(&inventory, scope))
                .await
                .map_err(|error| {
                    NarouError::Platform(format!("settings load task failed: {error}"))
                })?
        })
    }

    /// Replace the scope only while the stored map still equals `expected`.
    ///
    /// The comparison and the write happen in one `update_yaml` pass, so the
    /// legacy file lock / SQLite single-writer boundary serializes concurrent
    /// writers. A mismatch returns false and leaves the scope untouched.
    fn compare_save<'a>(
        &'a self,
        scope: crate::setting_core::SettingScope,
        expected: &'a HashMap<String, serde_yaml::Value>,
        replacement: &'a HashMap<String, serde_yaml::Value>,
    ) -> PlatformFuture<'a, Result<bool>> {
        let inventory = Arc::clone(&self.inventory);
        let expected = expected.clone();
        let replacement = replacement.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut conflict = false;
                let result = crate::db::settings::update_with_inventory(&inventory, scope, |current| {
                    if *current != expected {
                        conflict = true;
                        return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
                    }
                    *current = replacement;
                    Ok(true)
                });
                if conflict { Ok(false) } else { result }
            })
            .await
            .map_err(|error| {
                NarouError::Platform(format!("settings compare_save task failed: {error}"))
            })?
        })
    }

    fn load_replace_content<'a>(
        &'a self,
    ) -> PlatformFuture<'a, Result<String>> {
        let path = self.inventory.root_dir().join("replace.txt");
        Box::pin(async move {
            tokio::task::spawn_blocking(move || match std::fs::read_to_string(path) {
                Ok(content) => Ok(content),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                Err(error) => Err(error.into()),
            })
            .await
            .map_err(|error| NarouError::Platform(format!("replace load task failed: {error}")))?
        })
    }

    /// Replace the global `replace.txt` only while it still equals `expected`.
    ///
    /// Runs under the same process/file write lock as the settings CAS above.
    /// A mismatch returns false without rewriting or creating the content file.
    fn compare_save_replace_content<'a>(
        &'a self,
        expected: &'a str,
        replacement: &'a str,
    ) -> PlatformFuture<'a, Result<bool>> {
        let path = self.inventory.root_dir().join("replace.txt");
        let expected = expected.to_string();
        let replacement = replacement.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::db::inventory::compare_locked_file_content(&path, &expected, &replacement)
            })
            .await
            .map_err(|error| {
                NarouError::Platform(format!("replace compare_save task failed: {error}"))
            })?
        })
    }
}

/// Native adapter for `webnovel/*.yaml` site timezones and update capabilities.
///
/// サイト定義は [`crate::downloader::site_setting::effective_site_settings`] の
/// 共有スナップショットを見るので、`install_effective_site_settings` で差し替
/// わった後は次の呼び出しから新しい定義を使う（タイムゾーンのマップだけ
/// `Arc` の同一性を見て再構築をキャッシュする）。
#[derive(Clone)]
pub struct NativeSiteTimezoneProvider {
    snapshot: Arc<std::sync::RwLock<Arc<crate::application::site_definitions::SiteSettingsSnapshot>>>,
}


impl NativeSiteTimezoneProvider {
    pub fn load() -> Self {
        let source = crate::downloader::site_setting::effective_site_settings_shared();
        Self {
            snapshot: Arc::new(std::sync::RwLock::new(Arc::new(
                crate::application::site_definitions::SiteSettingsSnapshot::new(source),
            ))),
        }
    }


    fn snapshot(&self) -> Arc<crate::application::site_definitions::SiteSettingsSnapshot> {
        let source = crate::downloader::site_setting::effective_site_settings_shared();
        if let Ok(snapshot) = self.snapshot.read()
            && Arc::ptr_eq(snapshot.source(), &source)
        {
            return snapshot.clone();
        }
        let snapshot = Arc::new(crate::application::site_definitions::SiteSettingsSnapshot::new(source));
        if let Ok(mut current) = self.snapshot.write() {
            *current = snapshot.clone();
        }
        snapshot
    }
}
impl SiteTimezoneProvider for NativeSiteTimezoneProvider {
    fn timezone_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> PlatformFuture<'a, Result<Option<SiteTimezone>>> {
        let timezone = self.snapshot().timezone(domain);
        Box::pin(async move { Ok(timezone) })
    }
}

impl SiteDefinitionProvider for NativeSiteTimezoneProvider {
    fn resolve_toc_url(&self, url: &str) -> Option<String> {
        self.snapshot().resolve_toc_url(url)
    }

    fn url_patterns_for_validation(&self) -> Vec<String> {
        self.snapshot().url_patterns_for_validation()
    }
}
impl crate::application::SiteUpdateCapabilityProvider for NativeSiteTimezoneProvider {
    fn supports_narou_api(&self, toc_url: &str) -> bool {
        crate::application::SiteUpdateCapabilityProvider::supports_narou_api(self.snapshot().as_ref(), toc_url)
    }

    fn allows_automatic_update(&self, record: &crate::db::NovelRecord) -> bool {
        crate::application::SiteUpdateCapabilityProvider::allows_automatic_update(self.snapshot().as_ref(), record)
    }
}


/// Native composition root for the application services used by the web
/// command. Web handlers receive this bundle and never construct native
/// repositories or storage adapters themselves.
pub struct NativeAppServices {
    pub novels: Arc<dyn crate::platform::NovelRepository>,
    pub objects: Arc<dyn crate::platform::ObjectStore>,
    pub services: Arc<crate::application::AppServices>,
    pub site_updates: Arc<dyn crate::application::SiteUpdateCapabilityProvider>,
}

impl NativeAppServices {
    pub fn new(inventory: Arc<Inventory>) -> Result<Self> {
        let root_dir = inventory.root_dir().to_path_buf();
        let active_root = crate::db::with_database(|database| database.archive_root().parent().map(std::path::Path::to_path_buf)
            .ok_or_else(|| NarouError::Platform("archive has no library root".into())))?;
        if active_root != root_dir { return Err(NarouError::Platform("service inventory differs from the active database root".into())); }
        let novels: Arc<dyn crate::platform::NovelRepository> =
            Arc::new(crate::native::novel_repository::NativeNovelRepository::new());
        let stores = crate::native::object_store::NativeStores::for_narou_root(&root_dir)?;
        let objects = stores.objects;
        let freeze_native = Arc::new(Self::freeze_store(inventory.clone()));
        let site_provider = Arc::new(NativeSiteTimezoneProvider::load());
        let library = Arc::new(crate::application::LibraryService::new(
            novels.clone(),
            Arc::new(crate::platform::clock::SystemClock),
            freeze_native.clone(),
            site_provider.clone(),
        ));
        let actions = Arc::new(crate::application::NovelActionService::new(
            novels.clone(),
            freeze_native.clone(),
            freeze_native.clone(),
            Some(objects.clone()),
        ));
        let settings = Arc::new(crate::application::SettingsService::new(Arc::new(
            NativeSettingsStore::new(inventory.clone()),
        )));
        let novel_settings = Arc::new(crate::application::NovelSettingsService::new(
            novels.clone(),
            objects.clone(),
        ));
        let content = Arc::new(crate::application::NovelContentService::new(
            novels.clone(),
            objects.clone(),
        ));
        let tag_colors = Arc::new(crate::application::TagColorService::new(Arc::new(
            NativeTagColorStore::new(inventory.clone()),
        )));
        let scheduler = Arc::new(crate::application::SchedulerService::new(Arc::new(
            crate::platform::clock::SystemClock,
        )));
        let services = Arc::new(crate::application::AppServices::new(
            crate::application::AppServiceDependencies {
                library,
                novel_actions: actions,
                novel_settings,
                content,
                settings,
                tag_colors,
                jobs: Arc::new(crate::application::JobService),
                scheduler,
                site_definitions: site_provider.clone(),
                self_update: Arc::new(crate::native::self_update::NativeSelfUpdateService),
                web_actions: Arc::new(crate::native::web_actions::NativeWebActionService),
                virtual_novels: Arc::new(
                    crate::native::virtual_novel::NativeVirtualNovelDirectory,
                ),
                api_tokens: Arc::new(crate::native::api_tokens::NativeApiTokenStore),
            },
        ));
        Ok(Self {
            novels,
            objects,
            services,
            site_updates: site_provider,
        })
    }

    fn freeze_store(inventory: Arc<Inventory>) -> NativeFreezeStore {
        NativeFreezeStore::new(inventory)
    }
}

/// Finish every eligible closed remove owner and abandoned ordinary claim.
/// Live OS liveness owners are never reaped, and deferred readers stay closed.
pub fn recover_removals() -> Result<()> {
    crate::native::materialization::block_on(async {
        let (service, objects, root) = native_recovery_service()?;
        let repository = crate::native::novel_repository::NativeNovelRepository::new();
        loop {
            let mut cursor = None;
            let mut release_error = None;
            loop {
                let (next, error) = collect_abandoned_creation_page(&repository, &root, cursor, 100).await?;
                if release_error.is_none() { release_error = error; }
                cursor = next;
                if cursor.is_none() { break; }
            }
            let had_pending = has_pending_removals(objects.as_ref()).await?;
            let mut cursor = None;
            loop {
                cursor = service.recover_removals(cursor.as_deref(), 100, usize::MAX).await
                    .map_err(|error| NarouError::Platform(error.to_string()))?;
                if cursor.is_none() { break; }
            }
            if has_pending_removals(objects.as_ref()).await? {
                return Err(NarouError::Platform("novel_recovering: removal is waiting for admitted readers or closed file handles".into()));
            }
            // A drained owner may still need its exact creation claim ACK.
            // Revisit it through release_id, never through a guessed Remove.
            if had_pending { continue; }
            return match release_error { Some(error) => Err(error), None => Ok(()) };
        }
    })
}

fn native_recovery_service() -> Result<(crate::application::NovelActionService, Arc<dyn ObjectStore>, std::path::PathBuf)> {
    let inventory = Arc::new(Inventory::with_default_root()?);
    let root = crate::db::with_database(|database| database.archive_root().parent().map(std::path::Path::to_path_buf)
        .ok_or_else(|| NarouError::Platform("archive has no library root".into())))?;
    if root.canonicalize()? != inventory.root_dir().canonicalize()? {
        return Err(NarouError::Platform("active database belongs to a different library".into()));
    }
    let objects = crate::native::object_store::NativeStores::for_narou_root(inventory.root_dir())?.objects;
    let freeze = Arc::new(NativeFreezeStore::new(inventory));
    let service = crate::application::NovelActionService::new(
        Arc::new(crate::native::novel_repository::NativeNovelRepository::new()), freeze.clone(), freeze, Some(objects.clone()),
    );
    Ok((service, objects, root))
}

async fn collect_abandoned_creation_page(
    repository: &dyn crate::platform::NovelRepository,
    root: &std::path::Path,
    cursor: Option<i64>,
    limit: usize,
) -> Result<(Option<i64>, Option<NarouError>)> {
    let (candidates, next) = crate::db::with_database(|database| database.ordinary_id_liveness_page(cursor, limit))?;
    let mut release_error = None;
    for (id, generation) in candidates {
        let Some(reservation) = resume_abandoned_creation(root, id, generation)? else { continue; };
        if let Err(error)=repository.release_id(reservation).await{
            // The release commits its closed-removal ownership before it
            // returns; only the reader drain and the orphan sweep stay
            // deferred. Every error here is a real release failure.
            if release_error.is_none(){
                release_error=Some(error);
            }
        }
    }
    Ok((next, release_error))
}

/// A pending-liveness row belongs to a creator that either died holding its
/// OS claim or is still alive. Try the real lock: `None` means a live owner
/// and is skipped, never treated as an error; `Some` is the actual OS proof
/// retained on the reservation for `release_id` to revalidate under the
/// ledger. I/O failures and invalid identities propagate.
fn resume_abandoned_creation(
    root: &std::path::Path, id: crate::platform::NovelId, generation: i64,
) -> Result<Option<crate::platform::NovelIdReservation>> {
    let Some(liveness) = crate::db::database::id_reservations::try_liveness_lock(root, id, generation)? else { return Ok(None); };
    Ok(Some(crate::platform::NovelIdReservation::new(id, generation).with_liveness(liveness)))
}

async fn has_pending_removals(objects: &dyn ObjectStore) -> Result<bool> {
    let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("removal access fence unavailable".into()))?;
    let mut cursor = None;
    loop {
        let (pending, next) = fence.pending_removals(cursor.as_deref(), 100).await?;
        if !pending.is_empty() { return Ok(true); }
        cursor = next;
        if cursor.is_none() { return Ok(false); }
    }
}

#[derive(Default)]
pub struct NativeRecoveryCursor {
    creations: Option<i64>,
    removals: Option<String>,
}

/// Advance bounded ordinary cleanup on the existing native queue idle cadence.
pub fn recover_maintenance(cursor: &mut NativeRecoveryCursor) -> Result<()> {
    crate::native::materialization::block_on(async {
        let (service, _, root) = native_recovery_service()?;
        let repository = crate::native::novel_repository::NativeNovelRepository::new();
        let (next, release_error) = collect_abandoned_creation_page(&repository, &root, cursor.creations, 4).await?;
        cursor.creations = next;
        cursor.removals = service.recover_removals(cursor.removals.as_deref(), 4, 16).await
            .map_err(|error| NarouError::Platform(error.to_string()))?;
        if !crate::native::sqlite::export_yaml::export_in_progress()
            && let Some(state) = crate::native::sqlite::state::active_for(&root.join(".narou"))
        {
            let mut guard = state.conn_ref().lock();
            if !crate::native::sqlite::export_yaml::export_in_progress() {
                let _ = crate::native::sqlite::object_store::reclaim_sqlite_bloat(
                    &mut guard,
                    Some(64),
                    crate::platform::staging::STAGE_TTL_SECONDS,
                );
                let _ = guard.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
                let wal_path = root.join(".narou").join("db.sqlite-wal");
                if std::fs::metadata(&wal_path).is_ok_and(|meta| {
                    meta.len() > crate::native::sqlite::SQLITE_JOURNAL_SIZE_LIMIT_BYTES as u64
                }) {
                    let _ = guard.busy_timeout(std::time::Duration::from_millis(50));
                    let _ = guard.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                    let _ = guard.busy_timeout(std::time::Duration::from_secs(5));
                }
            }
        }
        match release_error { Some(error) => Err(error), None => Ok(()) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::site_setting::{SiteSetting, install_effective_site_settings};

    #[test]
    fn recovery_skips_live_creation_claim_and_keeps_os_proof() {
        let root = tempfile::tempdir().unwrap();
        let id = crate::platform::NovelId(42);
        let held = crate::db::database::id_reservations::try_liveness_lock(root.path(), id, 7)
            .unwrap().expect("first claim acquires the OS lock");
        assert!(resume_abandoned_creation(root.path(), id, 7).unwrap().is_none(),
            "a genuinely held claim is skipped, never released as abandoned");
        assert!(resume_abandoned_creation(root.path(), id, -1).is_err(),
            "invalid identities are errors, not skipped claims");
        drop(held);
        let reservation = resume_abandoned_creation(root.path(), id, 7).unwrap()
            .expect("a dead creator's claim resumes with real OS proof");
        assert!(crate::db::database::id_reservations::try_liveness_lock(root.path(), id, 7).unwrap().is_none(),
            "the resumed token must exclude another process until cleanup finishes");
        drop(reservation);
        assert!(crate::db::database::id_reservations::try_liveness_lock(root.path(), id, 7).unwrap().is_some(),
            "dropping the resumed token releases its actual OS ownership");
    }

    #[tokio::test]
    async fn rejected_settings_cas_preserves_raw_yaml_and_absent_replace_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".narou")).unwrap();
        let path = root.path().join(".narou/local_setting.yaml");
        let raw = "# preserve this comment\nconcurrency: true # also preserve\n";
        std::fs::write(&path, raw).unwrap();
        let store = NativeSettingsStore::new(Arc::new(Inventory::new(root.path().to_path_buf())));
        assert!(!store.compare_save(
            crate::setting_core::SettingScope::Local, &HashMap::new(), &HashMap::from([
                ("concurrency".to_string(), serde_yaml::Value::Bool(false)),
            ]),
        ).await.unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert!(!store.compare_save_replace_content("stale", "replacement").await.unwrap());
        assert!(!root.path().join("replace.txt").exists());
    }

    /// サイト定義を差し替えた直後に、タイムゾーン/URL 解決が新しい定義を
    /// 使うこと（`OnceLock` 時代は起動時スナップショットに固定されていた）。
    /// プロセスグローバルに触るので cwd ガードで直列化し、後始末として
    /// ファイル由来の定義を install し直す。
    /// `install_effective_site_settings` が内部で `futures::executor::block_on`
    /// を使うため、外側は tokio の runtime で回す。
    #[tokio::test]
    async fn provider_follows_reinstalled_site_settings() {
        let temp = tempfile::tempdir().unwrap();
        let guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let user_webnovel = temp.path().join("webnovel");
        std::fs::create_dir_all(&user_webnovel).unwrap();

        let yaml = |timezone: &str, toc: &str| {
            format!(
                "name: Example\ndomain: example.invalid\ntop_url: https://example.invalid\n\
                 sitename: Example\nurl:\n  - https?://example\\.invalid/novel/(?<ncode>\\d+)\n\
                 encoding: UTF-8\ntimezone: {timezone}\ntoc_url: {toc}\n"
            )
        };
        let target = "https://example.invalid/novel/42";

        std::fs::write(
            user_webnovel.join("example.invalid.yaml"),
            yaml("UTC", "https://example.invalid/ncode/\\k<ncode>"),
        )
        .unwrap();
        crate::native::site_definitions::install_effective_site_settings()
            .await
            .unwrap();

        let provider = NativeSiteTimezoneProvider::load();
        let timezone = SiteTimezoneProvider::timezone_for_domain(&provider, "example.invalid")
            .await
            .unwrap();
        assert_eq!(timezone, Some(SiteTimezone::Named(chrono_tz::UTC)));
        assert_eq!(
            SiteDefinitionProvider::resolve_toc_url(&provider, target).as_deref(),
            Some("https://example.invalid/ncode/42")
        );

        // 定義を差し替えて再 install → 同じプロセスで新しい値が見える。
        std::fs::write(
            user_webnovel.join("example.invalid.yaml"),
            yaml("America/New_York", "https://example.invalid/novel/\\k<ncode>"),
        )
        .unwrap();
        crate::native::site_definitions::install_effective_site_settings()
            .await
            .unwrap();

        let timezone = SiteTimezoneProvider::timezone_for_domain(&provider, "example.invalid")
            .await
            .unwrap();
        assert_eq!(
            timezone,
            Some(SiteTimezone::Named(chrono_tz::America::New_York))
        );
        assert_eq!(
            SiteDefinitionProvider::resolve_toc_url(&provider, target).as_deref(),
            Some("https://example.invalid/novel/42")
        );

        // 内容が変わっていなければ再コンパイルしない（Arc の同一性で確認）。
        let installed = crate::downloader::site_setting::effective_site_settings_shared();
        crate::native::site_definitions::install_effective_site_settings()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            &installed,
            &crate::downloader::site_setting::effective_site_settings_shared()
        ));

        drop(guard);
        install_effective_site_settings(SiteSetting::load_all().unwrap_or_default());
    }
}
