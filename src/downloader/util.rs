use super::preprocess;
use super::site_setting::SiteSetting;

pub fn build_section_url(setting: &SiteSetting, toc_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", setting.top_url(), href)
    } else if href.is_empty() {
        toc_url.to_string()
    } else {
        format!("{}/{}", toc_url.trim_end_matches('/'), href)
    }
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

pub fn sanitize_filename(name: &str) -> String {
    let invalid = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    name.chars()
        .map(|c| if invalid.contains(&c) { '_' } else { c })
        .collect::<String>()
        .chars()
        .take(80)
        .collect::<String>()
        .trim_end_matches([' ', '.'])
        .to_string()
}
