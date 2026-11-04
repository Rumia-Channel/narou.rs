//! Native の外部 API トークンストア。
//!
//! 在庫 (`Inventory`) の `api_tokens` キーを唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='api_tokens')`、YAML 管理時は
//! `.narou/api_tokens.yaml` に保存される。

use crate::application::api_tokens::{ApiToken, ApiTokenChange, ApiTokenStore};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::{NarouError, Result};
use crate::platform::PlatformFuture;

/// 在庫のキー。
pub const KEY: &str = "api_tokens";

/// 現在のライブラリのトークンを読む。未保存なら空。
pub fn load() -> Result<Vec<ApiToken>> {
    let inventory = Inventory::with_default_root()?;
    let raw = inventory.load_raw(KEY, InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_yaml::from_str(&raw)
        .map_err(|error| NarouError::Platform(format!("{KEY} を読めません: {error}")))
}

/// 変更を在庫に適用する。
pub fn apply(change: ApiTokenChange) -> Result<bool> {
    let inventory = Inventory::with_default_root()?;
    inventory.update_yaml::<bool, Vec<ApiToken>, _>(
        KEY,
        InventoryScope::Local,
        |current| {
            let mut next = current;
            let changed = match &change {
                ApiTokenChange::Create(token) => {
                    next.push(token.clone());
                    true
                }
                ApiTokenChange::Delete(id) => {
                    let before = next.len();
                    next.retain(|token| &token.id != id);
                    next.len() != before
                }
            };
            Ok((next, changed))
        },
    )
}

/// 在庫を [`ApiTokenStore`] として使うアダプタ。
pub struct NativeApiTokenStore;

impl ApiTokenStore for NativeApiTokenStore {
    fn load(&self) -> PlatformFuture<'_, Result<Vec<ApiToken>>> {
        Box::pin(async move { load() })
    }

    fn apply(&self, change: ApiTokenChange) -> PlatformFuture<'_, Result<bool>> {
        Box::pin(async move { apply(change) })
    }
}
