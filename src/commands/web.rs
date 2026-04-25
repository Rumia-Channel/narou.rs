use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use narou_rs::db::inventory::{Inventory, InventoryScope};
use serde_yaml::{Number, Value};
use tracing::info;

#[cfg(windows)]
#[path = "web_tray.rs"]
mod web_tray;

#[derive(Debug, Clone)]
struct WebAddress {
    host: String,
    port: u16,
    ws_port: u16,
}

pub async fn run_web_server(port: Option<u16>, no_browser: bool, hide_console: bool) {
    use narou_rs::web;

    #[cfg(not(windows))]
    if hide_console {
        eprintln!("warning: --hide-console は現在 Windows のみ対応です。通常モードで起動します。");
    }

    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = fill_general_all_no_in_database() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }

    let address = match resolve_web_address(port) {
        Ok(address) => address,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    let _ = confirm_first_web_boot(no_browser, hide_console);

    info!(
        "Starting narou.rs web server on {}:{} (ws:{})",
        address.host, address.port, address.ws_port
    );

    let security_settings = match load_web_security_settings() {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    let mut push_server = web::push::PushServer::new();
    let domains =
        match load_ws_accepted_domains(&address.host, security_settings.reverse_proxy_mode) {
            Ok(domains) => domains,
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };
    push_server.set_accepted_domains(domains);
    let push_server = Arc::new(push_server);
    if requires_basic_auth_for_bind(&address.host)
        && security_settings.require_basic_auth_for_external_bind
        && security_settings.basic_auth_header.is_none()
    {
        eprintln!(
            "Error: server-bind が外部公開設定のため、server-basic-auth を有効にして user/password を設定して下さい"
        );
        std::process::exit(1);
    }
    let control_token = generate_control_token();
    let root_dir = match Inventory::with_default_root() {
        Ok(inventory) => inventory.root_dir().to_path_buf(),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    let queue =
        match narou_rs::queue::PersistentQueue::new(&root_dir.join(".narou").join("queue.yaml")) {
            Ok(queue) => Arc::new(queue),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };
    let restorable_tasks_available = Arc::new(AtomicBool::new(queue.has_restorable_tasks()));
    let restore_prompt_pending = Arc::new(AtomicBool::new(queue.restore_prompt_pending()));
    let running_jobs = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let running_child_pids = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let cancelled_job_ids = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
    let auto_update_scheduler = Arc::new(parking_lot::Mutex::new(None));
    let app_state = web::AppState {
        port: address.port,
        ws_port: address.ws_port,
        push_server: push_server.clone(),
        basic_auth_header: security_settings.basic_auth_header,
        control_token: control_token.clone(),
        allowed_request_hosts: narou_rs::web::default_allowed_request_hosts(&address.host),
        reverse_proxy_mode: security_settings.reverse_proxy_mode,
        queue: queue.clone(),
        restore_prompt_pending: restore_prompt_pending.clone(),
        restorable_tasks_available: restorable_tasks_available.clone(),
        running_jobs: running_jobs.clone(),
        running_child_pids: running_child_pids.clone(),
        cancelled_job_ids: cancelled_job_ids.clone(),
        auto_update_scheduler: auto_update_scheduler.clone(),
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
    if !hide_console {
        println!("{}", url);
        println!("サーバを止めるには {}", web_stop_hint(hide_console));
        println!();
    }

    if !no_browser {
        let _ = open::that(&url);
    }

    let listener = bind_or_shutdown_and_retry(addr, &address.host, address.port, "HTTP").await;
    let ws_listener =
        bind_or_shutdown_and_retry(ws_addr, &address.host, address.ws_port, "WebSocket").await;

    // Write PID file for restart recovery
    write_pid_file(&control_token);

    #[cfg(windows)]
    if hide_console {
        web_tray::spawn_web_tray(
            control_request_host(&address.host).to_string(),
            address.port,
            control_token.clone(),
        );
    }

    let worker_tasks = web::worker::start_queue_workers(
        root_dir.clone(),
        queue.clone(),
        push_server.clone(),
        running_jobs.clone(),
        running_child_pids,
        cancelled_job_ids,
        narou_rs::compat::load_local_setting_bool("concurrency"),
    );
    web::scheduler::start_or_restart_auto_update_scheduler(
        queue,
        running_jobs,
        push_server.clone(),
        &auto_update_scheduler,
    );

    // Ruby parity: broadcast startup messages to web console
    {
        use narou_rs::termcolor::colored;
        let ver = narou_rs::version::create_version_string();
        push_server.broadcast_echo(
            &colored(&format!("Narou.rs version {}", ver), "white"),
            "stdout",
        );

        if let Ok(queue) = narou_rs::queue::PersistentQueue::with_default() {
            let count = queue.pending_count() + queue.running_count();
            if count > 0 {
                push_server.broadcast_echo(
                    &colored(
                        &format!(
                            "前回未完了のタスクが{}件見つかりました。WEB UI から再開できます。",
                            count
                        ),
                        "yellow",
                    ),
                    "stdout",
                );
            }
        }
    }

    // Graceful shutdown on Ctrl+C so the ports are properly released
    let shutdown_signal = async {
        tokio::signal::ctrl_c().await.ok();
        eprintln!();
    };

    let ws_task = tokio::spawn(async move { axum::serve(ws_listener, ws_app).await });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .unwrap();
    for worker_task in worker_tasks {
        worker_task.abort();
    }
    web::scheduler::stop_auto_update_scheduler(&auto_update_scheduler);
    ws_task.abort();
    remove_pid_file();
}

fn fill_general_all_no_in_database() -> Result<(), String> {
    narou_rs::db::with_database_mut(|db| {
        let archive_root = db.archive_root().to_path_buf();
        let ids: Vec<i64> = db
            .all_records()
            .values()
            .filter(|record| record.general_all_no.is_none())
            .map(|record| record.id)
            .collect();
        let mut modified = false;

        for id in ids {
            let Some(record) = db.get(id).cloned() else {
                continue;
            };
            let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
            let Some(toc) = narou_rs::downloader::persistence::load_toc_file(&novel_dir) else {
                continue;
            };
            let Some(target) = db.all_records_mut().get_mut(&id) else {
                continue;
            };
            target.general_all_no = Some(toc.subtitles.len() as i64);
            modified = true;
        }

        if modified {
            db.save()?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
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

async fn bind_or_shutdown_and_retry(
    addr: SocketAddr,
    host: &str,
    port: u16,
    label: &str,
) -> tokio::net::TcpListener {
    // First attempt: bind with SO_REUSEADDR (matching Ruby/WEBrick behavior).
    // On Windows this succeeds even when the old process still holds the port.
    // On Unix this handles TIME_WAIT but NOT an active listener.
    match create_reusable_listener(addr) {
        Ok(l) => return l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // Active listener exists (Unix) — try cleanup
            try_shutdown_via_http(host, port);
            try_kill_via_pid_file(port);
        }
        Err(e) => {
            eprintln!("{} サーバの起動に失敗しました: {}", label, e);
            std::process::exit(1);
        }
    }

    // Retry after cleanup
    match create_reusable_listener(addr) {
        Ok(l) => l,
        Err(_) => {
            eprintln!(
                "ポート {} は既に使用されています。\n\
                 既にサーバが起動していませんか？\n\
                 別のポートを指定するには --port オプションを使ってください。",
                port
            );
            std::process::exit(1);
        }
    }
}

/// Create a TCP listener with SO_REUSEADDR set, matching Ruby/WEBrick behaviour.
/// This allows rebinding a port that is in TIME_WAIT (all platforms) or still
/// held by a lingering old process (Windows).
fn create_reusable_listener(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

// --- PID file management ---

fn pid_file_path() -> Option<std::path::PathBuf> {
    Some(
        std::env::current_dir()
            .ok()?
            .join(".narou")
            .join("server.pid"),
    )
}

fn write_pid_file(control_token: &str) {
    if let Some(path) = pid_file_path() {
        let _ = std::fs::write(&path, format!("{} {}", std::process::id(), control_token));
    }
}

fn remove_pid_file() {
    if let Some(path) = pid_file_path() {
        let _ = std::fs::remove_file(&path);
    }
}

fn read_pid_file() -> Option<(u32, Option<String>)> {
    let path = pid_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut parts = content.split_whitespace();
    let pid = parts.next()?.trim().parse().ok()?;
    let token = parts.next().map(ToString::to_string);
    Some((pid, token))
}

// --- Shutdown strategies ---

/// Best-effort HTTP shutdown of a running narou server.
/// May fail if the server is in graceful-shutdown mode (Ctrl+C already pressed).
fn try_shutdown_via_http(host: &str, port: u16) {
    use std::net::TcpStream;

    let request_host = control_request_host(host);
    let control_token = read_pid_file().and_then(|(_, token)| token);

    eprintln!(
        "ポート {} で稼働中のサーバへシャットダウンを要求しています...",
        port
    );

    if !send_control_request(host, port, control_token.as_deref(), "/api/shutdown") {
        return;
    }

    // Brief wait for the old server to exit
    let addr: SocketAddr = match format!("{}:{}", request_host, port).parse() {
        Ok(a) => a,
        Err(_) => return,
    };
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(250));
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err() {
            return;
        }
    }
}

fn control_request_host(host: &str) -> &str {
    match host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        _ => host,
    }
}

pub(crate) fn send_control_request(
    host: &str,
    port: u16,
    control_token: Option<&str>,
    endpoint: &str,
) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let request_host = control_request_host(host);
    let display = if request_host == "127.0.0.1" {
        "localhost"
    } else {
        request_host
    };
    let addr: SocketAddr = match format!("{}:{}", request_host, port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };

    let control_header = control_token
        .map(|token| format!("{}: {}\r\n", narou_rs::web::INTERNAL_CONTROL_HEADER, token))
        .unwrap_or_default();
    let request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         {}\
         Content-Length: 2\r\n\
         Connection: close\r\n\r\n\
         {{}}",
        endpoint, display, port, control_header
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 512];
    let _ = stream.read(&mut buf);
    true
}

/// Best-effort kill of the old server process via PID file.
fn try_kill_via_pid_file(port: u16) {
    let Some((pid, _)) = read_pid_file() else {
        return;
    };

    if pid == std::process::id() {
        return;
    }

    eprintln!(
        "PIDファイルから前回のサーバプロセス (PID: {}) を終了しています...",
        pid
    );

    let _ = narou_rs::compat::terminate_process(pid);

    // Wait for the process to die and port to free, regardless of kill exit code
    let addr: SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return,
    };
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(250));
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err() {
            return;
        }
    }
}

struct WebSecuritySettings {
    basic_auth_header: Option<String>,
    require_basic_auth_for_external_bind: bool,
    reverse_proxy_mode: bool,
}

fn load_web_security_settings() -> Result<WebSecuritySettings, String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let global_setting: HashMap<String, Value> = inventory
        .load("global_setting", InventoryScope::Global)
        .unwrap_or_default();
    Ok(WebSecuritySettings {
        basic_auth_header: basic_auth_header_from_settings(&global_setting),
        require_basic_auth_for_external_bind: require_basic_auth_for_external_bind_from_settings(
            &global_setting,
        ),
        reverse_proxy_mode: reverse_proxy_mode_from_settings(&global_setting),
    })
}

fn basic_auth_header_from_settings(global_setting: &HashMap<String, Value>) -> Option<String> {
    let enabled = yaml_bool(global_setting.get("server-basic-auth.enable")).unwrap_or(false);
    if !enabled {
        return None;
    }
    let user = yaml_string(global_setting.get("server-basic-auth.user")).unwrap_or_default();
    let password =
        yaml_string(global_setting.get("server-basic-auth.password")).unwrap_or_default();
    if user.is_empty() || password.is_empty() {
        return None;
    }
    let token = encode_base64(format!("{}:{}", user, password).as_bytes());
    Some(format!("Basic {}", token))
}

fn require_basic_auth_for_external_bind_from_settings(
    global_setting: &HashMap<String, Value>,
) -> bool {
    yaml_bool(global_setting.get("server-basic-auth.require-for-external-bind")).unwrap_or(true)
}

fn reverse_proxy_mode_from_settings(global_setting: &HashMap<String, Value>) -> bool {
    yaml_bool(global_setting.get("server-reverse-proxy.enable")).unwrap_or(false)
}

#[cfg(test)]
fn is_wildcard_bind_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::")
}

fn requires_basic_auth_for_bind(host: &str) -> bool {
    !matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn default_ws_accepted_domains(host: &str) -> Vec<String> {
    narou_rs::web::default_allowed_request_hosts(host)
}

fn load_ws_accepted_domains(host: &str, reverse_proxy_mode: bool) -> Result<Vec<String>, String> {
    if reverse_proxy_mode {
        return Ok(Vec::new());
    }
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let global_setting: HashMap<String, Value> = inventory
        .load("global_setting", InventoryScope::Global)
        .unwrap_or_default();
    let mut accepted_domains = default_ws_accepted_domains(host);
    if let Some(extra) = yaml_string(global_setting.get("server-ws-add-accepted-domains")) {
        accepted_domains.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|domain| !domain.is_empty())
                .map(ToString::to_string),
        );
    }
    Ok(accepted_domains)
}

fn generate_control_token() -> String {
    let mut token = [0u8; 16];
    getrandom::fill(&mut token).expect("failed to generate control token");
    hex::encode(token)
}

fn confirm_first_web_boot(no_browser: bool, hide_console: bool) -> Result<bool, String> {
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
    println!(
        "また、起動したサーバを止めるには {}。",
        web_stop_hint(hide_console)
    );
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
    std::net::TcpListener::bind((host, port)).is_ok()
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

fn web_stop_hint(hide_console: bool) -> &'static str {
    #[cfg(windows)]
    if hide_console {
        return "タスクトレイのアイコンを右クリックして「終了」または「再起動」を選んで下さい";
    }

    #[cfg(not(windows))]
    let _ = hide_console;

    "コンソール上で Ctrl+C を入力して下さい"
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
    use std::collections::HashMap;

    use serde_yaml::Value;

    use super::{
        basic_auth_header_from_settings, control_request_host, default_ws_accepted_domains,
        display_host, encode_base64, generate_control_token, is_wildcard_bind_host,
        normalize_bind_host, require_basic_auth_for_external_bind_from_settings,
        requires_basic_auth_for_bind, reverse_proxy_mode_from_settings,
    };

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

    #[test]
    fn wildcard_bind_hosts_require_explicit_auth() {
        assert!(is_wildcard_bind_host("0.0.0.0"));
        assert!(requires_basic_auth_for_bind("0.0.0.0"));
        assert!(!requires_basic_auth_for_bind("127.0.0.1"));
    }

    #[test]
    fn external_bind_auth_guard_defaults_to_enabled() {
        let settings = HashMap::new();
        assert!(require_basic_auth_for_external_bind_from_settings(
            &settings
        ));
    }

    #[test]
    fn external_bind_auth_guard_can_be_disabled() {
        let mut settings = HashMap::new();
        settings.insert(
            "server-basic-auth.require-for-external-bind".to_string(),
            Value::Bool(false),
        );
        assert!(!require_basic_auth_for_external_bind_from_settings(
            &settings
        ));
    }

    #[test]
    fn reverse_proxy_mode_defaults_to_disabled() {
        let settings = HashMap::new();
        assert!(!reverse_proxy_mode_from_settings(&settings));
    }

    #[test]
    fn reverse_proxy_mode_can_be_enabled() {
        let mut settings = HashMap::new();
        settings.insert("server-reverse-proxy.enable".to_string(), Value::Bool(true));
        assert!(reverse_proxy_mode_from_settings(&settings));
    }

    #[test]
    fn basic_auth_header_uses_setting_values() {
        let mut settings = HashMap::new();
        settings.insert("server-basic-auth.enable".to_string(), Value::Bool(true));
        settings.insert(
            "server-basic-auth.user".to_string(),
            Value::String("user".to_string()),
        );
        settings.insert(
            "server-basic-auth.password".to_string(),
            Value::String("pass".to_string()),
        );

        assert_eq!(
            basic_auth_header_from_settings(&settings).as_deref(),
            Some("Basic dXNlcjpwYXNz")
        );
    }

    #[test]
    fn control_token_generation_is_non_empty() {
        let token = generate_control_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn wildcard_bind_defaults_do_not_accept_arbitrary_ws_domains() {
        let domains = default_ws_accepted_domains("0.0.0.0");
        assert!(domains.contains(&"127.0.0.1".to_string()));
        assert!(!domains.iter().any(|domain| domain == "*"));
    }

    #[test]
    fn wildcard_bind_control_requests_use_loopback() {
        assert_eq!(control_request_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(control_request_host("::"), "::1");
        assert_eq!(control_request_host("127.0.0.1"), "127.0.0.1");
    }
}
