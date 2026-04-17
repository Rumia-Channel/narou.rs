use super::preprocess;
use super::site_setting::SiteSetting;

pub fn build_section_url(setting: &SiteSetting, toc_url: &str, href: &str) -> String {
    let href = decode_html_href(href);
    if href.starts_with("http://") || href.starts_with("https://") {
        href
    } else if href.starts_with('/') {
        format!("{}{}", setting.top_url(), href)
    } else if href.is_empty() {
        toc_url.to_string()
    } else {
        format!("{}/{}", toc_url.trim_end_matches('/'), href)
    }
}

fn decode_html_href(href: &str) -> String {
    let mut decoded = href
        .split('#')
        .next()
        .unwrap_or("")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    decode_numeric_entities(&mut decoded);
    decoded
}

pub fn compile_html_pattern(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .size_limit(10_000_000)
        .build()
}

pub fn pretreatment_source(src: &mut String, _encoding: &str, setting: Option<&SiteSetting>) {
    src.retain(|c| c != '\r');
    decode_numeric_entities(src);
    if let Some(setting) = setting {
        if let Some(pipeline) = setting.preprocess_pipeline() {
            preprocess::run_preprocess(pipeline, src);
        }
    }
}

pub fn decode_numeric_entities(src: &mut String) {
    let hex_re = regex::Regex::new(r"&#x([0-9a-fA-F]+);").unwrap();
    let dec_re = regex::Regex::new(r"&#(\d+);").unwrap();

    *src = hex_re
        .replace_all(src, |caps: &regex::Captures| {
            let code = u32::from_str_radix(&caps[1], 16).unwrap_or(0xFFFD);
            char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
        })
        .to_string();

    *src = dec_re
        .replace_all(src, |caps: &regex::Captures| {
            let code: u32 = caps[1].parse().unwrap_or(0xFFFD);
            char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
        })
        .to_string();
}

pub fn load_length_limit(key: &str, default: Option<usize>) -> Option<usize> {
    crate::compat::load_local_setting_value(key)
        .and_then(|value| match value {
            serde_yaml::Value::Number(number) => number.as_i64(),
            serde_yaml::Value::String(raw) => raw.parse::<i64>().ok(),
            _ => None,
        })
        .map(|limit| limit.max(0) as usize)
        .or(default)
}

pub fn sanitize_filename_with_limit(name: &str, limit: Option<usize>) -> String {
    let invalid = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let sanitized = name
        .chars()
        .map(|c| if invalid.contains(&c) { '_' } else { c })
        .collect::<String>();
    let truncated = match limit {
        Some(limit) => sanitized.chars().take(limit).collect::<String>(),
        None => sanitized,
    };
    truncated.trim_end_matches([' ', '.']).to_string()
}

pub fn sanitize_filename(name: &str) -> String {
    sanitize_filename_with_limit(name, Some(80))
}

pub fn mask_spoiler_text(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '0'..='9' | '０'..='９' | ' ' | '　' | '、' | '。' | '!' | '?' | '！' | '？' => ch,
            _ => '●',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build_section_url, mask_spoiler_text, sanitize_filename_with_limit};
    use crate::downloader::site_setting::SiteSetting;

    #[test]
    fn build_section_url_decodes_html_entities_in_href() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings.iter().find(|s| s.name == "Arcadia").unwrap();

        let url = build_section_url(
            setting,
            "http://www.mai-net.net/bbs/sst/sst.php?act=dump&cate=all&all=6858&n=0&count=1",
            "/bbs/sst/sst.php?act=dump&amp;cate=all&amp;all=6858&amp;n=0#kiji",
        );

        assert_eq!(
            url,
            "http://www.mai-net.net/bbs/sst/sst.php?act=dump&cate=all&all=6858&n=0"
        );
    }

    #[test]
    fn sanitize_filename_with_limit_truncates_after_sanitizing() {
        assert_eq!(sanitize_filename_with_limit("ab/cd", Some(4)), "ab_c");
    }

    #[test]
    fn mask_spoiler_text_preserves_digits_and_punctuation() {
        assert_eq!(mask_spoiler_text("第12話!? テスト"), "●12●!? ●●●");
    }
}
