//! 外部公開 API (読み取り専用) の HTTP 層。
//!
//! `/api/v1` の下に、外部連携向けの読み取り専用 API を置く。返り値は
//! JSON / JSON Lines / XML から選べる (`format` パラメータ)。
//!
//! 認証は Bearer トークン。トークンは `Authorization: Bearer <token>` または
//! `x-narou-api-token` ヘッダで送る。トークンの作成・確認・削除は
//! `narou api token` と設定ページの「外部API」タブから行える。
//!
//! `external-api.require-token` が false のときは認証しない (Cloudflare
//! Zero Trust の内側で使う場合など)。

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::application::external_api::{
    render, ExternalListRequest, OutputFormat, RenderOptions,
};
use crate::application::mcp::McpServer;
use crate::platform::NovelId;
use crate::web::AppState;

/// 外部 API のパス前缀。
pub const API_PREFIX: &str = "/api/v1";

/// Bearer 認証を必須にするかどうかの設定。
pub use crate::application::external_api::REQUIRE_TOKEN_SETTING;

/// トークンを送る別のヘッダ名。
pub const TOKEN_HEADER: &str = "x-narou-api-token";

/// 一覧のエンドポイント。
pub async fn api_novels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let request = match ExternalListRequest::from_pairs(&pairs(&params)) {
        Ok(request) => request,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, "bad_request", error.message()),
    };
    let hidden = crate::native::hidden::load().unwrap_or_default();
    match state
        .services
        .external_api
        .list(&request, Some(hidden))
        .await
    {
        Ok(list) => {
            let document = list.to_json();
            let items = list.to_jsonl_items();
            respond(
                request.format,
                &document,
                "novels",
                &[("data", "novel"), ("tags", "tag"), ("members", "member")],
                Some(&items),
            )
        }
        Err(error) => application_error(error),
    }
}

/// 作品 1 件の情報。
pub async fn api_novel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let Some(id) = parse_id(&id) else {
        return invalid_id(&id);
    };
    let format = format_of(&params);
    match state.services.external_api.novel(id).await {
        Ok(novel) => {
            let document = serde_json::to_value(&novel).unwrap_or(Value::Null);
            respond(
                format,
                &document,
                "novel",
                &[("tags", "tag"), ("members", "member")],
                None,
            )
        }
        Err(error) => application_error(error),
    }
}

/// 作品 1 件の目次。
pub async fn api_novel_toc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let Some(id) = parse_id(&id) else {
        return invalid_id(&id);
    };
    let format = format_of(&params);
    match state.services.external_api.toc(id).await {
        Ok(toc) => {
            let document = serde_json::to_value(&toc).unwrap_or(Value::Null);
            respond(
                format,
                &document,
                "toc",
                &[("sections", "section"), ("members", "member")],
                None,
            )
        }
        Err(error) => application_error(error),
    }
}

/// 作品 1 件の全話の本文。
pub async fn api_novel_sections(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let Some(id) = parse_id(&id) else {
        return invalid_id(&id);
    };
    let format = format_of(&params);
    let toc = match state.services.external_api.toc(id).await {
        Ok(toc) => toc,
        Err(error) => return application_error(error),
    };
    let mut sections = Vec::new();
    for section in &toc.sections {
        match state.services.external_api.section(id, &section.key).await {
            Ok(body) => sections.push(serde_json::to_value(&body).unwrap_or(Value::Null)),
            // 1 話だけ読めなかった場合は、その話だけを飛ばす (話全体が
            // 失敗したときの取りこぼしを避ける)。
            Err(error) => {
                crate::logger::emit_stderr(
                    &format!(
                        "外部 API: 本文を読み込めませんでした ({id} / {}): {error}",
                        section.key
                    ),
                    true,
                );
            }
        }
    }
    let document = serde_json::json!({ "id": id, "sections": sections });
    respond(
        format,
        &document,
        "sections",
        &[("sections", "section")],
        None,
    )
}

/// 作品 1 件の 1 話の本文。
pub async fn api_novel_section(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, key)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let Some(id) = parse_id(&id) else {
        return invalid_id(&id);
    };
    let format = format_of(&params);
    match state.services.external_api.section(id, &key).await {
        Ok(section) => {
            let document = serde_json::to_value(&section).unwrap_or(Value::Null);
            respond(format, &document, "section", &[], None)
        }
        Err(error) => application_error(error),
    }
}

/// 作品 1 件の EPUB。
///
/// 保存済みの EPUB があればそれを、無ければその場で生成する (lite ビルド)。
pub async fn api_novel_epub(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let Some(id) = parse_id(&id) else {
        return invalid_id(&id);
    };
    let record = match state.services.library.get(NovelId(id)).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                &format!("作品が見つかりません: {id}"),
            );
        }
        Err(error) => return application_error(error),
    };
    let novel_dir = match crate::db::with_database(|db| {
        crate::web::safe_existing_novel_dir(db.archive_root(), &record).map_err(crate::error::NarouError::Database)
    }) {
        Ok(novel_dir) => novel_dir,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &error.to_string(),
            );
        }
    };
    match crate::web::novels::serve_epub(&state, id, &record, &novel_dir).await {
        Ok(response) => response,
        Err((status, message)) => error_response(status, "internal_error", &message),
    }
}

/// タグの一覧。
pub async fn api_tags(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let format = format_of(&params);
    match state.services.external_api.tags().await {
        Ok(tags) => {
            let document = serde_json::to_value(&tags).unwrap_or(Value::Null);
            respond(format, &document, "tags", &[], None)
        }
        Err(error) => application_error(error),
    }
}

/// MCP の JSON-RPC 窓口。
pub async fn api_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("JSON を解釈できません: {error}") }
            }))
            .into_response();
        }
    };
    let hidden = crate::native::hidden::load().unwrap_or_default();
    let server = McpServer::new(state.services.external_api.clone());
    match server.handle(&request, Some(hidden)).await {
        Some(response) => Json(response).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// API の案内。
pub async fn api_index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = authorize(&state, &headers).await {
        return response;
    }
    Json(json!({
        "name": "narou.rs external API",
        "version": crate::version::VERSION,
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
    .into_response()
}

/// 外部 API の設定情報 (トークン一覧・認証モード)。
///
/// WEB UI の設定ページ (外部API タブ) から使う。ブラウザの UI トークンで
/// 保護されるため、ここにトークン本体を返してよい。
pub async fn api_external_api_info(State(state): State<AppState>) -> Response {
    let base_url = format!(
        "http://{}:{}{}",
        state.bind_host.as_ref(),
        state.port,
        API_PREFIX
    );
    match state
        .services
        .external_api
        .info(Some(base_url), None, None)
        .await
    {
        Ok(info) => Json(serde_json::to_value(info).unwrap_or(Value::Null)).into_response(),
        Err(error) => application_error(error),
    }
}

/// 新しい Bearer トークンを作る。
///
/// 本文に `{"name": "..."}` を渡すと名前を付けられる。
pub async fn api_external_api_token_create(State(state): State<AppState>, body: Bytes) -> Response {
    let name = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|name| !name.trim().is_empty());
    let created = state
        .services
        .api_tokens
        .create(name.as_deref())
        .await
        .map(|token| token.to_info());
    match created {
        Ok(info) => Json(json!({ "success": true, "token": info })).into_response(),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &error.to_string(),
        ),
    }
}

/// Bearer トークンを削除する。
pub async fn api_external_api_token_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.services.api_tokens.delete(&id).await {
        Ok(true) => Json(json!({ "success": true, "message": "削除しました" })).into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "not_found", "トークンが見つかりません"),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &error.to_string(),
        ),
    }
}

/// Bearer トークンを検証する。
///
/// 認証に成功した場合は `None`、失敗した場合は返す応答を `Some` で返す。
async fn authorize(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if !require_token(state).await {
        return None;
    }
    let Some(presented) = presented_token(headers) else {
        return Some(error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "Bearer トークンが必要です (Authorization: Bearer <token>)",
        ));
    };
    let accepted = state
        .services
        .api_tokens
        .verify(&presented)
        .await
        .unwrap_or(false);
    if accepted {
        None
    } else {
        Some(error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "Bearer トークンが不正です",
        ))
    }
}

/// Bearer 認証を必須にするか。
async fn require_token(state: &AppState) -> bool {
    state
        .services
        .settings
        .get(REQUIRE_TOKEN_SETTING)
        .await
        .ok()
        .flatten()
        .and_then(|value| crate::setting_core::setting_bool_value(&value))
        .unwrap_or(true)
}

/// ヘッダから提示されたトークンを取り出す。
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        let trimmed = value.trim();
        let token = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "));
        if let Some(token) = token {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// パスの ID を取り出す (不正なら `None`)。
fn parse_id(id: &str) -> Option<i64> {
    id.parse::<i64>().ok()
}

/// 不正な ID の応答 (JSON の 400)。
fn invalid_id(id: &str) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "bad_request",
        &format!("ID が不正です: {id}"),
    )
}

/// クエリパラメータを (キー, 値) の列にする。
fn pairs(params: &HashMap<String, String>) -> Vec<(String, String)> {
    params
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// 出力形式を取り出す。
fn format_of(params: &HashMap<String, String>) -> OutputFormat {
    params
        .get("format")
        .and_then(|value| OutputFormat::parse(value))
        .unwrap_or_default()
}

/// 応答を組み立てる。
fn respond(
    format: OutputFormat,
    document: &Value,
    root: &str,
    array_names: &[(&str, &str)],
    jsonl_items: Option<&[Value]>,
) -> Response {
    let body = render(
        document,
        &RenderOptions {
            format,
            root,
            array_names,
            jsonl_items,
        },
    );
    ([(header::CONTENT_TYPE, format.content_type())], body).into_response()
}

/// エラー応答 (`{"error":{"code":..,"message":..}}`)。
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// アプリケーションエラーを HTTP 応答にする。
fn application_error(error: crate::application::error::ApplicationError) -> Response {
    use crate::application::error::ApplicationError;
    match error {
        ApplicationError::InvalidRequest(message) => {
            error_response(StatusCode::BAD_REQUEST, "bad_request", &message)
        }
        ApplicationError::NotFound(message) => {
            error_response(StatusCode::NOT_FOUND, "not_found", &message)
        }
        ApplicationError::Conflict(message) => {
            error_response(StatusCode::CONFLICT, "conflict", &message)
        }
        ApplicationError::Platform(message) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", &message)
        }
    }
}


#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use narou_rs::application::api_tokens::ApiToken;
    use narou_rs::db::NovelRecord;
    use narou_rs::downloader::{SectionElement, SectionFile, SubtitleInfo, TocFile};
    use narou_rs::platform::mocks::{MemoryNovelRepository, MemoryObjectStore};
    use narou_rs::platform::{NovelObjectKeys, PlatformFuture};
    use narou_rs::virtual_novel::VirtualNovelMembers;

    use crate::web::AppState;

    const TOKEN: &str = "nrt_test_token_value";

    fn record(id: i64, title: &str, file_title: &str) -> NovelRecord {
        NovelRecord {
            id,
            author: "作者".to_string(),
            title: title.to_string(),
            file_title: file_title.to_string(),
            toc_url: format!("https://example.com/{id}"),
            sitename: "site".to_string(),
            novel_type: 0,
            end: false,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["共通".to_string()],
            ncode: Some(format!("n{id}aaaaa")),
            domain: Some("example.com".to_string()),
            general_all_no: Some(1),
            length: Some(100),
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    /// テスト用のバーチャル小説ディレクトリ。
    struct TestDirectory {
        members: VirtualNovelMembers,
    }

    impl narou_rs::application::external_api::VirtualNovelDirectory for TestDirectory {
        fn members(
            &self,
        ) -> PlatformFuture<'_, narou_rs::error::Result<VirtualNovelMembers>> {
            Box::pin(async move { Ok(self.members.clone()) })
        }
    }

    /// 目次と本文をオブジェクトストアに置く。
    fn seed_objects(objects: &Arc<dyn narou_rs::platform::ObjectStore>, file_title: &str) {
        let keys = NovelObjectKeys::new("site", file_title, false).unwrap();
        let toc = TocFile {
            title: "タイトル".to_string(),
            author: "作者".to_string(),
            toc_url: "https://example.com/1".to_string(),
            story: Some("あらすじ".to_string()),
            subtitles: vec![SubtitleInfo {
                index: "1".to_string(),
                href: "https://example.com/1/1".to_string(),
                chapter: "第一章".to_string(),
                subchapter: String::new(),
                subtitle: "第一話".to_string(),
                file_subtitle: "第一話".to_string(),
                subdate: "2026-01-01".to_string(),
                subupdate: None,
                download_time: None,
            }],
            novel_type: Some(0),
        };
        let toc_bytes = serde_yaml::to_string(&toc).unwrap().into_bytes();
        futures::executor::block_on(objects.write_small(&keys.toc(), toc_bytes)).unwrap();

        let section = SectionFile {
            index: "1".to_string(),
            href: "https://example.com/1/1".to_string(),
            chapter: "第一章".to_string(),
            subchapter: String::new(),
            subtitle: "第一話".to_string(),
            file_subtitle: "第一話".to_string(),
            subdate: "2026-01-01".to_string(),
            subupdate: None,
            download_time: None,
            element: SectionElement {
                data_type: "html".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: "<p>本文</p>".to_string(),
            },
        };
        let section_bytes = serde_yaml::to_string(&section).unwrap().into_bytes();
        futures::executor::block_on(
            objects.write_small(&keys.section("1", "第一話"), section_bytes),
        )
        .unwrap();
    }

    /// テスト用の状態を作る。
    fn build_state(records: Vec<NovelRecord>, members: VirtualNovelMembers) -> AppState {
        let novels = Arc::new(MemoryNovelRepository::from_records(records));
        let objects: Arc<dyn narou_rs::platform::ObjectStore> = Arc::new(MemoryObjectStore::new());
        for record in ["通常", "メンバー"].iter() {
            seed_objects(&objects, record);
        }
        let tokens = vec![ApiToken {
            id: "token-1".to_string(),
            name: "test".to_string(),
            token: TOKEN.to_string(),
            created_at: "2026-01-01 00:00:00.000000 +0900".to_string(),
        }];
        let services = crate::web::default_app_services_with(
            novels,
            objects,
            tokens,
            Arc::new(TestDirectory { members }),
        );
        crate::web::tests::test_state_with_services(services, None)
    }

    /// サーバーを起動して URL を返す。
    async fn serve(mut state: AppState) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Host ヘッダの検査で待ち受けポートを照合するため、実ポートを state's
        // 側にも反映する。
        state.port = port;
        let router = crate::web::create_router(state);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://127.0.0.1:{port}"), server)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    /// 応答本文を JSON として読む (reqwest の json 機能を使わないため)。
    trait JsonValueExt {
        async fn json_value(self) -> serde_json::Value;
    }

    impl JsonValueExt for reqwest::Response {
        async fn json_value(self) -> serde_json::Value {
            let text = self.text().await.unwrap();
            serde_json::from_str(&text).unwrap()
        }
    }

    #[tokio::test]
    async fn the_list_needs_a_bearer_token() {
        let state = build_state(vec![record(1, "通常", "通常")], VirtualNovelMembers::default());
        let (base, server) = serve(state).await;
        let client = client();

        let denied = client
            .get(format!("{base}/api/v1/novels"))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);

        let wrong = client
            .get(format!("{base}/api/v1/novels"))
            .header("Authorization", "Bearer wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

        let allowed = client
            .get(format!("{base}/api/v1/novels"))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), reqwest::StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn the_list_serves_json_jsonl_and_xml() {
        let state = build_state(vec![record(1, "通常", "通常")], VirtualNovelMembers::default());
        let (base, server) = serve(state).await;
        let client = client();

        let json = client
            .get(format!("{base}/api/v1/novels"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(json.status(), reqwest::StatusCode::OK);
        assert_eq!(
            json.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
        let document: serde_json::Value = json.json_value().await;
        assert_eq!(document["records_filtered"], 1);
        assert_eq!(document["data"][0]["title"], "通常");

        let jsonl = client
            .get(format!("{base}/api/v1/novels?format=jsonl"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap();
        let body = jsonl.text().await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["title"], "通常");

        let xml = client
            .get(format!("{base}/api/v1/novels?format=xml"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap();
        let body = xml.text().await.unwrap();
        assert!(body.contains("<novels>"), "{body}");
        assert!(body.contains("<novel>"), "{body}");
        assert!(body.contains("<title>通常</title>"), "{body}");
        server.abort();
    }

    #[tokio::test]
    async fn the_list_groups_virtual_novels_unless_asked_otherwise() {
        let mut members = VirtualNovelMembers::default();
        members.insert(9, vec![1, 2]).unwrap();
        let mut virtual_record = record(9, "まとめ", "まとめ");
        virtual_record.ncode = Some("vs_9".to_string());
        let records = vec![record(1, "通常", "通常"), record(2, "メンバー", "メンバー"), virtual_record];
        let state = build_state(records, members);
        let (base, server) = serve(state).await;
        let client = client();

        let grouped: serde_json::Value = client
            .get(format!("{base}/api/v1/novels"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap()
            .json_value().await;
        let titles: Vec<&str> = grouped["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["title"].as_str().unwrap())
            .collect();
        assert!(titles.contains(&"まとめ"), "{titles:?}");
        assert!(!titles.contains(&"メンバー"), "{titles:?}");

        let expanded: serde_json::Value = client
            .get(format!("{base}/api/v1/novels?virtual=expand"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap()
            .json_value().await;
        let titles: Vec<&str> = expanded["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["title"].as_str().unwrap())
            .collect();
        assert!(titles.contains(&"メンバー"), "{titles:?}");
        assert!(!titles.contains(&"まとめ"), "{titles:?}");

        let excluded: serde_json::Value = client
            .get(format!("{base}/api/v1/novels?virtual=exclude"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap()
            .json_value().await;
        let titles: Vec<&str> = excluded["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["title"].as_str().unwrap())
            .collect();
        assert!(!titles.contains(&"まとめ"), "{titles:?}");
        assert!(!titles.contains(&"メンバー"), "{titles:?}");
        server.abort();
    }

    #[tokio::test]
    async fn the_toc_and_the_section_body_are_served() {
        let state = build_state(vec![record(1, "通常", "通常")], VirtualNovelMembers::default());
        let (base, server) = serve(state).await;
        let client = client();

        let toc: serde_json::Value = client
            .get(format!("{base}/api/v1/novels/1/toc"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap()
            .json_value().await;
        assert_eq!(toc["sections"][0]["index"], "1");
        assert_eq!(toc["sections"][0]["subtitle"], "第一話");

        let section: serde_json::Value = client
            .get(format!("{base}/api/v1/novels/1/sections/1"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap()
            .json_value().await;
        assert_eq!(section["body"], "<p>本文</p>");
        assert_eq!(section["index"], "1");

        let missing = client
            .get(format!("{base}/api/v1/novels/1/sections/9"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

        // 全話 (目次順) を返す経路。
        let sections: serde_json::Value = client
            .get(format!("{base}/api/v1/novels/1/sections"))
            .header("x-narou-api-token", TOKEN)
            .send()
            .await
            .unwrap()
            .json_value()
            .await;
        assert_eq!(sections["id"], 1);
        assert_eq!(sections["sections"].as_array().unwrap().len(), 1);
        assert_eq!(sections["sections"][0]["body"], "<p>本文</p>");
        assert_eq!(sections["sections"][0]["key"], "1:1");
        server.abort();
    }

    #[tokio::test]
    async fn the_mcp_endpoint_lists_and_calls_tools() {
        let state = build_state(vec![record(1, "通常", "通常")], VirtualNovelMembers::default());
        let (base, server) = serve(state).await;
        let client = client();

        let initialize: serde_json::Value = client
            .post(format!("{base}/api/v1/mcp"))
            .header("x-narou-api-token", TOKEN)
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .send()
            .await
            .unwrap()
            .json_value().await;
        assert_eq!(initialize["result"]["serverInfo"]["name"], "narou.rs");

        let tools: serde_json::Value = client
            .post(format!("{base}/api/v1/mcp"))
            .header("x-narou-api-token", TOKEN)
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .send()
            .await
            .unwrap()
            .json_value().await;
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"list_novels"), "{names:?}");
        assert!(names.contains(&"get_novel_toc"), "{names:?}");

        let call: serde_json::Value = client
            .post(format!("{base}/api/v1/mcp"))
            .header("x-narou-api-token", TOKEN)
            .header("content-type", "application/json")
            .body(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_novels","arguments":{"format":"json"}}}"#,
            )
            .send()
            .await
            .unwrap()
            .json_value().await;
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("通常"), "{text}");
        server.abort();
    }
}
