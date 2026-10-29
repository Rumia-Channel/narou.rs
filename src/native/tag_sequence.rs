//! Native のタグ登録順ストア。
//!
//! 在庫 (`Inventory`) の `tag_sequence` キーを唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='tag_sequence')`、YAML 管理時は
//! `.narou/tag_sequence.yaml` に保存される。
//!
//! 保存は `update_yaml` 経由なので、SQLite 管理時の `app_state` 書き込みと
//! YAML 管理時のファイル書き込みは同じ入口を通る。
//!
//! 連番表の更新は **小説の保存と同じ直列化境界** の中で行う。別に更新すると
//! 「作品を保存した順」と「連番を振った順」がずれてしまい、記録と順序の
//! 確定が別々になってしまうため。

use crate::application::tag_sequence::{LibraryTags, TagSequence};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

/// 在庫のキー名。
const STATE_KEY: &str = "tag_sequence";

/// 現在のライブラリのタグ登録順を読む。未保存なら空。
pub fn load() -> Result<TagSequence> {
    let inventory = Inventory::with_default_root()?;
    let raw = inventory.load_raw(STATE_KEY, InventoryScope::Local)?;
    Ok(parse(&raw))
}

/// 連番表の内容 (YAML) を復元する。空・読めなければ空の連番表。
pub fn parse(content: &str) -> TagSequence {
    if content.trim().is_empty() {
        return TagSequence::default();
    }
    serde_yaml::from_str(content).unwrap_or_default()
}

/// 連番表を YAML にする (在庫の保存形式と 같다 ✓)。
pub fn serialize(sequence: &TagSequence) -> Result<String> {
    crate::db::inventory::serialize_yaml_content(sequence)
}

/// 連番表を進める。
///
/// `current` は現在の連番表の内容 (YAML)、`before` は書き込み前のライブラリの
/// タグ (連番表が未初期化のときのバックフィルに使う)、`written` は書き込み後の
/// 小説に付けたタグ。
///
/// 返り値は (次の内容, 順序が変わったかどうか)。
pub fn advance_content(
    current: &str,
    before: Option<&LibraryTags>,
    written: &[&str],
) -> Result<(String, bool)> {
    notify_advance_hook();
    let mut sequence = parse(current);
    let changed = sequence.advance(before, written.iter().copied());
    Ok((serialize(&sequence)?, changed))
}

#[cfg(test)]
type RecordSaveHook = Box<dyn Fn() -> Result<()> + Send>;

#[cfg(test)]
thread_local! {
    /// テスト用: 連番表を進める直前に呼ぶフック。
    ///
    /// 書き込みの割り込み (2 つの書き込みの順序が入れ替わる状況) を再現する
    /// テストで使う。スレッドローカルなので、ほかのテストに混ざらない。
    static ADVANCE_HOOK: std::cell::RefCell<Option<Box<dyn Fn() + Send>>> =
        const { std::cell::RefCell::new(None) };
    /// テスト用: 作品データの保存に失敗を注入するフック。
    ///
    /// 連番表は作品と同じ直列化境界の中で確定するため、作品データの保存が
    /// 失敗したときの連番表の巻き戻しを検証する。
    static RECORD_SAVE_HOOK: std::cell::RefCell<Option<RecordSaveHook>> =
        const { std::cell::RefCell::new(None) };
}

/// テスト用: 連番表を進める直前に呼ぶフックを登録する。
#[cfg(test)]
pub(crate) fn set_advance_hook<F: Fn() + Send + 'static>(hook: F) {
    ADVANCE_HOOK.with(|slot| {
        slot.replace(Some(Box::new(hook)));
    });
}

/// テスト用: 作品データの保存に失敗を注入するフックを登録する。
#[cfg(test)]
pub(crate) fn set_record_save_hook<F: Fn() -> Result<()> + Send + 'static>(hook: F) {
    RECORD_SAVE_HOOK.with(|slot| {
        slot.replace(Some(Box::new(hook)));
    });
}

/// 登録されたフックを呼ぶ (未登録なら何もしない ✓)。
#[cfg(not(test))]
fn notify_advance_hook() {}

/// 登録されたフックを呼ぶ (未登録なら何もしない ✓)。
#[cfg(test)]
fn notify_advance_hook() {
    ADVANCE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook();
        }
    });
}

/// 作品データの保存に失敗を注入する (未登録なら何もしない ✓)。
#[cfg(not(test))]
pub(crate) fn notify_record_save_hook() -> Result<()> {
    Ok(())
}

/// 作品データの保存に失敗を注入する (未登録なら何もしない ✓)。
#[cfg(test)]
pub(crate) fn notify_record_save_hook() -> Result<()> {
    RECORD_SAVE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook()?;
        }
        Ok(())
    })
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
