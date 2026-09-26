//! `GET/POST /api/library_backup`。
//!
//! native (`src/web/library_backup.rs`) はローカル FS の `.narou/last-run-version`
//! marker と容量情報から「0.4.0 未満→以降へのアップデート直後にバックアップを
//! 提案するか」を返し、POST は `narou_rs_backup` サブプロセスで zip を作る。
//!
//! Worker には対象のファイルシステム (小説データ/ + `.narou/`) が存在しない。
//! メタデータ・本文は D1、挿絵は S3 にあるため「ライブラリフォルダ全体の zip」
//! という概念自体が成り立たない。よって:
//!
//! - GET は提案無し (`pending: false`) と台帳 (`worker_jobs`) 上の backup 実行数
//!   を native と同じキー形で返す。
//! - POST は zip をローカル FS へ書けないので、成功を偽装せず 501 を返す
//!   (dismiss も同様。記録すべき marker が Worker 側に存在しない)。

use worker::{Env, Method, Request, Response, console_log, wasm_bindgen::JsValue};

use crate::composition::WorkerRuntime;

/// 状態取得とバックアップ作成/提案の却下。
pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    match req.method() {
        Method::Get => status(&env).await,
        Method::Post => not_supported(),
        _ => Response::error("Method Not Allowed", 405),
    }
}

/// `GET /api/library_backup` — 提案が必要かと実行状態を返す。
///
/// native は `pending: false` 時に `total_bytes` / `free_bytes` / `enough_space` /
/// `default_output` を出さないので、こちらも同じキー形に揃える。`running` は
/// `worker_jobs` の `kind='backup'` で `status='running'` の行数から出す
/// (Worker の dispatch は backup を常に `blocked` へ落とすので通常 0)。
async fn status(env: &Env) -> worker::Result<Response> {
    // コンポジション全体を組み立てるのは、欠けた binding を native 同様に
    // 503 で報告するため (D1 だけの軽い経路は作らない)。
    let _runtime = match WorkerRuntime::build(env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    let running = match running_backups(env).await {
        Ok(count) => count > 0,
        Err(error) => {
            console_log!("library_backup status query failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    Response::from_json(&serde_json::json!({
        "pending": false,
        "running": running,
    }))
}

/// 台帳上で実行中の backup ジョブ数。
async fn running_backups(env: &Env) -> worker::Result<i64> {
    let db = env.d1("DB")?;
    let statement = db
        .prepare("SELECT COUNT(*) AS n FROM worker_jobs WHERE kind = ? AND status = 'running'")
        .bind(&[JsValue::from_str("backup")])?;
    let count = statement.first::<i64>(Some("n")).await?;
    Ok(count.unwrap_or(0))
}

/// `POST /api/library_backup` — zip をローカル FS へ書けないので常に 501。
///
/// `worker_entry::webui::json_error` の形
/// (`{"error": {"code": ..., "message": ...}}`) で返す。
fn not_supported() -> worker::Result<Response> {
    super::json_error(
        501,
        "library_backup_not_supported",
        Some("library backup writes a zip to the local filesystem, which does not exist on the worker; run it from the native UI or narou_rs_backup"),
    )
}
