//! サイト定義（`webnovel/*.yaml`）の管理 API。
//!
//! 保存先は保存方式に応じて選ばれる（YAML モードは `webnovel/` フォルダ、SQLite
//! モードはオブジェクトストア）。応答形は Worker の `/api/sites*` と同じで、
//! 実体は core の [`SiteDefinitions`] に集約してある。

use axum::Json;
use axum::extract::Path;

use narou_rs::application::site_definitions::{
    error_payload, list_payload, show_payload,
};

/// 管理用サービス（保存方式に応じたストアを使う）。
async fn service() -> Result<narou_rs::application::site_definitions::SiteDefinitions, String> {
    narou_rs::native::site_definitions::site_definition_service()
        .await
        .map_err(|error| error.to_string())
}

/// `GET /api/sites` — bundle とユーザー定義の一覧。
pub async fn sites_list() -> Json<serde_json::Value> {
    let service = match service().await {
        Ok(service) => service,
        Err(message) => return Json(error_payload(&message)),
    };
    match service.list().await {
        Ok(definitions) => Json(list_payload(&definitions)),
        Err(error) => Json(error_payload(&error.to_string())),
    }
}

/// `GET /api/sites/{name}` — 実効定義（ユーザー定義があればそれ、無ければ bundle）。
pub async fn site_show(Path(name): Path<String>) -> Json<serde_json::Value> {
    let service = match service().await {
        Ok(service) => service,
        Err(message) => return Json(error_payload(&message)),
    };
    match service.get(&name).await {
        Ok(Some(definition)) => Json(show_payload(&definition)),
        Ok(None) => Json(error_payload(&format!("no such site definition: {name}"))),
        Err(error) => Json(error_payload(&error.to_string())),
    }
}

/// `PUT /api/sites/{name}` — 本文（YAML）で置き換える。
pub async fn site_put(Path(name): Path<String>, body: String) -> Json<serde_json::Value> {
    let service = match service().await {
        Ok(service) => service,
        Err(message) => return Json(error_payload(&message)),
    };
    if let Err(error) = service.put(&name, &body).await {
        return Json(error_payload(&error.to_string()));
    }
    match service.list().await {
        Ok(definitions) => Json(list_payload(&definitions)),
        Err(error) => Json(error_payload(&error.to_string())),
    }
}

/// `DELETE /api/sites/{name}` — ユーザー定義を消して bundle に戻す。
pub async fn site_delete(Path(name): Path<String>) -> Json<serde_json::Value> {
    let service = match service().await {
        Ok(service) => service,
        Err(message) => return Json(error_payload(&message)),
    };
    if let Err(error) = service.delete(&name).await {
        return Json(error_payload(&error.to_string()));
    }
    match service.list().await {
        Ok(definitions) => Json(list_payload(&definitions)),
        Err(error) => Json(error_payload(&error.to_string())),
    }
}
