use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use super::AppState;

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
    console_history: Mutex<Vec<String>>,
    max_history: usize,
}

impl PushServer {
    pub fn new() -> Self {
        Self {
            channel: BroadcastChannel::new(),
            clients: DashMap::new(),
            next_client_id: Mutex::new(1),
            console_history: Mutex::new(Vec::new()),
            max_history: 10000,
        }
    }

    pub fn channel(&self) -> &BroadcastChannel {
        &self.channel
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
        self.append_history(data);
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
        self.append_history(body);
        self.channel.send(&payload.to_string());
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
        self.append_history(message);
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_error(&self, message: &str) {
        self.broadcast("error", message);
    }

    pub fn broadcast_download_start(&self, target: &str) {
        self.broadcast("download_start", target);
    }

    pub fn broadcast_download_complete(&self, result: &str) {
        self.broadcast("download_complete", result);
    }

    pub fn broadcast_convert_start(&self, target: &str) {
        self.broadcast("convert_start", target);
    }

    pub fn broadcast_convert_complete(&self, result: &str) {
        self.broadcast("convert_complete", result);
    }

    pub fn broadcast_progressbar_init(&self, topic: &str) {
        let payload = serde_json::json!({
            "type": "progressbar.init",
            "data": { "topic": topic }
        });
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_progressbar_step(&self, percent: f64, topic: &str) {
        let payload = serde_json::json!({
            "type": "progressbar.step",
            "data": { "percent": percent, "topic": topic }
        });
        self.channel.send(&payload.to_string());
    }

    pub fn broadcast_progressbar_clear(&self, topic: &str) {
        let payload = serde_json::json!({
            "type": "progressbar.clear",
            "data": { "topic": topic }
        });
        self.channel.send(&payload.to_string());
    }

    pub fn register_client(&self, sender: broadcast::Sender<String>) -> usize {
        let mut id_guard = self.next_client_id.lock();
        let id = *id_guard;
        *id_guard += 1;
        self.clients.insert(id, sender);
        id
    }

    pub fn unregister_client(&self, id: usize) {
        self.clients.remove(&id);
    }

    fn append_history(&self, message: &str) {
        let mut history = self.console_history.lock();
        history.push(message.to_string());
        if history.len() > self.max_history {
            let drain_count = history.len() - self.max_history + 500;
            history.drain(..drain_count);
        }
    }

    pub fn get_history(&self) -> String {
        let history = self.console_history.lock();
        history.join("\n")
    }

    pub fn clear_history(&self) {
        let mut history = self.console_history.lock();
        history.clear();
    }

    pub fn recent_logs(&self, count: usize) -> Vec<String> {
        let history = self.console_history.lock();
        let start = history.len().saturating_sub(count);
        history[start..].to_vec()
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
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state.push_server))
}

async fn handle_socket(socket: WebSocket, push_server: Arc<PushServer>) {
    let mut rx = push_server.channel().subscribe();

    let (mut sender, mut _receiver) = socket.split();

    let client_id = push_server.register_client(push_server.channel().sender.clone());

    // Send console history to new client (Ruby parity: pushserver.rb lines 76-78)
    {
        let history = push_server.console_history.lock().clone();
        for entry in history {
            let payload = serde_json::json!({
                "type": "echo",
                "body": entry,
                "target_console": "stdout",
            });
            if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                push_server.unregister_client(client_id);
                return;
            }
        }
    }

    let result = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if sender.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("WebSocket client lagged, skipped {} messages", n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .await;

    push_server.unregister_client(client_id);
    let _ = result;
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
            timestamp: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
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
