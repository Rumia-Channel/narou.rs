//! ログイン資格情報の管理 API。
//!
//! native Web UI の `/api/login*` と同じ形 (`{success, data|message}`) を返す。
//! 値は伏せて表示し、取り込みは native の書き出し形式 (YAML) を受け付ける。

use narou_rs::error::Result;
use narou_rs::platform::{CookieStore, LoginCredential, mask_cookie, parse_cookie_header};
use serde::Deserialize;
use worker::{Request, Response};

use crate::composition::WorkerRuntime;

/// `POST /api/login/set` の本文。
#[derive(Debug, Deserialize)]
struct SetRequest {
    host: String,
    cookie: String,
    #[serde(default)]
    label: Option<String>,
}

/// 一覧。値は名前と長さだけを返す（Cookie 本体は返さない）。
pub async fn status(runtime: &WorkerRuntime) -> Result<Response> {
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
    Ok(Response::from_json(&serde_json::json!({
        "success": true,
        "data": {
            "hosts": hosts,
            "count": cookies.len(),
            "credentials": total,
            "key_source": store.key_source(),
        },
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}

/// 1 ホスト分を置き換える。
pub async fn set(runtime: &WorkerRuntime, mut request: Request) -> Result<Response> {
    let body: SetRequest = match request.json().await {
        Ok(body) => body,
        Err(_) => return json_error("invalid JSON body"),
    };
    if body.host.trim().is_empty() || body.cookie.trim().is_empty() {
        return json_error("host and cookie are required");
    }
    let credential = LoginCredential::new(body.host.clone(), body.cookie.clone())
        .with_label(body.label.clone());
    if let Err(error) = runtime
        .cookie_store()
        .save_all(&body.host, &[credential])
        .await
    {
        return json_error(&error.to_string());
    }
    status(runtime).await
}

/// 1 ホスト分を削除する。
pub async fn clear(runtime: &WorkerRuntime, host: &str) -> Result<Response> {
    runtime.cookie_store().clear(host).await?;
    status(runtime).await
}

fn json_error(message: &str) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({
        "success": false,
        "message": message,
    }))
    .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}
