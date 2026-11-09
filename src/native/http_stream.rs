//! Blocking native engines bridged through a zero-capacity body channel.
//! At most one 64 KiB chunk is retained by a producer waiting for demand.

use super::*;
use futures::{SinkExt, channel::{mpsc as async_mpsc, oneshot}};
use crate::platform::{AssetStream, PlatformFuture, http::{HttpPacing, HttpStreamResponse}, staging::ASSET_CHUNK_BYTES};
use std::io::{BufRead, BufReader};

type Head = (u16, Vec<(String, String)>);
struct Sink {
    head: Option<oneshot::Sender<Result<Head>>>,
    body: async_mpsc::Sender<Result<Vec<u8>>>,
}
impl Sink {
    fn head(&mut self, status: u16, headers: Vec<(String, String)>) -> Result<()> {
        if let Some(sender) = self.head.take() {
            sender.send(Ok((status, headers))).map_err(|_| io_error("HTTP stream cancelled"))?;
        }
        Ok(())
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(ASSET_CHUNK_BYTES) {
            futures::executor::block_on(self.body.send(Ok(chunk.to_vec())))
                .map_err(|_| io_error("HTTP stream cancelled"))?;
        }
        Ok(())
    }
    fn fail(&mut self, error: NarouError) {
        if let Some(sender) = self.head.take() { let _ = sender.send(Err(error)); }
        else { let _ = futures::executor::block_on(self.body.send(Err(error))); }
    }
}

impl NativeHttpClient {
    async fn streaming_attempt(&self, request: HttpRequest, tier: usize, tool: Option<&'static str>) -> Result<HttpStreamResponse> {
        let (head_tx, head_rx) = oneshot::channel();
        let (body_tx, body_rx) = async_mpsc::channel(0);
        let worker = self.clone();
        let url = request.url.clone();
        std::thread::Builder::new().name("narou-http-stream".into()).spawn(move || {
            let mut sink = Sink { head: Some(head_tx), body: body_tx };
            let result = match tier {
                TIER_CURL => worker.stream_curl(&request, &mut sink),
                TIER_REQWEST => worker.stream_reqwest(&request, &mut sink),
                TIER_SUBPROCESS => worker.stream_tool(tool.expect("subprocess tool"), &request, &mut sink),
                _ => unreachable!(),
            };
            if let Err(error) = result { sink.fail(error); }
        }).map_err(NarouError::Io)?;
        let (status, headers) = head_rx.await.map_err(|_| io_error("HTTP producer stopped before response headers"))??;
        Ok(HttpStreamResponse { status, url, headers, body: Box::pin(body_rx) as AssetStream })
    }

    pub(super) fn send_stream_chain<'a>(&'a self, request: HttpRequest, pacing: Option<HttpPacing<'a>>) -> PlatformFuture<'a, Result<HttpStreamResponse>> {
        Box::pin(async move {
            let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
            loop {
                let (returned, validation) = tokio::task::spawn_blocking(move || {
                    let validation = Self::validate_hop(chain.request()); (chain, validation)
                }).await.map_err(|error| NarouError::Http(error.to_string()))?;
                chain = returned;
                validation?;
                let scope = pacing.map(|policy| policy.scope_for(&chain)).transpose()?;
                let tiered = chain.request().method == HttpMethod::Get && !chain.request().trusted_endpoint;
                let domain = crate::downloader::http_policy::domain_of(&chain.request().url).to_string();
                let mut order = if tiered { self.tier_order(&domain) } else { vec![TIER_REQWEST] };
                if tiered && chain.request().redirect == RedirectMode::Manual {
                    order.retain(|&tier| tier != TIER_CURL); order.insert(0, TIER_CURL);
                }
                let mut fallback = None;
                let mut last_error = None;
                let mut selected = None;
                'tiers: for tier in order {
                    let tools: &[Option<&'static str>] = if tier != TIER_SUBPROCESS { &[None] }
                        else if cfg!(windows) { &[Some("curl"), Some("wget")] }
                        else { &[Some("wget"), Some("curl")] };
                    for &tool in tools {
                        if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.acquire(scope).await?; }
                        let result = self.streaming_attempt(chain.request().clone(), tier, tool).await;
                        if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.complete(scope).await?; }
                        match result {
                            Ok(response) if tiered && should_try_next_tier(response.status) => {
                                self.tiers.lock().note_failure(&domain, tier);
                                // Only the most recent bounded diagnostic body is
                                // retained; dropping the previous one cancels it.
                                fallback = Some(response);
                            }
                            Ok(response) => {
                                if tiered { self.tiers.lock().note_success(&domain, tier); }
                                selected = Some(response); break 'tiers;
                            }
                            Err(error) => {
                                if tiered { self.tiers.lock().note_failure(&domain, tier); }
                                last_error = Some(error);
                            }
                        }
                    }
                }
                let response = selected.or(fallback).ok_or_else(|| last_error.unwrap_or_else(|| io_error("all HTTP transports failed")))?;
                let head = response.head();
                if let Some(store) = self.cookie_store.as_deref() {
                    crate::platform::cookie_store::persist_response_set_cookie(store, &head, chain.request().header("Cookie")).await;
                }
                if !chain.advance(&head)? { return Ok(response); }
            }
        })
    }

    fn stream_reqwest(&self, request: &HttpRequest, sink: &mut Sink) -> Result<()> {
        let mut builder = match request.method {
            HttpMethod::Get => self.client.get(&request.url), HttpMethod::Post => self.client.post(&request.url),
            HttpMethod::Put => self.client.put(&request.url), HttpMethod::Delete => self.client.delete(&request.url),
        };
        for (name, value) in &request.headers { builder = builder.header(name, value); }
        if let Some(body) = &request.body {
            builder = builder.body(reqwest::blocking::Body::sized(SharedRequestBody { body: body.clone(), position: 0 }, body.len() as u64));
        }
        let mut response = builder.send().map_err(|error| NarouError::Http(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        let expected = response.content_length();
        let limit = if !(200..300).contains(&status) { Some(MAX_RESPONSE_BYTES as u64) }
            else if request.trusted_endpoint { request.trusted_response_body_limit.map(|value| value as u64) } else { None };
        sink.head(status, headers)?;
        stream_reader(&mut response, sink, expected, limit)
    }

    fn stream_curl(&self, request: &HttpRequest, sink: &mut Sink) -> Result<()> {
        let mut handle = self.configured_curl_handle(request, request.header("User-Agent").unwrap_or(&self.user_agent))?;
        let sink = std::cell::RefCell::new(sink);
        let mut headers = CurlHeaders::default();
        let status = std::cell::Cell::new(0u16);
        let mut total = 0usize;
        let mut header_bytes = 0usize;
        let mut proxy_connect = false;
        let mut invalid_head = false;
        let mut transfer = handle.transfer();
        transfer.header_function(|line| {
            header_bytes = header_bytes.saturating_add(line.len());
            if header_bytes > MAX_HTTP1_HEAD { invalid_head = true; return false; }
            if line.starts_with(b"HTTP/") {
                status.set(parse_tool_status(line).unwrap_or(0));
                proxy_connect = String::from_utf8_lossy(line).to_ascii_lowercase().contains("connection established");
            }
            headers.feed(line);
            if matches!(line, b"\r\n" | b"\n") && status.get() >= 200 && !proxy_connect {
                return sink.borrow_mut().head(status.get(), std::mem::take(&mut headers.headers)).is_ok();
            }
            true
        }).map_err(|error| io_error(error.to_string()))?;
        transfer.write_function(|bytes| {
            total = total.saturating_add(bytes.len());
            if !(200..300).contains(&status.get()) && total > MAX_RESPONSE_BYTES { return Ok(0); }
            match sink.borrow_mut().bytes(bytes) { Ok(()) => Ok(bytes.len()), Err(_) => Ok(0) }
        }).map_err(|error| io_error(error.to_string()))?;
        let result = transfer.perform().map_err(|error| io_error(error.to_string()));
        drop(transfer);
        if invalid_head { return Err(io_error("HTTP response headers exceed limit")); }
        result
    }

    fn stream_tool(&self, tool: &str, request: &HttpRequest, sink: &mut Sink) -> Result<()> {
        let user_agent = request.header("User-Agent").unwrap_or(&self.user_agent);
        let mut command = Command::new(tool);
        if tool == "curl" {
            command.args(["-sS", "-D", "-", "--compressed", "--retry", "0", "--max-redirs", "0", "--no-buffer"])
                .arg("--connect-timeout").arg(CONNECT_TIMEOUT_SECS.to_string())
                .arg("--max-time").arg(TOTAL_TIMEOUT_SECS.to_string()).arg("-A").arg(user_agent);
            for (name, value) in [("Accept", DEFAULT_ACCEPT), ("Accept-Language", DEFAULT_ACCEPT_LANGUAGE), ("Connection", DEFAULT_CONNECTION)] {
                command.arg("-H").arg(format!("{name}: {value}"));
            }
            for (name, value) in &request.headers { command.arg("-H").arg(format!("{name}: {value}")); }
            command.arg(&request.url);
        } else {
            command.args(["--no-verbose", "--save-headers", "--server-response", "--content-on-error", "--compression=auto", "--output-document=-", "--tries=1", "--max-redirect=0"])
                .arg(format!("--connect-timeout={CONNECT_TIMEOUT_SECS}")).arg(format!("--read-timeout={READ_TIMEOUT_SECS}"))
                .arg(format!("--timeout={TOTAL_TIMEOUT_SECS}")).arg(format!("--user-agent={user_agent}"));
            for (name, value) in [("Accept", DEFAULT_ACCEPT), ("Accept-Language", DEFAULT_ACCEPT_LANGUAGE), ("Connection", DEFAULT_CONNECTION)] {
                command.arg(format!("--header={name}: {value}"));
            }
            for (name, value) in &request.headers { command.arg(format!("--header={name}: {value}")); }
            command.arg("--").arg(&request.url);
        }
        configure_hidden_console_command(&mut command);
        let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let output = child.stdout.take().expect("piped output");
        let stderr = child.stderr.take().expect("piped stderr");
        let errors = thread::spawn(move || {
            let mut reader = stderr;
            let mut retained = Vec::new();
            let mut buffer = [0; 4096];
            while let Ok(length) = reader.read(&mut buffer) {
                if length == 0 { break; }
                let take = length.min(MAX_HTTP1_HEAD.saturating_sub(retained.len()));
                retained.extend_from_slice(&buffer[..take]);
            }
            retained
        });
        let child = Arc::new(Mutex::new(child));
        let (done, wait) = mpsc::channel();
        let deadline_child = child.clone();
        let watchdog = thread::spawn(move || {
            if wait.recv_timeout(Duration::from_secs(TOTAL_TIMEOUT_SECS)).is_err() { let _ = deadline_child.lock().kill(); }
        });
        let mut guard = ChildGuard { child, done: Some(done), watchdog: Some(watchdog) };
        let mut reader = BufReader::with_capacity(ASSET_CHUNK_BYTES, output);
        let mut head = Vec::new();
        let (status, headers) = loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                let status = guard.wait()?;
                let stderr = errors.join().unwrap_or_default();
                if let Some((http_status, headers)) = parse_wget_response_head(&stderr)
                    && (status.success() || (status.code() == Some(8) && (300..600).contains(&http_status))) {
                    sink.head(http_status, headers)?; return Ok(());
                }
                return Err(io_error("download tool failed before response headers"));
            }
            let take = available.iter().position(|&byte| byte == b'\n').map_or(available.len(), |position| position + 1);
            if head.len().saturating_add(take) > MAX_HTTP1_HEAD { return Err(io_error("download tool response headers exceed limit")); }
            head.extend_from_slice(&available[..take]);
            reader.consume(take);
            if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                let status = parse_tool_status(&head).ok_or_else(|| io_error("invalid download tool response headers"))?;
                let proxy = String::from_utf8_lossy(&head).to_ascii_lowercase().contains("connection established");
                if status < 200 || proxy { head.clear(); continue; }
                break (status, parse_tool_headers(&head));
            }
        };
        let encoded = headers.iter().any(|(name, value)| name.eq_ignore_ascii_case("content-encoding") && !value.eq_ignore_ascii_case("identity"));
        let expected = if encoded { None } else { headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-length")).and_then(|(_, value)| value.parse::<u64>().ok()) };
        sink.head(status, headers)?;
        stream_reader(&mut reader, sink, expected, (!(200..300).contains(&status)).then_some(MAX_RESPONSE_BYTES as u64))?;
        let exit = guard.wait()?;
        let _ = errors.join();
        guard.finish();
        if !exit.success() && !(tool == "wget" && exit.code() == Some(8) && (300..600).contains(&status)) {
            return Err(io_error("download tool failed during streamed body"));
        }
        Ok(())
    }
}

fn stream_reader(reader: &mut impl Read, sink: &mut Sink, expected: Option<u64>, limit: Option<u64>) -> Result<()> {
    let mut bytes = [0; ASSET_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let length = reader.read(&mut bytes)?;
        if length == 0 { break; }
        total = total.checked_add(length as u64).ok_or_else(|| io_error("response length overflow"))?;
        if expected.is_some_and(|value| total > value) || limit.is_some_and(|value| total > value) {
            return Err(io_error("response exceeds declared length or body limit"));
        }
        sink.bytes(&bytes[..length])?;
    }
    if expected.is_some_and(|value| total != value) { return Err(io_error("truncated HTTP response")); }
    Ok(())
}

struct ChildGuard {
    child: Arc<Mutex<std::process::Child>>,
    done: Option<mpsc::Sender<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}
impl ChildGuard {
    fn wait(&self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.child.lock().try_wait()? { return Ok(status); }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn finish(&mut self) {
        if let Some(done) = self.done.take() { let _ = done.send(()); }
        if let Some(watchdog) = self.watchdog.take() { let _ = watchdog.join(); }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.lock().kill();
        let _ = self.child.lock().wait();
        self.finish();
    }
}
