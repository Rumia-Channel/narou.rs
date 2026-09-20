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
use crate::login::parse_export;
use crate::native::cookie_store::InventoryCookieStore;
use crate::platform::{normalize_cookie_host, parse_cookie_header};

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
    let cookies = match parse_export(&body.envelope, body.passphrase.as_deref()) {
        Ok(cookies) => cookies,
        Err(error) => return Json(failure(error)),
    };
    if cookies.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "message": "書き出しファイルに Cookie が含まれていません",
        }));
    }
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let imported = cookies.len();
    let stored = match if body.replace {
        store.replace_all(&cookies)
    } else {
        store.merge(&cookies)
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

/// `POST /api/login/set` — store one host's cookie header.
pub async fn login_set(
    State(_state): State<AppState>,
    Json(body): Json<LoginSetRequest>,
) -> Json<serde_json::Value> {
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
    let cookies = std::collections::BTreeMap::from([(host.clone(), body.cookie.trim().to_string())]);
    if let Err(error) = store.merge(&cookies) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{host} のログイン情報を保存しました"),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
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

/// `DELETE /api/login` — drop every host.
pub async fn login_clear_all(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let removed = store.load_all().map(|cookies| cookies.len()).unwrap_or(0);
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
    let cookies = store.load_all()?;
    let hosts = cookies
        .iter()
        .map(|(host, cookie)| {
            serde_json::json!({
                "host": host,
                "cookies": mask_cookie(cookie),
                "names": parse_cookie_header(cookie)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
                "encrypted": store.is_encrypted(host).unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "hosts": hosts,
        "count": cookies.len(),
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
