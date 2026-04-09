use regex::Regex;

use super::settings::NovelSettings;
use super::user_converter::UserConverter;

pub struct ConverterBase {
    pub settings: NovelSettings,
    pub user_converter: Option<UserConverter>,
    url_stash: Vec<String>,
    english_stash: Vec<String>,
    illust_stash: Vec<String>,
    kanji_num_stash: Vec<String>,
    hankaku_num_comma_stash: Vec<String>,
    force_indent_chapter_stash: Vec<String>,
    text_type: TextType,
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
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            hankaku_num_comma_stash: Vec::new(),
            force_indent_chapter_stash: Vec::new(),
            text_type: TextType::Body,
        }
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        Self {
            settings,
            user_converter: Some(user_converter),
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            hankaku_num_comma_stash: Vec::new(),
            force_indent_chapter_stash: Vec::new(),
            text_type: TextType::Body,
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

    fn convert_page_break(&self, text: &str) -> String {
        let threshold = self.settings.to_page_break_threshold;
        if threshold < 1 {
            return text.to_string();
        }
        let pattern = format!("(^\n){{{},}}", threshold);
        let re = Regex::new(&pattern).unwrap();
        re.replace_all(text, "\u{FF3B}\u{FF03}\u{6539}\u{9801}\u{FF3D}\n")
            .to_string()
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
        self.replace_url(&mut result);
        result = self.replace_narou_tag(&result);
        result = self.convert_numbers(&mut result);
        result = self.stash_kome(&result);
        result = self.convert_double_angle_quotation_to_gaiji(&result);
        result = self.convert_novel_rule(&result);
        result = self.convert_head_half_spaces(&result);
        result = self.modify_kana_ni_to_kanji_ni(&result);

        if self.settings.enable_prolonged_sound_mark_to_dash {
            result = self.convert_prolonged_sound_mark_to_dash(&result);
        }

        result
    }

    fn rstrip_all_lines(&self, text: &str) -> String {
        text.lines()
            .map(|line| line.trim_end_matches(|c: char| c.is_whitespace()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn hankakukana_to_zenkakukana(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        for ch in text.chars() {
            if is_halfwidth_katakana(ch) {
                result.push(to_fullwidth_katakana(ch));
            } else {
                result.push(ch);
            }
        }
        result
    }

    fn auto_join_in_brackets(&self, text: &str) -> String {
        if !self.settings.enable_auto_join_in_brackets {
            return text.to_string();
        }
        let mut result = text.to_string();
        for (open, close) in &[("\u{300C}", "\u{300D}"), ("\u{300E}", "\u{300F}")] {
            let re = Regex::new(&format!(
                r"({})(.*?)({})",
                regex::escape(open),
                regex::escape(close)
            ))
            .unwrap();
            result = re
                .replace_all(&result, |caps: &regex::Captures| {
                    let open_ch = &caps[1];
                    let inner = caps[2].replace('\n', "");
                    let close_ch = &caps[3];
                    format!("{}{}{}", open_ch, inner, close_ch)
                })
                .to_string();
        }
        result
    }

    fn auto_join_line(&self, text: &str) -> String {
        let re = Regex::new(r"([^、])、\n　([^「『\(（【<＜〈《≪・■…‥―　１-９一-九])").unwrap();
        re.replace_all(text, "$!1、$!2").to_string()
    }

    fn erase_comments_block(&self, text: &str) -> String {
        let re = Regex::new(r"(?m)^-{5,}.*$").unwrap();
        re.replace_all(text, "").to_string()
    }

    fn replace_illust_tag(&mut self, text: &mut String) {
        if !self.settings.enable_illust {
            let re = Regex::new(r#"<img[^>]+src="([^"]+)"[^>]*>"#).unwrap();
            *text = re.replace_all(text, "").to_string();
            return;
        }
        let re = Regex::new(r#"<img[^>]+src="([^"]+)"[^>]*>"#).unwrap();
        *text = re
            .replace_all(text, |caps: &regex::Captures| {
                let url = caps[1].to_string();
                let idx = self.illust_stash.len();
                self.illust_stash.push(format!(
                    "\u{FF3B}\u{FF03}\u{633F}\u{7D75}\u{FF08}{}\u{FF09}\u{5165}\u{308B}\u{FF3D}",
                    url
                ));
                format!("\u{FF3B}\u{FF03}ILUST={}\u{FF3D}", idx)
            })
            .to_string();
    }

    fn replace_url(&mut self, text: &str) -> String {
        let re = Regex::new(r#"https?://[^\s<>"']+"#).unwrap();
        let result = re
            .replace_all(text, |caps: &regex::Captures| {
                let url = caps[0].to_string();
                let idx = self.url_stash.len();
                self.url_stash.push(url);
                format!("\u{FF3B}\u{FF03}URL={}\u{FF3D}", idx)
            })
            .to_string();
        result
    }

    fn replace_narou_tag(&self, text: &str) -> String {
        text.replace("\u{3010}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{3011}", "")
    }

    fn convert_numbers(&mut self, text: &str) -> String {
        match self.text_type {
            TextType::Subtitle | TextType::Chapter | TextType::Story => {
                self.hankaku_num_to_zenkaku(text)
            }
            _ => {
                if self.settings.enable_convert_num_to_kanji {
                    self.convert_numbers_to_kanji(text)
                } else {
                    self.hankaku_num_to_zenkaku(text)
                }
            }
        }
    }

    fn convert_numbers_to_kanji(&mut self, text: &str) -> String {
        let re = Regex::new(r"[\d\u{FF10}-\u{FF19},\u{FF0C}]+").unwrap();
        let result = re
            .replace_all(text, |caps: &regex::Captures| {
                let num_str = &caps[0];
                if num_str.contains(',') || num_str.contains('\u{FF0C}') {
                    let cleaned = num_str.replace('\u{FF0C}', ",");
                    let idx = self.hankaku_num_comma_stash.len();
                    self.hankaku_num_comma_stash.push(cleaned);
                    return format!(
                        "\u{FF3B}\u{FF03}\u{534A}\u{89D2}\u{6570}\u{5B57}\u{FF1D}{}\u{FF3D}",
                        idx
                    );
                }
                let hankaku: String = num_str
                    .chars()
                    .map(|c| {
                        if ('\u{FF10}'..='\u{FF19}').contains(&c) {
                            char::from_u32(c as u32 - 0xFF10 + '0' as u32).unwrap_or(c)
                        } else {
                            c
                        }
                    })
                    .collect();
                if let Ok(num) = hankaku.parse::<u64>() {
                    num_to_kanji(num)
                } else {
                    num_str.to_string()
                }
            })
            .to_string();
        result
    }

    fn hankaku_num_to_zenkaku(&self, text: &str) -> String {
        let mut result = text.to_string();
        for ch in '0'..='9' {
            let zen = to_fullwidth_digit(ch);
            result = result.replace(ch, &zen.to_string());
        }
        result
    }

    fn stash_kome(&self, text: &str) -> String {
        text.replace('\u{203B}', "\u{203B}\u{203B}")
    }

    fn convert_double_angle_quotation_to_gaiji(&self, text: &str) -> String {
        let result = text.replace(
            '\u{226A}',
            "\u{203B}\u{FF3B}\u{FF03}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        );
        result.replace(
            '\u{226B}',
            "\u{203B}\u{FF3B}\u{FF03}\u{7D42}\u{308F}\u{308A}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        )
    }

    fn convert_novel_rule(&self, text: &str) -> String {
        let mut result = text.to_string();

        result = Regex::new(r"\u{3002}\u{300D}")
            .unwrap()
            .replace_all(&result, "\u{300D}")
            .to_string();

        result = Regex::new(r"\u{3002}\u{300F}")
            .unwrap()
            .replace_all(&result, "\u{300F}")
            .to_string();

        result = Regex::new(r"\u{3002}\u{FF09}")
            .unwrap()
            .replace_all(&result, "\u{FF09}")
            .to_string();

        result = normalize_ellipsis(&result);
        result = normalize_ditto(&result);

        let re = Regex::new(r"\u{3002}\u{3000}").unwrap();
        result = re.replace_all(&result, "\u{3000}").to_string();

        result
    }

    fn convert_head_half_spaces(&self, text: &str) -> String {
        let re = Regex::new(r"(?m)^ +").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            " ".repeat(caps[0].len()).replace(' ', "\u{3000}")
        })
        .to_string()
    }

    fn modify_kana_ni_to_kanji_ni(&self, text: &str) -> String {
        if !self.settings.enable_kana_ni_to_kanji_ni {
            return text.to_string();
        }
        let mut result = text.to_string();
        let re = Regex::new(r"\u{30CB}").unwrap();
        result = re
            .replace_all(&result, |_: &regex::Captures| "\u{4E8C}".to_string())
            .to_string();
        result
    }

    fn convert_prolonged_sound_mark_to_dash(&self, text: &str) -> String {
        let re = Regex::new(r"\u{30FC}{2,}").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            "\u{2015}".repeat(caps[0].chars().count())
        })
        .to_string()
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
            if self.settings.enable_half_indent_bracket {
                self.half_indent_bracket(&mut data);
            }
            self.auto_indent(&mut data);
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

    fn rebuild_force_indent_chapter(&self, data: &mut String) {
        let re = Regex::new(r"\u{FF3B}\u{FF03}\u{7AE0}\u{898B}\u{51FA}\u{3057}\u{3063}\u{307D}\u{3044}\u{6587}\u{FF1D}(\d+)\u{FF3D}")
            .unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                self.force_indent_chapter_stash
                    .get(idx)
                    .cloned()
                    .unwrap_or_default()
            })
            .to_string();
    }

    fn rebuild_illust(&self, data: &mut String) {
        let re = Regex::new(r"\u{FF3B}\u{FF03}ILUST=(\d+)\u{FF3D}").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                self.illust_stash.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    fn rebuild_url(&self, data: &mut String) {
        let re = Regex::new(r"\u{FF3B}\u{FF03}URL=(\d+)\u{FF3D}").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                self.url_stash.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    fn rebuild_hankaku_num_comma(&self, data: &mut String) {
        let re =
            Regex::new(r"\u{FF3B}\u{FF03}\u{534A}\u{89D2}\u{6570}\u{5B57}\u{FF1D}(\d+)\u{FF3D}")
                .unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                self.hankaku_num_comma_stash
                    .get(idx)
                    .cloned()
                    .unwrap_or_default()
            })
            .to_string();
    }

    fn rebuild_kome_to_gaiji(&self, data: &mut String) {
        *data = data.replace(
            "\u{203B}\u{203B}",
            "\u{203B}\u{FF3B}\u{FF03}\u{7C73}\u{5370}\u{3001}1-2-8\u{FF3D}",
        );
    }

    fn half_indent_bracket(&self, data: &mut String) {
        let re = Regex::new(
            r"(?m)^[ 　\t]*((?:[\u{3005}\u{300C}\u{300E}\u{FF08}\u{3010}\u{3008}\u{300A}\u{300C}\u{FF08}\u{3011}]|\u{203B}\u{FF3B}\u{FF03}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}))"
        ).unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                format!(
                    "\u{FF3B}\u{FF03}\u{4E8C}\u{5206}\u{30A2}\u{30AD}\u{FF3D}{}",
                    &caps[1]
                )
            })
            .to_string();
    }

    fn auto_indent(&self, data: &mut String) {
        let re = Regex::new(r"(?m)^(\u{2014}{1,})").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                format!("\u{3000}{}", &caps[1])
            })
            .to_string();

        if self.settings.enable_force_indent || self.settings.enable_auto_indent {
            let ignore_chars = "\u{3000}\u{3001}\u{3002}\u{2026}\u{2025}\u{2015}\u{30FC}\u{300D}\u{300F}\u{FF09}\u{FF5D}\u{300B}\u{3011}\u{FF1D}\u{FF01}\u{2605}\u{2606}\u{266A}\u{FF3B}\u{2014}\u{30FB}\u{2022}";
            let re = Regex::new(&format!(r"(?m)^([^{0}])", regex::escape(ignore_chars))).unwrap();
            *data = re
                .replace_all(data, |caps: &regex::Captures| {
                    let ch = &caps[1];
                    if ch == "・" {
                        let _rest = &caps[0];
                        let after = &data[caps.get(0).unwrap().end()..];
                        if after.starts_with('・') {
                            format!("\u{3000}{}", ch)
                        } else {
                            ch.to_string()
                        }
                    } else if ch == " " || ch == "\u{3000}" {
                        "\u{3000}".to_string()
                    } else {
                        format!("\u{3000}{}", ch)
                    }
                })
                .to_string();
        }
    }

    fn narou_ruby(&self, data: &mut String) {
        let guillemet_re = Regex::new(r"\u{226A}(.+?)\u{226B}").unwrap();
        let original = data.clone();
        *data = guillemet_re
            .replace_all(&original, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                let base = self.find_ruby_base(&original, caps.get(0).unwrap().start());
                format!("\u{FF5C}{}\u{300C}{}\u{300D}", base, ruby_text)
            })
            .to_string();

        let paren_re = Regex::new(r"\u{FF08}(.+?)\u{FF09}").unwrap();
        let original = data.clone();
        *data = paren_re
            .replace_all(&original, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                if ruby_text.is_empty() || ruby_text.starts_with(' ') {
                    return caps[0].to_string();
                }
                let base = self.find_ruby_base(&original, caps.get(0).unwrap().start());
                if base.is_empty() {
                    return caps[0].to_string();
                }
                format!("\u{FF5C}{}\u{300C}{}\u{300D}", base, ruby_text)
            })
            .to_string();
    }

    fn find_ruby_base(&self, text: &str, pos: usize) -> String {
        let before = &text[..pos];
        let chars: Vec<char> = before.chars().collect();

        let mut base = String::new();
        let ruby_eligible = |c: char| -> bool {
            matches!(
                c,
                '\u{4E00}'..='\u{9FFF}'
                    | '\u{3040}'..='\u{309F}'
                    | '\u{30A0}'..='\u{30FF}'
                    | '\u{FF66}'..='\u{FF9F}'
                    | '\u{FF21}'..='\u{FF3A}'
            )
        };

        for c in chars.iter().rev() {
            if ruby_eligible(*c) {
                base.insert(0, *c);
                if base.len() >= 20 {
                    break;
                }
            } else {
                break;
            }
        }

        base
    }

    fn convert_horizontal_ellipsis(&self, text: &str) -> String {
        let re = Regex::new(r"\u{30FB}{2,}").unwrap();
        re.replace_all(text, "\u{2026}").to_string()
    }

    fn convert_double_angle_quotation_to_gaiji_post(&self, data: &mut String) {
        *data = data.replace(
            '\u{226A}',
            "\u{203B}\u{FF3B}\u{FF03}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        );
        *data = data.replace(
            '\u{226B}',
            "\u{203B}\u{FF3B}\u{FF03}\u{7D42}\u{308F}\u{308A}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        );
    }

    fn delete_dust_char(&self, data: &mut String) {
        *data = data
            .chars()
            .filter(|&c| {
                !matches!(
                    c as u32,
                    0x200B..=0x200F | 0x2028..=0x202F | 0x2060..=0x206F | 0xFEFF
                )
            })
            .collect();
    }

    fn replace_by_replace_txt(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (src, dst) in &self.settings.replace_patterns {
            result = result.replace(src, dst);
        }
        result
    }
}

fn zenkaku_rstrip(line: &str) -> String {
    line.trim_end_matches(|c: char| c == '\u{3000}' || c.is_whitespace())
        .to_string()
}

fn is_blank_line(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_border_symbol(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.chars().next().unwrap();
    matches!(
        first,
        '\u{25A0}'
            | '\u{25A1}'
            | '\u{25B2}'
            | '\u{25B3}'
            | '\u{25C6}'
            | '\u{25C7}'
            | '\u{25CF}'
            | '\u{25CB}'
            | '\u{2605}'
            | '\u{2606}'
            | '\u{266A}'
            | '\u{266B}'
            | '\u{FF0A}'
            | '\u{FF0D}'
            | '\u{FF1A}'
            | '\u{FF1B}'
            | '\u{301C}'
    )
}

fn is_halfwidth_katakana(ch: char) -> bool {
    matches!(ch as u32, 0xFF66..=0xFF9F)
}

fn to_fullwidth_katakana(ch: char) -> char {
    let offset = ch as u32 - 0xFF66;
    char::from_u32(0x30A2 + offset - 1).unwrap_or(ch)
}

fn to_fullwidth_digit(ch: char) -> char {
    char::from_u32(ch as u32 - '0' as u32 + 0xFF10).unwrap_or(ch)
}

fn normalize_ellipsis(text: &str) -> String {
    let re = Regex::new(r"\u{2026}+").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let count = caps[0].chars().count();
        let even = (count + 1) / 2 * 2;
        "\u{2026}".repeat(even)
    })
    .to_string()
}

fn normalize_ditto(text: &str) -> String {
    let re = Regex::new(r"\u{2025}+").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let count = caps[0].chars().count();
        let even = (count + 1) / 2 * 2;
        "\u{2025}".repeat(even)
    })
    .to_string()
}

const KANJI_DIGITS: &[char] = &[
    '\u{96F6}', '\u{4E00}', '\u{4E8C}', '\u{4E09}', '\u{56DB}', '\u{4E94}', '\u{516D}', '\u{4E03}',
    '\u{516B}', '\u{4E5D}',
];

fn num_to_kanji(mut num: u64) -> String {
    if num == 0 {
        return KANJI_DIGITS[0].to_string();
    }

    let units: &[(u64, &str)] = &[
        (1_0000_0000_0000_0000, "\u{4EAC}"),
        (1_0000_0000_0000, "\u{5146}"),
        (1_0000_0000, "\u{5104}"),
        (1_0000, "\u{4E07}"),
        (1, ""),
    ];

    let small_units: &[(u64, &str)] = &[(1000, "\u{5343}"), (100, "\u{767E}"), (10, "\u{5341}")];

    let mut result = String::new();

    for (unit_val, unit_name) in units {
        if num >= *unit_val {
            let digit = num / *unit_val;
            num %= *unit_val;

            if digit > 0 {
                if *unit_val >= 1_0000 {
                    result.push_str(&num_to_kanji(digit));
                } else {
                    for (small_val, small_name) in small_units {
                        if digit >= *small_val {
                            let small_digit = digit / *small_val;
                            let idx = small_digit.min(9) as usize;
                            if small_digit > 1 || *small_val == digit {
                                result.push(KANJI_DIGITS[idx]);
                            }
                            result.push_str(small_name);
                        }
                    }
                }
                result.push_str(unit_name);
            }
        }
    }

    result
}
