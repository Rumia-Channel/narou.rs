//! Read-only browser integration; the existing UI authentication is authoritative.
//!
//! `HEAD` は本文を作らない: `GET` と同じヘッダだけを返す (EPUB の経路は
//! 変換も挿絵の stage も走らないので、`GET` の生成時間を使わない)。
use std::sync::Arc;

use narou_rs::application::external_api::ExternalListRequest;
use narou_rs::application::read_api::{
    ALLOWED_ORIGINS_SETTING, EXPOSE_HEADERS, PREFIX, origin_allowed, preflight_allowed,
};
use narou_rs::application::settings::SettingsStore;
use narou_rs::setting_core::SettingScope;
use worker::{Env, Method, Request, Response};

use crate::d1_repository::D1SettingsStore;
use crate::db_handle::DbHandle;

fn finish(mut response: Response, origin: Option<&str>) -> worker::Result<Response> {
    response.headers_mut().set("Cache-Control", "no-store")?;
    response.headers_mut().append("Vary", "Origin")?;
    if let Some(origin) = origin {
        response
            .headers_mut()
            .set("Access-Control-Allow-Origin", origin)?;
        response
            .headers_mut()
            .set("Access-Control-Allow-Credentials", "true")?;
        response
            .headers_mut()
            .set("Access-Control-Expose-Headers", EXPOSE_HEADERS)?;
    }
    Ok(response)
}

pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    let origin = req.headers().get("Origin")?;
    if let Some(origin) = origin.as_deref() {
        // Authorization policy reads must bypass isolate caches and replicas.
        let store = D1SettingsStore::for_writes(DbHandle::primary(Arc::new(env.d1("DB")?)));
        let settings = store
            .load(SettingScope::Global)
            .await
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let configured = settings
            .get(ALLOWED_ORIGINS_SETTING)
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !origin_allowed(configured, origin) {
            return finish(super::json_error(403, "origin_not_allowed", None)?, None);
        }
    }
    if req.method() == Method::Options && origin.is_some() {
        let method = req.headers().get("Access-Control-Request-Method")?;
        let headers = req.headers().get("Access-Control-Request-Headers")?;
        let mut response = if preflight_allowed(method.as_deref(), headers.as_deref()) {
            Response::empty()?.with_status(204)
        } else {
            super::json_error(403, "preflight_not_allowed", None)?
        };
        response
            .headers_mut()
            .set("Access-Control-Allow-Methods", "GET, HEAD")?;
        response
            .headers_mut()
            .set("Access-Control-Allow-Headers", "Range")?;
        return finish(response, origin.as_deref());
    }
    if !matches!(req.method(), Method::Get | Method::Head) {
        return finish(
            Response::error("Method Not Allowed", 405)?,
            origin.as_deref(),
        );
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return finish(response?, origin.as_deref());
    }
    let response = read(&req, &env).await?;
    finish(response, origin.as_deref())
}

async fn read(req: &Request, env: &Env) -> worker::Result<Response> {
    let path = req.path();
    let relative = path.strip_prefix(PREFIX).unwrap_or("");
    if relative == "/favorite-tags" {
        let db = DbHandle::primary(Arc::new(env.d1("DB")?));
        return match crate::favorite_tags::load_with_db(&db).await {
            Ok(state) => Response::from_json(&state.payload()),
            Err(error) => super::json_error(500, "internal_error", Some(&error.to_string())),
        };
    }
    if relative == "/novels" {
        let services = crate::composition::build_read_services(env).await?;
        let pairs: Vec<_> = req.url()?.query_pairs().into_owned().collect();
        let query = match ExternalListRequest::from_pairs(&pairs) {
            Ok(query) => query,
            Err(error) => return super::application_error_response(&error),
        };
        let hidden = if query.include_hidden {
            None
        } else {
            crate::hidden::load_from_env(env).await.ok()
        };
        return match services.app.external_api.list(&query, hidden).await {
            Ok(list) => Response::from_json(&list.to_json()),
            Err(error) => super::application_error_response(&error),
        };
    }
    let Some(novel_path) = relative.strip_prefix("/novels/") else {
        return Response::error("Not Found", 404);
    };
    let (id, epub) = novel_path
        .strip_suffix("/epub")
        .map_or((novel_path, false), |id| (id, true));
    let Ok(id) = id.parse::<i64>() else {
        return super::json_error(400, "bad_request", Some("Invalid novel id"));
    };
    if epub {
        return crate::build_epub_response(req, env, id, false).await;
    }
    let services = crate::composition::build_read_services(env).await?;
    match services.app.external_api.novel(id).await {
        Ok(novel) => Response::from_json(&novel),
        Err(error) => super::application_error_response(&error),
    }
}
