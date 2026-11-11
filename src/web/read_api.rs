//! Browser reads remain behind the normal UI authentication middleware.
use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

use super::AppState;
use crate::application::ApplicationError;
use crate::application::external_api::{ExternalListRequest, http_error};
use crate::application::read_api::{
    ALLOWED_ORIGINS_SETTING, EXPOSE_HEADERS, is_read_path, origin_allowed, preflight_allowed,
};
use crate::platform::NovelId;

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}

fn application_error(error_value: ApplicationError) -> Response {
    let (status, code) = http_error(&error_value);
    error(
        StatusCode::from_u16(status).expect("application HTTP status"),
        code,
        error_value.message(),
    )
}

pub async fn cors(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !is_read_path(request.uri().path()) {
        return next.run(request).await;
    }
    let origin = request.headers().get(header::ORIGIN).cloned();
    let allowed = if let Some(origin) = origin.as_ref() {
        let configured = match state.services.settings.get(ALLOWED_ORIGINS_SETTING).await {
            Ok(value) => value,
            Err(error_value) => return application_error(error_value),
        };
        origin.to_str().is_ok_and(|origin| {
            origin_allowed(
                configured
                    .as_ref()
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                origin,
            )
        })
    } else {
        true
    };
    if !allowed {
        return error(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "Origin is not allowed",
        );
    }
    let mut response = if request.method() == Method::OPTIONS && origin.is_some() {
        let method = request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_METHOD)
            .and_then(|value| value.to_str().ok());
        let headers = request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .and_then(|value| value.to_str().ok());
        if preflight_allowed(method, headers) {
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, HEAD"),
            );
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Range"),
            );
            response
        } else {
            error(
                StatusCode::FORBIDDEN,
                "preflight_not_allowed",
                "Read-only preflight required",
            )
        }
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(origin) = origin {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        response.headers_mut().insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(EXPOSE_HEADERS),
        );
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

pub async fn novels(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let pairs: Vec<_> = params.into_iter().collect();
    let request = match ExternalListRequest::from_pairs(&pairs) {
        Ok(request) => request,
        Err(error_value) => return application_error(error_value),
    };
    let hidden = crate::native::hidden::load().unwrap_or_default();
    match state
        .services
        .external_api
        .list(&request, Some(hidden))
        .await
    {
        Ok(list) => Json(list.to_json()).into_response(),
        Err(error_value) => application_error(error_value),
    }
}

pub async fn novel(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.services.external_api.novel(id).await {
        Ok(novel) => Json(novel).into_response(),
        Err(error_value) => application_error(error_value),
    }
}

pub async fn epub(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let record = match state.services.library.get(NovelId(id)).await {
        Ok(Some(record)) => record,
        Ok(None) => return error(StatusCode::NOT_FOUND, "not_found", "Novel not found"),
        Err(error_value) => return application_error(error_value),
    };
    let novel_dir = match crate::db::with_database(|db| {
        super::safe_existing_novel_dir(db.archive_root(), &record)
            .map_err(crate::error::NarouError::Database)
    }) {
        Ok(dir) => dir,
        Err(error_value) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &error_value.to_string(),
            );
        }
    };
    match super::novels::serve_epub(&state, id, &record, &novel_dir).await {
        Ok(response) => response,
        Err((status, message)) => error(status, "internal_error", &message),
    }
}
