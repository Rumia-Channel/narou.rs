//! Web UI のログイン資格情報アクション (native: `src/web/login.rs`)。
//!
//! - `POST /api/login/import` — `narou_rs_login` が書き出したエクスポート
//!   エンベロープ (YAML) を取り込む。`{success, message, data}` の応答形と
//!   エラーメッセージは native `login_import` に揃える。`name` でその
//!   ファイルが持ち込むログインに名前を付ける。
//! - `POST /api/login/rename` — 1 件のログインに名前を付ける。
//! - `POST /api/login/order` — 1 サイトのログインの試行順を並べ替える。
//!   native `login_order` と同じく、現在の添字の並び順を受け取る。
//!
//! native との差異:
//! - エンベロープの解析・復号 (Argon2id → XChaCha20-Poly1305) は native と同じ
//!   `narou_rs::login::{parse_export, apply_import_name}` を使うので、暗号化
//!   エクスポートもエラー文言も native と一致する。
//! - 保存は `D1CookieStore` (`app_state` の `login_cookie` 行) 経由。値は
//!   `enc:v1:` で暗号化して書く (native と同じ形式)。復号鍵の secret
//!   `NAROU_RS_LOGIN_KEY` が無いときは平文で書かず失敗する (fail-closed)。
//! - native は資格情報変更を push_server でブロードキャストするが、Worker
//!   には同等の経路が無いので通知は行わない。

use narou_rs::application::settings_view::{MAX_WEB_TEXT_INPUT_BYTES, validate_web_text_size};
use narou_rs::login::{apply_import_name, parse_export};
use narou_rs::platform::{LoginGroup, normalize_cookie_host};
use serde::Deserialize;
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

use super::json_error;

/// `POST /api/login/import` の本文 (native `LoginImportRequest`)。
#[derive(Debug, Deserialize)]
struct ImportBody {
    /// `narou_rs_login` が書き出したエクスポート文書 (YAML)。
    envelope: String,
    /// 暗号化エクスポートのパスフレーズ。
    #[serde(default)]
    passphrase: Option<String>,
    /// 取り込みに含まれないサイトを消すか。
    #[serde(default)]
    replace: bool,
    /// このファイルが持ち込むログインにつける名前 ("本垢", "サブ垢", …)。
    #[serde(default)]
    name: Option<String>,
}

/// `POST /api/login/rename` の本文 (native `LoginRenameRequest`)。
#[derive(Debug, Deserialize)]
struct RenameBody {
    /// ログインが属するサイト (`www.pixiv.net`)。
    site: String,
    /// リスト内の位置 (0 始まり)。
    index: usize,
    /// 表示名。空文字は消す。
    #[serde(default)]
    label: String,
}

/// `POST /api/login/order` の本文 (native `LoginOrderRequest`)。
#[derive(Debug, Deserialize)]
struct OrderBody {
    /// 並べ替えるログインを持つサイト。
    site: String,
    /// 新しい順序で並べた現在の添字 (例: `[1, 0, 2]`)。
    order: Vec<usize>,
}

/// 親がこの 1 ハンドラを各ルートへ割り当てる。パスとメソッドで振り分ける
/// (`/api/login/` の prefix ガードより先に置く前提)。
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let path = req.path();
    enum Route {
        Import,
        Rename,
        Order,
    }
    let route = match (req.method(), path.as_str()) {
        (Method::Post, "/api/login/import") => Route::Import,
        (Method::Post, "/api/login/rename") => Route::Rename,
        (Method::Post, "/api/login/order") => Route::Order,
        (_, "/api/login/import" | "/api/login/rename" | "/api/login/order") => {
            return json_error(405, "method_not_allowed", None);
        }
        _ => {
            return json_error(
                404,
                "not_found",
                Some("route is not handled by this Worker"),
            );
        }
    };
    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    match route {
        Route::Import => login_import(&mut req, &runtime).await,
        Route::Rename => login_rename(&mut req, &runtime).await,
        Route::Order => login_order(&mut req, &runtime).await,
    }
}

// ---------------------------------------------------------------------------
// POST /api/login/import (native: src/web/login.rs login_import)
// ---------------------------------------------------------------------------

async fn login_import(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    // axum `Json` と同じく、本文が壊れていればここで 400。
    let body: ImportBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    if let Err(message) =
        validate_web_text_size(&body.envelope, MAX_WEB_TEXT_INPUT_BYTES, "login export")
    {
        return api_failure(&message);
    }
    let mut sites = match parse_export(&body.envelope, body.passphrase.as_deref()) {
        Ok(sites) => sites,
        Err(error) => return api_failure(&error.to_string()),
    };
    if let Some(name) = body.name.as_deref() {
        apply_import_name(&mut sites, name);
    }
    let logins: usize = sites.values().map(Vec::len).sum();
    if logins == 0 {
        return api_failure("書き出しファイルにログイン情報が含まれていません");
    }
    let store = runtime.cookie_store();
    let site_count = sites.len();
    let stored = match if body.replace {
        store.replace_groups(&sites).await
    } else {
        store.merge_groups(&sites).await
    } {
        Ok(stored) => stored,
        Err(error) => return api_failure(&error.to_string()),
    };
    let data = status_data(runtime).await;
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{logins} 件を取り込みました ({site_count} サイト / 保存済み {stored} サイト)"),
        "data": data,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/login/rename (native: src/web/login.rs login_rename)
// ---------------------------------------------------------------------------

async fn login_rename(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let body: RenameBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let store = runtime.cookie_store();
    let site = normalize_cookie_host(&body.site);
    let mut groups = match store.groups_for(&site).await {
        Ok(groups) => groups,
        Err(error) => return api_failure(&error.to_string()),
    };
    if body.index >= groups.len() {
        return api_failure(&format!(
            "{site} の {} 番目のログイン情報はありません",
            body.index + 1
        ));
    }
    let label = body.label.trim();
    groups[body.index].label = (!label.is_empty()).then(|| label.to_string());
    let name = groups[body.index].display_name().to_string();
    if let Err(error) = store.save_groups_for(&site, &groups).await {
        return api_failure(&error.to_string());
    }
    let data = status_data(runtime).await;
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{site} の {} 番目を「{name}」にしました", body.index + 1),
        "data": data,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/login/order (native: src/web/login.rs login_order)
// ---------------------------------------------------------------------------

async fn login_order(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let body: OrderBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let store = runtime.cookie_store();
    let site = normalize_cookie_host(&body.site);
    let stored = match store.groups_for(&site).await {
        Ok(stored) => stored,
        Err(error) => return api_failure(&error.to_string()),
    };
    let reordered = match reorder_groups(&stored, &body.order) {
        Ok(reordered) => reordered,
        Err(message) => return api_failure(&message),
    };
    if let Err(error) = store.save_groups_for(&site, &reordered).await {
        return api_failure(&error.to_string());
    }
    let data = status_data(runtime).await;
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{site} の試行順を変更しました"),
        "data": data,
    }))
}

/// 並び順の指定（現在の添字の並び）を検証して適用する
/// (native `reorder_groups` と同じ検証とエラーメッセージ)。
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

// ---------------------------------------------------------------------------
// 応答ヘルパ
// ---------------------------------------------------------------------------

/// native `status_payload().unwrap_or(Value::Null)` 相当: `GET /api/login` の
/// `data` 部分だけを取り出す。値の伏せ方は `crate::login::status` と共有する
/// (Cookie 本体は応答に含めない)。
async fn status_data(runtime: &WorkerRuntime) -> serde_json::Value {
    crate::login::status_payload(runtime)
        .await
        .unwrap_or(serde_json::Value::Null)
}

/// native `failure()`: HTTP 200 + `{success: false, message}`。
fn api_failure(message: &str) -> worker::Result<Response> {
    Response::from_json(&super::api_response(false, message))
}
