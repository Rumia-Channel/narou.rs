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
use crate::platform::{LoginGroup, normalize_cookie_host, parse_cookie_header};

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
    let sites = match parse_export(&body.envelope, body.passphrase.as_deref()) {
        Ok(sites) => sites,
        Err(error) => return Json(failure(error)),
    };
    let logins: usize = sites.values().map(Vec::len).sum();
    if logins == 0 {
        return Json(serde_json::json!({
            "success": false,
            "message": "書き出しファイルにログイン情報が含まれていません",
        }));
    }
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let site_count = sites.len();
    let stored = match if body.replace {
        store.replace_groups(&sites)
    } else {
        store.merge_groups(&sites)
    } {
        Ok(stored) => stored,
        Err(error) => return Json(failure(error)),
    };
    Json(serde_json::json!({
        "success": true,
        "message": format!("{logins} 件を取り込みました ({site_count} サイト / 保存済み {stored} サイト)"),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
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
    let site = normalize_cookie_host(&body.site);
    let mut groups = match store.groups_for(&site) {
        Ok(groups) => groups,
        Err(error) => return Json(failure(error)),
    };
    if body.index >= groups.len() {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("{site} の {} 番目のログイン情報はありません", body.index + 1),
        }));
    }
    let label = body.label.trim();
    groups[body.index].label = (!label.is_empty()).then(|| label.to_string());
    let name = groups[body.index].display_name().to_string();
    if let Err(error) = store.save_groups_for(&site, &groups) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{site} の {} 番目を「{name}」にしました", body.index + 1),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
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
    let site = normalize_cookie_host(&body.site);
    let stored = match store.groups_for(&site) {
        Ok(stored) => stored,
        Err(error) => return Json(failure(error)),
    };
    let reordered = match reorder_groups(&stored, &body.order) {
        Ok(reordered) => reordered,
        Err(message) => {
            return Json(serde_json::json!({ "success": false, "message": message }));
        }
    };
    if let Err(error) = store.save_groups_for(&site, &reordered) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{site} の試行順を変更しました"),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

/// 並び順の指定（現在の添字の並び）を検証して適用する。
fn reorder_groups(
    stored: &[LoginGroup],
    order: &[usize],
) -> std::result::Result<Vec<LoginGroup>, String> {
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

/// `DELETE /api/login/{site}` — drop one site.
pub async fn login_clear_site(
    State(_state): State<AppState>,
    Path(site): Path<String>,
) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let site = normalize_cookie_host(&site);
    match store.remove(&site) {
        Ok(true) => Json(serde_json::json!({
            "success": true,
            "message": format!("{site} のログイン情報を削除しました"),
            "data": status_payload().unwrap_or(serde_json::Value::Null),
        })),
        Ok(false) => Json(serde_json::json!({
            "success": false,
            "message": format!("{site} のログイン情報は保存されていません"),
        })),
        Err(error) => Json(failure(error)),
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
    let site = normalize_cookie_host(&site);
    let mut groups = match store.groups_for(&site) {
        Ok(groups) => groups,
        Err(error) => return Json(failure(error)),
    };
    if index >= groups.len() {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("{site} の {} 番目のログイン情報はありません", index + 1),
        }));
    }
    let removed = groups.remove(index);
    if let Err(error) = store.save_groups_for(&site, &groups) {
        return Json(failure(error));
    }
    Json(serde_json::json!({
        "success": true,
        "message": format!("{site} の「{}」を削除しました", removed.display_name()),
        "data": status_payload().unwrap_or(serde_json::Value::Null),
    }))
}

/// `DELETE /api/login` — drop every site.
pub async fn login_clear_all(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let store = match store() {
        Ok(store) => store,
        Err(error) => return Json(failure(error)),
    };
    let removed = store.groups_by_site().map(|sites| sites.len()).unwrap_or(0);
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

/// Stored sites and their logins, with every cookie value masked.
fn status_payload() -> Result<serde_json::Value, NarouError> {
    let store = store()?;
    let sites = store.groups_by_site()?;
    let mut total = 0;
    let entries = sites
        .iter()
        .map(|(site, groups)| {
            total += groups.len();
            let logins = groups
                .iter()
                .enumerate()
                .map(|(index, group)| {
                    let hosts = group
                        .cookies
                        .iter()
                        .map(|entry| {
                            serde_json::json!({
                                "host": entry.host,
                                "cookies": mask_cookie(&entry.cookie),
                                "names": parse_cookie_header(&entry.cookie)
                                    .into_iter()
                                    .map(|(name, _)| name)
                                    .collect::<Vec<_>>(),
                                "length": entry.cookie.chars().count(),
                            })
                        })
                        .collect::<Vec<_>>();
                    serde_json::json!({
                        "index": index,
                        "id": group.id,
                        "short_id": group.short_id(),
                        "label": group.label,
                        "display_name": group.display_name(),
                        "hosts": hosts,
                        "host_count": group.cookies.len(),
                        "added_at": group.added_at,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "site": site,
                "logins": logins,
                "encrypted": store.is_encrypted(site).unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "sites": entries,
        "sites_count": sites.len(),
        "logins_count": total,
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
