//! `GET`/`POST /api/list` — DataTables 形式の小説一覧。
//!
//! native `src/web/novels.rs` の `api_list` / `api_list_post`
//! (`api_list_inner`) と同じパラメータ・同じ JSON 形を返す。
//! GET はクエリ文字列、POST は `application/x-www-form-urlencoded` 本文から
//! 読む。axum はどちらも `serde_urlencoded` で平らに展開するため、
//! `search[value]` / `order[0][column]` / `order[0][dir]` はブラケット込みの
//! リテラルキー名として扱う (ネストしない)。
//!
//! 一覧の絞り込み・ソート・ページング・frozen/new マーカーは native と
//! 同じ `narou_rs::application::LibraryService::list`
//! (`runtime.services.library`) に委譲するので、行の意味も一致する。

use narou_rs::application::web_payloads::{ListParams, NovelListItem, NovelListResponse};
use narou_rs::application::{
    ApplicationError, LibraryListRequest, LibrarySortColumn, LibrarySortOrder,
};
use std::collections::HashSet;
use worker::{Env, Method, Request, Response};

/// native `crate::web::MAX_WEB_PAGE_LENGTH` と同じページ長上限。
const MAX_WEB_PAGE_LENGTH: u64 = 500;
/// native `crate::web::MAX_WEB_SEARCH_BYTES` と同じ検索語バイト上限。
const MAX_WEB_SEARCH_BYTES: usize = 4096;
/// axum `Form` と同じ受理 Content-Type (パラメータ部は無視)。
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";


pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let params = match req.method() {
        // axum は `get(...)` に HEAD も通す。Query と同じく生クエリを読む。
        Method::Get | Method::Head => {
            let url = req.url()?;
            parse_list_params(url.query().unwrap_or("").as_bytes())
        }
        // axum `Form` と同じく urlencoded 本文のみを受け付ける。
        Method::Post => {
            let content_type = req.headers().get("content-type")?.unwrap_or_default();
            let essence = content_type.split(';').next().unwrap_or("").trim();
            if !essence.eq_ignore_ascii_case(FORM_CONTENT_TYPE) {
                return json_error(
                    415,
                    "unsupported_media_type",
                    Some("expected request with `Content-Type: application/x-www-form-urlencoded`"),
                );
            }
            let body = req.bytes().await?;
            parse_list_params(&body)
        }
        _ => return Response::error("Method Not Allowed", 405),
    };
    let params = match params {
        Ok(params) => params,
        Err(message) => return json_error(400, "invalid_request", Some(&message)),
    };
    api_list_inner(&env, params).await
}

/// native `api_list_inner` (`src/web/novels.rs`) の写し。
async fn api_list_inner(env: &Env, params: ListParams) -> worker::Result<Response> {
    let draw = params.draw.unwrap_or(1);
    let return_all = params.all.unwrap_or(false);
    let start = if return_all {
        0
    } else {
        params.start.unwrap_or(0) as usize
    };
    let length = if return_all {
        None
    } else {
        Some(
            params
                .length
                .unwrap_or(50)
                .min(MAX_WEB_PAGE_LENGTH) as usize,
        )
    };
    let total_query_bytes = params.filter.as_ref().map_or(0, String::len)
        + params.search_value.as_ref().map_or(0, String::len);
    if total_query_bytes > MAX_WEB_SEARCH_BYTES {
        return json_error(400, "invalid_request", Some("search query is too long"));
    }

    let search = combine_search_terms(params.filter, params.search_value);
    let sort_column = match params.order_column.unwrap_or(0) {
        1 => LibrarySortColumn::LastUpdate,
        2 => LibrarySortColumn::GeneralLastup,
        3 => LibrarySortColumn::LastCheckDate,
        4 => LibrarySortColumn::Title,
        5 => LibrarySortColumn::Author,
        6 => LibrarySortColumn::SiteName,
        7 => LibrarySortColumn::NovelType,
        9 => LibrarySortColumn::GeneralAllNo,
        10 => LibrarySortColumn::Length,
        _ => LibrarySortColumn::Id,
    };
    let sort_order = if params.order_dir.as_deref() == Some("desc") {
        LibrarySortOrder::Descending
    } else {
        LibrarySortOrder::Ascending
    };
    let runtime = match crate::composition::WorkerRuntime::build(env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            worker::console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    let page = match runtime
        .services
        .library
        .list(&LibraryListRequest {
            search,
            start,
            length,
            sort_column,
            sort_order,
        })
        .await
    {
        Ok(page) => page,
        Err(error) => return map_application_error(error),
    };

    let data = page
        .data
        .into_iter()
        .map(|record| NovelListItem {
            id: record.id,
            title: record.title,
            author: record.author,
            sitename: record.sitename,
            novel_type: record.novel_type,
            end: record.end,
            last_update: record.last_update.timestamp(),
            general_lastup: record.general_lastup.map(|dt| dt.timestamp()),
            last_check_date: record.last_check_date.map(|dt| dt.timestamp()),
            new_arrivals_date: record.new_arrivals_date.map(|dt| dt.timestamp()),
            tags: record.tags,
            new_arrivals: record.new_arrivals,
            frozen: record.frozen,
            suspend: record.suspend,
            length: record.length,
            toc_url: record.toc_url,
            ncode: record.ncode,
            general_all_no: record.general_all_no,
        })
        .collect();

    Response::from_json(&NovelListResponse {
        draw,
        records_total: page.records_total,
        records_filtered: page.records_filtered,
        data,
    })
}

/// native `combine_search_terms` の写し。
/// `filter` (列フィルタ) と `search[value]` (全体検索) を空白区切りで AND 連結する。
fn combine_search_terms(filter: Option<String>, search: Option<String>) -> Option<String> {
    match (filter, search) {
        (Some(filter), Some(search)) if !filter.is_empty() && !search.is_empty() => {
            Some(format!("{filter} {search}"))
        }
        (Some(filter), _) if !filter.is_empty() => Some(filter),
        (_, Some(search)) if !search.is_empty() => Some(search),
        _ => None,
    }
}

/// `serde_urlencoded` (= `form_urlencoded`) と同じ規則で生バイト列から
/// `ListParams` を組み立てる:
/// - `&` 区切り、`=` が無い要素は値が空文字列、`+` は空白、`%XX` は
///   バイト復号 (不完全な `%` はリテラル)、UTF-8 は lossy。
/// - 未知キーは無視、既知キーの重複・型不一致はエラー (= native の 400)。
fn parse_list_params(raw: &[u8]) -> Result<ListParams, String> {
    const KNOWN_KEYS: [&str; 8] = [
        "draw",
        "start",
        "length",
        "all",
        "filter",
        "search[value]",
        "order[0][column]",
        "order[0][dir]",
    ];
    let mut params = ListParams::default();
    let mut seen = HashSet::new();
    for pair in raw.split(|&byte| byte == b'&') {
        let (key_raw, value_raw) = match pair.iter().position(|&byte| byte == b'=') {
            Some(index) => (&pair[..index], &pair[index + 1..]),
            None => (pair, &[][..]),
        };
        let key = decode_component(key_raw);
        if !KNOWN_KEYS.contains(&key.as_str()) {
            continue;
        }
        if !seen.insert(key.clone()) {
            return Err(format!("duplicate parameter `{key}`"));
        }
        let value = decode_component(value_raw);
        match key.as_str() {
            "draw" => params.draw = Some(parse_u64(&key, &value)?),
            "start" => params.start = Some(parse_u64(&key, &value)?),
            "length" => params.length = Some(parse_u64(&key, &value)?),
            "all" => params.all = Some(parse_bool(&key, &value)?),
            "filter" => params.filter = Some(value),
            "search[value]" => params.search_value = Some(value),
            "order[0][column]" => params.order_column = Some(parse_u64(&key, &value)?),
            "order[0][dir]" => params.order_dir = Some(value),
            _ => unreachable!("known key guard above"),
        }
    }
    Ok(params)
}

fn parse_u64(key: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid parameter `{key}`: expected unsigned integer"))
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    value
        .parse::<bool>()
        .map_err(|_| format!("invalid parameter `{key}`: expected `true` or `false`"))
}

/// `form_urlencoded` の 1 要素分のデコード。
/// `+` → 空白、`%XX` → バイト (後続 2 桁が hex でない `%` はリテラル)。
fn decode_component(raw: &[u8]) -> String {
    if !raw.contains(&b'%') && !raw.contains(&b'+') {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%'
                if index + 2 < raw.len()
                    && hex_value(raw[index + 1]).is_some()
                    && hex_value(raw[index + 2]).is_some() =>
            {
                out.push(hex_value(raw[index + 1]).unwrap() << 4 | hex_value(raw[index + 2]).unwrap());
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// native `map_application_error` と同じステータス割当。
/// `json_error` がクレート非公開なので JSON 形だけ合わせて自前で返す。
fn map_application_error(error: ApplicationError) -> worker::Result<Response> {
    let (status, code, message) = match error {
        ApplicationError::InvalidRequest(message) => (400, "invalid_request", message),
        ApplicationError::NotFound(message) => (404, "not_found", message),
        ApplicationError::Platform(message) => (500, "internal_error", message),
    };
    json_error(status, code, Some(&message))
}

/// `worker_entry::json_error` と同じ機械可読エラー形
/// (`{error: {code, message?}}`)。
fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    let payload = match message {
        Some(message) => serde_json::json!({ "error": { "code": code, "message": message } }),
        None => serde_json::json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}
