//! Work-type classification ("属性タグ") handling for the transfer format.
//!
//! The classification is **metadata, not an ordinary tag**. It travels in the
//! manifest's `attribute_tag` field, so it never shows up in `tags`.
//!
//! Archives written before that field existed carried the value inside the
//! `tags` array instead. Those are migrated on read: a single distinct value
//! is moved into `attribute_tag`, conflicting values are ambiguous and are
//! dropped (the repository's own initialization then decides), and every
//! attribute name is removed from the ordinary tag list.

use crate::application::attribute_tags::{is_attribute_tag, MANGA_TAG, NOVEL_TAG};
use crate::transfer::{ArchiveNovel, Result, TransferError};

/// Normalises one manifest entry: validates the explicit classification and
/// moves any legacy attribute name out of the ordinary tag list.
///
/// An explicit `attribute_tag` always wins over a legacy value in `tags`.
pub fn normalize(novel: &mut ArchiveNovel) -> Result<()> {
    if let Some(tag) = novel.attribute_tag.as_deref() {
        validate(tag).map_err(TransferError::InvalidArchive)?;
    }
    let mut novel_seen = false;
    let mut manga_seen = false;
    novel.tags.retain(|tag| match tag.as_str() {
        NOVEL_TAG => { novel_seen = true; false }
        MANGA_TAG => { manga_seen = true; false }
        _ => true,
    });
    if novel.attribute_tag.is_none() {
        novel.attribute_tag = match (novel_seen, manga_seen) {
            (true, false) => Some(NOVEL_TAG.to_string()),
            (false, true) => Some(MANGA_TAG.to_string()),
            _ => None,
        };
    }
    Ok(())
}

/// Checks that a manifest's `attribute_tag` is a known classification.
pub fn validate(value: &str) -> std::result::Result<(), String> {
    if is_attribute_tag(value) {
        Ok(())
    } else {
        Err(format!("unknown attribute tag {value:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(attribute: Option<&str>, tags: &[&str]) -> ArchiveNovel {
        let mut novel = ArchiveNovel {
            attribute_tag: attribute.map(str::to_string),
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            ..ArchiveNovel::default()
        };
        normalize(&mut novel).unwrap();
        novel
    }

    #[test]
    fn explicit_attribute_wins_and_is_removed_from_tags() {
        let novel = normalized(Some("漫画"), &["異世界", "小説"]);
        assert_eq!(novel.attribute_tag.as_deref(), Some("漫画"));
        assert_eq!(novel.tags, vec!["異世界"]);
    }

    #[test]
    fn legacy_attribute_is_migrated_out_of_tags() {
        let novel = normalized(None, &["異世界", "漫画"]);
        assert_eq!(novel.attribute_tag.as_deref(), Some("漫画"));
        assert_eq!(novel.tags, vec!["異世界"]);
    }

    #[test]
    fn conflicting_legacy_attributes_are_dropped() {
        let novel = normalized(None, &["小説", "漫画"]);
        assert_eq!(novel.attribute_tag, None);
        assert!(novel.tags.is_empty());
    }

    #[test]
    fn duplicate_legacy_attribute_is_not_a_conflict() {
        let novel = normalized(None, &["小説", "小説"]);
        assert_eq!(novel.attribute_tag.as_deref(), Some("小説"));
        assert!(novel.tags.is_empty());
    }

    #[test]
    fn only_known_classifications_are_accepted() {
        assert!(validate("小説").is_ok());
        assert!(validate("漫画").is_ok());
        assert!(validate("ゲーム").is_err());
    }
}
