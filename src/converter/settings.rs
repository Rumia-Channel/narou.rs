use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IniValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
    Null,
}

impl Default for IniValue {
    fn default() -> Self {
        IniValue::Null
    }
}

#[derive(Debug, Clone)]
pub struct IniData {
    sections: HashMap<String, HashMap<String, IniValue>>,
}

impl IniData {
    pub fn new() -> Self {
        let mut sections = HashMap::new();
        sections.insert("global".to_string(), HashMap::new());
        Self { sections }
    }

    pub fn load(text: &str) -> Self {
        let mut data = Self::new();
        let mut current_section = "global".to_string();

        for line in text.lines() {
            let trimmed = line.trim();

            if trimmed.is_empty() || trimmed.starts_with(';') {
                continue;
            }

            if let Some(section_name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
            {
                current_section = section_name.trim().to_string();
                data.sections.entry(current_section.clone()).or_default();
                continue;
            }

            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim().to_string();
                let value = value.trim();
                let ini_value = cast_ini_value(value);
                data.sections
                    .entry(current_section.clone())
                    .or_default()
                    .insert(key, ini_value);
            }
        }

        data
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let content = fs::read_to_string(path)?;
        Ok(Self::load(&content))
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&IniValue> {
        self.sections.get(section).and_then(|s| s.get(key))
    }

    pub fn get_global(&self, key: &str) -> Option<&IniValue> {
        self.get("global", key)
    }

    pub fn set(&mut self, section: &str, key: &str, value: IniValue) {
        self.sections
            .entry(section.to_string())
            .or_default()
            .insert(key.to_string(), value);
    }

    pub fn set_global(&mut self, key: &str, value: IniValue) {
        self.set("global", key, value);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut output = String::new();

        let mut first = true;
        for (section, values) in &self.sections {
            if !first {
                output.push('\n');
            }
            first = false;

            if section != "global" {
                output.push_str(&format!("[{}]\n", section));
            }

            for (key, value) in values {
                let value_str = match value {
                    IniValue::String(s) => s.clone(),
                    IniValue::Integer(i) => i.to_string(),
                    IniValue::Float(f) => f.to_string(),
                    IniValue::Boolean(b) => {
                        if *b {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    }
                    IniValue::Null => String::new(),
                };
                output.push_str(&format!("{} = {}\n", key, value_str));
            }
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, output)?;
        Ok(())
    }

    pub fn global_section(&self) -> &HashMap<String, IniValue> {
        self.sections.get("global").unwrap()
    }
}

fn cast_ini_value(s: &str) -> IniValue {
    if s.is_empty() {
        return IniValue::Null;
    }

    if let Some(quoted) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return IniValue::String(quoted.to_string());
    }
    if let Some(quoted) = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return IniValue::String(quoted.to_string());
    }

    let lower = s.to_lowercase();
    if lower == "true" {
        return IniValue::Boolean(true);
    }
    if lower == "false" {
        return IniValue::Boolean(false);
    }
    if lower == "nil" || lower == "null" {
        return IniValue::Null;
    }

    if let Ok(i) = s.parse::<i64>() {
        return IniValue::Integer(i);
    }

    if let Ok(f) = s.parse::<f64>() {
        return IniValue::Float(f);
    }

    IniValue::String(s.to_string())
}

#[derive(Debug, Clone)]
pub struct NovelSettings {
    pub id: Option<i64>,
    pub author: Option<String>,
    pub title: Option<String>,
    pub archive_path: PathBuf,
    pub replace_patterns: Vec<(String, String)>,

    pub enable_yokogaki: bool,
    pub enable_inspect: bool,
    pub enable_convert_num_to_kanji: bool,
    pub enable_kanji_num_with_units: bool,
    pub kanji_num_with_units_lower_digit_zero: i64,
    pub enable_alphabet_force_zenkaku: bool,
    pub disable_alphabet_word_to_zenkaku: bool,
    pub enable_half_indent_bracket: bool,
    pub enable_auto_indent: bool,
    pub enable_force_indent: bool,
    pub enable_auto_join_in_brackets: bool,
    pub enable_auto_join_line: bool,
    pub enable_enchant_midashi: bool,
    pub enable_author_comments: bool,
    pub enable_erase_introduction: bool,
    pub enable_erase_postscript: bool,
    pub enable_ruby: bool,
    pub enable_illust: bool,
    pub enable_transform_fraction: bool,
    pub enable_transform_date: bool,
    pub date_format: String,
    pub enable_convert_horizontal_ellipsis: bool,
    pub enable_convert_page_break: bool,
    pub to_page_break_threshold: i64,
    pub enable_dakuten_font: bool,
    pub enable_display_end_of_book: bool,
    pub enable_add_date_to_title: bool,
    pub title_date_format: String,
    pub title_date_align: String,
    pub title_date_target: String,
    pub enable_ruby_youon_to_big: bool,
    pub enable_pack_blank_line: bool,
    pub enable_kana_ni_to_kanji_ni: bool,
    pub enable_insert_word_separator: bool,
    pub enable_insert_char_separator: bool,
    pub enable_strip_decoration_tag: bool,
    pub enable_add_end_to_title: bool,
    pub enable_prolonged_sound_mark_to_dash: bool,
    pub cut_old_subtitles: i64,
    pub slice_size: i64,
    pub author_comment_style: String,
    pub novel_author: String,
    pub novel_title: String,
    pub output_filename: String,
}

impl Default for NovelSettings {
    fn default() -> Self {
        Self {
            id: None,
            author: None,
            title: None,
            archive_path: PathBuf::new(),
            replace_patterns: Vec::new(),

            enable_yokogaki: false,
            enable_inspect: false,
            enable_convert_num_to_kanji: true,
            enable_kanji_num_with_units: true,
            kanji_num_with_units_lower_digit_zero: 3,
            enable_alphabet_force_zenkaku: false,
            disable_alphabet_word_to_zenkaku: false,
            enable_half_indent_bracket: true,
            enable_auto_indent: true,
            enable_force_indent: false,
            enable_auto_join_in_brackets: true,
            enable_auto_join_line: true,
            enable_enchant_midashi: true,
            enable_author_comments: true,
            enable_erase_introduction: false,
            enable_erase_postscript: false,
            enable_ruby: true,
            enable_illust: true,
            enable_transform_fraction: false,
            enable_transform_date: false,
            date_format: "%Y\u{5E74}%m\u{6708}%d\u{65E5}".to_string(),
            enable_convert_horizontal_ellipsis: true,
            enable_convert_page_break: false,
            to_page_break_threshold: 10,
            enable_dakuten_font: false,
            enable_display_end_of_book: true,
            enable_add_date_to_title: false,
            title_date_format: "(%-m/%-d)".to_string(),
            title_date_align: "right".to_string(),
            title_date_target: "general_lastup".to_string(),
            enable_ruby_youon_to_big: false,
            enable_pack_blank_line: true,
            enable_kana_ni_to_kanji_ni: true,
            enable_insert_word_separator: false,
            enable_insert_char_separator: false,
            enable_strip_decoration_tag: false,
            enable_add_end_to_title: false,
            enable_prolonged_sound_mark_to_dash: false,
            cut_old_subtitles: 0,
            slice_size: 0,
            author_comment_style: "css".to_string(),
            novel_author: String::new(),
            novel_title: String::new(),
            output_filename: String::new(),
        }
    }
}

impl NovelSettings {
    pub fn load_from_ini(ini: &IniData) -> Self {
        let g = |key: &str| -> Option<bool> {
            ini.get_global(key).and_then(|v| match v {
                IniValue::Boolean(b) => Some(*b),
                IniValue::String(s) if s == "on" => Some(true),
                IniValue::String(s) if s == "off" => Some(false),
                _ => None,
            })
        };
        let gi = |key: &str| -> Option<i64> {
            ini.get_global(key).and_then(|v| match v {
                IniValue::Integer(i) => Some(*i),
                _ => None,
            })
        };
        let gs = |key: &str, default: &str| -> String {
            ini.get_global(key)
                .and_then(|v| match v {
                    IniValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| default.to_string())
        };

        let mut settings = Self::default();

        if let Some(v) = g("enable_yokogaki") {
            settings.enable_yokogaki = v;
        }
        if let Some(v) = g("enable_inspect") {
            settings.enable_inspect = v;
        }
        if let Some(v) = g("enable_convert_num_to_kanji") {
            settings.enable_convert_num_to_kanji = v;
        }
        if let Some(v) = g("enable_kanji_num_with_units") {
            settings.enable_kanji_num_with_units = v;
        }
        if let Some(v) = gi("kanji_num_with_units_lower_digit_zero") {
            settings.kanji_num_with_units_lower_digit_zero = v;
        }
        if let Some(v) = g("enable_alphabet_force_zenkaku") {
            settings.enable_alphabet_force_zenkaku = v;
        }
        if let Some(v) = g("disable_alphabet_word_to_zenkaku") {
            settings.disable_alphabet_word_to_zenkaku = v;
        }
        if let Some(v) = g("enable_half_indent_bracket") {
            settings.enable_half_indent_bracket = v;
        }
        if let Some(v) = g("enable_auto_indent") {
            settings.enable_auto_indent = v;
        }
        if let Some(v) = g("enable_force_indent") {
            settings.enable_force_indent = v;
        }
        if let Some(v) = g("enable_auto_join_in_brackets") {
            settings.enable_auto_join_in_brackets = v;
        }
        if let Some(v) = g("enable_auto_join_line") {
            settings.enable_auto_join_line = v;
        }
        if let Some(v) = g("enable_enchant_midashi") {
            settings.enable_enchant_midashi = v;
        }
        if let Some(v) = g("enable_author_comments") {
            settings.enable_author_comments = v;
        }
        if let Some(v) = g("enable_erase_introduction") {
            settings.enable_erase_introduction = v;
        }
        if let Some(v) = g("enable_erase_postscript") {
            settings.enable_erase_postscript = v;
        }
        if let Some(v) = g("enable_ruby") {
            settings.enable_ruby = v;
        }
        if let Some(v) = g("enable_illust") {
            settings.enable_illust = v;
        }
        if let Some(v) = g("enable_transform_fraction") {
            settings.enable_transform_fraction = v;
        }
        if let Some(v) = g("enable_transform_date") {
            settings.enable_transform_date = v;
        }
        if let Some(v) = g("enable_convert_horizontal_ellipsis") {
            settings.enable_convert_horizontal_ellipsis = v;
        }
        if let Some(v) = g("enable_convert_page_break") {
            settings.enable_convert_page_break = v;
        }
        if let Some(v) = gi("to_page_break_threshold") {
            settings.to_page_break_threshold = v;
        }
        if let Some(v) = g("enable_dakuten_font") {
            settings.enable_dakuten_font = v;
        }
        if let Some(v) = g("enable_display_end_of_book") {
            settings.enable_display_end_of_book = v;
        }
        if let Some(v) = g("enable_add_date_to_title") {
            settings.enable_add_date_to_title = v;
        }
        if let Some(v) = g("enable_ruby_youon_to_big") {
            settings.enable_ruby_youon_to_big = v;
        }
        if let Some(v) = g("enable_pack_blank_line") {
            settings.enable_pack_blank_line = v;
        }
        if let Some(v) = g("enable_kana_ni_to_kanji_ni") {
            settings.enable_kana_ni_to_kanji_ni = v;
        }
        if let Some(v) = g("enable_insert_word_separator") {
            settings.enable_insert_word_separator = v;
        }
        if let Some(v) = g("enable_insert_char_separator") {
            settings.enable_insert_char_separator = v;
        }
        if let Some(v) = g("enable_strip_decoration_tag") {
            settings.enable_strip_decoration_tag = v;
        }
        if let Some(v) = g("enable_add_end_to_title") {
            settings.enable_add_end_to_title = v;
        }
        if let Some(v) = g("enable_prolonged_sound_mark_to_dash") {
            settings.enable_prolonged_sound_mark_to_dash = v;
        }
        if let Some(v) = gi("cut_old_subtitles") {
            settings.cut_old_subtitles = v;
        }
        if let Some(v) = gi("slice_size") {
            settings.slice_size = v;
        }

        settings.date_format = gs("date_format", "%Y\u{5E74}%m\u{6708}%d\u{65E5}");
        settings.title_date_format = gs("title_date_format", "(%-m/%-d)");
        settings.title_date_align = gs("title_date_align", "right");
        settings.title_date_target = gs("title_date_target", "general_lastup");
        settings.author_comment_style = gs("author_comment_style", "css");
        settings.novel_author = gs("novel_author", "");
        settings.novel_title = gs("novel_title", "");
        settings.output_filename = gs("output_filename", "");

        settings
    }
}

pub fn load_replace_patterns(path: &Path) -> Vec<(String, String)> {
    if !path.exists() {
        return Vec::new();
    }
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut patterns = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        if let Some((pattern, replacement)) = trimmed.split_once('\t') {
            patterns.push((pattern.to_string(), replacement.to_string()));
        }
    }
    patterns
}
