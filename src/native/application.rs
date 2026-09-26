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
use crate::platform::PlatformFuture;

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

    fn save<'a>(
        &'a self,
        colors: &'a crate::tag_colors::TagColors,
    ) -> PlatformFuture<'a, Result<()>> {
        let inventory = Arc::clone(&self.inventory);
        let colors = colors.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::tag_colors::save_tag_colors(&inventory, &colors))
                .await
                .map_err(|error| NarouError::Platform(format!("tag color save task failed: {error}")))?
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
    fn set_frozen<'a>(
        &'a self,
        ids: &'a [crate::platform::NovelId],
        frozen: bool,
    ) -> PlatformFuture<'a, Result<()>> {
        let inventory = Arc::clone(&self.inventory);
        let ids = ids.iter().map(|id| id.0).collect::<Vec<_>>();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                inventory.update_yaml(
                    "freeze",
                    InventoryScope::Local,
                    |mut current: HashMap<i64, serde_yaml::Value>| {
                        for id in ids {
                            if frozen {
                                current.insert(id, serde_yaml::Value::Bool(true));
                            } else {
                                current.remove(&id);
                            }
                        }
                        Ok((current, ()))
                    },
                )
            })
            .await
            .map_err(|error| NarouError::Platform(format!("freeze inventory task failed: {error}")))?
        })
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

    fn save<'a>(
        &'a self,
        scope: crate::setting_core::SettingScope,
        settings: &'a HashMap<String, serde_yaml::Value>,
    ) -> PlatformFuture<'a, Result<()>> {
        let inventory = Arc::clone(&self.inventory);
        let settings = settings.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::db::settings::save_with_inventory(&inventory, scope, &settings)
            })
                .await
                .map_err(|error| {
                    NarouError::Platform(format!("settings save task failed: {error}"))
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

    fn save_replace_content<'a>(
        &'a self,
        content: &'a str,
    ) -> PlatformFuture<'a, Result<()>> {
        let path = self.inventory.root_dir().join("replace.txt");
        let content = content.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || std::fs::write(path, content))
                .await
                .map_err(|error| NarouError::Platform(format!("replace save task failed: {error}")))?
                .map_err(Into::into)
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
    snapshot: Arc<std::sync::RwLock<SiteProviderSnapshot>>,
}

#[derive(Clone)]
struct SiteProviderSnapshot {
    source: Arc<Vec<crate::downloader::site_setting::SiteSetting>>,
    timezones: Arc<HashMap<String, SiteTimezone>>,
}

impl NativeSiteTimezoneProvider {
    pub fn load() -> Self {
        let source = crate::downloader::site_setting::effective_site_settings_shared();
        Self {
            snapshot: Arc::new(std::sync::RwLock::new(Self::snapshot_for(source))),
        }
    }

    fn snapshot_for(
        source: Arc<Vec<crate::downloader::site_setting::SiteSetting>>,
    ) -> SiteProviderSnapshot {
        let timezones = source
            .iter()
            .map(|setting| {
                let domain = setting.domain.clone();
                let timezone = native_timezone(setting.site_timezone());
                (domain, timezone)
            })
            .collect();
        SiteProviderSnapshot {
            source,
            timezones: Arc::new(timezones),
        }
    }

    fn snapshot(&self) -> SiteProviderSnapshot {
        let source = crate::downloader::site_setting::effective_site_settings_shared();
        if let Ok(snapshot) = self.snapshot.read()
            && Arc::ptr_eq(&snapshot.source, &source)
        {
            return snapshot.clone();
        }
        let snapshot = Self::snapshot_for(source);
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
        let timezone = self.snapshot().timezones.get(domain).copied();
        Box::pin(async move { Ok(timezone) })
    }
}

impl SiteDefinitionProvider for NativeSiteTimezoneProvider {
    fn resolve_toc_url(&self, url: &str) -> Option<String> {
        self.snapshot()
            .source
            .iter()
            .find(|setting| setting.matches_url(url))
            .map(|setting| {
                setting
                    .toc_url_with_url_captures(url)
                    .unwrap_or_else(|| setting.toc_url())
            })
    }

    fn url_patterns_for_validation(&self) -> Vec<String> {
        self.snapshot()
            .source
            .iter()
            .flat_map(|setting| setting.url_patterns_for_validation())
            .collect()
    }
}
impl crate::application::SiteUpdateCapabilityProvider for NativeSiteTimezoneProvider {
    fn supports_narou_api(&self, toc_url: &str) -> bool {
        self.snapshot()
            .source
            .iter()
            .find(|setting| setting.matches_url(toc_url))
            .and_then(|setting| setting.narou_api_url.as_ref())
            .is_some()
    }
}

fn native_timezone(timezone: crate::downloader::SiteTimezone) -> SiteTimezone {
    match timezone {
        crate::downloader::SiteTimezone::Named(tz) => SiteTimezone::Named(tz),
        crate::downloader::SiteTimezone::Fixed(offset) => SiteTimezone::Fixed(offset),
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
        let novels: Arc<dyn crate::platform::NovelRepository> =
            Arc::new(crate::native::novel_repository::NativeNovelRepository::new());
        let objects: Arc<dyn crate::platform::ObjectStore> = Arc::new(
            crate::native::object_store::NativeStore::for_narou_root(&root_dir)?,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::site_setting::{SiteSetting, install_effective_site_settings};

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
