use std::collections::HashSet;

use crate::compat;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

pub use crate::application::tag_colors::{
    ensure_tag_colors_with_default_color, is_valid_new_tag_color_value, is_valid_tag_color,
    tag_color_names, MemoryTagColorStore, TagColorService, TagColorStore, TagColors,
    NEW_TAG_COLOR_SETTING, TAG_COLOR_DEFAULT,
};

pub fn load_tag_colors(inventory: &Inventory) -> Result<TagColors> {
    let raw = inventory.load_raw("tag_colors", InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(TagColors::default());
    }

    let value: serde_yaml::Value = serde_yaml::from_str(&raw)?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(TagColors::default());
    };

    let mut tag_colors = TagColors::default();
    for (tag, color) in mapping {
        let (Some(tag), Some(color)) = (tag.as_str(), color.as_str()) else {
            continue;
        };
        tag_colors.order.push(tag.to_string());
        tag_colors.colors.insert(tag.to_string(), color.to_string());
    }
    Ok(tag_colors)
}

pub fn save_tag_colors(inventory: &Inventory, tag_colors: &TagColors) -> Result<()> {
    let mut mapping = serde_yaml::Mapping::new();
    let mut written = HashSet::new();

    for tag in &tag_colors.order {
        let Some(color) = tag_colors.colors.get(tag) else {
            continue;
        };
        mapping.insert(
            serde_yaml::Value::String(tag.clone()),
            serde_yaml::Value::String(color.clone()),
        );
        written.insert(tag.clone());
    }

    for (tag, color) in &tag_colors.colors {
        if written.contains(tag) {
            continue;
        }
        mapping.insert(
            serde_yaml::Value::String(tag.clone()),
            serde_yaml::Value::String(color.clone()),
        );
    }

    inventory.save(
        "tag_colors",
        InventoryScope::Local,
        &serde_yaml::Value::Mapping(mapping),
    )?;
    Ok(())
}

/// Assigns colors to any `tags` not already present in `tag_colors`, using the
/// user-configured default color (falling back to rotation through the
/// standard color order when unset or invalid).
///
/// # Warning: do not call while holding the `DATABASE` lock
pub fn ensure_tag_colors<'a>(
    tag_colors: &mut TagColors,
    tags: impl IntoIterator<Item = &'a str>,
) -> bool {
    let configured_color = configured_new_tag_color();
    ensure_tag_colors_with_default_color(tag_colors, tags, configured_color.as_deref())
}

pub fn configured_new_tag_color() -> Option<String> {
    compat::load_local_setting_string(NEW_TAG_COLOR_SETTING)
        .and_then(|raw| normalize_default_color(&raw))
}

fn normalize_default_color(raw: &str) -> Option<String> {
    let color = raw.trim().to_ascii_lowercase();
    if is_valid_tag_color(&color) {
        Some(color)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("narou-rs-{}-{}", name, unique))
    }

    #[test]
    fn ensure_tag_colors_rotates_in_insertion_order() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["fav"],
            None
        ));
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["later"],
            None
        ));
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["todo"],
            None
        ));
        assert_eq!(
            tag_colors.colors.get("fav").map(String::as_str),
            Some("green")
        );
        assert_eq!(
            tag_colors.colors.get("later").map(String::as_str),
            Some("yellow")
        );
        assert_eq!(
            tag_colors.colors.get("todo").map(String::as_str),
            Some("blue")
        );
    }

    #[test]
    fn ensure_tag_colors_uses_configured_default_color() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["auto", "manual"],
            Some("white")
        ));
        assert_eq!(
            tag_colors.colors.get("auto").map(String::as_str),
            Some("white")
        );
        assert_eq!(
            tag_colors.colors.get("manual").map(String::as_str),
            Some("white")
        );
    }

    #[test]
    fn ensure_tag_colors_falls_back_to_rotation_for_invalid_default_color() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["fav", "later"],
            Some("default")
        ));
        assert_eq!(
            tag_colors.colors.get("fav").map(String::as_str),
            Some("green")
        );
        assert_eq!(
            tag_colors.colors.get("later").map(String::as_str),
            Some("yellow")
        );
    }

    #[test]
    fn new_tag_color_validation_accepts_default_and_colors() {
        assert!(is_valid_new_tag_color_value(TAG_COLOR_DEFAULT));
        for color in tag_color_names() {
            assert!(is_valid_new_tag_color_value(color));
        }
        assert!(!is_valid_new_tag_color_value(""));
        assert!(!is_valid_new_tag_color_value("purple"));
    }

    #[test]
    fn normalize_default_color_accepts_case_and_ignores_default() {
        assert_eq!(
            normalize_default_color(" White "),
            Some("white".to_string())
        );
        assert_eq!(normalize_default_color(TAG_COLOR_DEFAULT), None);
        assert_eq!(normalize_default_color(""), None);
    }

    #[test]
    fn remove_drops_color_and_order() {
        let mut tag_colors = TagColors::default();
        tag_colors.set("fav", "green");
        tag_colors.remove("fav");
        assert!(!tag_colors.colors.contains_key("fav"));
        assert!(!tag_colors.order.iter().any(|tag| tag == "fav"));
    }

    #[test]
    fn ensure_tag_colors_with_default_color_inside_with_database_does_not_deadlock() {
        // Regression test for a576208 / "Add configurable new tag color":
        // `configured_new_tag_color` reads settings through
        // `crate::db::with_database`, which uses a non-reentrant
        // `parking_lot::Mutex`. Calling `ensure_tag_colors` (which internally
        // calls `configured_new_tag_color`) from inside a `with_database` /
        // `with_database_mut` closure deadlocks the current thread forever.
        // Web handlers and `commands::manage` now fetch the configured color
        // *before* entering the closure and pass it to
        // `ensure_tag_colors_with_default_color` instead. This test mirrors
        // that pattern and would hang (rather than fail cleanly) if that
        // invariant were ever broken again.
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        fs::create_dir_all(temp.path().join(".narou")).unwrap();
        fs::write(
            temp.path().join(".narou").join("local_setting.yaml"),
            "webui.new-tag-color: white\n",
        )
        .unwrap();

        *crate::db::DATABASE.lock() = None;
        crate::db::init_database().unwrap();

        let new_tag_color = configured_new_tag_color();
        let result = crate::db::with_database(|_db| {
            let mut tag_colors = TagColors::default();
            ensure_tag_colors_with_default_color(&mut tag_colors, ["fresh"], new_tag_color.as_deref());
            Ok(tag_colors.color_for("fresh").map(|color| color.to_string()))
        })
        .unwrap();

        *crate::db::DATABASE.lock() = None;

        assert_eq!(result.as_deref(), Some("white"));
    }

    #[test]
    fn load_tag_colors_preserves_unknown_colors() {
        let root = temp_dir("tag-colors");
        fs::create_dir_all(root.join(".narou")).unwrap();
        fs::write(
            root.join(".narou").join("tag_colors.yaml"),
            "fav: purple\nlater: green\n",
        )
        .unwrap();

        let inventory = Inventory::new(root.clone());
        let tag_colors = load_tag_colors(&inventory).unwrap();

        assert_eq!(
            tag_colors.colors.get("fav").map(String::as_str),
            Some("purple")
        );
        assert_eq!(
            tag_colors.colors.get("later").map(String::as_str),
            Some("green")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_tag_colors_keeps_unknown_colors() {
        let root = temp_dir("tag-colors-save");
        fs::create_dir_all(root.join(".narou")).unwrap();

        let inventory = Inventory::new(root.clone());
        let mut tag_colors = TagColors::default();
        tag_colors.set("fav", "purple");
        tag_colors.set("later", "green");
        save_tag_colors(&inventory, &tag_colors).unwrap();

        let raw = fs::read_to_string(root.join(".narou").join("tag_colors.yaml")).unwrap();
        assert!(raw.contains("fav: purple"));
        assert!(raw.contains("later: green"));

        fs::remove_dir_all(root).unwrap();
    }
}
