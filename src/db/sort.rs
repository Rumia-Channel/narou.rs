use std::cmp::Ordering;

use super::novel_record::NovelRecord;

pub fn compare_records_by_key(a: &NovelRecord, b: &NovelRecord, key: &str) -> Ordering {
    match key {
        "id" => a.id.cmp(&b.id),
        "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        "author" => a.author.to_lowercase().cmp(&b.author.to_lowercase()),
        "last_update" => a.last_update.cmp(&b.last_update),
        "general_lastup" => compare_optional(a.general_lastup, b.general_lastup),
        "sitename" => a.sitename.cmp(&b.sitename),
        "novel_type" => a.novel_type.cmp(&b.novel_type),
        "length" => compare_optional(a.length, b.length),
        "last_check_date" => compare_optional(a.last_check_date, b.last_check_date),
        "tags" => record_tags_key(a).cmp(&record_tags_key(b)),
        "general_all_no" => compare_optional(a.general_all_no, b.general_all_no),
        "status" => record_status_key(a).cmp(&record_status_key(b)),
        "toc_url" => a.toc_url.cmp(&b.toc_url),
        "new_arrivals_date" => a.new_arrivals_date.cmp(&b.new_arrivals_date),
        _ => a.id.cmp(&b.id),
    }
}

pub const SORT_KEYS: &[&str] = &[
    "id",
    "last_update",
    "general_lastup",
    "last_check_date",
    "title",
    "author",
    "sitename",
    "novel_type",
    "tags",
    "general_all_no",
    "length",
    "status",
    "toc_url",
    "new_arrivals_date",
];

pub fn sort_keys() -> &'static [&'static str] {
    SORT_KEYS
}

pub fn sort_key_valid(key: &str) -> bool {
    SORT_KEYS.contains(&key)
}

fn compare_optional<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn record_tags_key(record: &NovelRecord) -> String {
    record
        .tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>()
        .join("\u{0}")
}

fn record_status_key(record: &NovelRecord) -> String {
    let mut status = Vec::new();
    if record.tags.iter().any(|tag| tag == "end") || record.end {
        status.push("完結");
    }
    if record.tags.iter().any(|tag| tag == "404") {
        status.push("削除");
    }
    if record.suspend {
        status.push("中断");
    }
    status.join(", ").to_lowercase()
}
