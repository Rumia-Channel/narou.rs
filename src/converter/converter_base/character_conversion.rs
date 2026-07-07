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
        let text = self.stash_kanji_num(text);
        let re = Regex::new(r"[\d\u{FF10}-\u{FF19},\u{FF0C}]+").unwrap();
        let result = re
            .replace_all(&text, |caps: &regex::Captures| {
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

    /// 濁点合成 (`か゛` 等) を `［＃濁点］か［＃濁点終わり］` に変換し、
    /// 1件でも変換が起きたら `use_dakuten_font = true` を立てる。Ruby の
    /// `convert_dakuten_char_to_font` (lib/converterbase.rb) と等価。
    pub(super) fn convert_dakuten_char_to_font(&mut self, text: &str) -> String {
        if !self.settings.enable_dakuten_font {
            return text.to_string();
        }
        // [ぁ-んァ-ヶι] と続く濁点記号 (U+309B 全角濁点 / U+FF9E 半角濁点)。
        let re = Regex::new(
            "([\u{3041}-\u{3093}\u{30A1}-\u{30F6}\u{03B9}])[\u{309B}\u{FF9E}]",
        )
        .unwrap();
        let mut hit = false;
        let result = re
            .replace_all(text, |caps: &regex::Captures| {
                hit = true;
                format!(
                    "\u{FF3B}\u{FF03}\u{6FC1}\u{70B9}\u{FF3D}{}\u{FF3B}\u{FF03}\u{6FC1}\u{70B9}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
                    &caps[1]
                )
            })
            .to_string();
        if hit {
            self.use_dakuten_font = true;
        }
        result
    }

    pub(super) fn convert_rome_numeric(&self, text: &str) -> String {
        const FROM: &[&str] = &[
            "II", "III", "IV", "VI", "VII", "VIII", "IX", "ii", "iii", "iv", "vi", "vii",
            "viii", "ix",
        ];
        const TO: &[&str] = &[
            "\u{2161}", "\u{2162}", "\u{2163}", "\u{2165}", "\u{2166}", "\u{2167}", "\u{2168}",
            "\u{2171}", "\u{2172}", "\u{2173}", "\u{2175}", "\u{2176}", "\u{2177}", "\u{2178}",
        ];

        let mut result = text.to_string();
        for (from, to) in FROM.iter().zip(TO.iter()) {
            let re = Regex::new(&format!(r"([^A-Za-z]){}([^A-Za-z])", regex::escape(from)))
                .unwrap();
            result = re.replace_all(&result, format!("$1{to}$2")).to_string();
        }
        result
    }

    pub(super) fn stash_kanji_num(&mut self, text: &str) -> String {
        let re = Regex::new(r"[〇一二三四五六七八九十百千万億兆京]+").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let matched = caps.get(0).unwrap();
            let prev = text[..matched.start()].chars().last();
            let next = text[matched.end()..].chars().next();
            if prev.is_some_and(is_arabic_digit) || next.is_some_and(is_arabic_digit) {
                return matched.as_str().to_string();
            }

            let index = self.kanji_num_stash.len();
            self.kanji_num_stash.push(matched.as_str().to_string());
            format!("［＃漢数字＝{}］", usize_to_kanji_digits(index))
        })
        .to_string()
    }

    pub(super) fn convert_kanji_num_with_unit(&self, text: &str, lower_digit_zero: i64) -> String {
        let re = Regex::new(r"[〇一二三四五六七八九十百千万億兆京]+").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let matched = &caps[0];
            let Some(total) = kanji_num_to_integer(matched) else {
                return matched.to_string();
            };
            let total_string = total.to_string();
            if total == 0 || total_string.len() > 20 {
                return matched.to_string();
            }

            let kanji_digits = digits_to_kanji(&total_string);
            if !has_trailing_kanji_zeros(&kanji_digits, lower_digit_zero) {
                return matched.to_string();
            }

            kanji_digits_to_unit_expression(&kanji_digits)
        })
        .to_string()
    }

    pub(super) fn rebuild_kanji_num(&self, data: &mut String) {
        let re = Regex::new(r"［＃漢数字＝(.+?)］").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let Some(index) = marker_index_to_usize(&caps[1]) else {
                    return caps[0].to_string();
                };
                self.kanji_num_stash
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| caps[0].to_string())
            })
            .to_string();
    }

    pub(super) fn alphabet_to_zenkaku(&mut self, text: &str) -> String {
        if self.settings.enable_alphabet_force_zenkaku {
            let re = Regex::new(r"[A-Za-z]+").unwrap();
            return re
                .replace_all(text, |caps: &regex::Captures| {
                    ascii_letters_to_fullwidth(&caps[0])
                })
                .to_string();
        }

        let re = Regex::new(r#"[A-Za-z0-9_.,!?'" &:;-]+"#).unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let word = &caps[0];
            if self.settings.disable_alphabet_word_to_zenkaku && has_ascii_alpha(word) {
                let index = self.english_stash.len();
                self.english_stash.push(word.to_string());
                format!("\u{FF3B}\u{FF03}\u{82F1}\u{6587}\u{FF1D}{index}\u{FF3D}")
            } else {
                ascii_letters_to_fullwidth(word)
            }
        })
        .to_string()
    }

    pub(super) fn rebuild_english_sentences(&self, data: &mut String) {
        for (index, sentence) in self.english_stash.iter().enumerate() {
            let marker = format!("\u{FF3B}\u{FF03}\u{82F1}\u{6587}\u{FF1D}{index}\u{FF3D}");
            *data = data.replacen(&marker, sentence, 1);
        }
    }

    pub(super) fn convert_fraction_and_date(&mut self, text: &str) -> String {
        if !self.settings.enable_transform_fraction && !self.settings.enable_transform_date {
            return text.to_string();
        }

        let re =
            Regex::new(r"[0-9０-９〇一二三四五六七八九十百千万億兆京垓/／]+").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let matched = &caps[0];
            let numerics: Vec<&str> = matched.split(['/', '／']).collect();
            match numerics.len() {
                2 if self.settings.enable_transform_fraction => format!(
                    "{}分の{}",
                    zenkaku_num_to_kanji_literal(numerics[1]),
                    zenkaku_num_to_kanji_literal(numerics[0])
                ),
                3 if self.settings.enable_transform_date => {
                    let year = ruby_numeric_to_i(numerics[0]);
                    let month = ruby_numeric_to_i(numerics[1]);
                    let day = ruby_numeric_to_i(numerics[2]);
                    let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month as u32, day as u32)
                    else {
                        return matched.to_string();
                    };
                    let formatted = date.format(&self.settings.date_format).to_string();
                    self.convert_numbers(&formatted)
                }
                _ => matched.to_string(),
            }
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

fn ascii_letters_to_fullwidth(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'a'..='z' => char::from_u32(c as u32 - 'a' as u32 + 'ａ' as u32).unwrap_or(c),
            'A'..='Z' => char::from_u32(c as u32 - 'A' as u32 + 'Ａ' as u32).unwrap_or(c),
            _ => c,
        })
        .collect()
}

fn has_ascii_alpha(text: &str) -> bool {
    text.bytes().any(|b| b.is_ascii_alphabetic())
}

fn is_arabic_digit(ch: char) -> bool {
    ch.is_ascii_digit() || matches!(ch, '０'..='９')
}

fn zenkaku_num_to_kanji_literal(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '０'..='９' => KANJI_DIGITS[(c as u32 - '０' as u32) as usize],
            _ => c,
        })
        .collect()
}

fn ruby_numeric_to_i(text: &str) -> i32 {
    let mut digits = String::new();
    for ch in text.chars() {
        let digit = match ch {
            '0'..='9' => Some(ch),
            '０'..='９' => char::from_u32(ch as u32 - '０' as u32 + '0' as u32),
            '〇' => Some('0'),
            '一' => Some('1'),
            '二' => Some('2'),
            '三' => Some('3'),
            '四' => Some('4'),
            '五' => Some('5'),
            '六' => Some('6'),
            '七' => Some('7'),
            '八' => Some('8'),
            '九' => Some('9'),
            _ => None,
        };
        match digit {
            Some(digit) => digits.push(digit),
            None if digits.is_empty() => return 0,
            None => break,
        }
    }
    digits.parse::<i32>().unwrap_or(0)
}

fn usize_to_kanji_digits(value: usize) -> String {
    value
        .to_string()
        .chars()
        .map(|c| KANJI_DIGITS[(c as u32 - '0' as u32) as usize])
        .collect()
}

fn marker_index_to_usize(text: &str) -> Option<usize> {
    let mut digits = String::new();
    for ch in text.chars() {
        let digit = match ch {
            '0'..='9' => ch,
            '０'..='９' => char::from_u32(ch as u32 - '０' as u32 + '0' as u32)?,
            '〇' => '0',
            '一' => '1',
            '二' => '2',
            '三' => '3',
            '四' => '4',
            '五' => '5',
            '六' => '6',
            '七' => '7',
            '八' => '8',
            '九' => '9',
            _ => return None,
        };
        digits.push(digit);
    }
    digits.parse().ok()
}

fn digits_to_kanji(text: &str) -> String {
    text.chars()
        .map(|c| KANJI_DIGITS[(c as u32 - '0' as u32) as usize])
        .collect()
}

fn has_trailing_kanji_zeros(text: &str, lower_digit_zero: i64) -> bool {
    if lower_digit_zero <= 0 {
        return true;
    }
    let required = lower_digit_zero as usize;
    let trailing = text.chars().rev().take_while(|&c| c == '〇').count();
    trailing >= required
}

fn kanji_num_to_integer(text: &str) -> Option<u128> {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    let mut total = 0u128;

    while index < chars.len() {
        let mut num = String::new();
        while index < chars.len() && (is_kanji_digit(chars[index]) || is_small_kanji_unit(chars[index])) {
            num.push(chars[index]);
            index += 1;
        }

        if num.is_empty() {
            return None;
        }

        let mut units = String::new();
        while index < chars.len() && is_large_kanji_unit(chars[index]) {
            units.push(chars[index]);
            index += 1;
        }

        let mut value = calc_kanji_num_with_unit(&num)?;
        for unit in units.chars() {
            value = value.checked_mul(10u128.pow(kanji_unit_digit(unit)?))?;
        }
        total = total.checked_add(value)?;
    }

    Some(total)
}

fn calc_kanji_num_with_unit(text: &str) -> Option<u128> {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    let mut total = 0u128;

    while index < chars.len() {
        let mut num = String::new();
        while index < chars.len() && is_kanji_digit(chars[index]) {
            num.push(chars[index]);
            index += 1;
        }

        let mut units = String::new();
        while index < chars.len() && is_small_kanji_unit(chars[index]) {
            units.push(chars[index]);
            index += 1;
        }

        if num.is_empty() && units.is_empty() {
            return None;
        }

        let num_digits = if num.is_empty() {
            "1".to_string()
        } else {
            kanji_digits_to_ascii(&num)?
        };
        let value = if units.is_empty() {
            num_digits.parse::<u128>().ok()?
        } else {
            let unit_sum = calc_sum_unit(&units)?;
            let suffix = unit_sum.to_string().chars().skip(1).collect::<String>();
            format!("{num_digits}{suffix}").parse::<u128>().ok()?
        };
        total = total.checked_add(value)?;
    }

    Some(total)
}

fn calc_sum_unit(units: &str) -> Option<u128> {
    let mut sum = 0u128;
    for unit in units.chars() {
        sum = sum.checked_add(10u128.pow(kanji_unit_digit(unit)?))?;
    }
    Some(sum)
}

fn kanji_digits_to_ascii(text: &str) -> Option<String> {
    let mut result = String::new();
    for ch in text.chars() {
        result.push(match ch {
            '〇' => '0',
            '一' => '1',
            '二' => '2',
            '三' => '3',
            '四' => '4',
            '五' => '5',
            '六' => '6',
            '七' => '7',
            '八' => '8',
            '九' => '9',
            _ => return None,
        });
    }
    Some(result)
}

fn kanji_digits_to_unit_expression(text: &str) -> String {
    const LARGE_UNITS: [&str; 5] = ["", "万", "億", "兆", "京"];
    const SMALL_UNITS: [&str; 4] = ["", "十", "百", "千"];

    let chars: Vec<char> = text.chars().collect();
    let first_group_len = match chars.len() % 4 {
        0 => 4,
        n => n,
    };
    let mut groups: Vec<&[char]> = Vec::new();
    groups.push(&chars[..first_group_len]);
    let mut index = first_group_len;
    while index < chars.len() {
        groups.push(&chars[index..index + 4]);
        index += 4;
    }

    let keta = groups.len() - 1;
    let mut result = String::new();
    for (group_index, group) in groups.iter().enumerate() {
        let mut four_digit = String::new();
        for (digit_index, digit) in group.iter().enumerate() {
            if *digit == '〇' {
                continue;
            }
            let kurai = SMALL_UNITS[group.len() - digit_index - 1];
            if *digit == '一' && !kurai.is_empty() && !(keta > 0 && kurai == "千") {
                four_digit.push_str(kurai);
            } else {
                four_digit.push(*digit);
                four_digit.push_str(kurai);
            }
        }

        if !four_digit.is_empty() {
            result.push_str(&four_digit);
            result.push_str(LARGE_UNITS[keta - group_index]);
        }
    }

    result
}

fn is_kanji_digit(ch: char) -> bool {
    matches!(ch, '〇' | '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九')
}

fn is_small_kanji_unit(ch: char) -> bool {
    matches!(ch, '十' | '百' | '千')
}

fn is_large_kanji_unit(ch: char) -> bool {
    matches!(ch, '万' | '億' | '兆' | '京')
}

fn kanji_unit_digit(ch: char) -> Option<u32> {
    match ch {
        '十' => Some(1),
        '百' => Some(2),
        '千' => Some(3),
        '万' => Some(4),
        '億' => Some(8),
        '兆' => Some(12),
        '京' => Some(16),
        _ => None,
    }
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
    use crate::converter::device::Device;
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

    #[test]
    fn ruby_parity_roman_numerals_are_converted_before_alphabet_width() {
        let mut cb = ConverterBase::new(NovelSettings::default());
        let out = cb.convert("第 II 部", TextType::Story);
        assert!(out.contains("第 Ⅱ 部"), "{out}");
        assert!(!out.contains("ＩＩ"), "{out}");
    }

    #[test]
    fn ruby_parity_english_sentences_are_fullwidth_by_default() {
        let mut cb = ConverterBase::new(NovelSettings::default());
        let out = cb.convert("Hello world.", TextType::Story);
        assert!(out.contains("Ｈｅｌｌｏ ｗｏｒｌｄ．"), "{out}");
    }

    #[test]
    fn ruby_parity_disable_alphabet_word_to_zenkaku_keeps_short_words_halfwidth() {
        let mut settings = NovelSettings::default();
        settings.disable_alphabet_word_to_zenkaku = true;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert("API", TextType::Story);
        assert_eq!(out, "API");
    }

    #[test]
    fn ruby_parity_fraction_and_date_settings_are_applied() {
        let mut settings = NovelSettings::default();
        settings.enable_transform_fraction = true;
        settings.enable_transform_date = true;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert("1/2 2026/7/8", TextType::Story);
        assert!(out.contains("二分の一"), "{out}");
        assert!(out.contains("２０２６年"), "{out}");
    }

    #[test]
    fn ruby_parity_kanji_numbers_with_units_are_applied_after_digit_conversion() {
        let mut settings = NovelSettings::default();
        settings.enable_kanji_num_with_units_explicit = true;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert("1000円と8001000円と一万歩", TextType::Body);
        assert!(out.contains("千円"), "{out}");
        assert!(out.contains("八百万一千円"), "{out}");
        assert!(out.contains("一万歩"), "{out}");
    }

    #[test]
    fn ruby_parity_kanji_numbers_with_units_are_not_applied_without_explicit_setting() {
        let mut cb = ConverterBase::new(NovelSettings::default());
        let out = cb.convert("3000円", TextType::Body);
        assert!(out.contains("三〇〇〇円"), "{out}");
    }

    #[test]
    fn ruby_parity_kanji_numbers_with_units_respects_setting() {
        let mut settings = NovelSettings::default();
        settings.enable_kanji_num_with_units = false;
        settings.enable_kanji_num_with_units_explicit = true;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert("1000円", TextType::Body);
        assert!(out.contains("一〇〇〇円"), "{out}");
    }

    #[test]
    fn ruby_parity_kindle_arrow_and_zws_are_device_gated() {
        let mut settings = NovelSettings::default();
        settings.enable_insert_char_separator = true;
        let mut cb = ConverterBase::new(settings);
        cb.target_device = Some(Device::Mobi);

        let out = cb.convert("あ⇒い", TextType::Body);

        assert!(out.contains("→"), "{out}");
        assert!(out.contains("［＃zws］"), "{out}");
        assert!(!out.contains("⇒"), "{out}");
    }

    #[test]
    fn ruby_parity_dakuten_font_off_does_not_convert() {
        let mut settings = NovelSettings::default();
        settings.enable_dakuten_font = false;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert_dakuten_char_to_font("あ\u{309B}い");
        assert_eq!(out, "あ\u{309B}い");
        assert!(!cb.use_dakuten_font);
    }

    #[test]
    fn ruby_parity_dakuten_font_fullwidth_mark_replaced() {
        let mut settings = NovelSettings::default();
        settings.enable_dakuten_font = true;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert_dakuten_char_to_font("あ\u{309B}い");
        assert_eq!(out, "［＃濁点］あ［＃濁点終わり］い");
        assert!(cb.use_dakuten_font);
    }

    #[test]
    fn ruby_parity_dakuten_font_halfwidth_mark_replaced() {
        let mut settings = NovelSettings::default();
        settings.enable_dakuten_font = true;
        let mut cb = ConverterBase::new(settings);
        // U+FF9E (halfwidth katakana voiced sound mark) on katakana ヴ-base char.
        let out = cb.convert_dakuten_char_to_font("カ\u{FF9E}");
        assert_eq!(out, "［＃濁点］カ［＃濁点終わり］");
        assert!(cb.use_dakuten_font);
    }

    #[test]
    fn ruby_parity_dakuten_font_no_match_keeps_flag_false() {
        let mut settings = NovelSettings::default();
        settings.enable_dakuten_font = true;
        let mut cb = ConverterBase::new(settings);
        let out = cb.convert_dakuten_char_to_font("あいうえお");
        assert_eq!(out, "あいうえお");
        assert!(!cb.use_dakuten_font);
    }
}
