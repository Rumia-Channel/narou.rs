use std::collections::BTreeMap;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::ser::SerializeMap;

use crate::db::NovelRecord;
/// Resolve saved works using page/API URL first, retaining domain-only legacy records.
pub fn allows_automatic_update(sites: &[super::SiteSetting], record: &NovelRecord) -> bool {
    let display_url = record.display_url();
    sites.iter()
        .find(|site| site.matches_url(&record.toc_url)
            || (display_url != record.toc_url && site.matches_url(display_url)))
        .or_else(|| sites.iter().find(|site| record.domain.as_deref() == Some(site.domain.as_str())))
        .is_none_or(|site| site.allows_automatic_update(record))
}


/// Ordered, metadata-only policy for scheduled updates. Manual operations do not use it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRule {
    #[serde(rename = "match", deserialize_with = "deserialize_match", serialize_with = "serialize_match")]
    conditions: BTreeMap<UpdateField, Regex>,
    automatic: AutomaticUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdateField {
    Ncode,
    Title,
    Author,
    Url,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AutomaticUpdate {
    Allow,
    Skip,
}

impl UpdateRule {
    pub(super) fn decision(&self, record: &NovelRecord) -> Option<bool> {
        self.conditions.iter().all(|(field, pattern)| {
            let value = match field {
                UpdateField::Ncode => record.ncode.as_deref(),
                UpdateField::Title => Some(record.title.as_str()),
                UpdateField::Author => Some(record.author.as_str()),
                UpdateField::Url => Some(record.display_url()),
            };
            value.is_some_and(|value| pattern.is_match(value))
        }).then_some(matches!(self.automatic, AutomaticUpdate::Allow))
    }
}

fn deserialize_match<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<UpdateField, Regex>, D::Error> {
    let patterns = BTreeMap::<UpdateField, String>::deserialize(deserializer)?;
    if patterns.is_empty() {
        return Err(serde::de::Error::custom("update_rules.match requires at least one condition"));
    }
    patterns.into_iter().map(|(field, source)| {
        Regex::new(&source).map(|pattern| (field, pattern)).map_err(serde::de::Error::custom)
    }).collect()
}

fn serialize_match<S: Serializer>(conditions: &BTreeMap<UpdateField, Regex>, serializer: S) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(conditions.len()))?;
    for (field, pattern) in conditions {
        map.serialize_entry(field, pattern.as_str())?;
    }
    map.end()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ncode: Option<&str>) -> NovelRecord {
        let mut record: NovelRecord = serde_yaml::from_str("id: 1\nauthor: Writer\ntitle: Picture\nfile_title: Picture\ntoc_url: https://images.example/ajax/1\nsitename: Images\nnovel_type: 2\nlast_update: '2026-10-31T00:00:00Z'\n").unwrap();
        record.ncode = ncode.map(str::to_string);
        record
    }

    #[test]
    fn rules_match_metadata_and_preserve_first_match_precedence() {
        let rules: Vec<UpdateRule> = serde_yaml::from_str("- match: {ncode: '^a', author: '^Writer$'}\n  automatic: allow\n- match: {ncode: '^a'}\n  automatic: skip\n").unwrap();
        let decision = |record: &NovelRecord| rules.iter().find_map(|rule| rule.decision(record)).unwrap_or(true);
        let mut single = record(Some("a123"));
        assert!(decision(&single));
        single.author = "Other".to_string();
        assert!(!decision(&single));
        assert!(decision(&record(Some("c123"))));
        assert!(decision(&record(None)));
        let restored: Vec<UpdateRule> = serde_yaml::from_str(&serde_yaml::to_string(&rules).unwrap()).unwrap();
        assert_eq!(restored[1].decision(&single), Some(false));
    }

    #[test]
    fn url_condition_uses_the_original_page_when_available() {
        let rule: UpdateRule = serde_yaml::from_str("match: {url: '/artworks/', title: '^Picture$'}\nautomatic: skip\n").unwrap();
        let mut single = record(Some("a123"));
        assert_eq!(rule.decision(&single), None);
        single.set_original_url("https://images.example/artworks/123");
        assert_eq!(rule.decision(&single), Some(false));
    }

    #[test]
    fn invalid_rules_are_rejected_instead_of_silently_disabling_policy() {
        for yaml in [
            "match: {ncode: '['}\nautomatic: skip",
            "match: {}\nautomatic: skip",
            "match: {ncode: '^a'}\nautomatic: skpi",
            "match: {ncdoe: '^a'}\nautomatic: skip",
            "match: {ncode: '^a'}\nautomatic: skip\nunknown: true",
        ] {
            assert!(serde_yaml::from_str::<UpdateRule>(yaml).is_err(), "{yaml}");
        }
    }

    #[test]
    fn bundled_pixiv_excludes_single_art_but_keeps_series_and_novels() {
        let sites = super::super::SiteSetting::load_bundled(&[include_str!("../../../webnovel/www.pixiv.net.yaml")]).unwrap();
        for (ncode, expected) in [("a123", false), ("c123", true), ("n123", true), ("s123", true)] {
            let mut work = record(Some(ncode));
            work.toc_url = "https://www.pixiv.net/ajax/illust/123".into();
            assert_eq!(allows_automatic_update(&sites, &work), expected, "{ncode}");
        }
        let mut legacy = record(Some("a123"));
        legacy.domain = Some("www.pixiv.net".into());
        assert!(!allows_automatic_update(&sites, &legacy));
        legacy.domain = Some("unrelated.example".into());
        assert!(allows_automatic_update(&sites, &legacy));
    }
}
