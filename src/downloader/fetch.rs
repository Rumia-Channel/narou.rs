use std::collections::HashMap;

use reqwest::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONNECTION, HeaderMap, HeaderValue,
};

use crate::error::{NarouError, Result};

use super::rate_limit::RateLimiter;

const FAIL_THRESHOLD: u8 = 5;

pub struct HttpFetcher {
    pub client: reqwest::blocking::Client,
    pub user_agent: String,
    pub tier_failures: HashMap<String, [u8; 3]>,
    pub rate_limiter: RateLimiter,
    pub prefer_curl: bool,
}

impl HttpFetcher {
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(user_agent)
            .default_headers(default_request_headers())
            .cookie_store(true)
            .http1_only()
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
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
        let domain = domain_of(url).to_string();

        if self.prefer_curl {
            if let Some(body) = self.fetch_tier_curl(url, cookie, encoding) {
                return Ok(body);
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
            if let Some(body) = self.fetch_tier_curl(url, cookie, encoding) {
                self.prefer_curl = true;
                return Ok(body);
            }
            self.tier_failures.entry(domain.clone()).or_insert([0; 3])[0] += 1;
        }

        if !skip_reqwest {
            match self.fetch_tier_reqwest(url, cookie, encoding) {
                Ok(body) => return Ok(body),
                Err(_) => {
                    self.tier_failures.entry(domain.clone()).or_insert([0; 3])[1] += 1;
                }
            }
        }

        if !skip_wget {
            if let Some(body) = self.fetch_tier_wget(url, cookie, encoding) {
                return Ok(body);
            }
            self.tier_failures.entry(domain.clone()).or_insert([0; 3])[2] += 1;
        }

        Err(NarouError::NotFound(url.to_string()))
    }

    pub fn fetch_tier_curl(
        &self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Option<String> {
        let mut handle = curl::easy::Easy::new();
        handle.url(url).ok()?;
        handle.useragent(&self.user_agent).ok()?;
        handle.follow_location(true).ok()?;
        handle.accept_encoding("gzip, deflate").ok();

        let mut headers = curl::easy::List::new();
        headers
            .append("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
            .ok()?;
        headers
            .append("Accept-Language: ja,en-US;q=0.9,en;q=0.8")
            .ok()?;
        headers.append("Accept-Charset: utf-8").ok()?;
        headers.append("Connection: keep-alive").ok()?;
        if let Some(cookie) = cookie {
            headers.append(&format!("Cookie: {cookie}")).ok()?;
        }
        handle.http_headers(headers).ok()?;

        let mut body = Vec::new();
        {
            let mut transfer = handle.transfer();
            transfer
                .write_function(|data| {
                    body.extend_from_slice(data);
                    Ok(data.len())
                })
                .ok()?;
            transfer.perform().ok()?;
        }

        let code = handle.response_code().ok()?;
        if code >= 400 {
            return None;
        }

        Some(decode_with_encoding(&body, encoding))
    }

    pub fn fetch_tier_reqwest(
        &self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<String> {
        let mut request = self.client.get(url);
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        let response = request.send()?;
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
        match encoding {
            Some(e) if !e.eq_ignore_ascii_case("utf-8") && !e.eq_ignore_ascii_case("utf8") => {
                let bytes = response.bytes()?;
                Ok(decode_with_encoding(&bytes, encoding))
            }
            _ => Ok(response.text()?),
        }
    }

    pub fn fetch_tier_wget(
        &self,
        url: &str,
        cookie: Option<&str>,
        encoding: Option<&str>,
    ) -> Option<String> {
        let mut cmd = std::process::Command::new("wget");
        cmd.arg("--quiet")
            .arg("--output-document=-")
            .arg("--no-check-certificate")
            .arg(format!("--user-agent={}", &self.user_agent))
            .arg("--header=Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8")
            .arg("--header=Accept-Language: ja,en-US;q=0.9,en;q=0.8")
            .arg("--header=Accept-Encoding: gzip, deflate")
            .arg("--header=Connection: keep-alive");
        if let Some(cookie) = cookie {
            cmd.arg(format!("--header=Cookie: {cookie}"));
        }
        let output = cmd.arg(url).output().ok()?;
        if !output.status.success() {
            return None;
        }
        Some(decode_with_encoding(&output.stdout, encoding))
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
