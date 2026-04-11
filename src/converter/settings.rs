use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::ini::{IniData, IniValue};
use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
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
            enable_half_indent_bracket: false,
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
    pub fn load_for_novel(
        novel_id: i64,
        novel_title: &str,
        novel_author: &str,
        archive_path: &Path,
    ) -> Self {
        let _original = Self::default();

        let ini_path = archive_path.join("setting.ini");
        let replace_path = archive_path.join("replace.txt");
        let local_setting_path = archive_path.join(".narou").join("local_setting.yaml");

        let ini = match IniData::load_file(&ini_path) {
            Ok(i) => i,
            Err(_) => IniData::new(),
        };

        let mut settings = Self::default();
        settings.id = Some(novel_id);
        settings.title = Some(novel_title.to_string());
        settings.author = Some(novel_author.to_string());
        settings.archive_path = archive_path.to_path_buf();
        settings.replace_patterns = load_replace_patterns(&replace_path);

        settings = Self::apply_ini_defaults(&settings, &ini);
        settings = Self::apply_ini_novel(&settings, &ini, novel_id);
        settings = Self::apply_force_settings(&settings, &local_setting_path);

        settings.novel_title = if settings.novel_title.is_empty() {
            novel_title.to_string()
        } else {
            settings.novel_title.clone()
        };
        settings.novel_author = if settings.novel_author.is_empty() {
            novel_author.to_string()
        } else {
            settings.novel_author.clone()
        };

        settings
    }

    fn apply_ini_defaults(settings: &Self, ini: &IniData) -> Self {
        let mut s = Self::load_from_ini(ini);
        s.id = settings.id;
        s.title = settings.title.clone();
        s.author = settings.author.clone();
        s.archive_path = settings.archive_path.clone();
        s.replace_patterns = settings.replace_patterns.clone();
        s
    }

    fn apply_ini_novel(settings: &Self, ini: &IniData, novel_id: i64) -> Self {
        let mut s = settings.clone();
        let section_key = novel_id.to_string();
        if let Some(section) = ini.sections.get(&section_key) {
            for (key, value) in section {
                Self::apply_single_setting(&mut s, key, value);
            }
        }
        s
    }

    fn apply_force_settings(settings: &Self, local_setting_path: &Path) -> Self {
        let mut s = settings.clone();
        if !local_setting_path.exists() {
            return s;
        }
        if let Ok(content) = fs::read_to_string(local_setting_path) {
            if let Ok(data) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                if let Some(force_map) = data.get("force").and_then(|v| v.as_mapping()) {
                    for (key, value) in force_map {
                        if let Some(key_str) = key.as_str() {
                            let ini_val = yaml_value_to_ini(value);
                            Self::apply_single_setting(&mut s, key_str, &ini_val);
                        }
                    }
                }
            }
        }
        s
    }

    fn apply_single_setting(settings: &mut Self, key: &str, value: &IniValue) {
        match key {
            "enable_yokogaki" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_yokogaki = b;
                }
            }
            "enable_inspect" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_inspect = b;
                }
            }
            "enable_convert_num_to_kanji" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_convert_num_to_kanji = b;
                }
            }
            "enable_kanji_num_with_units" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_kanji_num_with_units = b;
                }
            }
            "kanji_num_with_units_lower_digit_zero" => {
                if let Some(i) = to_i64(value) {
                    settings.kanji_num_with_units_lower_digit_zero = i;
                }
            }
            "enable_alphabet_force_zenkaku" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_alphabet_force_zenkaku = b;
                }
            }
            "disable_alphabet_word_to_zenkaku" => {
                if let Some(b) = to_bool(value) {
                    settings.disable_alphabet_word_to_zenkaku = b;
                }
            }
            "enable_half_indent_bracket" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_half_indent_bracket = b;
                }
            }
            "enable_auto_indent" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_auto_indent = b;
                }
            }
            "enable_force_indent" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_force_indent = b;
                }
            }
            "enable_auto_join_in_brackets" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_auto_join_in_brackets = b;
                }
            }
            "enable_auto_join_line" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_auto_join_line = b;
                }
            }
            "enable_enchant_midashi" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_enchant_midashi = b;
                }
            }
            "enable_author_comments" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_author_comments = b;
                }
            }
            "enable_erase_introduction" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_erase_introduction = b;
                }
            }
            "enable_erase_postscript" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_erase_postscript = b;
                }
            }
            "enable_ruby" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_ruby = b;
                }
            }
            "enable_illust" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_illust = b;
                }
            }
            "enable_transform_fraction" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_transform_fraction = b;
                }
            }
            "enable_transform_date" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_transform_date = b;
                }
            }
            "enable_convert_horizontal_ellipsis" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_convert_horizontal_ellipsis = b;
                }
            }
            "enable_convert_page_break" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_convert_page_break = b;
                }
            }
            "to_page_break_threshold" => {
                if let Some(i) = to_i64(value) {
                    settings.to_page_break_threshold = i;
                }
            }
            "enable_dakuten_font" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_dakuten_font = b;
                }
            }
            "enable_display_end_of_book" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_display_end_of_book = b;
                }
            }
            "enable_add_date_to_title" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_add_date_to_title = b;
                }
            }
            "enable_ruby_youon_to_big" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_ruby_youon_to_big = b;
                }
            }
            "enable_pack_blank_line" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_pack_blank_line = b;
                }
            }
            "enable_kana_ni_to_kanji_ni" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_kana_ni_to_kanji_ni = b;
                }
            }
            "enable_insert_word_separator" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_insert_word_separator = b;
                }
            }
            "enable_insert_char_separator" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_insert_char_separator = b;
                }
            }
            "enable_strip_decoration_tag" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_strip_decoration_tag = b;
                }
            }
            "enable_add_end_to_title" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_add_end_to_title = b;
                }
            }
            "enable_prolonged_sound_mark_to_dash" => {
                if let Some(b) = to_bool(value) {
                    settings.enable_prolonged_sound_mark_to_dash = b;
                }
            }
            "cut_old_subtitles" => {
                if let Some(i) = to_i64(value) {
                    settings.cut_old_subtitles = i;
                }
            }
            "slice_size" => {
                if let Some(i) = to_i64(value) {
                    settings.slice_size = i;
                }
            }
            "date_format" => {
                if let Some(s) = to_string_val(value) {
                    settings.date_format = s;
                }
            }
            "title_date_format" => {
                if let Some(s) = to_string_val(value) {
                    settings.title_date_format = s;
                }
            }
            "title_date_align" => {
                if let Some(s) = to_string_val(value) {
                    settings.title_date_align = s;
                }
            }
            "title_date_target" => {
                if let Some(s) = to_string_val(value) {
                    settings.title_date_target = s;
                }
            }
            "author_comment_style" => {
                if let Some(s) = to_string_val(value) {
                    settings.author_comment_style = s;
                }
            }
            "novel_author" => {
                if let Some(s) = to_string_val(value) {
                    settings.novel_author = s;
                }
            }
            "novel_title" => {
                if let Some(s) = to_string_val(value) {
                    settings.novel_title = s;
                }
            }
            "output_filename" => {
                if let Some(s) = to_string_val(value) {
                    settings.output_filename = s;
                }
            }
            _ => {}
        }
    }

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

fn to_bool(value: &IniValue) -> Option<bool> {
    match value {
        IniValue::Boolean(b) => Some(*b),
        IniValue::String(s) if s == "on" => Some(true),
        IniValue::String(s) if s == "off" => Some(false),
        IniValue::Integer(i) => Some(*i != 0),
        _ => None,
    }
}

fn to_i64(value: &IniValue) -> Option<i64> {
    match value {
        IniValue::Integer(i) => Some(*i),
        IniValue::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn to_string_val(value: &IniValue) -> Option<String> {
    match value {
        IniValue::String(s) => Some(s.clone()),
        IniValue::Integer(i) => Some(i.to_string()),
        IniValue::Float(f) => Some(f.to_string()),
        IniValue::Boolean(b) => Some(b.to_string()),
        IniValue::Null => None,
    }
}

fn yaml_value_to_ini(value: &serde_yaml::Value) -> IniValue {
    match value {
        serde_yaml::Value::Bool(b) => IniValue::Boolean(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                IniValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                IniValue::Float(f)
            } else {
                IniValue::Null
            }
        }
        serde_yaml::Value::String(s) => IniValue::String(s.clone()),
        serde_yaml::Value::Null => IniValue::Null,
        _ => IniValue::Null,
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
