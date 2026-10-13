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
use crate::login::actions;
use crate::native::cookie_store::InventoryCookieStore;
use crate::platform::cookie_store::login_status_json;

use super::AppState;
use super::{MAX_WEB_TEXT_INPUT_BYTES, validate_web_text_size};

#[derive(Debug, Deserialize)]
pub struct LoginImportRequest {
    /// Export envelope text (YAML) written by `narou_rs_login`.
    pub envelope: String,
    /// Passphrase of an encrypted export.
    #[serde(default)]
    pub passphrase: Option<String>,
    /// Drop sites that are not part of the import.
    #[serde(default)]
    pub replace: bool,
    /// Name the logins this file brings in ("本垢", "サブ垢", …).
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /api/login/rename` — name one stored login.
#[derive(Debug, Deserialize)]
pub struct LoginRenameRequest {
    /// Site the login belongs to (`www.pixiv.net`).
    pub site: String,
    /// Position in the list (0 始まり).
    pub index: usize,
    /// Name to show ("Pixiv1", "メインアカウント", …). Empty clears it.
    #[serde(default)]
    pub label: String,
}

/// `POST /api/login/order` — reorder a site's logins.
#[derive(Debug, Deserialize)]
pub struct LoginOrderRequest {
    /// Site whose logins are being reordered.
    pub site: String,
    /// Current indices in their new order, e.g. `[1, 0]`.
    pub order: Vec<usize>,
}

/// `GET /api/login` — stored sites with their logins (values masked).
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
    if let Err(message) =
        validate_web_text_size(&body.envelope, MAX_WEB_TEXT_INPUT_BYTES, "login export")
    {
        return Json(serde_json::json!({ "success": false, "message": message }));
    }
    let plan = match actions::plan_import(
        &body.envelope,
        body.passphrase.as_deref(),
        body.name.as_deref(),
    ) {
        Ok(plan) => plan,
        Err(error) => return Json(failure(error)),
    };
    if plan.logins == 0 {
        return Json(serde_json::json!({
            "success": false,
            "message": actions::IMPORT_EMPTY_MESSAGE,
        }));
    }
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    match actions::import_logins(&store, &plan, body.replace).await {
        Ok(message) => Json(serde_json::json!({
            "success": true,
            "message": message,
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Err(message) => Json(serde_json::json!({ "success": false, "message": message })),
    }
}

/// `POST /api/login/rename` — name one login.
pub async fn login_rename(
    State(_state): State<AppState>,
    Json(body): Json<LoginRenameRequest>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    match actions::rename_login(&store, &body.site, body.index, &body.label).await {
        Ok(message) => Json(serde_json::json!({
            "success": true,
            "message": message,
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Err(message) => Json(serde_json::json!({ "success": false, "message": message })),
    }
}

/// `POST /api/login/order` — reorder a site's logins.
pub async fn login_order(
    State(_state): State<AppState>,
    Json(body): Json<LoginOrderRequest>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    match actions::reorder_logins(&store, &body.site, &body.order).await {
        Ok(message) => Json(serde_json::json!({
            "success": true,
            "message": message,
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Err(message) => Json(serde_json::json!({ "success": false, "message": message })),
    }
}

/// `DELETE /api/login/{site}` — drop one site.
pub async fn login_clear_site(
    State(_state): State<AppState>,
    Path(site): Path<String>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    match actions::clear_site_logins(&store, &site).await {
        Ok(message) => Json(serde_json::json!({
            "success": true,
            "message": message,
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Err(message) => Json(serde_json::json!({ "success": false, "message": message })),
    }
}

/// `DELETE /api/login/{site}/{index}` — drop one login of a site.
pub async fn login_clear_group(
    State(_state): State<AppState>,
    Path((site, index)): Path<(String, usize)>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    match actions::clear_login(&store, &site, index).await {
        Ok(message) => Json(serde_json::json!({
            "success": true,
            "message": message,
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Err(message) => Json(serde_json::json!({ "success": false, "message": message })),
    }
}

/// `DELETE /api/login` — drop every site.
pub async fn login_clear_all(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    match actions::clear_all_logins(&store).await {
        Ok(message) => Json(serde_json::json!({
            "success": true,
            "message": message,
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Err(message) => Json(serde_json::json!({ "success": false, "message": message })),
    }
}

fn store() -> Result<InventoryCookieStore, NarouError> {
    InventoryCookieStore::for_current_root()
}

/// Stored sites and their logins, with every cookie value masked.
fn status_payload() -> Result<serde_json::Value, NarouError> {
    let store = store()?;
    let (sites, encrypted) = store.groups_and_encryption()?;
    let key_source = store
        .key_source()
        .map(|source| source.describe())
        .unwrap_or("unknown");
    Ok(login_status_json(&sites, &encrypted, key_source))
}

fn failure(error: NarouError) -> serde_json::Value {
    serde_json::json!({ "success": false, "message": error.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_values_and_lists_the_cookie_names() {
        let masked = crate::platform::mask_cookie("over18=yes; ses=abcdef");
        assert_eq!(masked, "over18=…; ses=… (2 件, 22 文字)");
        assert!(!masked.contains("abcdef"));
        assert_eq!(crate::platform::mask_cookie(""), "(0 文字)");
    }

    #[test]
    fn import_request_accepts_a_missing_passphrase() {
        let body: LoginImportRequest =
            serde_json::from_str(r#"{"envelope":"version: 1"}"#).unwrap();
        assert!(body.passphrase.is_none());
        assert!(!body.replace);
    }
}
