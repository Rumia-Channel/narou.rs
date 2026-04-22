use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use reqwest::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONNECTION, HeaderMap, HeaderValue,
    LOCATION,
};

use crate::compat::configure_hidden_console_command;
use crate::error::{NarouError, Result};

use super::rate_limit::RateLimiter;
use super::security::{
    CONNECT_TIMEOUT_SECS, MAX_REDIRECTS, MAX_RESPONSE_BYTES, READ_TIMEOUT_SECS,
    TOTAL_TIMEOUT_SECS, is_safe_header_value, validate_public_url,
};

const FAIL_THRESHOLD: u8 = 5;

pub struct HttpFetcher {
    pub client: reqwest::blocking::Client,
    pub manual_redirect_client: reqwest::blocking::Client,
    pub user_agent: String,
    pub tier_failures: HashMap<String, [u8; 3]>,
    pub rate_limiter: RateLimiter,
    pub prefer_curl: bool,
}

impl HttpFetcher {
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = build_reqwest_client(user_agent, true)?;
        let manual_redirect_client = build_reqwest_client(user_agent, false)?;

        Ok(Self {
            client,
            manual_redirect_client,
            user_agent: user_agent.to_string(),
            tier_failures: HashMap::new(),
            rate_limiter: RateLimiter::new(false),
            prefer_curl: false,
        })
    }

    pub fn configure_rate_limiter(&mut self, is_narou: bool) {
        self.rate_limiter = RateLimiter::new(is_narou);
    }

    pub fn fetch_text(
        &mut self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<String> {
        validate_public_url(url).map_err(io_error)?;
        let domain = domain_of(url).to_string();
        let mut last_error = None;

        if self.prefer_curl {
            match self.fetch_tier_curl(url, cookie, encoding) {
                Ok(body) => return Ok(body),
                Err(err) => last_error = Some(err),
            }
        }

        let skip_curl = self
            .tier_failures
            .get(&domain)
            .map_or(false, |f| f[0] >= FAIL_THRESHOLD);
        let skip_reqwest = self
            .tier_failures
            .get(&domain)
            .map_or(false, |f| f[1] >= FAIL_THRESHOLD);
        let skip_wget = self
            .tier_failures
            .get(&domain)
            .map_or(false, |f| f[2] >= FAIL_THRESHOLD);

        if !skip_curl && !self.prefer_curl {
            match self.fetch_tier_curl(url, cookie, encoding) {
                Ok(body) => {
                    self.prefer_curl = true;
                    return Ok(body);
                }
                Err(err) => {
                    last_error = Some(err);
                    self.tier_failures.entry(domain.clone()).or_insert([0; 3])[0] += 1;
                }
            }
        }

        if !skip_reqwest {
            match self.fetch_tier_reqwest(url, cookie, encoding) {
                Ok(body) => return Ok(body),
                Err(err) => {
                    last_error = Some(err);
                    self.tier_failures.entry(domain.clone()).or_insert([0; 3])[1] += 1;
                }
            }
        }

        if !skip_wget {
            match self.fetch_tier_wget(url, cookie, encoding) {
                Ok(body) => return Ok(body),
                Err(err) => {
                    last_error = Some(err);
                    self.tier_failures.entry(domain.clone()).or_insert([0; 3])[2] += 1;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| NarouError::NotFound(url.to_string())))
    }

    pub fn fetch_bytes(&self, url: &str, cookie: Option<&str>) -> Result<Vec<u8>> {
        validate_public_url(url).map_err(io_error)?;
        let response = self.send_reqwest(url, cookie)?;
        let response = ensure_success_status(url, response)?;
        read_response_bytes(response)
    }

    pub fn fetch_tier_curl(
        &self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<String> {
        let mut handle = curl::easy::Easy::new();
        handle.url(url).map_err(|e| io_error(e.to_string()))?;
        handle
            .useragent(&self.user_agent)
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .follow_location(cookie.is_none())
            .map_err(|e| io_error(e.to_string()))?;
        handle
            .max_redirections(MAX_REDIRECTS as u32)
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

        let mut body = Vec::new();
        let mut response_too_large = false;
        {
            let mut transfer = handle.transfer();
            transfer
                .write_function(|data| {
                    if body.len() + data.len() > MAX_RESPONSE_BYTES {
                        response_too_large = true;
                        return Err(curl::easy::WriteError::Pause);
                    }
                    body.extend_from_slice(data);
                    Ok(data.len())
                })
                .map_err(|e| io_error(e.to_string()))?;
            transfer.perform().map_err(|e| io_error(e.to_string()))?;
        }
        if response_too_large {
            return Err(io_error(format!(
                "response body exceeded {} bytes while fetching {url}",
                MAX_RESPONSE_BYTES
            )));
        }

        let code = handle
            .response_code()
            .map_err(|e| io_error(e.to_string()))?;
        if code >= 400 {
            return Err(io_error(format!("HTTP {code} while fetching {url}")));
        }

        Ok(decode_with_encoding(&body, encoding))
    }

    pub fn fetch_tier_reqwest(
        &self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<String> {
        let response = self.send_reqwest(url, cookie)?;
        let response = ensure_success_status(url, response)?;
        let bytes = read_response_bytes(response)?;
        Ok(decode_with_encoding(&bytes, encoding))
    }

    pub fn fetch_tier_wget(
        &self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<String> {
        let mut cmd = Command::new("wget");
        let max_redirects = if cookie.is_some() { 0 } else { MAX_REDIRECTS };
        cmd.arg("--quiet")
            .arg("--output-document=-")
            .arg("--tries=1")
            .arg(format!("--connect-timeout={CONNECT_TIMEOUT_SECS}"))
            .arg(format!("--read-timeout={READ_TIMEOUT_SECS}"))
            .arg(format!("--timeout={TOTAL_TIMEOUT_SECS}"))
            .arg(format!("--max-redirect={max_redirects}"))
            .arg(format!("--max-filesize={MAX_RESPONSE_BYTES}"))
            .arg(format!("--user-agent={}", &self.user_agent))
            .arg("--header=Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
            .arg("--header=Accept-Language: ja,en-US;q=0.9,en;q=0.8")
            .arg("--header=Accept-Encoding: gzip, deflate")
            .arg("--header=Connection: keep-alive");
        if let Some(cookie) = cookie {
            if !is_safe_header_value(cookie) {
                return Err(io_error("unsafe Cookie header value"));
            }
            cmd.arg(format!("--header=Cookie: {cookie}"));
        }
        cmd.arg("--").arg(url);
        let output =
            run_command_with_timeout(cmd, Duration::from_secs(TOTAL_TIMEOUT_SECS)).map_err(NarouError::Io)?;
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
        Ok(decode_with_encoding(&output.stdout, encoding))
    }
}

fn build_reqwest_client(user_agent: &str, follow_redirects: bool) -> Result<reqwest::blocking::Client> {
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

    Ok(reqwest::blocking::Client::builder()
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
        .build()?)
}

fn ensure_success_status(
    url: &str,
    response: reqwest::blocking::Response,
) -> Result<reqwest::blocking::Response> {
    let status = response.status();
    if status.as_u16() == 503 {
        return Err(NarouError::SuspendDownload("Rate limited (503)".into()));
    }
    if status.as_u16() == 404 {
        return Err(NarouError::NotFound(url.to_string()));
    }
    if !status.is_success() {
        return Err(response.error_for_status().unwrap_err().into());
    }
    Ok(response)
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

impl HttpFetcher {
    fn send_reqwest(
        &self,
        url: &str,
        cookie: Option<&str>,
    ) -> Result<reqwest::blocking::Response> {
        if let Some(cookie) = cookie {
            if !is_safe_header_value(cookie) {
                return Err(io_error("unsafe Cookie header value"));
            }
            return self.send_reqwest_with_manual_cookie_redirects(url, cookie);
        }

        Ok(self.client.get(url).send()?)
    }

    fn send_reqwest_with_manual_cookie_redirects(
        &self,
        url: &str,
        cookie: &str,
    ) -> Result<reqwest::blocking::Response> {
        let mut current = reqwest::Url::parse(url).map_err(|e| io_error(e.to_string()))?;
        let mut current_cookie = Some(cookie.to_string());

        for _ in 0..=MAX_REDIRECTS {
            validate_public_url(current.as_str()).map_err(io_error)?;
            let mut request = self.manual_redirect_client.get(current.clone());
            if let Some(cookie) = current_cookie.as_deref() {
                request = request.header("Cookie", cookie);
            }

            let response = request.send()?;
            if response.status().is_redirection() {
                let Some(location) = response.headers().get(LOCATION) else {
                    return Ok(response);
                };
                let location = location
                    .to_str()
                    .map_err(|e| io_error(format!("invalid redirect location: {e}")))?;
                let next = current.join(location).map_err(|e| io_error(e.to_string()))?;
                if next.host_str() != current.host_str() {
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
}

fn decode_with_encoding(bytes: &[u8], encoding: Option<&str>) -> String {
    let enc = match encoding {
        Some(e) if !e.eq_ignore_ascii_case("utf-8") && !e.eq_ignore_ascii_case("utf8") => e,
        _ => return String::from_utf8_lossy(bytes).into_owned(),
    };
    let encoder = encoding_rs::Encoding::for_label(enc.as_bytes());
    match encoder {
        Some(enc) => {
            let (cow, _encoding_used, _had_errors) = enc.decode(bytes);
            cow.into_owned()
        }
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

pub fn domain_of(url: &str) -> &str {
    let s = url.strip_prefix("https://").unwrap_or(url);
    let s = s.strip_prefix("http://").unwrap_or(s);
    s.split('/').next().unwrap_or(s)
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
