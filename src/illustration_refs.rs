//! Image references shared by conversion, virtual books, and EPUB preparation.

/// Collect unique Aozora and HTML image references, including unresolved URLs.
pub fn collect(text: &str) -> Vec<String> {
    let mut references = Vec::new();
    for caps in note_pattern().captures_iter(text) {
        push_reference(&mut references, &caps[1]);
    }
    for caps in image_pattern().captures_iter(text) {
        push_reference(&mut references, &caps[2]);
    }
    references
}

fn note_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"［＃挿絵（([^、）]+)(?:、[^）]*)?）入る］")
            .expect("illustration note pattern is valid")
    });
    &PATTERN
}

fn image_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)(<img[^>]+src=["'])([^"']+)(["'])"#)
            .expect("illustration img pattern is valid")
    });
    &PATTERN
}

fn push_reference(references: &mut Vec<String>, reference: &str) {
    let reference = reference.trim();
    if !reference.is_empty() && !references.iter().any(|known| known == reference) {
        references.push(reference.to_string());
    }
}

/// Resolve image names while retaining captions and surrounding markup.
/// Unresolved references remain unchanged and are returned once each.
/// Taking ownership preserves the original allocation when nothing changes.
pub fn rewrite(
    text: String,
    mut resolve: impl FnMut(&str) -> Option<String>,
) -> (String, Vec<String>) {
    let mut unresolved = Vec::new();
    let text = rewrite_matches(text, note_pattern(), 1, &mut resolve, &mut unresolved);
    let text = rewrite_matches(text, image_pattern(), 2, &mut resolve, &mut unresolved);
    (text, unresolved)
}

fn rewrite_matches(
    text: String,
    pattern: &regex::Regex,
    group: usize,
    resolve: &mut impl FnMut(&str) -> Option<String>,
    unresolved: &mut Vec<String>,
) -> String {
    let mut output = None;
    let mut last = 0;
    for captures in pattern.captures_iter(&text) {
        let value = captures.get(group).expect("image reference group");
        let reference = value.as_str().trim();
        let Some(localized) = resolve(reference) else {
            push_reference(unresolved, reference);
            continue;
        };
        if localized == value.as_str() {
            continue;
        }
        let output = output.get_or_insert_with(|| String::with_capacity(text.len()));
        output.push_str(&text[last..value.start()]);
        output.push_str(&localized);
        last = value.end();
    }
    match output {
        Some(mut output) => {
            output.push_str(&text[last..]);
            output
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE: &str = "題名\n作者";

    #[test]
    fn rewrites_illustration_references_to_stored_names() {
        // Worker の本文は取得元 URL のままなので、保存済みの名前へ寄せる。
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（https://example.invalid/a.png）入る］\n");
        let (rewritten, unresolved) = rewrite(text, |reference| {
            (reference == "https://example.invalid/a.png").then(|| "挿絵/0001.png".to_string())
        });
        assert!(
            rewritten.contains("［＃挿絵（挿絵/0001.png）入る］"),
            "{rewritten}"
        );
        assert!(!rewritten.contains("example.invalid"), "{rewritten}");
        assert!(unresolved.is_empty());
    }

    #[test]
    fn keeps_unresolved_references_and_reports_each_once() {
        let text = format!(
            "{SAMPLE}\n［＃挿絵（https://example.invalid/a.png）入る］\n\
             ［＃挿絵（https://example.invalid/a.png）入る］\n［＃挿絵（挿絵/local.png）入る］\n"
        );
        let (rewritten, unresolved) = rewrite(text.clone(), |_| None);
        assert_eq!(rewritten, text, "解決できない参照は書き換えない");
        assert_eq!(
            unresolved,
            vec![
                "https://example.invalid/a.png".to_string(),
                "挿絵/local.png".to_string()
            ]
        );
    }

    #[test]
    fn rewrites_raw_img_sources_too() {
        let text = format!("{SAMPLE}\n<img src=\"https://example.invalid/b.jpg\" alt=\"x\">\n");
        let (rewritten, unresolved) =
            rewrite(text, |_| Some("挿絵/b.jpg".to_string()));
        assert!(rewritten.contains(r#"src="挿絵/b.jpg""#), "{rewritten}");
        assert!(unresolved.is_empty());
    }
}
