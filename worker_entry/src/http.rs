use std::sync::Arc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    CookieStore, HttpClient, HttpMethod, HttpRequest, HttpResponse, PlatformFuture, RedirectMode,
};
use wasm_bindgen::JsValue;
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

use crate::budget::SubrequestBudget;

/// 1 レスポンスの上限。
///
/// Workers の 128 MiB 予算のうち、取得したバイト列に許す枠。実データのうごイラは
/// zip 4.3 MiB (19 フレーム 1920×1080) で、挿絵も数 MB なので 16 MiB で足りる。
/// 大きいのは「入力」ではなく APNG の出力側で、そちらは
/// `illustration_animation` の上限 (40 MiB) で別に制限している。
pub const MAX_WORKER_HTTP_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct WorkerHttpClient {
    subrequests: SubrequestBudget,
    /// 保存済みログイン Cookie。`Set-Cookie` の書き戻しにだけ使う。
    cookie_store: Option<Arc<dyn CookieStore>>,
}

impl std::fmt::Debug for WorkerHttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerHttpClient")
            .field("cookie_store", &self.cookie_store.is_some())
            .finish()
    }
}

impl WorkerHttpClient {
    pub fn new(subrequests: SubrequestBudget) -> Self {
        Self {
            subrequests,
            cookie_store: None,
        }
    }

    /// 保存済み Cookie を `Set-Cookie` で更新できるようにする (native と同じ挙動)。
    pub fn with_cookie_store(mut self, store: Arc<dyn CookieStore>) -> Self {
        self.cookie_store = Some(store);
        self
    }

    /// 送信した Cookie に対応する資格情報だけを `Set-Cookie` で更新する。
    ///
    /// サイトは複数のログイン情報を持てるので、送っていない資格情報を書き換えては
    /// いけない (別アカウントのセッションを壊す)。保存値は暗号化されて書き戻る。
    async fn persist_set_cookie(
        &self,
        url: &str,
        response: &HttpResponse,
        sent_cookie: Option<&str>,
    ) {
        let Some(store) = self.cookie_store.as_ref() else {
            return;
        };
        let values: Vec<String> = response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
            .collect();
        if values.is_empty() {
            return;
        }
        let Some(host) = narou_rs::platform::cookie_host_for_url(url) else {
            return;
        };
        let Some(sent) = sent_cookie else {
            return;
        };
        let sent_pairs = narou_rs::platform::parse_cookie_header(sent);
        if sent_pairs.is_empty() {
            return;
        }
        let Ok(all) = store.list().await else {
            return;
        };
        for key in narou_rs::platform::cookie_lookup_hosts(&host) {
            let Some(credentials) = all.get(&key) else {
                continue;
            };
            let mut updated = credentials.clone();
            let mut changed = false;
            for credential in updated.iter_mut() {
                if !narou_rs::platform::credential_was_sent(&credential.cookie, &sent_pairs) {
                    continue;
                }
                let cookie = narou_rs::platform::apply_set_cookie(&credential.cookie, &values);
                if cookie != credential.cookie {
                    credential.cookie = cookie;
                    changed = true;
                }
            }
            if changed {
                let _ = store.save_all(&key, &updated).await;
            }
        }
    }

    async fn send_request(&self, request: HttpRequest) -> Result<HttpResponse> {
        let method = match request.method {
            HttpMethod::Get => Method::Get,
            HttpMethod::Post => Method::Post,
        };
        let redirect = match request.redirect {
            RedirectMode::Follow => RequestRedirect::Follow,
            RedirectMode::Manual => RequestRedirect::Manual,
        };
        let headers = Headers::new();
        for (name, value) in &request.headers {
            headers
                .append(name, value)
                .map_err(|error| NarouError::Platform(format!("invalid HTTP header: {error}")))?;
        }
        let mut init = RequestInit::new();
        init.with_method(method);
        init.with_redirect(redirect);
        init.with_headers(headers);
        if let Some(body) = request.body {
            if body.len() > MAX_WORKER_HTTP_BODY {
                return Err(NarouError::Platform("Worker HTTP request body exceeds limit".to_string()));
            }
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(body.as_slice()))));
        }
        let request = Request::new_with_init(&request.url, &init)
            .map_err(|error| NarouError::Platform(format!("invalid HTTP request: {error}")))?;
        self.subrequests.record();
        let mut response = Fetch::Request(request)
            .send()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        let status = response.status_code();
        let headers = response
            .headers()
            .entries()
            .collect::<Vec<_>>();
        if response
            .headers()
            .get("content-length")
            .map_err(|error| NarouError::Platform(format!("invalid HTTP response headers: {error}")))?
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|size| size > MAX_WORKER_HTTP_BODY)
        {
            return Err(NarouError::Platform("Worker HTTP response body exceeds limit".to_string()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        if body.len() > MAX_WORKER_HTTP_BODY {
            return Err(NarouError::Platform("Worker HTTP response body exceeds limit".to_string()));
        }
        Ok(HttpResponse { status, headers, body })
    }
}

impl HttpClient for WorkerHttpClient {
    /// wasm には DNS 解決手段が無いので、構文とアドレスリテラルの判定だけを
    /// 行う (到達可否は Cloudflare の egress 側の制約に委ねる)。ホスト名が
    /// 非公開アドレスに解決される場合を弾けない点が native との差。
    fn validate_url<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            narou_rs::platform::url_policy::validate_url_syntax(url)
                .map_err(narou_rs::error::NarouError::Http)
        })
    }

    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        let url = request.url.clone();
        let sent_cookie = request.header("Cookie").map(str::to_string);
        Box::pin(async move {
            let response = self.send_request(request).await?;
            self.persist_set_cookie(&url, &response, sent_cookie.as_deref())
                .await;
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_limit_is_explicit_and_shared() {
        assert_eq!(MAX_WORKER_HTTP_BODY, 16 * 1024 * 1024);
    }
}
