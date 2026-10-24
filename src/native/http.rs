//! Native HTTP transport: curl → reqwest → subprocess (curl/wget) tier fallback.
//!
//! This is the native implementation of [`HttpClient`]. It owns everything
//! transport-specific: the manual-redirect reqwest client, libcurl handle
//! configuration, subprocess fallback (curl.exe on Windows, wget elsewhere),
//! and per-domain tier-failure bookkeeping. The async entry point owns the
//! shared redirect chain; every concrete transport executes just one hop.
//!
//! It never decodes response bytes or maps statuses to domain errors — that
//! is `downloader::http_policy`'s job. 404/503 stop fallback immediately;
//! other 4xx/5xx let later tiers try, but remain available as raw responses
//! if no tier returns a successful or definitive response.
//!
//! Site admission is injected by `send_paced`. Each redirect or fallback
//! attempt acquires its own permit; infrastructure `send` remains unpaced.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use reqwest::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONNECTION, HeaderMap, HeaderValue,
};

use crate::compat::configure_hidden_console_command;
use crate::downloader::security::{
    CONNECT_TIMEOUT_SECS, MAX_REDIRECTS, MAX_RESPONSE_BYTES, READ_TIMEOUT_SECS, TOTAL_TIMEOUT_SECS,
    validate_public_url,
};
use crate::error::{NarouError, Result};
use crate::platform::http::{
    RedirectChain, is_safe_header_name, is_safe_header_value, response_body_limit,
    validate_http_url_syntax,
};
use crate::platform::http1::MAX_HTTP1_HEAD;
use crate::platform::{HttpClient, HttpMethod, HttpRequest, HttpResponse, RedirectMode};

const FAIL_THRESHOLD: u8 = 5;

/// Default site-fetch headers, shared by every native transport so the
/// reqwest default header map, the libcurl handle and both subprocess tools
/// send the same values.
const DEFAULT_ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8";
const DEFAULT_ACCEPT_LANGUAGE: &str = "ja,en-US;q=0.9,en;q=0.8";
const DEFAULT_ACCEPT_ENCODING: &str = "gzip, deflate";
const DEFAULT_ACCEPT_CHARSET: &str = "utf-8";
const DEFAULT_CONNECTION: &str = "keep-alive";

/// フォールバック段の番号 (試行順は `TierState::order` が決める)。
const TIER_CURL: usize = 0;
const TIER_REQWEST: usize = 1;
const TIER_SUBPROCESS: usize = 2;
const TIER_COUNT: usize = 3;

fn should_try_next_tier(status: u16) -> bool {
    (400..600).contains(&status) && status != 404 && status != 503
}

/// ドメインごとの段の実績。
///
/// `preferred` は「そのドメインで最後に成功した段」で、次のリクエストでは
/// これを最初に試す。以前は libcurl の成功だけを覚えていたため、
/// wget が勝つサイトでは毎回 curl と reqwest を空打ちしていた。
/// 成功した段の失敗数は 0 に戻すので、一時的な障害だけで段が実行中ずっと
/// 外れることもない。
#[derive(Default)]
struct TierState {
    preferred: HashMap<String, usize>,
    failures: HashMap<String, [u8; TIER_COUNT]>,
}

impl TierState {
    /// このドメインで試す段の順序。直近で成功した段を先頭に置く。
    fn order(&self, domain: &str) -> Vec<usize> {
        let mut order = Vec::with_capacity(TIER_COUNT);
        if let Some(&preferred) = self.preferred.get(domain)
            && preferred < TIER_COUNT
        {
            order.push(preferred);
        }
        for tier in 0..TIER_COUNT {
            if !order.contains(&tier) {
                order.push(tier);
            }
        }
        order
    }

    /// 連続失敗が閾値に達した段はそのドメインでは試さない。
    fn should_try(&self, domain: &str, tier: usize) -> bool {
        self.failures.get(domain).map_or(0, |f| f[tier]) < FAIL_THRESHOLD
    }

    fn note_success(&mut self, domain: &str, tier: usize) {
        self.preferred.insert(domain.to_string(), tier);
        if let Some(failures) = self.failures.get_mut(domain) {
            failures[tier] = 0;
        }
    }

    fn note_failure(&mut self, domain: &str, tier: usize) {
        let failures = self.failures.entry(domain.to_string()).or_insert([0; TIER_COUNT]);
        failures[tier] = failures[tier].saturating_add(1);
        if self.preferred.get(domain) == Some(&tier) {
            self.preferred.remove(domain);
        }
    }

    /// 全段が失敗閾値に達したドメインのカウンタを捨ててやり直せるようにする。
    fn forget(&mut self, domain: &str) {
        self.failures.remove(domain);
        self.preferred.remove(domain);
    }
}

/// Native HTTP transport with the curl → reqwest → subprocess fallback.
///
/// `Clone` is cheap: the reqwest client is `Arc`-backed and the
/// per-domain tier state is shared through `Arc`, so clones observe the same
/// fallback bookkeeping (matching the old `HttpFetcher` semantics where one
/// fetcher served a whole download run).
#[derive(Clone)]
pub struct NativeHttpClient {
    client: reqwest::blocking::Client,
    user_agent: String,
    tiers: Arc<Mutex<TierState>>,
    /// Login cookies are only refreshed for hosts that already store one, so a
    /// site the user never signed in to never gains an entry here.
    cookie_store: Option<Arc<dyn crate::platform::CookieStore>>,
}


impl NativeHttpClient {
    /// Refresh stored login cookies from `Set-Cookie` responses.
    pub fn with_cookie_store(mut self, store: Arc<dyn crate::platform::CookieStore>) -> Self {
        self.cookie_store = Some(store);
        self
    }

    pub fn new(user_agent: &str) -> Result<Self> {
        if tokio::runtime::Handle::try_current().is_ok() {
            let user_agent = user_agent.to_string();
            return std::thread::Builder::new()
                .name("narou-native-http-init".to_string())
                .spawn(move || Self::new_inner(&user_agent))
                .map_err(|error| NarouError::Platform(error.to_string()))?
                .join()
                .map_err(|_| NarouError::Platform("native HTTP initialization thread panicked".into()))?;
        }
        Self::new_inner(user_agent)
    }

    fn new_inner(user_agent: &str) -> Result<Self> {
        let client = build_reqwest_client(user_agent)?;

        Ok(Self {
            client,
            user_agent: user_agent.to_string(),
            tiers: Arc::new(Mutex::new(TierState::default())),
            cookie_store: None,
        })
    }

    /// Validate one outgoing hop before either admission or network I/O.
    fn validate_hop(request: &HttpRequest) -> Result<()> {
        if request.trusted_endpoint {
            validate_http_url_syntax(&request.url)?;
        } else {
            validate_public_url(&request.url).map_err(io_error)?;
        }
        for (name, value) in &request.headers {
            if !is_safe_header_name(name) || !is_safe_header_value(value) {
                return Err(io_error("unsafe HTTP header"));
            }
        }
        Ok(())
    }

    async fn send_chain(
        &self,
        request: HttpRequest,
        pacing: Option<crate::platform::http::HttpPacing<'_>>,
    ) -> Result<HttpResponse> {
        let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
        loop {
            let (returned_chain, validation) = tokio::task::spawn_blocking(move || {
                let validation = Self::validate_hop(chain.request());
                (chain, validation)
            }).await.map_err(|error| NarouError::Http(error.to_string()))?;
            chain = returned_chain;
            validation?;
            let (returned_chain, response) = self.send_validated_hop(chain, pacing).await?;
            chain = returned_chain;
            if let Some(store) = self.cookie_store.as_deref() {
                crate::platform::cookie_store::persist_response_set_cookie(
                    store, &response, chain.request().header("Cookie"),
                ).await;
            }
            if !chain.advance(&response)? { return Ok(response); }
        }
    }

    /// Dispatch one already-validated hop without changing its trust policy.
    async fn send_validated_hop(
        &self,
        mut chain: RedirectChain,
        pacing: Option<crate::platform::http::HttpPacing<'_>>,
    ) -> Result<(RedirectChain, HttpResponse)> {
        let scope = pacing.map(|policy| policy.scope_for(&chain)).transpose()?;
        let tiered = chain.request().method == HttpMethod::Get && !chain.request().trusted_endpoint;
        let domain = tiered.then(|| crate::downloader::http_policy::domain_of(&chain.request().url).to_string());
        let mut order = domain.as_deref().map(|domain| self.tier_order(domain)).unwrap_or_default();
        // Manual URL resolution retains curl-first CDN compatibility.
        if tiered && chain.request().redirect == RedirectMode::Manual {
            order.retain(|&tier| tier != TIER_CURL);
            order.insert(0, TIER_CURL);
        }
        let attempts = if tiered { order.as_slice() } else { &[TIER_REQWEST] };
        let mut response = None;
        let mut last_error = None;
        for &tier in attempts {
            if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) {
                policy.limiter.acquire(scope).await?;
            }
            let worker = self.clone();
            let (returned_chain, attempt) = tokio::task::spawn_blocking(move || {
                let request = chain.request();
                let user_agent = request.header("User-Agent").unwrap_or(&worker.user_agent);
                let attempt = if !tiered {
                    worker.send_reqwest_hop(request)
                } else {
                    match tier {
                        TIER_CURL => worker.fetch_tier_curl(request, user_agent),
                        TIER_REQWEST => worker.send_reqwest_hop(request),
                        TIER_SUBPROCESS => worker.fetch_tier_subprocess(request, user_agent),
                        _ => unreachable!("tier index out of range: {tier}"),
                    }
                };
                (chain, attempt)
            }).await.map_err(|error| NarouError::Http(error.to_string()))?;
            chain = returned_chain;
            match attempt {
                Ok(value) => {
                    if tiered && should_try_next_tier(value.status) {
                        if let Some(domain) = domain.as_deref() { self.tiers.lock().note_failure(domain, tier); }
                        response = Some(value);
                        continue;
                    }
                    if let Some(domain) = domain.as_deref() { self.tiers.lock().note_success(domain, tier); }
                    response = Some(value);
                    break;
                }
                Err(error) => {
                    if let Some(domain) = domain.as_deref() { self.tiers.lock().note_failure(domain, tier); }
                    last_error = Some(error);
                }
            }
        }
        let response = response.ok_or_else(|| last_error.unwrap_or_else(|| {
            NarouError::NotFound(chain.request().url.clone())
        }))?;
        Ok((chain, response))
    }

    /// Non-GET and trusted infrastructure use only manual-redirect reqwest.
    fn send_reqwest_hop(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let mut builder = match request.method {
            HttpMethod::Get => self.client.get(&request.url),
            HttpMethod::Post => self.client.post(&request.url),
            HttpMethod::Put => self.client.put(&request.url),
            HttpMethod::Delete => self.client.delete(&request.url),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(reqwest::blocking::Body::sized(
                SharedRequestBody { body: Arc::clone(body), position: 0 },
                body.len() as u64,
            ));
        }
        let response = builder.send().map_err(|e| NarouError::Http(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        let trusted_limit = request.trusted_endpoint
            .then_some(request.trusted_response_body_limit).flatten();
        let limit = response_body_limit(status, trusted_limit, MAX_RESPONSE_BYTES);
        let body = read_response_bytes(response, limit)?;
        Ok(HttpResponse { status, headers, body })
    }


    /// このドメインで試す段を、直近成功した段を先頭にした順序で返す。
    ///
    /// 全段が失敗閾値に達している場合はカウンタを捨てて通常順序に戻す。
    /// そうしないと一時的な障害で全段が外れ、以降そのドメインの取得が
    /// すべて失敗してしまう。
    fn tier_order(&self, domain: &str) -> Vec<usize> {
        let mut state = self.tiers.lock();
        let order: Vec<usize> = state
            .order(domain)
            .into_iter()
            .filter(|&tier| state.should_try(domain, tier))
            .collect();
        if order.is_empty() {
            state.forget(domain);
            return (0..TIER_COUNT).collect();
        }
        order
    }


    fn fetch_tier_curl(
        &self,
        request: &HttpRequest,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        let url = request.url.as_str();
        let mut handle = self.configured_curl_handle(request, user_agent)?;

        let mut body = Vec::new();
        let mut head = CurlHeaders::default();
        let mut response_too_large = false;
        let perform_result = {
            let mut transfer = handle.transfer();
            transfer
                .write_function(|data| {
                    if body.len() + data.len() > MAX_RESPONSE_BYTES {
                        response_too_large = true;
                        return Ok(0);
                    }
                    body.extend_from_slice(data);
                    Ok(data.len())
                })
                .map_err(|e| io_error(e.to_string()))?;
            transfer
                .header_function(|line| {
                    head.feed(line);
                    true
                })
                .map_err(|e| io_error(e.to_string()))?;
            transfer.perform()
        };
        if response_too_large {
            return Err(io_error(format!(
                "response body exceeded {} bytes while fetching {url}",
                MAX_RESPONSE_BYTES
            )));
        }
        perform_result.map_err(|e| io_error(e.to_string()))?;

        let code = handle
            .response_code()
            .map_err(|e| io_error(e.to_string()))?;
        if !(100..600).contains(&code) {
            return Err(io_error(format!("invalid HTTP status {code} while fetching {url}")));
        }

        Ok(HttpResponse {
            status: code as u16,
            headers: head.into_headers(),
            body,
        })
    }


    /// Last-resort subprocess fetch. Windows has no `wget` but ships
    /// `curl.exe` since Windows 10, so the candidate order is
    /// platform-dependent: `curl` first on Windows, `wget` first elsewhere.
    /// A tool that is not installed falls through to the next candidate;
    /// HTTP error responses retain their status while later candidates get
    /// a chance to succeed; incomplete responses remain transport failures.
    fn fetch_tier_subprocess(
        &self,
        request: &HttpRequest,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        let url = &request.url;
        for (_, value) in &request.headers {
            if !is_safe_header_value(value) {
                return Err(io_error("unsafe header value"));
            }
        }

        let candidates: &[&str] = if cfg!(windows) {
            &["curl", "wget"]
        } else {
            &["wget", "curl"]
        };
        let mut first_error: Option<NarouError> = None;
        let mut missing: Vec<&str> = Vec::new();
        let mut response = None;
        for tool in candidates {
            match self.run_fetch_tool(tool, request, user_agent) {
                Ok(value) if should_try_next_tier(value.status) => response = Some(value),
                Ok(value) => return Ok(value),
                Err(err) => {
                    let not_found = matches!(
                        &err,
                        NarouError::Io(e) if e.kind() == std::io::ErrorKind::NotFound
                    );
                    if not_found {
                        missing.push(tool);
                    } else {
                        tracing::warn!("{tool} fetch failed for {url}: {err}");
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                }
            }
        }
        response.ok_or_else(|| first_error.unwrap_or_else(|| {
            io_error(format!(
                "no download tool available (tried: {})",
                missing.join(", ")
            ))
        }))
    }

    /// Run one subprocess fetch without redirect following. Every complete,
    /// bounded HTTP response is returned intact to the fallback dispatcher.
    fn run_fetch_tool(
        &self,
        tool: &str,
        request: &HttpRequest,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        let url = request.url.as_str();
        let mut cmd = Command::new(tool);
        match tool {
            "curl" => {
                const STATUS_MARKER: &str = "\n__NAROU_HTTP_STATUS__";
                // `-D -` writes the response head before the body, so
                // `Set-Cookie` / `Location` survive the last-resort tier too.
                cmd.arg("-sS")
                    .arg("-D")
                    .arg("-")
                    .arg("--compressed")
                    .arg("--retry")
                    .arg("0")
                    .arg("--connect-timeout")
                    .arg(CONNECT_TIMEOUT_SECS.to_string())
                    .arg("--max-time")
                    .arg(TOTAL_TIMEOUT_SECS.to_string())
                    .arg("--max-filesize")
                    .arg(MAX_RESPONSE_BYTES.to_string())
                    .arg("--max-redirs")
                    .arg("0")
                    .arg("-A")
                    .arg(user_agent)
                    .arg("-H")
                    .arg(format!("Accept: {DEFAULT_ACCEPT}"))
                    .arg("-H")
                    .arg(format!("Accept-Language: {DEFAULT_ACCEPT_LANGUAGE}"))
                    .arg("-H")
                    .arg(format!("Connection: {DEFAULT_CONNECTION}"))
                    .arg("-w")
                    .arg(format!("{STATUS_MARKER}%{{http_code}}"));
                for (name, value) in &request.headers {
                    cmd.arg("-H").arg(format!("{name}: {value}"));
                }
                cmd.arg(url);
                let output = run_command_with_timeout(cmd, Duration::from_secs(TOTAL_TIMEOUT_SECS))
                    .map_err(NarouError::Io)?;
                if !output.status.success() {
                    return Err(io_error(format!(
                        "curl fetch failed for {url}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                // The head dump precedes the body; both belong to the same
                // response (this tier never follows redirects).
                let (head, rest) = split_tool_output(&output.stdout)
                    .ok_or_else(|| io_error(format!("curl response headers missing for {url}")))?;
                let headers = parse_tool_headers(head);
                let marker = STATUS_MARKER.as_bytes();
                let Some(pos) = rest.windows(marker.len()).rposition(|window| window == marker)
                else {
                    return Err(io_error(format!(
                        "curl status marker missing for {url}"
                    )));
                };
                let body = &rest[..pos];
                let status_text = String::from_utf8_lossy(&rest[pos + marker.len()..]);
                let status: u16 = status_text.trim().parse()
                    .ok().filter(|status| (100..600).contains(status))
                    .ok_or_else(|| io_error(format!("invalid curl status for {url}")))?;
                if body.len() > MAX_RESPONSE_BYTES {
                    return Err(io_error(format!(
                        "response body exceeded {} bytes while fetching {url}",
                        MAX_RESPONSE_BYTES
                    )));
                }
                Ok(HttpResponse {
                    status,
                    headers,
                    body: body.to_vec(),
                })
            }
            _ => {
                // `--no-verbose` (not `--quiet`) keeps error messages on stderr
                // so a failed fetch can be reported with its status, matching
                // the curl tier's `-sS`. `--save-headers` preserves
                // `Set-Cookie` / `Location` here like in the other tiers.
                cmd.arg("--no-verbose")
                    .arg("--save-headers")
                    .arg("--server-response")
                    .arg("--content-on-error")
                    .arg("--compression=auto")
                    .arg("--output-document=-")
                    .arg("--tries=1")
                    .arg(format!("--connect-timeout={CONNECT_TIMEOUT_SECS}"))
                    .arg(format!("--read-timeout={READ_TIMEOUT_SECS}"))
                    .arg(format!("--timeout={TOTAL_TIMEOUT_SECS}"))
                    .arg("--max-redirect=0")
                    .arg(format!("--user-agent={user_agent}"))
                    .arg(format!("--header=Accept: {DEFAULT_ACCEPT}"))
                    .arg(format!("--header=Accept-Language: {DEFAULT_ACCEPT_LANGUAGE}"))
                    .arg(format!("--header=Connection: {DEFAULT_CONNECTION}"));
                for (name, value) in &request.headers {
                    cmd.arg(format!("--header={name}: {value}"));
                }
                cmd.arg("--").arg(url);
                let output = run_command_with_timeout(cmd, Duration::from_secs(TOTAL_TIMEOUT_SECS))
                    .map_err(NarouError::Io)?;
                let dumped = split_tool_output(&output.stdout);
                let (status, headers, body) = if let Some((head, body)) = dumped {
                    let status = parse_tool_status(head)
                        .ok_or_else(|| io_error(format!("invalid wget status for {url}")))?;
                    (status, parse_tool_headers(head), body)
                } else {
                    let (status, headers) = parse_wget_response_head(&output.stderr)
                        .ok_or_else(|| io_error(format!(
                            "wget fetch failed for {url}: {}", String::from_utf8_lossy(&output.stderr),
                        )))?;
                    (status, headers, output.stdout.as_slice())
                };
                // Exit 8 means an HTTP error, not a truncated transfer.
                // A read/connection failure must never masquerade as a response.
                let http_error_exit = output.status.code() == Some(8) && (300..600).contains(&status);
                if !output.status.success() && !http_error_exit
                {
                    return Err(io_error(format!(
                        "wget fetch failed (HTTP {status}) for {url}: {}",
                        String::from_utf8_lossy(&output.stderr),
                    )));
                }
                if body.len() > MAX_RESPONSE_BYTES {
                    return Err(io_error(format!(
                        "response body exceeded {} bytes while fetching {url}",
                        MAX_RESPONSE_BYTES
                    )));
                }
                Ok(HttpResponse {
                    status,
                    headers,
                    body: body.to_vec(),
                })
            }
        }
    }


    fn configured_curl_handle(
        &self,
        request: &HttpRequest,
        user_agent: &str,
    ) -> Result<curl::easy::Easy> {
        let mut handle = curl::easy::Easy::new();
        handle
            .url(&request.url)
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .useragent(user_agent)
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .follow_location(false)
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .max_redirections(0)
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .timeout(Duration::from_secs(TOTAL_TIMEOUT_SECS))
            .map_err(|e| io_error(e.to_string()))?;
        handle.accept_encoding(DEFAULT_ACCEPT_ENCODING).ok();

        let mut headers = curl::easy::List::new();
        headers
            .append(&format!("Accept: {DEFAULT_ACCEPT}"))
            .map_err(|e| io_error(e.to_string()))?;
        headers
            .append(&format!("Accept-Language: {DEFAULT_ACCEPT_LANGUAGE}"))
            .map_err(|e| io_error(e.to_string()))?;
        headers
            .append(&format!("Accept-Charset: {DEFAULT_ACCEPT_CHARSET}"))
            .map_err(|e| io_error(e.to_string()))?;
        headers
            .append(&format!("Connection: {DEFAULT_CONNECTION}"))
            .map_err(|e| io_error(e.to_string()))?;
        // Caller headers (Cookie, Referer, …) win over the defaults above.
        for (name, value) in &request.headers {
            if !is_safe_header_value(value) {
                return Err(io_error("unsafe header value"));
            }
            headers
                .append(&format!("{name}: {value}"))
                .map_err(|e| io_error(e.to_string()))?;
        }
        handle
            .http_headers(headers)
            .map_err(|e| io_error(e.to_string()))?;
        Ok(handle)
    }

}

impl HttpClient for NativeHttpClient {
    /// DNS まで見る検証 (プラットフォーム既定は構文検証のみ)。
    fn validate_url<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            validate_public_url(url).map_err(|error| NarouError::Http(error.to_string()))
        })
    }

    fn send<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(self.send_chain(request, None))
    }

    fn send_paced<'a>(
        &'a self,
        request: HttpRequest,
        pacing: crate::platform::http::HttpPacing<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(self.send_chain(request, Some(pacing)))
    }
}

#[cfg(test)]
mod tier_state_tests {
    use super::*;

    #[test]
    fn winning_tier_is_tried_first() {
        let mut state = TierState::default();
        assert_eq!(state.order("example.com"), vec![0, 1, 2]);
        state.note_success("example.com", TIER_SUBPROCESS);
        assert_eq!(state.order("example.com"), vec![2, 0, 1]);
        assert_eq!(state.order("other.example"), vec![0, 1, 2]);
    }

    #[test]
    fn success_clears_accumulated_failures() {
        let mut state = TierState::default();
        for _ in 0..FAIL_THRESHOLD {
            state.note_failure("example.com", TIER_CURL);
        }
        assert!(!state.should_try("example.com", TIER_CURL));
        state.note_success("example.com", TIER_CURL);
        assert!(state.should_try("example.com", TIER_CURL));
    }

    #[test]
    fn failing_preferred_tier_drops_the_preference() {
        let mut state = TierState::default();
        state.note_success("example.com", TIER_REQWEST);
        state.note_failure("example.com", TIER_REQWEST);
        assert_eq!(state.order("example.com"), vec![0, 1, 2]);
    }

    #[test]
    fn forgetting_a_domain_restores_every_tier() {
        let mut state = TierState::default();
        for tier in 0..TIER_COUNT {
            for _ in 0..FAIL_THRESHOLD {
                state.note_failure("example.com", tier);
            }
        }
        assert!((0..TIER_COUNT).all(|tier| !state.should_try("example.com", tier)));
        state.forget("example.com");
        assert!((0..TIER_COUNT).all(|tier| state.should_try("example.com", tier)));
    }
}

fn build_reqwest_client(user_agent: &str) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(user_agent)
        .default_headers(default_request_headers())
        .http1_only()
        .gzip(true)
        // Brotli 応答の展開は無効にする。CVE-2026-111497 の対象は Google Brotli の
        // C 実装 (1.0.3〜1.2.0) で、このプログラムは C 版をリンクしていないが、
        // 純 Rust 実装も修正前と同じデコーダ経路を持つ。外部サイトの応答を
        // 展開する必要はなく、gzip / deflate / identity で取得できれば十分。
        .deflate(true)
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(TOTAL_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| NarouError::Http(e.to_string()))
}

/// A fresh cursor for each retained-body hop; the payload is never copied.
struct SharedRequestBody {
    body: Arc<Vec<u8>>,
    position: usize,
}

impl Read for SharedRequestBody {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = output.len().min(self.body.len() - self.position);
        output[..count].copy_from_slice(&self.body[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

fn read_response_bytes(mut response: reqwest::blocking::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|size| size > limit as u64) {
        return Err(io_error(format!("response body exceeded {limit} bytes")));
    }
    let mut body = Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    let mut chunk = [0u8; 8192];
    loop {
        let read = response.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        if read > limit.saturating_sub(body.len()) {
            return Err(io_error(format!("response body exceeded {limit} bytes")));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

fn run_command_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
    configure_hidden_console_command(&mut cmd);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr pipe missing"))?;

    let exceeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let stdout_exceeded = Arc::clone(&exceeded);
    thread::spawn(move || {
        let result = read_tool_pipe(
            stdout, MAX_RESPONSE_BYTES + MAX_HTTP1_HEAD + 1024, &stdout_exceeded,
        );
        let _ = stdout_tx.send(result);
    });

    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stderr_exceeded = Arc::clone(&exceeded);
    thread::spawn(move || {
        let result = read_tool_pipe(stderr, MAX_HTTP1_HEAD, &stderr_exceeded);
        let _ = stderr_tx.send(result);
    });

    let started_at = Instant::now();
    let status = loop {
        if exceeded.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("subprocess response exceeds limit"));
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("subprocess timed out after {} seconds", timeout.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    };

    Ok(Output {
        status,
        stdout: stdout_rx.recv().map_err(std::io::Error::other)??,
        stderr: stderr_rx.recv().map_err(std::io::Error::other)??,
    })
}

fn read_tool_pipe(
    mut reader: impl Read,
    limit: usize,
    exceeded: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(bytes);
        }
        if count > limit.saturating_sub(bytes.len()) {
            exceeded.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(std::io::Error::other("subprocess response exceeds limit"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn io_error(message: impl Into<String>) -> NarouError {
    std::io::Error::other(message.into()).into()
}

/// Response head collector for libcurl's `header_function`.
///
/// libcurl reports every line it reads, status lines included, so a fresh
/// status line starts a new head: a 100-continue (or a proxy CONNECT reply)
/// must not leak its fields into the final response's headers.
#[derive(Default)]
struct CurlHeaders {
    headers: Vec<(String, String)>,
}

impl CurlHeaders {
    fn feed(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return;
        }
        if line.starts_with("HTTP/") {
            self.headers.clear();
            return;
        }
        match line.split_once(':') {
            Some((name, value)) => {
                let name = name.trim();
                if !name.is_empty() {
                    self.headers
                        .push((name.to_string(), value.trim().to_string()));
                }
            }
            // Obsolete folded line: append to the previous field's value.
            None => {
                if let Some((_, value)) = self.headers.last_mut() {
                    value.push(' ');
                    value.push_str(line.trim());
                }
            }
        }
    }

    fn into_headers(self) -> Vec<(String, String)> {
        self.headers
    }
}

/// Every response header of a reqwest response, as received.
fn response_headers(response: &reqwest::blocking::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Split a tool's header dump (`curl -D -` / `wget --save-headers`) into the
/// final response head and the body.
///
/// Both tools write every head they read before the body; a new `HTTP/`
/// status line starts another block and only the last block belongs to the
/// response that produced the body. `None` means no syntactically valid head
/// was found, which must fail the tier rather than leak a head into the body.
fn split_tool_output(output: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut start = 0usize;
    loop {
        let rest = &output[start..];
        let (head_end, skip) = find_head_terminator(rest)?;
        let block = &rest[..head_end];
        let first_line_end = block
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(block.len());
        if !block[..first_line_end].starts_with(b"HTTP/") {
            return None;
        }
        let next = start + head_end + skip;
        if output[next..].starts_with(b"HTTP/") {
            start = next;
            continue;
        }
        return Some((block, &output[next..]));
    }
}

/// End of a header block and the length of its terminating blank line.
fn find_head_terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    if let Some(at) = find_bytes(bytes, b"\r\n\r\n") {
        return Some((at, 4));
    }
    find_bytes(bytes, b"\n\n").map(|at| (at, 2))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Header fields of a dumped response head (the status line starts the head;
/// the collector drops it and keeps the fields).
fn parse_tool_headers(head: &[u8]) -> Vec<(String, String)> {
    let mut collector = CurlHeaders::default();
    for line in head.split(|&byte| byte == b'\n') {
        collector.feed(line);
    }
    collector.into_headers()
}

fn parse_tool_status(head: &[u8]) -> Option<u16> {
    let first_line = head.split(|&byte| byte == b'\n').next()?;
    let text = std::str::from_utf8(first_line).ok()?;
    let mut parts = text.lines().next()?.split_whitespace();
    parts.next()?.starts_with("HTTP/").then_some(())?;
    let status = parts.next()?.parse::<u16>().ok()?;
    (100..600).contains(&status).then_some(status)
}

/// wget reports redirects/error heads on stderr even when it saves no body.
fn parse_wget_response_head(stderr: &[u8]) -> Option<(u16, Vec<(String, String)>)> {
    let text = String::from_utf8_lossy(stderr);
    let mut status = None;
    let mut headers = CurlHeaders::default();
    let mut in_head = false;
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) {
            in_head = false;
            continue;
        }
        let line = line.trim();
        if line.starts_with("HTTP/") {
            status = parse_tool_status(line.as_bytes());
            headers.feed(line.as_bytes());
            in_head = true;
        } else if in_head && line.contains(':') {
            headers.feed(line.as_bytes());
        }
    }
    status.map(|status| (status, headers.into_headers()))
}

pub fn default_request_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static(DEFAULT_ACCEPT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(DEFAULT_ACCEPT_LANGUAGE),
    );
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static(DEFAULT_ACCEPT_ENCODING),
    );
    headers.insert(ACCEPT_CHARSET, HeaderValue::from_static(DEFAULT_ACCEPT_CHARSET));
    headers.insert(CONNECTION, HeaderValue::from_static(DEFAULT_CONNECTION));
    headers
}

#[cfg(test)]
mod tests {
    use super::{NativeHttpClient, parse_tool_headers, split_tool_output};
    use crate::error::NarouError;
    use crate::platform::{CookieStore, HttpClient, HttpRequest, RedirectMode};
    use crate::platform::http::RedirectChain;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn curl_404_is_not_found() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/missing", listener.local_addr().unwrap());

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let response = client.fetch_tier_curl(&HttpRequest::get(url.clone()), "narou_rs-test").unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 404);
    }

    #[test]
    fn curl_redirect_returns_the_single_hop_status_and_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/redirect", listener.local_addr().unwrap());

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/private\r\nSet-Cookie: sid=next\r\nSet-Cookie: extra=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let response = client
            .fetch_tier_curl(
                &HttpRequest::get(url.clone()).with_header("Cookie", "over18=yes"),
                "narou_rs-test",
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 302);
        assert_eq!(response.header("Location"), Some("http://127.0.0.1/private"));
        assert_eq!(response.header_values("Set-Cookie"), vec!["sid=next", "extra=1"]);
    }


    #[test]
    fn curl_503_is_returned_raw_for_policy_mapping() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/limited", listener.local_addr().unwrap());

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let response = client.fetch_tier_curl(&HttpRequest::get(url.clone()), "narou_rs-test").unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 503);
        assert!(matches!(
            crate::downloader::http_policy::ensure_success_response(&url, response),
            Err(NarouError::SuspendDownload(_))
        ));
    }

    /// Serves `response` once on an ephemeral port and returns its URL.
    fn serve_once(response: &'static [u8]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // リクエスト全体を読み切ってから応答する。未読の受信データを残した
            // まま close すると RST になり、クライアントが応答本文を取り落とす
            // ことがある (CI で稀に body error になる)。
            let _ = read_request(&mut stream);
            stream.write_all(response).unwrap();
            let _ = stream.shutdown(std::net::Shutdown::Write);
        });
        (url, server)
    }

    fn serve_responses(responses: Vec<&'static [u8]>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            for response in responses {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "expected another fallback attempt");
                            thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                read_request(&mut stream);
                stream.write_all(response).unwrap();
            }
        });
        (url, server)
    }

    fn native_tiers_only(url: &str) -> NativeHttpClient {
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let mut failures = [0; super::TIER_COUNT];
        failures[super::TIER_SUBPROCESS] = super::FAIL_THRESHOLD;
        client.tiers.lock().failures.insert(
            crate::downloader::http_policy::domain_of(url).to_string(), failures,
        );
        client
    }

    #[tokio::test]
    async fn native_untrusted_get_retains_last_http_response_after_fallback() {
        for redirect in [RedirectMode::Follow, RedirectMode::Manual] {
            let (url, server) = serve_responses(vec![
                b"HTTP/1.1 410 Gone\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfirst",
                b"HTTP/1.1 410 Gone\r\nSet-Cookie: sid=last\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlast",
            ]);
            let client = native_tiers_only(&url);
            let request = HttpRequest::get(url.clone()).with_redirect(redirect);
            assert!(!request.trusted_endpoint);
            // Enter at the post-validation dispatcher, as the loopback fixture
            // is deliberately not an admissible public site URL.
            let result = client.send_validated_hop(RedirectChain::new(request, 10), None).await;
            server.join().unwrap();
            let (_, response) = result.unwrap();
            assert_eq!(response.status, 410);
            assert_eq!(response.body, b"last");
            assert_eq!(response.header("Set-Cookie"), Some("sid=last"));
            let tiers = client.tiers.lock();
            let domain = crate::downloader::http_policy::domain_of(&url);
            assert_eq!(tiers.failures[domain][super::TIER_CURL], 1);
            assert_eq!(tiers.failures[domain][super::TIER_REQWEST], 1);
            assert!(!tiers.preferred.contains_key(domain));
        }
    }

    #[tokio::test]
    async fn native_untrusted_get_prefers_fallback_success() {
        for redirect in [RedirectMode::Follow, RedirectMode::Manual] {
            let (url, server) = serve_responses(vec![
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngate",
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ]);
            let client = native_tiers_only(&url);
            let domain = crate::downloader::http_policy::domain_of(&url);
            if redirect == RedirectMode::Manual {
                // Manual URL resolution must still try curl before a preferred
                // reqwest tier; the health counters identify the attempted order.
                client.tiers.lock().note_success(domain, super::TIER_REQWEST);
            }
            let request = HttpRequest::get(url.clone()).with_redirect(redirect);
            let result = client.send_validated_hop(RedirectChain::new(request, 10), None).await;
            server.join().unwrap();
            let (_, response) = result.unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"ok");
            let tiers = client.tiers.lock();
            assert_eq!(tiers.preferred.get(domain), Some(&super::TIER_REQWEST));
            assert_eq!(tiers.failures[domain][super::TIER_CURL], 1);
            assert_eq!(tiers.failures[domain][super::TIER_REQWEST], 0);
        }
    }

    #[tokio::test]
    async fn native_untrusted_get_keeps_http_response_when_later_tier_is_truncated() {
        let (url, server) = serve_responses(vec![
            b"HTTP/1.1 410 Gone\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone",
            b"HTTP/1.1 410 Gone\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort",
        ]);
        let client = native_tiers_only(&url);
        let result = client.send_validated_hop(
            RedirectChain::new(HttpRequest::get(url.clone()), 10), None,
        ).await;
        server.join().unwrap();
        let (_, response) = result.unwrap();
        assert_eq!(response.status, 410);
        assert_eq!(response.body, b"gone", "a truncated later response must not replace the complete one");
        let tiers = client.tiers.lock();
        let domain = crate::downloader::http_policy::domain_of(&url);
        assert_eq!(tiers.failures[domain][super::TIER_CURL], 1);
        assert_eq!(tiers.failures[domain][super::TIER_REQWEST], 1);
    }

    #[tokio::test]
    async fn native_untrusted_get_definitive_status_stops_fallback() {
        for status in [404, 503] {
            let response = if status == 404 {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            };
            let (url, server) = serve_once(response);
            let client = native_tiers_only(&url);
            let result = client.send_validated_hop(
                RedirectChain::new(HttpRequest::get(url.clone()), 10), None,
            ).await;
            server.join().unwrap();
            assert_eq!(result.unwrap().1.status, status);
            let tiers = client.tiers.lock();
            let domain = crate::downloader::http_policy::domain_of(&url);
            assert_eq!(tiers.preferred.get(domain), Some(&super::TIER_CURL));
            assert_eq!(tiers.failures[domain][super::TIER_CURL], 0);
            assert_eq!(tiers.failures[domain][super::TIER_REQWEST], 0);
        }
    }

    #[tokio::test]
    async fn native_untrusted_get_still_rejects_loopback_before_dispatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        client.send(HttpRequest::get(url)).await.expect_err("loopback must remain untrusted");
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn native_untrusted_get_rejects_oversized_http_error_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            let mut remaining = super::MAX_RESPONSE_BYTES + 1;
            stream.write_all(format!(
                "HTTP/1.1 410 Gone\r\nContent-Length: {remaining}\r\nConnection: close\r\n\r\n"
            ).as_bytes()).unwrap();
            let chunk = [b'x'; 8192];
            while remaining > 0 {
                let count = remaining.min(chunk.len());
                if stream.write_all(&chunk[..count]).is_err() { break; }
                remaining -= count;
            }
        });
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let result = client.send_reqwest_hop(&HttpRequest::get(url));
        server.join().unwrap();
        assert!(matches!(result, Err(NarouError::Io(_))));
    }

    #[test]
    fn subprocess_tools_and_tier_keep_nondefinitive_http_status() {
        let mut available = Vec::new();
        for tool in ["curl", "wget"] {
            match std::process::Command::new(tool).arg("--version").output() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("could not probe {tool}: {error}"),
                Ok(output) => assert!(output.status.success(), "{tool} --version failed"),
            }
            available.push(tool);
            let (url, server) = serve_responses(vec![
                b"HTTP/1.1 410 Gone\r\nSet-Cookie: sid=gone\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone",
            ]);
            let client = NativeHttpClient::new("narou_rs-test").unwrap();
            let result = client.run_fetch_tool(tool, &HttpRequest::get(url), "narou_rs-test");
            server.join().unwrap();
            let response = result.unwrap();
            assert_eq!(response.status, 410, "{tool}");
            assert_eq!(response.body, b"gone", "{tool}");
            assert_eq!(response.header("Set-Cookie"), Some("sid=gone"), "{tool}");
        }
        if available.is_empty() {
            return;
        }
        let (url, server) = serve_responses(vec![
            b"HTTP/1.1 410 Gone\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone".as_slice();
            available.len()
        ]);
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let result = client.fetch_tier_subprocess(&HttpRequest::get(url), "narou_rs-test");
        server.join().unwrap();
        let response = result.unwrap();
        assert_eq!(response.status, 410);
        assert_eq!(response.body, b"gone");
    }

    #[test]
    fn native_untrusted_get_tiers_keep_nondefinitive_http_status() {
        for tier in [super::TIER_CURL, super::TIER_REQWEST] {
            for status in [403, 410] {
                let response = if status == 403 {
                    b"HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull".as_slice()
                } else {
                    b"HTTP/1.1 410 Gone\r\nContent-Type: application/json\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull".as_slice()
                };
                let (url, server) = serve_once(response);
                let request = HttpRequest::get(url.clone());
                assert!(!request.trusted_endpoint);
                let client = NativeHttpClient::new("narou_rs-test").unwrap();
                let result = match tier {
                    super::TIER_CURL => client.fetch_tier_curl(&request, "narou_rs-test"),
                    super::TIER_REQWEST => client.send_reqwest_hop(&request),
                    _ => unreachable!(),
                };
                server.join().unwrap();
                let response = result.expect("a complete HTTP error must retain its numeric status");
                assert_eq!(response.status, status);
                assert_eq!(response.body, b"null");
                assert!(matches!(
                    crate::downloader::http_policy::ensure_success_response(&url, response),
                    Err(NarouError::Http(message)) if message.contains(&format!("HTTP {status}"))
                ));
            }
        }
    }

    #[test]
    fn subprocess_tier_fetches_200_body() {
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        match client.fetch_tier_subprocess(&HttpRequest::get(url.clone()), "narou_rs-test") {
            Ok(response) => {
                assert_eq!(response.status, 200);
                assert_eq!(response.body, b"hello");
                // The dumped head must not leak into the body and every
                // response header (both cookies included) must survive.
                assert_eq!(response.header_values("set-cookie"), vec!["a=1", "b=2"]);
            }
            // No curl/wget on this machine: nothing to verify.
            Err(err) if err.to_string().contains("no download tool available") => {}
            Err(err) => panic!("subprocess fetch failed: {err}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn curl_tier_keeps_repeated_response_headers() {
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nSet-Cookie: sid=1; Path=/\r\nSet-Cookie: over18=yes\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let response = client
            .fetch_tier_curl(&HttpRequest::get(url.clone()), "narou_rs-test")
            .unwrap();
        server.join().unwrap();

        assert_eq!(
            response.header_values("set-cookie"),
            vec!["sid=1; Path=/", "over18=yes"]
        );
        assert_eq!(
            response.header("content-type"),
            Some("text/html; charset=UTF-8")
        );
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn tool_output_split_keeps_only_the_final_head() {
        let output = b"HTTP/1.1 302 Found\r\nLocation: /a\r\n\r\nHTTP/1.1 100 Continue\r\nX-Interim: 1\r\n\r\nHTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 2\r\n\r\nok";
        let (head, body) = split_tool_output(output).expect("a head block");
        let headers = parse_tool_headers(head);
        assert_eq!(body, b"ok");
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["a=1", "b=2"]
        );
        assert!(!headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("location")));
        assert!(!headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("x-interim")));
    }

    #[tokio::test]
    async fn response_set_cookie_updates_only_the_login_that_was_sent() {
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: sid=fresh; Path=/\r\nSet-Cookie: extra=1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let store = Arc::new(crate::platform::mocks::MemoryCookieStore::new());
        store
            .save_groups(
                "example.com",
                &[
                    crate::platform::LoginGroup::new(
                        "example.com",
                        vec![crate::platform::HostCookie {
                            host: "127.0.0.1".to_string(),
                            cookie: "sid=stale".to_string(),
                        }],
                    ),
                    crate::platform::LoginGroup::new(
                        "example.com",
                        vec![crate::platform::HostCookie {
                            host: "other.example".to_string(),
                            cookie: "sid=other".to_string(),
                        }],
                    ),
                ],
            )
            .await
            .unwrap();

        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let request = HttpRequest::get(url.clone()).with_header("Cookie", "sid=stale");
        let response = client
            .fetch_tier_curl(&request, "narou_rs-test")
            .unwrap();
        server.join().unwrap();
        crate::platform::cookie_store::persist_response_set_cookie(
            store.as_ref(),
            &response,
            Some("sid=stale"),
        )
        .await;

        let stored = store.load_groups("example.com").await.unwrap();
        assert_eq!(stored[0].cookies[0].cookie, "sid=fresh; extra=1");
        assert_eq!(
            stored[1].cookies[0].cookie, "sid=other",
            "a login that was not sent must not be touched"
        );
    }

    #[tokio::test]
    async fn a_complete_response_refreshes_the_sent_login_cookie() {
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=wired\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let store = Arc::new(crate::platform::mocks::MemoryCookieStore::new());
        store
            .save_groups(
                "example.com",
                &[crate::platform::LoginGroup::new(
                    "example.com",
                    vec![crate::platform::HostCookie {
                        host: "127.0.0.1".to_string(),
                        cookie: "sid=stale".to_string(),
                    }],
                )],
            )
            .await
            .unwrap();

        let client = NativeHttpClient::new("narou_rs-test")
            .unwrap()
            .with_cookie_store(store.clone());
        // The test server is loopback-only, so the request rides the trusted
        // endpoint path (no public-address check); the `Set-Cookie` write-back
        // is the same call the site paths make.
        let request = HttpRequest::get(url.clone())
            .with_header("Cookie", "sid=stale")
            .with_trusted_endpoint();
        client.send(request).await.unwrap();
        server.join().unwrap();

        let stored = store.load_groups("example.com").await.unwrap();
        assert_eq!(stored[0].cookies[0].cookie, "sid=wired");
    }

    #[test]
    fn reqwest_hop_to_a_private_host_is_rejected_before_it_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/start");
        let location = format!("http://localhost:{port}/next");
        let second_request = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = second_request.clone();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            drop(stream);

            // The next hop must be rejected by validation before any second
            // connection is attempted.
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => {
                        seen.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        // Seed the first single hop directly: the shared outer send correctly
        // rejects this loopback test origin as untrusted. The returned redirect
        // must still be checked before its next outgoing request.
        let request = HttpRequest::get(url);
        let response = client.send_reqwest_hop(&request).unwrap();
        let mut chain = RedirectChain::new(request, 10);
        assert!(chain.advance(&response).unwrap());
        let err = NativeHttpClient::validate_hop(chain.request()).unwrap_err();
        server.join().unwrap();

        assert!(
            !second_request.load(std::sync::atomic::Ordering::SeqCst),
            "a hop to a private address must be rejected, not sent"
        );
        assert!(
            err.to_string().contains("non-public") || err.to_string().contains("DNS"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn subprocess_tier_returns_a_definitive_404_unchanged() {
        let (url, server) = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        match client.fetch_tier_subprocess(&HttpRequest::get(url.clone()), "narou_rs-test") {
            Err(err) if err.to_string().contains("no download tool available") => {}
            Err(err) => panic!("subprocess fetch failed: {err}"),
            Ok(response) => assert_eq!(response.status, 404),
        }
        server.join().unwrap();
    }

    #[tokio::test]
    async fn constructor_is_safe_inside_async_runtime() {
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        drop(client);
    }

    fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "request ended before its head");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(at) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
            assert!(bytes.len() <= super::MAX_HTTP1_HEAD);
        };
        let head = String::from_utf8(bytes[..head_end].to_vec()).unwrap();
        let length = request_header(&head, "Content-Length")
            .map(|value| value.parse::<usize>().unwrap()).unwrap_or(0);
        while bytes.len() - head_end < length {
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "request body was truncated");
            bytes.extend_from_slice(&chunk[..count]);
        }
        (head, bytes[head_end..head_end + length].to_vec())
    }

    fn request_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    }

    #[tokio::test]
    async fn native_redirects_send_the_correct_method_body_and_body_headers() {
        for status in [301, 302, 303, 307, 308] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/start", listener.local_addr().unwrap());
            let payload: Vec<u8> = (0u8..=255).cycle().take(32 * 1024).collect();
            let expected = payload.clone();
            let server = thread::spawn(move || {
                let (mut first, _) = listener.accept().unwrap();
                let (head, body) = read_request(&mut first);
                assert!(head.starts_with("POST /start HTTP/1.1"));
                assert_eq!(body, expected);
                first.write_all(format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: /finish\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ).as_bytes()).unwrap();
                drop(first);
                let (mut second, _) = listener.accept().unwrap();
                let (head, body) = read_request(&mut second);
                if matches!(status, 301..=303) {
                    assert!(head.starts_with("GET /finish HTTP/1.1"), "{head}");
                    assert!(body.is_empty());
                    assert!(request_header(&head, "Content-Type").is_none());
                    assert!(request_header(&head, "Content-Encoding").is_none());
                    assert!(request_header(&head, "Content-Length").is_none());
                } else {
                    assert!(head.starts_with("POST /finish HTTP/1.1"), "{head}");
                    assert_eq!(body, expected);
                    assert_eq!(request_header(&head, "Content-Type"), Some("application/octet-stream"));
                }
                assert_eq!(request_header(&head, "User-Agent"), Some("site-agent"));
                assert_eq!(head.lines().filter_map(|line| line.split_once(':'))
                    .filter(|(name, _)| name.eq_ignore_ascii_case("User-Agent")).count(), 1);
                second.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            });
            let client = NativeHttpClient::new("native-default").unwrap();
            let response = client.send(HttpRequest::post(url, payload)
                .with_header("Content-Type", "application/octet-stream")
                .with_header("Content-Encoding", "identity")
                .with_header("User-Agent", "site-agent")
                .with_trusted_endpoint()).await.unwrap();
            server.join().unwrap();
            assert_eq!(response.body, b"ok");
        }
    }

    #[tokio::test]
    async fn manual_native_send_returns_the_single_redirect_without_another_connection() {
        let (url, server) = serve_once(
            b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/next\r\nSet-Cookie: sid=next\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwait",
        );
        let client = NativeHttpClient::new("native-test").unwrap();
        let response = client.send(
            HttpRequest::post(url, b"payload".to_vec())
                .with_trusted_endpoint().with_redirect(RedirectMode::Manual),
        ).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.status, 307);
        assert_eq!(response.body, b"wait");
        assert_eq!(response.header("Location"), Some("http://127.0.0.1:1/next"));
        assert_eq!(response.header_values("Set-Cookie"), vec!["sid=next"]);
    }

    #[tokio::test]
    async fn native_origin_port_change_strips_authorization_but_keeps_the_actual_cookie() {
        let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/start", first_listener.local_addr().unwrap());
        let destination = format!("http://{}/finish", second_listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = first_listener.accept().unwrap();
            let (head, _) = read_request(&mut first);
            assert_eq!(request_header(&head, "Authorization"), Some("Bearer secret"));
            first.write_all(format!(
                "HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ).as_bytes()).unwrap();
            drop(first);
            let (mut second, _) = second_listener.accept().unwrap();
            let (head, _) = read_request(&mut second);
            assert!(request_header(&head, "Authorization").is_none());
            assert_eq!(request_header(&head, "Cookie"), Some("sid=stale"));
            second.write_all(b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=fresh\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
        });
        let store = Arc::new(crate::platform::mocks::MemoryCookieStore::new());
        store.save_groups("example.com", &[crate::platform::LoginGroup::new(
            "example.com", vec![crate::platform::HostCookie {
                host: "127.0.0.1".into(), cookie: "sid=stale".into(),
            }],
        )]).await.unwrap();
        let client = NativeHttpClient::new("native-test").unwrap().with_cookie_store(store.clone());
        client.send(HttpRequest::get(url)
            .with_header("Authorization", "Bearer secret")
            .with_header("Cookie", "sid=stale")
            .with_trusted_endpoint()).await.unwrap();
        server.join().unwrap();
        assert_eq!(store.load_groups("example.com").await.unwrap()[0].cookies[0].cookie, "sid=fresh");
    }

    #[tokio::test]
    async fn intermediate_redirect_cookie_is_saved_and_cross_site_cookie_is_not_written_back() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/start");
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let (head, _) = read_request(&mut first);
            assert_eq!(request_header(&head, "Cookie"), Some("sid=stale"));
            first.write_all(format!(
                "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/finish\r\nSet-Cookie: sid=intermediate\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ).as_bytes()).unwrap();
            drop(first);
            let (mut second, _) = listener.accept().unwrap();
            let (head, _) = read_request(&mut second);
            assert!(request_header(&head, "Cookie").is_none());
            assert!(request_header(&head, "Authorization").is_none());
            second.write_all(b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=foreign\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
        });
        let store = Arc::new(crate::platform::mocks::MemoryCookieStore::new());
        store.save_groups("example.com", &[crate::platform::LoginGroup::new(
            "example.com", vec![crate::platform::HostCookie {
                host: "127.0.0.1".into(), cookie: "sid=stale".into(),
            }],
        )]).await.unwrap();
        let client = NativeHttpClient::new("native-test").unwrap().with_cookie_store(store.clone());
        client.send(HttpRequest::get(url)
            .with_header("Cookie", "sid=stale")
            .with_header("Authorization", "Bearer secret")
            .with_trusted_endpoint()).await.unwrap();
        server.join().unwrap();
        assert_eq!(store.load_groups("example.com").await.unwrap()[0].cookies[0].cookie, "sid=intermediate");
    }

    #[tokio::test]
    async fn trusted_response_truncation_is_an_error_not_a_fallback_response() {
        let (url, server) = serve_once(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort",
        );
        let client = NativeHttpClient::new("native-test").unwrap();
        assert!(client.send(HttpRequest::get(url).with_trusted_endpoint()).await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn same_site_redirect_rotation_is_sent_and_final_rotation_is_persisted() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/start", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let (head, _) = read_request(&mut first);
            assert_eq!(request_header(&head, "Cookie"), Some("sid=stale"));
            first.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /finish\r\nSet-Cookie: sid=intermediate\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ).unwrap();
            drop(first);
            let (mut second, _) = listener.accept().unwrap();
            let (head, _) = read_request(&mut second);
            assert_eq!(request_header(&head, "Cookie"), Some("sid=intermediate"));
            second.write_all(
                b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=final\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ).unwrap();
        });
        let store = Arc::new(crate::platform::mocks::MemoryCookieStore::new());
        store.save_groups("example.com", &[crate::platform::LoginGroup::new(
            "example.com", vec![crate::platform::HostCookie {
                host: "127.0.0.1".into(), cookie: "sid=stale".into(),
            }],
        )]).await.unwrap();
        let client = NativeHttpClient::new("native-test").unwrap().with_cookie_store(store.clone());
        let response = client.send(HttpRequest::get(url)
            .with_header("Cookie", "sid=stale").with_trusted_endpoint()).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.body, b"ok");
        assert_eq!(store.load_groups("example.com").await.unwrap()[0].cookies[0].cookie, "sid=final");
    }
}
