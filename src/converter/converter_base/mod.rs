mod character_conversion;
mod indentation;
mod ruby;
mod stash_rebuild;
mod text_normalization;

use std::cell::RefCell;
use std::rc::Rc;

use regex::Regex;

use super::inspector::Inspector;
use super::settings::NovelSettings;
use super::user_converter::UserConverter;

pub struct ConverterBase {
    pub settings: NovelSettings,
    pub user_converter: Option<UserConverter>,
    pub inspector: Option<Rc<RefCell<Inspector>>>,
    pub url_stash: Vec<String>,
    pub english_stash: Vec<String>,
    pub illust_stash: Vec<String>,
    pub kanji_num_stash: Vec<String>,
    pub hankaku_num_comma_stash: Vec<String>,
    pub force_indent_chapter_stash: Vec<String>,
    pub text_type: TextType,
    pub use_dakuten_font: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextType {
    Story,
    Chapter,
    Subtitle,
    Introduction,
    Body,
    Postscript,
    TextFile,
}

impl ConverterBase {
    pub fn new(settings: NovelSettings) -> Self {
        Self {
            settings,
            user_converter: None,
            inspector: None,
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            hankaku_num_comma_stash: Vec::new(),
            force_indent_chapter_stash: Vec::new(),
            text_type: TextType::Body,
            use_dakuten_font: false,
        }
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        Self {
            settings,
            user_converter: Some(user_converter),
            inspector: None,
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            hankaku_num_comma_stash: Vec::new(),
            force_indent_chapter_stash: Vec::new(),
            text_type: TextType::Body,
            use_dakuten_font: false,
        }
    }

    pub fn with_inspector(settings: NovelSettings, inspector: Rc<RefCell<Inspector>>) -> Self {
        Self {
            settings,
            user_converter: None,
            inspector: Some(inspector),
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            hankaku_num_comma_stash: Vec::new(),
            force_indent_chapter_stash: Vec::new(),
            text_type: TextType::Body,
            use_dakuten_font: false,
        }
    }

    pub fn with_user_converter_and_inspector(
        settings: NovelSettings,
        user_converter: UserConverter,
        inspector: Rc<RefCell<Inspector>>,
    ) -> Self {
        Self {
            settings,
            user_converter: Some(user_converter),
            inspector: Some(inspector),
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            hankaku_num_comma_stash: Vec::new(),
            force_indent_chapter_stash: Vec::new(),
            text_type: TextType::Body,
            use_dakuten_font: false,
        }
    }

    fn reset_stashes(&mut self) {
        self.url_stash.clear();
        self.english_stash.clear();
        self.illust_stash.clear();
        self.kanji_num_stash.clear();
        self.hankaku_num_comma_stash.clear();
        self.force_indent_chapter_stash.clear();
    }

    pub fn convert(&mut self, text: &str, text_type: TextType) -> String {
        if text.is_empty() {
            return String::new();
        }

        self.reset_stashes();
        self.text_type = text_type;

        let mut result = text.to_string();

        result = self.rstrip_all_lines(&result);

        if let Some(ref uc) = self.user_converter {
            uc.apply_before_settings(&mut self.settings);
            result = uc.apply_before(&result, text_type, &mut self.settings);
        }

        result = self.before_hook(&result);
        result = self.convert_for_all_data(&result);
        result = self.convert_main_loop(&result, text_type);

        if let Some(ref uc) = self.user_converter {
            result = uc.apply_after(&result, text_type, &mut self.settings);
            uc.apply_after_settings(&mut self.settings);
        }

        result = self.replace_by_replace_txt(&result);

        result
    }

    pub fn convert_multi(&mut self, inputs: &[(String, TextType)]) -> Vec<String> {
        inputs
            .iter()
            .map(|(text, tt)| self.convert(text, *tt))
            .collect()
    }

    fn before_hook(&self, text: &str) -> String {
        let mut result = text.to_string();

        match self.text_type {
            TextType::Body | TextType::TextFile => {
                if self.settings.enable_convert_page_break {
                    result = self.convert_page_break(&result);
                }
            }
            _ => {}
        }

        if self.text_type != TextType::Story && self.settings.enable_pack_blank_line {
            result = result.replace("\n\n", "\n");
            let re = Regex::new(r"(^\n){3}").unwrap();
            result = re.replace_all(&result, "\n\n").to_string();
        }

        result
    }

    fn convert_for_all_data(&mut self, text: &str) -> String {
        let mut result = text.to_string();

        result = self.hankakukana_to_zenkakukana(&result);
        result = self.auto_join_in_brackets(&result);
        if self.settings.enable_auto_join_line {
            result = self.auto_join_line(&result);
        }
        result = self.erase_comments_block(&result);
        self.replace_illust_tag(&mut result);
        result = self.replace_url(&result);
        result = self.replace_narou_tag(&result);
        result = self.convert_numbers(&mut result);
        result = self.exception_reconvert_kanji_to_num(&result);
        result = self.insert_separate_space(&result);
        result = self.stash_kome(&result);
        result = self.convert_double_angle_quotation_to_gaiji(&result);
        result = self.alphabet_to_zenkaku(&result);
        result = self.symbols_to_zenkaku(&result);
        result = self.convert_tatechuyoko(&result);
        result = self.convert_novel_rule(&result);
        result = self.convert_head_half_spaces(&result);
        result = self.modify_kana_ni_to_kanji_ni(&result);

        result = self.convert_dakuten_char_to_font(&result);

        if self.settings.enable_prolonged_sound_mark_to_dash {
            result = self.convert_prolonged_sound_mark_to_dash(&result);
        }

        result
    }

    fn convert_main_loop(&self, text: &str, text_type: TextType) -> String {
        if text_type == TextType::Subtitle {
            return text.to_string();
        }

        let lines: Vec<&str> = text.lines().collect();
        let mut result = Vec::new();
        let mut before_line = String::new();
        let mut request_insert_blank = false;

        for line in &lines {
            let mut line = line.to_string();
            line = zenkaku_rstrip(&line);

            if request_insert_blank {
                if !is_blank_line(&line) {
                    result.push(String::new());
                }
                request_insert_blank = false;
                before_line.clear();
            }

            if matches!(text_type, TextType::Body | TextType::TextFile) {
                let mut prefix = String::new();
                if line.contains("\u{FF3B}\u{FF03}\u{7AE0}\u{898B}\u{51FA}\u{3057}\u{3063}\u{307D}\u{3044}\u{6587}\u{FF1D}") {
                    if !is_blank_line(&before_line) {
                        prefix.push('\n');
                    }
                    request_insert_blank = true;
                }
                if is_border_symbol(&line) {
                    if !is_blank_line(&before_line) {
                        prefix.push('\n');
                    }
                    request_insert_blank = true;
                    line = format!("\u{3000}\u{3000}\u{3000}\u{3000}{}", line.trim_start());
                }
                if !prefix.is_empty() {
                    line = format!("{}{}", prefix, line);
                }
                if !line.is_empty()
                    && !line.starts_with(' ')
                    && !line.starts_with('\u{3000}')
                    && !line.starts_with('\t')
                    && !is_border_symbol(&line)
                {
                    let marker_pos = line.bytes().take_while(|&byte| byte == b'\n').count();
                    line.insert(marker_pos, '\u{E000}');
                }
            }

            result.push(line.clone());
            before_line = line;
        }

        let mut data = result.join("\n");

        if matches!(text_type, TextType::Body | TextType::TextFile) {
            self.rebuild_force_indent_chapter(&mut data);
        }

        self.rebuild_illust(&mut data);
        self.rebuild_url(&mut data);
        self.rebuild_hankaku_num_comma(&mut data);
        self.rebuild_kome_to_gaiji(&mut data);

        if matches!(text_type, TextType::Body | TextType::TextFile) {
            self.half_indent_bracket(&mut data);
            self.auto_indent(&mut data);
            data = data.replace('\u{E000}', "");
        }

        if self.settings.enable_ruby {
            self.narou_ruby(&mut data);
        }

        if self.settings.enable_convert_horizontal_ellipsis {
            data = self.convert_horizontal_ellipsis(&data);
        }

        self.convert_double_angle_quotation_to_gaiji_post(&mut data);
        self.delete_dust_char(&mut data);

        data
    }
}

pub(crate) use text_normalization::{is_blank_line, is_border_symbol, zenkaku_rstrip};
