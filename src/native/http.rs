//! Native HTTP transport: curl → reqwest → subprocess (curl/wget) tier fallback.
//!
//! This is the native implementation of [`HttpClient`]. It owns everything
//! transport-specific: the two reqwest clients (follow / manual-redirect),
//! the libcurl handle configuration, the subprocess fallback (curl.exe on
//! Windows, wget elsewhere), the per-domain tier-failure bookkeeping, and the
//! CDN curl-probe enhancement used by `resolve_final_url`.
//!
//! It never decodes response bytes and never decides what a status code
//! means — that is `downloader::http_policy`'s job. 404/503 responses are
//! returned raw so the policy layer maps them to `NotFound` /
//! `SuspendDownload`; other 4xx/5xx are transport errors that let the tier
//! fallback try the next transport.
//!
//! Rate limiting is *not* part of this transport; the downloader injects a
//! separate [`RateLimiter`] and paces requests through
//! `downloader::http_policy` helpers.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use reqwest::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONNECTION, CONTENT_TYPE, HeaderMap,
    HeaderValue, LOCATION, USER_AGENT,
};

use crate::compat::configure_hidden_console_command;
use crate::downloader::http_policy::same_site_hosts;
use crate::downloader::security::{
    CONNECT_TIMEOUT_SECS, MAX_REDIRECTS, MAX_RESPONSE_BYTES, READ_TIMEOUT_SECS, TOTAL_TIMEOUT_SECS,
    is_safe_header_value, validate_public_url,
};
use crate::error::{NarouError, Result};
use crate::platform::{HttpClient, HttpMethod, HttpRequest, HttpResponse, RedirectMode};

const FAIL_THRESHOLD: u8 = 5;

/// Native HTTP transport with the curl → reqwest → subprocess fallback.
///
/// `Clone` is cheap: the two reqwest clients are `Arc`-backed and the
/// tier-failure / prefer-curl state is shared through `Arc`, so clones
/// observe the same fallback bookkeeping (matching the old `HttpFetcher`
/// semantics where one fetcher served a whole download run).
#[derive(Clone)]
pub struct NativeHttpClient {
    client: reqwest::blocking::Client,
    manual_redirect_client: reqwest::blocking::Client,
    user_agent: String,
    tier_failures: Arc<Mutex<HashMap<String, [u8; 3]>>>,
    prefer_curl: Arc<AtomicBool>,
}

impl NativeHttpClient {
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
        let client = build_reqwest_client(user_agent, true)?;
        let manual_redirect_client = build_reqwest_client(user_agent, false)?;

        Ok(Self {
            client,
            manual_redirect_client,
            user_agent: user_agent.to_string(),
            tier_failures: Arc::new(Mutex::new(HashMap::new())),
            prefer_curl: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Synchronous entry point used by `send` inside `spawn_blocking`.
    fn send_blocking(&self, request: &HttpRequest) -> Result<HttpResponse> {
        match request.method {
            HttpMethod::Get => match request.redirect {
                RedirectMode::Follow => self.send_follow(request),
                RedirectMode::Manual => self.send_manual(request),
            },
            HttpMethod::Post => self.send_post(request),
        }
    }

    /// Follow-mode GET: tiered curl → reqwest → subprocess fallback.
    ///
    /// A tier that receives a definitive server answer (404/503) returns the
    /// raw response immediately; the policy layer maps it. Other 4xx/5xx and
    /// transport failures fall through to the next tier, mirroring the old
    /// `HttpFetcher::fetch_text` behaviour.
    fn send_follow(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let url = &request.url;
        validate_public_url(url).map_err(io_error)?;
        let cookie = request.header("Cookie");
        let user_agent = request.header("User-Agent").unwrap_or(&self.user_agent);
        let domain = crate::downloader::http_policy::domain_of(url).to_string();
        let mut last_error = None;

        if self.prefer_curl.load(Ordering::Relaxed) {
            match self.fetch_tier_curl(url, cookie, user_agent) {
                Ok(response) => return Ok(response),
                Err(err) => last_error = Some(err),
            }
        }

        let tier_failures = self.tier_failures.lock();
        let skip_curl = tier_failures
            .get(&domain)
            .is_some_and(|f| f[0] >= FAIL_THRESHOLD);
        let skip_reqwest = tier_failures
            .get(&domain)
            .is_some_and(|f| f[1] >= FAIL_THRESHOLD);
        let skip_wget = tier_failures
            .get(&domain)
            .is_some_and(|f| f[2] >= FAIL_THRESHOLD);
        drop(tier_failures);

        if !skip_curl && !self.prefer_curl.load(Ordering::Relaxed) {
            match self.fetch_tier_curl(url, cookie, user_agent) {
                Ok(response) => {
                    self.prefer_curl.store(true, Ordering::Relaxed);
                    return Ok(response);
                }
                Err(err) => {
                    last_error = Some(err);
                    self.tier_failures
                        .lock()
                        .entry(domain.clone())
                        .or_insert([0; 3])[0] += 1;
                }
            }
        }

        if !skip_reqwest {
            match self.fetch_tier_reqwest(url, cookie, user_agent) {
                Ok(response) => return Ok(response),
                Err(err) => {
                    last_error = Some(err);
                    self.tier_failures
                        .lock()
                        .entry(domain.clone())
                        .or_insert([0; 3])[1] += 1;
                }
            }
        }

        if !skip_wget {
            match self.fetch_tier_subprocess(url, cookie, user_agent) {
                Ok(response) => return Ok(response),
                Err(err) => {
                    last_error = Some(err);
                    self.tier_failures
                        .lock()
                        .entry(domain.clone())
                        .or_insert([0; 3])[2] += 1;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| NarouError::NotFound(url.to_string())))
    }

    /// Manual-mode GET: no redirect following; 3xx responses are returned
    /// as-is so `resolve_final_url` can follow the chain. On 4xx/5xx the
    /// transport probes the same hop via libcurl: a CDN (e.g. Cloudflare)
    /// may challenge reqwest while letting curl through, hiding the origin's
    /// real redirect. A 3xx probe result is returned as a synthetic
    /// redirect response; otherwise the original response is returned.
    fn send_manual(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let url = &request.url;
        validate_public_url(url).map_err(io_error)?;
        let cookie = request.header("Cookie");
        let user_agent = request.header("User-Agent").unwrap_or(&self.user_agent);

        let mut req = self.manual_redirect_client.get(url);
        if let Some(cookie) = cookie {
            if !is_safe_header_value(cookie) {
                return Err(io_error("unsafe Cookie header value"));
            }
            req = req.header("Cookie", cookie);
        }
        if user_agent != self.user_agent {
            req = req.header(USER_AGENT, user_agent);
        }

        let response = req.send().map_err(|e| NarouError::Http(e.to_string()))?;
        let status = response.status().as_u16();
        if (400..600).contains(&status) {
            if let Ok((code, Some(location))) = self.curl_probe_redirect(url, cookie, user_agent)
                && (300..400).contains(&code)
            {
                return Ok(HttpResponse {
                    status: code as u16,
                    headers: vec![("Location".into(), location)],
                    body: Vec::new(),
                });
            }
        }

        let headers = content_type_header(content_type_of(&response));
        let body = read_response_bytes(response)?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    /// POST via the follow client. Used by future API endpoints; the current
    /// downloader only issues GETs.
    fn send_post(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let url = &request.url;
        validate_public_url(url).map_err(io_error)?;
        let mut req = self.client.post(url);
        for (name, value) in &request.headers {
            if !is_safe_header_value(value) {
                return Err(io_error("unsafe header value"));
            }
            req = req.header(name, value);
        }
        if let Some(body) = &request.body {
            req = req.body(body.clone());
        }
        let response = req.send().map_err(|e| NarouError::Http(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = content_type_header(content_type_of(&response));
        let body = read_response_bytes(response)?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn fetch_tier_curl(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        let mut handle = self.configured_curl_handle(url, cookie, user_agent)?;

        let mut body = Vec::new();
        let mut content_type = String::new();
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
                .header_function(|header| {
                    let header = String::from_utf8_lossy(header);
                    if let Some(value) = header.strip_prefix("Content-Type:") {
                        content_type = value.trim().to_string();
                    }
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
        if (300..400).contains(&code) {
            return Err(io_error(format!(
                "HTTP redirect {code} while fetching {url}"
            )));
        }
        if code >= 400 {
            // 404/503 are definitive server answers; return them raw so the
            // policy layer maps them to domain errors. Other 4xx/5xx are
            // transport errors that continue the tier fallback.
            if code == 404 || code == 503 {
                return Ok(HttpResponse {
                    status: code as u16,
                    headers: content_type_header(content_type),
                    body,
                });
            }
            return Err(io_error(format!("HTTP {code} while fetching {url}")));
        }

        Ok(HttpResponse {
            status: code as u16,
            headers: content_type_header(content_type),
            body,
        })
    }

    fn fetch_tier_reqwest(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        let response = self.send_reqwest(url, cookie, user_agent)?;
        let status = response.status().as_u16();
        if status == 404 || status == 503 {
            let headers = content_type_header(content_type_of(&response));
            let body = read_response_bytes(response)?;
            return Ok(HttpResponse {
                status,
                headers,
                body,
            });
        }
        if !(200..300).contains(&status) {
            return Err(io_error(format!("HTTP {status} while fetching {url}")));
        }
        let headers = content_type_header(content_type_of(&response));
        let body = read_response_bytes(response)?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    /// Last-resort subprocess fetch. Windows has no `wget` but ships
    /// `curl.exe` since Windows 10, so the candidate order is
    /// platform-dependent: `curl` first on Windows, `wget` first elsewhere.
    /// A tool that is not installed falls through to the next candidate;
    /// a tool that runs but fails (HTTP error, timeout) is a real transport
    /// failure and is reported as-is.
    fn fetch_tier_subprocess(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        if let Some(cookie) = cookie
            && !is_safe_header_value(cookie)
        {
            return Err(io_error("unsafe Cookie header value"));
        }

        let candidates: &[&str] = if cfg!(windows) {
            &["curl", "wget"]
        } else {
            &["wget", "curl"]
        };
        let mut first_error: Option<NarouError> = None;
        let mut missing: Vec<&str> = Vec::new();
        for tool in candidates {
            match self.run_fetch_tool(tool, url, cookie, user_agent) {
                Ok(response) => return Ok(response),
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
        Err(first_error.unwrap_or_else(|| {
            io_error(format!(
                "no download tool available (tried: {})",
                missing.join(", ")
            ))
        }))
    }

    /// Run one subprocess fetch tool (`curl` or `wget`) against `url`.
    /// Both tools are invoked without redirect following and fail on
    /// non-2xx statuses, matching the historical wget tier semantics.
    fn run_fetch_tool(
        &self,
        tool: &str,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<HttpResponse> {
        let mut cmd = Command::new(tool);
        match tool {
            "curl" => {
                const STATUS_MARKER: &str = "\n__NAROU_HTTP_STATUS__";
                cmd.arg("-sS")
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
                    .arg("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
                    .arg("-H")
                    .arg("Accept-Language: ja,en-US;q=0.9,en;q=0.8")
                    .arg("-H")
                    .arg("Connection: keep-alive")
                    .arg("-w")
                    .arg(format!("{STATUS_MARKER}%{{http_code}}"));
                if let Some(cookie) = cookie {
                    cmd.arg("-H").arg(format!("Cookie: {cookie}"));
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
                let marker = STATUS_MARKER.as_bytes();
                let Some(pos) = output
                    .stdout
                    .windows(marker.len())
                    .rposition(|window| window == marker)
                else {
                    return Err(io_error(format!(
                        "curl status marker missing for {url}"
                    )));
                };
                let body = &output.stdout[..pos];
                let status_text = String::from_utf8_lossy(&output.stdout[pos + marker.len()..]);
                let status: u16 = status_text.trim().parse().unwrap_or(0);
                if !(200..300).contains(&status) {
                    return Err(io_error(format!(
                        "HTTP {status} while fetching {url} via curl"
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
                    headers: Vec::new(),
                    body: body.to_vec(),
                })
            }
            _ => {
                cmd.arg("--quiet")
                    .arg("--output-document=-")
                    .arg("--tries=1")
                    .arg(format!("--connect-timeout={CONNECT_TIMEOUT_SECS}"))
                    .arg(format!("--read-timeout={READ_TIMEOUT_SECS}"))
                    .arg(format!("--timeout={TOTAL_TIMEOUT_SECS}"))
                    .arg("--max-redirect=0")
                    .arg(format!("--max-filesize={MAX_RESPONSE_BYTES}"))
                    .arg(format!("--user-agent={user_agent}"))
                    .arg("--header=Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
                    .arg("--header=Accept-Language: ja,en-US;q=0.9,en;q=0.8")
                    .arg("--header=Accept-Encoding: gzip, deflate")
                    .arg("--header=Connection: keep-alive");
                if let Some(cookie) = cookie {
                    cmd.arg(format!("--header=Cookie: {cookie}"));
                }
                cmd.arg("--").arg(url);
                let output = run_command_with_timeout(cmd, Duration::from_secs(TOTAL_TIMEOUT_SECS))
                    .map_err(NarouError::Io)?;
                if !output.status.success() {
                    return Err(io_error(format!(
                        "wget fetch failed for {url}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                if output.stdout.len() > MAX_RESPONSE_BYTES {
                    return Err(io_error(format!(
                        "response body exceeded {} bytes while fetching {url}",
                        MAX_RESPONSE_BYTES
                    )));
                }
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: output.stdout,
                })
            }
        }
    }

    fn send_reqwest(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<reqwest::blocking::Response> {
        if let Some(cookie) = cookie {
            if !is_safe_header_value(cookie) {
                return Err(io_error("unsafe Cookie header value"));
            }
            return self.send_reqwest_with_manual_cookie_redirects(url, cookie, user_agent);
        }

        let mut request = self.client.get(url);
        if user_agent != self.user_agent {
            request = request.header(USER_AGENT, user_agent);
        }
        request.send().map_err(|e| NarouError::Http(e.to_string()))
    }

    /// Follow redirects manually so the Cookie header can be dropped when a
    /// hop leaves the current site (see [`same_site_hosts`]).
    fn send_reqwest_with_manual_cookie_redirects(
        &self,
        url: &str,
        cookie: &str,
        user_agent: &str,
    ) -> Result<reqwest::blocking::Response> {
        let mut current = reqwest::Url::parse(url).map_err(|e| io_error(e.to_string()))?;
        let mut current_cookie = Some(cookie.to_string());

        for _ in 0..=MAX_REDIRECTS {
            validate_public_url(current.as_str()).map_err(io_error)?;
            let mut request = self.manual_redirect_client.get(current.clone());
            if let Some(cookie) = current_cookie.as_deref() {
                request = request.header("Cookie", cookie);
            }
            if user_agent != self.user_agent {
                request = request.header(USER_AGENT, user_agent);
            }

            let response = request
                .send()
                .map_err(|e| NarouError::Http(e.to_string()))?;
            if response.status().is_redirection() {
                let Some(location) = response.headers().get(LOCATION) else {
                    return Ok(response);
                };
                let location = location
                    .to_str()
                    .map_err(|e| io_error(format!("invalid redirect location: {e}")))?;
                let next = current
                    .join(location)
                    .map_err(|e| io_error(e.to_string()))?;
                if !same_site_hosts(next.host_str(), current.host_str()) {
                    current_cookie = None;
                }
                current = next;
                continue;
            }
            return Ok(response);
        }

        Err(io_error(format!(
            "redirect limit exceeded for {url} after {} hops",
            MAX_REDIRECTS
        )))
    }

    fn configured_curl_handle(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<curl::easy::Easy> {
        let mut handle = curl::easy::Easy::new();
        handle.url(url).map_err(|e| io_error(e.to_string()))?;
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
        handle.accept_encoding("gzip, deflate").ok();

        let mut headers = curl::easy::List::new();
        headers
            .append("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
            .map_err(|e| io_error(e.to_string()))?;
        headers
            .append("Accept-Language: ja,en-US;q=0.9,en;q=0.8")
            .map_err(|e| io_error(e.to_string()))?;
        headers
            .append("Accept-Charset: utf-8")
            .map_err(|e| io_error(e.to_string()))?;
        headers
            .append("Connection: keep-alive")
            .map_err(|e| io_error(e.to_string()))?;
        if let Some(cookie) = cookie {
            if !is_safe_header_value(cookie) {
                return Err(io_error("unsafe Cookie header value"));
            }
            headers
                .append(&format!("Cookie: {cookie}"))
                .map_err(|e| io_error(e.to_string()))?;
        }
        handle
            .http_headers(headers)
            .map_err(|e| io_error(e.to_string()))?;
        Ok(handle)
    }

    /// Perform a single non-following GET via libcurl and report the response
    /// status code plus the redirect target (when the status is 3xx). Used as
    /// a fallback by `send_manual` when the reqwest hop is blocked (e.g. a
    /// CDN challenge that passes curl but rejects reqwest), so redirect
    /// chains can still be observed. The response body is discarded.
    fn curl_probe_redirect(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<(u32, Option<String>)> {
        let mut handle = self.configured_curl_handle(url, cookie, user_agent)?;

        let mut received = 0usize;
        let perform_result = {
            let mut transfer = handle.transfer();
            transfer
                .write_function(move |data| {
                    received += data.len();
                    if received > MAX_RESPONSE_BYTES {
                        return Ok(0);
                    }
                    Ok(data.len())
                })
                .map_err(|e| io_error(e.to_string()))?;
            transfer.perform()
        };
        perform_result.map_err(|e| io_error(e.to_string()))?;

        let code = handle
            .response_code()
            .map_err(|e| io_error(e.to_string()))?;
        let redirect = handle.redirect_url().ok().flatten().map(str::to_string);
        Ok((code, redirect))
    }
}

impl HttpClient for NativeHttpClient {
    fn send<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || this.send_blocking(&request))
                .await
                .map_err(|e| NarouError::Http(e.to_string()))?
        })
    }
}

fn build_reqwest_client(
    user_agent: &str,
    follow_redirects: bool,
) -> Result<reqwest::blocking::Client> {
    let redirect_policy = if follow_redirects {
        reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.stop()
            } else if validate_public_url(attempt.url().as_str()).is_err() {
                attempt.stop()
            } else {
                attempt.follow()
            }
        })
    } else {
        reqwest::redirect::Policy::none()
    };

    reqwest::blocking::Client::builder()
        .user_agent(user_agent)
        .default_headers(default_request_headers())
        .cookie_store(true)
        .http1_only()
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(TOTAL_TIMEOUT_SECS))
        .redirect(redirect_policy)
        .build()
        .map_err(|e| NarouError::Http(e.to_string()))
}

fn read_response_bytes(mut response: reqwest::blocking::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = response.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        if body.len() + read > MAX_RESPONSE_BYTES {
            return Err(io_error(format!(
                "response body exceeded {} bytes",
                MAX_RESPONSE_BYTES
            )));
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

    let (stdout_tx, stdout_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });

    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });

    let started_at = Instant::now();
    let status = loop {
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
        stdout: stdout_rx.recv().unwrap_or_default(),
        stderr: stderr_rx.recv().unwrap_or_default(),
    })
}

fn io_error(message: impl Into<String>) -> NarouError {
    std::io::Error::other(message.into()).into()
}

fn content_type_of(response: &reqwest::blocking::Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

fn content_type_header(content_type: String) -> Vec<(String, String)> {
    if content_type.is_empty() {
        Vec::new()
    } else {
        vec![("Content-Type".into(), content_type)]
    }
}

pub fn default_request_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("ja,en-US;q=0.9,en;q=0.8"),
    );
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    );
    headers.insert(ACCEPT_CHARSET, HeaderValue::from_static("utf-8"));
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    headers
}

#[cfg(test)]
mod tests {
    use super::NativeHttpClient;
    use crate::error::NarouError;
    use std::io::{Read, Write};
    use std::net::TcpListener;
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
        let response = client.fetch_tier_curl(&url, None, "narou_rs-test").unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 404);
    }

    #[test]
    fn curl_redirect_is_not_returned_as_success() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/redirect", listener.local_addr().unwrap());

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        let err = client
            .fetch_tier_curl(&url, Some("over18=yes"), "narou_rs-test")
            .unwrap_err();
        server.join().unwrap();

        assert!(err.to_string().contains("HTTP redirect 302"));
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
        let response = client.fetch_tier_curl(&url, None, "narou_rs-test").unwrap();
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
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream.write_all(response).unwrap();
        });
        (url, server)
    }

    #[test]
    fn subprocess_tier_fetches_200_body() {
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        match client.fetch_tier_subprocess(&url, None, "narou_rs-test") {
            Ok(response) => {
                assert_eq!(response.status, 200);
                assert_eq!(response.body, b"hello");
            }
            // No curl/wget on this machine: nothing to verify.
            Err(err) if err.to_string().contains("no download tool available") => {}
            Err(err) => panic!("subprocess fetch failed: {err}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn subprocess_tier_treats_non_2xx_as_error() {
        let (url, server) = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        match client.fetch_tier_subprocess(&url, None, "narou_rs-test") {
            Err(err) if err.to_string().contains("no download tool available") => {}
            Err(err) => assert!(err.to_string().contains("404"), "{err}"),
            Ok(_) => panic!("404 must not succeed"),
        }
        server.join().unwrap();
    }

    #[tokio::test]
    async fn constructor_is_safe_inside_async_runtime() {
        let client = NativeHttpClient::new("narou_rs-test").unwrap();
        assert_eq!(client.user_agent, "narou_rs-test");
        drop(client);
    }
}
