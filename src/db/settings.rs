//! Centralized native settings persistence.
//!
//! Production code must access `local_setting` / `global_setting` through
//! this module instead of reading or writing the compatibility YAML files
//! directly. `Inventory` remains the backend boundary and transparently
//! selects SQLite app_state or legacy YAML storage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;
use crate::setting_core::SettingScope;

pub type SettingsMap = HashMap<String, Value>;

fn inventory_scope(scope: SettingScope) -> InventoryScope {
    match scope {
        SettingScope::Local => InventoryScope::Local,
        SettingScope::Global => InventoryScope::Global,
    }
}

fn inventory_name(scope: SettingScope) -> &'static str {
    match scope {
        SettingScope::Local => "local_setting",
        SettingScope::Global => "global_setting",
    }
}

fn current_inventory() -> Inventory {
    Inventory::with_default_root().unwrap_or_else(|_| {
        Inventory::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    })
}

pub fn load(scope: SettingScope) -> Result<SettingsMap> {
    let inventory = current_inventory();
    load_with_inventory(&inventory, scope)
}

pub fn load_for_root(root: &Path, scope: SettingScope) -> Result<SettingsMap> {
    let inventory = Inventory::new(root.to_path_buf());
    load_with_inventory(&inventory, scope)
}

pub fn load_with_inventory(inventory: &Inventory, scope: SettingScope) -> Result<SettingsMap> {
    inventory.load(inventory_name(scope), inventory_scope(scope))
}

pub fn save(scope: SettingScope, settings: &SettingsMap) -> Result<()> {
    let inventory = current_inventory();
    save_with_inventory(&inventory, scope, settings)
}

pub fn save_for_root(root: &Path, scope: SettingScope, settings: &SettingsMap) -> Result<()> {
    let inventory = Inventory::new(root.to_path_buf());
    save_with_inventory(&inventory, scope, settings)
}

pub fn save_with_inventory(
    inventory: &Inventory,
    scope: SettingScope,
    settings: &SettingsMap,
) -> Result<()> {
    inventory.save(inventory_name(scope), inventory_scope(scope), settings)
}

pub fn update<T, F>(scope: SettingScope, update: F) -> Result<T>
where
    F: FnOnce(&mut SettingsMap) -> Result<T>,
{
    let inventory = current_inventory();
    update_with_inventory(&inventory, scope, update)
}

pub fn update_with_inventory<T, F>(
    inventory: &Inventory,
    scope: SettingScope,
    update: F,
) -> Result<T>
where
    F: FnOnce(&mut SettingsMap) -> Result<T>,
{
    inventory.update_yaml(
        inventory_name(scope),
        inventory_scope(scope),
        |mut settings: SettingsMap| {
            let result = update(&mut settings)?;
            Ok((settings, result))
        },
    )
}

pub fn value(scope: SettingScope, key: &str) -> Option<Value> {
    load(scope).ok()?.get(key).cloned()
}

pub fn string(scope: SettingScope, key: &str) -> Option<String> {
    value(scope, key).and_then(|value| match value {
        Value::String(value) => Some(value),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

pub fn bool_value(scope: SettingScope, key: &str) -> Option<bool> {
    value(scope, key).and_then(|value| match value {
        Value::Bool(value) => Some(value),
        Value::Number(value) => value.as_i64().map(|value| value != 0),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

pub fn list(scope: SettingScope, key: &str) -> Vec<String> {
    match value(scope, key) {
        Some(Value::Sequence(values)) => values
            .into_iter()
            .filter_map(|value| match value {
                Value::String(value) => Some(value),
                Value::Bool(value) => Some(value.to_string()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            })
            .collect(),
        Some(Value::String(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(_) | None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_uses_inventory_backend() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());

        let mut expected = SettingsMap::new();
        expected.insert("default.enable_yokogaki".into(), Value::Bool(true));
        save_with_inventory(&inventory, SettingScope::Local, &expected).unwrap();

        let actual = load_with_inventory(&inventory, SettingScope::Local).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn update_with_inventory_is_the_shared_mutation_path() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());

        update_with_inventory(&inventory, SettingScope::Local, |settings| {
            settings.insert("force.enable_illust".into(), Value::Bool(false));
            Ok(())
        })
        .unwrap();

        let actual = load_with_inventory(&inventory, SettingScope::Local).unwrap();
        assert_eq!(actual.get("force.enable_illust"), Some(&Value::Bool(false)));
    }
}
