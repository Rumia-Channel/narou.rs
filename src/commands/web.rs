use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use narou_rs::db::inventory::{Inventory, InventoryScope};
use serde_yaml::{Number, Value};
use tracing::info;

#[derive(Debug, Clone)]
struct WebAddress {
    host: String,
    port: u16,
    ws_port: u16,
}

pub async fn run_web_server(port: Option<u16>, no_browser: bool) {
    use narou_rs::web;

    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let address = match resolve_web_address(port) {
        Ok(address) => address,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    let _ = confirm_first_web_boot(no_browser);

    info!(
        "Starting narou.rs web server on {}:{} (ws:{})",
        address.host, address.port, address.ws_port
    );

    let push_server = Arc::new(web::push::PushServer::new());
    let basic_auth_header = match load_basic_auth_header() {
        Ok(header) => header,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    let app_state = web::AppState {
        port: address.port,
        ws_port: address.ws_port,
        push_server: push_server.clone(),
        basic_auth_header,
    };
    let app = web::create_router(app_state.clone());
    let ws_app = web::push::create_push_router(app_state);

    let addr: SocketAddr = format!("{}:{}", address.host, address.port)
        .parse()
        .unwrap();
    let ws_addr: SocketAddr = format!("{}:{}", address.host, address.ws_port)
        .parse()
        .unwrap();
    let url = format!("http://{}:{}/", display_host(&address.host), address.port);
    println!("{}", url);
    println!("サーバを止めるには Ctrl+C を入力");
    println!();

    if !no_browser {
        let _ = open::that(&url);
    }

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let ws_listener = tokio::net::TcpListener::bind(ws_addr).await.unwrap();

    let ws_task = tokio::spawn(async move { axum::serve(ws_listener, ws_app).await });
    axum::serve(listener, app).await.unwrap();
    ws_task.abort();
}

fn resolve_web_address(user_port: Option<u16>) -> Result<WebAddress, String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let mut global_setting: HashMap<String, Value> = inventory
        .load("global_setting", InventoryScope::Global)
        .unwrap_or_default();
    let host = normalize_bind_host(yaml_string(global_setting.get("server-bind")));
    let port = if let Some(port) = user_port {
        port
    } else if let Some(port) = yaml_u16(global_setting.get("server-port")) {
        port
    } else {
        let port = find_available_web_port(&host)?;
        global_setting.insert("server-port".to_string(), Value::Number(Number::from(port)));
        inventory
            .save("global_setting", InventoryScope::Global, &global_setting)
            .map_err(|e| e.to_string())?;
        port
    };
    let ws_port = port
        .checked_add(1)
        .ok_or_else(|| "server-port + 1 が不正な値になります".to_string())?;
    Ok(WebAddress {
        host,
        port,
        ws_port,
    })
}

fn load_basic_auth_header() -> Result<Option<String>, String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let global_setting: HashMap<String, Value> = inventory
        .load("global_setting", InventoryScope::Global)
        .unwrap_or_default();
    let enabled = yaml_bool(global_setting.get("server-basic-auth.enable")).unwrap_or(false);
    if !enabled {
        return Ok(None);
    }
    let user = yaml_string(global_setting.get("server-basic-auth.user")).unwrap_or_default();
    let password =
        yaml_string(global_setting.get("server-basic-auth.password")).unwrap_or_default();
    if user.is_empty() || password.is_empty() {
        return Ok(None);
    }
    let token = encode_base64(format!("{}:{}", user, password).as_bytes());
    Ok(Some(format!("Basic {}", token)))
}

fn confirm_first_web_boot(no_browser: bool) -> Result<bool, String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let mut server_setting: HashMap<String, Value> = inventory
        .load("server_setting", InventoryScope::Global)
        .unwrap_or_default();
    let is_first = !yaml_bool(server_setting.get("already-server-boot")).unwrap_or(false);
    if !is_first {
        return Ok(false);
    }

    println!(
        "初めてサーバを起動します。ファイアウォールのアクセス許可を尋ねられた場合、許可をして下さい。"
    );
    println!("また、起動したサーバを止めるにはコンソール上で Ctrl+C を入力して下さい。");
    println!();
    if io::stdin().is_terminal() {
        if no_browser {
            println!("(何かキーを押して下さい)");
        } else {
            println!("(何かキーを押して下さい。サーバ起動後ブラウザが立ち上がります)");
        }
        let mut buffer = String::new();
        let _ = io::stdin().read_line(&mut buffer);
    }

    server_setting.insert("already-server-boot".to_string(), Value::Bool(true));
    inventory
        .save("server_setting", InventoryScope::Global, &server_setting)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

fn find_available_web_port(host: &str) -> Result<u16, String> {
    let range_start = 4000u16;
    let range_len = 61000u16;
    let seed = chrono::Utc::now().timestamp_subsec_nanos() as u16;
    for offset in 0..range_len {
        let port = range_start + ((seed.wrapping_add(offset)) % range_len);
        if port == u16::MAX {
            continue;
        }
        if can_bind(host, port) && can_bind(host, port + 1) {
            return Ok(port);
        }
    }
    Err("使用可能な server-port を確保できませんでした".to_string())
}

fn can_bind(host: &str, port: u16) -> bool {
    TcpListener::bind((host, port)).is_ok()
}

fn normalize_bind_host(bind: Option<String>) -> String {
    match bind.as_deref() {
        Some("localhost") => "127.0.0.1".to_string(),
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => "127.0.0.1".to_string(),
    }
}

fn display_host(host: &str) -> &str {
    if host == "127.0.0.1" {
        "localhost"
    } else {
        host
    }
}

fn yaml_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => Some(matches!(s.as_str(), "true" | "yes" | "on" | "1")),
        Some(Value::Number(n)) => Some(n.as_i64().unwrap_or(0) != 0),
        _ => None,
    }
}

fn yaml_u16(value: Option<&Value>) -> Option<u16> {
    match value {
        Some(Value::Number(n)) => n.as_u64().and_then(|v| u16::try_from(v).ok()),
        Some(Value::String(s)) => s.parse::<u16>().ok(),
        _ => None,
    }
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut index = 0usize;
    while index < input.len() {
        let b0 = input[index];
        let b1 = input.get(index + 1).copied().unwrap_or(0);
        let b2 = input.get(index + 2).copied().unwrap_or(0);
        let combined = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);

        out.push(TABLE[((combined >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((combined >> 12) & 0x3f) as usize] as char);
        if index + 1 < input.len() {
            out.push(TABLE[((combined >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if index + 2 < input.len() {
            out.push(TABLE[(combined & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }

        index += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{display_host, encode_base64, normalize_bind_host};

    #[test]
    fn normalize_bind_host_defaults_to_loopback() {
        assert_eq!(normalize_bind_host(None), "127.0.0.1");
        assert_eq!(
            normalize_bind_host(Some("localhost".to_string())),
            "127.0.0.1"
        );
    }

    #[test]
    fn display_host_prefers_localhost_alias() {
        assert_eq!(display_host("127.0.0.1"), "localhost");
        assert_eq!(display_host("0.0.0.0"), "0.0.0.0");
    }

    #[test]
    fn encode_base64_matches_basic_examples() {
        assert_eq!(encode_base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(encode_base64(b"ab"), "YWI=");
    }
}
