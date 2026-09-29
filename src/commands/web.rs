use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::db::settings as settings_store;
use narou_rs::setting_core::SettingScope;
use serde_yaml::{Number, Value};
use tracing::info;

#[cfg(windows)]
#[path = "web_tray.rs"]
mod web_tray;

#[derive(Debug, Clone)]
struct WebAddress {
    host: String,
    port: u16,
    /// 併設 WebSocket リスナーのポート。`server-ws-port=0` のときは None
    /// (本体ポートの `/ws` だけで受ける)。
    ws_port: Option<u16>,
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
        address.host,
        address.port,
        match address.ws_port {
            Some(port) => port.to_string(),
            None => "off (same port)".to_string(),
        }
    );

    let server_security =
        match narou_rs::web::server_security::ServerSecurity::load(&address.host) {
            Ok(settings) => settings,
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };
    let push_server = web::push::PushServer::new();
    push_server.set_accepted_domains(server_security.accepted_ws_domains.clone());
    let push_server = Arc::new(push_server);
    if requires_basic_auth_for_bind(&address.host)
        && server_security.require_basic_auth_for_external_bind
        && server_security.basic_auth_header.is_none()
    {
        eprintln!(
            "Error: server-bind が外部公開設定のため、server-basic-auth を有効にして user/password を設定して下さい"
        );
        std::process::exit(1);
    }
    let control_token = generate_control_token();
    let inventory = match Inventory::with_default_root() {
        Ok(inventory) => Arc::new(inventory),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };
    let root_dir = inventory.root_dir().to_path_buf();
    let native_services =
        match narou_rs::native::application::NativeAppServices::new(inventory.clone()) {
            Ok(services) => services,
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };
    let site_updates = native_services.site_updates;
    let services = native_services.services;
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
        // 併設リスナーを切っているときは本体ポートで受ける (`/ws` は本体のルータにもある)。
        ws_port: address.ws_port.unwrap_or(address.port),
        push_server: push_server.clone(),
        services: services.clone(),
        server_security: Arc::new(parking_lot::RwLock::new(server_security)),
        bind_host: Arc::from(address.host.as_str()),
        control_token: control_token.clone(),
        queue: queue.clone(),
        restore_prompt_pending: restore_prompt_pending.clone(),
        restorable_tasks_available: restorable_tasks_available.clone(),
        running_jobs: running_jobs.clone(),
        running_child_pids: running_child_pids.clone(),
        cancelled_job_ids: cancelled_job_ids.clone(),
        auto_update_scheduler: auto_update_scheduler.clone(),
        library_backup: Arc::new(web::library_backup::LibraryBackupState::new()),
    };
    let app = web::create_router(app_state.clone());
    let ws_app = address
        .ws_port
        .map(|_| web::push::create_push_router(app_state.clone()));
    let addr: SocketAddr = format!("{}:{}", address.host, address.port)
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
    let ws_listener = match address.ws_port {
        Some(ws_port) => {
            let ws_addr: SocketAddr = format!("{}:{}", address.host, ws_port)
                .parse()
                .unwrap();
            Some(bind_or_shutdown_and_retry(ws_addr, &address.host, ws_port, "WebSocket").await)
        }
        None => None,
    };

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
        web::worker::QueueWorkerContext {
            root_dir: root_dir.clone(),
            queue: queue.clone(),
            push_server: push_server.clone(),
            library: services.library.clone(),
            site_updates,
            running_jobs: running_jobs.clone(),
            running_child_pids,
            cancelled_job_ids,
        },
        narou_rs::compat::load_local_setting_bool("concurrency"),
    );
    web::scheduler::start_or_restart_auto_update_scheduler(
        queue,
        running_jobs,
        push_server.clone(),
        &auto_update_scheduler,
    );

    // サイト定義 (webnovel/*.yaml またはオブジェクトストア) の外部変更を拾う。
    // 自分のプロセスからの保存は PUT/DELETE が即時差し替えるので、ここは
    // 「外部編集が反映されるまでの最大遅延」として 30 秒間隔で再読込する
    // (worker_entry::isolate_cache の TTL と同じ粒度)。
    let site_definitions_poll = tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if !narou_rs::db::narou_root_exists() {
                continue;
            }
            if let Err(error) =
                narou_rs::native::site_definitions::install_effective_site_settings().await
            {
                tracing::warn!("サイト定義の再読み込みに失敗しました: {error}");
            }
        }
    });

    // `server-*` / `update.auto-schedule.*` / `webui.*` / `logging*` など、
    // 別プロセス (narou setting や手編集) からの設定変更を拾って再適用する。
    // 保存 API からの変更は即時反映済みなので、ここは外部変更が反映されるまでの
    // 最大遅延として 30 秒間隔でポーリングする (上のサイト定義と同じ粒度)。
    let settings_watch = watch_server_settings(app_state.clone());

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

    let ws_task = match (ws_listener, ws_app) {
        (Some(listener), Some(app)) => {
            Some(tokio::spawn(async move { axum::serve(listener, app).await }))
        }
        _ => None,
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .unwrap();
    for worker_task in worker_tasks {
        worker_task.abort();
    }
    web::scheduler::stop_auto_update_scheduler(&auto_update_scheduler);
    if let Some(task) = ws_task {
        task.abort();
    }
    site_definitions_poll.abort();
    settings_watch.abort();
    remove_pid_file();
}

fn fill_general_all_no_in_database() -> Result<(), String> {
    use narou_rs::platform::{NovelFilter, NovelMutation};

    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let ids = novels
        .scan_ids_sync(&NovelFilter::all(), None, usize::MAX)
        .map_err(|e| e.to_string())?;
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .map_err(|e| e.to_string())?;
    let mut modified = false;

    for id in ids {
        let Ok(Some(mut record)) = novels.get_sync(id) else {
            continue;
        };
        if record.general_all_no.is_some() {
            continue;
        }
        let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
        let Some(toc) = narou_rs::native::legacy_persistence::load_toc_file(&novel_dir) else {
            continue;
        };
        record.general_all_no = Some(toc.subtitles.len() as i64);
        novels
            .apply_batch_sync(vec![NovelMutation::Upsert(record)])
            .map_err(|e| e.to_string())?;
        modified = true;
    }

    let _ = modified;
    Ok(())
}

fn resolve_web_address(user_port: Option<u16>) -> Result<WebAddress, String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let mut global_setting: HashMap<String, Value> =
        settings_store::load_with_inventory(&inventory, SettingScope::Global).unwrap_or_default();
    let host = normalize_bind_host(yaml_string(global_setting.get("server-bind")));
    let port = if let Some(port) = user_port {
        port
    } else if let Some(port) = yaml_u16(global_setting.get("server-port")) {
        port
    } else {
        let port = find_available_web_port(&host)?;
        global_setting.insert("server-port".to_string(), Value::Number(Number::from(port)));
        settings_store::save_with_inventory(&inventory, SettingScope::Global, &global_setting)
            .map_err(|e| e.to_string())?;
        port
    };
    // `server-ws-port`: 未設定は従来どおり server-port + 1。0 を指定すると
    // 併設リスナーを作らない (本体ポートの `/ws` で受ける)。SORAHOST のように
    // 前段が port + 1 を使うプラットフォーム向け。
    let ws_port = match yaml_u16(global_setting.get("server-ws-port")) {
        Some(0) => None,
        Some(value) => Some(value),
        None => Some(
            port.checked_add(1)
                .ok_or_else(|| "server-port + 1 が不正な値になります".to_string())?,
        ),
    };
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

// `server-*` 系の導出値は `narou_rs::web::server_security::ServerSecurity` が
// `global_setting` から再計算する。起動時と同じ関数を、保存 API フックと
// `watch_server_settings` のポーリングからも呼び、稼働中の再適用を可能にする。

#[cfg(test)]
fn is_wildcard_bind_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::")
}

fn requires_basic_auth_for_bind(host: &str) -> bool {
    !matches!(host, "127.0.0.1" | "localhost" | "::1")
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
    if !is_first_web_boot(&server_setting) {
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

    mark_first_web_boot_done(&mut server_setting);
    inventory
        .save("server_setting", InventoryScope::Global, &server_setting)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// `server_setting` の `already-server-boot` を読む。
///
/// narou.rb は素の文字列キー `"already-server-boot"` を読み書きするが、YAML
/// ファイルには Ruby シンボル由来の `:already-server-boot:` キーが紛れ込む
/// ことがあるため、読みは両形式を見る。
fn is_first_web_boot(server_setting: &HashMap<String, Value>) -> bool {
    let value = server_setting
        .get("already-server-boot")
        .or_else(|| server_setting.get(":already-server-boot"));
    !yaml_bool(value).unwrap_or(false)
}

/// `already-server-boot = true` を narou.rb と同じ素の文字列キーで書く
/// (`narou.rb` は `setting["already-server-boot"]` を読む)。シンボルキー
/// 由来の `:already-server-boot:` が残っていれば消して一本化する。
fn mark_first_web_boot_done(server_setting: &mut HashMap<String, Value>) {
    server_setting.remove(":already-server-boot");
    server_setting.insert("already-server-boot".to_string(), Value::Bool(true));
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

/// Polls `local_setting` / `global_setting` for changes made outside this
/// process (`narou setting`, manual YAML edits, other tools) and re-applies
/// the affected runtime state:
///
/// - `server-*` 系 (`server-basic-auth.*` / `server-add-accepted-hosts` /
///   `server-ws-add-accepted-domains` / `server-reverse-proxy.enable`) →
///   `AppState::reload_server_security`
/// - `update.auto-schedule(.enable/.timezone)` → 自動更新スケジューラの再起動
/// - `webui.*` / `concurrency` → `webui.config.reload` をブラウザへ broadcast
/// - `logging*` / `concurrency` (ログ分割) → `logger::init` で状態を再構築
///
/// 保存 API からの変更は `save_global_settings` が即時反映するため、この
/// ポーリングは「外部編集が反映されるまでの最大遅延」を決めるだけ。
fn watch_server_settings(state: narou_rs::web::AppState) -> tokio::task::JoinHandle<()> {
    fn signature(
        settings: &HashMap<String, Value>,
        names: &[&str],
    ) -> Vec<(String, Value)> {
        names
            .iter()
            .map(|name| (name.to_string(), settings.get(*name).cloned().unwrap_or(Value::Null)))
            .collect()
    }

    const AUTO_SCHEDULE_NAMES: &[&str] = &[
        "update.auto-schedule.enable",
        "update.auto-schedule",
        "update.auto-schedule.timezone",
    ];
    const LOGGING_NAMES: &[&str] = &[
        "logging",
        "logging.format-filename",
        "logging.format-timestamp",
        "concurrency",
    ];

    tokio::spawn(async move {
        let mut last_auto_schedule: Option<Vec<(String, Value)>> = None;
        let mut last_webui: Option<Vec<(String, Value)>> = None;
        let mut last_logging: Option<Vec<(String, Value)>> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if !narou_rs::db::narou_root_exists() {
                continue;
            }

            // global: server-* セキュリティ設定。load が失敗した tick は
            // 既存の値を維持する (一時的な読み取り失敗で締め出さない)。
            match narou_rs::web::server_security::ServerSecurity::load(&state.bind_host) {
                Ok(security) => {
                    if *state.server_security.read() != security {
                        state.apply_server_security(&security);
                        state
                            .push_server
                            .broadcast_echo("server-* 設定を再適用しました", "stdout");
                    }
                }
                Err(error) => {
                    tracing::warn!("server 設定の再読み込みに失敗しました: {error}");
                }
            }

            let local = settings_store::load(SettingScope::Local).unwrap_or_default();

            let auto_schedule = signature(&local, AUTO_SCHEDULE_NAMES);
            if last_auto_schedule.as_ref() == Some(&auto_schedule) {
                // unchanged
            } else {
                let first = last_auto_schedule.replace(auto_schedule).is_none();
                if !first {
                    let started = narou_rs::web::scheduler::start_or_restart_auto_update_scheduler(
                        state.queue.clone(),
                        state.running_jobs.clone(),
                        state.push_server.clone(),
                        &state.auto_update_scheduler,
                    );
                    let message = if started {
                        "自動アップデートスケジューラーを更新しました"
                    } else {
                        "自動アップデートスケジューラーを停止しました"
                    };
                    state.push_server.broadcast_echo(message, "stdout");
                }
            }

            let webui = signature(&local, narou_rs::application::LIVE_WEBUI_CONFIG_NAMES);
            if last_webui.as_ref() == Some(&webui) {
                // unchanged
            } else {
                let first = last_webui.replace(webui).is_none();
                if !first {
                    state.push_server.broadcast_event("webui.config.reload", "");
                }
            }

            let logging = signature(&local, LOGGING_NAMES);
            if last_logging.as_ref() == Some(&logging) {
                // unchanged
            } else {
                let first = last_logging.replace(logging).is_none();
                if !first {
                    // bin 側 (`mod logger` in main.rs) と lib 側の両方の
                    // LoggerState を再構築する。
                    crate::logger::init();
                    narou_rs::logger::init();
                }
            }
        }
    })
}


#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_yaml::Value;

    use super::{
        control_request_host, display_host, generate_control_token, is_wildcard_bind_host,
        normalize_bind_host, requires_basic_auth_for_bind,
    };

    #[test]
    fn first_boot_reads_both_key_forms() {
        // narou.rb は素の文字列キー、紛れ込んだ YAML にはシンボルキー由来の
        // `:already-server-boot` もある。どちらも「起動済み」として読む。
        let mut settings = HashMap::new();
        assert!(super::is_first_web_boot(&settings));
        settings.insert("already-server-boot".to_string(), Value::Bool(true));
        assert!(!super::is_first_web_boot(&settings));

        let mut symbol_only = HashMap::new();
        symbol_only.insert(":already-server-boot".to_string(), Value::Bool(true));
        assert!(!super::is_first_web_boot(&symbol_only));

        let mut symbol_false = HashMap::new();
        symbol_false.insert(":already-server-boot".to_string(), Value::Bool(false));
        assert!(super::is_first_web_boot(&symbol_false));
    }

    #[test]
    fn first_boot_write_uses_plain_key_and_drops_symbol_key() {
        // 書き込みは narou.rb が読む素のキー一本に揃える。
        let mut settings = HashMap::new();
        settings.insert(":already-server-boot".to_string(), Value::Bool(false));
        super::mark_first_web_boot_done(&mut settings);
        assert_eq!(
            settings.get("already-server-boot"),
            Some(&Value::Bool(true))
        );
        assert!(!settings.contains_key(":already-server-boot"));
    }

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
    fn wildcard_bind_hosts_require_explicit_auth() {
        assert!(is_wildcard_bind_host("0.0.0.0"));
        assert!(requires_basic_auth_for_bind("0.0.0.0"));
        assert!(!requires_basic_auth_for_bind("127.0.0.1"));
    }


    #[test]
    fn control_token_generation_is_non_empty() {
        let token = generate_control_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn wildcard_bind_control_requests_use_loopback() {
        assert_eq!(control_request_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(control_request_host("::"), "::1");
        assert_eq!(control_request_host("127.0.0.1"), "127.0.0.1");
    }
}
