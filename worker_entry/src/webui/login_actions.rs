//! Web UI のログイン資格情報アクション (native: `src/web/login.rs`)。
//!
//! - `POST /api/login/import` — `narou_rs_login` が書き出したエクスポート
//!   エンベロープ (YAML) を取り込む。`{success, message, data}` の応答形と
//!   エラーメッセージは native `login_import` に揃える。
//! - `POST /api/login/order` — 1 ホストの資格情報の試行順を並べ替える。
//!   native `login_order` と同じく、現在の添字の並び順を受け取る。
//!
//! native との差異:
//! - エンベロープの暗号化形式 (Argon2id → XChaCha20-Poly1305) のうち Argon2id
//!   は `native-runtime` 専用で wasm ビルドには入らない。暗号化された
//!   エクスポートは検証 (salt/kdf/payload の欠落・非対応 KDF) までは native と
//!   同じエラーを返し、復号が必要な段で明示的に失敗させる。平文エクスポート
//!   (`encrypted: false`) は v1/v2 とも native と同じく取り込める。
//! - 保存は `D1CookieStore` (`app_state` の `login_cookie` 行) 経由。値は
//!   `save_all` が `enc:v1:` で暗号化して書く (native と同じ形式)。復号鍵の
//!   secret `NAROU_RS_LOGIN_KEY` が無いときは平文で書かず失敗する
//!   (fail-closed、`crate::login::set` と同じ判断)。
//! - native は資格情報変更を push_server でブロードキャストするが、Worker
//!   には同等の経路が無いので通知は行わない。

use std::collections::BTreeMap;

use narou_rs::application::settings_view::{MAX_WEB_TEXT_INPUT_BYTES, validate_web_text_size};
use narou_rs::error::{NarouError, Result as NarouResult};
use narou_rs::platform::{CookieStore, LoginCredential, normalize_cookie_host, tidy_credentials};
use serde::Deserialize;
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;
use crate::d1_cookie_store::D1CookieStore;

/// エクスポート形式の現行バージョン (`narou_rs::login::EXPORT_VERSION`、
/// native-runtime 限定なのでここに同名の値を持つ)。
const EXPORT_VERSION: u32 = 2;
/// 1 ホスト複数資格情報になる前の旧形式 (`EXPORT_VERSION_LEGACY`)。
const EXPORT_VERSION_LEGACY: u32 = 1;
/// 暗号化エンベロープの `kdf` 値 (`transfer::KDF_ARGON2ID`)。
const KDF_ARGON2ID: &str = "argon2id";

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

/// エクスポート文書 (native `narou_rs::login::CookieEnvelope` と同じ受理形)。
///
/// `payload` (暗号化) と `credentials`/`cookies` (平文) のどちらか一方が
/// 埋まっていて、`encrypted` がどちらを期待するかを示す。`exported_at` /
/// `library` は内容には使わないが、native と同じ必須/任意の区別にするため
/// フィールド自体は残す。
#[derive(Debug, Deserialize)]
struct Envelope {
    /// 形式バージョン。未知の値は取り込みを拒否する。
    version: u32,
    /// エクスポート時刻 (RFC 3339)。native と同じく必須。
    #[allow(dead_code)]
    exported_at: String,
    /// エクスポート元ライブラリ (表示用)。native と同じく任意。
    #[allow(dead_code)]
    #[serde(default)]
    library: Option<String>,
    /// `payload` が暗号化されたクッキーマップを持つか。
    encrypted: bool,
    /// `payload` の鍵導出関数。
    #[serde(default)]
    kdf: Option<String>,
    /// `payload` の base64 Argon2id ソルト。
    #[serde(default)]
    salt: Option<String>,
    /// 暗号化されたクッキーマップ (`nonce:payload`、base64)。
    #[serde(default)]
    payload: Option<String>,
    /// 平文の資格情報 (試行順)。`encrypted: false` のときのみ。
    #[serde(default)]
    credentials: Vec<LoginCredential>,
    /// v1 平文のクッキーマップ (`host → cookie`)。読み取り専用。
    #[serde(default)]
    cookies: BTreeMap<String, String>,
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
// エンベロープの読み出し (native: src/login/transfer.rs parse_export)
// ---------------------------------------------------------------------------

/// エクスポート文書を資格情報に戻す (native `parse_export` と同じ受理・拒否)。
///
/// 暗号化エクスポートの復号 (Argon2id) は wasm に無いので、native が復号前に
/// 行う検証を再現した上で、復号段で明示的な失敗を返す。
fn parse_export(text: &str, passphrase: Option<&str>) -> NarouResult<Vec<LoginCredential>> {
    let envelope: Envelope = serde_yaml::from_str(text).map_err(NarouError::from)?;
    if envelope.version != EXPORT_VERSION && envelope.version != EXPORT_VERSION_LEGACY {
        return Err(login_error(format!(
            "unsupported export version {} (this build reads versions {} and {})",
            envelope.version, EXPORT_VERSION_LEGACY, EXPORT_VERSION
        )));
    }
    if !envelope.encrypted {
        if envelope.version == EXPORT_VERSION_LEGACY {
            return Ok(envelope
                .cookies
                .into_iter()
                .map(|(host, cookie)| LoginCredential::new(host, cookie))
                .collect());
        }
        return Ok(envelope.credentials);
    }
    // 以下は native が復号前に返すエラーと同じ順で検証する。
    passphrase
        .ok_or_else(|| login_error("this export is encrypted: pass a passphrase to import it"))?;
    if envelope.salt.is_none() {
        return Err(login_error("encrypted export without a salt"));
    }
    if let Some(kdf) = envelope.kdf.as_deref()
        && kdf != KDF_ARGON2ID
    {
        return Err(login_error(format!("unsupported key derivation: {kdf}")));
    }
    if envelope.payload.is_none() {
        return Err(login_error("encrypted export without a payload"));
    }
    Err(login_error(
        "encrypted exports need Argon2id, which the Worker build does not carry; \
         import with the desktop app or export again without a passphrase",
    ))
}

/// 資格情報をホストごとにまとめる (native `group_credentials`)。
///
/// 取り込みはフラットな順序付きリストで来るが、保存はホストごとのリスト。
fn group_credentials(credentials: Vec<LoginCredential>) -> BTreeMap<String, Vec<LoginCredential>> {
    let mut grouped: BTreeMap<String, Vec<LoginCredential>> = BTreeMap::new();
    for credential in credentials {
        grouped
            .entry(normalize_cookie_host(&credential.host))
            .or_default()
            .push(credential);
    }
    grouped
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
    let Ok(mut response) = crate::login::status(runtime).await else {
        return serde_json::Value::Null;
    };
    response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| body.get("data").cloned())
        .unwrap_or(serde_json::Value::Null)
}

/// native `failure()`: HTTP 200 + `{success: false, message}`。
fn api_failure(message: &str) -> worker::Result<Response> {
    Response::from_json(&serde_json::json!({
        "success": false,
        "message": message,
    }))
}

/// `lib.rs::json_error` と同じ JSON 形 (`{error: {code, message?}}`)。あちらは
/// private なので形だけ合わせてここに持つ。
fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    let payload = match message {
        Some(message) => serde_json::json!({ "error": { "code": code, "message": message } }),
        None => serde_json::json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}

fn login_error(message: impl Into<String>) -> NarouError {
    NarouError::Login(message.into())
}
