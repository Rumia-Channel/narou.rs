//! Work-type classification constants ("属性タグ").
//!
//! Kept in a dependency-free leaf module so the portable transfer core
//! (`transfer-runtime`, compiled without any application/runtime feature) can
//! validate classifications without pulling in the application services.
//! [`crate::application::attribute_tags`] re-exports everything defined here.

/// Classification for novels (小説).
pub const NOVEL_TAG: &str = "小説";

/// Classification for manga (漫画).
pub const MANGA_TAG: &str = "漫画";

/// Whether `tag` is a reserved work-type classification name.
///
/// Classifications are metadata, not ordinary tags; they travel in the
/// `attribute_tag` field and must never appear inside `tags`.
pub fn is_attribute_tag(tag: &str) -> bool {
    matches!(tag, NOVEL_TAG | MANGA_TAG)
}
