use regex::Regex;

use super::ConverterBase;

impl ConverterBase {
    pub(super) fn replace_illust_tag(&mut self, text: &mut String) {
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

    pub(super) fn replace_url(&mut self, text: &str) -> String {
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
        let re = Regex::new(r"\u{FF3B}\u{FF03}ILUST=(\d+)\u{FF3D}").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                self.illust_stash.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    pub(super) fn rebuild_url(&self, data: &mut String) {
        let re = Regex::new(r"\u{FF3B}\u{FF03}URL=(\d+)\u{FF3D}").unwrap();
        *data = re
            .replace_all(data, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
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
