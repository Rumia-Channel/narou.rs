//! `GET /ws` — Web UI 用 WebSocket エンドポイント。
//!
//! native (`src/web/push.rs`) の `PushServer` 相当の配信は `PushHub`
//! Durable Object (`worker_entry/src/push_hub.rs`) が担う。ここでは
//! 認証と upgrade 判定を行い、リクエストをそのままシングルトン DO の
//! stub へ転送するだけ。接続の受け入れ・履歴リプレイ・broadcast 配信は
//! 全て DO 側で行われる。

use serde_json::json;
use worker::{Env, Method, Request, Response, Result};

pub async fn handle(req: Request, env: Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return ws_error(405, "method_not_allowed", Some("expected GET"));
    }

    // ネイティブ (axum の WebSocketUpgrade extractor) は Upgrade ヘッダの無い
    // GET を 400 で拒否する。同じ扱いにする (DO 側でも再確認される)。
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

    let stub = crate::push_hub::hub_stub(&env)?;
    stub.fetch_with_request(req).await
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
