//! Worker の秘密値の解決。
//!
//! 解決順は **Secrets Store (`<NAME>_STORE`) → vars → secrets**。
//!
//! - バケットの endpoint / region / bucket / 資格情報は Secrets Store に置くと、
//!   リポジトリにも CI にも「名前」しか残らない（Dantalian と同じ方式）。
//! - アプリのトークン (`NAROU_ADMIN_TOKEN`) や資格情報の復号鍵
//!   (`NAROU_RS_LOGIN_KEY`) は通常の Worker secret（`wrangler secret` /
//!   `--secrets-file`）を既定にしつつ、同じ仕組みで Secrets Store にも置ける。
//!
//! Secrets Store の値はランタイムが isolate ごとにキャッシュするので、
//! リクエストごとに `get` しても実害は小さい。

use worker::{Env, Result};

/// `<NAME>_STORE` → `NAME`(var) → `NAME`(secret) の順に解決する。
pub async fn value(env: &Env, name: &str) -> Option<String> {
    let binding = format!("{name}_STORE");
    if let Ok(store) = env.secret_store(&binding)
        && let Ok(Some(value)) = store.get().await
        && !value.is_empty()
    {
        return Some(value);
    }
    if let Ok(value) = env.var(name) {
        let value = value.to_string();
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    env.secret(name)
        .ok()
        .map(|secret| secret.to_string())
        .filter(|value| !value.trim().is_empty())
}

/// 解決できなければ設定名を添えて失敗する。
pub async fn require(env: &Env, name: &str) -> Result<String> {
    value(env, name)
        .await
        .ok_or_else(|| worker::Error::RustError(format!("{name} is not configured")))
}
