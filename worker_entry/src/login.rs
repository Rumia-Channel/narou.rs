//! ログイン資格情報の管理 API。
//!
//! native Web UI の `/api/login*` (`src/web/login.rs`) と同じ形
//! (`{success, message?, data?}`) を返す。値は伏せて表示し、取り込みは
//! native の書き出し形式 (YAML) を受け付ける。応答メッセージは native の
//! 日本語文言に揃える。資格情報はサイト単位の `LoginGroup` で保存する。

use narou_rs::error::Result;
use narou_rs::platform::normalize_cookie_host;
use worker::Response;

use crate::composition::WorkerRuntime;

/// 資格情報一覧の整形 (平文を出さない規則) はホストテスト対象の純関数。
pub(crate) mod listing;

/// 一覧ペイロード (native `status_payload`)。値は名前と長さだけを返す
/// (Cookie 本体は返さない)。
///
/// 保存 payload は 1 回だけ読み、サイトごとの暗号化フラグは復号前の生値
/// からその場で判定する — サイト数ぶん保存行を読み直さない。
pub async fn status_payload(runtime: &WorkerRuntime) -> Result<serde_json::Value> {
    let store = runtime.cookie_store();
    let (sites, encrypted) = store.groups_and_encryption().await?;
    Ok(listing::status_json(&sites, &encrypted, store.key_source()))
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

/// `DELETE /api/login/{site}` — native `login_clear_site`。サイト名と
/// 完全一致するエントリだけを消し、存在しなければ `success:false`。
pub async fn clear_site(runtime: &WorkerRuntime, site: &str) -> Result<Response> {
    let store = runtime.cookie_store();
    let site = normalize_cookie_host(site);
    match store.remove(&site).await {
        Ok(true) => {
            let data = status_payload(runtime)
                .await
                .unwrap_or(serde_json::Value::Null);
            Ok(Response::from_json(&serde_json::json!({
                "success": true,
                "message": format!("{site} のログイン情報を削除しました"),
                "data": data,
            }))
            .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
        }
        Ok(false) => json_error(&format!("{site} のログイン情報は保存されていません")),
        Err(error) => json_error(&error.to_string()),
    }
}

/// `DELETE /api/login/{site}/{index}` — native `login_clear_group`。
/// 試行順リストから 1 件だけ抜いて保存し直す。
pub async fn clear_group(
    runtime: &WorkerRuntime,
    site: &str,
    index: usize,
) -> Result<Response> {
    let store = runtime.cookie_store();
    let site = normalize_cookie_host(site);
    let mut groups = match store.groups_for(&site).await {
        Ok(groups) => groups,
        Err(error) => return json_error(&error.to_string()),
    };
    if index >= groups.len() {
        return json_error(&format!(
            "{site} の {} 番目のログイン情報はありません",
            index + 1
        ));
    }
    let removed = groups.remove(index);
    if let Err(error) = store.save_groups_for(&site, &groups).await {
        return json_error(&error.to_string());
    }
    let data = status_payload(runtime)
        .await
        .unwrap_or(serde_json::Value::Null);
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{site} の「{}」を削除しました", removed.display_name()),
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))
}

/// `DELETE /api/login` — native `login_clear_all`。全サイトを消す。
pub async fn clear_all(runtime: &WorkerRuntime) -> Result<Response> {
    let store = runtime.cookie_store();
    let removed = store
        .groups_by_site()
        .await
        .map(|sites| sites.len())
        .unwrap_or(0);
    if let Err(error) = store.clear_all().await {
        return json_error(&error.to_string());
    }
    let data = status_payload(runtime)
        .await
        .unwrap_or(serde_json::Value::Null);
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("ログイン情報をすべて削除しました ({removed} サイト)"),
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))
}

/// native `failure()`: HTTP 200 + `{success: false, message}`。
fn json_error(message: &str) -> Result<Response> {
    Response::from_json(&serde_json::json!({
        "success": false,
        "message": message,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))
}
