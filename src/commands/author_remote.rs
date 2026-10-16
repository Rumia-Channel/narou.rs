use std::sync::Arc;
use narou_rs::author::AuthorRecord;
use narou_rs::error::{NarouError, Result};
use narou_rs::native::http::NativeHttpClient;
use narou_rs::platform::{HttpClient, HttpMethod, HttpRequest, RedirectMode};
use serde_json::{Value, json};
use super::AuthorAction;

struct Remote { base: String, client: NativeHttpClient, headers: Vec<(String, String)> }
fn invalid_response() -> NarouError { NarouError::Http("作者 API の応答形式が不正です".into()) }
impl Remote {
    fn new(server: &str) -> Result<Self> {
        let url = url::Url::parse(server).map_err(|_| NarouError::InvalidTarget("--server には HTTP(S) のサーバー URL を指定してください".into()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err(NarouError::InvalidTarget("--server の URL に資格情報・クエリ・フラグメントは指定できません".into()));
        }
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        let env = |name| std::env::var(name).ok().filter(|value| !value.trim().is_empty());
        if let Some(token) = env("NAROU_ADMIN_TOKEN") { headers.push(("Authorization".into(), format!("Bearer {token}"))); }
        match (env("CF_ACCESS_CLIENT_ID"), env("CF_ACCESS_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => { headers.push(("CF-Access-Client-Id".into(), id)); headers.push(("CF-Access-Client-Secret".into(), secret)); }
            (None, None) => {}
            _ => return Err(NarouError::InvalidTarget("CF_ACCESS_CLIENT_ID と CF_ACCESS_CLIENT_SECRET は両方指定してください".into())),
        }
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36";
        Ok(Self { base: url.as_str().trim_end_matches('/').to_string(), client: NativeHttpClient::new(ua)?, headers })
    }
    async fn request(&self, method: HttpMethod, path: &str, body: Option<Value>) -> Result<Value> {
        let mut request = HttpRequest::get(format!("{}{path}", self.base));
        request.method = method;
        request.headers.clone_from(&self.headers);
        request.body = body.map(|body| serde_json::to_vec(&body).map(Arc::new)).transpose().map_err(|error| NarouError::Platform(error.to_string()))?;
        request.trusted_endpoint = true;
        // A login/Access redirect must never forward server credentials elsewhere.
        request.redirect = RedirectMode::Manual;
        let response = self.client.send(request).await?;
        let value = serde_json::from_slice::<Value>(&response.body).ok();
        if !(200..300).contains(&response.status) || value.as_ref().and_then(|value| value.get("success")).and_then(Value::as_bool) != Some(true) {
            let message = value.as_ref().and_then(|value| value.get("message").and_then(Value::as_str).or_else(|| value.pointer("/error/code").and_then(Value::as_str))).unwrap_or("作者 API にアクセスできません。URL と認証設定を確認してください");
            return Err(NarouError::Http(format!("HTTP {}: {message}", response.status)));
        }
        value.ok_or_else(invalid_response)
    }
    async fn list(&self) -> Result<Vec<AuthorRecord>> {
        let mut value = self.request(HttpMethod::Get, "/api/authors", None).await?;
        serde_json::from_value(value.get_mut("authors").ok_or_else(invalid_response)?.take()).map_err(|_| invalid_response())
    }
}

pub async fn run(action: AuthorAction, server: &str) -> Result<()> {
    let remote = Remote::new(server)?;
    match action {
        AuthorAction::List => {
            let authors = remote.list().await?;
            if authors.is_empty() { println!("追跡中の作者はいません。"); }
            else {
                println!("追跡中の作者: {} 件", authors.len());
                for (index, author) in authors.iter().enumerate() { println!("  {}. [{}] {}", index + 1, author.site, author.url); }
            }
        }
        AuthorAction::Add { url } => {
            let result = remote.request(HttpMethod::Post, "/api/authors", Some(json!({"url":url}))).await?;
            let changed = result.get("changed").and_then(Value::as_bool).ok_or_else(invalid_response)?;
            if changed { println!("作者を登録しました: {url}"); }
            else { println!("{url} は既に登録されています。"); }
        }
        AuthorAction::Remove { target } => {
            let url = if let Ok(position) = target.parse::<usize>() {
                let authors = remote.list().await?;
                authors.get(position.checked_sub(1).ok_or_else(|| NarouError::Database(format!("{target} 番目の作者はいません")))?)
                    .ok_or_else(|| NarouError::Database(format!("{target} 番目の作者はいません")))?.url.clone()
            } else { target };
            let result = remote.request(HttpMethod::Delete, "/api/authors", Some(json!({"url":url}))).await?;
            if result.get("changed").and_then(Value::as_bool).ok_or_else(invalid_response)? { println!("{url} の追跡をやめました。"); }
            else { println!("{url} は登録されていません。"); }
        }
        AuthorAction::Check { dry_run } => {
            let mut result = remote.request(HttpMethod::Post, "/api/authors/check", Some(json!({"dry_run":dry_run}))).await?;
            if dry_run {
                let urls: Vec<String> = serde_json::from_value(result.get_mut("urls").ok_or_else(invalid_response)?.take()).map_err(|_| invalid_response())?;
                for url in urls { println!("  {url}"); }
            } else {
                let count = result.get("count").and_then(Value::as_u64).ok_or_else(invalid_response)?;
                println!("作者の確認・取得ジョブを登録しました: {count} 件");
            }
        }
    }
    Ok(())
}
