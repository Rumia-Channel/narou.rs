//! Web UI の API のうち、native (`src/web/*.rs`) から Worker へ移植したもの。
//!
//! 各モジュールは `pub async fn handle(req, env) -> worker::Result<Response>` を
//! 公開し、`worker_entry/src/lib.rs` のルート表から呼ばれる。JSON の形・キー名・
//! ステータスコードは native 実装と一致させる（フロントエンドを共有するため）。
//! Worker で実現できない操作（ローカル FS 前提など）は、成功を偽装せず 501 と
//! 機械可読な code を返す。

pub mod download;
pub mod job_actions;
pub mod library_backup;
pub mod read_views;
pub mod login_actions;
pub mod queue_actions;
pub mod row_actions;
pub mod tag_actions;
pub mod native_only;
pub mod list;
pub mod ui_prefs;
pub mod queue;
