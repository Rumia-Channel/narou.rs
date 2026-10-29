//! Native のタグ登録順ストア。
//!
//! 在庫 (`Inventory`) の `tag_sequence` キーを唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='tag_sequence')`、YAML 管理時は
//! `.narou/tag_sequence.yaml` に保存される。
//!
//! 保存は `update_yaml` 経由なので、SQLite 管理時の `app_state` 書き込みと
//! YAML 管理時のファイル書き込みは同じ入口を通る。

use crate::application::tag_sequence::{LibraryTags, TagSequence};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

/// 在庫のキー名。
const STATE_KEY: &str = "tag_sequence";

/// 現在のライブラリのタグ登録順を読む。未保存なら空。
pub fn load() -> Result<TagSequence> {
    let inventory = Inventory::with_default_root()?;
    let raw = inventory.load_raw(STATE_KEY, InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(TagSequence::default());
    }
    Ok(serde_yaml::from_str(&raw)?)
}

/// 読み込み → 変更 → 保存を同じロック内で行う。
///
/// 保存される内容は変更後の状態。変更が無ければ同じ内容になる (YAML 管理時は
/// 同じ内容は書き直さない)。
pub fn update_with<F>(inventory: &Inventory, change: F) -> Result<TagSequence>
where
    F: FnOnce(&mut TagSequence),
{
    inventory.update_yaml::<TagSequence, TagSequence, _>(
        STATE_KEY,
        InventoryScope::Local,
        |mut state| {
            change(&mut state);
            Ok((state.clone(), state))
        },
    )
}

/// 小説の書き込みでタグの登録順を進める。
///
/// `before` は書き込み前のライブラリのタグ (登録順が未初期化のときの
/// バックフィルに使う)、`written` は書き込み後の小説に付いたタグ。
pub fn sync_records(
    inventory: &Inventory,
    before: Option<&LibraryTags>,
    written: &[&str],
) -> Result<TagSequence> {
    update_with(inventory, |sequence| {
        sequence.advance(before, written.iter().copied());
    })
}
