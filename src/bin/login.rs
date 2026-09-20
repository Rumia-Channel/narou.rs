//! `narou_rs_login` — sign in to a site and store the cookies narou.rs needs.
//!
//! The main binary never performs a login: it only detects that a fetch needs
//! authentication, retries with the stored cookie, and keeps that cookie fresh
//! from `Set-Cookie` responses. This executable does the signing in itself.
//!
//! Two ways to capture the session:
//!
//! * drive a Chromium-family browser through the DevTools protocol (`ws://`
//!   only, so no browser automation dependency), wait for the user to finish
//!   signing in — two-factor codes and CAPTCHAs work because it is a real
//!   browser session — then store the cookies the browser holds;
//! * or accept a cookie string copied from the browser (`--cookie`), for
//!   environments without a Chromium-family browser.
//!
//! Cookies are stored through the library inventory, so they live in
//! `.narou/login_cookie.yaml` or in SQLite `app_state` depending on the storage
//! backend. When this executable runs on a *different* machine than the one
//! that downloads, `--export <file>` writes a portable envelope instead (the
//! download host reads it back with `narou login import`), and outside a
//! library that export is the only output.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::native::cookie_store::InventoryCookieStore;
use narou_rs::platform::cookie_store::{cookie_host_for_url, format_cookie_header};
use narou_rs::login::build_export;
use narou_rs::platform::CookieStore;
use std::collections::BTreeMap;

#[derive(Parser)]
#[command(
    name = "narou_rs_login",
    about = "サイトにログインして、narou.rs が使う Cookie を保存します",
    version
)]
struct Args {
    /// ログインするサイト（URL / ドメイン / サイト名）
    target: Option<String>,
    /// ブラウザを開かず、ブラウザからコピーした Cookie 文字列を保存する
    #[arg(long, value_name = "COOKIE")]
    cookie: Option<String>,
    /// 使用する Chromium 系ブラウザのパス
    #[arg(long, value_name = "PATH")]
    browser: Option<PathBuf>,
    /// ブラウザのリモートデバッグポート
    #[arg(long, default_value_t = 9222)]
    port: u16,
    /// ログイン完了を待つ秒数
    #[arg(long, default_value_t = 300)]
    timeout: u64,
    /// 保存済みの Cookie を表示する
    #[arg(long)]
    list: bool,
    /// 保存済みの Cookie を削除する
    #[arg(long)]
    clear: bool,
    /// 取得した Cookie を書き出しファイル (YAML) に出力する
    #[arg(long, value_name = "FILE")]
    export: Option<PathBuf>,
    /// 書き出しファイルを暗号化するパスフレーズ
    #[arg(long, value_name = "PASS")]
    passphrase: Option<String>,
    /// パスフレーズ指定時でも平文で書き出す
    #[arg(long = "clear-text")]
    clear_text: bool,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("Error: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Default export file name when no library is present.
const DEFAULT_EXPORT_FILE: &str = "narou_login_export.yaml";

fn no_library_message() -> String {
    "このフォルダは初期化されていません (narou init)。ライブラリ外では --export <ファイル> で書き出してください".to_string()
}

fn run(args: Args) -> std::result::Result<(), String> {
    let store = InventoryCookieStore::for_current_root().ok();

    if args.list {
        let store = store.ok_or_else(no_library_message)?;
        return list_cookies(&store);
    }

    let target = args
        .target
        .clone()
        .ok_or_else(|| "ログインするサイト (URL / ドメイン / サイト名) を指定してください".to_string())?;
    let site = resolve_site(&target)?;
    let hosts = site_hosts(&site, &target);

    if args.clear {
        let store = store.ok_or_else(no_library_message)?;
        for host in &hosts {
            block_on(store.clear(host)).map_err(|error| error.to_string())?;
            println!("{host} の Cookie を削除しました");
        }
        return Ok(());
    }

    // Collect the cookies to store or export.
    let captured: BTreeMap<String, String> = if let Some(cookie) = args.cookie.as_deref() {
        let host = hosts
            .first()
            .ok_or_else(|| "Cookie を保存するホストを特定できませんでした".to_string())?
            .clone();
        BTreeMap::from([(host, cookie.to_string())])
    } else {
        let login_url = site
            .login_url()
            .or_else(|| Some(site.top_url()))
            .ok_or_else(|| format!("{} のログイン URL がサイト定義にありません", site.sitename))?;
        println!("{login_url} をブラウザで開きます。ログインが終わったら Enter を押してください。");

        let browser = args.browser.clone().or_else(find_browser).ok_or_else(|| {
            "Chromium 系ブラウザが見つかりませんでした。--browser でパスを指定するか、--cookie で Cookie 文字列を渡してください"
                .to_string()
        })?;
        let mut child = launch_browser(&browser, &login_url, args.port)?;
        let captured = match capture_cookies(&mut child, args.port, args.timeout, &site, &hosts) {
            Ok(captured) => captured,
            Err(error) => {
                let _ = child.kill();
                return Err(error);
            }
        };
        let _ = child.kill();
        captured.into_iter().collect()
    };

    if captured.is_empty() {
        return Err("Cookie を取得できませんでした".to_string());
    }

    // Outside a library the export is the only output; inside one it is
    // optional and the inventory keeps the cookies as before.
    let export_path = args
        .export
        .clone()
        .or_else(|| store.is_none().then(|| PathBuf::from(DEFAULT_EXPORT_FILE)));
    if let Some(path) = export_path {
        write_export(&path, &captured, store.as_ref(), &args)?;
    }
    if let Some(store) = &store {
        for (host, cookie) in &captured {
            save_cookie(store, host, cookie)?;
        }
    }
    Ok(())
}

/// Write the captured cookies (plus the library's existing ones) to a portable
/// export file the download host reads back with `narou login import`.
fn write_export(
    path: &Path,
    captured: &BTreeMap<String, String>,
    store: Option<&InventoryCookieStore>,
    args: &Args,
) -> std::result::Result<(), String> {
    let mut cookies = match store {
        Some(store) => store.load_all().map_err(|error| error.to_string())?,
        None => BTreeMap::new(),
    };
    cookies.extend(captured.iter().map(|(host, cookie)| (host.clone(), cookie.clone())));

    let passphrase = if args.clear_text {
        None
    } else {
        args.passphrase.as_deref()
    };
    let library = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned()));
    let text = build_export(
        &cookies,
        passphrase,
        &chrono::Local::now().to_rfc3339(),
        library.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| format!("{} に書き込めません: {error}", path.display()))?;

    println!("{} に {} サイトの Cookie を書き出しました", path.display(), cookies.len());
    match passphrase {
        Some(_) => println!(
            "  暗号化済みです。取り込み先で `narou login import {} --passphrase <同じパスフレーズ>` を実行してください。",
            path.display()
        ),
        None => println!(
            "  平文です。取り込み先で `narou login import {}` を実行してください。",
            path.display()
        ),
    }
    Ok(())
}

fn list_cookies(store: &InventoryCookieStore) -> std::result::Result<(), String> {
    let cookies = block_on(store.list()).map_err(|error| error.to_string())?;
    if cookies.is_empty() {
        println!("保存済みのログイン Cookie はありません");
        return Ok(());
    }
    for (host, cookie) in cookies {
        let names: Vec<&str> = cookie
            .split(';')
            .filter_map(|pair| pair.split('=').next())
            .map(str::trim)
            .collect();
        println!("{host}: {}", names.join(", "));
    }
    Ok(())
}

fn save_cookie(
    store: &InventoryCookieStore,
    host: &str,
    cookie: &str,
) -> std::result::Result<(), String> {
    let names: Vec<&str> = cookie
        .split(';')
        .filter_map(|pair| pair.split('=').next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    block_on(store.save(host, cookie)).map_err(|error| error.to_string())?;
    println!("{host} の Cookie を保存しました ({})", names.join(", "));
    Ok(())
}

/// Site definition matching a URL, domain, or site name.
fn resolve_site(target: &str) -> std::result::Result<SiteSetting, String> {
    let settings = SiteSetting::load_all().map_err(|error| error.to_string())?;
    if settings.is_empty() {
        return Err("サイト定義 (webnovel/*.yaml) が見つかりません".to_string());
    }
    let lower = target.to_ascii_lowercase();
    settings
        .iter()
        .find(|setting| setting.matches_url(target))
        .or_else(|| settings.iter().find(|setting| setting.domain.eq_ignore_ascii_case(&lower)))
        .or_else(|| {
            settings
                .iter()
                .find(|setting| setting.sitename.eq_ignore_ascii_case(target))
        })
        .cloned()
        .ok_or_else(|| format!("{target} に対応するサイト定義が見つかりません"))
}

/// Hosts whose cookies belong to this site: the login page, the top page, and
/// the site's declared domain.
fn site_hosts(site: &SiteSetting, target: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    let mut push = |url: &str| {
        if let Some(host) = cookie_host_for_url(url)
            && !hosts.contains(&host)
        {
            hosts.push(host);
        }
    };
    push(target);
    if let Some(login_url) = site.login_url() {
        push(&login_url);
    }
    push(&site.top_url());
    push(&format!("https://{}", site.domain));
    hosts
}

/// Launch the browser with remote debugging enabled and the login page open.
fn launch_browser(browser: &Path, login_url: &str, port: u16) -> std::result::Result<Child, String> {
    let profile = std::env::temp_dir().join(format!("narou-rs-login-{}", std::process::id()));
    Command::new(browser)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(login_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("ブラウザの起動に失敗しました: {error}"))
}

/// Poll the browser's cookies until the user finishes signing in.
fn capture_cookies(
    child: &mut Child,
    port: u16,
    timeout: u64,
    site: &SiteSetting,
    hosts: &[String],
) -> std::result::Result<Vec<(String, String)>, String> {
    let websocket_url = wait_for_devtools(port, Duration::from_secs(30))?;
    let (mut socket, _) = tungstenite::connect(websocket_url.as_str())
        .map_err(|error| format!("DevTools に接続できませんでした: {error}"))?;

    let finished = Arc::new(AtomicBool::new(false));
    {
        let finished = Arc::clone(&finished);
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let _ = stdin.lock().lines().next();
            finished.store(true, Ordering::SeqCst);
        });
    }

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut request_id = 0u64;
    let mut last_summary: Vec<String> = Vec::new();
    loop {
        request_id += 1;
        let request = serde_json::json!({ "id": request_id, "method": "Storage.getCookies" });
        socket
            .send(tungstenite::Message::text(request.to_string()))
            .map_err(|error| format!("DevTools への送信に失敗しました: {error}"))?;
        let cookies = read_cookies(&mut socket, request_id)?;
        let captured = cookies_for_site(&cookies, site, hosts);
        let summary: Vec<String> = captured
            .iter()
            .map(|(host, value)| format!("{host} ({} 件)", value.split(';').count()))
            .collect();
        if summary != last_summary {
            if !summary.is_empty() {
                println!("Cookie を検出しました: {}", summary.join(", "));
            }
            last_summary = summary;
        }
        if finished.load(Ordering::SeqCst) {
            return Ok(captured);
        }
        if Instant::now() >= deadline {
            println!("待ち時間が上限に達しました。取得できた Cookie を保存します。");
            return Ok(captured);
        }
        if child.try_wait().ok().flatten().is_some() {
            println!("ブラウザが終了しました。取得できた Cookie を保存します。");
            return Ok(captured);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Wait for the DevTools endpoint and return its browser websocket URL.
fn wait_for_devtools(port: u16, timeout: Duration) -> std::result::Result<String, String> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| error.to_string())?;
    let url = format!("http://127.0.0.1:{port}/json/version");
    loop {
        if let Ok(response) = client.get(&url).send()
            && let Ok(body) = response.text()
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
            && let Some(websocket) = value
                .get("webSocketDebuggerUrl")
                .and_then(|value| value.as_str())
        {
            return Ok(websocket.to_string());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "ブラウザの DevTools ポート {port} に接続できませんでした"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn read_cookies(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    request_id: u64,
) -> std::result::Result<Vec<serde_json::Value>, String> {
    loop {
        let message = socket
            .read()
            .map_err(|error| format!("DevTools からの受信に失敗しました: {error}"))?;
        let tungstenite::Message::Text(text) = message else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
            continue;
        };
        if value.get("id").and_then(|id| id.as_u64()) != Some(request_id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("DevTools がエラーを返しました: {error}"));
        }
        return Ok(value
            .get("result")
            .and_then(|result| result.get("cookies"))
            .and_then(|cookies| cookies.as_array())
            .cloned()
            .unwrap_or_default());
    }
}

/// Group the browser's cookies per host, keeping only this site's hosts.
fn cookies_for_site(
    cookies: &[serde_json::Value],
    site: &SiteSetting,
    hosts: &[String],
) -> Vec<(String, String)> {
    let mut grouped: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for cookie in cookies {
        let Some(name) = cookie.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(value) = cookie.get("value").and_then(|value| value.as_str()) else {
            continue;
        };
        let domain = cookie
            .get("domain")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if domain.is_empty() || !belongs_to_site(&domain, site, hosts) {
            continue;
        }
        match grouped.iter_mut().find(|(host, _)| *host == domain) {
            Some((_, pairs)) => pairs.push((name.to_string(), value.to_string())),
            None => grouped.push((domain, vec![(name.to_string(), value.to_string())])),
        }
    }
    grouped
        .into_iter()
        .map(|(host, pairs)| (host, format_cookie_header(&pairs)))
        .collect()
}

fn belongs_to_site(domain: &str, site: &SiteSetting, hosts: &[String]) -> bool {
    let site_domain = site.domain.to_ascii_lowercase();
    hosts.iter().any(|host| host == domain)
        || domain == site_domain
        || domain.ends_with(&format!(".{site_domain}"))
}

/// First Chromium-family browser found for the current platform.
fn find_browser() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let candidates = [
            ("PROGRAMFILES", r"Google\Chrome\Application\chrome.exe"),
            ("PROGRAMFILES(X86)", r"Google\Chrome\Application\chrome.exe"),
            ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
            ("PROGRAMFILES", r"Microsoft\Edge\Application\msedge.exe"),
            ("PROGRAMFILES(X86)", r"Microsoft\Edge\Application\msedge.exe"),
            ("LOCALAPPDATA", r"Microsoft\Edge\Application\msedge.exe"),
        ];
        for (variable, relative) in candidates {
            if let Some(root) = std::env::var_os(variable) {
                let path = PathBuf::from(root).join(relative);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let candidates = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ];
        return candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let names = ["google-chrome", "chromium", "chromium-browser", "microsoft-edge"];
        for name in names {
            if let Some(path) = find_on_path(name) {
                return Some(path);
            }
        }
        return None;
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Minimal blocking executor: the login flow is single-threaded.
fn block_on<T>(future: narou_rs::platform::PlatformFuture<'_, narou_rs::error::Result<T>>) -> narou_rs::error::Result<T> {
    futures::executor::block_on(future)
}
