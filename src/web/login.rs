//! Web API for login credentials captured on a browser machine.
//!
//! The browser half is the separate `narou_rs_login` executable: it runs where
//! the browser is, signs in, and writes an export envelope. This endpoint set is
//! the receiving end — the Web UI uploads that envelope (or pastes a cookie
//! header), and the credentials are stored encrypted at rest with the library
//! login key.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use crate::error::NarouError;
use crate::login::{group_credentials, parse_export};
use crate::native::cookie_store::InventoryCookieStore;
use crate::platform::{LoginCredential, normalize_cookie_host, parse_cookie_header};

use super::AppState;
use super::{MAX_WEB_TEXT_INPUT_BYTES, validate_web_text_size};

#[derive(Debug, Deserialize)]
pub struct LoginImportRequest {
    /// Export envelope text (YAML) written by `narou_rs_login`.
    pub envelope: String,
    /// Passphrase of an encrypted export.
    #[serde(default)]
    pub passphrase: Option<String>,
    /// Drop hosts that are not part of the import.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
pub struct LoginSetRequest {
    /// Request host, for example `ncode.syosetu.com`.
    pub host: String,
    /// `Cookie:` header value as copied from the browser.
    pub cookie: String,
    /// Optional label shown in the list ("メイン", "R18用", …).
    #[serde(default)]
    pub label: Option<String>,
}

/// `POST /api/login/order` — reorder a host's credentials.
#[derive(Debug, Deserialize)]
pub struct LoginOrderRequest {
    /// Request host whose credentials are being reordered.
    pub host: String,
    /// Current indices in their new order, e.g. `[1, 0, 2]`.
    pub order: Vec<usize>,
}

/// `GET /api/login` — stored hosts with their values masked.
pub async fn login_status(State(_state): State<AppState>) -> Json<serde_json::Value> {
    match status_payload() {
        Ok(data) => Json(serde_json::json!({ "success": true, "data": data })),
        Err(error) => Json(failure(error)),
    }
}

/// `POST /api/login/import` — import an export envelope.
pub async fn login_import(
    State(_state): State<AppState>,
    Json(body): Json<LoginImportRequest>,
) -> Json<serde_json::Value> {
    if let Err(message) = validate_web_text_size(
        &body.envelope,
        MAX_WEB_TEXT_INPUT_BYTES,
        "login export",
    ) {
        return Json(serde_json::json!({ "success": false, "message": message }));
    }
    let credentials = match parse_export(&body.envelope, body.passphrase.as_deref()) {
        Ok(credentials) => credentials,
        Err(error) => return Json(failure(error)),
    };
    if credentials.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "message": "書き出しファイルに Cookie が含まれていません",
        }));
    }
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let imported = credentials.len();
    let grouped = group_credentials(credentials);
    let stored = match if body.replace {
        store.replace_credentials(&grouped)
    } else {
        store.merge_credentials(&grouped)
    } {
        Ok(stored) => stored,
        Err(error) => return Json(failure(error)),
    };
    Json(serde_json::json!({
        "success": true,
        "message": format!("{imported} サイトを取り込みました (保存済み {stored} サイト)"),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

/// `POST /api/login/set` — replace one host's cookie header.
pub async fn login_set(
    State(_state): State<AppState>,
    Json(body): Json<LoginSetRequest>,
) -> Json<serde_json::Value> {
    login_set_or_add(body, false).await
}

/// 資格情報を 1 件保存する。`append` が真なら既存の後ろに足す。
async fn login_set_or_add(body: LoginSetRequest, append: bool) -> Json<serde_json::Value> {
    if let Err(message) =
        validate_web_text_size(&body.cookie, MAX_WEB_TEXT_INPUT_BYTES, "cookie header")
    {
        return Json(serde_json::json!({ "success": false, "message": message }));
    }
    if body.host.trim().is_empty() || body.cookie.trim().is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "message": "サイトと Cookie の両方を入力してください",
        }));
    }
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let host = normalize_cookie_host(&body.host);
    let credential = LoginCredential::new(host.clone(), body.cookie.trim())
        .with_label(body.label)
        .with_added_at(Some(chrono::Local::now().to_rfc3339()));
    let stored = match store.credentials_for(&host) {
        Ok(stored) => stored,
        Err(error) => return Json(failure(error)),
    };
    let credentials = if append {
        let mut credentials = stored;
        credentials.push(credential);
        credentials
    } else {
        vec![credential]
    };
    if let Err(error) = store.save_credentials_for(&host, &credentials) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{host} のログイン情報を保存しました ({} 件)", credentials.len()),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

/// `POST /api/login/add` — append one credential to a host's list.
pub async fn login_add(
    State(_state): State<AppState>,
    Json(body): Json<LoginSetRequest>,
) -> Json<serde_json::Value> {
    login_set_or_add(body, true).await
}

/// `POST /api/login/{host}/{index}/order` 用の並べ替え。
pub async fn login_order(
    State(_state): State<AppState>,
    Json(body): Json<LoginOrderRequest>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let host = normalize_cookie_host(&body.host);
    let stored = match store.credentials_for(&host) {
        Ok(stored) => stored,
        Err(error) => return Json(failure(error)),
    };
    let reordered = match reorder_credentials(&stored, &body.order) {
        Ok(reordered) => reordered,
        Err(message) => {
            return Json(serde_json::json!({ "success": false, "message": message }));
        }
    };
    if let Err(error) = store.save_credentials_for(&host, &reordered) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{host} の試行順を変更しました"),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

/// 並び順の指定（現在の添字の並び）を検証して適用する。
fn reorder_credentials(
    stored: &[LoginCredential],
    order: &[usize],
) -> std::result::Result<Vec<LoginCredential>, String> {
    if order.len() != stored.len() {
        return Err("並び順の指定が保存済みの件数と一致しません".to_string());
    }
    let mut seen = vec![false; stored.len()];
    let mut reordered = Vec::with_capacity(stored.len());
    for index in order {
        if *index >= stored.len() || seen[*index] {
            return Err("並び順の指定が不正です".to_string());
        }
        seen[*index] = true;
        reordered.push(stored[*index].clone());
    }
    Ok(reordered)
}

/// `DELETE /api/login/{host}` — drop one host.
pub async fn login_clear_host(
    State(_state): State<AppState>,
    Path(host): Path<String>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let host = normalize_cookie_host(&host);
    match store.remove(&host) {
        Ok(true) => Json(serde_json::json!({
            "success": true,
            "message": format!("{host} のログイン情報を削除しました"),
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Ok(false) => Json(serde_json::json!({
            "success": false,
            "message": format!("{host} のログイン情報は保存されていません"),
        })),
        Err(error) => Json(failure(error)),
    }
}

/// `DELETE /api/login/{host}/{index}` — drop one credential of a host.
pub async fn login_clear_credential(
    State(_state): State<AppState>,
    Path((host, index)): Path<(String, usize)>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let host = normalize_cookie_host(&host);
    let mut credentials = match store.credentials_for(&host) {
        Ok(credentials) => credentials,
        Err(error) => return Json(failure(error)),
    };
    if index >= credentials.len() {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("{host} の {} 番目のログイン情報はありません", index + 1),
        }));
    }
    credentials.remove(index);
    if let Err(error) = store.save_credentials_for(&host, &credentials) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{host} の {} 番目のログイン情報を削除しました", index + 1),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

/// `DELETE /api/login` — drop every host.
pub async fn login_clear_all(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let removed = store
        .credentials_by_host()
        .map(|cookies| cookies.len())
        .unwrap_or(0);
    if let Err(error) = store.clear_all() {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("ログイン情報をすべて削除しました ({removed} サイト)"),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

fn store() -> Result<InventoryCookieStore, NarouError> {
    InventoryCookieStore::for_current_root()
}

/// Stored hosts and their key source, with every cookie value masked.
fn status_payload() -> Result<serde_json::Value, NarouError> {
    let store = store()?;
    let cookies = store.credentials_by_host()?;
    let mut total = 0;
    let hosts = cookies
        .iter()
        .map(|(host, credentials)| {
            total += credentials.len();
            let entries = credentials
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
                .collect::<Vec<_>>();
            serde_json::json!({
                "host": host,
                "credentials": entries,
                "encrypted": store.is_encrypted(host).unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "hosts": hosts,
        "count": cookies.len(),
        "credentials": total,
        "key_source": store.key_source().map(|source| source.describe()).unwrap_or("unknown"),
    }))
}

/// Show which cookies a header carries without exposing their values.
fn mask_cookie(cookie: &str) -> String {
    let pairs = parse_cookie_header(cookie);
    let length = cookie.chars().count();
    if pairs.is_empty() {
        return format!("({length} 文字)");
    }
    let names = pairs
        .iter()
        .map(|(name, _)| format!("{name}=…"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("{names} ({} 件, {length} 文字)", pairs.len())
}

fn failure(error: NarouError) -> serde_json::Value {
    serde_json::json!({ "success": false, "message": error.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_values_and_lists_the_cookie_names() {
        let masked = mask_cookie("over18=yes; ses=abcdef");
        assert_eq!(masked, "over18=…; ses=… (2 件, 22 文字)");
        assert!(!masked.contains("abcdef"));
        assert_eq!(mask_cookie(""), "(0 文字)");
    }

    #[test]
    fn import_request_accepts_a_missing_passphrase() {
        let body: LoginImportRequest =
            serde_json::from_str(r#"{"envelope":"version: 1"}"#).unwrap();
        assert!(body.passphrase.is_none());
        assert!(!body.replace);
    }
}
