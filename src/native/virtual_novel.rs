//! Native のバーチャル小説メンバー構成ストア。
//!
//! 在庫 `virtual_novel_members` を唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='virtual_novel_members')`、YAML 管理時は
//! `.narou/virtual_novel_members.yaml`。

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;
use crate::virtual_novel::VirtualNovelMembers;

pub fn load() -> Result<VirtualNovelMembers> {
    let inventory = Inventory::with_default_root()?;
    Ok(inventory
        .load("virtual_novel_members", InventoryScope::Local)
        .unwrap_or_default())
}

pub fn save(members: &VirtualNovelMembers) -> Result<()> {
    let inventory = Inventory::with_default_root()?;
    inventory.save("virtual_novel_members", InventoryScope::Local, members)
}

pub fn update<F>(change: F) -> Result<VirtualNovelMembers>
where
    F: FnOnce(&mut VirtualNovelMembers) -> Result<()>,
{
    let inventory = Inventory::with_default_root()?;
    inventory.update_yaml::<VirtualNovelMembers, VirtualNovelMembers, _>(
        "virtual_novel_members",
        InventoryScope::Local,
        |mut members| {
            change(&mut members)?;
            Ok((members.clone(), members))
        },
    )
}
