use regex::Regex;

use super::ConverterBase;

impl ConverterBase {
    pub(super) fn rstrip_all_lines(&self, text: &str) -> String {
        text.lines()
            .map(|line| line.trim_end_matches(|c: char| c.is_whitespace()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn auto_join_in_brackets(&self, text: &str) -> String {
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
