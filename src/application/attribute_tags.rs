//! Work-type classification and its reserved, read-only tag projection.

pub const NOVEL_TAG: &str = "小説";
pub const MANGA_TAG: &str = "漫画";
pub const ENABLED_SETTING: &str = "tag.attributes-enabled";

pub async fn enabled(settings: &crate::application::settings::SettingsService) -> bool {
    settings.get(ENABLED_SETTING).await.ok().flatten()
        .and_then(|value| crate::setting_core::setting_bool_value(&value))
        .unwrap_or(false)
}

#[cfg(feature = "native-runtime")]
pub fn enabled_native() -> bool {
    crate::db::settings::bool_value(crate::setting_core::SettingScope::Local, ENABLED_SETTING)
        .unwrap_or(false)
}

/// Add attributes only to a disposable presentation snapshot, never storage.
pub fn project(records: &mut [crate::db::NovelRecord], enabled: bool) {
    if enabled {
        for record in records {
            if let Some(tag) = record.attribute_tag().map(str::to_owned) {
                record.tags.push(tag);
            }
        }
    }
}

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

/// Infer an existing record's type from persisted metadata. Preserve a previous
/// classification when the site or required page content is unavailable.
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub fn initial_tag(
    record: &crate::db::NovelRecord,
    sites: &[crate::downloader::site_setting::SiteSetting],
) -> &'static str {
    if let Some(tag) = record.attribute_tag() {
        return if tag == MANGA_TAG { MANGA_TAG } else { NOVEL_TAG };
    }
    use crate::downloader::site_setting::{WorkType, WorkTypeField, WorkTypeSubjects};
    let site = sites.iter().find(|site| site.matches_url(&record.toc_url))
        .or_else(|| sites.iter().find(|site| record.domain.as_deref() == Some(site.domain.as_str())));
    if site.is_none() || site.is_some_and(|site| site.work_type_rules.iter().any(|rule| rule.field == WorkTypeField::Source)) {
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
pub fn needs_initialization(record: &crate::db::NovelRecord, _sites: &[crate::downloader::site_setting::SiteSetting]) -> bool {
    record.attribute_tag().is_none() || record.tags.iter().any(|tag| is_attribute_tag(tag))
}

#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub fn initialize_record(record: &mut crate::db::NovelRecord, sites: &[crate::downloader::site_setting::SiteSetting]) -> bool {
    if !needs_initialization(record, sites) {
        return false;
    }
    let tag = initial_tag(record, sites);
    record.tags.retain(|value| !is_attribute_tag(value));
    record.set_attribute_tag(tag);
    true
}
