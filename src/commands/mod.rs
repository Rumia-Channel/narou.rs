use std::collections::HashMap;

use narou_rs::compat::yaml_value_to_string;
use narou_rs::db::inventory::InventoryScope;

pub mod alias;
pub mod backup;
pub mod browser;
pub mod clean;
pub mod convert;
pub mod diff;
pub mod download;
pub mod folder;
pub mod help;
pub mod init;
pub mod log;
pub mod mail;
pub mod manage;
pub mod setting;
pub mod trace;
pub mod update;
pub mod version;
pub mod web;

fn resolve_alias_target(target: &str) -> String {
    narou_rs::db::with_database(|db| {
        let aliases: HashMap<String, serde_yaml::Value> =
            db.inventory().load("alias", InventoryScope::Local)?;
        Ok(aliases.get(target).and_then(yaml_value_to_string))
    })
    .ok()
    .flatten()
    .unwrap_or_else(|| target.to_string())
}

fn resolve_target_to_id(target: &str) -> Option<i64> {
    let target = resolve_alias_target(target);
    if let Ok(i) = target.parse::<i64>() {
        return Some(i);
    }
    narou_rs::db::with_database(|db| Ok(db.find_by_title(&target).map(|r| r.id)))
        .ok()
        .flatten()
}
