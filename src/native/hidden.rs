//! Native の非表示設定ストア。
//!
//! 在庫 (`Inventory`) の `hidden` キーを唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='hidden')`、YAML 管理時は
//! `.narou/hidden.yaml` に保存される。

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;
use crate::hidden::HiddenState;

/// 現在のライブラリの非表示設定を読む。未保存なら空。
pub fn load() -> Result<HiddenState> {
    let inventory = Inventory::with_default_root()?;
    Ok(inventory.load("hidden", InventoryScope::Local).unwrap_or_default())
}

/// 現在のライブラリへ非表示設定を保存する。
pub fn save(state: &HiddenState) -> Result<()> {
    let inventory = Inventory::with_default_root()?;
    inventory.save("hidden", InventoryScope::Local, state)
}

/// 読み込み → 変更 → 保存を同じロック内で行う。
pub fn update<F>(change: F) -> Result<HiddenState>
where
    F: FnOnce(&mut HiddenState),
{
    let inventory = Inventory::with_default_root()?;
    inventory.update_yaml::<HiddenState, HiddenState, _>(
        "hidden",
        InventoryScope::Local,
        |mut state| {
            change(&mut state);
            Ok((state.clone(), state))
        },
    )
}
