//! Web UI の操作系エンドポイントのうち、Cloudflare Workers では原理的に
//! 実現できないもの (native: `src/web/jobs.rs` / `src/web/feature_tour.rs`。
//! 小説本文の版履歴を要する `/api/diff*` も対象 — Worker の D1/R2 は本文を
//! 1 世代しか持たない)。
//!
//! `docs/cloudflare-workers-migration-plan.md` §3.3 に従い、成功を偽装せず
//! `501 Not Implemented` + 機械可読な `code` (`not_supported_on_worker`) を返す。
//! 応答ボディは `lib.rs::json_error` と同形の `{error: {code, message}}` で、
//! `message` は native ハンドラが返す日本語の失敗文に揃えてある
//! (フロントは非 2xx をそのまま通知に表示する)。
//!
//! 例外は `GET /api/storage/mode` だけで、こちらは実際のストア実態
//! (D1 = SQLite 系、プラットフォーム固定) を native のキーで返す。

use worker::{Env, Method, Request, Response};

use super::json_error;

/// Entry point; `lib.rs` が担当ルートをここへ振る。
pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    match (req.method(), req.path().as_str()) {
        // POST /api/shutdown — Worker はプロセスを持たず終了させる対象が無い。
        (Method::Post, "/api/shutdown") => {
            not_supported("この Worker 環境ではシャットダウンは利用できません")
        }
        // POST /api/reboot — 同じく再起動するプロセス自体が存在しない。
        (Method::Post, "/api/reboot") => {
            not_supported("この Worker 環境では再起動は利用できません")
        }
        // POST /api/folder — ローカルファイルマネージャを開く操作で、
        // Worker には端末の FS もデスクトップも無い。
        (Method::Post, "/api/folder") => {
            not_supported("この Worker 環境ではフォルダを開けません")
        }
        // POST /api/backup — native は作品ディレクトリをローカル FS 上で
        // バックアップする。Worker の小説データは D1/R2 にあり、対象の
        // ディレクトリ構造が存在しない。
        (Method::Post, "/api/backup") => {
            not_supported("この Worker 環境ではバックアップは利用できません")
        }
        // POST /api/backup_bookmark — native は `send --backup-bookmark` を
        // 走らせ端末からしおりを回収する。端末送信経路 (メール/Kindle) が
        // Worker には無い。
        (Method::Post, "/api/backup_bookmark") => {
            not_supported("この Worker 環境ではしおりバックアップは利用できません")
        }
        // POST /api/setting_burn — native は各小説の `setting.ini` を
        // ローカル FS に書き換える。Worker では設定は D1 `app_state` にあり
        // 焼き込み先の ini ファイルが存在しない。
        (Method::Post, "/api/setting_burn") => {
            not_supported("この Worker 環境では設定の焼き込みは利用できません")
        }
        // GET /api/csv/download — native は小説一覧をローカル FS から CSV に
        // 起こす。Worker 側に CSV エクスポート経路は未実装 (D1 直読みの
        // エクスポートは別機能として要実装)。
        (Method::Get, "/api/csv/download") => {
            not_supported("この Worker 環境では CSV エクスポートは利用できません")
        }
        // POST /api/csv/import — native は CSV を解析してローカルライブラリへ
        // 流し込む。Worker 側に CSV インポート経路は未実装。
        (Method::Post, "/api/csv/import") => {
            not_supported("この Worker 環境では CSV インポートは利用できません")
        }
        // POST /api/mail — SMTP / メール送信は Worker から行えない
        // (§3.3: send/mail(SMTP) は 501)。
        (Method::Post, "/api/mail") => {
            not_supported("この Worker 環境ではメール送信は利用できません")
        }
        // POST /api/send — 端末 (Kindle 等) へのファイル送信はメール経路前提
        // で、Worker には送信手段が無い (§3.3)。
        (Method::Post, "/api/send") => {
            not_supported("この Worker 環境では端末への送信は利用できません")
        }
        // GET /api/storage/mode — Worker の管理ストアは D1 (SQLite 系) で
        // 固定。native の `mode`/`locked_by_env` キーで実態を返す
        // (`locked_by_env`: プラットフォームがモードを固定している = true)。
        (Method::Get, "/api/storage/mode") => storage_mode(&req, &env).await,
        // POST /api/storage/mode — 管理方式の切替はローカル FS の
        // `.narou` マーカー書き換えと DB 再初期化を伴う。Worker では
        // ストアは D1 に固定で切替先が存在しない。
        (Method::Post, "/api/storage/mode") => {
            not_supported("この Worker 環境では管理方式は D1 に固定です")
        }
        // POST /api/diff — native は保存済みの版履歴 (novel_versions) と
        // 作業セットの差分を取る。Worker の D1/R2 には版履歴が存在しない
        // (本文は 1 世代だけ保持)。成功を偽装せず 501。
        (Method::Post, "/api/diff") => {
            not_supported("この Worker 環境では小説本文のバージョン差分は利用できません")
        }
        // GET /api/diff_history — 版履歴そのものが無い。
        (Method::Get, "/api/diff_history") => {
            not_supported("この Worker 環境ではバージョン履歴は利用できません")
        }
        // GET /api/diff_show — 保存済みの unified diff (履歴) が無い。
        (Method::Get, "/api/diff_show") => {
            not_supported("この Worker 環境では保存済み差分の表示は利用できません")
        }
        // POST /api/diff_restore — 過去バージョンへの復元元が無い。
        (Method::Post, "/api/diff_restore") => {
            not_supported("この Worker 環境ではバージョンへの復元は利用できません")
        }
        // POST /api/diff_merge — マージ元のバージョンが無い。
        (Method::Post, "/api/diff_merge") => {
            not_supported("この Worker 環境ではバージョンのマージは利用できません")
        }
        // 担当パスだがメソッドが違う場合は native と同じく 405。
        (
            _,
            "/api/shutdown"
            | "/api/reboot"
            | "/api/folder"
            | "/api/backup"
            | "/api/backup_bookmark"
            | "/api/setting_burn"
            | "/api/csv/download"
            | "/api/csv/import"
            | "/api/mail"
            | "/api/send"
            | "/api/storage/mode"
            | "/api/diff"
            | "/api/diff_history"
            | "/api/diff_show"
            | "/api/diff_restore"
            | "/api/diff_merge",
        ) => json_error(405, "method_not_allowed", None),
        _ => json_error(404, "not_found", Some("route is not handled by this Worker")),
    }
}

/// 501 + `not_supported_on_worker`。§3.3 の「明示的に拒否する」要件。
fn not_supported(message: &str) -> worker::Result<Response> {
    json_error(501, "not_supported_on_worker", Some(message))
}

/// Report the selected stores without exposing configuration or object names.
/// The optional authenticated probe performs a bounded, read-only S3 LIST.
async fn storage_mode(req: &Request, env: &Env) -> worker::Result<Response> {
    let probe = req.url()?.query_pairs().any(|(key, value)| key == "probe" && value == "s3");
    let required = match crate::composition::required_s3(env) {
        Ok(required) => required,
        Err(_) => return json_error(503, "storage_configuration_invalid", None),
    };
    let services = match crate::composition::build_read_services(env).await {
        Ok(services) => services,
        Err(_) => return json_error(503, "storage_configuration_unavailable", None),
    };
    let s3 = services.s3_illustrations.as_deref()
        .map(|store| store as &dyn narou_rs::platform::ObjectStore);
    let (status, payload) = crate::storage_probe::storage_mode_response(s3, required, probe).await;
    Ok(Response::from_json(&payload)?.with_status(status))
}
