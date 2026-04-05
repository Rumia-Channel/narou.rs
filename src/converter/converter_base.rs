use regex::Regex;

use super::settings::NovelSettings;

pub struct ConverterBase {
    pub settings: NovelSettings,
    url_stash: Vec<String>,
    english_stash: Vec<String>,
    illust_stash: Vec<String>,
    kanji_num_stash: Vec<String>,
    kome_count: usize,
}

#[derive(Debug, Clone, Copy)]
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
            url_stash: Vec::new(),
            english_stash: Vec::new(),
            illust_stash: Vec::new(),
            kanji_num_stash: Vec::new(),
            kome_count: 0,
        }
    }

    pub fn convert(&mut self, text: &str, text_type: TextType) -> String {
        let mut result = text.to_string();

        result = self.rstrip_all_lines(&result);
        result = self.hankakukana_to_zenkakukana(&result);
        result = self.auto_join_in_brackets(&result);
        if self.settings.enable_auto_join_line {
            result = self.auto_join_line(&result);
        }
        result = self.erase_comments_block(&result);
        result = self.replace_url(&result);
        result = self.replace_narou_tag(&result);
        result = self.convert_numbers(&result);
        result = self.insert_separate_space(&result);
        result = self.convert_special_characters(&result);
        result = self.modify_kana_ni_to_kanji_ni(&result);

        result = self.convert_main_loop(&result, text_type);

        if self.settings.enable_ruby {
            result = self.narou_ruby(&result);
        }

        if self.settings.enable_convert_horizontal_ellipsis {
            result = self.convert_horizontal_ellipsis(&result);
        }

        result = self.rebuild_url(&result);
        result = self.rebuild_kome_to_gaiji(&result);
        result = self.rebuild_illust(&result);
        result = self.delete_dust_char(&result);

        result
    }

    pub fn convert_multi(&mut self, inputs: &[(String, TextType)]) -> Vec<String> {
        inputs
            .iter()
            .map(|(text, tt)| self.convert(text, *tt))
            .collect()
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
        let lines: Vec<&str> = text.lines().collect();
        let mut result = Vec::new();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i];
            if i + 1 < lines.len() {
                let next = lines[i + 1];
                let trimmed_next = next.trim_start_matches('\u{3000}');

                let should_join = line.ends_with('\u{3001}')
                    || line.ends_with('\u{3002}')
                    || line.ends_with('\u{FF01}')
                    || line.ends_with('\u{FF1F}')
                    || line.ends_with('\u{2026}')
                    || line.ends_with('\u{2025}')
                    || (line.ends_with('\u{300C}') && !next.starts_with('\u{3000}'));

                if should_join && !trimmed_next.is_empty() && !is_special_line_start(trimmed_next) {
                    result.push(format!("{}{}", line, trimmed_next));
                    i += 2;
                    continue;
                }
            }
            result.push(line.to_string());
            i += 1;
        }

        result.join("\n")
    }

    fn erase_comments_block(&self, text: &str) -> String {
        let re = Regex::new(r"(?m)^-{5,}.*$").unwrap();
        re.replace_all(text, "").to_string()
    }

    fn replace_url(&mut self, text: &str) -> String {
        let re = Regex::new(r#"https?://[^\s<>"']+"#).unwrap();
        let result = re
            .replace_all(text, |caps: &regex::Captures| {
                let url = caps[0].to_string();
                let idx = self.url_stash.len();
                self.url_stash.push(url);
                format!("\u{FF3B}\u{FF23}URL={}\u{FF3D}", idx)
            })
            .to_string();
        result
    }

    fn replace_narou_tag(&self, text: &str) -> String {
        let result = text.replace("\u{3010}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{3011}", "");
        result
    }

    fn convert_numbers(&mut self, text: &str) -> String {
        if self.settings.enable_convert_num_to_kanji {
            self.convert_numbers_to_kanji(text)
        } else {
            self.hankaku_num_to_zenkaku(text)
        }
    }

    fn convert_numbers_to_kanji(&mut self, text: &str) -> String {
        let re = Regex::new(r"\d+").unwrap();
        let result = re
            .replace_all(text, |caps: &regex::Captures| {
                let num_str = &caps[0];
                if num_str.contains(',') {
                    let idx = self.kanji_num_stash.len();
                    self.kanji_num_stash.push(num_str.to_string());
                    return format!("\u{FF3B}\u{FF23}KNUM={}\u{FF3D}", idx);
                }
                if let Ok(num) = num_str.parse::<u64>() {
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

    fn convert_special_characters(&mut self, text: &str) -> String {
        let mut result = text.to_string();

        let symbols: &[(&str, &str)] = &[
            ("!", "\u{FF01}"),
            ("?", "\u{FF1F}"),
            ("#", "\u{FF03}"),
            ("$", "\u{FF04}"),
            ("%", "\u{FF05}"),
            ("&", "\u{FF06}"),
            ("(", "\u{FF08}"),
            (")", "\u{FF09}"),
            ("*", "\u{FF0A}"),
            ("+", "\u{FF0B}"),
            (",", "\u{FF0C}"),
            ("-", "\u{FF0D}"),
            (".", "\u{3002}"),
            ("/", "\u{FF0F}"),
            (":", "\u{FF1A}"),
            (";", "\u{FF1B}"),
            ("<", "\u{FF1C}"),
            ("=", "\u{FF1D}"),
            (">", "\u{FF1E}"),
            ("@", "\u{FF20}"),
            ("[", "\u{FF3B}"),
            ("]", "\u{FF3D}"),
            ("^", "\u{FF3E}"),
            ("_", "\u{FF3F}"),
            ("`", "\u{FF40}"),
            ("{", "\u{FF5B}"),
            ("|", "\u{FF5C}"),
            ("}", "\u{FF5D}"),
            ("~", "\u{FF5E}"),
            ("\\", "\u{FFE5}"),
        ];

        for (half, full) in symbols {
            result = result.replace(half, full);
        }

        result = self.stash_kome(&result);
        result = self.convert_double_angle_quotation_to_gaiji(&result);
        result = self.convert_novel_rule(&result);
        result = self.convert_head_half_spaces(&result);

        result
    }

    fn stash_kome(&mut self, text: &str) -> String {
        let count = text.matches('\u{203B}').count();
        self.kome_count = count;
        text.to_string()
    }

    fn convert_double_angle_quotation_to_gaiji(&self, text: &str) -> String {
        let result = text.replace('\u{226A}', "\u{203B}\u{FF3B}\u{FF23}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}");
        let result = result.replace('\u{226B}', "\u{203B}\u{FF3B}\u{FF23}\u{7D42}\u{308F}\u{308A}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}");
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
        let mut result = text.to_string();
        let re = Regex::new(r"\u{30CB}").unwrap();
        result = re
            .replace_all(&result, |_: &regex::Captures| "\u{4E8C}".to_string())
            .to_string();
        result
    }

    fn convert_main_loop(&self, text: &str, text_type: TextType) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut result = Vec::new();

        for line in &lines {
            let processed = self.process_line(line, text_type);
            result.push(processed);
        }

        result.join("\n")
    }

    fn process_line(&self, line: &str, _text_type: TextType) -> String {
        let mut line = line.to_string();

        line = line
            .trim_end_matches(|c: char| c.is_whitespace() || c == '\u{3000}')
            .to_string();

        line
    }

    fn narou_ruby(&self, text: &str) -> String {
        let mut result = text.to_string();

        let guillemet_re = Regex::new(r"\u{226A}(.+?)\u{226B}").unwrap();
        result = guillemet_re
            .replace_all(&result, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                let base = self.find_ruby_base(&result, caps.get(0).unwrap().start());
                format!("\u{FF5C}{}\u{300C}{}\u{300D}", base, ruby_text)
            })
            .to_string();

        let paren_re = Regex::new(r"\u{FF08}(.+?)\u{FF09}").unwrap();
        result = paren_re
            .replace_all(&result, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                if ruby_text.is_empty() || ruby_text.starts_with(' ') {
                    return caps[0].to_string();
                }
                let base = self.find_ruby_base(&result, caps.get(0).unwrap().start());
                if base.is_empty() {
                    return caps[0].to_string();
                }
                format!("\u{FF5C}{}\u{300C}{}\u{300D}", base, ruby_text)
            })
            .to_string();

        result
    }

    fn find_ruby_base(&self, text: &str, pos: usize) -> String {
        let before = &text[..pos];
        let chars: Vec<char> = before.chars().collect();

        let mut base = String::new();
        let ruby_eligible = |c: char| -> bool {
            matches!(c,
                '\u{4E00}'..='\u{9FFF}' |
                '\u{3040}'..='\u{309F}' |
                '\u{30A0}'..='\u{30FF}' |
                '\u{FF66}'..='\u{FF9F}' |
                '\u{FF21}'..='\u{FF3A}'
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

    fn rebuild_url(&self, text: &str) -> String {
        let re = Regex::new(r"\u{FF3B}\u{FF23}URL=(\d+)\u{FF3D}").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let idx: usize = caps[1].parse().unwrap_or(0);
            self.url_stash.get(idx).cloned().unwrap_or_default()
        })
        .to_string()
    }

    fn rebuild_kome_to_gaiji(&self, text: &str) -> String {
        text.to_string()
    }

    fn rebuild_illust(&self, text: &str) -> String {
        let re = Regex::new(r"\u{FF3B}\u{FF23}ILUST=(\d+)\u{FF3D}").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            let idx: usize = caps[1].parse().unwrap_or(0);
            self.illust_stash.get(idx).cloned().unwrap_or_default()
        })
        .to_string()
    }

    fn delete_dust_char(&self, text: &str) -> String {
        text.chars()
            .filter(|&c| {
                !matches!(c as u32,
                    0x200B..=0x200F |
                    0x2028..=0x202F |
                    0x2060..=0x206F |
                    0xFEFF
                )
            })
            .collect()
    }

    fn insert_separate_space(&self, text: &str) -> String {
        if !self.settings.enable_insert_word_separator
            && !self.settings.enable_insert_char_separator
        {
            return text.to_string();
        }
        text.to_string()
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
    char::from_u32(ch as u32 - '0' as u32 + 0xFF10).unwrap_or(ch)
}

fn is_special_line_start(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let first = s.chars().next().unwrap();
    matches!(
        first,
        '\u{3000}'
            | '\u{3001}'
            | '\u{3002}'
            | '\u{2026}'
            | '\u{2025}'
            | '\u{2015}'
            | '\u{30FC}'
            | '\u{300D}'
            | '\u{300F}'
            | '\u{FF09}'
            | '\u{FF5D}'
            | '\u{300B}'
            | '\u{3011}'
            | '\u{FF1D}'
            | '\u{FF01}'
            | '\u{2605}'
            | '\u{2606}'
            | '\u{266A}'
            | '\u{FF3B}'
            | '\u{2014}'
            | '\u{30FB}'
            | '\u{2022}'
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
                            if small_digit > 1 || *small_val == digit {
                                result.push(KANJI_DIGITS[small_digit as usize]);
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
