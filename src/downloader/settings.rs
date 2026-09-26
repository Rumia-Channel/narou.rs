//! Settings and interactive-decision capability for the shared downloader.
//!
//! The downloader core must not depend on the `.narou` Inventory or the
//! terminal. This small capability trait is the boundary: the native
//! implementation (gated below) reads the Inventory via `crate::compat` /
//! `crate::db` and prompts on the terminal exactly as the CLI always did;
//! the Worker implementation supplies bundled defaults and returns an
//! explicit `NarouError::Unsupported` for anything interactive instead of
//! prompting, continuing, or deleting silently.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;

/// Settings and interactive decisions consumed by the downloader pipeline.
pub trait DownloaderSettings: Send + Sync {
    /// Read a boolean local setting (`local_setting.yaml`). Worker: bundled
    /// default (`false`).
    fn local_setting_bool(&self, _key: &str) -> bool {
        false
    }

    /// Read a global setting (`global_setting.yaml`) as an optional bool.
    /// Worker: `None` (no stored value).
    fn global_setting_optional_bool(&self, _key: &str) -> Option<bool> {
        None
    }

    /// Persist a global bool setting. Worker: no-op (nothing to persist).
    fn save_global_setting_bool(&self, _key: &str, _value: bool) -> Result<()> {
        Ok(())
    }

    /// Whether new novels should be stored in a two-character subdirectory
    /// (`download.use-subdirectory` local setting). Worker: bundled default
    /// (`false`).
    fn download_use_subdirectory(&self) -> bool {
        false
    }

    /// Whether the novel's `setting.ini` strips bracketed title prefixes
    /// (`enable_strip_title_prefix`). Worker: no converter settings on disk,
    /// so `false`.
    fn strip_title_prefix_for(&self, _novel_id: i64, _raw_title: &str, _author: &str) -> bool {
        false
    }

    /// Load the section hash cache used by strong-update comparison. Worker:
    /// the constructor-supplied value is used directly, so the default is
    /// empty.
    fn load_section_hash_cache(&self) -> HashMap<String, HashMap<String, String>> {
        HashMap::new()
    }

    /// Persist the section hash cache. Worker: advisory in-memory cache, so
    /// the default is a no-op that reports success.
    fn save_section_hash_cache(
        &self,
        _cache: &HashMap<String, HashMap<String, String>>,
    ) -> Result<()> {
        Ok(())
    }

    /// Ask the user a yes/no question (e.g. age verification). Worker: no
    /// terminal, so this returns an explicit domain error rather than
    /// guessing.
    fn confirm(&self, _message: &str, _default: bool, _nontty_default: bool) -> Result<bool> {
        Err(crate::error::NarouError::Unsupported(
            "interactive confirmation is not supported on this platform".to_string(),
        ))
    }
}

/// Worker default capability: bundled values, no filesystem, no prompts.
/// All `DownloaderSettings` methods use their portable defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerDownloaderSettings;

impl DownloaderSettings for WorkerDownloaderSettings {}

/// Platform default capability used by `Downloader::with_platform_and_storage_and_settings`.
pub fn default_settings() -> Arc<dyn DownloaderSettings> {
    #[cfg(feature = "native-runtime")]
    {
        Arc::new(NativeDownloaderSettings)
    }
    #[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
    {
        Arc::new(WorkerDownloaderSettings)
    }
}

/// A `DownloaderSettings` snapshot built from values read ahead of time.
///
/// The trait is synchronous while Worker storage (D1) is asynchronous, so
/// the caller loads `app_state` rows once per invocation and injects this
/// immutable view. Local bools default to `false` when the key is absent —
/// matching the native inventory behavior where an unset local setting has
/// no default entry — and `over18` stays `None` when unset so the age
/// confirmation path is preserved (the default `confirm` returns
/// `NarouError::Unsupported`, which the executor turns into `Blocked`).
#[derive(Debug, Clone, Default)]
pub struct SnapshotDownloaderSettings {
    local: HashMap<String, bool>,
    over18: Option<bool>,
    section_hash_cache: HashMap<String, HashMap<String, String>>,
}

impl SnapshotDownloaderSettings {
    pub fn new(
        local: HashMap<String, bool>,
        over18: Option<bool>,
        section_hash_cache: HashMap<String, HashMap<String, String>>,
    ) -> Self {
        Self {
            local,
            over18,
            section_hash_cache,
        }
    }
}

impl DownloaderSettings for SnapshotDownloaderSettings {
    fn local_setting_bool(&self, key: &str) -> bool {
        self.local.get(key).copied().unwrap_or(false)
    }

    fn global_setting_optional_bool(&self, key: &str) -> Option<bool> {
        match key {
            "over18" => self.over18,
            _ => None,
        }
    }

    /// No-op (`Ok(())`): the trait is synchronous but D1 is asynchronous, so
    /// a snapshot cannot write back. Global settings such as `over18` are
    /// set on the Web UI / native side; the Worker only reads them.
    fn save_global_setting_bool(&self, _key: &str, _value: bool) -> Result<()> {
        Ok(())
    }

    fn download_use_subdirectory(&self) -> bool {
        self.local_setting_bool("download.use-subdirectory")
    }

    fn load_section_hash_cache(&self) -> HashMap<String, HashMap<String, String>> {
        self.section_hash_cache.clone()
    }

    /// No-op (`Ok(())`): the trait is synchronous but D1 is asynchronous, so
    /// the updated cache is not persisted from here. The cache is advisory
    /// (strong-update comparison); a stale or lost cache only re-downloads
    /// sections instead of corrupting state.
    fn save_section_hash_cache(
        &self,
        _cache: &HashMap<String, HashMap<String, String>>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Native capability: reads the `.narou` Inventory and prompts on the
/// terminal. Preserves the historical CLI settings/decision behavior.
#[cfg(feature = "native-runtime")]
#[derive(Debug, Clone, Copy)]
pub struct NativeDownloaderSettings;

#[cfg(feature = "native-runtime")]
impl DownloaderSettings for NativeDownloaderSettings {
    fn local_setting_bool(&self, key: &str) -> bool {
        crate::compat::load_local_setting_bool(key)
    }

    fn global_setting_optional_bool(&self, key: &str) -> Option<bool> {
        crate::db::settings::bool_value(crate::setting_core::SettingScope::Global, key)
    }

    fn save_global_setting_bool(&self, key: &str, value: bool) -> Result<()> {
        crate::db::settings::update(crate::setting_core::SettingScope::Global, |settings| {
            settings.insert(key.to_string(), serde_yaml::Value::Bool(value));
            Ok(())
        })
    }

    fn download_use_subdirectory(&self) -> bool {
        crate::db::settings::bool_value(
            crate::setting_core::SettingScope::Local,
            "download.use-subdirectory",
        )
        .unwrap_or(false)
    }

    fn strip_title_prefix_for(&self, novel_id: i64, raw_title: &str, author: &str) -> bool {
        let previous_novel_dir = crate::db::with_database(|db| {
            Ok(db.get(novel_id).map(|record| {
                crate::db::existing_novel_dir_for_record(
                    std::path::Path::new(crate::downloader::types::ARCHIVE_ROOT_DIR),
                    record,
                )
            }))
        })
        .ok()
        .flatten();
        let settings_dir = previous_novel_dir.unwrap_or_else(|| {
            std::path::PathBuf::from(crate::downloader::types::ARCHIVE_ROOT_DIR)
                .join(".new-novel-settings")
        });
        crate::converter::settings::NovelSettings::load_for_novel(
            novel_id,
            raw_title,
            author,
            &settings_dir,
        )
        .enable_strip_title_prefix
    }

    fn load_section_hash_cache(&self) -> HashMap<String, HashMap<String, String>> {
        crate::db::with_database(|db| {
            db.inventory().load(
                crate::downloader::SECTION_HASH_CACHE_NAME,
                crate::db::inventory::InventoryScope::Local,
            )
        })
        .unwrap_or_default()
    }

    fn save_section_hash_cache(
        &self,
        cache: &HashMap<String, HashMap<String, String>>,
    ) -> Result<()> {
        crate::db::with_database(|db| {
            db.inventory().save(
                crate::downloader::SECTION_HASH_CACHE_NAME,
                crate::db::inventory::InventoryScope::Local,
                cache,
            )?;
            Ok(())
        })
    }

    fn confirm(&self, message: &str, default: bool, nontty_default: bool) -> Result<bool> {
        Ok(crate::compat::confirm(message, default, nontty_default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_defaults_match_unset_settings() {
        let settings = SnapshotDownloaderSettings::default();
        assert!(!settings.local_setting_bool("update.strong"));
        assert!(!settings.local_setting_bool("guard-spoiler"));
        assert!(!settings.local_setting_bool("auto-add-tags"));
        assert!(!settings.local_setting_bool("download.use-subdirectory"));
        assert!(!settings.download_use_subdirectory());
        assert_eq!(settings.global_setting_optional_bool("over18"), None);
        assert!(settings.load_section_hash_cache().is_empty());
    }

    #[test]
    fn snapshot_reads_supplied_values() {
        let local = HashMap::from([
            ("update.strong".to_string(), true),
            ("guard-spoiler".to_string(), false),
            ("download.use-subdirectory".to_string(), true),
        ]);
        let cache = HashMap::from([(
            "42".to_string(),
            HashMap::from([("1/section.txt".to_string(), "abc".to_string())]),
        )]);
        let settings = SnapshotDownloaderSettings::new(local, Some(true), cache.clone());
        assert!(settings.local_setting_bool("update.strong"));
        assert!(!settings.local_setting_bool("guard-spoiler"));
        assert!(!settings.local_setting_bool("auto-add-tags"));
        assert!(settings.download_use_subdirectory());
        assert_eq!(settings.global_setting_optional_bool("over18"), Some(true));
        assert_eq!(settings.global_setting_optional_bool("other"), None);
        assert_eq!(settings.load_section_hash_cache(), cache);
    }

    #[test]
    fn snapshot_save_methods_are_noop_and_confirm_is_unsupported() {
        let settings = SnapshotDownloaderSettings::default();
        assert!(settings.save_global_setting_bool("over18", true).is_ok());
        assert!(settings.save_section_hash_cache(&HashMap::new()).is_ok());
        // `confirm` keeps the trait default: no terminal on this platform, so
        // the caller (executor) turns the Unsupported error into `Blocked`.
        assert!(matches!(
            settings.confirm("?", false, false),
            Err(crate::error::NarouError::Unsupported(_))
        ));
    }
}
