//! S3 バケットの構成 (Worker ランタイム)。
//!
//! 実装は core の [`narou_rs::platform::S3Store`] にあり、ここは Worker の
//! バインディング (secret / Secrets Store / vars) から接続情報を読んで
//! 組み立てるだけ。native と同じコードが S3 の読み書き・署名・presign を
//! 担うので、経路ごとの差異が生まれない。
//!
//! 値が欠けていれば失敗させる (fail-closed)。片肺の設定で D1 へ黙って
//! フォールバックさせない。

use std::sync::Arc;

use narou_rs::platform::{S3Store, S3StoreConfig, SystemClock};
use worker::Env;

/// 環境から S3 ストアを組み立てる。
///
/// `subrequests` は呼び出し元の実行単位で共有する (S3 への読み書きも
/// Worker の subrequest 予算を消費するため、別カウンタを持たせない)。
pub async fn store_from_env(
    env: &Env,
    subrequests: crate::budget::SubrequestBudget,
) -> worker::Result<Arc<S3Store>> {
    let config = S3StoreConfig {
        endpoint: crate::secrets::require(env, "S3_ENDPOINT").await?,
        bucket: crate::secrets::require(env, "S3_BUCKET").await?,
        region: crate::secrets::require(env, "S3_REGION").await?,
        prefix: crate::secrets::value(env, "S3_PREFIX").await.unwrap_or_default(),
        access_key_id: crate::secrets::require(env, "S3_ACCESS_KEY_ID").await?,
        secret_access_key: crate::secrets::require(env, "S3_SECRET_ACCESS_KEY").await?,
    };
    let http = Arc::new(crate::http::WorkerHttpClient::new(subrequests));
    S3Store::new(config, http, Arc::new(SystemClock))
        .map(Arc::new)
        .map_err(|error| worker::Error::RustError(error.to_string()))
}
