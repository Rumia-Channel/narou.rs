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

    /// Replace one scope only while it still matches the loaded snapshot.
    /// A conflict returns false without replacing any setting values.
    fn compare_save<'a>(
        &'a self,
        scope: SettingScope,
        expected: &'a HashMap<String, serde_yaml::Value>,
        replacement: &'a HashMap<String, serde_yaml::Value>,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<bool>>;

    /// Load the global replacement text. Native adapters map this to the
    /// legacy root-level `replace.txt`; other backends may use a logical key.
    fn load_replace_content<'a>(
        &'a self,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<String>>;

    fn compare_save_replace_content<'a>(
        &'a self,
        expected: &'a str,
        replacement: &'a str,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<bool>>;
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

    fn compare_save<'a>(
        &'a self,
        scope: SettingScope,
        expected: &'a HashMap<String, serde_yaml::Value>,
        replacement: &'a HashMap<String, serde_yaml::Value>,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<bool>> {
        Box::pin(async move {
            let mut values = match scope {
                SettingScope::Local => self.local.lock(),
                SettingScope::Global => self.global.lock(),
            };
            if &*values != expected {
                return Ok(false);
            }
            values.clone_from(replacement);
            Ok(true)
        })
    }
    fn load_replace_content<'a>(
        &'a self,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<String>> {
        let content = self.replace_content.lock().clone();
        Box::pin(async move { Ok(content) })
    }

    fn compare_save_replace_content<'a>(
        &'a self,
        expected: &'a str,
        replacement: &'a str,
    ) -> crate::platform::PlatformFuture<'a, crate::error::Result<bool>> {
        Box::pin(async move {
            let mut content = self.replace_content.lock();
            if content.as_str() != expected {
                return Ok(false);
            }
            replacement.clone_into(&mut content);
            Ok(true)
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
    /// A live webui config value (`webui.*` or `concurrency`) changed.
    WebuiConfigChanged,
    /// A `server-*` security setting (`server-basic-auth.*` /
    /// `server-add-accepted-hosts` / `server-ws-add-accepted-domains` /
    /// `server-reverse-proxy.enable`) changed.
    ServerSecurityChanged,
    /// The `device` setting changed; device-related defaults were applied.
    DeviceRelatedDefaultsApplied,
    /// Native content/asset stores retain their construction-time backend.
    StorageRestartRequired,
}

/// Exact acknowledgements and effects of a save, including partial commits.
#[derive(Debug, Default)]
pub struct SettingsSaveResult {
    pub effects: Vec<SettingsEffect>,
    pub settings: HashMap<String, serde_yaml::Value>,
    pub replace_content: Option<String>,
    pub failure: Option<ApplicationError>,
}

/// Names whose values `GET /api/webui/config` serves live; a change to any of
/// them is reported as [`SettingsEffect::WebuiConfigChanged`]. Also used by the
/// native server-settings watcher to notice external edits.
pub const LIVE_WEBUI_CONFIG_NAMES: &[&str] = &[
    "webui.theme",
    "webui.table.reload-timing",
    "webui.performance-mode",
    "webui.new-tag-color",
    "tag.show-novel",
    "tag.show-manga",
    "tag.attributes-enabled",
    "webui.debug-mode",
    "concurrency",
];

/// One setting value with its metadata, for listing.
#[derive(Debug, Clone)]
pub struct SettingEntry {
    pub name: String,
    pub scope: SettingScope,
    pub value: Option<serde_yaml::Value>,
    pub default_value: Option<serde_yaml::Value>,
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
        self.update_raw(scope, name, |_| value).await
    }
    /// Compute compatibility metadata from the snapshot guarded by the write.
    pub async fn update_raw<F>(
        &self,
        scope: SettingScope,
        name: &str,
        update: F,
    ) -> Result<(), ApplicationError>
    where
        F: FnOnce(Option<&serde_yaml::Value>) -> serde_yaml::Value,
    {
        let original = self.store.load(scope).await.map_err(ApplicationError::platform)?;
        let value = update(original.get(name));
        if original.get(name) == Some(&value) {
            return Ok(());
        }
        let mut replacement = original.clone();
        replacement.insert(name.to_string(), value);
        if !self.store.compare_save(scope, &original, &replacement).await.map_err(ApplicationError::platform)? {
            return Err(settings_conflict());
        }
        Ok(())
    }



    /// Set one setting value, coercing a JSON value through the pure
    /// `setting_core` coercion. Returns the effects the change implies.
    pub async fn set(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<SettingsSaveResult, ApplicationError> {
        self.apply_json(&[(name.to_string(), value.clone())]).await
    }

    /// Delete one setting value (equivalent to setting it to null).
    pub async fn delete(&self, name: &str) -> Result<SettingsSaveResult, ApplicationError> {
        self.set(name, &serde_json::Value::Null).await
    }

    /// Apply several changes in one save. A `Null` value deletes the key.
    /// Returns the effects the batch implies.
    pub async fn apply(
        &self,
        changes: &[(String, serde_yaml::Value)],
    ) -> Result<SettingsSaveResult, ApplicationError> {
        self.apply_checked(changes, None).await
    }
    /// Coerce JSON values using the setting metadata, then apply the batch.
    pub async fn apply_json(
        &self,
        changes: &[(String, serde_json::Value)],
    ) -> Result<SettingsSaveResult, ApplicationError> {
        self.apply_json_checked(changes, None).await
    }

    /// Compare only the edited fields with the values shown to the client.
    /// The store also atomically compares the loaded scope to catch races
    /// between this validation and persistence.
    pub async fn apply_json_checked(
        &self,
        changes: &[(String, serde_json::Value)],
        expected: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<SettingsSaveResult, ApplicationError> {
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
        self.apply_checked(&coerced, expected).await
    }

    async fn apply_checked(
        &self,
        changes: &[(String, serde_yaml::Value)],
        expected: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<SettingsSaveResult, ApplicationError> {
        let mut local_changes = Vec::new();
        let mut global_changes = Vec::new();
        for (name, value) in changes {
            match self.scope_of(name) {
                Some(SettingScope::Local) => local_changes.push((name.clone(), value.clone())),
                Some(SettingScope::Global) => global_changes.push((name.clone(), value.clone())),
                None => return Err(ApplicationError::InvalidRequest(format!("不明な設定名です: {name}"))),
            }
        }
        let mut prepared = Vec::with_capacity(2);
        for (scope, updates) in [(SettingScope::Local, local_changes), (SettingScope::Global, global_changes)] {
            if updates.is_empty() {
                continue;
            }
            let original = self.store.load(scope).await.map_err(ApplicationError::platform)?;
            if let Some(expected) = expected {
                for (name, _) in &updates {
                    let shown = expected.get(name).ok_or_else(|| {
                        ApplicationError::InvalidRequest(format!("初期値がありません: {name}"))
                    })?;
                    let shown_raw: serde_yaml::Value = serde_yaml::to_value(shown).map_err(ApplicationError::platform)?;
                    let current = original.get(name).filter(|value| !value.is_null());
                    // JSON loses the integer/float distinction for integral float settings.
                    // Keep exact legacy snapshots valid before using the metadata coercion.
                    if current != (!shown_raw.is_null()).then_some(&shown_raw)
                        && (current.is_none() || coerce_json_setting_value(name, shown).ok().as_ref() != current)
                    {
                        return Err(settings_conflict());
                    }
                }
            }
            let mut replacement = original.clone();
            let effects = self.apply_changes(scope, &mut replacement, &updates)?;
            prepared.push((scope, original, replacement, effects, updates));
        }
        let mut result = SettingsSaveResult::default();
        for (scope, original, mut replacement, changed, updates) in prepared {
            if replacement != original {
                let failure = match self.store.compare_save(scope, &original, &replacement).await {
                    Ok(true) => None,
                    Ok(false) => Some(settings_conflict()),
                    Err(error) => Some(ApplicationError::platform(error)),
                };
                if let Some(failure) = failure {
                    if result.settings.is_empty() {
                        return Err(failure);
                    }
                    result.failure = Some(failure);
                    break;
                }
                result.effects.extend(changed);
            }
            for (name, _) in updates {
                let value = replacement.remove(&name).unwrap_or(serde_yaml::Value::Null);
                result.settings.insert(name, value);
            }
        }
        Ok(result)
    }

    /// List all known settings with their current values, grouped by scope.
    pub async fn list(&self) -> Result<Vec<SettingEntry>, ApplicationError> {
        let (local, global) = futures::join!(
            self.store.load(SettingScope::Local),
            self.store.load(SettingScope::Global),
        );
        let local = local.map_err(ApplicationError::platform)?;
        let global = global.map_err(ApplicationError::platform)?;

        let mut entries = Vec::new();
        let variables = crate::setting_info::setting_variables();
        for (name, info) in &variables.local {
            entries.push(SettingEntry {
                name: name.to_string(),
                scope: SettingScope::Local,
                value: local.get(*name).cloned(),
                default_value: crate::setting_info::default_local_setting_value(name),
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
                default_value: None,
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
                    default_value: None,
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
                default_value: None,
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
        let expected = self.load_replace_content().await?;
        self.save_replace_content_checked(&expected, content).await
    }

    pub async fn save_replace_content_checked(
        &self,
        expected: &str,
        content: &str,
    ) -> Result<(), ApplicationError> {
        if !self.store.compare_save_replace_content(expected, content).await.map_err(ApplicationError::platform)? {
            return Err(settings_conflict());
        }
        Ok(())
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
        let server_security_before = server_security_snapshot(settings);
        let device_before = settings.get("device").cloned();
        let storage_changed = scope == SettingScope::Local && changes.iter().any(|(name, value)| {
            storage_setting_requires_restart(name) && if value.is_null() {
                settings.contains_key(name)
            } else {
                settings.get(name) != Some(value)
            }
        });

        for (name, value) in changes {
            if value.is_null() {
                settings.remove(name);
            } else {
                settings.insert(name.clone(), value.clone());
            }
        }

        let mut effects = Vec::new();
        if storage_changed {
            effects.push(SettingsEffect::StorageRestartRequired);
        }
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
        if scope == SettingScope::Global
            && server_security_before != server_security_snapshot(settings)
        {
            effects.push(SettingsEffect::ServerSecurityChanged);
        }
        Ok(effects)
    }
}

fn settings_conflict() -> ApplicationError {
    ApplicationError::Conflict("他の画面または処理で設定が更新されました。変更内容を確認し、再読み込みしてから保存してください".to_string())
}

pub fn storage_setting_requires_restart(name: &str) -> bool {
    name == "sqlite.mirror-files" || name.starts_with("s3.")
}

/// `server-*` values that `narou_rs::web::server_security` derives runtime
/// behaviour from. Changing any of them re-applies the security snapshot
/// without a restart.
///
/// 値が変わったことを検出する設定名の一覧。`narou setting` 側も
/// 同じ一覧を見て、稼働中のサーバへ再適用を求める。
pub const SERVER_SECURITY_NAMES: &[&str] = &[
    "server-basic-auth.enable",
    "server-basic-auth.user",
    "server-basic-auth.password",
    "server-add-accepted-hosts",
    "server-ws-add-accepted-domains",
    "server-reverse-proxy.enable",
];

fn server_security_snapshot(
    settings: &HashMap<String, serde_yaml::Value>,
) -> Vec<(String, serde_yaml::Value)> {
    SERVER_SECURITY_NAMES
        .iter()
        .filter_map(|name| {
            settings
                .get(*name)
                .cloned()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

/// Snapshot of the auto-update schedule settings.
fn auto_schedule_snapshot(
    settings: &HashMap<String, serde_yaml::Value>,
) -> (
    Option<serde_yaml::Value>,
    Option<serde_yaml::Value>,
    Option<serde_yaml::Value>,
) {
    (
        settings.get("update.auto-schedule.enable").cloned(),
        settings.get("update.auto-schedule").cloned(),
        settings.get("update.auto-schedule.timezone").cloned(),
    )
}

/// Snapshot of the live webui config settings.
fn webui_config_snapshot(
    settings: &HashMap<String, serde_yaml::Value>,
) -> Vec<(String, serde_yaml::Value)> {
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
        assert!(effects.effects.is_empty());

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
        assert!(effects.effects.contains(&SettingsEffect::AutoScheduleChanged));
    }

    #[test]
    fn webui_config_change_reports_effect() {
        let (service, _) = service();
        let effects = futures::executor::block_on(service.set(
            "webui.theme",
            &serde_json::json!("Darkly"),
        ))
        .unwrap();
        assert!(effects.effects.contains(&SettingsEffect::WebuiConfigChanged));
    }
    #[test]
    fn server_security_change_reports_effect() {
        let (service, _) = service();
        let effects = futures::executor::block_on(service.set(
            "server-basic-auth.enable",
            &serde_json::json!(true),
        ))
        .unwrap();
        assert!(effects.effects.contains(&SettingsEffect::ServerSecurityChanged));
        // 無関係な global 設定では発火しない。
        let effects = futures::executor::block_on(service.set(
            "server-port",
            &serde_json::json!(33000),
        ))
        .unwrap();
        assert!(!effects.effects.contains(&SettingsEffect::ServerSecurityChanged));
    }


    #[test]
    fn device_change_applies_related_defaults() {
        let (service, store) = service();
        let effects = futures::executor::block_on(service.set(
            "device",
            &serde_json::json!("kindle"),
        ))
        .unwrap();
        assert!(effects.effects.contains(&SettingsEffect::DeviceRelatedDefaultsApplied));
        let snapshot = store.local_snapshot();
        assert_eq!(
            snapshot.get("default.enable_half_indent_bracket"),
            Some(&serde_yaml::Value::Bool(true))
        );
    }

    #[test]
    fn dirty_save_preserves_unsubmitted_and_unknown_settings() {
        let store = Arc::new(MemorySettingsStore::new().with_local(HashMap::from([
            ("convert.keep-txt".to_string(), serde_yaml::Value::Bool(true)),
            ("user.extension".to_string(), serde_yaml::Value::String("keep".to_string())),
        ])));
        let service = SettingsService::new(store.clone());
        futures::executor::block_on(async {
            let expected = serde_json::Map::from_iter([("concurrency".to_string(), serde_json::Value::Null)]);
            service.apply_json_checked(&[("concurrency".to_string(), serde_json::json!(false))], Some(&expected)).await.unwrap();
            let saved = store.load(SettingScope::Local).await.unwrap();
            assert_eq!(saved.get("concurrency"), Some(&serde_yaml::Value::Bool(false)));
            assert_eq!(saved.get("convert.keep-txt"), Some(&serde_yaml::Value::Bool(true)));
            assert_eq!(saved.get("user.extension").and_then(serde_yaml::Value::as_str), Some("keep"));
        });
    }

    #[test]
    fn checked_float_setting_accepts_integral_json_baseline_but_rejects_stale_values() {
        let (service, _) = service();
        futures::executor::block_on(async {
            service.set("download.interval", &serde_json::json!(60)).await.unwrap();
            let expected = serde_json::Map::from_iter([
                ("download.interval".to_string(), serde_json::json!(60)),
            ]);
            service.apply_json_checked(&[
                ("download.interval".to_string(), serde_json::json!(0.2)),
            ], Some(&expected)).await.unwrap();
            assert_eq!(service.get("download.interval").await.unwrap().and_then(|value| value.as_f64()), Some(0.2));
            let error = service.apply_json_checked(&[
                ("download.interval".to_string(), serde_json::json!(1.2)),
            ], Some(&expected)).await.unwrap_err();
            assert!(matches!(error, ApplicationError::Conflict(_)));
            assert_eq!(service.get("download.interval").await.unwrap().and_then(|value| value.as_f64()), Some(0.2));
        });
    }

    #[test]
    fn stale_dirty_value_rejects_all_prepared_changes() {
        let (service, store) = service();
        futures::executor::block_on(async {
            service.set("concurrency", &serde_json::json!(true)).await.unwrap();
            let expected = serde_json::Map::from_iter([
                ("concurrency".to_string(), serde_json::Value::Null),
                ("over18".to_string(), serde_json::Value::Null),
            ]);
            let error = service.apply_json_checked(&[
                ("over18".to_string(), serde_json::json!(true)),
                ("concurrency".to_string(), serde_json::json!(false)),
            ], Some(&expected)).await.unwrap_err();
            assert!(matches!(error, ApplicationError::Conflict(_)));
            assert_eq!(service.get("concurrency").await.unwrap(), Some(serde_yaml::Value::Bool(true)));
            assert!(!store.load(SettingScope::Global).await.unwrap().contains_key("over18"));
        });
    }

    #[test]
    fn stale_replace_content_does_not_replace_a_concurrent_edit() {
        let (service, _) = service();
        futures::executor::block_on(async {
            service.save_replace_content("new editor content").await.unwrap();
            let error = service.save_replace_content_checked("", "stale editor content").await.unwrap_err();
            assert!(matches!(error, ApplicationError::Conflict(_)));
            assert_eq!(service.load_replace_content().await.unwrap(), "new editor content");
        });
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
}
