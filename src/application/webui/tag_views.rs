//! タグ表示の共有実装: タグ一覧 (`/api/tag_list`) とタグ情報
//! (`/api/taginfo.json`) の並び・HTML・payload。
//!
//! native (`src/web/misc.rs` / `src/web/jobs.rs`) と Worker
//! (`webui/queue.rs` / `webui/read_views.rs`) は同じ HTML と JSON を返す。
//! ここには表示規則だけを置き、一覧の読み取り (全レコード走査 / SQL 集計) は
//! 各ランタイムのアダプタに残す。

use std::collections::{HashMap, HashSet};

use crate::db::NovelRecord;

use super::{html_escape, tag_color_class};

/// タグ一覧の表示順の比較: 出現件数の降順、同数はタグ名の昇順 (バイト比較 —
/// SQLite の `COLLATE BINARY` と同じ)。Worker は SQL の
/// `ORDER BY COUNT(*) DESC, t.tag COLLATE BINARY ASC` で同じ順を読む。
/// この順は新規タグへの色割り当ても左右するため、両ランタイムで一致させる。
pub fn tag_count_ordering(a: &(String, usize), b: &(String, usize)) -> std::cmp::Ordering {
    b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))
}

/// 全レコードのタグ名 (バイト昇順)。タグ色の一括割り当て (`for_tags`) へ
/// 渡す順序を native/Worker で一致させる — 未着色タグに巡回で割り当てる色は
/// この順序に依存する。
pub fn tag_names(records: &[NovelRecord]) -> Vec<String> {
    let mut names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for record in records {
        for tag in &record.tags {
            names.insert(tag.as_str());
        }
    }
    names.into_iter().map(str::to_string).collect()
}

/// タグサイドバーの HTML 断片 (native `tag_list` / Worker `tag_list` と同一)。
/// `tags` は表示順に並んだタグ名、`tag_colors` はタグ色の表 (未知は既定色)。
pub fn tag_list_html(tags: &[String], tag_colors: &HashMap<String, String>) -> String {
    let mut html = String::from(
        "<div><span class=\"tag-label tag-default tag-reset\" data-tag=\"\">タグ検索を解除</span></div>\
<div class=\"text-muted\" style=\"font-size:0.8em\">Altキーを押しながらで除外検索</div>",
    );
    for tag in tags {
        let escaped_tag = html_escape(tag);
        let class = tag_color_class(tag_colors.get(tag).map(String::as_str).unwrap_or("default"));
        html.push_str(&format!(
            "<div><span class=\"tag-label {}\" data-tag=\"{}\">{}</span> \
<span class=\"select-color-button\" data-target-tag=\"{}\"><span class=\"tag-label {} tag-fixed-width\">a</span></span></div>",
            class, escaped_tag, escaped_tag, escaped_tag, class
        ));
    }
    html
}

/// `/api/taginfo.json` の応答本体: タグ名の昇順に
/// `{tag, count, total_count, html}` (+ `with_exclusion` なら `exclusion_html`)。
///
/// `count` は選択 id のうちそのタグを持つものの数。選択 id は集合として扱う
/// (重複 id は 1 回だけ数える — native の並べ直し/重複排除と同じ規則)、
/// `total_count` は選択に関係なく全レコード中の出現数。
pub fn taginfo_entries(
    records: &[NovelRecord],
    selected_ids: &[i64],
    with_exclusion: bool,
    tag_colors: &HashMap<String, String>,
) -> Vec<serde_json::Value> {
    let mut tag_index: HashMap<String, Vec<i64>> = HashMap::new();
    let mut record_tags: HashMap<i64, Vec<String>> = HashMap::new();
    for record in records {
        record_tags.insert(record.id, record.tags.clone());
        for tag in &record.tags {
            tag_index.entry(tag.clone()).or_default().push(record.id);
        }
    }

    let mut selected_counts: HashMap<String, usize> = HashMap::new();
    for id in selected_ids.iter().copied().collect::<HashSet<_>>() {
        if let Some(tags) = record_tags.get(&id) {
            for tag in tags {
                *selected_counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut sorted_tags: Vec<(&String, &Vec<i64>)> = tag_index.iter().collect();
    sorted_tags.sort_by(|a, b| a.0.cmp(b.0));
    let mut tag_info = Vec::with_capacity(sorted_tags.len());
    for (tag, tag_ids) in sorted_tags {
        let color = tag_colors.get(tag).map(|c| c.as_str()).unwrap_or("");
        let class = tag_color_class(color);
        let escaped_tag = html_escape(tag);
        let html = format!("<span class=\"tag-label {}\">{}</span>", class, escaped_tag);
        let mut entry = serde_json::json!({
            "tag": tag,
            "count": selected_counts.get(tag.as_str()).copied().unwrap_or(0),
            "total_count": tag_ids.len(),
            "html": html,
        });
        if with_exclusion {
            entry["exclusion_html"] = serde_json::json!(format!(
                "<span class=\"tag-label {} tag-exclusion\">{}</span>",
                class, escaped_tag
            ));
        }
        tag_info.push(entry);
    }
    tag_info
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn record(id: i64, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: format!("author-{id}"),
            title: format!("title-{id}"),
            file_title: format!("file-{id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn tag_count_order_is_count_desc_then_name_asc() {
        let mut counts = vec![
            ("b".to_string(), 2),
            ("a".to_string(), 2),
            ("c".to_string(), 5),
        ];
        counts.sort_by(tag_count_ordering);
        assert_eq!(
            counts,
            vec![
                ("c".to_string(), 5),
                ("a".to_string(), 2),
                ("b".to_string(), 2)
            ]
        );
    }

    #[test]
    fn tag_list_html_escapes_and_uses_colors() {
        let tags = vec!["<a&b>".to_string()];
        let colors = HashMap::from([("<a&b>".to_string(), "green".to_string())]);
        let html = tag_list_html(&tags, &colors);
        assert!(html.contains("&lt;a&amp;b&gt;"));
        assert!(html.contains("tag-green"));
        assert!(!html.contains("<a&b>"));
    }

    #[test]
    fn tag_names_are_sorted_and_unique() {
        let records = vec![record(1, &["b", "a"]), record(2, &["a", "c"])];
        assert_eq!(
            tag_names(&records),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(tag_names(&[]).is_empty());
    }

    #[test]
    fn taginfo_counts_selected_ids_once() {
        let records = vec![
            record(1, &["fav", "todo"]),
            record(2, &["fav"]),
            record(3, &["other"]),
        ];
        let entries = taginfo_entries(&records, &[1, 1, 2, 3, 99], false, &HashMap::new());
        assert_eq!(
            entries,
            vec![
                serde_json::json!({
                    "tag": "fav",
                    "count": 2,
                    "total_count": 2,
                    "html": "<span class=\"tag-label tag-default\">fav</span>",
                }),
                serde_json::json!({
                    "tag": "other",
                    "count": 1,
                    "total_count": 1,
                    "html": "<span class=\"tag-label tag-default\">other</span>",
                }),
                serde_json::json!({
                    "tag": "todo",
                    "count": 1,
                    "total_count": 1,
                    "html": "<span class=\"tag-label tag-default\">todo</span>",
                }),
            ]
        );
    }

    #[test]
    fn taginfo_with_exclusion_adds_variant_html() {
        let records = vec![record(1, &["fav"])];
        let colors = HashMap::from([("fav".to_string(), "blue".to_string())]);
        let entries = taginfo_entries(&records, &[1], true, &colors);
        assert_eq!(
            entries[0]["exclusion_html"],
            serde_json::json!(
                "<span class=\"tag-label tag-blue tag-exclusion\">fav</span>"
            )
        );
    }
}
