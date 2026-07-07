use regex::Regex;

use super::{ConverterBase, TextType};
use crate::converter::device::Device;

impl ConverterBase {
    pub(super) fn rstrip_all_lines(&self, text: &str) -> String {
        text.lines()
            .map(|line| line.trim_end_matches(|c: char| c.is_whitespace()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn auto_join_in_brackets(&self, text: &str) -> String {
        if !self.settings.enable_auto_join_in_brackets && !self.settings.enable_inspect {
            return text.to_string();
        }
        let mut result = text.to_string();
        for (open, close) in [('\u{300C}', '\u{300D}'), ('\u{300E}', '\u{300F}')] {
            let mut replacements = Vec::new();
            let mut transformed = String::new();
            let mut last = 0usize;
            let mut depth = 0usize;
            let mut start = None;

            for (idx, ch) in result.char_indices() {
                if ch == open {
                    if depth == 0 {
                        start = Some(idx);
                    }
                    depth += 1;
                } else if ch == close && depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        let begin = start.take().unwrap();
                        let end = idx + ch.len_utf8();
                        transformed.push_str(&result[last..begin]);

                        let raw = &result[begin..end];
                        let replacement = if self.settings.enable_auto_join_in_brackets {
                            if let Some(joined) = join_inner_bracket(raw) {
                                let blocked = self
                                    .inspector
                                    .as_ref()
                                    .map(|inspector| {
                                        inspector
                                            .borrow_mut()
                                            .validate_joined_inner_brackets(raw, &joined)
                                    })
                                    .unwrap_or(false);
                                if blocked { raw.to_string() } else { joined }
                            } else {
                                raw.to_string()
                            }
                        } else {
                            raw.to_string()
                        };

                        transformed.push_str(&format!("［＃かぎ括弧＝{}］", replacements.len()));
                        replacements.push(replacement);
                        last = end;
                    }
                }
            }
            transformed.push_str(&result[last..]);

            if self.settings.enable_inspect {
                if let Some(ref inspector) = self.inspector {
                    inspector.borrow_mut().inspect_invalid_openclose_brackets(
                        &transformed,
                        open,
                        close,
                        &replacements,
                    );
                }
            }

            result = rebuild_brackets(&transformed, &replacements);
        }
        result
    }

    pub(super) fn auto_join_line(&self, text: &str) -> String {
        let re = Regex::new(r"([^、])、\n　([^「『\(（【<＜〈《≪・■…‥―　１-９一-九])").unwrap();
        re.replace_all(text, "$1、$2").to_string()
    }

    pub(super) fn erase_comments_block(&self, text: &str) -> String {
        let re = Regex::new(r"(?m)^-{5,}.*$").unwrap();
        re.replace_all(text, "").to_string()
    }

    pub(super) fn convert_page_break(&self, text: &str) -> String {
        let threshold = self.settings.to_page_break_threshold;
        if threshold < 1 {
            return text.to_string();
        }
        let pattern = format!("(^\n){{{},}}", threshold);
        let re = Regex::new(&pattern).unwrap();
        re.replace_all(text, "\u{FF3B}\u{FF03}\u{6539}\u{9801}\u{FF3D}\n")
            .to_string()
    }

    pub(super) fn convert_novel_rule(&self, text: &str) -> String {
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

    pub(super) fn convert_horizontal_ellipsis(&self, text: &str) -> String {
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

    pub(super) fn delete_dust_char(&self, data: &mut String) {
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

    pub(super) fn replace_by_replace_txt(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (src, dst) in &self.settings.replace_patterns {
            result = result.replace(src, dst);
        }
        result
    }

    pub(super) fn convert_arrow(&self, text: &str) -> String {
        if self.target_device != Some(Device::Mobi) {
            return text.to_string();
        }
        text.replace('⇒', "→").replace('⇐', "←")
    }

    pub(super) fn insert_separator_for_selection(&self, text: &str) -> String {
        if !matches!(self.text_type, TextType::Body | TextType::TextFile) {
            return text.to_string();
        }
        if self.target_device != Some(Device::Mobi) {
            return text.to_string();
        }
        if self.settings.enable_insert_word_separator {
            insert_word_separator(text, self.text_type == TextType::TextFile)
        } else if self.settings.enable_insert_char_separator {
            insert_char_separator(text)
        } else {
            text.to_string()
        }
    }
}

const WORD_SEPARATOR: &str = "［＃zws］";

fn insert_word_separator(text: &str, skip_textfile_header: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut output = String::new();
    let mut index = 0;
    let mut before_symbol = false;

    if skip_textfile_header {
        let mut newlines = 0;
        while index < chars.len() && newlines < 2 {
            let ch = chars[index];
            output.push(ch);
            index += 1;
            if ch == '\n' {
                newlines += 1;
            }
        }
    }

    while index < chars.len() {
        if let Some((token, next)) = take_annotation_or_tag(&chars, index) {
            append_word_token(&mut output, &mut before_symbol, &token);
            index = next;
            continue;
        }

        let ch = chars[index];
        if is_opening_no_separator(ch) {
            output.push(ch);
            before_symbol = false;
            index += 1;
            continue;
        }

        if let Some((token, next)) = take_word_group(&chars, index) {
            append_word_token(&mut output, &mut before_symbol, &token);
            index = next;
            continue;
        }

        output.push(ch);
        before_symbol = true;
        index += 1;
    }

    output
}

fn append_word_token(output: &mut String, before_symbol: &mut bool, token: &str) {
    if *before_symbol {
        output.push_str(WORD_SEPARATOR);
    }
    output.push_str(token);
    output.push_str(WORD_SEPARATOR);
    *before_symbol = false;
}

fn insert_char_separator(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut output = String::new();
    let mut index = 0;
    let mut before_symbol = false;

    while index < chars.len() {
        if let Some((token, next)) = take_annotation_or_tag(&chars, index) {
            output.push_str(&token);
            before_symbol = false;
            index = next;
            continue;
        }

        let ch = chars[index];
        if is_opening_no_separator(ch) {
            output.push(ch);
            before_symbol = false;
        } else if is_separator_symbol(ch) {
            output.push(ch);
            before_symbol = true;
        } else {
            if before_symbol {
                output.push_str(WORD_SEPARATOR);
            }
            output.push(ch);
            output.push_str(WORD_SEPARATOR);
            before_symbol = false;
        }
        index += 1;
    }

    output
}

fn take_annotation_or_tag(chars: &[char], index: usize) -> Option<(String, usize)> {
    match chars.get(index).copied()? {
        '｜' => take_until(chars, index, '》'),
        '［' if chars.get(index + 1) == Some(&'＃') => take_until(chars, index, '］'),
        '<' => take_until(chars, index, '>'),
        _ => None,
    }
}

fn take_until(chars: &[char], index: usize, close: char) -> Option<(String, usize)> {
    let mut token = String::new();
    let mut cursor = index;
    while let Some(&ch) = chars.get(cursor) {
        token.push(ch);
        cursor += 1;
        if ch == close {
            return Some((token, cursor));
        }
    }
    None
}

fn take_word_group(chars: &[char], index: usize) -> Option<(String, usize)> {
    let ch = *chars.get(index)?;
    let kind = word_kind(ch)?;
    let mut token = String::new();
    let mut cursor = index;
    while let Some(&current) = chars.get(cursor) {
        if word_kind(current) == Some(kind) || is_word_connector(current, kind) {
            token.push(current);
            cursor += 1;
        } else {
            break;
        }
    }
    Some((token, cursor))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Hiragana,
    Katakana,
    Alphabet,
    Kanji,
}

fn word_kind(ch: char) -> Option<WordKind> {
    match ch {
        'ぁ'..='ん' | 'ゝ' | 'ゞ' => Some(WordKind::Hiragana),
        'ァ'..='ヶ' => Some(WordKind::Katakana),
        'Ａ'..='Ｚ' | 'ａ'..='ｚ' | 'A'..='Z' | 'a'..='z' => Some(WordKind::Alphabet),
        '一'..='龥' | '朗'..='鶴' => Some(WordKind::Kanji),
        _ => None,
    }
}

fn is_word_connector(ch: char, kind: WordKind) -> bool {
    match kind {
        WordKind::Hiragana => ch == 'ー',
        WordKind::Katakana => ch == 'ー' || ch == '・',
        WordKind::Alphabet => ch == ' ',
        WordKind::Kanji => false,
    }
}

fn is_opening_no_separator(ch: char) -> bool {
    matches!(ch, '〔' | '「' | '『' | '(' | '（' | '【' | '〈' | '《' | '≪' | '〝')
}

fn is_separator_symbol(ch: char) -> bool {
    matches!(ch, '―' | '…' | '!' | '?' | '！' | '？' | '※')
}

fn join_inner_bracket(text: &str) -> Option<String> {
    if !text.contains('\n') {
        return None;
    }

    let re = Regex::new(r"([…―])\n").unwrap();
    let joined = re.replace_all(text, "$1。\n").to_string();
    Some(
        joined
            .split('\n')
            .map(|line| line.trim_start_matches('\u{3000}'))
            .collect::<Vec<_>>()
            .join(""),
    )
}

fn rebuild_brackets(text: &str, replacements: &[String]) -> String {
    let re = Regex::new(r"［＃かぎ括弧＝(\d+)］").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let index = caps[1].parse::<usize>().unwrap_or(usize::MAX);
        replacements
            .get(index)
            .cloned()
            .unwrap_or_else(|| caps[0].to_string())
    })
    .to_string()
}

pub fn zenkaku_rstrip(line: &str) -> String {
    line.trim_end_matches(|c: char| c == '\u{3000}' || c.is_whitespace())
        .to_string()
}

pub fn tcy(text: &str) -> String {
    format!(
        "\u{FF3B}\u{FF03}\u{7E26}\u{4E2D}\u{6A2A}\u{FF3D}{}\u{FF3B}\u{FF03}\u{7E26}\u{4E2D}\u{6A2A}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
        text
    )
}

pub fn is_blank_line(line: &str) -> bool {
    line.trim().is_empty()
}

pub fn is_border_symbol(line: &str) -> bool {
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
