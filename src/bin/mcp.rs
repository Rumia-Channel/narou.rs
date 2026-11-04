//! `narou_rs_mcp` — 外部公開 API と同じ MCP サーバーを stdIO で動かす。
//!
//! MCP (Model Context Protocol) のクライアント (Claude Desktop など) から
//! `narou_rs_mcp` を起動すると、ライブラリの読み取り専用 API をそのまま
//! 工具として使える。外部公開 API (`/api/v1`) と同じ条件・同じ出力形式
//! (JSON / JSON Lines / XML) を返す。
//!
//! 通信は 1 行 1 JSON の JSON-RPC 2.0 (stdIO 伝送)。応答が要らない通知
//! (id なし) には答えない。
//!
//! Usage:
//! ```text
//! narou_rs_mcp [--root <LIBRARY_DIR>]
//! ```
//! - `--root`: `.narou` を持つライブラリフォルダ。省略時はカレント
//!   ディレクトリから親方向へ `.narou` を探す。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value;

use narou_rs::application::mcp::McpServer;
use narou_rs::db::inventory::Inventory;
use narou_rs::native::application::NativeAppServices;

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("Error: {message}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("Error: {message}");
            ExitCode::from(1)
        }
    }
}

/// 走査する JSON-RPC の要求。
struct Args {
    root: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut root = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--root にはパスを指定してください".to_string())?;
                root = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                println!("narou_rs_mcp [--root <LIBRARY_DIR>]");
                println!("  外部公開 API と同じ MCP サーバーを stdIO で起動します。");
                std::process::exit(0);
            }
            other => return Err(format!("未知の引数です: {other}")),
        }
    }
    Ok(Args { root })
}

fn run(args: Args) -> Result<(), String> {
    // ライブラリの管理データ (SQLite / YAML) を先に開く。
    narou_rs::db::init_database().map_err(|error| error.to_string())?;
    let inventory = match &args.root {
        Some(root) => Inventory::new(root.clone()),
        None => Inventory::with_default_root().map_err(|error| {
            format!("{error}\n(ライブラリフォルダは --root で指定できます)")
        })?,
    };
    let services = NativeAppServices::new(std::sync::Arc::new(inventory))
        .map_err(|error| error.to_string())?;
    let server = McpServer::new(services.services.external_api.clone());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut reader = stdin.lock();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                // JSON を解釈できない要求は JSON-RPC のエラーとして返す。
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("JSON を解釈できません: {error}") }
                });
                writeln!(stdout, "{response}").map_err(|error| error.to_string())?;
                stdout.flush().map_err(|error| error.to_string())?;
                continue;
            }
        };
        let hidden = narou_rs::native::hidden::load().ok();
        let response = runtime.block_on(server.handle(&request, hidden));
        if let Some(response) = response {
            writeln!(stdout, "{response}").map_err(|error| error.to_string())?;
            stdout.flush().map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
