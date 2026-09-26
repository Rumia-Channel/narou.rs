//! サイト定義（`webnovel/*.yaml`）の管理 API。
//!
//! native では初期化フォルダの `webnovel/*.yaml` をユーザーが編集・差し替え
//! できる。Worker にはファイルシステムが無いので、同じ役割をオブジェクト
//! ストア（D1）の `webnovel/<name>.yaml` に置く。bundle 済み定義は種と
//! フォールバックで、同名のユーザー定義が上書きする。
//!
//! 実体は core の [`SiteDefinitions`] にあり、native の `/api/sites*` と
//! 同じ規則・同じ応答形（`{success, data|message}`）を返す。

use narou_rs::error::Result;
use worker::{Request, Response};

use crate::composition::WorkerRuntime;

fn json(value: serde_json::Value) -> Result<Response> {
    Ok(Response::from_json(&value)
        .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?)
}

fn error_response(message: &str) -> Result<Response> {
    json(narou_rs::application::site_definitions::error_payload(message))
}

/// 一覧（bundle + ユーザー定義）。
pub async fn list(runtime: &WorkerRuntime) -> Result<Response> {
    let service = crate::bundled_sites::site_definitions(runtime.objects());
    match service.list().await {
        Ok(definitions) => json(narou_rs::application::site_definitions::list_payload(
            &definitions,
        )),
        Err(error) => error_response(&error.to_string()),
    }
}

/// 1 件の実効定義（編集用に本文を返す）。
pub async fn show(runtime: &WorkerRuntime, name: &str) -> Result<Response> {
    let service = crate::bundled_sites::site_definitions(runtime.objects());
    match service.get(name).await {
        Ok(Some(definition)) => json(
            narou_rs::application::site_definitions::show_payload(&definition),
        ),
        Ok(None) => error_response(&format!("no such site definition: {name}")),
        Err(error) => error_response(&error.to_string()),
    }
}

/// 置き換え（YAML 本文）。
pub async fn put(runtime: &WorkerRuntime, name: &str, mut request: Request) -> Result<Response> {
    let body = match request.text().await {
        Ok(body) => body,
        Err(error) => return error_response(&error.to_string()),
    };
    let service = crate::bundled_sites::site_definitions(runtime.objects());
    if let Err(error) = service.put(name, &body).await {
        return error_response(&error.to_string());
    }
    list(runtime).await
}

/// 削除（bundle があればそれに戻る）。
pub async fn delete(runtime: &WorkerRuntime, name: &str) -> Result<Response> {
    let service = crate::bundled_sites::site_definitions(runtime.objects());
    if let Err(error) = service.delete(name).await {
        return error_response(&error.to_string());
    }
    list(runtime).await
}

/// ルートから呼ぶ入口（認証は呼び出し側で済ませる）。
pub async fn handle(
    runtime: &WorkerRuntime,
    request: Request,
    name: Option<&str>,
) -> Result<Response> {
    match (request.method(), name) {
        (worker::Method::Get, None) => list(runtime).await,
        (worker::Method::Get, Some(name)) => show(runtime, name).await,
        (worker::Method::Put, Some(name)) => put(runtime, name, request).await,
        (worker::Method::Delete, Some(name)) => delete(runtime, name).await,
        _ => Ok(Response::error("Method Not Allowed", 405)
            .map_err(|error| narou_rs::error::NarouError::Platform(error.to_string()))?),
    }
}
