//! 外部公開 API (読み取り専用) の Worker 実装。
//!
//! native `src/web/external.rs` と同じ契約:
//!
//! - `GET /api/v1/novels` 一覧 (条件・並び替え・JSON / JSON Lines / XML)
//! - `GET /api/v1/novels/{id}` 作品 1 件
//! - `GET /api/v1/novels/{id}/toc` 目次 (バーチャル小説は連結)
//! - `GET /api/v1/novels/{id}/sections/{key}` 1 話の本文
//! - `GET /api/v1/novels/{id}/epub` EPUB
//! - `GET /api/v1/tags` タグ一覧
//! - `POST /api/v1/mcp` MCP (JSON-RPC 2.0)
//!
//! 認証は Cloudflare Zero Trust (Access) の Service Token
//! (`CF-Access-Client-Id` / `CF-Access-Client-Secret`) を使う。Access を
//! 設定していない Worker では narou の Bearer トークンを要求する。

use narou_rs::application::external_api::{
    parse_format, render, ExternalListRequest, OutputFormat, RenderOptions,
};
use narou_rs::application::mcp::McpServer;
use serde_json::{json, Value};
use worker::{Env, Method, Request, Response};

use super::json_error;

/// Cloudflare Access の Service Token の Client Id を入れる secret 名。
pub const ACCESS_CLIENT_ID_SECRET: &str = "NAROU_CF_ACCESS_CLIENT_ID";
/// Cloudflare Access の Service Token の Client Secret を入れる secret 名。
pub const ACCESS_CLIENT_SECRET_SECRET: &str = "NAROU_CF_ACCESS_CLIENT_SECRET";

fn application_error(error: narou_rs::application::ApplicationError) -> worker::Result<Response> {
    let (status, code) = narou_rs::application::external_api::http_error(&error);
    json_error(status, code, Some(error.message()))
}

/// 外部公開 API の設定 (トークン管理)。
///
/// WEB UI の設定ページ (外部API タブ) から使う。管理トークン (または
/// 管理 Cookie) で保護されるため、ここにトークン本体を返してよい。
pub async fn handle_settings(req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let path = req.path();
    let method = req.method();
    let services = match crate::composition::build_read_services(&env).await {
        Ok(services) => services,
        Err(error) => {
            worker::console_log!("external api settings: composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    if path == "/api/external_api" && matches!(method, Method::Get | Method::Head) {
        let base_url = req
            .url()
            .ok()
            .map(|url| format!("{}/api/v1", url.origin().ascii_serialization()));
        let client_id = crate::secrets::value(&env, ACCESS_CLIENT_ID_SECRET).await;
        let client_secret = crate::secrets::value(&env, ACCESS_CLIENT_SECRET_SECRET).await;
        let info = match services
            .app
            .external_api
            .info(base_url, client_id, client_secret)
            .await
        {
            Ok(info) => info,
            Err(error) => return json_error(500, "internal_error", Some(error.message())),
        };
        return Response::from_json(&info);
    }
    if path == "/api/external_api/token" && method == Method::Post {
        let mut req = req;
        let body = req.bytes().await.unwrap_or_default();
        let name = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|name| !name.trim().is_empty());
        return match services.app.api_tokens.create(name.as_deref()).await {
            Ok(token) => Ok(Response::from_json(&json!({
                "success": true,
                "token": token.to_info()
            }))?),
            Err(error) => json_error(500, "internal_error", Some(&error.to_string())),
        };
    }
    if let Some(id) = path.strip_prefix("/api/external_api/token/") {
        if method != Method::Delete {
            return Response::error("Method Not Allowed", 405);
        }
        return match services.app.api_tokens.delete(id).await {
            Ok(true) => Ok(Response::from_json(&json!({
                "success": true,
                "message": "削除しました"
            }))?),
            Ok(false) => json_error(404, "not_found", Some("トークンが見つかりません")),
            Err(error) => json_error(500, "internal_error", Some(&error.to_string())),
        };
    }
    Response::error("Not Found", 404)
}

/// 外部 API は空のパス要素を無視する。HEAD の振り分けも同じ規則を使う。
pub(crate) fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.strip_prefix("/api/v1").unwrap_or_default()
        .split('/').filter(|part| !part.is_empty())
}

/// 外部公開 API の入口。
pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    let path = req.path();
    if !(path == "/api/v1" || path.starts_with("/api/v1/")) {
        return Response::error("Not Found", 404);
    }
    let segments: Vec<&str> = path_segments(&path).collect();

    let services = match crate::composition::build_read_services(&env).await {
        Ok(services) => services,
        Err(error) => {
            worker::console_log!("external api: service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    if let Some(response) = authorize(&req, &env, &services.app).await {
        return response;
    }

    match segments.as_slice() {
        [] => api_index(&req).await,
        ["novels"] => api_novels(&req, &env, &services.app).await,
        ["tags"] => api_tags(&req, &env, &services.app).await,
        ["mcp"] => api_mcp(req, &env, &services.app).await,
        ["novels", id] => api_novel(&req, &env, &services.app, id).await,
        ["novels", id, "toc"] => api_novel_toc(&req, &env, &services.app, id).await,
        ["novels", id, "epub"] => api_novel_epub(&req, &env, &services, id).await,
        ["novels", id, "sections"] => api_novel_sections(&req, &env, &services.app, id).await,
        ["novels", id, "sections", key] => {
            api_novel_section(&req, &env, &services.app, id, key).await
        }
        _ => Response::error("Not Found", 404),
    }
}

/// ID を数値にする (不正なら 400 の JSON)。
fn parse_id(value: &str) -> worker::Result<i64> {
    value.parse::<i64>().map_err(|_| {
        worker::Error::RustError(format!("ID が不正です: {value}"))
    })
}

/// 外部 API の認証。
///
/// Cloudflare Zero Trust (Access) の Service Token を設定している Worker
/// では、その資格情報だけを照合する (narou の Bearer トークンは不要)。
/// Access を設定していない場合は narou の Bearer トークンを要求する。
async fn authorize(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
) -> Option<worker::Result<Response>> {
    let client_id = crate::secrets::value(env, ACCESS_CLIENT_ID_SECRET).await;
    let client_secret = crate::secrets::value(env, ACCESS_CLIENT_SECRET_SECRET).await;
    if let (Some(client_id), Some(client_secret)) = (client_id, client_secret) {
        // Access の「単一ヘッダー」設定 (read_service_tokens_from_header:
        // Authorization) を有効にした場合、Service Token は
        // `CF-Access-Client-Id` / `CF-Access-Client-Secret` の組ではなく
        // Authorization ヘッダに JSON で届く:
        //   Authorization: {"cf-access-client-id": "…", "cf-access-client-secret": "…"}
        // AI 系のクライアントは 1 つのヘッダしか送れないため、この形も
        // 受け付ける。
        if let Some((presented_id, presented_secret)) = access_token_from_headers(req) {
            let matched = constant_time_eq(&presented_id, &client_id)
                && constant_time_eq(&presented_secret, &client_secret);
            if matched {
                return None;
            }
            return Some(json_error(
                401,
                "authentication_required",
                Some("Cloudflare Access の Service Token が不正です"),
            ));
        }
        // Access の前段を通った要求は、Access が `CF-Access-Client-Id` /
        // `CF-Access-Client-Secret` を除去してから Worker へ渡す (-edge
        // 側の認証が済んでいるので、資格情報が看不到)。前段の認証済みを
        // 示す `Cf-Access-Jwt-Assertion` があれば、Access を境界として
        // そのまま通す。前段を通っていない要求 (Worker へ直接届く要求) は
        // 下の Bearer 検査へ落とす。
        if header_value(req, "Cf-Access-Jwt-Assertion").is_some()
            || header_value(req, "Cf-Access-Authenticated-User-Email").is_some()
        {
            return None;
        }
    }

    if !services.external_api.require_token().await {
        return None;
    }
    let Some(token) = bearer_token(req) else {
        return Some(json_error(
            401,
            "authentication_required",
            Some("Bearer トークンが必要です (Authorization: Bearer <token>)"),
        ));
    };
    match services.api_tokens.verify(&token).await {
        Ok(true) => None,
        _ => Some(json_error(
            401,
            "authentication_required",
            Some("Bearer トークンが不正です"),
        )),
    }
}

/// ヘッダの値を取り出す。
fn header_value(req: &Request, name: &str) -> Option<String> {
    let value = req.headers().get(name).ok().flatten()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Access の Service Token を取り出す。
///
/// 2 通りを受け付ける。
///
/// - `CF-Access-Client-Id` / `CF-Access-Client-Secret` のヘッダ対 (既定)
/// - `Authorization` ヘッダの JSON (Access の「単一ヘッダー」設定で
///   `read_service_tokens_from_header: Authorization` を有効にした場合)
fn access_token_from_headers(req: &Request) -> Option<(String, String)> {
    if let (Some(id), Some(secret)) = (
        header_value(req, "CF-Access-Client-Id"),
        header_value(req, "CF-Access-Client-Secret"),
    ) {
        return Some((id, secret));
    }
    let value = header_value(req, "authorization")?;
    let parsed: serde_json::Value = serde_json::from_str(value.trim()).ok()?;
    let id = parsed.get("cf-access-client-id")?.as_str()?.trim().to_string();
    let secret = parsed
        .get("cf-access-client-secret")?
        .as_str()?
        .trim()
        .to_string();
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some((id, secret))
}

/// Bearer トークンを取り出す。
///
/// native と同じく `Authorization: Bearer <token>` と
/// `x-narou-api-token: <token>` のどちらでも受け取る。認証方式の
/// 大文字小文字は区別しない (RFC 7235)。
fn bearer_token(req: &Request) -> Option<String> {
    if let Some(value) = header_value(req, "authorization")
        && let Some((scheme, token)) = value.trim().split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    header_value(req, "x-narou-api-token")
}

/// 一定時間比較。
fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.as_bytes().iter().zip(right.as_bytes().iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// クエリパラメータを (キー, 値) の列にする。
fn query_pairs(req: &Request) -> Vec<(String, String)> {
    let Ok(url) = req.url() else {
        return Vec::new();
    };
    url.query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// 出力形式を取り出す (不正なら 400 の JSON)。
///
/// native と同じく、不正な形式を黙って JSON へ落とさない。
fn format_of(req: &Request) -> std::result::Result<OutputFormat, worker::Result<Response>> {
    let Ok(url) = req.url() else {
        return Ok(OutputFormat::default());
    };
    let value = url
        .query_pairs()
        .find(|(key, _)| key == "format")
        .map(|(_, value)| value.into_owned());
    match value {
        Some(value) => match parse_format(&value) {
            Ok(format) => Ok(format),
            Err(error) => Err(json_error(400, "bad_request", Some(error.message()))),
        },
        None => Ok(OutputFormat::default()),
    }
}

/// 応答を組み立てる。
fn respond(
    format: OutputFormat,
    document: &Value,
    root: &str,
    array_names: &[(&str, &str)],
    xml_item: &str,
    jsonl_items: Option<&[Value]>,
) -> worker::Result<Response> {
    let body = render(
        document,
        &RenderOptions {
            format,
            root,
            array_names,
            xml_item,
            jsonl_items,
        },
    );
    let mut response = Response::from_bytes(body.into_bytes())?;
    let _ = response
        .headers_mut()
        .set("content-type", format.content_type());
    Ok(response)
}

/// API の案内。
async fn api_index(req: &Request) -> worker::Result<Response> {
    let _ = req;
    Response::from_json(&json!({
        "name": "narou.rs external API",
        "version": env!("CARGO_PKG_VERSION"),
        "authentication": "Bearer",
        "endpoints": [
            { "path": "/api/v1/novels", "description": "一覧 (条件・並び替え・出力形式を選べる)" },
            { "path": "/api/v1/novels/{id}", "description": "作品 1 件の情報" },
            { "path": "/api/v1/novels/{id}/toc", "description": "作品 1 件の目次 (バーチャル小説は連結)" },
            { "path": "/api/v1/novels/{id}/sections", "description": "作品 1 件の全話の本文" },
            { "path": "/api/v1/novels/{id}/sections/{key}", "description": "作品 1 件の 1 話の本文" },
            { "path": "/api/v1/novels/{id}/epub", "description": "作品 1 件の EPUB" },
            { "path": "/api/v1/tags", "description": "タグの一覧" },
            { "path": "/api/v1/mcp", "description": "MCP (JSON-RPC 2.0) の窓口" }
        ],
        "formats": ["json", "jsonl", "xml"]
    }))
}

/// 一覧。
async fn api_novels(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
) -> worker::Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    let request = match ExternalListRequest::from_pairs(&query_pairs(req)) {
        Ok(request) => request,
        Err(error) => return json_error(400, "bad_request", Some(error.message())),
    };
    let hidden = if request.include_hidden {
        None
    } else {
        crate::hidden::load_from_env(env).await.ok()
    };
    match services.external_api.list(&request, hidden).await {
        Ok(list) => {
            let document = list.to_json();
            respond(
                request.format,
                &document,
                "novels",
                &[("data", "novel"), ("tags", "tag"), ("members", "member")],
                "novel",
                document.get("data").and_then(Value::as_array).map(Vec::as_slice),
            )
        }
        Err(error) => application_error(error),
    }
}

/// 作品 1 件。
async fn api_novel(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
    id: &str,
) -> worker::Result<Response> {
    let _ = env;
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    let id = match parse_id(id) {
        Ok(id) => id,
        Err(error) => return json_error(400, "bad_request", Some(&error.to_string())),
    };
    let format = match format_of(req) {
        Ok(format) => format,
        Err(response) => return response,
    };
    match services.external_api.novel(id).await {
        Ok(novel) => {
            let document = serde_json::to_value(&novel).unwrap_or(Value::Null);
            respond(
                format,
                &document,
                "novel",
                &[("tags", "tag"), ("members", "member")],
                "novel",
                None,
            )
        }
        Err(error) => application_error(error),
    }
}

/// 目次。
async fn api_novel_toc(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
    id: &str,
) -> worker::Result<Response> {
    let _ = env;
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    let id = match parse_id(id) {
        Ok(id) => id,
        Err(error) => return json_error(400, "bad_request", Some(&error.to_string())),
    };
    let format = match format_of(req) {
        Ok(format) => format,
        Err(response) => return response,
    };
    match services.external_api.toc(id).await {
        Ok(toc) => {
            let document = serde_json::to_value(&toc).unwrap_or(Value::Null);
            respond(
                format,
                &document,
                "toc",
                &[("sections", "section"), ("members", "member")],
                "section",
                None,
            )
        }
        Err(error) => application_error(error),
    }
}

/// 全話の本文。
async fn api_novel_sections(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
    id: &str,
) -> worker::Result<Response> {
    let _ = env;
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    let id = match parse_id(id) {
        Ok(id) => id,
        Err(error) => return json_error(400, "bad_request", Some(&error.to_string())),
    };
    let format = match format_of(req) {
        Ok(format) => format,
        Err(response) => return response,
    };
    // 目次は所有者ごとに 1 回だけ読む (話ごとに目次を読み直さない)。
    let sections = match services.external_api.sections(id).await {
        Ok(sections) => sections,
        Err(error) => return application_error(error),
    };
    let document = serde_json::to_value(&sections).unwrap_or(Value::Null);
    respond(
        format,
        &document,
        "sections",
        &[("sections", "section")],
        "section",
        document.get("sections").and_then(Value::as_array).map(Vec::as_slice),
    )
}

/// 1 話の本文。
async fn api_novel_section(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
    id: &str,
    key: &str,
) -> worker::Result<Response> {
    let _ = env;
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    let id = match parse_id(id) {
        Ok(id) => id,
        Err(error) => return json_error(400, "bad_request", Some(&error.to_string())),
    };
    let key = percent_decode(key);
    let format = match format_of(req) {
        Ok(format) => format,
        Err(response) => return response,
    };
    match services.external_api.section(id, &key).await {
        Ok(section) => {
            let document = serde_json::to_value(&section).unwrap_or(Value::Null);
            respond(format, &document, "section", &[], "section", None)
        }
        Err(error) => application_error(error),
    }
}

/// EPUB。
///
/// 認証判定のために組み立てた `services` をそのまま使うので、同じ要求で
/// 組立を 2 回行わない (組立のたびに D1 を読むため、応答が 1 往復遅くなる)。
async fn api_novel_epub(
    req: &Request,
    env: &Env,
    services: &crate::composition::ReadServices,
    id: &str,
) -> worker::Result<Response> {
    let id = match parse_id(id) {
        Ok(id) => id,
        Err(error) => return json_error(400, "bad_request", Some(&error.to_string())),
    };
    crate::build_epub_with_services(req, env, services, id, false).await
}

/// タグ一覧。
async fn api_tags(
    req: &Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
) -> worker::Result<Response> {
    let _ = env;
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    let format = match format_of(req) {
        Ok(format) => format,
        Err(response) => return response,
    };
    match services.external_api.tags().await {
        Ok(tags) => {
            let document = serde_json::to_value(&tags).unwrap_or(Value::Null);
            // タグ一覧は 1 件ずつ並べる (JSON Lines も XML も 1 タグ = 1 要素)。
            respond(format, &document, "tags", &[], "tag", document.as_array().map(Vec::as_slice))
        }
        Err(error) => application_error(error),
    }
}

/// MCP の窓口。
async fn api_mcp(
    mut req: Request,
    env: &Env,
    services: &narou_rs::application::AppServices,
) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(
            405,
            "method_not_allowed",
            Some("MCP は POST (JSON-RPC 2.0) のみ対応です"),
        );
    }
    let body = req.bytes().await?;
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return Ok(Response::from_json(&json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("JSON を解釈できません: {error}") }
            }))?
            .with_status(400));
        }
    };
    let hidden = crate::hidden::load_from_env(env).await.ok();
    let server = McpServer::new(&services.external_api);
    match server.handle(&request, hidden).await {
        Some(response) => Ok(Response::from_json(&response)?),
        // 通知 (id なし) には応答しない。native と同じく 202 を返す。
        None => Ok(Response::from_bytes(Vec::new())?.with_status(202)),
    }
}

/// `%xx` をデコードする (パスの話番号 `12:3` をそのまま扱うため)。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
