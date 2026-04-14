use regex::Regex;

use super::ConverterBase;

const STASH_INDEX_BASE: u32 = 0xE100;
const ILLUST_STASH_MARKER: char = '\u{E010}';
const URL_STASH_MARKER: char = '\u{E011}';

impl ConverterBase {
    pub(super) fn replace_illust_tag(&mut self, text: &mut String) {
        let re = Regex::new(
            r"[ 　\t]*?(\u{FF3B}\u{FF03}\u{633F}\u{7D75}\u{FF08}.+?\u{FF09}\u{5165}\u{308B}\u{FF3D})\n?",
        )
        .unwrap();
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
        let re = Regex::new(r#"https?://[^\s<>"']+"#).unwrap();
        let result = re
            .replace_all(text, |caps: &regex::Captures| {
                let url = caps[0].to_string();
                let idx = self.url_stash.len();
                self.url_stash.push(url);
                encode_stash_token(URL_STASH_MARKER, idx)
            })
            .to_string();
        result
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

    pub(super) fn rebuild_illust(&self, data: &mut String) {
        let re = Regex::new(r"\u{E010}([\u{E100}-\u{F8FF}])").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx = decode_stash_index(&caps[1]).unwrap_or(usize::MAX);
                self.illust_stash.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    pub(super) fn rebuild_url(&self, data: &mut String) {
        let re = Regex::new(r"\u{E011}([\u{E100}-\u{F8FF}])").unwrap();
        *data = re
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
    let encoded = char::from_u32(STASH_INDEX_BASE + idx as u32).expect("stash index overflow");
    format!("{}{}", marker, encoded)
}

fn decode_stash_index(encoded: &str) -> Option<usize> {
    let ch = encoded.chars().next()? as u32;
    ch.checked_sub(STASH_INDEX_BASE).map(|idx| idx as usize)
}

#[cfg(test)]
mod tests {
    use super::{ConverterBase, ILLUST_STASH_MARKER, decode_stash_index, encode_stash_token};
    use crate::converter::{converter_base::TextType, settings::NovelSettings};

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
        let mut settings = NovelSettings::default();
        settings.enable_illust = false;
        let mut converter = ConverterBase::new(settings);
        let mut text = "前\n［＃挿絵（挿絵/test.jpg）入る］\n後".to_string();

        converter.replace_illust_tag(&mut text);

        assert!(!text.contains("［＃挿絵（"));
    }

    #[test]
    fn stash_tokens_roundtrip_private_use_index() {
        let token = encode_stash_token(ILLUST_STASH_MARKER, 12);
        let encoded = token.chars().nth(1).unwrap().to_string();

        assert_eq!(decode_stash_index(&encoded), Some(12));
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
