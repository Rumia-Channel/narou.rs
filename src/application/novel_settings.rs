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

/// Maximum number of replace patterns accepted in one save (mirrors the web
/// layer's limit).
pub const MAX_REPLACE_PATTERNS: usize = 128;
/// Maximum length of a replace pattern's left side (mirrors the web layer).
pub const MAX_REPLACE_PATTERN_LENGTH: usize = 255;

/// One replace pattern: `left` is replaced by `right` during conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A patch to apply to a novel's settings.
#[derive(Debug, Clone, Default)]
pub struct NovelSettingsPatch {
    /// Option name → new value. `None` clears the option (removes the INI
    /// key). Unknown names are rejected.
    pub settings: HashMap<String, Option<IniValue>>,
    /// When `Some`, replaces the whole replace-pattern list.
    pub replace_patterns: Option<Vec<ReplacePattern>>,
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

        let ini = self
            .load_ini(&keys)
            .await
            .map_err(ApplicationError::platform)?;
        let replace = self
            .load_replace(&keys)
            .await
            .map_err(ApplicationError::platform)?;

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
    ) -> Result<(), ApplicationError> {
        let record = self
            .novels
            .get(id)
            .await
            .map_err(ApplicationError::platform)?
            .ok_or_else(|| ApplicationError::NotFound(format!("ID: {}", id.0)))?;
        let keys = self.keys_for(&record)?;

        let mut ini = self
            .load_ini(&keys)
            .await
            .map_err(ApplicationError::platform)?;
        apply_patch_to_ini(&mut ini, patch)?;
        self.objects
            .write_small(&keys.setting(), ini.to_ini_string().into_bytes())
            .await
            .map_err(ApplicationError::platform)?;

        if let Some(patterns) = &patch.replace_patterns {
            let content = serialize_replace_patterns(patterns)?;
            self.objects
                .write_small(&keys.replace(), content.into_bytes())
                .await
                .map_err(ApplicationError::platform)?;
        }

        Ok(())
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

/// Apply a patch to an `IniData`, preserving unknown keys.
fn apply_patch_to_ini(ini: &mut IniData, patch: &NovelSettingsPatch) -> Result<(), ApplicationError> {
    let known: HashMap<&str, crate::setting_info::VarInfo> =
        crate::setting_info::original_setting_var_infos()
            .into_iter()
            .collect();
    for (name, value) in &patch.settings {
        if !known.contains_key(name.as_str()) {
            return Err(ApplicationError::InvalidRequest(format!(
                "不明な設定名です: {name}"
            )));
        }
        match value {
            Some(value) => ini.set_global(name, value.clone()),
            None => {
                if let Some(global) = ini.sections.get_mut("global") {
                    global.remove(name);
                }
            }
        }
    }
    Ok(())
}

/// Parse tab-separated replace patterns from text, mirroring
/// `converter::settings::load_replace_patterns` (which reads from a file).
fn parse_replace_text(text: &str) -> Vec<ReplacePattern> {
    let mut patterns = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        if let Some((left, right)) = trimmed.split_once('\t') {
            patterns.push(ReplacePattern {
                left: left.to_string(),
                right: right.to_string(),
            });
        }
    }
    patterns
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
        let left = pattern.left.trim();
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
        let right = pattern.right.replace(['\t', '\n', '\r'], "");
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
        assert_eq!(view.id, 1);
        assert_eq!(view.title, "title 1");
        assert!(view.replace_patterns.is_empty());
        assert!(!view.settings.is_empty());
        assert!(view.settings.iter().all(|entry| entry.value.is_none()));
    }

    #[test]
    fn load_returns_not_found_for_unknown_id() {
        let (service, _, _) = service();
        let err = futures::executor::block_on(service.load(NovelId(99))).unwrap_err();
        assert!(matches!(err, ApplicationError::NotFound(_)));
    }

    #[test]
    fn save_preserves_unknown_ini_keys() {
        let (service, _, objects) = service();
        let keys = NovelObjectKeys::new("example", "title 1", false).unwrap();
        futures::executor::block_on(
            objects.write_small(&keys.setting(), b"unknown_key = 42\n".to_vec()),
        )
        .unwrap();

        let mut patch = NovelSettingsPatch::default();
        patch
            .settings
            .insert("enable_yokogaki".to_string(), Some(IniValue::Boolean(true)));
        futures::executor::block_on(service.save(NovelId(1), &patch)).unwrap();

        let bytes = futures::executor::block_on(objects.read_small(&keys.setting()))
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("unknown_key = 42"));
        assert!(text.contains("enable_yokogaki = true"));
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
    fn save_roundtrips_replace_patterns() {
        let (service, _, _) = service();
        let patch = NovelSettingsPatch {
            settings: HashMap::new(),
            replace_patterns: Some(vec![
                ReplacePattern {
                    left: "foo".into(),
                    right: "bar".into(),
                },
                ReplacePattern {
                    left: "a".into(),
                    right: "b".into(),
                },
            ]),
        };
        futures::executor::block_on(service.save(NovelId(1), &patch)).unwrap();

        let view = futures::executor::block_on(service.load(NovelId(1))).unwrap();
        assert_eq!(
            view.replace_patterns,
            vec![
                ReplacePattern {
                    left: "foo".into(),
                    right: "bar".into(),
                },
                ReplacePattern {
                    left: "a".into(),
                    right: "b".into(),
                },
            ]
        );
    }

    #[test]
    fn replace_parser_matches_existing_line_format() {
        let patterns = parse_replace_text("; comment\nfoo\tbar\n\n  a\tb  \n");
        assert_eq!(
            patterns,
            vec![
                ReplacePattern {
                    left: "foo".into(),
                    right: "bar".into(),
                },
                ReplacePattern {
                    left: "a".into(),
                    right: "b".into(),
                },
            ]
        );
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
}
