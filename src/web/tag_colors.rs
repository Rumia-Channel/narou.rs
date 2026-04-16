use std::collections::{HashMap, HashSet};

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

const TAG_COLOR_ORDER: [&str; 7] = ["green", "yellow", "blue", "magenta", "cyan", "red", "white"];

#[derive(Debug, Clone, Default)]
pub struct TagColors {
    order: Vec<String>,
    colors: HashMap<String, String>,
}

impl TagColors {
    pub fn into_map(self) -> HashMap<String, String> {
        self.colors
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
}

pub fn is_valid_tag_color(color: &str) -> bool {
    TAG_COLOR_ORDER.contains(&color)
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
        if !is_valid_tag_color(color) {
            continue;
        }
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
    let mut changed = false;
    for tag in tags {
        if tag_colors.colors.contains_key(tag) {
            continue;
        }
        let next_color = next_tag_color(tag_colors).to_string();
        tag_colors.set(tag, &next_color);
        changed = true;
    }
    changed
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
        assert!(ensure_tag_colors(&mut tag_colors, ["fav"]));
        assert!(ensure_tag_colors(&mut tag_colors, ["later"]));
        assert!(ensure_tag_colors(&mut tag_colors, ["todo"]));
        assert_eq!(tag_colors.colors.get("fav").map(String::as_str), Some("green"));
        assert_eq!(tag_colors.colors.get("later").map(String::as_str), Some("yellow"));
        assert_eq!(tag_colors.colors.get("todo").map(String::as_str), Some("blue"));
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
    fn load_tag_colors_skips_invalid_colors() {
        let root = temp_dir("tag-colors");
        fs::create_dir_all(root.join(".narou")).unwrap();
        fs::write(
            root.join(".narou").join("tag_colors.yaml"),
            "fav: purple\nlater: green\n",
        )
        .unwrap();

        let inventory = Inventory::new(root.clone());
        let tag_colors = load_tag_colors(&inventory).unwrap();

        assert_eq!(tag_colors.colors.get("fav"), None);
        assert_eq!(tag_colors.colors.get("later").map(String::as_str), Some("green"));

        fs::remove_dir_all(root).unwrap();
    }
}
