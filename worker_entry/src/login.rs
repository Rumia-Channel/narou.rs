//! ログイン資格情報の管理 API。
//!
//! native Web UI の `/api/login*` (`src/web/login.rs`) と同じ形
//! (`{success, message?, data?}`) を返す。値は伏せて表示し、取り込みは
//! native の書き出し形式 (YAML) を受け付ける。応答メッセージは native の
//! 日本語文言に揃える。

use narou_rs::application::settings_view::{MAX_WEB_TEXT_INPUT_BYTES, validate_web_text_size};
use narou_rs::error::Result;
use narou_rs::platform::{
    CookieStore, LoginCredential, mask_cookie, normalize_cookie_host, parse_cookie_header,
};
use serde::Deserialize;
use worker::{Request, Response};

use crate::composition::WorkerRuntime;

/// `POST /api/login/set` / `POST /api/login/add` の本文
/// (native `LoginSetRequest`)。
#[derive(Debug, Deserialize)]
struct SetRequest {
    host: String,
    cookie: String,
    #[serde(default)]
    label: Option<String>,
}

/// 一覧ペイロード (native `status_payload`)。値は名前と長さだけを返す
/// (Cookie 本体は返さない)。
pub async fn status_payload(runtime: &WorkerRuntime) -> Result<serde_json::Value> {
    let store = runtime.cookie_store();
    let cookies = store.list().await?;
    let mut total = 0usize;
    let mut encrypted_hosts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for host in cookies.keys() {
        if store.is_encrypted(host).await.unwrap_or(false) {
            encrypted_hosts.insert(host.clone());
        }
    }
    let hosts: Vec<serde_json::Value> = cookies
        .iter()
        .map(|(host, credentials)| {
            total += credentials.len();
            let entries: Vec<serde_json::Value> = credentials
                .iter()
                .enumerate()
                .map(|(index, credential)| {
                    serde_json::json!({
                        "index": index,
                        "id": credential.id,
                        "short_id": credential.short_id(),
                        "label": credential.label,
                        "host": credential.host,
                        "cookies": mask_cookie(&credential.cookie),
                        "names": parse_cookie_header(&credential.cookie)
                            .into_iter()
                            .map(|(name, _)| name)
                            .collect::<Vec<_>>(),
                        "length": credential.cookie.chars().count(),
                        "added_at": credential.added_at,
                    })
                })
                .collect();
            serde_json::json!({
                "host": host,
                "credentials": entries,
                "encrypted": encrypted_hosts.contains(host),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "hosts": hosts,
        "count": cookies.len(),
        "credentials": total,
        "key_source": store.key_source(),
    }))
}

/// `GET /api/login` — `{success: true, data: <status_payload>}` (native
/// `login_status`)。
pub async fn status(runtime: &WorkerRuntime) -> Result<Response> {
    let data = status_payload(runtime).await?;
    Ok(Response::from_json(&serde_json::json!({
        "success": true,
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}

/// `POST /api/login/set` / `POST /api/login/add` — native `login_set_or_add`。
/// `append` が真なら既存の資格情報の後ろに足す。
pub async fn set_or_add(
    runtime: &WorkerRuntime,
    mut request: Request,
    append: bool,
) -> Result<Response> {
    let body: SetRequest = match request.json().await {
        Ok(body) => body,
        Err(_) => return json_error("invalid JSON body"),
    };
    if let Err(message) =
        validate_web_text_size(&body.cookie, MAX_WEB_TEXT_INPUT_BYTES, "cookie header")
    {
        return json_error(&message);
    }
    if body.host.trim().is_empty() || body.cookie.trim().is_empty() {
        return json_error("サイトと Cookie の両方を入力してください");
    }
    let store = runtime.cookie_store();
    let host = normalize_cookie_host(&body.host);
    let credential = LoginCredential::new(host.clone(), body.cookie.trim())
        .with_label(body.label)
        // native は `Local::now()`。Worker のタイムゾーンは UTC 固定なので
        // 同じ RFC3339 文字列を UTC 時刻で入れる。
        .with_added_at(Some(runtime.now_rfc3339()));
    // `load_all` は `merge_credentials_for` 経由なので、ネイティブの
    // `credentials_for` と同じく親ドメインのエントリも畳み込む。
    let stored = store.load_all(&host).await?;
    let credentials = if append {
        let mut credentials = stored;
        credentials.push(credential);
        credentials
    } else {
        vec![credential]
    };
    let count = credentials.len();
    if let Err(error) = store.save_all(&host, &credentials).await {
        return json_error(&error.to_string());
    }
    let data = status_payload(runtime)
        .await
        .unwrap_or(serde_json::Value::Null);
    Ok(Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{host} のログイン情報を保存しました ({count} 件)"),
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}

/// `DELETE /api/login/{host}` — native `login_clear_host`。ホスト名と
/// 完全一致するエントリだけを消し、存在しなければ `success:false`。
pub async fn clear_host(runtime: &WorkerRuntime, host: &str) -> Result<Response> {
    let store = runtime.cookie_store();
    let host = normalize_cookie_host(host);
    match store.remove(&host).await {
        Ok(true) => {
            let data = status_payload(runtime)
                .await
                .unwrap_or(serde_json::Value::Null);
            Ok(Response::from_json(&serde_json::json!({
                "success": true,
                "message": format!("{host} のログイン情報を削除しました"),
                "data": data,
            }))
            .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
        }
        Ok(false) => json_error(&format!("{host} のログイン情報は保存されていません")),
        Err(error) => json_error(&error.to_string()),
    }
}

/// `DELETE /api/login/{host}/{index}` — native `login_clear_credential`。
/// 試行順リストから 1 件だけ抜いて保存し直す。
pub async fn clear_credential(
    runtime: &WorkerRuntime,
    host: &str,
    index: usize,
) -> Result<Response> {
    let store = runtime.cookie_store();
    let host = normalize_cookie_host(host);
    let mut credentials = match store.load_all(&host).await {
        Ok(credentials) => credentials,
        Err(error) => return json_error(&error.to_string()),
    };
    if index >= credentials.len() {
        return json_error(&format!(
            "{host} の {} 番目のログイン情報はありません",
            index + 1
        ));
    }
    credentials.remove(index);
    if let Err(error) = store.save_all(&host, &credentials).await {
        return json_error(&error.to_string());
    }
    let data = status_payload(runtime)
        .await
        .unwrap_or(serde_json::Value::Null);
    Ok(Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{host} の {} 番目のログイン情報を削除しました", index + 1),
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}

/// `DELETE /api/login` — native `login_clear_all`。全ホストを消す。
pub async fn clear_all(runtime: &WorkerRuntime) -> Result<Response> {
    let store = runtime.cookie_store();
    let removed = store.list().await.map(|cookies| cookies.len()).unwrap_or(0);
    if let Err(error) = store.clear_all().await {
        return json_error(&error.to_string());
    }
    let data = status_payload(runtime)
        .await
        .unwrap_or(serde_json::Value::Null);
    Ok(Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("ログイン情報をすべて削除しました ({removed} サイト)"),
        "data": data,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}

/// native `failure()`: HTTP 200 + `{success: false, message}`。
fn json_error(message: &str) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({
        "success": false,
        "message": message,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}
