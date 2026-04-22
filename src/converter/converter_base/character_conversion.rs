use regex::Regex;

use super::{ConverterBase, TextType};
use crate::converter::converter_base::text_normalization::tcy;

const KANJI_DIGITS: &[char] = &[
    '\u{3007}', '\u{4E00}', '\u{4E8C}', '\u{4E09}', '\u{56DB}', '\u{4E94}', '\u{516D}', '\u{4E03}',
    '\u{516B}', '\u{4E5D}',
];

impl ConverterBase {
    pub(super) fn hankakukana_to_zenkakukana(&self, text: &str) -> String {
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

    pub(super) fn convert_numbers(&mut self, text: &str) -> String {
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

    pub(super) fn convert_numbers_to_kanji(&mut self, text: &str) -> String {
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

    pub(super) fn hankaku_num_to_zenkaku(&self, text: &str) -> String {
        let re = Regex::new(r"[0-9]+").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let num = &caps[0];
            let m = caps.get(0).unwrap();
            let is_line_start = m.start() == 0 || text[..m.start()].ends_with('\n');
            if num.len() == 2 || (num.len() == 3 && self.text_type == TextType::Subtitle && is_line_start) {
                tcy(num)
            } else {
                num.chars().map(to_fullwidth_digit).collect::<String>()
            }
        })
        .to_string()
    }

    pub(super) fn alphabet_to_zenkaku(&self, text: &str) -> String {
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

    pub(super) fn symbols_to_zenkaku(&self, text: &str) -> String {
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

    pub(super) fn convert_tatechuyoko(&self, text: &str) -> String {
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

    pub(super) fn exception_reconvert_kanji_to_num(&self, text: &str) -> String {
        if !self.settings.enable_convert_num_to_kanji {
            return text.to_string();
        }

        let kanji_digits =
            "\u{3007}\u{4E00}\u{4E8C}\u{4E09}\u{56DB}\u{4E94}\u{516D}\u{4E03}\u{516B}\u{4E5D}";
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

        let re2 = Regex::new(&format!(r"({digit_like})([Ａ-Ｚａ-ｚ％㎜㎝㎞㎎㎏㏄㎡㎥])")).unwrap();
        re2.replace_all(&result, |caps: &regex::Captures| {
            format!("{}{}", digit_to_zenkaku(&caps[1]), &caps[2])
        })
        .to_string()
    }

    pub(super) fn rebuild_hankaku_num_comma(&self, data: &mut String) {
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
}

fn is_halfwidth_katakana(ch: char) -> bool {
    matches!(ch as u32, 0xFF66..=0xFF9F)
}

fn to_fullwidth_katakana(ch: char) -> char {
    let offset = ch as u32 - 0xFF66;
    char::from_u32(0x30A2 + offset - 1).unwrap_or(ch)
}

fn to_fullwidth_digit(ch: char) -> char {
    match ch {
        '0'..='9' => char::from_u32(ch as u32 - '0' as u32 + 0xFF10).unwrap_or(ch),
        _ => ch,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ConverterBase, TextType};
    use crate::converter::settings::NovelSettings;

    fn run(text: &str, ty: TextType) -> String {
        let settings = NovelSettings::default();
        let mut cb = ConverterBase::new(settings);
        cb.text_type = ty;
        cb.hankaku_num_to_zenkaku(text)
    }

    #[test]
    fn ruby_parity_halfwidth_two_digits_in_subtitle_become_tcy() {
        // Ruby: \d (ASCII) matches "10" → tcy("10")
        let out = run("第四章10　『知識欲の権化』", TextType::Subtitle);
        assert_eq!(out, "第四章［＃縦中横］10［＃縦中横終わり］　『知識欲の権化』");
    }

    #[test]
    fn ruby_parity_fullwidth_digits_pass_through_unchanged() {
        // Ruby: \d does NOT match ０-９ (ASCII-only) → stays as-is, no tcy, no garbage codepoints
        let out = run("第四章１０　『知識欲の権化』", TextType::Subtitle);
        assert_eq!(out, "第四章１０　『知識欲の権化』");
    }

    #[test]
    fn ruby_parity_single_digit_becomes_fullwidth() {
        let out = run("第1章", TextType::Subtitle);
        assert_eq!(out, "第１章");
    }

    #[test]
    fn ruby_parity_three_digit_subtitle_at_line_start_becomes_tcy() {
        let out = run("100話", TextType::Subtitle);
        assert_eq!(out, "［＃縦中横］100［＃縦中横終わり］話");
    }

    #[test]
    fn ruby_parity_three_digit_mid_line_becomes_fullwidth() {
        let out = run("第100章", TextType::Subtitle);
        assert_eq!(out, "第１００章");
    }

    #[test]
    fn ruby_parity_n2267be_chapter_subtitle_with_fullwidth_digits() {
        // Real source from https://ncode.syosetu.com/n2267be/176/
        // Title is already fullwidth: 第四章１０　『知識欲の権化』
        // Ruby \d (ASCII-only) does not match ０-９ → digits stay untouched, no tcy.
        let settings = NovelSettings::default();
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert("第四章１０　『知識欲の権化』", TextType::Subtitle);
        assert_eq!(out, "第四章１０　『知識欲の権化』");
    }
}
