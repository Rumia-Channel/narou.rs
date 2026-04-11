use regex::Regex;

use super::ConverterBase;

impl ConverterBase {
    pub(super) fn insert_separate_space(&self, text: &str) -> String {
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

    pub(super) fn modify_kana_ni_to_kanji_ni(&self, text: &str) -> String {
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

    pub(super) fn convert_prolonged_sound_mark_to_dash(&self, text: &str) -> String {
        let re = Regex::new(r"\u{30FC}{2,}").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            "\u{2015}".repeat(caps[0].chars().count())
        })
        .to_string()
    }

    pub(super) fn convert_head_half_spaces(&self, text: &str) -> String {
        let re = Regex::new(r"(?m)^ +").unwrap();
        re.replace_all(text, |caps: &regex::Captures| {
            " ".repeat(caps[0].len()).replace(' ', "\u{3000}")
        })
        .to_string()
    }

    pub(super) fn half_indent_bracket(&self, data: &mut String) {
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

    pub(super) fn auto_indent(&self, data: &mut String) {
        let re = Regex::new(r"(?m)^(\u{2014}{1,})").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                format!("\u{3000}{}", &caps[1])
            })
            .to_string();

        if self.settings.enable_force_indent || self.settings.enable_auto_indent {
            let ignore_chars = "(\u{FF08}\u{300C}\u{300E}\u{3008}\u{300A}\u{226A}\u{3010}\u{3014}\u{2015}\u{30FB}\u{203B}\u{FF3B}\u{301D}\u{E000}\n";
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
}
