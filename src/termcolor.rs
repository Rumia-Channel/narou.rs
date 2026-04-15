/// Terminal color helpers matching Ruby's TermColorLight behavior.
///
/// In web mode (NAROU_RS_WEB_MODE=1), produces HTML `<span>` tags.
/// In CLI mode, returns plain text (indicatif handles CLI colors separately).
use crate::progress::is_web_mode;

/// Color name → (normal CSS, bold/bright CSS)
fn color_css(name: &str) -> (&str, &str) {
    match name {
        "black" => ("black", "#888"),
        "red" => ("indianred", "red"),
        "green" => ("green", "lime"),
        "yellow" => ("goldenrod", "yellow"),
        "blue" => ("#33c", "#33f"),
        "magenta" => ("darkmagenta", "magenta"),
        "cyan" => ("darkcyan", "cyan"),
        "white" => ("#bbb", "white"),
        _ => ("inherit", "inherit"),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Wrap text in color (normal intensity).
/// Web mode: `<span style="color:darkcyan">text</span>`
/// CLI mode: plain text
pub fn colored(text: &str, color: &str) -> String {
    if is_web_mode() {
        let (normal, _) = color_css(color);
        format!(
            "<span style=\"color:{}\">{}</span>",
            normal,
            html_escape(text)
        )
    } else {
        text.to_string()
    }
}

/// Wrap text in bold + bright color.
/// Web mode: `<span style="font-weight:bold;color:lime">text</span>`
/// CLI mode: plain text
pub fn bold_colored(text: &str, color: &str) -> String {
    if is_web_mode() {
        let (_, bright) = color_css(color);
        format!(
            "<span style=\"font-weight:bold;color:{}\">{}</span>",
            bright,
            html_escape(text)
        )
    } else {
        text.to_string()
    }
}
