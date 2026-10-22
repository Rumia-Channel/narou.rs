use std::sync::LazyLock;

use regex::Regex;

use super::ConverterBase;

const ILLUST_STASH_MARKER: char = '\u{E010}';
const URL_STASH_MARKER: char = '\u{E011}';
/// stash 番号の終端。可変長の 10 進表記をここまで読む。
const STASH_TERMINATOR: char = '\u{E012}';
/// stash 番号の 10 進各桁 (0-9) を表す PUA 文字。
///
/// 番号は「目印 + 各桁 + 終端」(可変長の 10 進表記) で持つ。従来の
/// `0xE100 + idx` (1 文字) は 6143 件目から `RE_ILLUST_STASH` の範囲
/// (U+E100..U+F8FF) の外へ出て黙って欠落し、約 105 万件で
/// `char::from_u32(...).expect` が panic していた。桁を ASCII/全角数字で
/// 書くと `convert_numbers` などの変換 (全角化・漢数字化・縦中横化) が
/// トークンを壊すため、数字も PUA 文字で表す。桁数に上限は無く panic もしない。
const STASH_DIGITS: [char; 10] = [
    '\u{E020}', '\u{E021}', '\u{E022}', '\u{E023}', '\u{E024}', '\u{E025}', '\u{E026}', '\u{E027}',
    '\u{E028}', '\u{E029}',
];

static RE_ILLUST_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"[ 　\t]*?(\u{FF3B}\u{FF03}\u{633F}\u{7D75}\u{FF08}.+?\u{FF09}\u{5165}\u{308B}\u{FF3D})\n?",
    )
    .unwrap()
});
static RE_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s<>"']+"#).unwrap());
static RE_FORCE_INDENT_CHAPTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\u{FF3B}\u{FF03}\u{7AE0}\u{898B}\u{51FA}\u{3057}\u{3063}\u{307D}\u{3044}\u{6587}\u{FF1D}(\d+)\u{FF3D}")
        .unwrap()
});
static RE_ILLUST_STASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u{E010}([\u{E020}-\u{E029}]+)\u{E012}").unwrap());
static RE_URL_STASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u{E011}([\u{E020}-\u{E029}]+)\u{E012}").unwrap());

impl ConverterBase {
    pub(super) fn replace_illust_tag(&mut self, text: &mut String) {
        let re = &*RE_ILLUST_TAG;
        if !self.settings.enable_illust {
            *text = re.replace_all(text, "").to_string();
            return;
        }
        *text = re
            .replace_all(text, |caps: &regex::Captures| {
                let idx = self.illust_stash.len();
                self.illust_stash.push(caps[1].to_string());
                format!("{}\n", encode_stash_token(ILLUST_STASH_MARKER, idx))
            })
            .to_string();
    }

    pub(super) fn replace_url(&mut self, text: &str) -> String {
        RE_URL
            .replace_all(text, |caps: &regex::Captures| {
                let url = caps[0].to_string();
                let idx = self.url_stash.len();
                self.url_stash.push(url);
                encode_stash_token(URL_STASH_MARKER, idx)
            })
            .to_string()
    }

    pub(super) fn replace_narou_tag(&self, text: &str) -> String {
        text.replace("\u{3010}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{3011}", "")
    }

    pub(super) fn stash_kome(&self, text: &str) -> String {
        text.replace('\u{203B}', "\u{203B}\u{203B}")
    }

    pub(super) fn convert_double_angle_quotation_to_gaiji(&self, text: &str) -> String {
        let result = text.replace(
            '\u{226A}',
            "\u{203B}\u{FF3B}\u{FF03}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        );
        result.replace(
            '\u{226B}',
            "\u{203B}\u{FF3B}\u{FF03}\u{7D42}\u{308F}\u{308A}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        )
    }

    pub(super) fn rebuild_force_indent_chapter(&self, data: &mut String) {
        *data = RE_FORCE_INDENT_CHAPTER
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                self.force_indent_chapter_stash
                    .get(idx)
                    .cloned()
                    .unwrap_or_default()
            })
            .to_string();
    }

    pub(super) fn rebuild_illust(&self, data: &mut String) {
        *data = RE_ILLUST_STASH
            .replace_all(data, |caps: &regex::Captures| {
                let idx = decode_stash_index(&caps[1]).unwrap_or(usize::MAX);
                self.illust_stash.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    pub(super) fn rebuild_url(&self, data: &mut String) {
        *data = RE_URL_STASH
            .replace_all(data, |caps: &regex::Captures| {
                let idx = decode_stash_index(&caps[1]).unwrap_or(usize::MAX);
                self.url_stash.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    pub(super) fn rebuild_kome_to_gaiji(&self, data: &mut String) {
        *data = data.replace(
            "\u{203B}\u{203B}",
            "\u{203B}\u{FF3B}\u{FF03}\u{7C73}\u{5370}\u{3001}1-2-8\u{FF3D}",
        );
    }

    pub(super) fn convert_double_angle_quotation_to_gaiji_post(&self, data: &mut String) {
        *data = data.replace(
            '\u{226A}',
            "\u{203B}\u{FF3B}\u{FF03}\u{59CB}\u{3081}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        );
        *data = data.replace(
            '\u{226B}',
            "\u{203B}\u{FF3B}\u{FF03}\u{7D42}\u{308F}\u{308A}\u{4E8C}\u{91CD}\u{5C71}\u{62EC}\u{5F27}\u{FF3D}",
        );
    }
}

fn encode_stash_token(marker: char, idx: usize) -> String {
    let decimal = idx.to_string();
    let mut token = String::with_capacity(2 + decimal.len());
    token.push(marker);
    token.extend(
        decimal
            .bytes()
            .map(|digit| STASH_DIGITS[usize::from(digit - b'0')]),
    );
    token.push(STASH_TERMINATOR);
    token
}

/// トークンの数字部分を番号へ戻す。桁数は固定しない。壊れた入力や
/// `usize` を超える番号は panic せず `None` を返す。
fn decode_stash_index(encoded: &str) -> Option<usize> {
    if encoded.is_empty() {
        return None;
    }
    let mut index = 0usize;
    for encoded_char in encoded.chars() {
        let digit = STASH_DIGITS
            .iter()
            .position(|stash_digit| *stash_digit == encoded_char)?;
        index = index.checked_mul(10)?.checked_add(digit)?;
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::{
        ConverterBase, ILLUST_STASH_MARKER, STASH_DIGITS, STASH_TERMINATOR, URL_STASH_MARKER,
        decode_stash_index, encode_stash_token,
    };
    use crate::converter::{converter_base::TextType, settings::NovelSettings};

    /// トークンから「目印」と「終端」を外した数字部分を取り出す。
    fn token_digits(token: &str, marker: char) -> &str {
        token
            .strip_prefix(marker)
            .and_then(|rest| rest.strip_suffix(STASH_TERMINATOR))
            .unwrap()
    }

    #[test]
    fn replace_illust_tag_stashes_aozora_annotation() {
        let settings = NovelSettings::default();
        let mut converter = ConverterBase::new(settings);
        let mut text = "前\n［＃挿絵（挿絵/test.jpg）入る］\n後".to_string();

        converter.replace_illust_tag(&mut text);
        converter.rebuild_illust(&mut text);

        assert!(text.contains("［＃挿絵（挿絵/test.jpg）入る］"));
    }

    #[test]
    fn replace_illust_tag_removes_annotation_when_disabled() {
        let settings = NovelSettings {
            enable_illust: false,
            ..NovelSettings::default()
        };
        let mut converter = ConverterBase::new(settings);
        let mut text = "前\n［＃挿絵（挿絵/test.jpg）入る］\n後".to_string();

        converter.replace_illust_tag(&mut text);

        assert!(!text.contains("［＃挿絵（"));
    }

    /// 6143 件の範囲制限と約 105 万件での panic を無くしたことを、境界と
    /// `usize` の上限で固定する。番号は可変長なので桁数に上限が無い。
    #[test]
    fn stash_tokens_roundtrip_private_use_index() {
        for index in [0usize, 12, 6_143, 6_144, 7_000, 2_000_000, usize::MAX] {
            let token = encode_stash_token(ILLUST_STASH_MARKER, index);
            assert!(
                token.starts_with(ILLUST_STASH_MARKER),
                "index {index}: {token:?}"
            );
            assert!(
                token.ends_with(STASH_TERMINATOR),
                "index {index}: {token:?}"
            );
            assert_eq!(
                decode_stash_index(token_digits(&token, ILLUST_STASH_MARKER)),
                Some(index),
                "index {index}"
            );
        }

        // 2 桁なら目印 + 2 + 終端。桁数は番号に応じて伸びる。
        let token = encode_stash_token(ILLUST_STASH_MARKER, 12);
        assert_eq!(token.chars().count(), 1 + 2 + 1);

        // 壊れた数字部分は panic せず None。
        assert_eq!(decode_stash_index(""), None);
        assert_eq!(decode_stash_index("x"), None);
        // usize を超える番号も panic せず None。
        let too_large = "9".repeat(64);
        let too_large: String = too_large
            .bytes()
            .map(|digit| STASH_DIGITS[usize::from(digit - b'0')])
            .collect();
        assert_eq!(decode_stash_index(&too_large), None);
        // 数字の途中に別文字が混ざっても None。
        let mut broken = String::new();
        broken.push(STASH_DIGITS[1]);
        broken.push('2');
        assert_eq!(decode_stash_index(&broken), None);
    }

    /// 6143 件を超える URL も stash / rebuild で欠落しないこと。
    /// (従来は 6144 件目から PUA の範囲外になり黙って 1 文字残っていた)
    #[test]
    fn thousands_of_urls_are_rebuilt_exactly() {
        let settings = NovelSettings::default();
        let mut converter = ConverterBase::new(settings);
        let urls: Vec<String> = (0..7_000)
            .map(|index| format!("https://example.com/novel/{index}/?q={}", index * 2))
            .collect();
        let text = urls.join("\n");

        let mut stashed = converter.replace_url(&text);
        assert!(
            !stashed.contains("https://"),
            "URLs must be stashed before rebuild"
        );
        assert_eq!(
            stashed.matches(URL_STASH_MARKER).count(),
            7_000,
            "every URL must become one token, whatever its index"
        );
        converter.rebuild_url(&mut stashed);

        assert_eq!(stashed, text);
        assert_eq!(converter.url_stash.len(), 7_000);
    }

    /// 実際の変換パイプライン (`convert_numbers` → 漢数字化など) を通しても
    /// stash した URL が壊れないこと。数字を PUA で持つ理由を固定する。
    #[test]
    fn stashed_urls_survive_the_conversion_pipeline() {
        // 既定は enable_convert_num_to_kanji = true。件数は「6143 件を超えても
        // 数字トークンが変換されない」ことの確認に足る範囲に絞る。
        for (settings, count) in [
            (NovelSettings::default(), 7_000usize),
            (
                NovelSettings {
                    enable_convert_num_to_kanji: false,
                    ..NovelSettings::default()
                },
                300,
            ),
        ] {
            let urls: Vec<String> = (0..count)
                .map(|index| format!("https://example.com/novel/{index}/?q={}", index * 2))
                .collect();
            let text = urls.join("\n");
            let mut converter = ConverterBase::new(settings);
            let converted = converter.convert(&text, TextType::Body);
            for url in &urls {
                assert!(converted.contains(url.as_str()), "lost URL {url}");
            }
        }
    }

    #[test]
    fn convert_body_preserves_ascii_url() {
        let settings = NovelSettings::default();
        let mut converter = ConverterBase::new(settings);
        let text = converter.convert("https://example.com/123", TextType::Body);

        assert!(text.contains("https://example.com/123"));
        assert!(!text.contains("ｈｔｔｐｓ"));
    }
}
