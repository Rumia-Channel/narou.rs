//! Per-novel settings service: `setting.ini` and `replace.txt` over the
//! object store.
//!
//! The web UI's novel settings page reads and writes two per-novel files:
//! `setting.ini` (converter options, INI format) and `replace.txt`
//! (tab-separated replace patterns). This service owns that logic against
//! injected platform ports only:
//!
//! - [`ObjectStore`] for the files, addressed by [`NovelObjectKeys::setting`]
//!   and [`NovelObjectKeys::replace`] (logical keys, never OS paths),
//! - [`NovelRepository`] to resolve the record (sitename / file_title /
//!   use_subdirectory) that determines the key namespace.
//!
//! Parsing reuses the existing pure parsers: `converter::ini::IniData` for
//! the INI file and the tab-separated replace parser from
//! `converter::settings`. Unknown INI keys are preserved on save. The
//! service never touches `Path`, the inventory, the database globals, or
//! the web framework.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::error::ApplicationError;
use crate::converter::ini::{IniData, IniValue};
use crate::db::NovelRecord;
use crate::platform::{NovelId, NovelObjectKeys, NovelRepository, ObjectStore};
use crate::setting_info::{VarInfo, VarType};

/// Maximum number of replace patterns accepted in one save (mirrors the web
/// layer's limit).
pub const MAX_REPLACE_PATTERNS: usize = 128;
/// Maximum length of a replace pattern's left side (mirrors the web layer).
pub const MAX_REPLACE_PATTERN_LENGTH: usize = 255;

/// One replace pattern: `left` is replaced by `right` during conversion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReplacePattern {
    pub left: String,
    pub right: String,
}

/// One per-novel setting entry: the converter option name, its metadata, and
/// its current value (or `None` when unset).
#[derive(Debug, Clone)]
pub struct NovelSettingEntry {
    pub name: String,
    pub help: String,
    pub var_type: crate::setting_info::VarType,
    pub select_keys: Option<Vec<String>>,
    pub value: Option<IniValue>,
}

/// The full settings view for one novel.
#[derive(Debug, Clone)]
pub struct NovelSettingsView {
    pub id: i64,
    pub title: String,
    pub author: String,
    /// One entry per known converter option, in metadata order.
    pub settings: Vec<NovelSettingEntry>,
    pub replace_patterns: Vec<ReplacePattern>,
}

impl NovelSettingsView {
    pub fn to_json_value(&self, runtime: crate::application::webui::capabilities::RuntimeKind) -> serde_json::Value {
        use crate::application::webui::capabilities::{RuntimeKind, worker_novel_setting_ineffective};
        let settings: Vec<serde_json::Value> = self.settings.iter().map(|entry| {
            let mut value = serde_json::json!({
                "name": entry.name, "help": entry.help, "var_type": entry.var_type,
                "select_keys": entry.select_keys, "value": ini_value_to_json(entry.value.as_ref()),
            });
            if runtime == RuntimeKind::Worker {
                let ineffective = worker_novel_setting_ineffective(&entry.name);
                value["worker_ineffective"] = serde_json::json!(ineffective);
                value["worker_note"] = serde_json::json!(ineffective.then_some(
                    "Cloudflare Workers 版では調査ログを生成できません。"));
            }
            value
        }).collect();
        serde_json::json!({
            "id": self.id, "title": self.title, "author": self.author,
            "settings": settings, "replace_patterns": self.replace_patterns,
        })
    }
}

/// A patch to apply to a novel's settings.
#[derive(Debug, Clone, Default)]
pub struct NovelSettingsPatch {
    /// Option name → new value. `None` clears the option (removes the INI
    /// key). Unknown names are rejected.
    pub settings: HashMap<String, Option<IniValue>>,
    /// When `Some`, replaces the whole replace-pattern list.
    pub replace_patterns: Option<Vec<ReplacePattern>>,
}

/// Acknowledges only committed fields if the second file cannot be saved.
#[derive(Debug, Default)]
pub struct NovelSettingsSaveResult {
    pub settings: HashMap<String, Option<IniValue>>,
    pub replace_patterns: Option<Vec<ReplacePattern>>,
    pub failure: Option<ApplicationError>,
}

impl NovelSettingsSaveResult {
    pub fn to_json_value(&self) -> serde_json::Value {
        let settings = self.settings.iter()
            .map(|(name, value)| (name.clone(), ini_value_to_json(value.as_ref())))
            .collect::<serde_json::Map<String, serde_json::Value>>();
        let mut payload = serde_json::json!({
            "success": self.failure.is_none(),
            "partial": self.failure.is_some() && (!self.settings.is_empty() || self.replace_patterns.is_some()),
            "message": self.failure.as_ref().map(ToString::to_string).unwrap_or_else(|| "Settings saved".into()),
            "settings": settings,
        });
        if let Some(patterns) = &self.replace_patterns {
            payload["replace_patterns"] = serde_json::json!(patterns);
        }
        payload
    }
}

/// Concrete per-novel settings service.
///
/// All dependencies are injected platform ports; the service is testable
/// with the platform mocks and usable from both the native web layer and a
/// future Worker backend.
pub struct NovelSettingsService {
    novels: Arc<dyn NovelRepository>,
    objects: Arc<dyn ObjectStore>,
}

impl NovelSettingsService {
    /// Create the service with injected dependencies.
    pub fn new(novels: Arc<dyn NovelRepository>, objects: Arc<dyn ObjectStore>) -> Self {
        Self { novels, objects }
    }

    /// Load the settings view for one novel.
    ///
    /// Missing `setting.ini` / `replace.txt` objects are treated as empty
    /// (matching the existing web behavior).
    pub async fn load(&self, id: NovelId) -> Result<NovelSettingsView, ApplicationError> {
        let record = self
            .novels
            .get(id)
            .await
            .map_err(ApplicationError::platform)?
            .ok_or_else(|| ApplicationError::NotFound(format!("ID: {}", id.0)))?;
        let keys = self.keys_for(&record)?;

        // setting.ini と replace.txt は独立なので同時に読む。エラーの優先
        // 順位は直列時と同じ (INI → replace) になるよう、この順で評価する。
        let (ini, replace) = futures::join!(self.load_ini(&keys), self.load_replace(&keys));
        let ini = ini.map_err(ApplicationError::platform)?;
        let replace = replace.map_err(ApplicationError::platform)?;

        let settings = build_setting_entries(&ini);
        Ok(NovelSettingsView {
            id: id.0,
            title: record.title,
            author: record.author,
            settings,
            replace_patterns: replace,
        })
    }

    /// Apply a patch and persist both files.
    ///
    /// Unknown setting names are rejected with
    /// [`ApplicationError::InvalidRequest`]; unknown INI keys already in the
    /// file are preserved. When `replace_patterns` is `None` the replace
    /// file is left untouched.
    pub async fn save(
        &self,
        id: NovelId,
        patch: &NovelSettingsPatch,
    ) -> Result<NovelSettingsSaveResult, ApplicationError> {
        let record = self
            .novels
            .get(id)
            .await
            .map_err(ApplicationError::platform)?
            .ok_or_else(|| ApplicationError::NotFound(format!("ID: {}", id.0)))?;
        let keys = self.keys_for(&record)?;

        let replace_content = patch.replace_patterns.as_deref()
            .map(serialize_replace_patterns).transpose()?;
        let mut result = NovelSettingsSaveResult::default();
        if !patch.settings.is_empty() {
            validate_patch_names(patch)?;
            let commit = {
                let mut mutate = |raw: Option<&[u8]>| {
                    let text = std::str::from_utf8(raw.unwrap_or_default()).map_err(|error| {
                        crate::error::NarouError::Platform(format!("setting.ini is not UTF-8: {error}"))
                    })?;
                    let mut ini = IniData::load(text);
                    apply_patch_to_ini(&mut ini, patch);
                    for name in patch.settings.keys() {
                        result.settings.insert(name.clone(), ini.get_global(name).cloned());
                    }
                    let content = ini.to_ini_string().into_bytes();
                    Ok((raw != Some(content.as_slice())).then_some(std::borrow::Cow::Owned(content)))
                };
                self.objects.update_small(&keys.setting(), &mut mutate)
                    .await.map_err(ApplicationError::platform)?
            };
            match commit {
                crate::platform::ObjectUpdateResult::Committed => {}
                crate::platform::ObjectUpdateResult::Conflict => return Err(ApplicationError::Conflict(
                    "設定が同時に更新されました。読み込み直してください。".into(),
                )),
                crate::platform::ObjectUpdateResult::ProjectionFailed(error) => {
                    result.failure = Some(ApplicationError::platform(error));
                    return Ok(result);
                }
            }
        }
        if let Some(content) = replace_content {
            let write = {
                let mut mutate = |raw: Option<&[u8]>| {
                    Ok((raw != Some(content.as_bytes())).then_some(std::borrow::Cow::Borrowed(content.as_bytes())))
                };
                self.objects.update_small(&keys.replace(), &mut mutate).await
            };
            match write {
                Ok(crate::platform::ObjectUpdateResult::Committed)
                | Ok(crate::platform::ObjectUpdateResult::ProjectionFailed(_)) => {
                    result.replace_patterns = patch.replace_patterns.as_ref().map(|patterns| {
                        patterns.iter().filter(|pattern| !pattern.left.is_empty()).cloned().collect()
                    });
                    if let Ok(crate::platform::ObjectUpdateResult::ProjectionFailed(error)) = write {
                        result.failure = Some(ApplicationError::platform(error));
                    }
                }
                Ok(crate::platform::ObjectUpdateResult::Conflict) => {
                    let error = ApplicationError::Conflict("置換設定が同時に更新されました。読み込み直してください。".into());
                    if result.settings.is_empty() { return Err(error); }
                    result.failure = Some(error);
                }
                Err(error) if result.settings.is_empty() => return Err(ApplicationError::platform(error)),
                Err(error) => result.failure = Some(ApplicationError::platform(error)),
            }
        }
        Ok(result)
    }

    /// Resolve the logical object keys for a record.
    fn keys_for(&self, record: &NovelRecord) -> Result<NovelObjectKeys, ApplicationError> {
        NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        )
        .map_err(ApplicationError::platform)
    }

    async fn load_ini(&self, keys: &NovelObjectKeys) -> crate::error::Result<IniData> {
        let Some(bytes) = self.objects.read_small(&keys.setting()).await? else {
            return Ok(IniData::new());
        };
        let text = String::from_utf8(bytes).map_err(|e| {
            crate::error::NarouError::Platform(format!("setting.ini is not UTF-8: {e}"))
        })?;
        Ok(IniData::load(&text))
    }

    async fn load_replace(&self, keys: &NovelObjectKeys) -> crate::error::Result<Vec<ReplacePattern>> {
        let Some(bytes) = self.objects.read_small(&keys.replace()).await? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(bytes).map_err(|e| {
            crate::error::NarouError::Platform(format!("replace.txt is not UTF-8: {e}"))
        })?;
        // Reuse the existing pure tab-separated line format (the converter
        // parser reads from a file; we parse the same format from bytes).
        Ok(parse_replace_text(&text))
    }
}

/// Build the settings entries for the known converter options, in metadata
/// order, reading values from the INI global section.
fn build_setting_entries(ini: &IniData) -> Vec<NovelSettingEntry> {
    crate::setting_info::original_setting_var_infos()
        .into_iter()
        .map(|(name, info)| NovelSettingEntry {
            name: name.to_string(),
            help: info.help.to_string(),
            var_type: info.var_type,
            select_keys: info.select_keys,
            value: ini.get_global(name).cloned(),
        })
        .collect()
}

/// Parse the shared Web UI request without performing any storage writes.
pub fn parse_settings_patch(body: &serde_json::Value) -> Result<NovelSettingsPatch, String> {
    let values = body.get("settings").and_then(serde_json::Value::as_array)
        .ok_or_else(|| "settings array required".to_string())?;
    let known: HashMap<&'static str, VarInfo> =
        crate::setting_info::original_setting_var_infos().into_iter().collect();
    let mut settings = HashMap::new();
    let mut parsed = IniData::new();
    for item in values {
        let Some(name) = item.get("name").and_then(serde_json::Value::as_str) else { continue };
        let info = known.get(name).ok_or_else(|| format!("不明な設定名です: {name}"))?;
        apply_setting_value(&mut parsed, name, info,
            item.get("value").unwrap_or(&serde_json::Value::Null))?;
        let value = parsed.sections.get_mut("global").and_then(|global| global.remove(name));
        settings.insert(name.to_string(), value);
    }
    let replace_patterns = body.get("replace_patterns").map(|value| {
        let values = value.as_array().ok_or_else(|| "replace_patterns must be an array".to_string())?;
        values.iter().map(|value| {
            let left = value.get("left").and_then(serde_json::Value::as_str)
                .ok_or_else(|| "replace pattern left is required".to_string())?;
            let right = value.get("right").and_then(serde_json::Value::as_str)
                .ok_or_else(|| "replace pattern right is required".to_string())?;
            Ok(ReplacePattern { left: left.to_string(), right: right.to_string() })
        }).collect::<Result<Vec<_>, String>>()
    }).transpose()?;
    Ok(NovelSettingsPatch { settings, replace_patterns })
}

/// Validate per-novel names before entering the storage mutation.
fn validate_patch_names(patch: &NovelSettingsPatch) -> Result<(), ApplicationError> {
    let known: HashMap<&str, crate::setting_info::VarInfo> =
        crate::setting_info::original_setting_var_infos()
            .into_iter()
            .collect();
    for name in patch.settings.keys() {
        if !known.contains_key(name.as_str()) {
            return Err(ApplicationError::InvalidRequest(format!(
                "不明な設定名です: {name}"
            )));
        }
    }
    Ok(())
}

/// Apply validated fields without reparsing metadata on a storage retry.
fn apply_patch_to_ini(ini: &mut IniData, patch: &NovelSettingsPatch) {
    for (name, value) in &patch.settings {
        match value {
            Some(value) => ini.set_global(name, value.clone()),
            None => {
                if let Some(global) = ini.sections.get_mut("global") {
                    global.remove(name);
                }
            }
        }
    }
}

pub fn ini_value_to_json(value: Option<&IniValue>) -> serde_json::Value {
    match value {
        None | Some(IniValue::Null) => serde_json::Value::Null,
        Some(IniValue::Boolean(b)) => serde_json::Value::Bool(*b),
        Some(IniValue::Integer(i)) => serde_json::json!(*i),
        Some(IniValue::Float(f)) => serde_json::json!(*f),
        Some(IniValue::String(s)) => serde_json::Value::String(s.clone()),
    }
}

pub fn apply_setting_value(
    ini: &mut IniData,
    name: &str,
    info: &VarInfo,
    value: &serde_json::Value,
) -> Result<(), String> {
    let parsed = match info.var_type {
        VarType::Boolean => parse_bool_value(value)?,
        VarType::Integer => parse_integer_value(value)?,
        VarType::Float => parse_float_value(value)?,
        VarType::String => parse_string_value(value)?,
        VarType::Select => parse_select_value(value, info.select_keys.as_deref())?,
        VarType::Multiple => parse_multiple_value(value, info.select_keys.as_deref())?,
        VarType::Directory => parse_string_value(value)?,
    };

    if let Some(parsed) = parsed {
        ini.set_global(name, parsed);
    } else if let Some(global) = ini.sections.get_mut("global") {
        global.remove(name);
    }

    Ok(())
}

fn parse_bool_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(flag) => Ok(Some(IniValue::Boolean(*flag))),
        serde_json::Value::String(raw) => {
            match raw.trim().to_ascii_lowercase().as_str() {
                "" => Ok(None),
                "true" => Ok(Some(IniValue::Boolean(true))),
                "false" => Ok(Some(IniValue::Boolean(false))),
                _ => Err("true か false を指定して下さい".to_string()),
            }
        }
        _ => Err("true か false を指定して下さい".to_string()),
    }
}

fn parse_integer_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(raw) => raw
            .as_i64()
            .map(|v| Some(IniValue::Integer(v)))
            .ok_or_else(|| "整数を指定して下さい".to_string()),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<i64>()
                    .map(|v| Some(IniValue::Integer(v)))
                    .map_err(|_| "整数を指定して下さい".to_string())
            }
        }
        _ => Err("整数を指定して下さい".to_string()),
    }
}

fn parse_float_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(raw) => raw
            .as_f64()
            .map(|v| Some(IniValue::Float(v)))
            .ok_or_else(|| "数値を指定して下さい".to_string()),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<f64>()
                    .map(|v| Some(IniValue::Float(v)))
                    .map_err(|_| "数値を指定して下さい".to_string())
            }
        }
        _ => Err("数値を指定して下さい".to_string()),
    }
}

fn parse_string_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(raw) => {
            if raw.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(IniValue::String(raw.clone())))
            }
        }
        serde_json::Value::Number(raw) => Ok(Some(IniValue::String(raw.to_string()))),
        serde_json::Value::Bool(raw) => Ok(Some(IniValue::String(raw.to_string()))),
        _ => Err("文字列を指定して下さい".to_string()),
    }
}

fn parse_select_value(
    value: &serde_json::Value,
    select_keys: Option<&[String]>,
) -> Result<Option<IniValue>, String> {
    let Some(keys) = select_keys else {
        return parse_string_value(value);
    };

    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if keys.iter().any(|key| key == trimmed) {
                Ok(Some(IniValue::String(trimmed.to_string())))
            } else {
                Err("選択肢の中から指定して下さい".to_string())
            }
        }
        _ => Err("選択肢の中から指定して下さい".to_string()),
    }
}

fn parse_multiple_value(
    value: &serde_json::Value,
    select_keys: Option<&[String]>,
) -> Result<Option<IniValue>, String> {
    let Some(keys) = select_keys else {
        return parse_string_value(value);
    };

    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Array(items) => {
            let mut selected = Vec::new();
            for item in items {
                let Some(raw) = item.as_str() else {
                    return Err("選択肢の中から指定して下さい".to_string());
                };
                if !keys.iter().any(|key| key == raw) {
                    return Err("選択肢の中から指定して下さい".to_string());
                }
                selected.push(raw.to_string());
            }
            if selected.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IniValue::String(selected.join(","))))
            }
        }
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IniValue::String(trimmed.to_string())))
            }
        }
        _ => Err("選択肢の中から指定して下さい".to_string()),
    }
}

/// Parse the converter's shared tab-separated replacement format.
fn parse_replace_text(text: &str) -> Vec<ReplacePattern> {
    crate::converter::settings::parse_replace_patterns(text).into_iter()
        .map(|(left, right)| ReplacePattern { left, right }).collect()
}

/// Serialize replace patterns to the tab-separated text format, validating
/// the count and length limits.
fn serialize_replace_patterns(patterns: &[ReplacePattern]) -> Result<String, ApplicationError> {
    if patterns.len() > MAX_REPLACE_PATTERNS {
        return Err(ApplicationError::InvalidRequest(format!(
            "too many replace patterns: {} (max {})",
            patterns.len(),
            MAX_REPLACE_PATTERNS
        )));
    }
    let mut lines = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let left = pattern.left.as_str();
        if left.is_empty() {
            continue;
        }
        if left.len() > MAX_REPLACE_PATTERN_LENGTH {
            return Err(ApplicationError::InvalidRequest(format!(
                "replace pattern is too long: {left:?}"
            )));
        }
        if left.contains('\t') || left.contains('\n') || left.contains('\r') {
            return Err(ApplicationError::InvalidRequest(
                "replace pattern left side must not contain tabs or newlines".to_string(),
            ));
        }
        let right = pattern.right.as_str();
        if right.contains(['\n', '\r']) {
            return Err(ApplicationError::InvalidRequest(
                "replace pattern right side must not contain newlines".to_string(),
            ));
        }
        lines.push(format!("{left}\t{right}"));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::{MemoryNovelRepository, MemoryObjectStore};

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".into(),
            title: format!("title {id}"),
            file_title: format!("title {id}"),
            toc_url: format!("https://example.com/novel/{id}"),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    fn service() -> (NovelSettingsService, Arc<MemoryNovelRepository>, Arc<MemoryObjectStore>) {
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![sample_record(1)]));
        let objects = Arc::new(MemoryObjectStore::new());
        let service = NovelSettingsService::new(repo.clone(), objects.clone());
        (service, repo, objects)
    }

    #[test]
    fn load_returns_defaults_for_missing_objects() {
        let (service, _, _) = service();
        let view = futures::executor::block_on(service.load(NovelId(1))).unwrap();
        assert!(view.replace_patterns.is_empty());
        let entry = view.settings.iter().find(|entry| entry.name == "enable_yokogaki").unwrap();
        assert_eq!(entry.value, None);
    }

    #[test]
    fn load_returns_not_found_for_unknown_id() {
        let (service, _, _) = service();
        let err = futures::executor::block_on(service.load(NovelId(99))).unwrap_err();
        assert!(matches!(err, ApplicationError::NotFound(_)));
    }

    #[test]
    fn saving_known_fields_preserves_other_sections_and_string_types() {
        let (service, _, objects) = service();
        let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
        futures::executor::block_on(objects.write_small(
            &keys.setting(),
            b"unknown_key = 42\nliteral_bool = \"false\"\n[1]\nliteral_number = \"123\"\n".to_vec(),
        )).unwrap();
        let mut patch = NovelSettingsPatch::default();
        patch.settings.insert("enable_yokogaki".to_string(), Some(IniValue::Boolean(true)));
        futures::executor::block_on(service.save(NovelId(1), &patch)).unwrap();
        let bytes = futures::executor::block_on(objects.read_small(&keys.setting())).unwrap().unwrap();
        let restored = IniData::load(std::str::from_utf8(&bytes).unwrap());
        assert_eq!(restored.get_global("unknown_key"), Some(&IniValue::Integer(42)));
        assert_eq!(restored.get_global("literal_bool"), Some(&IniValue::String("false".into())));
        assert_eq!(restored.get("1", "literal_number"), Some(&IniValue::String("123".into())));
        assert_eq!(restored.get_global("enable_yokogaki"), Some(&IniValue::Boolean(true)));
        assert_eq!(restored.get("1", "enable_yokogaki"), None);
    }

    #[test]
    fn save_rejects_unknown_setting_name() {
        let (service, _, _) = service();
        let mut patch = NovelSettingsPatch::default();
        patch
            .settings
            .insert("no-such-setting".to_string(), Some(IniValue::Boolean(true)));
        let err = futures::executor::block_on(service.save(NovelId(1), &patch)).unwrap_err();
        assert!(matches!(err, ApplicationError::InvalidRequest(_)));
    }

    #[test]
    fn invalid_replace_cannot_partially_change_novel_settings() {
        let (service, _, objects) = service();
        let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
        futures::executor::block_on(async {
            let original_ini = b"enable_yokogaki = false\nunknown_key = keep\n".to_vec();
            let original_replace = b"old\tnew\n".to_vec();
            objects.write_small(&keys.setting(), original_ini.clone()).await.unwrap();
            objects.write_small(&keys.replace(), original_replace.clone()).await.unwrap();
            let patch = NovelSettingsPatch {
                settings: HashMap::from_iter([("enable_yokogaki".into(), Some(IniValue::Boolean(true)))]),
                replace_patterns: Some(vec![ReplacePattern { left: "valid".into(), right: "bad\nline".into() }]),
            };
            assert!(matches!(service.save(NovelId(1), &patch).await, Err(ApplicationError::InvalidRequest(_))));
            assert_eq!(objects.read_small(&keys.setting()).await.unwrap().unwrap(), original_ini);
            assert_eq!(objects.read_small(&keys.replace()).await.unwrap().unwrap(), original_replace);
        });
    }



    #[test]
    fn replace_serializer_rejects_tabs_in_left() {
        let err = serialize_replace_patterns(&[ReplacePattern {
            left: "a\tb".into(),
            right: "c".into(),
        }])
        .unwrap_err();
        assert!(matches!(err, ApplicationError::InvalidRequest(_)));
    }

    /// setting.ini と replace.txt が両方読めなくても、直列時と同じく先に
    /// 読む setting.ini のエラーを返す (2 つの読みを join した後も順序を保つ)。
    #[test]
    fn load_reports_the_ini_error_before_the_replace_error() {
        let (service, _, objects) = service();
        let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
        futures::executor::block_on(async {
            objects
                .write_small(&keys.setting(), vec![0xff, 0xfe, 0xfd])
                .await
                .unwrap();
            objects
                .write_small(&keys.replace(), vec![0xff, 0xfe, 0xfd])
                .await
                .unwrap();
        });

        let error = futures::executor::block_on(service.load(NovelId(1))).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("setting.ini is not UTF-8"),
            "expected the setting.ini error to win, got: {message}"
        );
    }

    #[test]
    fn blank_numeric_values_are_removed() {
        let mut ini = IniData::new();
        let info = VarInfo {
            var_type: VarType::Integer,
            help: "",
            invisible: false,
            select_keys: None,
        };

        apply_setting_value(
            &mut ini,
            "to_page_break_threshold",
            &info,
            &serde_json::Value::String(String::new()),
        )
        .unwrap();
        assert!(ini.get_global("to_page_break_threshold").is_none());
    }

    #[test]
    fn blank_string_values_are_removed() {
        let mut ini = IniData::new();
        let info = VarInfo {
            var_type: VarType::String,
            help: "",
            invisible: false,
            select_keys: None,
        };

        apply_setting_value(
            &mut ini,
            "novel_title",
            &info,
            &serde_json::Value::String("   ".to_string()),
        )
        .unwrap();
        assert!(ini.get_global("novel_title").is_none());
    }
}
