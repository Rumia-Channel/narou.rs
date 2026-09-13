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
        crate::db::with_database(|db| {
            let settings: HashMap<String, serde_yaml::Value> = db.inventory().load(
                "global_setting",
                crate::db::inventory::InventoryScope::Global,
            )?;
            Ok(settings.get(key).and_then(|value| match value {
                serde_yaml::Value::Bool(v) => Some(*v),
                serde_yaml::Value::String(v) => {
                    Some(matches!(v.as_str(), "true" | "yes" | "on" | "1"))
                }
                serde_yaml::Value::Number(v) => Some(v.as_i64().unwrap_or(0) != 0),
                _ => None,
            }))
        })
        .ok()
        .flatten()
    }

    fn save_global_setting_bool(&self, key: &str, value: bool) -> Result<()> {
        crate::db::with_database_mut(|db| {
            let mut settings: HashMap<String, serde_yaml::Value> = db
                .inventory()
                .load(
                    "global_setting",
                    crate::db::inventory::InventoryScope::Global,
                )
                .unwrap_or_default();
            settings.insert(key.to_string(), serde_yaml::Value::Bool(value));
            db.inventory().save(
                "global_setting",
                crate::db::inventory::InventoryScope::Global,
                &settings,
            )?;
            Ok(())
        })
    }

    fn download_use_subdirectory(&self) -> bool {
        crate::db::with_database(|db| {
            let settings: HashMap<String, serde_yaml::Value> = db
                .inventory()
                .load("local_setting", crate::db::inventory::InventoryScope::Local)?;
            Ok(settings
                .get("download.use-subdirectory")
                .and_then(|value| value.as_bool())
                .unwrap_or(false))
        })
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
