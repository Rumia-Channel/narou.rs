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
        let (sender, _) = broadcast::channel(256);
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
}

impl PushServer {
    pub fn new() -> Self {
        Self {
            channel: BroadcastChannel::new(),
            clients: DashMap::new(),
            next_client_id: Mutex::new(1),
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
        self.channel.send(&message.to_string());
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
}

impl Default for PushServer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_push_router(push_server: Arc<PushServer>) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .with_state(push_server)
}

pub async fn ws_handler_with_app_state(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state.push_server))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(push_server): State<Arc<PushServer>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, push_server))
}

async fn handle_socket(socket: WebSocket, push_server: Arc<PushServer>) {
    let mut rx = push_server.channel().subscribe();

    let (mut sender, mut _receiver) = socket.split();

    let client_id = push_server.register_client(push_server.channel().sender.clone());

    let result = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
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
}
