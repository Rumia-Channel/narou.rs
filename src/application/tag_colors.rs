use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::Result;
use crate::platform::PlatformFuture;

const TAG_COLOR_ORDER: [&str; 7] = ["green", "yellow", "blue", "magenta", "cyan", "red", "white"];
pub const NEW_TAG_COLOR_SETTING: &str = "webui.new-tag-color";
pub const TAG_COLOR_DEFAULT: &str = "default";

#[derive(Debug, Clone, Default)]
pub struct TagColors {
    pub(crate) order: Vec<String>,
    pub(crate) colors: HashMap<String, String>,
}

/// An owned delta applied to the store's current, ordered table.
#[derive(Debug, Clone)]
pub enum TagColorUpdate {
    Ensure {
        tags: Vec<String>,
        default_color: Option<String>,
    },
    Set {
        tag: String,
        color: Option<String>,
    },
}

impl TagColors {
    pub fn into_map(self) -> HashMap<String, String> {
        self.colors
    }

    /// Parses a persisted tag color payload (a YAML mapping; JSON objects are
    /// valid YAML), preserving key order. Non-string values are skipped and a
    /// payload that is not a mapping yields an empty table.
    pub fn from_yaml_payload(payload: &str) -> std::result::Result<Self, serde_yaml::Error> {
        let value: serde_yaml::Value = serde_yaml::from_str(payload)?;
        Ok(Self::from_yaml_value(&value))
    }

    pub fn from_yaml_value(value: &serde_yaml::Value) -> Self {
        let mut tag_colors = Self::default();
        if let Some(mapping) = value.as_mapping() {
            for (tag, color) in mapping {
                if let (Some(tag), Some(color)) = (tag.as_str(), color.as_str()) {
                    tag_colors.set(tag, color);
                }
            }
        }
        tag_colors
    }

    /// Serializes the table as an ordered YAML mapping (persisted order).
    pub fn to_yaml_value(&self) -> serde_yaml::Value {
        let mut mapping = serde_yaml::Mapping::new();
        for (tag, color) in self.ordered_entries() {
            mapping.insert(
                serde_yaml::Value::String(tag.to_string()),
                serde_yaml::Value::String(color.to_string()),
            );
        }
        serde_yaml::Value::Mapping(mapping)
    }

    /// YAML text of [`Self::to_yaml_value`], for byte-oriented stores.
    pub fn to_yaml_string(&self) -> std::result::Result<String, serde_yaml::Error> {
        serde_yaml::to_string(&self.to_yaml_value())
    }

    /// Merges `other` into `self`: tags already present keep their position,
    /// tags only in `other` are appended in `other`'s order, and `other` wins
    /// on duplicate tags.
    pub fn merge(&mut self, other: &TagColors) {
        for (tag, color) in other.ordered_entries() {
            self.set(tag, color);
        }
    }

    /// Entries in persisted order: `order` first, then any entry without a
    /// recorded position (defensive; the mutators keep both in sync).
    fn ordered_entries(&self) -> Vec<(&str, &str)> {
        let mut entries = Vec::with_capacity(self.colors.len());
        let mut seen = HashSet::with_capacity(self.colors.len());
        for tag in &self.order {
            if let Some(color) = self.colors.get(tag) {
                entries.push((tag.as_str(), color.as_str()));
                seen.insert(tag.as_str());
            }
        }
        for (tag, color) in &self.colors {
            if seen.insert(tag.as_str()) {
                entries.push((tag.as_str(), color.as_str()));
            }
        }
        entries
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

fn ensure_tag_colors_with_default_color<'a>(
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
        let next_color = default_color.unwrap_or_else(|| next_tag_color(tag_colors));
        tag_colors.set(tag, next_color);
        changed = true;
    }
    changed
}

fn next_tag_color(tag_colors: &TagColors) -> &'static str {
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

/// Applies a delta without consulting settings or persistence. Ensuring tags
/// never overwrites an existing color, and rotation uses the current order.
pub fn apply_tag_color_update(colors: &mut TagColors, update: &TagColorUpdate) -> bool {
    match update {
        TagColorUpdate::Ensure { tags, default_color } => ensure_tag_colors_with_default_color(
            colors,
            tags.iter().map(String::as_str),
            default_color.as_deref(),
        ),
        TagColorUpdate::Set { tag, color } => {
            if colors.color_for(tag) == color.as_deref() {
                return false;
            }
            match color {
                Some(color) => colors.set(tag, color),
                None => colors.remove(tag),
            }
            true
        }
    }
}

pub trait TagColorStore: Send + Sync {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<TagColors>>;
    fn update<'a>(&'a self, update: TagColorUpdate) -> PlatformFuture<'a, Result<TagColors>>;
}

pub struct TagColorService {
    store: Arc<dyn TagColorStore>,
}

impl TagColorService {
    pub fn new(store: Arc<dyn TagColorStore>) -> Self {
        Self { store }
    }

    pub async fn for_tags(
        &self,
        tags: impl IntoIterator<Item = String>,
        default_color: Option<&str>,
    ) -> Result<HashMap<String, String>> {
        let colors = self
            .store
            .update(TagColorUpdate::Ensure {
                tags: tags.into_iter().collect(),
                default_color: default_color.map(str::to_string),
            })
            .await?;
        Ok(colors.into_map())
    }

    pub async fn set(&self, tag: &str, color: Option<&str>) -> Result<()> {
        self.store
            .update(TagColorUpdate::Set {
                tag: tag.to_string(),
                color: color.map(str::to_string),
            })
            .await?;
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryTagColorStore {
    colors: parking_lot::Mutex<TagColors>,
}

impl TagColorStore for MemoryTagColorStore {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<TagColors>> {
        Box::pin(async { Ok(self.colors.lock().clone()) })
    }

    fn update<'a>(&'a self, update: TagColorUpdate) -> PlatformFuture<'a, Result<TagColors>> {
        Box::pin(async move {
            let mut colors = self.colors.lock();
            apply_tag_color_update(&mut colors, &update);
            Ok(colors.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_and_persists_colors_without_filesystem() {
        let store = Arc::new(MemoryTagColorStore::default());
        let service = TagColorService::new(store.clone());
        let colors = futures::executor::block_on(service.for_tags(
            vec!["a".to_string(), "b".to_string()],
            Some("red"),
        ))
        .unwrap();
        assert_eq!(colors.get("a").map(String::as_str), Some("red"));
        assert_eq!(colors.get("b").map(String::as_str), Some("red"));
        assert!(store.colors.lock().contains("a"));
    }

    #[test]
    fn queued_autoassignment_uses_committed_manual_colors_and_removals() {
        for manual_color in [Some("red"), None] {
            let store = Arc::new(MemoryTagColorStore::default());
            let manual = TagColorService::new(store.clone());
            futures::executor::block_on(manual.set("X", Some("green"))).unwrap();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let automatic = TagColorService::new(store.clone());
            let pending = std::thread::spawn(move || {
                let tags = std::iter::once_with(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    "Y".to_string()
                });
                futures::executor::block_on(automatic.for_tags(tags, None)).unwrap()
            });

            entered_rx.recv().unwrap();
            futures::executor::block_on(manual.set("X", manual_color)).unwrap();
            release_tx.send(()).unwrap();
            let colors = pending.join().unwrap();
            let expected_y = if manual_color.is_some() { "white" } else { "green" };
            assert_eq!(colors.get("X").map(String::as_str), manual_color);
            assert_eq!(colors.get("Y").map(String::as_str), Some(expected_y));
            let persisted = futures::executor::block_on(store.load()).unwrap();
            assert_eq!(persisted.color_for("X"), manual_color);
            assert_eq!(persisted.color_for("Y"), Some(expected_y));
        }
    }

    #[test]
    fn concurrent_manual_sets_keep_every_committed_tag() {
        let store = Arc::new(MemoryTagColorStore::default());
        let barrier = Arc::new(std::sync::Barrier::new(25));
        let writers = (0..24).map(|index| {
            let service = TagColorService::new(store.clone());
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                futures::executor::block_on(service.set(&format!("tag-{index}"), Some("blue"))).unwrap();
            })
        }).collect::<Vec<_>>();
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        let persisted = futures::executor::block_on(store.load()).unwrap();
        assert_eq!(persisted.colors.len(), 24);
        for index in 0..24 {
            assert_eq!(persisted.color_for(&format!("tag-{index}")), Some("blue"));
        }
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
        assert_eq!(tag_colors.color_for("fav"), Some("green"));
        assert_eq!(tag_colors.color_for("later"), Some("yellow"));
        assert_eq!(tag_colors.color_for("todo"), Some("blue"));
    }

    #[test]
    fn ensure_tag_colors_uses_configured_default_color() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["auto", "manual"],
            Some("white")
        ));
        assert_eq!(tag_colors.color_for("auto"), Some("white"));
        assert_eq!(tag_colors.color_for("manual"), Some("white"));
    }

    #[test]
    fn ensure_tag_colors_falls_back_to_rotation_for_invalid_default_color() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["fav", "later"],
            Some("default")
        ));
        assert_eq!(tag_colors.color_for("fav"), Some("green"));
        assert_eq!(tag_colors.color_for("later"), Some("yellow"));
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
    fn remove_drops_color_and_order() {
        let mut tag_colors = TagColors::default();
        tag_colors.set("fav", "green");
        tag_colors.remove("fav");
        assert!(!tag_colors.colors.contains_key("fav"));
        assert!(!tag_colors.order.iter().any(|tag| tag == "fav"));
    }

    #[test]
    fn payload_round_trip_preserves_order_and_unknown_colors() {
        let mut tag_colors = TagColors::default();
        tag_colors.set("fav", "purple");
        tag_colors.set("later", "green");

        let yaml = tag_colors.to_yaml_string().unwrap();
        assert!(yaml.contains("fav: purple"));
        assert!(yaml.contains("later: green"));
        assert!(yaml.find("fav").unwrap() < yaml.find("later").unwrap());

        let reloaded = TagColors::from_yaml_payload(&yaml).unwrap();
        assert_eq!(reloaded.color_for("fav"), Some("purple"));
        assert_eq!(reloaded.color_for("later"), Some("green"));
        assert_eq!(reloaded.order, tag_colors.order);
    }

    #[test]
    fn json_payload_rotates_like_the_equivalent_yaml_payload() {
        let yaml = TagColors::from_yaml_payload("fav: green\nlater: yellow\n").unwrap();
        let json = TagColors::from_yaml_payload(r#"{"fav":"green","later":"yellow"}"#).unwrap();
        assert_eq!(yaml.to_yaml_string().unwrap(), json.to_yaml_string().unwrap());

        for mut tag_colors in [yaml, json] {
            assert!(ensure_tag_colors_with_default_color(&mut tag_colors, ["todo"], None));
            assert_eq!(tag_colors.color_for("todo"), Some("blue"));
        }
    }

    #[test]
    fn rotation_survives_save_and_reload() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors_with_default_color(
            &mut tag_colors,
            ["fav", "later"],
            None
        ));

        let persisted = tag_colors.to_yaml_string().unwrap();
        let mut reloaded = TagColors::from_yaml_payload(&persisted).unwrap();
        assert!(ensure_tag_colors_with_default_color(&mut reloaded, ["todo"], None));
        assert!(ensure_tag_colors_with_default_color(&mut tag_colors, ["todo"], None));
        assert_eq!(reloaded.color_for("todo"), Some("blue"));
        assert_eq!(reloaded.color_for("todo"), tag_colors.color_for("todo"));
    }

    #[test]
    fn merged_legacy_payload_keeps_deterministic_rotation_order() {
        let legacy = TagColors::from_yaml_payload(r#"{"shared":"red","legacy":"yellow"}"#).unwrap();
        let canonical = TagColors::from_yaml_payload("shared: blue\ncurrent: green\n").unwrap();

        let mut merged = legacy;
        merged.merge(&canonical);
        assert_eq!(merged.color_for("shared"), Some("blue"));
        assert_eq!(merged.color_for("legacy"), Some("yellow"));
        assert_eq!(merged.color_for("current"), Some("green"));
        assert_eq!(
            merged.order,
            vec![
                "shared".to_string(),
                "legacy".to_string(),
                "current".to_string()
            ]
        );

        let mut reloaded = TagColors::from_yaml_payload(&merged.to_yaml_string().unwrap()).unwrap();
        assert!(ensure_tag_colors_with_default_color(&mut reloaded, ["todo"], None));
        assert_eq!(reloaded.color_for("todo"), Some("yellow"));
    }

    #[test]
    fn payload_keeps_only_string_entries_in_mapping_order() {
        let tag_colors = TagColors::from_yaml_payload("good: green\nbad: [1, 2]\nnum: 7\n").unwrap();
        assert_eq!(tag_colors.color_for("good"), Some("green"));
        assert!(!tag_colors.contains("bad"));
        assert!(!tag_colors.contains("num"));

        let empty = TagColors::from_yaml_payload("just a scalar").unwrap();
        assert_eq!(empty.order.len(), 0);
        assert_eq!(empty.colors.len(), 0);
    }

    #[test]
    fn payload_parse_error_is_reported() {
        assert!(TagColors::from_yaml_payload("fav: [green").is_err());
    }
}
