use std::collections::HashSet;
use std::sync::Arc;
use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::{Value, json};
use crate::application::authors::{AuthorService, new_work_urls};
use crate::error::{NarouError, Result};
use crate::native::author_store::NativeAuthorStore;
use super::{AppState, state::DownloadBody};

#[derive(Deserialize)]
pub struct UrlBody { pub url: String }
#[derive(Deserialize)]
pub struct CheckBody { #[serde(default)] pub dry_run: bool }
fn service() -> Result<AuthorService> { Ok(AuthorService::new(Arc::new(NativeAuthorStore::for_current_root()?))) }
fn response(result: Result<Value>) -> Json<Value> {
    Json(result.unwrap_or_else(|error| json!({"success":false,"message":error.to_string()})))
}

pub async fn list() -> Json<Value> {
    response(async { Ok(json!({"success":true,"authors":service()?.list().await?})) }.await)
}
pub async fn add(Json(body): Json<UrlBody>) -> Json<Value> {
    response(async {
        let sites = crate::downloader::site_setting::effective_site_settings();
        Ok(json!({"success":true,"changed":service()?.add(&body.url, &sites).await?}))
    }.await)
}
pub async fn remove(Json(body): Json<UrlBody>) -> Json<Value> {
    response(async { Ok(json!({"success":true,"changed":service()?.remove_url(&body.url).await?})) }.await)
}
pub async fn check(State(state): State<AppState>, Json(body): Json<CheckBody>) -> Json<Value> {
    response(check_inner(state, body).await)
}
async fn check_inner(state: AppState, body: CheckBody) -> Result<Value> {
    let authors = service()?.list().await?;
    if authors.is_empty() {
        return Ok(if body.dry_run { json!({"success":true,"urls":[]}) } else { json!({"success":true,"count":0,"job_ids":[]}) });
    }
    let downloader = crate::downloader::Downloader::with_user_agent(None)?;
    let expander = downloader.download_target_expander();
    let sites = crate::downloader::site_setting::effective_site_settings();
    let novels = crate::native::novel_repository::NativeNovelRepository::new();
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for author in authors {
        for url in new_work_urls(&author.url, &expander, &sites, &novels).await? {
            if seen.insert(url.clone()) { urls.push(url); }
        }
    }
    if body.dry_run { return Ok(json!({"success":true,"urls":urls})); }
    let limit = super::max_web_targets_per_request(&state).await.max(1);
    let mut urls = urls.into_iter().peekable();
    let mut ids = Vec::new();
    while urls.peek().is_some() {
        let targets = urls.by_ref().take(limit).collect();
        let Json(result) = super::jobs::api_download(State(state.clone()), Json(DownloadBody { targets, force: false, mail: false })).await;
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(NarouError::Platform(result.get("message").and_then(Value::as_str).unwrap_or("作者作品の取得を登録できませんでした").to_string()));
        }
        if let Some(results) = result.get("results").and_then(Value::as_array) {
            ids.extend(results.iter().filter_map(|result| result.get("job_id").and_then(Value::as_str)).map(str::to_string));
        }
    }
    Ok(json!({"success":true,"count":ids.len(),"job_ids":ids}))
}
