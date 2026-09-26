//! `GET /ws` — Web UI 用 WebSocket エンドポイントの最小実装。
//!
//! ネイティブ版 (`src/web/push.rs`) では `PushServer` がジョブの標準出力・
//! キュー状態・テーブル再読込などのイベントを全クライアントへ broadcast する。
//! Worker 版にはそれに相当する共有ブロードキャスト層 (Durable Object 等) が
//! まだ無いため、ここでは **接続の受理だけ** を行う。
//!
//! この制約:
//! - サーバーからのプッシュイベントは一切送らない (送る手段が無い)。
//! - クライアントからのメッセージは全て読み捨てる。drain しないと受信
//!   バッファが溢れて接続が切られるため、イベントストリームは回し続ける。
//!
//! 接続時に `{"type":"hello","data":{}}` を 1 通だけ送る。フロントエンドの
//! `handleWsMessage` (`src/web/assets/js/main.js`) は未知の `type` を無視する
//! ので UI 状態には影響しない。ネイティブのイベント名 ("status" 等) を名乗ると
//! 再読込が発火してしまうため、独自名にしている。
//!
//! フロントエンドは `onclose` で 5 秒ごとに再接続するため、
//! むやみに切断しないことが重要。

use futures::StreamExt;
use serde_json::json;
use worker::ws_events::WebsocketEvent;
use worker::{Env, Method, Request, Response, Result, WebSocketPair, console_log, console_warn};

/// 接続直後に送るハンドシェイク。UI を動かさない在席確認用。
const HELLO_MESSAGE: &str = r#"{"type":"hello","data":{}}"#;

pub async fn handle(req: Request, env: Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return ws_error(405, "method_not_allowed", Some("expected GET"));
    }

    // ネイティブ (axum の WebSocketUpgrade extractor) は Upgrade ヘッダの無い
    // GET を 400 で拒否する。同じ扱いにする。
    let upgrade = req.headers().get("upgrade")?;
    if upgrade
        .as_deref()
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
    {
        return ws_error(
            400,
            "upgrade_required",
            Some("expected a WebSocket upgrade request"),
        );
    }

    let pair = WebSocketPair::new()?;
    let server = pair.server;
    server.accept()?;

    if let Err(error) = server.send_with_str(HELLO_MESSAGE) {
        // ハンドシェイクを送れない接続は生きていない。閉じてクライアントに
        // 再試行させる (送れないまま開きっぱなしにしない)。
        console_warn!("websocket: failed to send handshake: {error}");
        let _ = server.close(Some(1011), Some("internal error"));
        return Response::from_websocket(pair.client);
    }

    // 受信イベントを drain し続けるタスク。push するイベントは無いので
    // 届いたものは全て読み捨てる。close / 切断でタスクは終了する。
    wasm_bindgen_futures::spawn_local(async move {
        match server.events() {
            Ok(mut events) => {
                while let Some(event) = events.next().await {
                    match event {
                        Ok(WebsocketEvent::Message(_)) => {
                            // 読み捨て。クライアントからの入力は使わない。
                        }
                        Ok(WebsocketEvent::Close(event)) => {
                            console_log!(
                                "websocket closed: code={} reason={}",
                                event.code(),
                                event.reason()
                            );
                            break;
                        }
                        Err(error) => {
                            console_warn!("websocket event error: {error}");
                        }
                    }
                }
            }
            Err(error) => {
                console_warn!("websocket: failed to subscribe to events: {error}");
            }
        }
    });

    Response::from_websocket(pair.client)
}

/// `lib.rs` の `json_error` と同じ `{error:{code,message?}}` 形を組み立てる
/// (lib.rs 側は private なのでここで用意する)。
fn ws_error(status: u16, code: &str, message: Option<&str>) -> Result<Response> {
    let payload = match message {
        Some(message) => json!({ "error": { "code": code, "message": message } }),
        None => json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}
