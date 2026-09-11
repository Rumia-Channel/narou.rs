use std::sync::LazyLock;

use regex::Regex;

use super::ConverterBase;

static RE_EXPLICIT_RUBY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u{FF5C}([^《\n]+?)《([^》\n]*?)》").unwrap());
static RE_SESAME_RUBY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u{FF5C}([^《\n]+?)《([・、]+)》").unwrap());
static RE_GUILLEMET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u{226A}(.+?)\u{226B}").unwrap());
static RE_PAREN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u{FF08}(.+?)\u{FF09}").unwrap());
static RE_RUBY_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ぁ-んァ-ヶーゝゞ・]+[ 　]?[ぁ-んァ-ヶーゝゞ・]*$").unwrap()
});

impl ConverterBase {
    pub(super) fn narou_ruby(&self, data: &mut String) {
        let original: &str = data;
        *data = RE_EXPLICIT_RUBY
            .replace_all(original, |caps: &regex::Captures| {
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

        let original: &str = data;
        *data = RE_SESAME_RUBY
            .replace_all(original, |caps: &regex::Captures| {
                format!(
                    "\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{FF3D}{}\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
                    &caps[1]
                )
            })
            .to_string();

        let original: &str = data;
        *data = RE_GUILLEMET
            .replace_all(original, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                let base = self.find_ruby_base(original, caps.get(0).unwrap().start());
                format!("\u{FF5C}{}\u{300C}{}\u{300D}", base, ruby_text)
            })
            .to_string();

        if !self.enable_parenthesized_ruby {
            return;
        }

        let original: &str = data;
        *data = RE_PAREN
            .replace_all(original, |caps: &regex::Captures| {
                let ruby_text = &caps[1];
                if ruby_text.is_empty()
                    || ruby_text.starts_with(' ')
                    || !RE_RUBY_TEXT.is_match(ruby_text)
                {
                    return caps[0].to_string();
                }
                let base = self.find_ruby_base(original, caps.get(0).unwrap().start());
                if base.is_empty() {
                    return caps[0].to_string();
                }
                format!("\u{FF5C}{}\u{300C}{}\u{300D}", base, ruby_text)
            })
            .to_string();
    }

    pub(super) fn find_ruby_base(&self, text: &str, pos: usize) -> String {
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
}
