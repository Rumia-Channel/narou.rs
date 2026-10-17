use std::sync::Arc;
use narou_rs::application::authors::{AuthorService, AuthorStore};
use narou_rs::author::{AuthorRecord, authors_from_value, authors_to_value};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::PlatformFuture;
use serde::Deserialize;
use serde_json::json;
use worker::{Env, Method, Request, Response};
use worker::wasm_bindgen::JsValue;
use crate::{composition::WorkerRuntime, db_handle::DbHandle};

// Exact snapshot CAS also handles the initially absent row without losing
// concurrent registrations. Keep the native inventory JSON/YAML pair readable.
pub(crate) const SAVE: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','author',?1,?2
WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='author' AND value_json=?3)
ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml
WHERE ?3 IS NOT NULL AND app_state.value_json=?3
RETURNING key
"#;
#[derive(Deserialize)]
struct Row { value_json: String }
#[derive(Deserialize)]
struct KeyRow { key: String }
fn error(value: impl std::fmt::Display) -> NarouError { NarouError::Platform(value.to_string()) }

pub struct D1AuthorStore { db: DbHandle }
impl D1AuthorStore {
    pub fn new(db: DbHandle) -> Self { Self { db: db.primary_view() } }
    async fn snapshot(&self) -> Result<Option<String>> {
        Ok(self.db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key='author'")
            .first::<Row>(None).await.map_err(error)?.map(|row| row.value_json))
    }
    async fn change(&self, url: &str, addition: Option<&AuthorRecord>) -> Result<bool> {
        for _ in 0..8 {
            let snapshot = self.snapshot().await?;
            let value = serde_yaml::from_str(snapshot.as_deref().unwrap_or("null"))?;
            let mut authors = authors_from_value(&value);
            let exists = authors.iter().any(|author| author.url == url);
            if exists == addition.is_some() { return Ok(false); }
            if let Some(author) = addition { authors.push(author.clone()); }
            else { authors.retain(|author| author.url != url); }
            let value = authors_to_value(&authors);
            let encoded = serde_json::to_string(&value).map_err(error)?;
            let yaml = serde_yaml::to_string(&value)?;
            let expected = snapshot.map(JsValue::from).unwrap_or(JsValue::NULL);
            let rows = self.db.prepare(SAVE).bind(&[encoded.into(), yaml.into(), expected]).map_err(error)?
                .all().await.map_err(error)?.results::<KeyRow>().map_err(error)?;
            if rows.iter().any(|row| row.key == "author") { return Ok(true); }
        }
        Err(error("作者一覧が同時に更新されています。再試行してください"))
    }
}
impl AuthorStore for D1AuthorStore {
    fn load(&self) -> PlatformFuture<'_, Result<Vec<AuthorRecord>>> {
        Box::pin(async move {
            let snapshot = self.snapshot().await?;
            Ok(authors_from_value(&serde_yaml::from_str(snapshot.as_deref().unwrap_or("null"))?))
        })
    }
    fn add<'a>(&'a self, author: &'a AuthorRecord) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(self.change(&author.url, Some(author)))
    }
    fn remove<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(self.change(url, None))
    }
}

pub fn service(db: DbHandle) -> AuthorService { AuthorService::new(Arc::new(D1AuthorStore::new(db))) }
#[derive(Deserialize)]
struct UrlBody { url: String }
#[derive(Deserialize, Default)]
struct CheckBody { #[serde(default)] dry_run: bool }

pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await { return response; }
    if !matches!((req.path().as_str(), req.method()), ("/api/authors", Method::Get | Method::Post | Method::Delete) | ("/api/authors/check", Method::Post)) {
        return Ok(Response::from_json(&json!({"success":false,"message":"Method Not Allowed"}))?.with_status(405));
    }
    let runtime = WorkerRuntime::build_ui(&env).await?;
    let result = handle_with(&mut req, &runtime).await;
    match result {
        Ok(value) => Response::from_json(&value),
        Err(err) => {
            let status = if matches!(err, NarouError::SiteSetting(_) | NarouError::InvalidTarget(_)) { 400 } else { 503 };
            Ok(Response::from_json(&json!({"success":false,"message":err.to_string()}))?.with_status(status))
        }
    }
}
async fn handle_with(req: &mut Request, runtime: &WorkerRuntime) -> Result<serde_json::Value> {
    let service = runtime.author_service();
    match (req.path().as_str(), req.method()) {
        ("/api/authors", Method::Get) => Ok(json!({"success":true,"authors":service.list().await?})),
        ("/api/authors", Method::Post) => {
            let body: UrlBody = req.json().await.map_err(|_| NarouError::InvalidTarget("invalid JSON body".into()))?;
            let sites = crate::bundled_sites::load_site_settings_uncached(&runtime.write_objects()).await?;
            Ok(json!({"success":true,"changed":service.add(&body.url, &sites).await?}))
        }
        ("/api/authors", Method::Delete) => {
            let body: UrlBody = req.json().await.map_err(|_| NarouError::InvalidTarget("invalid JSON body".into()))?;
            Ok(json!({"success":true,"changed":service.remove_url(&body.url).await?}))
        }
        ("/api/authors/check", Method::Post) => {
            let body: CheckBody = req.json().await.map_err(|_| NarouError::InvalidTarget("invalid JSON body".into()))?;
            if body.dry_run {
                let mut urls = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for author in service.list().await? {
                    for url in runtime.author_work_urls(&author.url).await? {
                        if seen.insert(url.clone()) { urls.push(url); }
                    }
                }
                Ok(json!({"success":true,"urls":urls}))
            } else {
                let jobs = runtime.enqueue_author_checks(false).await?;
                Ok(json!({"success":true,"count":jobs.len(),"job_ids":jobs.into_iter().map(|job| job.job_id).collect::<Vec<_>>()}))
            }
        }
        _ => Err(error("Method Not Allowed")),
    }
}
