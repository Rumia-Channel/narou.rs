//! native の S3 互換ストレージ設定。
//!
//! 値は `local_setting` の `s3.*` を先に見て、無ければ環境変数 (`S3_*`) を
//! 使う。SORAHOST のようなコンテナでは環境変数だけで完結させられる
//! (`narou setting` を使わずに済む)。
//!
//! 挿絵を S3 に置くかは `s3.asset-backend` (`local` | `s3`、環境変数
//! `NAROU_RS_ASSET_BACKEND`) で決める。`s3` を選んだのに接続情報が欠けて
//! いれば失敗させる (fail-closed)。黙ってローカル保存へ落とすと、利用者が
//! 気付かないままディスクを食い潰す。

use std::sync::Arc;

use crate::error::Result;
use crate::platform::{S3Store, S3StoreConfig, SystemClock};

/// 挿絵の保存先を選ぶ設定 (`local` | `s3`)。既定は `local`。
pub const ASSET_BACKEND_SETTING: &str = "s3.asset-backend";

/// 設定 (`s3.*`) → 環境変数 (`S3_*`) の順に値を解決する。
///
/// 空文字は未設定として扱う (コンテナの未設定変数がそのまま入るケース)。
fn resolve(setting: &str, env: &str) -> Option<String> {
    crate::compat::load_local_setting_string(setting)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var(env)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

/// 挿絵を S3 に置く構成か。
pub fn illustrations_in_s3() -> bool {
    resolve(ASSET_BACKEND_SETTING, "NAROU_RS_ASSET_BACKEND")
        .is_some_and(|value| value.eq_ignore_ascii_case("s3"))
}

/// S3 の接続情報を解決する。`s3.asset-backend=s3` でなければ `None`。
///
/// 値が欠けていてもここでは失敗させない (何が足りないかは
/// [`S3Store::new`] の検証が示す)。呼び出し側は `None` を「ローカル保存」と
/// 読む。
pub fn store_config() -> Option<S3StoreConfig> {
    if !illustrations_in_s3() {
        return None;
    }
    Some(S3StoreConfig {
        endpoint: resolve("s3.endpoint", "S3_ENDPOINT").unwrap_or_default(),
        bucket: resolve("s3.bucket", "S3_BUCKET").unwrap_or_default(),
        region: resolve("s3.region", "S3_REGION").unwrap_or_default(),
        prefix: resolve("s3.prefix", "S3_PREFIX").unwrap_or_default(),
        // 挿絵の重複除去プールは SQLite モードと S3 が揃ったときだけ
        // 有効にする。YAML モードでは従来どおり小説ごとの `挿絵/` 名。
        illustration_dedup: crate::native::sqlite::state::illustration_dedup_enabled(),
        access_key_id: resolve("s3.access-key-id", "S3_ACCESS_KEY_ID").unwrap_or_default(),
        secret_access_key: resolve("s3.secret-access-key", "S3_SECRET_ACCESS_KEY")
            .unwrap_or_default(),
    })
}

/// 挿絵用の S3 ストアを作る。ローカル保存の構成では `None`。
pub fn illustration_store() -> Result<Option<Arc<S3Store>>> {
    let Some(config) = store_config() else {
        return Ok(None);
    };
    // 保存先は自分たちの設定値なので、サイト取得用のヘッダやティア
    // フォールバックは要らない (S3 は素の HTTPS API)。
    let http = Arc::new(crate::native::http::NativeHttpClient::new("narou_rs")?);
    let store = S3Store::new(config, http, Arc::new(SystemClock))?;
    Ok(Some(Arc::new(store)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 環境変数はテスト全体で共有されるため、触るテストは直列化する。
    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn clear_env() {
        for key in [
            "NAROU_RS_ASSET_BACKEND",
            "S3_ENDPOINT",
            "S3_BUCKET",
            "S3_REGION",
            "S3_PREFIX",
            "S3_ACCESS_KEY_ID",
            "S3_SECRET_ACCESS_KEY",
        ] {
            unsafe { std::env::remove_var(key) };
        }
    }

    #[test]
    fn the_backend_defaults_to_local() {
        let _guard = ENV_LOCK.lock();
        clear_env();
        assert!(!illustrations_in_s3());
        assert!(store_config().is_none());
    }

    #[test]
    fn the_environment_selects_s3_and_supplies_the_connection() {
        let _guard = ENV_LOCK.lock();
        clear_env();
        unsafe {
            std::env::set_var("NAROU_RS_ASSET_BACKEND", "s3");
            std::env::set_var("S3_ENDPOINT", "https://s3.example.com");
            std::env::set_var("S3_BUCKET", "bucket");
            std::env::set_var("S3_REGION", "ap-northeast-1");
            std::env::set_var("S3_PREFIX", "narou/test");
            std::env::set_var("S3_ACCESS_KEY_ID", "key");
            std::env::set_var("S3_SECRET_ACCESS_KEY", "secret");
        }
        assert!(illustrations_in_s3());
        let config = store_config().unwrap();
        assert_eq!(config.endpoint, "https://s3.example.com");
        assert_eq!(config.bucket, "bucket");
        assert_eq!(config.region, "ap-northeast-1");
        assert_eq!(config.prefix, "narou/test");
        clear_env();
    }

    #[test]
    fn a_half_configured_backend_is_not_silently_ignored() {
        let _guard = ENV_LOCK.lock();
        clear_env();
        unsafe { std::env::set_var("NAROU_RS_ASSET_BACKEND", "s3") };
        // backend だけあって接続情報が無い: config は作られるが検証で落ちる。
        let config = store_config().unwrap();
        assert!(
            S3Store::new(
                config,
                Arc::new(crate::platform::mocks::MockHttpClient::new()),
                Arc::new(SystemClock),
            )
            .is_err()
        );
        clear_env();
    }
}
