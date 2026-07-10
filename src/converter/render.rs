use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::db::NovelRecord;
use crate::downloader::TocObject;

use super::device::Device;
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
    record: Option<&NovelRecord>,
    device: Option<Device>,
) -> String {
    let mut output = String::new();

    let title = settings.title_for_output(&toc.title);
    let author = if settings.novel_author.is_empty() {
        &toc.author
    } else {
        &settings.novel_author
    };
    let processed_title = decorate_title(settings, &title, record);

    output.push_str(&processed_title);
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
            if device == Some(Device::Ibooks) {
                output.push_str("\n\n\n\n\n\n");
            }
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

fn decorate_title(settings: &NovelSettings, title: &str, record: Option<&NovelRecord>) -> String {
    let mut processed_title = add_date_to_title(settings, title, record);
    if settings.enable_add_end_to_title
        && record.is_some_and(|record| record.tags.iter().any(|tag| tag == "end"))
    {
        processed_title.push_str(" (完結)");
    }
    processed_title
        .replace("《", "※［＃始め二重山括弧］")
        .replace("》", "※［＃終わり二重山括弧］")
}

fn add_date_to_title(
    settings: &NovelSettings,
    title: &str,
    record: Option<&NovelRecord>,
) -> String {
    if !settings.enable_add_date_to_title {
        return title.to_string();
    }

    let title_time = title_date_target_time(settings, record);
    let mut date_str = title_time.format(&settings.title_date_format).to_string();
    let dollar_t_included = date_str.contains("$t");
    let replacements = [
        ("$s", calc_reverse_short_time(title_time.timestamp())),
        (
            "$ns",
            record
                .map(|record| record.sitename.clone())
                .unwrap_or_default(),
        ),
        ("$ntag", tags_join_comma(record)),
        (
            "$nt",
            novel_type_text(record.map(|record| record.novel_type)),
        ),
        ("$t", title.to_string()),
    ];
    for (symbol, replacement) in replacements {
        date_str = date_str.replace(symbol, &replacement);
    }

    if dollar_t_included {
        date_str
    } else if settings.title_date_align == "left" {
        format!("{date_str}{title}")
    } else {
        format!("{title}{date_str}")
    }
}

fn title_date_target_time(
    settings: &NovelSettings,
    record: Option<&NovelRecord>,
) -> chrono::DateTime<chrono::FixedOffset> {
    let selected = match settings.title_date_target.as_str() {
        "general_lastup" => record.and_then(|record| record.general_lastup),
        "last_update" => record.map(|record| record.last_update),
        "new_arrivals_date" => record.and_then(|record| record.new_arrivals_date),
        "convert" => None,
        _ => None,
    };
    selected
        .map(|time| time.with_timezone(&jst_offset()))
        .unwrap_or_else(|| chrono::Local::now().with_timezone(&jst_offset()))
}

fn calc_reverse_short_time(timestamp: i64) -> String {
    let value = (2_091_149_000_i64 - timestamp).div_euclid(10 * 60);
    let encoded = to_base36(value);
    if encoded.len() < 4 {
        format!("{}{}", "0".repeat(4 - encoded.len()), encoded)
    } else {
        encoded
    }
}

fn to_base36(value: i64) -> String {
    let negative = value < 0;
    let mut remaining = if negative {
        -(value as i128)
    } else {
        value as i128
    };
    let mut digits = Vec::new();
    loop {
        let digit = (remaining % 36) as u8;
        digits.push(match digit {
            0..=9 => (b'0' + digit) as char,
            _ => (b'a' + (digit - 10)) as char,
        });
        remaining /= 36;
        if remaining == 0 {
            break;
        }
    }
    if negative {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

fn jst_offset() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(9 * 3600).expect("valid JST offset")
}

fn tags_join_comma(record: Option<&NovelRecord>) -> String {
    let mut tags = record.map(|record| record.tags.clone()).unwrap_or_default();
    tags.sort();
    tags.join(",")
}

fn novel_type_text(novel_type: Option<u8>) -> String {
    if novel_type == Some(2) {
        "短編".to_string()
    } else {
        "連載".to_string()
    }
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

pub(crate) fn looks_like_html(text: &str) -> bool {
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
    use chrono::{TimeZone, Utc};

    use super::{
        ConvertedSection, decorate_title, normalize_story_markup, normalize_subtitle_markup,
        render_novel_text,
    };
    use crate::converter::device::Device;
    use crate::db::NovelRecord;
    use crate::{converter::settings::NovelSettings, downloader::TocObject};

    fn sample_record() -> NovelRecord {
        NovelRecord {
            id: 1,
            author: "作者".to_string(),
            title: "作品".to_string(),
            file_title: "file".to_string(),
            toc_url: "https://example.com/novel/".to_string(),
            sitename: "Example".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: Some(Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap()),
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn rendered_title_uses_title_date_format_with_title_placeholder() {
        let mut settings = NovelSettings::default();
        settings.enable_add_date_to_title = true;
        settings.title_date_format = "$t (%F) $ns".to_string();
        settings.title_date_target = "general_lastup".to_string();
        let toc = TocObject {
            title: "作品".to_string(),
            author: "作者".to_string(),
            toc_url: String::new(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(1),
        };
        let text = render_novel_text(&settings, &toc, "", &[], Some(&sample_record()), None);

        assert!(
            text.starts_with("作品 (2026-05-16) Example\n作者\n"),
            "{text}"
        );
    }

    #[test]
    fn decorate_title_applies_left_align_end_marker_and_ruby_escape() {
        let mut settings = NovelSettings::default();
        settings.enable_add_date_to_title = true;
        settings.title_date_format = "%F ".to_string();
        settings.title_date_align = "left".to_string();
        settings.enable_add_end_to_title = true;
        let mut record = sample_record();
        record.tags = vec!["end".to_string()];

        assert_eq!(
            decorate_title(&settings, "作品《仮》", Some(&record)),
            "2026-05-16 作品※［＃始め二重山括弧］仮※［＃終わり二重山括弧］ (完結)"
        );
    }

    #[test]
    fn render_inserts_ibooks_chapter_spacing_like_ruby_template() {
        let settings = NovelSettings::default();
        let toc = TocObject {
            title: "作品".to_string(),
            author: "作者".to_string(),
            toc_url: String::new(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(1),
        };
        let section = ConvertedSection {
            chapter: "第一章".to_string(),
            subchapter: String::new(),
            subtitle: "第一話".to_string(),
            introduction: String::new(),
            body: "本文".to_string(),
            postscript: String::new(),
        };

        let text = render_novel_text(&settings, &toc, "", &[section], None, Some(Device::Ibooks));

        assert!(
            text.contains("［＃ここで柱終わり］\n\n\n\n\n\n\n［＃３字下げ］"),
            "{text}"
        );
    }

    #[test]
    fn decorate_title_replaces_all_narou_rb_extended_format_symbols() {
        let mut settings = NovelSettings::default();
        settings.enable_add_date_to_title = true;
        settings.title_date_format = "$t $s $ns $ntag $nt".to_string();
        settings.title_date_target = "general_lastup".to_string();
        let mut record = sample_record();
        record.sitename = "Site".to_string();
        record.novel_type = 2;
        record.tags = vec!["end".to_string(), "alpha".to_string()];
        record.general_lastup = Some(Utc.timestamp_opt(2_091_149_000, 0).unwrap());

        assert_eq!(
            decorate_title(&settings, "作品", Some(&record)),
            "作品 0000 Site alpha,end 短編"
        );
    }

    #[test]
    fn rendered_title_strips_prefix_when_enabled() {
        let mut settings = NovelSettings::default();
        settings.enable_strip_title_prefix = true;
        let toc = TocObject {
            title: "《コミカライズ企画進行中》マジカル".to_string(),
            author: "作者".to_string(),
            toc_url: "https://example.com/works/1".to_string(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(1),
        };

        let text = render_novel_text(&settings, &toc, "", &[], None, None);

        assert!(text.starts_with("マジカル\n作者\n"), "{text}");
    }

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
