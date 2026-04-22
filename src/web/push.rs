use std::sync::Arc;

use axum::{
    Router,
    http::{HeaderMap, StatusCode, header},
    extract::State,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::get,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio::time::{Duration, Instant};

use super::AppState;

const MAX_WS_CLIENTS: usize = 64;
const MAX_WS_HISTORY_LINES: usize = 1000;
const MAX_WS_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_WS_FRAME_SIZE: usize = 64 * 1024;
const MAX_WS_MESSAGE_SIZE: usize = 256 * 1024;
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct BroadcastChannel {
    pub sender: broadcast::Sender<String>,
}

impl BroadcastChannel {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(4096);
        Self { sender }
    }

    pub fn send(&self, message: &str) {
        let _ = self.sender.send(message.to_string());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.sender.subscribe()
    }
}

#[derive(Debug)]
pub struct PushServer {
    channel: BroadcastChannel,
    clients: DashMap<usize, broadcast::Sender<String>>,
    next_client_id: Mutex<usize>,
    console_history: Mutex<Vec<(String, String)>>,
    max_history: usize,
    max_clients: usize,
    accepted_domains: Vec<String>,
}

impl PushServer {
    pub fn new() -> Self {
        Self {
            channel: BroadcastChannel::new(),
            clients: DashMap::new(),
            next_client_id: Mutex::new(1),
            console_history: Mutex::new(Vec::new()),
            max_history: 10000,
            max_clients: MAX_WS_CLIENTS,
            accepted_domains: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        }
    }

    pub fn channel(&self) -> &BroadcastChannel {
        &self.channel
    }

    pub fn set_accepted_domains<I, S>(&mut self, domains: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let collected: Vec<String> = domains
            .into_iter()
            .map(Into::into)
            .map(|domain| domain.trim().to_string())
            .filter(|domain| !domain.is_empty())
            .collect();
        self.accepted_domains = if collected.is_empty() {
            vec!["*".to_string()]
        } else {
            collected
        };
    }

    pub fn accepted_domains(&self) -> &[String] {
        &self.accepted_domains
    }

    pub fn accepts_origin(&self, origin: &str) -> bool {
        let domain = origin_to_domain(origin);
        if domain == "null" {
            return false;
        }
        self.accepted_domains.iter().any(|pattern| {
            pattern == "*"
                || wildcard_match(&pattern.to_ascii_lowercase(), &domain.to_ascii_lowercase())
        })
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn broadcast(&self, event_type: &str, data: &str) {
        let message = serde_json::json!({
            "type": event_type,
            "data": data,
        });
        let msg_str = message.to_string();
        self.append_history(data, "stdout");
        self.channel.send(&msg_str);
    }

    /// Send a control event (table.reload, queue_start, etc.) without polluting console history.
    pub fn broadcast_event(&self, event_type: &str, data: &str) {
        let message = serde_json::json!({
            "type": event_type,
            "data": data,
        });
        self.channel.send(&message.to_string());
    }

    /// Send an echo event to stream subprocess output to the browser console.
    /// Matches Ruby's `{echo: {target_console: "stdout", body: "...", no_history: false}}`.
    pub fn broadcast_echo(&self, body: &str, target_console: &str) {
        let payload = serde_json::json!({
            "type": "echo",
            "body": body,
            "target_console": target_console,
        });
        self.append_history(body, target_console);
        self.channel.send(&payload.to_string());
    }

    /// Send a pre-built JSON message directly (used by WebProgress interception in worker).
    pub fn broadcast_raw(&self, value: &serde_json::Value) {
        self.channel.send(&value.to_string());
    }

    pub fn broadcast_progress(&self, current: usize, total: usize, message: &str) {
        let payload = serde_json::json!({
            "type": "progress",
            "current": current,
            "total": total,
            "message": message,
        });
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_log(&self, level: &str, message: &str) {
        let payload = serde_json::json!({
            "type": "log",
            "level": level,
            "message": message,
        });
        self.append_history(message, "stdout");
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_error(&self, message: &str) {
        self.broadcast("error", message);
    }

    pub fn broadcast_progressbar_init(&self, topic: &str) {
        self.broadcast_progressbar_init_to(topic, "stdout");
    }

    pub fn broadcast_progressbar_init_to(&self, topic: &str, target_console: &str) {
        let payload = serde_json::json!({
            "type": "progressbar.init",
            "data": { "topic": topic },
            "target_console": target_console,
        });
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_progressbar_step(&self, percent: f64, topic: &str) {
        self.broadcast_progressbar_step_to(percent, topic, "stdout");
    }

    pub fn broadcast_progressbar_step_to(&self, percent: f64, topic: &str, target_console: &str) {
        let payload = serde_json::json!({
            "type": "progressbar.step",
            "data": { "percent": percent, "topic": topic },
            "target_console": target_console,
        });
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_progressbar_clear(&self, topic: &str) {
        self.broadcast_progressbar_clear_to(topic, "stdout");
    }

    pub fn broadcast_progressbar_clear_to(&self, topic: &str, target_console: &str) {
        let payload = serde_json::json!({
            "type": "progressbar.clear",
            "data": { "topic": topic },
            "target_console": target_console,
        });
        self.channel.send(&payload.to_string());
    }

    pub fn try_register_client(&self, sender: broadcast::Sender<String>) -> Option<usize> {
        if self.client_count() >= self.max_clients {
            return None;
        }
        let mut id_guard = self.next_client_id.lock();
        let id = *id_guard;
        *id_guard += 1;
        self.clients.insert(id, sender);
        Some(id)
    }

    pub fn unregister_client(&self, id: usize) {
        self.clients.remove(&id);
    }

    fn history_snapshot(&self) -> Vec<(String, String)> {
        let history = self.console_history.lock();
        let mut selected = Vec::new();
        let mut total_bytes = 0usize;
        for (message, target_console) in history.iter().rev() {
            let entry_bytes = message.len() + target_console.len();
            if selected.len() >= MAX_WS_HISTORY_LINES
                || total_bytes.saturating_add(entry_bytes) > MAX_WS_HISTORY_BYTES
            {
                break;
            }
            total_bytes += entry_bytes;
            selected.push((message.clone(), target_console.clone()));
        }
        selected.reverse();
        selected
    }

    fn append_history(&self, message: &str, target_console: &str) {
        let mut history = self.console_history.lock();
        history.push((message.to_string(), target_console.to_string()));
        if history.len() > self.max_history {
            let drain_count = history.len() - self.max_history + 500;
            history.drain(..drain_count);
        }
    }

    pub fn get_history_for(&self, stream: Option<&str>) -> String {
        let history = self.console_history.lock();
        let target_stream = stream.unwrap_or("stdout");
        history
            .iter()
            .filter(|(_, target_console)| target_console == target_stream)
            .map(|(msg, _)| msg.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear_history(&self) {
        let mut history = self.console_history.lock();
        history.clear();
    }

    pub fn recent_logs(&self, count: usize) -> Vec<String> {
        let history = self.console_history.lock();
        let start = history.len().saturating_sub(count);
        history[start..].iter().map(|(msg, _)| msg.clone()).collect()
    }
}

impl Default for PushServer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_push_router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler_with_app_state))
        .with_state(state)
}

pub async fn ws_handler_with_app_state(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = validate_ws_request(&headers, &state) {
        let body = if status == StatusCode::UNAUTHORIZED {
            "Unauthorized"
        } else {
            "Forbidden"
        };
        let mut response = (status, body).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Basic realm=\"narou.rs\""),
            );
        }
        return response;
    }
    ws.max_frame_size(MAX_WS_FRAME_SIZE)
        .max_message_size(MAX_WS_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state.push_server))
        .into_response()
}

fn validate_ws_request(headers: &HeaderMap, state: &AppState) -> Result<(), StatusCode> {
    if !super::request_host_allowed(headers, state, state.ws_port) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !super::basic_auth_matches(headers, state.basic_auth_header.as_deref()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !state.push_server.accepts_origin(origin) || !super::origin_allowed(headers, state, state.port) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

async fn handle_socket(socket: WebSocket, push_server: Arc<PushServer>) {
    let Some(client_id) = push_server.try_register_client(push_server.channel().sender.clone()) else {
        let (mut sender, _) = socket.split();
        let _ = sender.send(Message::Close(None)).await;
        return;
    };

    let mut rx = push_server.channel().subscribe();
    let history = push_server.history_snapshot();
    let (mut sender, mut receiver) = socket.split();
    let mut ping_interval = tokio::time::interval(WS_PING_INTERVAL);
    let mut last_activity = Instant::now();

    for (body, target_console) in history {
        let payload = serde_json::json!({
            "type": "echo",
            "body": body,
            "target_console": target_console,
        });
        if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
            push_server.unregister_client(client_id);
            return;
        }
    }

    loop {
        tokio::select! {
            message = rx.recv() => {
                match message {
                    Ok(msg) => {
                        if sender.send(Message::Text(msg.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("WebSocket client lagged, skipped {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Ping(payload))) => {
                        last_activity = Instant::now();
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_)))
                    | Some(Ok(Message::Text(_)))
                    | Some(Ok(Message::Binary(_))) => {
                        last_activity = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
            _ = ping_interval.tick() => {
                if last_activity.elapsed() >= WS_IDLE_TIMEOUT {
                    let _ = sender.send(Message::Close(None)).await;
                    break;
                }
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    push_server.unregister_client(client_id);
}

fn origin_to_domain(origin: &str) -> String {
    let trimmed = origin.trim();
    if trimmed.is_empty() || trimmed == "null" || trimmed == "file://" {
        return "null".to_string();
    }
    let without_scheme = trimmed.split_once("://").map(|(_, rest)| rest).unwrap_or(trimmed);
    let host_port = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let host = host_port.split('@').next_back().unwrap_or(host_port);
    host.split(':').next().unwrap_or(host).to_string()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        b'*' => {
            wildcard_match_bytes(&pattern[1..], text)
                || (!text.is_empty() && wildcard_match_bytes(pattern, &text[1..]))
        }
        b'?' => !text.is_empty() && wildcard_match_bytes(&pattern[1..], &text[1..]),
        c => !text.is_empty() && c == text[0] && wildcard_match_bytes(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_origin_respects_wildcards() {
        let mut server = PushServer::new();
        server.set_accepted_domains(["127.0.0.1", "localhost", "*.example.com"]);
        assert!(server.accepts_origin("http://localhost:3000"));
        assert!(server.accepts_origin("https://api.example.com"));
        assert!(!server.accepts_origin("https://evil.test"));
    }

    #[test]
    fn origin_to_domain_handles_null_and_file() {
        assert_eq!(origin_to_domain("null"), "null");
        assert_eq!(origin_to_domain("file://"), "null");
        assert_eq!(origin_to_domain("https://Example.com:8080/path"), "Example.com");
    }

    #[test]
    fn accepts_origin_allows_ip_literals_only_when_enabled() {
        let mut server = PushServer::new();
        server.set_accepted_domains(["localhost"]);
        assert!(!server.accepts_origin("http://192.168.1.10:4001"));
        server.set_accepted_domains(["localhost", "192.168.1.10"]);
        assert!(server.accepts_origin("http://192.168.1.10:4001"));
    }

    #[test]
    fn ws_request_requires_basic_auth_and_valid_origin() {
        let queue_dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("push-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&queue_dir);
        std::fs::create_dir_all(&queue_dir).unwrap();

        let mut push_server = PushServer::new();
        push_server.set_accepted_domains(["localhost"]);
        let state = AppState {
            port: 4000,
            ws_port: 4001,
            push_server: Arc::new(push_server),
            basic_auth_header: Some("Basic dXNlcjpwYXNz".to_string()),
            control_token: "control-token".to_string(),
            allowed_request_hosts: vec!["localhost".to_string()],
            queue: Arc::new(
                crate::queue::PersistentQueue::new(&queue_dir.join("queue.yaml")).unwrap(),
            ),
            restore_prompt_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            restorable_tasks_available: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            running_jobs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            running_child_pids: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            auto_update_scheduler: Arc::new(parking_lot::Mutex::new(None)),
        };

        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, header::HeaderValue::from_static("localhost:4001"));
        headers.insert(header::ORIGIN, header::HeaderValue::from_static("http://localhost:4000"));
        assert_eq!(validate_ws_request(&headers, &state), Err(StatusCode::UNAUTHORIZED));

        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(validate_ws_request(&headers, &state), Ok(()));

        headers.insert(
            header::ORIGIN,
            header::HeaderValue::from_static("http://evil.test:4000"),
        );
        assert_eq!(validate_ws_request(&headers, &state), Err(StatusCode::FORBIDDEN));
    }
}

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StreamingLogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

pub struct StreamingLogger {
    push_server: Arc<PushServer>,
    buffer: Mutex<Vec<StreamingLogEntry>>,
    max_buffer: usize,
}

impl StreamingLogger {
    pub fn new(push_server: Arc<PushServer>) -> Self {
        Self {
            push_server,
            buffer: Mutex::new(Vec::new()),
            max_buffer: 1000,
        }
    }

    pub fn log(&self, level: &str, message: &str) {
        let entry = StreamingLogEntry {
            timestamp: chrono::Utc::now()
                .with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap())
                .format("%Y-%m-%d %H:%M:%S").to_string(),
            level: level.to_string(),
            message: message.to_string(),
        };

        {
            let mut buf = self.buffer.lock();
            if buf.len() >= self.max_buffer {
                let drain_count = buf.len() - self.max_buffer + 100;
                buf.drain(..drain_count);
            }
            buf.push(StreamingLogEntry {
                timestamp: entry.timestamp,
                level: entry.level,
                message: entry.message,
            });
        }

        self.push_server.broadcast_log(level, message);
    }

    pub fn info(&self, message: &str) {
        self.log("info", message);
    }

    pub fn warn(&self, message: &str) {
        self.log("warn", message);
    }

    pub fn error(&self, message: &str) {
        self.log("error", message);
    }

    pub fn recent_logs(&self, count: usize) -> Vec<StreamingLogEntry> {
        let buf = self.buffer.lock();
        let start = buf.len().saturating_sub(count);
        buf[start..].to_vec()
    }

    pub fn full_history(&self) -> String {
        let buf = self.buffer.lock();
        buf.iter()
            .map(|e| format!("[{}] {}", e.timestamp, e.message))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear_history(&self) {
        let mut buf = self.buffer.lock();
        buf.clear();
    }
}
