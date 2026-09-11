use std::collections::HashMap;
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

pub trait TagColorStore: Send + Sync {
    fn load<'a>(&'a self) -> PlatformFuture<'a, Result<TagColors>>;
    fn save<'a>(&'a self, colors: &'a TagColors) -> PlatformFuture<'a, Result<()>>;
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
        let mut colors = self.store.load().await?;
        let tags = tags.into_iter().collect::<Vec<_>>();
        if ensure_tag_colors_with_default_color(
            &mut colors,
            tags.iter().map(String::as_str),
            default_color,
        ) {
            self.store.save(&colors).await?;
        }
        Ok(colors.into_map())
    }

    pub async fn set(&self, tag: &str, color: Option<&str>) -> Result<()> {
        let mut colors = self.store.load().await?;
        match color {
            Some(color) => colors.set(tag, color),
            None => colors.remove(tag),
        }
        self.store.save(&colors).await
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

    fn save<'a>(&'a self, colors: &'a TagColors) -> PlatformFuture<'a, Result<()>> {
        let colors = colors.clone();
        Box::pin(async move {
            *self.colors.lock() = colors;
            Ok(())
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
}
