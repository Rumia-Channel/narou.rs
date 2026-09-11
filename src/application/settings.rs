//! Application settings service: typed get/set/delete/list over an
//! application-owned [`SettingsStore`] port.
//!
//! The web UI's settings page reads and writes local (`local_setting.yaml`)
//! and global (`global_setting.yaml`) YAML files. This service owns that
//! logic against an injected [`SettingsStore`] port — the native adapter
//! wraps the inventory, a Worker adapter can wrap D1 — and the pure
//! `setting_core` / `setting_info` types for scope resolution and value
//! coercion. The service never imports the inventory, the database
//! globals, the filesystem, or the web framework.
//!
//! A [`MemorySettingsStore`] is provided for tests and for wiring before a
//! real adapter exists.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::error::ApplicationError;
use crate::setting_core::{SettingScope, coerce_json_setting_value, setting_scope};

/// A settings store: a map of setting name → YAML value per scope.
///
/// The native adapter persists to `local_setting.yaml` / `global_setting.yaml`
/// via the inventory; the service only sees the typed map.
pub trait SettingsStore: Send + Sync {
    /// Load the current settings for a scope (an empty map when unset).
    fn load<'a>(
        &'a self,
        scope: SettingScope,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<HashMap<String, serde_yaml::Value>>>;

    /// Persist the full settings map for a scope.
    fn save<'a>(
        &'a self,
        scope: SettingScope,
        settings: &'a HashMap<String, serde_yaml::Value>,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<()>>;

    /// Load the global replacement text. Native adapters map this to the
    /// legacy root-level `replace.txt`; other backends may use a logical key.
    fn load_replace_content<'a>(
        &'a self,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<String>> {
        Box::pin(async { Ok(String::new()) })
    }

    /// Persist the global replacement text.
    fn save_replace_content<'a>(
        &'a self,
        content: &'a str,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<()>> {
        let _ = content;
        Box::pin(async { Ok(()) })
    }
}

/// In-memory settings store for tests and early wiring.
#[derive(Debug, Default)]
pub struct MemorySettingsStore {
    local: parking_lot::Mutex<HashMap<String, serde_yaml::Value>>,
    global: parking_lot::Mutex<HashMap<String, serde_yaml::Value>>,
    replace_content: parking_lot::Mutex<String>,
}

impl MemorySettingsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the local scope with initial values.
    pub fn with_local(self, settings: HashMap<String, serde_yaml::Value>) -> Self {
        *self.local.lock() = settings;
        self
    }

    /// Seed the global scope with initial values.
    pub fn with_global(self, settings: HashMap<String, serde_yaml::Value>) -> Self {
        *self.global.lock() = settings;
        self
    }

    /// The current local settings (for assertions in tests).
    pub fn local_snapshot(&self) -> HashMap<String, serde_yaml::Value> {
        self.local.lock().clone()
    }

    /// The current global settings (for assertions in tests).
    pub fn global_snapshot(&self) -> HashMap<String, serde_yaml::Value> {
        self.global.lock().clone()
    }
    pub fn with_replace_content(self, content: String) -> Self {
        *self.replace_content.lock() = content;
        self
    }

    pub fn replace_content_snapshot(&self) -> String {
        self.replace_content.lock().clone()
    }
}

impl SettingsStore for MemorySettingsStore {
    fn load<'a>(
        &'a self,
        scope: SettingScope,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<HashMap<String, serde_yaml::Value>>>
    {
        let snapshot = match scope {
            SettingScope::Local => self.local.lock().clone(),
            SettingScope::Global => self.global.lock().clone(),
        };
        Box::pin(async move { Ok(snapshot) })
    }

    fn save<'a>(
        &'a self,
        scope: SettingScope,
        settings: &'a HashMap<String, serde_yaml::Value>,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<()>> {
        let settings = settings.clone();
        Box::pin(async move {
            match scope {
                SettingScope::Local => *self.local.lock() = settings,
                SettingScope::Global => *self.global.lock() = settings,
            }
            Ok(())
        })
    }
    fn load_replace_content<'a>(
        &'a self,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<String>> {
        let content = self.replace_content.lock().clone();
        Box::pin(async move { Ok(content) })
    }

    fn save_replace_content<'a>(
        &'a self,
        content: &'a str,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<()>> {
        let content = content.to_string();
        Box::pin(async move {
            *self.replace_content.lock() = content;
            Ok(())
        })
    }
}

/// Side effects a settings change may trigger in the presentation layer.
///
/// The service reports these; the web layer decides what to do (restart the
/// auto-update scheduler, reload live webui config, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsEffect {
    /// The auto-update schedule (`update.auto-schedule.enable` /
    /// `update.auto-schedule`) changed.
    AutoScheduleChanged,
    /// A live webui config value (`webui.*`) changed.
    WebuiConfigChanged,
    /// The `device` setting changed; device-related defaults were applied.
    DeviceRelatedDefaultsApplied,
}

/// One setting value with its metadata, for listing.
#[derive(Debug, Clone)]
pub struct SettingEntry {
    pub name: String,
    pub scope: SettingScope,
    pub value: Option<serde_yaml::Value>,
    pub var_type: crate::setting_info::VarType,
    pub help: String,
    pub select_keys: Option<Vec<String>>,
}

/// Concrete settings service.
///
/// All dependencies are injected; the service is testable with
/// [`MemorySettingsStore`] and usable from both the native web layer and a
/// future Worker backend.
pub struct SettingsService {
    store: Arc<dyn SettingsStore>,
}

impl SettingsService {
    /// Create the service with an injected store.
    pub fn new(store: Arc<dyn SettingsStore>) -> Self {
        Self { store }
    }

    /// Resolve the scope of a setting name, or `None` when unknown.
    pub fn scope_of(&self, name: &str) -> Option<SettingScope> {
        setting_scope(name)
    }

    /// Get one typed setting value.
    pub async fn get(
        &self,
        name: &str,
    ) -> Result<Option<serde_yaml::Value>, ApplicationError> {
        let scope = self
            .scope_of(name)
            .ok_or_else(|| ApplicationError::InvalidRequest(format!("不明な設定名です: {name}")))?;
        let settings = self
            .store
            .load(scope)
            .await
            .map_err(ApplicationError::platform)?;
        Ok(settings.get(name).cloned())
    }

    /// Get several typed setting values with at most one store load per
    /// scope. Results are returned in the same order as `names`; an unknown
    /// setting name fails the whole call, matching [`Self::get`].
    pub async fn get_many(
        &self,
        names: &[&str],
    ) -> Result<Vec<Option<serde_yaml::Value>>, ApplicationError> {
        let mut local: Option<HashMap<String, serde_yaml::Value>> = None;
        let mut global: Option<HashMap<String, serde_yaml::Value>> = None;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let scope = self
                .scope_of(name)
                .ok_or_else(|| {
                    ApplicationError::InvalidRequest(format!("不明な設定名です: {name}"))
                })?;
            let settings = match scope {
                SettingScope::Local => {
                    if local.is_none() {
                        local = Some(
                            self.store
                                .load(scope)
                                .await
                                .map_err(ApplicationError::platform)?,
                        );
                    }
                    local.as_ref().unwrap()
                }
                SettingScope::Global => {
                    if global.is_none() {
                        global = Some(
                            self.store
                                .load(scope)
                                .await
                                .map_err(ApplicationError::platform)?,
                        );
                    }
                    global.as_ref().unwrap()
                }
            };
            out.push(settings.get(*name).cloned());
        }
        Ok(out)
    }
    /// Read a raw value without requiring a built-in setting definition.
    ///
    /// This is reserved for compatibility metadata such as feature-tour
    /// markers and server extension settings.
    pub async fn get_raw(
        &self,
        scope: SettingScope,
        name: &str,
    ) -> Result<Option<serde_yaml::Value>, ApplicationError> {
        let settings = self
            .store
            .load(scope)
            .await
            .map_err(ApplicationError::platform)?;
        Ok(settings.get(name).cloned())
    }
    pub async fn web_target_limit(&self, fallback: usize) -> usize {
        self.get_raw(SettingScope::Global, "server-max-targets-per-request")
            .await
            .ok()
            .flatten()
            .and_then(|value| value.as_u64())
            .filter(|&value| value > 0)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(fallback)
    }
    /// Persist a raw compatibility setting without built-in metadata.
    pub async fn set_raw(
        &self,
        scope: SettingScope,
        name: &str,
        value: serde_yaml::Value,
    ) -> Result<(), ApplicationError> {
        let mut settings = self
            .store
            .load(scope)
            .await
            .map_err(ApplicationError::platform)?;
        settings.insert(name.to_string(), value);
        self.store
            .save(scope, &settings)
            .await
            .map_err(ApplicationError::platform)
    }


    /// Set one setting value, coercing a JSON value through the pure
    /// `setting_core` coercion. Returns the effects the change implies.
    pub async fn set(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<Vec<SettingsEffect>, ApplicationError> {
        let scope = self
            .scope_of(name)
            .ok_or_else(|| ApplicationError::InvalidRequest(format!("不明な設定名です: {name}")))?;
        let yaml_value = coerce_json_setting_value(name, value)
            .map_err(|message| ApplicationError::InvalidRequest(format!("{name}: {message}")))?;
        let mut settings = self
            .store
            .load(scope)
            .await
            .map_err(ApplicationError::platform)?;
        let effects = self.apply_changes(scope, &mut settings, &[(name.to_string(), yaml_value)])?;
        self.store
            .save(scope, &settings)
            .await
            .map_err(ApplicationError::platform)?;
        Ok(effects)
    }

    /// Delete one setting value (equivalent to setting it to null).
    pub async fn delete(&self, name: &str) -> Result<Vec<SettingsEffect>, ApplicationError> {
        let scope = self
            .scope_of(name)
            .ok_or_else(|| ApplicationError::InvalidRequest(format!("不明な設定名です: {name}")))?;
        let mut settings = self
            .store
            .load(scope)
            .await
            .map_err(ApplicationError::platform)?;
        let effects = self.apply_changes(scope, &mut settings, &[(name.to_string(), serde_yaml::Value::Null)])?;
        self.store
            .save(scope, &settings)
            .await
            .map_err(ApplicationError::platform)?;
        Ok(effects)
    }

    /// Apply several changes in one save. A `Null` value deletes the key.
    /// Returns the effects the batch implies.
    pub async fn apply(
        &self,
        changes: &[(String, serde_yaml::Value)],
    ) -> Result<Vec<SettingsEffect>, ApplicationError> {
        let mut local_changes: Vec<(String, serde_yaml::Value)> = Vec::new();
        let mut global_changes: Vec<(String, serde_yaml::Value)> = Vec::new();
        for (name, value) in changes {
            match self.scope_of(name) {
                Some(SettingScope::Local) => local_changes.push((name.clone(), value.clone())),
                Some(SettingScope::Global) => global_changes.push((name.clone(), value.clone())),
                None => {
                    return Err(ApplicationError::InvalidRequest(format!(
                        "不明な設定名です: {name}"
                    )));
                }
            }
        }

        let mut effects = Vec::new();
        if !local_changes.is_empty() {
            let mut settings = self
                .store
                .load(SettingScope::Local)
                .await
                .map_err(ApplicationError::platform)?;
            effects.extend(
                self.apply_changes(SettingScope::Local, &mut settings, &local_changes)?,
            );
            self.store
                .save(SettingScope::Local, &settings)
                .await
                .map_err(ApplicationError::platform)?;
        }
        if !global_changes.is_empty() {
            let mut settings = self
                .store
                .load(SettingScope::Global)
                .await
                .map_err(ApplicationError::platform)?;
            effects.extend(
                self.apply_changes(SettingScope::Global, &mut settings, &global_changes)?,
            );
            self.store
                .save(SettingScope::Global, &settings)
                .await
                .map_err(ApplicationError::platform)?;
        }
        Ok(effects)
    }
    /// Coerce JSON values using the setting metadata, then apply the batch.
    pub async fn apply_json(
        &self,
        changes: &[(String, serde_json::Value)],
    ) -> Result<Vec<SettingsEffect>, ApplicationError> {
        let mut coerced = Vec::with_capacity(changes.len());
        for (name, value) in changes {
            let value = if value.is_null() {
                serde_yaml::Value::Null
            } else {
                coerce_json_setting_value(name, value).map_err(|message| {
                    ApplicationError::InvalidRequest(format!("{name}: {message}"))
                })?
            };
            coerced.push((name.clone(), value));
        }
        self.apply(&coerced).await
    }

    /// List all known settings with their current values, grouped by scope.
    pub async fn list(&self) -> Result<Vec<SettingEntry>, ApplicationError> {
        let local = self
            .store
            .load(SettingScope::Local)
            .await
            .map_err(ApplicationError::platform)?;
        let global = self
            .store
            .load(SettingScope::Global)
            .await
            .map_err(ApplicationError::platform)?;

        let mut entries = Vec::new();
        let variables = crate::setting_info::setting_variables();
        for (name, info) in &variables.local {
            entries.push(SettingEntry {
                name: name.to_string(),
                scope: SettingScope::Local,
                value: local
                    .get(*name)
                    .cloned()
                    .or_else(|| crate::setting_info::default_local_setting_value(name)),
                var_type: info.var_type,
                help: info.help.to_string(),
                select_keys: info.select_keys.clone(),
            });
        }
        for (name, info) in &variables.global {
            entries.push(SettingEntry {
                name: name.to_string(),
                scope: SettingScope::Global,
                value: global.get(*name).cloned(),
                var_type: info.var_type,
                help: info.help.to_string(),
                select_keys: info.select_keys.clone(),
            });
        }
        for prefix in ["default", "force"] {
            for (base_name, info) in crate::setting_info::original_setting_var_infos() {
                let name = format!("{prefix}.{base_name}");
                entries.push(SettingEntry {
                    value: local.get(&name).cloned(),
                    name,
                    scope: SettingScope::Local,
                    var_type: info.var_type,
                    help: info.help.to_string(),
                    select_keys: info.select_keys,
                });
            }
        }
        for command in crate::setting_info::default_arg_command_names() {
            let name = format!("default_args.{command}");
            entries.push(SettingEntry {
                value: local.get(&name).cloned(),
                name,
                scope: SettingScope::Local,
                var_type: crate::setting_info::VarType::String,
                help: format!("{command} コマンドのデフォルトオプション"),
                select_keys: None,
            });
        }
        Ok(entries)
    }

    pub async fn load_replace_content(&self) -> Result<String, ApplicationError> {
        self.store
            .load_replace_content()
            .await
            .map_err(ApplicationError::platform)
    }

    pub async fn save_replace_content(&self, content: &str) -> Result<(), ApplicationError> {
        self.store
            .save_replace_content(content)
            .await
            .map_err(ApplicationError::platform)
    }

    /// Apply changes to one scope's map, computing effects. `Null` deletes.
    fn apply_changes(
        &self,
        scope: SettingScope,
        settings: &mut HashMap<String, serde_yaml::Value>,
        changes: &[(String, serde_yaml::Value)],
    ) -> Result<Vec<SettingsEffect>, ApplicationError> {
        let auto_schedule_before = auto_schedule_snapshot(settings);
        let webui_before = webui_config_snapshot(settings);
        let device_before = settings.get("device").cloned();

        for (name, value) in changes {
            if value.is_null() {
                settings.remove(name);
            } else {
                settings.insert(name.clone(), value.clone());
            }
        }

        let mut effects = Vec::new();
        if scope == SettingScope::Local {
            if auto_schedule_before != auto_schedule_snapshot(settings) {
                effects.push(SettingsEffect::AutoScheduleChanged);
            }
            if webui_before != webui_config_snapshot(settings) {
                effects.push(SettingsEffect::WebuiConfigChanged);
            }
            if device_before != settings.get("device").cloned()
                && crate::setting_core::apply_device_related_settings(settings).is_some()
            {
                effects.push(SettingsEffect::DeviceRelatedDefaultsApplied);
            }
        }
        Ok(effects)
    }
}

/// Snapshot of the auto-update schedule settings.
fn auto_schedule_snapshot(
    settings: &HashMap<String, serde_yaml::Value>,
) -> (Option<serde_yaml::Value>, Option<serde_yaml::Value>) {
    (
        settings.get("update.auto-schedule.enable").cloned(),
        settings.get("update.auto-schedule").cloned(),
    )
}

/// Snapshot of the live webui config settings.
fn webui_config_snapshot(
    settings: &HashMap<String, serde_yaml::Value>,
) -> Vec<(String, serde_yaml::Value)> {
    const LIVE_WEBUI_CONFIG_NAMES: &[&str] = &[
        "webui.theme",
        "webui.table.reload-timing",
        "webui.performance-mode",
        "webui.new-tag-color",
        "webui.debug-mode",
    ];
    LIVE_WEBUI_CONFIG_NAMES
        .iter()
        .filter_map(|name| {
            settings
                .get(*name)
                .cloned()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (SettingsService, Arc<MemorySettingsStore>) {
        let store = Arc::new(MemorySettingsStore::new());
        let service = SettingsService::new(store.clone());
        (service, store)
    }

    #[test]
    fn scope_resolution_uses_setting_core() {
        let (service, _) = service();
        assert_eq!(service.scope_of("update.interval"), Some(SettingScope::Local));
        assert_eq!(service.scope_of("server-port"), Some(SettingScope::Global));
        assert_eq!(service.scope_of("no-such-setting"), None);
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let (service, store) = service();
        let effects = futures::executor::block_on(service.set(
            "update.interval",
            &serde_json::json!(1.5),
        ))
        .unwrap();
        assert!(effects.is_empty());

        let value = futures::executor::block_on(service.get("update.interval"))
            .unwrap()
            .unwrap();
        assert_eq!(value, serde_yaml::Value::Number(serde_yaml::Number::from(1.5)));

        futures::executor::block_on(service.delete("update.interval")).unwrap();
        assert!(futures::executor::block_on(service.get("update.interval"))
            .unwrap()
            .is_none());
        assert!(store.local_snapshot().is_empty());
    }

    #[test]
    fn set_rejects_unknown_name() {
        let (service, _) = service();
        let err = futures::executor::block_on(service.set(
            "no-such-setting",
            &serde_json::json!(1),
        ))
        .unwrap_err();
        assert!(matches!(err, ApplicationError::InvalidRequest(_)));
    }

    #[test]
    fn auto_schedule_change_reports_effect() {
        let (service, _) = service();
        let effects = futures::executor::block_on(service.set(
            "update.auto-schedule.enable",
            &serde_json::json!(true),
        ))
        .unwrap();
        assert!(effects.contains(&SettingsEffect::AutoScheduleChanged));
    }

    #[test]
    fn webui_config_change_reports_effect() {
        let (service, _) = service();
        let effects = futures::executor::block_on(service.set(
            "webui.theme",
            &serde_json::json!("Darkly"),
        ))
        .unwrap();
        assert!(effects.contains(&SettingsEffect::WebuiConfigChanged));
    }

    #[test]
    fn device_change_applies_related_defaults() {
        let (service, store) = service();
        let effects = futures::executor::block_on(service.set(
            "device",
            &serde_json::json!("kindle"),
        ))
        .unwrap();
        assert!(effects.contains(&SettingsEffect::DeviceRelatedDefaultsApplied));
        let snapshot = store.local_snapshot();
        assert_eq!(
            snapshot.get("default.enable_half_indent_bracket"),
            Some(&serde_yaml::Value::Bool(true))
        );
    }

    #[test]
    fn list_returns_known_settings_with_values() {
        let mut local = HashMap::new();
        local.insert(
            "update.interval".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(2.0)),
        );
        let store = Arc::new(MemorySettingsStore::new().with_local(local));
        let service = SettingsService::new(store);

        let entries = futures::executor::block_on(service.list()).unwrap();
        assert!(entries.iter().any(|entry| {
            entry.name == "update.interval"
                && entry.scope == SettingScope::Local
                && entry.value.is_some()
        }));
        assert!(entries.iter().any(|entry| {
            entry.name == "server-port" && entry.scope == SettingScope::Global
        }));
    }
    #[test]
    fn raw_compatibility_settings_roundtrip_and_target_limit() {
        let mut global = HashMap::new();
        global.insert(
            "server-max-targets-per-request".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(42)),
        );
        let store = Arc::new(MemorySettingsStore::new().with_global(global));
        let service = SettingsService::new(store);
        assert_eq!(
            futures::executor::block_on(service.web_target_limit(100_000)),
            42
        );
        futures::executor::block_on(service.set_raw(
            SettingScope::Local,
            "webui.feature-tour.disabled",
            serde_yaml::Value::Bool(true),
        ))
        .unwrap();
        assert_eq!(
            futures::executor::block_on(service.get_raw(
                SettingScope::Local,
                "webui.feature-tour.disabled",
            ))
            .unwrap(),
            Some(serde_yaml::Value::Bool(true))
        );
    }
}
