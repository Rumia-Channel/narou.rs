//! Native / Worker share one settings snapshot and one Web UI configuration shape.

use crate::application::{ApplicationError, SettingsService};
use crate::setting_core::setting_bool_value;
use super::capabilities::{RuntimeKind, WebUiCapabilities};
use super::tag_order::TagSortOrder;

const CONFIG_NAMES: &[&str] = &[
    "webui.theme",
    "webui.performance-mode",
    "webui.table.reload-timing",
    "webui.debug-mode",
    "webui.tag-sort-order",
    "tag.show-novel",
    "tag.show-manga",
    "tag.attributes-enabled",
    "concurrency",
];

pub struct ConfigContext {
    pub capabilities: WebUiCapabilities,
    pub port: Option<u16>,
    pub ws_port: Option<u16>,
    pub convert_keep_txt_default: bool,
    pub queue_max_retries_limit: Option<u32>,
}

pub async fn load(settings: &SettingsService, context: ConfigContext) -> Result<serde_json::Value, ApplicationError> {
    // A failed read is not an empty configuration: callers must preserve the
    // user's current UI instead of replacing it with defaults after an outage.
    let loaded = settings.get_many(CONFIG_NAMES).await?;
    let find = |name: &str| CONFIG_NAMES.iter().zip(&loaded)
        .find(|(candidate, _)| **candidate == name)
        .and_then(|(_, value)| value.as_ref());
    let string = |name, default| find(name).and_then(serde_yaml::Value::as_str).unwrap_or(default);
    let boolean = |name, default| find(name).and_then(setting_bool_value).unwrap_or(default);
    let worker = context.capabilities.runtime == RuntimeKind::Worker;
    let order = TagSortOrder::from_setting(find("webui.tag-sort-order").and_then(serde_yaml::Value::as_str));
    Ok(serde_json::json!({
        "theme": string("webui.theme", "Cerulean"),
        "performance_mode": string("webui.performance-mode", "auto"),
        "reload_timing": string("webui.table.reload-timing", "every"),
        "debug_mode": boolean("webui.debug-mode", false),
        "tag_sort_order": order.setting_value(),
        "show_novel_attribute": boolean("tag.show-novel", false),
        "show_manga_attribute": boolean("tag.show-manga", true),
        "attribute_tags_enabled": boolean("tag.attributes-enabled", false),
        "ws_port": context.ws_port,
        "port": context.port,
        "concurrency_enabled": boolean("concurrency", worker),
        "concurrency_scope": if worker { "console" } else { "lanes" },
        "convert_keep_txt_default": context.convert_keep_txt_default,
        "queue_max_retries_limit": context.queue_max_retries_limit,
        "capabilities": context.capabilities,
    }))
}

pub struct TagListConfig {
    pub sort_order: TagSortOrder,
    pub attribute_tags_enabled: bool,
    pub new_tag_color: Option<String>,
}

pub async fn load_tag_list(settings: &SettingsService) -> Result<TagListConfig, ApplicationError> {
    let values = settings.get_many(&[
        crate::application::tag_colors::NEW_TAG_COLOR_SETTING,
        "tag.attributes-enabled",
        super::tag_order::TAG_SORT_ORDER_SETTING,
    ]).await?;
    let mut values = values.into_iter();
    let color = values.next().flatten();
    let new_tag_color = color.as_ref().and_then(serde_yaml::Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| crate::application::tag_colors::is_valid_tag_color(value));
    let attribute_tags_enabled = values.next().flatten().as_ref()
        .and_then(setting_bool_value).unwrap_or(false);
    let order = values.next().flatten();
    let sort_order = TagSortOrder::from_setting(order.as_ref().and_then(serde_yaml::Value::as_str));
    Ok(TagListConfig { sort_order, attribute_tags_enabled, new_tag_color })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::application::MemorySettingsStore;
    use super::super::capabilities::ConsoleHistoryCapabilities;

    fn context(worker: bool) -> ConfigContext {
        ConfigContext {
            capabilities: if worker {
                WebUiCapabilities::worker(ConsoleHistoryCapabilities {
                    available: true, capacity: 1000, clear: true, max_bytes: 1024,
                })
            } else { WebUiCapabilities::native(Vec::new()) },
            port: Some(33000), ws_port: Some(33000),
            convert_keep_txt_default: !worker,
            queue_max_retries_limit: worker.then_some(3),
        }
    }

    #[test]
    fn explicit_false_and_deletion_keep_runtime_concurrency_semantics() {
        futures::executor::block_on(async {
            let settings = SettingsService::new(Arc::new(MemorySettingsStore::new()));
            settings.set("concurrency", &serde_json::json!(false)).await.unwrap();
            settings.set("webui.tag-sort-order", &serde_json::json!("alphabetical")).await.unwrap();
            for worker in [false, true] {
                let config = load(&settings, context(worker)).await.unwrap();
                assert_eq!(config["concurrency_enabled"], false);
                assert_eq!(config["tag_sort_order"], "alphabetical");
            }
            settings.delete("concurrency").await.unwrap();
            settings.delete("webui.tag-sort-order").await.unwrap();
            for worker in [false, true] {
                let config = load(&settings, context(worker)).await.unwrap();
                assert_eq!(config["concurrency_enabled"], worker);
                assert_eq!(config["tag_sort_order"], "favorites_first");
            }
        });
    }
}
