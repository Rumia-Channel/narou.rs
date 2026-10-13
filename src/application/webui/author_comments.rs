//! `GET /api/novels/{id}/author_comments` の応答組み立て (native
//! `src/web/novels.rs` / Worker `webui/novels.rs` で共有)。
//!
//! TOC とセクションの読み取り (オブジェクトストア) は各ランタイムの
//! アダプタに残し、まえがき/あとがきの集計・比率・JSON 形だけをここに置く。

use serde_json::Value;

use crate::downloader::types::{SectionFile, TocObject};

/// TOC と読み込めたセクションから応答本体を組み立てる。
///
/// `sections` は `toc.subtitles` と同じ並びで、読み取れなかった / YAML と
/// して解釈できなかった要素は `None` (どちらのランタイムもそのセクションを
/// 結果から飛ばすが、比率の分母は `toc.subtitles` の全件数)。
pub fn author_comments_payload(
    title: &str,
    toc: &TocObject,
    sections: &[Option<SectionFile>],
) -> Value {
    let mut comments = Vec::new();
    let mut introductions_count: usize = 0;
    let mut postscripts_count: usize = 0;

    for (sub, section) in toc.subtitles.iter().zip(sections) {
        let Some(section) = section else {
            continue;
        };
        let data_type = if section.element.data_type.is_empty() {
            "text"
        } else {
            &section.element.data_type
        };
        let (introduction, postscript) = if data_type == "html" {
            (
                crate::downloader::html::to_aozora_strip_decoration(&section.element.introduction),
                crate::downloader::html::to_aozora_strip_decoration(&section.element.postscript),
            )
        } else {
            (
                section.element.introduction.clone(),
                section.element.postscript.clone(),
            )
        };

        if !introduction.is_empty() {
            introductions_count += 1;
        }
        if !postscript.is_empty() {
            postscripts_count += 1;
        }

        comments.push(serde_json::json!({
            "subtitle": sub.subtitle,
            "introduction": introduction,
            "postscript": postscript,
        }));
    }

    let total = toc.subtitles.len() as f64;
    let ratio = |count: usize| {
        if total > 0.0 {
            (count as f64 / total * 100.0 * 100.0).round() / 100.0
        } else {
            0.0
        }
    };

    serde_json::json!({
        "title": title,
        "introductions_ratio": ratio(introductions_count),
        "postscripts_ratio": ratio(postscripts_count),
        "comments": comments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::types::{SectionElement, SubtitleInfo};

    fn subtitle(subtitle: &str, index: &str) -> SubtitleInfo {
        SubtitleInfo {
            index: index.to_string(),
            href: format!("{index}.html"),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: subtitle.to_string(),
            file_subtitle: subtitle.to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
        }
    }

    fn section(index: &str, data_type: &str, introduction: &str, postscript: &str) -> SectionFile {
        SectionFile {
            index: index.to_string(),
            href: format!("{index}.html"),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: index.to_string(),
            file_subtitle: index.to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
            element: SectionElement {
                data_type: data_type.to_string(),
                introduction: introduction.to_string(),
                postscript: postscript.to_string(),
                body: String::new(),
            },
        }
    }

    fn toc(subtitles: Vec<SubtitleInfo>) -> TocObject {
        TocObject {
            title: "toc title".to_string(),
            author: "author".to_string(),
            toc_url: "https://example.com/".to_string(),
            story: None,
            subtitles,
            novel_type: None,
        }
    }

    #[test]
    fn counts_ratios_from_readable_sections_only() {
        let toc = toc(vec![
            subtitle("一", "1"),
            subtitle("二", "2"),
            subtitle("三", "3"),
        ]);
        let sections = vec![
            Some(section("1", "text", "まえがき", "")),
            None,
            Some(section("3", "text", "", "あとがき")),
        ];
        let value = author_comments_payload("novel title", &toc, &sections);
        assert_eq!(value["comments"][0]["subtitle"], "一");
        assert_eq!(value["comments"][1]["subtitle"], "三");
    }

    #[test]
    fn html_sections_strip_decoration_and_empty_type_is_text() {
        let toc = toc(vec![subtitle("一", "1")]);
        let sections = vec![Some(section("1", "html", "&amp;まえがき", "あとがき&lt;"))];
        let value = author_comments_payload("t", &toc, &sections);
        assert_eq!(value["comments"][0]["introduction"], "&まえがき");
        assert_eq!(value["comments"][0]["postscript"], "あとがき<");

        let sections = vec![Some(section("1", "", "まえがき", ""))];
        let value = author_comments_payload("t", &toc, &sections);
        assert_eq!(value["comments"][0]["introduction"], "まえがき");
    }

    #[test]
    fn empty_toc_yields_zero_ratios() {
        let value = author_comments_payload("t", &toc(Vec::new()), &[]);
        assert_eq!(value["introductions_ratio"], 0.0);
        assert_eq!(value["postscripts_ratio"], 0.0);
        assert_eq!(value["comments"].as_array().unwrap().len(), 0);
    }
}
