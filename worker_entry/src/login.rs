//! ログイン資格情報の管理 API。
//!
//! native Web UI の `/api/login*` (`src/web/login.rs`) と同じ形
//! (`{success, message?, data?}`) を返す。値は伏せて表示し、取り込みは
//! native の書き出し形式 (YAML) を受け付ける。応答メッセージとアクションの
//! 判断は native と共有する `narou_rs::login::actions` が持ち、一覧ペイロード
//! も `narou_rs::platform::cookie_store::login_status_json` を共有する。
//! このモジュールは D1 ストアの読み書きと応答の組み立てだけを行う。

use narou_rs::error::Result;
use narou_rs::login::actions;
use narou_rs::platform::cookie_store::login_status_json;
use worker::Response;

use crate::composition::WorkerRuntime;

/// 一覧ペイロード (native `status_payload`)。値は名前と長さだけを返す
/// (Cookie 本体は返さない)。
///
/// 保存 payload は 1 回だけ読み、サイトごとの暗号化フラグは復号前の生値
/// からその場で判定する — サイト数ぶん保存行を読み直さない。
pub async fn status_payload(runtime: &WorkerRuntime) -> Result<serde_json::Value> {
    let store = runtime.cookie_store();
    let (sites, encrypted) = store.groups_and_encryption().await?;
    Ok(login_status_json(&sites, &encrypted, store.key_source()))
}

/// `GET /api/login` — `{success: true, data: <status_payload>}` (native
/// `login_status`)。
pub async fn status(runtime: &WorkerRuntime) -> Result<Response> {
    let data = status_payload(runtime).await?;
    Response::from_json(&serde_json::json!({
        "success": true,
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))
}

/// `DELETE /api/login/{site}` — native `login_clear_site`。
pub async fn clear_site(runtime: &WorkerRuntime, site: &str) -> Result<Response> {
    let store = runtime.cookie_store();
    let outcome = actions::clear_site_logins(store.as_ref(), site).await;
    action_response(runtime, outcome).await
}

/// `DELETE /api/login/{site}/{index}` — native `login_clear_group`。
///
/// 試行順リストから 1 件だけ抜いて保存し直す。
pub async fn clear_group(runtime: &WorkerRuntime, site: &str, index: usize) -> Result<Response> {
    let store = runtime.cookie_store();
    let outcome = actions::clear_login(store.as_ref(), site, index).await;
    action_response(runtime, outcome).await
}

/// `DELETE /api/login` — native `login_clear_all`。全サイトを消す。
pub async fn clear_all(runtime: &WorkerRuntime) -> Result<Response> {
    let store = runtime.cookie_store();
    let outcome = actions::clear_all_logins(store.as_ref()).await;
    action_response(runtime, outcome).await
}

/// アクションの結果を native と同じ応答形に包む。成功時は最新の
/// `status_payload` を `data` に載せる。
async fn action_response(
    runtime: &WorkerRuntime,
    outcome: std::result::Result<String, String>,
) -> Result<Response> {
    match outcome {
        Ok(message) => {
            let data = status_payload(runtime)
                .await
                .unwrap_or(serde_json::Value::Null);
            Response::from_json(&serde_json::json!({
                "success": true,
                "message": message,
                "data": data,
            }))
            .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))
        }
        Err(message) => json_error(&message),
    }
}

/// native `failure()`: HTTP 200 + `{success: false, message}`。
fn json_error(message: &str) -> Result<Response> {
    Response::from_json(&serde_json::json!({
        "success": false,
        "message": message,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))
}
