use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
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

#[derive(Debug, Clone)]
struct ClientMessage {
    id: u64,
    payload: Arc<String>,
    target_console: Option<String>,
    scope: Option<String>,
    replayed_by_history: bool,
}

#[derive(Debug, Clone)]
struct ConsoleHistoryEntry {
    id: u64,
    body: String,
    target_console: String,
    scope: Option<String>,
    payload: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WsClientQuery {
    #[serde(default)]
    target_console: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ClientFilter {
    target_console: Option<String>,
    scope: Option<String>,
}

impl From<WsClientQuery> for ClientFilter {
    fn from(value: WsClientQuery) -> Self {
        Self {
            target_console: normalize_filter_value(value.target_console),
            scope: normalize_filter_value(value.scope),
        }
    }
}

#[derive(Debug)]
pub struct PushServer {
    channel: BroadcastChannel,
    clients: DashMap<usize, mpsc::UnboundedSender<ClientMessage>>,
    connected_clients: AtomicUsize,
    next_client_id: AtomicUsize,
    next_message_id: AtomicU64,
    console_history: Mutex<Vec<ConsoleHistoryEntry>>,
    max_history: usize,
    max_clients: usize,
    accepted_domains: Vec<String>,
}

impl PushServer {
    pub fn new() -> Self {
        Self {
            channel: BroadcastChannel::new(),
            clients: DashMap::new(),
            connected_clients: AtomicUsize::new(0),
            next_client_id: AtomicUsize::new(1),
            next_message_id: AtomicU64::new(0),
            console_history: Mutex::new(Vec::new()),
            max_history: 10000,
            max_clients: MAX_WS_CLIENTS,
            accepted_domains: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        }
    }

    #[cfg(test)]
    fn new_with_limits(max_clients: usize, max_history: usize) -> Self {
        Self {
            channel: BroadcastChannel::new(),
            clients: DashMap::new(),
            connected_clients: AtomicUsize::new(0),
            next_client_id: AtomicUsize::new(1),
            next_message_id: AtomicU64::new(0),
            console_history: Mutex::new(Vec::new()),
            max_history,
            max_clients,
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
        self.connected_clients.load(Ordering::Acquire)
    }

    pub fn broadcast(&self, event_type: &str, data: &str) {
        self.publish_json(
            serde_json::json!({
                "type": event_type,
                "data": data,
            }),
            Some((data, "stdout")),
        );
    }

    /// Send a control event (table.reload, queue_start, etc.) without polluting console history.
    pub fn broadcast_event(&self, event_type: &str, data: &str) {
        self.publish_json(
            serde_json::json!({
                "type": event_type,
                "data": data,
            }),
            None,
        );
    }

    /// Send an echo event to stream subprocess output to the browser console.
    /// Matches Ruby's `{echo: {target_console: "stdout", body: "...", no_history: false}}`.
    pub fn broadcast_echo(&self, body: &str, target_console: &str) {
        self.publish_json(
            serde_json::json!({
                "type": "echo",
                "body": body,
                "target_console": target_console,
            }),
            Some((body, target_console)),
        );
    }

    /// Send a pre-built JSON message directly (used by WebProgress interception in worker).
    pub fn broadcast_raw(&self, value: &serde_json::Value) {
        self.publish_json(value.clone(), None);
    }

    pub fn broadcast_progress(&self, current: usize, total: usize, message: &str) {
        self.publish_json(
            serde_json::json!({
                "type": "progress",
                "current": current,
                "total": total,
                "message": message,
            }),
            None,
        );
    }

    pub fn broadcast_log(&self, level: &str, message: &str) {
        self.publish_json(
            serde_json::json!({
                "type": "log",
                "level": level,
                "message": message,
            }),
            Some((message, "stdout")),
        );
    }

    pub fn broadcast_error(&self, message: &str) {
        self.broadcast("error", message);
    }

    pub fn broadcast_progressbar_init(&self, topic: &str) {
        self.broadcast_progressbar_init_to(topic, "stdout");
    }

    pub fn broadcast_progressbar_init_to(&self, topic: &str, target_console: &str) {
        self.publish_json(
            serde_json::json!({
                "type": "progressbar.init",
                "data": { "topic": topic },
                "target_console": target_console,
            }),
            None,
        );
    }

    pub fn broadcast_progressbar_step(&self, percent: f64, topic: &str) {
        self.broadcast_progressbar_step_to(percent, topic, "stdout");
    }

    pub fn broadcast_progressbar_step_to(&self, percent: f64, topic: &str, target_console: &str) {
        self.publish_json(
            serde_json::json!({
                "type": "progressbar.step",
                "data": { "percent": percent, "topic": topic },
                "target_console": target_console,
            }),
            None,
        );
    }

    pub fn broadcast_progressbar_clear(&self, topic: &str) {
        self.broadcast_progressbar_clear_to(topic, "stdout");
    }

    pub fn broadcast_progressbar_clear_to(&self, topic: &str, target_console: &str) {
        self.publish_json(
            serde_json::json!({
                "type": "progressbar.clear",
                "data": { "topic": topic },
                "target_console": target_console,
            }),
            None,
        );
    }

    fn publish_json(&self, value: serde_json::Value, history: Option<(&str, &str)>) {
        let payload = value.to_string();
        let id = self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1;
        let target_console = message_target_console(&value);
        let scope = message_scope(&value);
        if let Some((body, history_target_console)) = history {
            self.append_history(id, body, history_target_console, scope.clone());
        }
        self.channel.send(&payload);
        self.dispatch_to_clients(ClientMessage {
            id,
            payload: Arc::new(payload),
            target_console,
            scope,
            replayed_by_history: history.is_some(),
        });
    }

    fn dispatch_to_clients(&self, message: ClientMessage) {
        let mut stale_client_ids = Vec::new();
        for client in self.clients.iter() {
            if client.value().send(message.clone()).is_err() {
                stale_client_ids.push(*client.key());
            }
        }
        for client_id in stale_client_ids {
            self.unregister_client(client_id);
        }
    }

    fn try_register_client(&self, sender: mpsc::UnboundedSender<ClientMessage>) -> Option<usize> {
        loop {
            let current = self.connected_clients.load(Ordering::Acquire);
            if current >= self.max_clients {
                return None;
            }
            if self
                .connected_clients
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        self.clients.insert(id, sender);
        Some(id)
    }

    fn unregister_client(&self, id: usize) {
        if self.clients.remove(&id).is_some() {
            self.connected_clients.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn history_snapshot(&self) -> Vec<ConsoleHistoryEntry> {
        let history = self.console_history.lock();
        let mut selected = Vec::new();
        let mut total_bytes = 0usize;
        for entry in history.iter().rev() {
            let entry_bytes = entry.payload.len();
            if selected.len() >= MAX_WS_HISTORY_LINES
                || total_bytes.saturating_add(entry_bytes) > MAX_WS_HISTORY_BYTES
            {
                break;
            }
            total_bytes += entry_bytes;
            selected.push(entry.clone());
        }
        selected.reverse();
        selected
    }

    fn append_history(&self, id: u64, message: &str, target_console: &str, scope: Option<String>) {
        let mut history = self.console_history.lock();
        history.push(ConsoleHistoryEntry {
            id,
            body: message.to_string(),
            target_console: target_console.to_string(),
            scope,
            payload: history_payload(message, target_console),
        });
        if history.len() > self.max_history {
            let drain_count = history.len() - self.max_history + 500;
            history.drain(..drain_count);
        }
    }

    fn client_matches_filter(
        filter: &ClientFilter,
        target_console: Option<&str>,
        scope: Option<&str>,
    ) -> bool {
        let matches_target_console = filter
            .target_console
            .as_deref()
            .is_none_or(|expected| target_console.is_none_or(|actual| actual == expected));
        let matches_scope = filter
            .scope
            .as_deref()
            .is_none_or(|expected| scope.is_none_or(|actual| actual == expected));
        matches_target_console && matches_scope
    }

    pub fn get_history_for(&self, stream: Option<&str>) -> String {
        let history = self.console_history.lock();
        let target_stream = stream.unwrap_or("stdout");
        history
            .iter()
            .filter(|entry| entry.target_console == target_stream)
            .map(|entry| entry.body.as_str())
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
        history[start..]
            .iter()
            .map(|entry| entry.body.clone())
            .collect()
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
    Query(query): Query<WsClientQuery>,
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
    let client_filter = ClientFilter::from(query);
    ws.max_frame_size(MAX_WS_FRAME_SIZE)
        .max_message_size(MAX_WS_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state.push_server, client_filter))
        .into_response()
}

fn validate_ws_request(headers: &HeaderMap, state: &AppState) -> Result<(), StatusCode> {
    if !super::request_host_allowed(headers, state, state.ws_port)
        && !super::request_host_allowed(headers, state, state.port)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !super::basic_auth_matches(headers, state.basic_auth_header.as_deref()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !state.push_server.accepts_origin(origin)
        || !super::origin_allowed(headers, state, state.port)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

async fn handle_socket(socket: WebSocket, push_server: Arc<PushServer>, client_filter: ClientFilter) {
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let Some(client_id) = push_server.try_register_client(client_tx) else {
        let (mut sender, _) = socket.split();
        let _ = sender.send(Message::Close(None)).await;
        return;
    };

    let history = push_server.history_snapshot();
    let replayed_history_ids: HashSet<u64> = history.iter().map(|entry| entry.id).collect();
    let (mut sender, mut receiver) = socket.split();
    let mut ping_interval = tokio::time::interval(WS_PING_INTERVAL);
    let mut last_activity = Instant::now();

    for entry in history {
        if !PushServer::client_matches_filter(
            &client_filter,
            Some(entry.target_console.as_str()),
            entry.scope.as_deref(),
        ) {
            continue;
        }
        if sender
            .send(Message::Text(entry.payload.into()))
            .await
            .is_err()
        {
            push_server.unregister_client(client_id);
            return;
        }
    }

    loop {
        tokio::select! {
            message = client_rx.recv() => {
                let Some(message) = message else {
                    break;
                };
                if message.replayed_by_history && replayed_history_ids.contains(&message.id) {
                    continue;
                }
                if !PushServer::client_matches_filter(
                    &client_filter,
                    message.target_console.as_deref(),
                    message.scope.as_deref(),
                ) {
                    continue;
                }
                if sender
                    .send(Message::Text((*message.payload).clone().into()))
                    .await
                    .is_err()
                {
                    break;
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

fn normalize_filter_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn message_target_console(value: &serde_json::Value) -> Option<String> {
    value
        .get("target_console")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn message_scope(value: &serde_json::Value) -> Option<String> {
    value
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| data.get("scope"))
                .and_then(serde_json::Value::as_str)
        })
        .map(ToOwned::to_owned)
}

fn history_payload(body: &str, target_console: &str) -> String {
    serde_json::json!({
        "type": "echo",
        "body": body,
        "target_console": target_console,
    })
    .to_string()
}

fn origin_to_domain(origin: &str) -> String {
    let trimmed = origin.trim();
    if trimmed.is_empty() || trimmed == "null" || trimmed == "file://" {
        return "null".to_string();
    }
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
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
        assert_eq!(
            origin_to_domain("https://Example.com:8080/path"),
            "Example.com"
        );
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
            reverse_proxy_mode: false,
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
        headers.insert(
            header::HOST,
            header::HeaderValue::from_static("localhost:4001"),
        );
        headers.insert(
            header::ORIGIN,
            header::HeaderValue::from_static("http://localhost:4000"),
        );
        assert_eq!(
            validate_ws_request(&headers, &state),
            Err(StatusCode::UNAUTHORIZED)
        );

        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(validate_ws_request(&headers, &state), Ok(()));

        headers.insert(
            header::ORIGIN,
            header::HeaderValue::from_static("http://evil.test:4000"),
        );
        assert_eq!(
            validate_ws_request(&headers, &state),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn ws_request_accepts_same_origin_proxy_mode() {
        let queue_dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("push-proxy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&queue_dir);
        std::fs::create_dir_all(&queue_dir).unwrap();

        let mut push_server = PushServer::new();
        push_server.set_accepted_domains(Vec::<String>::new());
        let state = AppState {
            port: 4000,
            ws_port: 4001,
            push_server: Arc::new(push_server),
            basic_auth_header: Some("Basic dXNlcjpwYXNz".to_string()),
            control_token: "control-token".to_string(),
            allowed_request_hosts: vec!["localhost".to_string()],
            reverse_proxy_mode: true,
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
        headers.insert(
            header::HOST,
            header::HeaderValue::from_static("narou.example.com:8443"),
        );
        headers.insert(
            header::ORIGIN,
            header::HeaderValue::from_static("https://narou.example.com:8443"),
        );
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(validate_ws_request(&headers, &state), Ok(()));
    }

    #[test]
    fn register_client_respects_max_clients_limit() {
        let server = PushServer::new_with_limits(1, 16);
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();

        let first_id = server.try_register_client(tx1).expect("first client");
        assert_eq!(server.client_count(), 1);
        assert!(server.try_register_client(tx2).is_none());

        server.unregister_client(first_id);
        assert_eq!(server.client_count(), 0);

        let (tx3, _rx3) = mpsc::unbounded_channel();
        assert!(server.try_register_client(tx3).is_some());
    }

    #[test]
    fn history_snapshot_ids_allow_live_dedup_without_dropping_control_events() {
        let server = PushServer::new_with_limits(4, 16);
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let client_id = server.try_register_client(client_tx).expect("client");

        server.broadcast_echo("history line", "stdout");
        server.broadcast_raw(&serde_json::json!({ "type": "table.reload" }));

        let history = server.history_snapshot();
        let history_ids: HashSet<u64> = history.iter().map(|entry| entry.id).collect();
        let first_live = client_rx.try_recv().expect("history message");
        let second_live = client_rx.try_recv().expect("control message");

        assert!(first_live.replayed_by_history);
        assert!(history_ids.contains(&first_live.id));
        assert_eq!(serde_json::from_str::<serde_json::Value>(first_live.payload.as_str()).unwrap()["type"], "echo");

        assert!(!second_live.replayed_by_history);
        assert!(!history_ids.contains(&second_live.id));
        assert_eq!(serde_json::from_str::<serde_json::Value>(second_live.payload.as_str()).unwrap()["type"], "table.reload");

        server.unregister_client(client_id);
    }

    #[test]
    fn client_filter_allows_global_events_and_matching_console_scope() {
        let filter = ClientFilter {
            target_console: Some("stdout2".to_string()),
            scope: Some("job-123".to_string()),
        };

        assert!(PushServer::client_matches_filter(&filter, None, None));
        assert!(PushServer::client_matches_filter(
            &filter,
            Some("stdout2"),
            Some("job-123")
        ));
        assert!(!PushServer::client_matches_filter(
            &filter,
            Some("stdout"),
            Some("job-123")
        ));
        assert!(!PushServer::client_matches_filter(
            &filter,
            Some("stdout2"),
            Some("job-999")
        ));
    }
}

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
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
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
