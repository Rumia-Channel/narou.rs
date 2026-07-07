use std::collections::{HashMap, HashSet};

use crate::compat;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

const TAG_COLOR_ORDER: [&str; 7] = ["green", "yellow", "blue", "magenta", "cyan", "red", "white"];
pub const NEW_TAG_COLOR_SETTING: &str = "webui.new-tag-color";
pub const TAG_COLOR_DEFAULT: &str = "default";

#[derive(Debug, Clone, Default)]
pub struct TagColors {
    order: Vec<String>,
    colors: HashMap<String, String>,
}

impl TagColors {
    pub fn into_map(self) -> HashMap<String, String> {
        self.colors
    }

    pub fn color_for(&self, tag: &str) -> Option<&str> {
        self.colors.get(tag).map(String::as_str)
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.colors.contains_key(tag)
    }

    pub fn remove(&mut self, tag: &str) {
        self.colors.remove(tag);
        self.order.retain(|name| name != tag);
    }

    pub fn set(&mut self, tag: &str, color: &str) {
        if !self.colors.contains_key(tag) {
            self.order.push(tag.to_string());
        }
        self.colors.insert(tag.to_string(), color.to_string());
    }

    pub fn set_color(&mut self, tag: &str, color: &str, no_overwrite_color: bool) -> bool {
        if no_overwrite_color && self.colors.contains_key(tag) {
            return false;
        }

        if !self.colors.contains_key(tag) {
            self.order.push(tag.to_string());
        }

        if self.colors.get(tag).is_some_and(|current| current == color) {
            return false;
        }
        self.colors.insert(tag.to_string(), color.to_string());
        true
    }
}

pub fn is_valid_tag_color(color: &str) -> bool {
    TAG_COLOR_ORDER.contains(&color)
}

pub fn is_valid_new_tag_color_value(color: &str) -> bool {
    color == TAG_COLOR_DEFAULT || is_valid_tag_color(color)
}

pub fn tag_color_names() -> &'static [&'static str] {
    &TAG_COLOR_ORDER
}

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

pub fn ensure_tag_colors<'a>(
    tag_colors: &mut TagColors,
    tags: impl IntoIterator<Item = &'a str>,
) -> bool {
    let configured_color = configured_new_tag_color();
    ensure_tag_colors_with_default_color(tag_colors, tags, configured_color.as_deref())
}

pub fn ensure_tag_colors_with_default_color<'a>(
    tag_colors: &mut TagColors,
    tags: impl IntoIterator<Item = &'a str>,
    default_color: Option<&str>,
) -> bool {
    let default_color = default_color.filter(|color| is_valid_tag_color(color));
    let mut changed = false;
    for tag in tags {
        if tag_colors.colors.contains_key(tag) {
            continue;
        }
        let next_color = default_color
            .unwrap_or_else(|| next_tag_color(tag_colors))
            .to_string();
        tag_colors.set(tag, &next_color);
        changed = true;
    }
    changed
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

fn next_tag_color(tag_colors: &TagColors) -> &str {
    let last_color = tag_colors
        .order
        .iter()
        .rev()
        .find_map(|tag| tag_colors.colors.get(tag))
        .map(String::as_str)
        .unwrap_or(TAG_COLOR_ORDER[TAG_COLOR_ORDER.len() - 1]);
    let current_index = TAG_COLOR_ORDER
        .iter()
        .position(|color| *color == last_color)
        .unwrap_or(TAG_COLOR_ORDER.len() - 1);
    TAG_COLOR_ORDER[(current_index + 1) % TAG_COLOR_ORDER.len()]
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
