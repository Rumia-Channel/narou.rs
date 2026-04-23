use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::downloader::TocObject;

use super::settings::NovelSettings;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConvertedSection {
    pub chapter: String,
    pub subchapter: String,
    pub subtitle: String,
    pub introduction: String,
    pub body: String,
    pub postscript: String,
}

pub(crate) fn render_novel_text(
    settings: &NovelSettings,
    toc: &TocObject,
    story: &str,
    sections: &[ConvertedSection],
) -> String {
    let mut output = String::new();

    let title = if settings.novel_title.is_empty() {
        &toc.title
    } else {
        &settings.novel_title
    };
    let author = if settings.novel_author.is_empty() {
        &toc.author
    } else {
        &settings.novel_author
    };

    output.push_str(title);
    output.push('\n');
    output.push_str(author);
    output.push('\n');

    let cover_chuki = create_cover_chuki(settings);
    output.push_str(&cover_chuki);
    output.push('\n');

    output.push_str("\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n");

    if !story.is_empty() {
        output.push_str("あらすじ：\n");
        output.push_str(&normalize_story_markup(story));
        if !story.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
    }

    if !toc.toc_url.is_empty() {
        output.push_str("掲載ページ:\n");
        output.push_str(&format!(
            "<a href=\"{}\">{}</a>\n",
            toc.toc_url, toc.toc_url
        ));
        output.push_str("\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n");
    }

    output.push('\n');

    for section in sections {
        output.push_str("\u{FF3B}\u{FF03}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{FF3D}\n");

        if !section.chapter.is_empty() {
            output.push_str("\u{FF3B}\u{FF03}\u{30DA}\u{30FC}\u{30B8}\u{306E}\u{5DE6}\u{53F3}\u{4E2D}\u{592E}\u{FF3D}\n");
            output.push_str(&format!(
                "\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{67F1}\u{FF3D}{}\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{67F1}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                title
            ));
            output.push_str(&format!(
                "\u{FF3B}\u{FF03}\u{FF13}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\u{FF3B}\u{FF03}\u{5927}\u{898B}\u{51FA}\u{3057}\u{FF3D}{}\u{FF3B}\u{FF03}\u{5927}\u{898B}\u{51FA}\u{3057}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                section.chapter
            ));
            output.push_str("\u{FF3B}\u{FF03}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{FF3D}\n");
        }

        if !section.subchapter.is_empty() {
            let trimmed_subchapter = section.subchapter.trim_end();
            output.push_str(&format!(
                "\u{FF3B}\u{FF03}\u{FF11}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\u{FF3B}\u{FF03}\u{FF11}\u{6BB5}\u{968E}\u{5927}\u{304D}\u{306A}\u{6587}\u{5B57}\u{FF3D}{}\u{FF3B}\u{FF03}\u{5927}\u{304D}\u{306A}\u{6587}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                trimmed_subchapter
            ));
        }

        output.push('\n');

        let indent = if settings.enable_yokogaki {
            "\u{FF3B}\u{FF03}\u{FF11}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}"
        } else {
            "\u{FF3B}\u{FF03}\u{FF13}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}"
        };
        let trimmed_subtitle = normalize_subtitle_markup(section.subtitle.trim_end());
        output.push_str(&format!(
            "{}［＃中見出し］{}［＃中見出し終わり］\n",
            indent, trimmed_subtitle
        ));

        output.push_str("\n\n");

        let trimmed_intro = section.introduction.trim_end_matches('\n');
        let trimmed_body = section.body.trim_end_matches('\n');
        let trimmed_post = trim_author_comment_text(&section.postscript);

        if !section.introduction.is_empty() {
            let style = &settings.author_comment_style;
            if style == "simple" {
                output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF18}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\n");
                output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF12}\u{6BB5}\u{968E}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{FF3D}\n");
                output.push_str(trimmed_intro);
                output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
                output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5B57}\u{4E0B}\u{3052}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
            } else if style == "plain" {
                output.push_str("\n\n");
                output.push_str(trimmed_intro);
                output.push_str("\n\n\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n\n");
            } else {
                output.push_str(&format!(
                    "\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{524D}\u{66F8}\u{304D}\u{FF3D}\n{}\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{524D}\u{66F8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                    trimmed_intro
                ));
            }
        }

        if !section.introduction.is_empty() {
            output.push_str("\n\n");
        }

        output.push_str(trimmed_body);

        if !section.postscript.is_empty() {
            let style = &settings.author_comment_style;
            if style == "simple" {
                output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF18}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\n");
                output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF12}\u{6BB5}\u{968E}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{FF3D}\n");
                output.push_str(&trimmed_post);
                output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
                output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5B57}\u{4E0B}\u{3052}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
            } else if style == "plain" {
                output.push_str("\n\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n\n");
                output.push_str(&trimmed_post);
                output.push_str("\n");
            } else {
                output.push_str(&format!(
                    "\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{5F8C}\u{66F8}\u{304D}\u{FF3D}\n{}\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5F8C}\u{66F8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                    trimmed_post
                ));
            }
        }

        if !output.ends_with('\n') {
            output.push('\n');
        }
    }

    if settings.enable_display_end_of_book {
        output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{5730}\u{4ED8}\u{304D}\u{FF3D}\u{FF3B}\u{FF03}\u{5C0F}\u{66F8}\u{304D}\u{FF3D}\u{FF08}\u{672C}\u{3092}\u{8AAD}\u{307F}\u{7D42}\u{308F}\u{308A}\u{307E}\u{3057}\u{305F}\u{FF09}\u{FF3B}\u{FF03}\u{5C0F}\u{66F8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5730}\u{4ED8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
    }

    output
}

fn create_cover_chuki(settings: &NovelSettings) -> String {
    let archive_path = &settings.archive_path;
    for ext in &[".jpg", ".png", ".jpeg"] {
        let cover_path = archive_path.join(format!("cover{}", ext));
        if cover_path.exists() {
            return format!(
                "\u{FF3B}\u{FF03}\u{633F}\u{7D75}\u{FF08}cover{}\u{FF09}\u{5165}\u{308B}\u{FF3D}",
                ext
            );
        }
    }
    String::new()
}

pub(crate) fn insert_cover_chuki_for_textfile(settings: &NovelSettings, text: &str) -> String {
    let cover_chuki = create_cover_chuki(settings);
    if cover_chuki.is_empty() {
        return text.to_string();
    }

    let parts: Vec<&str> = text.splitn(3, '\n').collect();
    match parts.as_slice() {
        [title, author, rest] => format!("{title}\n{author}\n{cover_chuki}\n{rest}"),
        [title, author] => format!("{title}\n{author}\n{cover_chuki}"),
        [title] => format!("{title}\n\n{cover_chuki}"),
        [] => cover_chuki,
        _ => text.to_string(),
    }
}

pub(crate) fn trim_author_comment_text(text: &str) -> String {
    text.trim_end_matches('\n')
        .lines()
        .map(|line| line.strip_prefix('\u{3000}').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_subtitle_markup(text: &str) -> String {
    let text = text
        .replace("幕間［＃縦中横］１［＃縦中横終わり］", "幕間１")
        .replace("幕間［＃縦中横］２［＃縦中横終わり］", "幕間２")
        .replace("幕間［＃縦中横］３［＃縦中横終わり］", "幕間３")
        .replace("（［＃縦中横］１［＃縦中横終わり］）", "（１）")
        .replace("（［＃縦中横］２［＃縦中横終わり］）", "（２）")
        .replace("（［＃縦中横］３［＃縦中横終わり］）", "（３）");

    let episode_re = Regex::new(r"\A([０-９])話").unwrap();
    let text = episode_re
        .replace(&text, |caps: &regex::Captures| {
            format!("［＃縦中横］{}［＃縦中横終わり］話", &caps[1])
        })
        .to_string();

    let side_re = Regex::new(r"－([０-９])－").unwrap();
    side_re
        .replace_all(&text, |caps: &regex::Captures| {
            format!("－［＃縦中横］{}［＃縦中横終わり］－", &caps[1])
        })
        .to_string()
}

pub(crate) fn normalize_story_source(story: &str) -> String {
    if looks_like_html(story) {
        crate::downloader::html::to_aozora(story)
    } else {
        story.to_string()
    }
}

fn looks_like_html(text: &str) -> bool {
    text.contains("<br")
        || text.contains("<BR")
        || text.contains("</p>")
        || text.contains("</P>")
        || text.contains("<ruby")
        || text.contains("<RUBY")
}

fn normalize_story_markup(text: &str) -> String {
    let re = Regex::new(r"年([０-９])月([０-９])日").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        format!(
            "年{}月{}日",
            segmented_digit(&caps[1]),
            segmented_digit(&caps[2])
        )
    })
    .to_string()
}

fn segmented_digit(text: &str) -> char {
    match text {
        "０" => '\u{1FDF0}',
        "１" => '\u{1FDF1}',
        "２" => '\u{1FDF2}',
        "３" => '\u{1FDF3}',
        "４" => '\u{1FDF4}',
        "５" => '\u{1FDF5}',
        "６" => '\u{1FDF6}',
        "７" => '\u{1FDF7}',
        "８" => '\u{1FDF8}',
        "９" => '\u{1FDF9}',
        _ => text.chars().next().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_story_markup, normalize_subtitle_markup};

    #[test]
    fn subtitle_single_digit_episode_keeps_tcy_markup() {
        assert_eq!(
            normalize_subtitle_markup("１話　　味噌汁"),
            "［＃縦中横］１［＃縦中横終わり］話　　味噌汁"
        );
    }

    #[test]
    fn subtitle_side_number_keeps_tcy_markup() {
        assert_eq!(
            normalize_subtitle_markup("［＃縦中横］11［＃縦中横終わり］話　　後藤愛依梨　－１－"),
            "［＃縦中横］11［＃縦中横終わり］話　　後藤愛依梨　－［＃縦中横］１［＃縦中横終わり］－"
        );
    }

    #[test]
    fn story_single_digit_month_day_become_segmented_digits() {
        assert_eq!(
            normalize_story_markup("２０１８年２月１日に発売します。"),
            "２０１８年🷲月🷱日に発売します。"
        );
    }
}
