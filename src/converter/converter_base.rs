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
        re.replace_all(text, "$1、$2").to_string()
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
                num_str
                    .chars()
                    .map(|c| match c {
                        '0'..='9' => KANJI_DIGITS[(c as u32 - '0' as u32) as usize],
                        '\u{FF10}'..='\u{FF19}' => {
                            KANJI_DIGITS[(c as u32 - '\u{FF10}' as u32) as usize]
                        }
                        _ => c,
                    })
                    .collect::<String>()
            })
            .to_string();
        result
    }

    fn hankaku_num_to_zenkaku(&self, text: &str) -> String {
        let re = Regex::new(r"\d+").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let num = &caps[0];
            if num.len() == 2 || (num.len() == 3 && self.text_type == TextType::Subtitle) {
                tcy(num)
            } else {
                num.chars().map(to_fullwidth_digit).collect::<String>()
            }
        })
        .to_string()
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

    fn symbols_to_zenkaku(&self, text: &str) -> String {
        let single_minute_family = "\u{2018}\u{2019}'";
        let double_minute_family = "\u{201C}\u{201D}\u{301D}\u{301F}\"";

        let single_re = Regex::new(&format!(
            r#"[{0}]([^"\n]+?)[{0}]"#,
            regex::escape(single_minute_family)
        ))
        .unwrap();
        let mut result = single_re
            .replace_all(text, "\u{301D}$1\u{301F}")
            .to_string();

        let double_re = Regex::new(&format!(
            r#"[{0}]([^"\n]+?)[{0}]"#,
            regex::escape(double_minute_family)
        ))
        .unwrap();
        result = double_re
            .replace_all(&result, "\u{301D}$1\u{301F}")
            .to_string();

        result
            .chars()
            .map(|c| match c {
                '-' | '‐' => '\u{FF0D}',
                '=' => '\u{FF1D}',
                '+' => '\u{FF0B}',
                '/' => '\u{FF0F}',
                '*' => '\u{FF0A}',
                '\'' => '\u{2019}',
                '"' => '\u{301D}',
                '%' => '\u{FF05}',
                '$' => '\u{FF04}',
                '#' => '\u{FF03}',
                '&' => '\u{FF06}',
                '!' => '\u{FF01}',
                '?' => '\u{FF1F}',
                '<' | '＜' => '\u{3008}',
                '>' | '＞' => '\u{3009}',
                '(' => '\u{FF08}',
                ')' => '\u{FF09}',
                '|' => '\u{FF5C}',
                ',' => '\u{FF0C}',
                '.' => '\u{FF0E}',
                '_' => '\u{FF3F}',
                ';' => '\u{FF1B}',
                ':' => '\u{FF1A}',
                '[' => '\u{FF3B}',
                ']' => '\u{FF3D}',
                '{' => '\u{FF5B}',
                '}' => '\u{FF5D}',
                '\\' => '\u{FFE5}',
                _ => c,
            })
            .collect()
    }

    fn alphabet_to_zenkaku(&self, text: &str) -> String {
        let re = Regex::new(r"[A-Za-z]+").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            caps[0]
                .chars()
                .map(|c| match c {
                    'a'..='z' => char::from_u32(c as u32 - 'a' as u32 + 'ａ' as u32).unwrap_or(c),
                    'A'..='Z' => char::from_u32(c as u32 - 'A' as u32 + 'Ａ' as u32).unwrap_or(c),
                    _ => c,
                })
                .collect::<String>()
        })
        .to_string()
    }

    fn convert_tatechuyoko(&self, text: &str) -> String {
        let re_exclam = Regex::new(r"！+").unwrap();
        let mut result = re_exclam
            .replace_all(text, |caps: &regex::Captures| {
                let matched = &caps[0];
                let start = caps.get(0).unwrap().start();
                let end = caps.get(0).unwrap().end();
                let prev = text[..start].chars().last();
                let next = text[end..].chars().next();
                if prev == Some('？') || next == Some('？') {
                    return matched.to_string();
                }
                let mut len = matched.chars().count();
                if len == 3 {
                    tcy("!!!")
                } else if len >= 4 {
                    if len % 2 == 1 {
                        len += 1;
                    }
                    tcy("!!").repeat(len / 2)
                } else {
                    matched.to_string()
                }
            })
            .to_string();

        let re_mix = Regex::new(r"[！？]+").unwrap();
        result = re_mix
            .replace_all(&result, |caps: &regex::Captures| {
                let matched = &caps[0];
                match matched.chars().count() {
                    2 => tcy(&matched.replace('！', "!").replace('？', "?")),
                    3 if matched == "！！？" || matched == "？！！" => {
                        tcy(&matched.replace('！', "!").replace('？', "?"))
                    }
                    _ => matched.to_string(),
                }
            })
            .to_string();

        result
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
        result = re.replace_all(&result, "\u{3002}").to_string();

        result
    }

    fn convert_head_half_spaces(&self, text: &str) -> String {
        let re = Regex::new(r"(?m)^ +").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            " ".repeat(caps[0].len()).replace(' ', "\u{3000}")
        })
        .to_string()
    }

    fn exception_reconvert_kanji_to_num(&self, text: &str) -> String {
        if !self.settings.enable_convert_num_to_kanji {
            return text.to_string();
        }

        let kanji_digits = "\u{3007}\u{4E00}\u{4E8C}\u{4E09}\u{56DB}\u{4E94}\u{516D}\u{4E03}\u{516B}\u{4E5D}";
        let digit_like = format!("[{kanji_digits}・～]+");
        let digit_to_zenkaku = |s: &str| {
            s.chars()
                .map(|c| match c {
                    '\u{3007}' => '\u{FF10}',
                    '\u{4E00}' => '\u{FF11}',
                    '\u{4E8C}' => '\u{FF12}',
                    '\u{4E09}' => '\u{FF13}',
                    '\u{56DB}' => '\u{FF14}',
                    '\u{4E94}' => '\u{FF15}',
                    '\u{516D}' => '\u{FF16}',
                    '\u{4E03}' => '\u{FF17}',
                    '\u{516B}' => '\u{FF18}',
                    '\u{4E5D}' => '\u{FF19}',
                    _ => c,
                })
                .collect::<String>()
        };

        let re1 = Regex::new(&format!(r"([Ａ-Ｚａ-ｚ])({digit_like})")).unwrap();
        let result = re1
            .replace_all(text, |caps: &regex::Captures| {
                format!("{}{}", &caps[1], digit_to_zenkaku(&caps[2]))
            })
            .to_string();

        let re2 = Regex::new(&format!(
            r"({digit_like})([Ａ-Ｚａ-ｚ％㎜㎝㎞㎎㎏㏄㎡㎥])"
        ))
        .unwrap();
        re2.replace_all(&result, |caps: &regex::Captures| {
            format!("{}{}", digit_to_zenkaku(&caps[1]), &caps[2])
        })
        .to_string()
    }

    fn insert_separate_space(&self, text: &str) -> String {
        let re = Regex::new(r"([!?！？]+)([^!?！？])").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let marks = &caps[1];
            let mut next = caps[2].to_string();
            if matches!(next.as_str(), " " | "\u{3000}" | "\u{3001}" | "\u{3002}") {
                next = "\u{3000}".to_string();
            }
            let ch = next.chars().next().unwrap_or('\0');
            if !matches!(
                ch,
                '\u{300D}'
                    | '\u{FF3D}'
                    | '\u{FF5D}'
                    | ']'
                    | '}'
                    | '\u{300F}'
                    | '\u{3011}'
                    | '\u{3009}'
                    | '\u{300B}'
                    | '\u{3015}'
                    | '\u{FF1E}'
                    | '>'
                    | '\u{226B}'
                    | ')'
                    | '\u{FF09}'
                    | '"'
                    | '\u{201D}'
                    | '\u{2019}'
                    | '\u{301F}'
                    | '\u{3000}'
                    | '\u{2606}'
                    | '\u{2605}'
                    | '\u{266A}'
                    | '\u{FF3B}'
                    | '\u{2015}'
            ) {
                format!("{marks}\u{3000}{next}")
            } else {
                format!("{marks}{next}")
            }
        })
        .to_string()
    }

    fn modify_kana_ni_to_kanji_ni(&self, text: &str) -> String {
        if !self.settings.enable_kana_ni_to_kanji_ni {
            return text.to_string();
        }
        let kana = "\u{30A1}-\u{30F6}\u{30FC}";
        let re = Regex::new(&format!(r"([^{0}]{{2}})\u{{30CB}}([^{0}]{{2}})", kana)).unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            format!("{}\u{4E8C}{}", &caps[1], &caps[2])
        })
        .to_string()
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
                if !line.is_empty()
                    && !line.starts_with(' ')
                    && !line.starts_with('\u{3000}')
                    && !line.starts_with('\t')
                    && !is_border_symbol(&line)
                {
                    line.insert(0, '\u{E000}');
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
                if self.settings.enable_half_indent_bracket {
                    format!(
                        "\u{FF3B}\u{FF03}\u{4E8C}\u{5206}\u{30A2}\u{30AD}\u{FF3D}{}",
                        &caps[1]
                    )
                } else {
                    caps[1].to_string()
                }
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
            let ignore_chars = "(\u{FF08}\u{300C}\u{300E}\u{3008}\u{300A}\u{226A}\u{3010}\u{3014}\u{2015}\u{30FB}\u{203B}\u{FF3B}\u{301D}\u{E000}\n";
            // Ignore blank lines so a leading '\n' is not rewritten into "　\n".
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
        let explicit_ruby_re = Regex::new(r"\u{FF5C}([^《\n]+?)《([^》\n]*?)》").unwrap();
        let original = data.clone();
        *data = explicit_ruby_re
            .replace_all(&original, |caps: &regex::Captures| {
                let ruby_text = &caps[2];
                if ruby_text.starts_with(' ') || ruby_text.ends_with("  ") {
                    format!(
                        "\u{203B}\u{FF3B}\u{FF03}\u{7E26}\u{7DDA}\u{FF3D}{}\u{203B}\u{FF3B}\u{FF03}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}{}\u{203B}\u{FF3B}\u{FF03}\u{7D42}\u{308F}\u{308A}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
                        &caps[1], ruby_text
                    )
                } else {
                    caps[0].to_string()
                }
            })
            .to_string();

        let sesame_re = Regex::new(r"\u{FF5C}([^《\n]+?)《([・、]+)》").unwrap();
        let original = data.clone();
        *data = sesame_re
            .replace_all(&original, |caps: &regex::Captures| {
                format!(
                    "\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{FF3D}{}\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
                    &caps[1]
                )
            })
            .to_string();

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
        let ruby_re = Regex::new(r"^[ぁ-んァ-ヶーゝゞ・]+[ 　]?[ぁ-んァ-ヶーゝゞ・]*$").unwrap();
        let original = data.clone();
        *data = paren_re
            .replace_all(&original, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                if ruby_text.is_empty()
                    || ruby_text.starts_with(' ')
                    || !ruby_re.is_match(ruby_text)
                {
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
        let mut result = text.to_string();
        for target in ['\u{30FB}', '\u{3002}', '\u{3001}', '\u{FF0E}'] {
            let re = Regex::new(&format!("{}{{3,}}", regex::escape(&target.to_string()))).unwrap();
            result = re
                .replace_all(&result, |caps: &regex::Captures| {
                    let len = caps[0].chars().count();
                    let start = caps.get(0).unwrap().start();
                    let end = caps.get(0).unwrap().end();
                    let prev = result[..start].chars().last();
                    let next = result[end..].chars().next();
                    if prev == Some('\u{2015}') || next == Some('\u{2015}') {
                        caps[0].to_string()
                    } else {
                        "\u{2026}".repeat(((len as f32 / 3.0 / 2.0).ceil() as usize) * 2)
                    }
                })
                .to_string();
        }
        result
            .replace("\u{3002}\u{3002}", "\u{3002}")
            .replace("\u{3001}\u{3001}", "\u{3001}")
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

fn tcy(text: &str) -> String {
    format!(
        "\u{FF3B}\u{FF03}\u{7E26}\u{4E2D}\u{6A2A}\u{FF3D}{}\u{FF3B}\u{FF03}\u{7E26}\u{4E2D}\u{6A2A}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
        text
    )
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
    '\u{3007}', '\u{4E00}', '\u{4E8C}', '\u{4E09}', '\u{56DB}', '\u{4E94}', '\u{516D}', '\u{4E03}',
    '\u{516B}', '\u{4E5D}',
];
