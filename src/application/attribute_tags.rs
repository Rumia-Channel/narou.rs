//! Reserved work-type tags, shared by the downloader and tag-editing APIs.

pub const NOVEL_TAG: &str = "小説";
pub const MANGA_TAG: &str = "漫画";

pub fn is_attribute_tag(tag: &str) -> bool {
    matches!(tag, NOVEL_TAG | MANGA_TAG)
}

pub fn is_visible(tag: &str, show_novel: bool, show_manga: bool) -> bool {
    match tag {
        NOVEL_TAG => show_novel,
        MANGA_TAG => show_manga,
        _ => true,
    }
}

/// Replace the previous classification without touching ordinary tags.
pub fn set_attribute_tag(tags: &mut Vec<String>, tag: &str) {
    debug_assert!(is_attribute_tag(tag));
    if tags.iter().filter(|value| is_attribute_tag(value)).count() == 1
        && tags.iter().any(|value| value == tag)
    {
        return;
    }
    tags.retain(|value| !is_attribute_tag(value));
    tags.push(tag.to_string());
}

/// Infer an existing record's type from persisted metadata. A previous
/// classification must be kept when a rule needs unavailable page content.
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub fn initial_tag(
    record: &crate::db::NovelRecord,
    sites: &[crate::downloader::site_setting::SiteSetting],
) -> &'static str {
    use crate::downloader::site_setting::{WorkType, WorkTypeField, WorkTypeSubjects};
    let site = sites.iter().find(|site| site.matches_url(&record.toc_url))
        .or_else(|| sites.iter().find(|site| record.domain.as_deref() == Some(site.domain.as_str())));
    if site.is_some_and(|site| site.work_type_rules.iter().any(|rule| rule.field == WorkTypeField::Source)) {
        let mut attributes = record.tags.iter().filter(|tag| is_attribute_tag(tag));
        if let (Some(tag), None) = (attributes.next(), attributes.next()) {
            return if tag == MANGA_TAG { MANGA_TAG } else { NOVEL_TAG };
        }
    }
    site.map(|site| {
        if site.work_type_rules.is_empty() { return WorkType::Novel; }
        let novel_type = if site.work_type_rules.iter().any(|rule| rule.field == WorkTypeField::NovelType) {
            record.novel_type.to_string()
        } else { String::new() };
        let episode_count = if site.work_type_rules.iter().any(|rule| rule.field == WorkTypeField::EpisodeCount) {
            record.general_all_no.map(|count| count.to_string()).unwrap_or_default()
        } else { String::new() };
        site.work_type_for(&WorkTypeSubjects {
            ncode: record.ncode.as_deref().unwrap_or_default(),
            title: &record.title,
            author: &record.author,
            url: &record.toc_url,
            novel_type: &novel_type,
            episode_count: &episode_count,
            source: "",
        })
    }).unwrap_or(WorkType::Novel).tag()
}

#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub fn needs_initialization(record: &crate::db::NovelRecord, sites: &[crate::downloader::site_setting::SiteSetting]) -> bool {
    let tag = initial_tag(record, sites);
    record.tags.iter().filter(|value| is_attribute_tag(value)).count() != 1
        || !record.tags.iter().any(|value| value == tag)
}

#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub fn initialize_record(record: &mut crate::db::NovelRecord, sites: &[crate::downloader::site_setting::SiteSetting]) -> bool {
    let tag = initial_tag(record, sites);
    if record.tags.iter().filter(|value| is_attribute_tag(value)).count() == 1
        && record.tags.iter().any(|value| value == tag) {
        return false;
    }
    set_attribute_tag(&mut record.tags, tag);
    true
}
