//! Web UI のログイン資格情報アクション (native: `src/web/login.rs`)。
//!
//! - `POST /api/login/import` — `narou_rs_login` が書き出したエクスポート
//!   エンベロープ (YAML) を取り込む。`{success, message, data}` の応答形と
//!   エラーメッセージは native `login_import` に揃える。
//! - `POST /api/login/order` — 1 ホストの資格情報の試行順を並べ替える。
//!   native `login_order` と同じく、現在の添字の並び順を受け取る。
//!
//! native との差異:
//! - エンベロープの解析・復号 (Argon2id → XChaCha20-Poly1305) は native と同じ
//!   `narou_rs::login::{parse_export, group_credentials}` を使うので、暗号化
//!   エクスポートもエラー文言も native と一致する。
//! - 保存は `D1CookieStore` (`app_state` の `login_cookie` 行) 経由。値は
//!   `save_all` が `enc:v1:` で暗号化して書く (native と同じ形式)。復号鍵の
//!   secret `NAROU_RS_LOGIN_KEY` が無いときは平文で書かず失敗する
//!   (fail-closed、`crate::login::set` と同じ判断)。
//! - native は資格情報変更を push_server でブロードキャストするが、Worker
//!   には同等の経路が無いので通知は行わない。

use std::collections::BTreeMap;

use narou_rs::application::settings_view::{MAX_WEB_TEXT_INPUT_BYTES, validate_web_text_size};
use narou_rs::error::Result as NarouResult;
use narou_rs::login::{group_credentials, parse_export};
use narou_rs::platform::{CookieStore, LoginCredential, normalize_cookie_host, tidy_credentials};
use serde::Deserialize;
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;
use crate::d1_cookie_store::D1CookieStore;

use super::json_error;

/// `POST /api/login/import` の本文 (native `LoginImportRequest`)。
#[derive(Debug, Deserialize)]
struct ImportBody {
    /// `narou_rs_login` が書き出したエクスポート文書 (YAML)。
    envelope: String,
    /// 暗号化エクスポートのパスフレーズ。
    #[serde(default)]
    passphrase: Option<String>,
    /// 取り込みに含まれないホストを消すか。
    #[serde(default)]
    replace: bool,
}

/// `POST /api/login/order` の本文 (native `LoginOrderRequest`)。
#[derive(Debug, Deserialize)]
struct OrderBody {
    /// 並べ替える資格情報を持つリクエストホスト。
    host: String,
    /// 新しい順序で並べた現在の添字 (例: `[1, 0, 2]`)。
    order: Vec<usize>,
}

/// 親がこの 1 ハンドラを 2 ルートへ割り当てる。パスとメソッドで振り分ける
/// (`/api/login/` の prefix ガードより先に置く前提)。
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let path = req.path();
    enum Route {
        Import,
        Order,
    }
    let route = match (req.method(), path.as_str()) {
        (Method::Post, "/api/login/import") => Route::Import,
        (Method::Post, "/api/login/order") => Route::Order,
        (_, "/api/login/import" | "/api/login/order") => {
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
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    match route {
        Route::Import => login_import(&mut req, &runtime).await,
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
    let credentials = match parse_export(&body.envelope, body.passphrase.as_deref()) {
        Ok(credentials) => credentials,
        Err(error) => return api_failure(&error.to_string()),
    };
    if credentials.is_empty() {
        return api_failure("書き出しファイルに Cookie が含まれていません");
    }
    let store = runtime.cookie_store();
    let imported = credentials.len();
    let grouped = group_credentials(credentials);
    let stored = match if body.replace {
        replace_credentials(store, &grouped).await
    } else {
        merge_credentials(store, &grouped).await
    } {
        Ok(stored) => stored,
        Err(error) => return api_failure(&error.to_string()),
    };
    let data = status_data(runtime).await;
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{imported} サイトを取り込みました (保存済み {stored} サイト)"),
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
    let host = normalize_cookie_host(&body.host);
    // `load_all` は親ドメインのエントリも畳み込む (native `credentials_for`
    // と同じ `merge_credentials_for` 経路)。
    let stored = match store.load_all(&host).await {
        Ok(stored) => stored,
        Err(error) => return api_failure(&error.to_string()),
    };
    let reordered = match reorder_credentials(&stored, &body.order) {
        Ok(reordered) => reordered,
        Err(message) => return api_failure(&message),
    };
    if let Err(error) = store.save_all(&host, &reordered).await {
        return api_failure(&error.to_string());
    }
    let data = status_data(runtime).await;
    Response::from_json(&serde_json::json!({
        "success": true,
        "message": format!("{host} の試行順を変更しました"),
        "data": data,
    }))
}

// ---------------------------------------------------------------------------
// D1 ストアへの書き戻し (native: InventoryCookieStore::merge/replace_credentials)
// ---------------------------------------------------------------------------

/// 資格情報を取り込み、既に持っているホストには後ろに足す。
///
/// 同じ値が既に保存済みなら重複追加しない (native `merge_credentials`)。
/// 戻り値は書き込まれたホスト数。D1 にはマップ全体の一括書き込みが無いので、
/// 取り込み対象のホストだけ `save_all` で書き戻す。
async fn merge_credentials(
    store: &D1CookieStore,
    entries: &BTreeMap<String, Vec<LoginCredential>>,
) -> NarouResult<usize> {
    let mut map = store.list().await?;
    for (host, credentials) in entries {
        let host = normalize_cookie_host(host);
        let stored = map.entry(host).or_default();
        for credential in tidy_credentials(credentials) {
            if !stored
                .iter()
                .any(|existing| existing.same_cookie(&credential))
            {
                stored.push(credential);
            }
        }
    }
    map.retain(|_, credentials| !credentials.is_empty());
    let written = map.len();
    for (host, _) in entries {
        let host = normalize_cookie_host(host);
        match map.get(&host) {
            Some(credentials) => store.save_all(&host, credentials).await?,
            // 取り込み分がすべて空なら既存のエントリも消える。
            None => store.save_all(&host, &[]).await?,
        }
    }
    Ok(written)
}

/// 資格情報を取り込み、取り込みに含まれないホストはすべて消す
/// (native `replace_credentials`)。戻り値は書き込まれたホスト数。
async fn replace_credentials(
    store: &D1CookieStore,
    entries: &BTreeMap<String, Vec<LoginCredential>>,
) -> NarouResult<usize> {
    let mut map: BTreeMap<String, Vec<LoginCredential>> = BTreeMap::new();
    for (host, credentials) in entries {
        let credentials = tidy_credentials(credentials);
        if !credentials.is_empty() {
            map.insert(normalize_cookie_host(host), credentials);
        }
    }
    let written = map.len();
    for host in store.list().await?.keys() {
        if !map.contains_key(host) {
            store.save_all(host, &[]).await?;
        }
    }
    for (host, credentials) in &map {
        store.save_all(host, credentials).await?;
    }
    Ok(written)
}

/// 並び順の指定（現在の添字の並び）を検証して適用する
/// (native `reorder_credentials` と同じ検証とエラーメッセージ)。
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

