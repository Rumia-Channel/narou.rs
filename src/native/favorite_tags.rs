//! Native のお気に入りタグストア。
//!
//! 在庫 (`Inventory`) の `favorite_tags` キーを唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='favorite_tags')`、YAML 管理時は
//! `.narou/favorite_tags.yaml` に保存される。

use crate::application::favorite_tags::FavoriteTags;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

/// 現在のライブラリのお気に入りタグを読む。未保存なら空。
pub fn load() -> Result<FavoriteTags> {
    let inventory = Inventory::with_default_root()?;
    let raw = inventory.load_raw("favorite_tags", InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(FavoriteTags::default());
    }
    let value: serde_yaml::Value = serde_yaml::from_str(&raw)?;
    Ok(FavoriteTags::from_yaml_value(&value))
}

/// 現在のライブラリへお気に入りタグを保存する。
pub fn save(state: &FavoriteTags) -> Result<()> {
    let inventory = Inventory::with_default_root()?;
    inventory.save("favorite_tags", InventoryScope::Local, state)
}

/// 読み込み → 変更 → 保存を同じロック内で行う。
pub fn update<F>(change: F) -> Result<FavoriteTags>
where
    F: FnOnce(&mut FavoriteTags),
{
    let inventory = Inventory::with_default_root()?;
    inventory.update_yaml::<FavoriteTags, FavoriteTags, _>(
        "favorite_tags",
        InventoryScope::Local,
        |mut state| {
            change(&mut state);
            Ok((state.clone(), state))
        },
    )
}
