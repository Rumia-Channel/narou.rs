use regex::Regex;

pub fn to_aozora(html: &str) -> String {
    to_aozora_with_options(html, false)
}

pub fn to_aozora_strip_decoration(html: &str) -> String {
    to_aozora_with_options(html, true)
}

fn to_aozora_with_options(html: &str, strip_decoration_tag: bool) -> String {
    let mut text = html.to_string();
    text = br_to_aozora(&text);
    text = p_to_aozora(&text);
    text = ruby_to_aozora(&text);
    if !strip_decoration_tag {
        text = b_to_aozora(&text);
        text = i_to_aozora(&text);
        text = s_to_aozora(&text);
    }
    text = img_to_aozora(&text);
    text = em_to_sesame(&text);
    text = delete_all_tags(&text);
    text = restore_entities(&text);
    text
}

fn br_to_aozora(text: &str) -> String {
    let re = Regex::new(r"[\r\n]+").unwrap();
    let text = re.replace_all(text, "").to_string();
    let re = Regex::new(r"(?i)<br\s*/?>").unwrap();
    re.replace_all(&text, "\n").to_string()
}

fn p_to_aozora(text: &str) -> String {
    let re = Regex::new(r"(?i)\n?</p>").unwrap();
    re.replace_all(text, "\n").to_string()
}

fn ruby_to_aozora(text: &str) -> String {
    let mut result = text.to_string();
    result = result.replace('《', "\u{226A}").replace('》', "\u{226B}");

    let re = Regex::new(r"(?i)<ruby>(.+?)</ruby>").unwrap();
    let rt_re = Regex::new(r"(?i)<rt>").unwrap();
    let rp_re = Regex::new(r"(?i)<rp>").unwrap();
    result = re
        .replace_all(&result, |caps: &regex::Captures| {
            let inner = &caps[1];
            let parts: Vec<&str> = rt_re.splitn(inner, 2).collect();

            if parts.len() < 2 {
                return strip_tags(parts[0]);
            }

            let base = strip_tags(rp_re.split(parts[0]).next().unwrap_or(parts[0]));
            let ruby = strip_tags(rp_re.split(parts[1]).next().unwrap_or(parts[1]));

            format!("｜{}《{}》", base, ruby)
        })
        .to_string();

    result
}

fn b_to_aozora(text: &str) -> String {
    let text = Regex::new(r"(?i)<b(?:\s[^>]*)?>")
        .unwrap()
        .replace_all(text, "\u{FF3B}\u{FF03}\u{592A}\u{5B57}\u{FF3D}")
        .to_string();
    Regex::new(r"(?i)</b>")
        .unwrap()
        .replace_all(
            &text,
            "\u{FF3B}\u{FF03}\u{592A}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
        )
        .to_string()
}

fn i_to_aozora(text: &str) -> String {
    let text = Regex::new(r"(?i)<i(?:\s[^>]*)?>")
        .unwrap()
        .replace_all(text, "\u{FF3B}\u{FF03}\u{659C}\u{4F53}\u{FF3D}")
        .to_string();
    Regex::new(r"(?i)</i>")
        .unwrap()
        .replace_all(
            &text,
            "\u{FF3B}\u{FF03}\u{659C}\u{4F53}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
        )
        .to_string()
}

fn s_to_aozora(text: &str) -> String {
    let text = Regex::new(r"(?i)<s(?:\s[^>]*)?>")
        .unwrap()
        .replace_all(text, "\u{FF3B}\u{FF03}\u{53D6}\u{6D88}\u{7DDA}\u{FF3D}")
        .to_string();
    Regex::new(r"(?i)</s>")
        .unwrap()
        .replace_all(
            &text,
            "\u{FF3B}\u{FF03}\u{53D6}\u{6D88}\u{7DDA}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
        )
        .to_string()
}

fn img_to_aozora(text: &str) -> String {
    let re = Regex::new(r#"(?i)<img[^>]+src=["']([^"']+)["'][^>]*>"#).unwrap();
    re.replace_all(
        text,
        "\u{FF3B}\u{FF03}\u{633F}\u{7D75}\u{FF08}$1\u{FF09}\u{5165}\u{308B}\u{FF3D}",
    )
    .to_string()
}

fn em_to_sesame(text: &str) -> String {
    let re = Regex::new(r#"(?i)<em\s+class=["']emphasisDots["']\s*>(.+?)</em>"#).unwrap();
    let text = re.replace_all(text, "\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{FF3D}$1\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{7D42}\u{308F}\u{308A}\u{FF3D}").to_string();

    let re2 = Regex::new(r"(?i)<em[^>]*>(.+?)</em>").unwrap();
    re2.replace_all(&text, "\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{FF3D}$1\u{FF3B}\u{FF03}\u{508D}\u{70B9}\u{7D42}\u{308F}\u{308A}\u{FF3D}")
        .to_string()
}

fn delete_all_tags(text: &str) -> String {
    let mut result = text.to_string();
    let re = Regex::new(r"<[^>]+>").unwrap();
    while re.is_match(&result) {
        result = re.replace_all(&result, "").to_string();
    }
    result
}

fn strip_tags(text: &str) -> String {
    let re = Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(text, "").to_string()
}

fn restore_entities(text: &str) -> String {
    let mut result = text.to_string();
    let entities: &[(&str, &str)] = &[
        ("&quot;", "\""),
        ("&amp;", "&"),
        ("&nbsp;", "\u{00A0}"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&copy;", "(c)"),
        ("&#39;", "'"),
    ];

    for (entity, replacement) in entities {
        result = result.replace(entity, replacement);
    }

    result
}

pub fn sanitize_text(text: &str) -> String {
    let mut result = text.to_string();

    let script_re = Regex::new(r"(?si)<script[^>]*>.*?</script>").unwrap();
    result = script_re.replace_all(&result, "").to_string();

    let style_re = Regex::new(r"(?si)<style[^>]*>.*?</style>").unwrap();
    result = style_re.replace_all(&result, "").to_string();

    let comment_re = Regex::new(r"<!--.*?-->").unwrap();
    result = comment_re.replace_all(&result, "").to_string();

    result = delete_all_tags(&result);

    result = result.replace("&nbsp;", " ").replace("&#160;", " ");

    result = restore_entities(&result);

    let ws_re = Regex::new(r"\s+").unwrap();
    result = ws_re.replace_all(&result.trim(), " ").to_string();

    result
}

#[cfg(test)]
mod tests {
    use super::to_aozora;

    #[test]
    fn to_aozora_keeps_img_as_illustration_not_italic() {
        let html = r#"<p>前</p><p><a href="//29644.mitemin.net/i422674/" target="_blank"><img src="挿絵/16-0.jpg" alt="挿絵(By みてみん)" border="0" /></a></p><p>後</p>"#;

        let text = to_aozora(html);

        assert!(text.contains("［＃挿絵（挿絵/16-0.jpg）入る］"));
        assert!(!text.contains("［＃斜体］"));
    }
}
