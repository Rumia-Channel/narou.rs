//! 外部 HTTP 取得リレー (`scripts/relay/` の Rust 実装)。
//!
//! Cloudflare Workers の取得が 403 になるサイト (ハーメルン等) を、別の出口 IP を
//! 持つホスト (VPS / 自宅回線) から中継取得するための小さな HTTP サービス。
//! プロトコルは従来の Node 版および `src/platform/relay.rs` (Worker 側クライアント)
//! と同一で、`/proxy` の JSON/base64 契約と `/health` を維持する。
//!
//! 構成:
//! - `server`: 認証・入力上限・期限・切断中断を持つ HTTP/1.1 サーバー
//! - `security`: 許可ホスト、DNS 検証、接続先の固定 (SSRF 防止)
//! - `transport`: 実取得 (システムの curl / wget。TLS とブラウザ指紋は各クライアントに任せる)
//! - `config`: 環境変数と `.env` の解決
//! - `ca`: 起動時の CA バンドル検証 (既知の侵害ルートを拒否)
//! - `wire`: JSON・ヘッダ・サイズ上限のコーデック

#![forbid(unsafe_code)]

pub mod ca;
pub mod config;
pub mod security;
pub mod server;
pub mod transport;
pub mod wire;
