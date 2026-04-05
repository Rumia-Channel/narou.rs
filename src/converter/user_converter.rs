use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use super::converter_base::TextType;
use super::settings::NovelSettings;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UserConverter {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub before: Vec<ReplaceRule>,
    #[serde(default)]
    pub after: Vec<ReplaceRule>,
    #[serde(default)]
    pub before_settings: Vec<SettingOverride>,
    #[serde(default)]
    pub after_settings: Vec<SettingOverride>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplaceRule {
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub text_type: Vec<String>,
    #[serde(default)]
    pub prepend_blank: bool,
    #[serde(default)]
    pub append_blank: bool,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub multiline: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingOverride {
    pub key: String,
    pub value: serde_yaml::Value,
}

#[derive(Debug, Clone)]
struct CompiledReplaceRule {
    regex: Regex,
    replacement: String,
    text_types: Vec<String>,
    prepend_blank: bool,
    append_blank: bool,
}

impl UserConverter {
    pub fn load(archive_path: &Path) -> Option<Self> {
        let yaml_path = archive_path.join("converter.yaml");
        if !yaml_path.exists() {
            let rb_path = archive_path.join("converter.rb");
            if rb_path.exists() {
                eprintln!(
                    "Warning: converter.rb found but not supported. Use converter.yaml instead."
                );
            }
            return None;
        }
        let content = std::fs::read_to_string(&yaml_path).ok()?;
        let converter: UserConverter = serde_yaml::from_str(&content).ok()?;
        Some(converter)
    }

    pub fn load_with_title(archive_path: &Path, novel_title: &str) -> Option<Self> {
        let converter = Self::load(archive_path)?;
        if converter.title.is_empty() {
            return Some(converter);
        }
        if converter.title == novel_title {
            Some(converter)
        } else {
            None
        }
    }

    fn compile_rules(rules: &[ReplaceRule]) -> Vec<CompiledReplaceRule> {
        rules
            .iter()
            .filter_map(|rule| {
                let mut pattern = rule.pattern.clone();
                if rule.case_insensitive {
                    pattern = format!("(?i){}", pattern);
                }
                if rule.multiline {
                    pattern = format!("(?m){}", pattern);
                }
                let regex = Regex::new(&pattern).ok()?;
                Some(CompiledReplaceRule {
                    regex,
                    replacement: rule.replacement.clone(),
                    text_types: rule.text_type.clone(),
                    prepend_blank: rule.prepend_blank,
                    append_blank: rule.append_blank,
                })
            })
            .collect()
    }

    pub fn apply_before(
        &self,
        text: &str,
        text_type: TextType,
        _settings: &mut NovelSettings,
    ) -> String {
        let compiled = Self::compile_rules(&self.before);
        let mut result = text.to_string();
        for rule in &compiled {
            if !rule.text_types.is_empty() {
                let type_str = text_type_to_str(text_type);
                if !rule.text_types.iter().any(|t| t == type_str) {
                    continue;
                }
            }
            if rule.prepend_blank && !result.is_empty() {
                result.insert(0, '\n');
            }
            result = rule
                .regex
                .replace_all(&result, rule.replacement.as_str())
                .to_string();
            if rule.append_blank && !result.is_empty() {
                result.push('\n');
            }
        }
        result
    }

    pub fn apply_after(
        &self,
        text: &str,
        text_type: TextType,
        _settings: &mut NovelSettings,
    ) -> String {
        let compiled = Self::compile_rules(&self.after);
        let mut result = text.to_string();
        for rule in &compiled {
            if !rule.text_types.is_empty() {
                let type_str = text_type_to_str(text_type);
                if !rule.text_types.iter().any(|t| t == type_str) {
                    continue;
                }
            }
            if rule.prepend_blank && !result.is_empty() {
                result.insert(0, '\n');
            }
            result = rule
                .regex
                .replace_all(&result, rule.replacement.as_str())
                .to_string();
            if rule.append_blank && !result.is_empty() {
                result.push('\n');
            }
        }
        result
    }

    pub fn apply_before_settings(&self, settings: &mut NovelSettings) {
        for override_rule in &self.before_settings {
            apply_setting_override(settings, &override_rule.key, &override_rule.value);
        }
    }

    pub fn apply_after_settings(&self, settings: &mut NovelSettings) {
        for override_rule in &self.after_settings {
            apply_setting_override(settings, &override_rule.key, &override_rule.value);
        }
    }

    pub fn signature(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.title.as_bytes());
        for rule in &self.before {
            hasher.update(rule.pattern.as_bytes());
            hasher.update(rule.replacement.as_bytes());
        }
        for rule in &self.after {
            hasher.update(rule.pattern.as_bytes());
            hasher.update(rule.replacement.as_bytes());
        }
        hex::encode(hasher.finalize())
    }
}

fn apply_setting_override(settings: &mut NovelSettings, key: &str, value: &serde_yaml::Value) {
    match key {
        "enable_convert_num_to_kanji" => {
            if let Some(b) = value.as_bool() {
                settings.enable_convert_num_to_kanji = b;
            }
        }
        "enable_ruby" => {
            if let Some(b) = value.as_bool() {
                settings.enable_ruby = b;
            }
        }
        "enable_auto_join_line" => {
            if let Some(b) = value.as_bool() {
                settings.enable_auto_join_line = b;
            }
        }
        "enable_pack_blank_line" => {
            if let Some(b) = value.as_bool() {
                settings.enable_pack_blank_line = b;
            }
        }
        "enable_yokogaki" => {
            if let Some(b) = value.as_bool() {
                settings.enable_yokogaki = b;
            }
        }
        "enable_auto_indent" => {
            if let Some(b) = value.as_bool() {
                settings.enable_auto_indent = b;
            }
        }
        "enable_auto_join_in_brackets" => {
            if let Some(b) = value.as_bool() {
                settings.enable_auto_join_in_brackets = b;
            }
        }
        "enable_convert_horizontal_ellipsis" => {
            if let Some(b) = value.as_bool() {
                settings.enable_convert_horizontal_ellipsis = b;
            }
        }
        "enable_dakuten_font" => {
            if let Some(b) = value.as_bool() {
                settings.enable_dakuten_font = b;
            }
        }
        "enable_display_end_of_book" => {
            if let Some(b) = value.as_bool() {
                settings.enable_display_end_of_book = b;
            }
        }
        "enable_erase_introduction" => {
            if let Some(b) = value.as_bool() {
                settings.enable_erase_introduction = b;
            }
        }
        "enable_erase_postscript" => {
            if let Some(b) = value.as_bool() {
                settings.enable_erase_postscript = b;
            }
        }
        "enable_enchant_midashi" => {
            if let Some(b) = value.as_bool() {
                settings.enable_enchant_midashi = b;
            }
        }
        "enable_author_comments" => {
            if let Some(b) = value.as_bool() {
                settings.enable_author_comments = b;
            }
        }
        "enable_illust" => {
            if let Some(b) = value.as_bool() {
                settings.enable_illust = b;
            }
        }
        "enable_transform_fraction" => {
            if let Some(b) = value.as_bool() {
                settings.enable_transform_fraction = b;
            }
        }
        "enable_transform_date" => {
            if let Some(b) = value.as_bool() {
                settings.enable_transform_date = b;
            }
        }
        "enable_convert_page_break" => {
            if let Some(b) = value.as_bool() {
                settings.enable_convert_page_break = b;
            }
        }
        "enable_add_date_to_title" => {
            if let Some(b) = value.as_bool() {
                settings.enable_add_date_to_title = b;
            }
        }
        "enable_ruby_youon_to_big" => {
            if let Some(b) = value.as_bool() {
                settings.enable_ruby_youon_to_big = b;
            }
        }
        "enable_kana_ni_to_kanji_ni" => {
            if let Some(b) = value.as_bool() {
                settings.enable_kana_ni_to_kanji_ni = b;
            }
        }
        "enable_insert_word_separator" => {
            if let Some(b) = value.as_bool() {
                settings.enable_insert_word_separator = b;
            }
        }
        "enable_insert_char_separator" => {
            if let Some(b) = value.as_bool() {
                settings.enable_insert_char_separator = b;
            }
        }
        "enable_strip_decoration_tag" => {
            if let Some(b) = value.as_bool() {
                settings.enable_strip_decoration_tag = b;
            }
        }
        "enable_add_end_to_title" => {
            if let Some(b) = value.as_bool() {
                settings.enable_add_end_to_title = b;
            }
        }
        "enable_prolonged_sound_mark_to_dash" => {
            if let Some(b) = value.as_bool() {
                settings.enable_prolonged_sound_mark_to_dash = b;
            }
        }
        _ => {}
    }
}

fn text_type_to_str(tt: TextType) -> &'static str {
    match tt {
        TextType::Story => "story",
        TextType::Chapter => "chapter",
        TextType::Subtitle => "subtitle",
        TextType::Introduction => "introduction",
        TextType::Body => "body",
        TextType::Postscript => "postscript",
        TextType::TextFile => "textfile",
    }
}
