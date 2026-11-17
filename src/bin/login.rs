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

use std::borrow::Cow;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use clap::Parser;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::native::cookie_store::InventoryCookieStore;
use narou_rs::platform::cookie_store::{cookie_host_for_url, format_cookie_header};
use narou_rs::platform::{HostCookie, LoginGroup};
use narou_rs::login::build_export;
use narou_rs::platform::CookieStore;
use std::collections::BTreeMap;

#[path = "login/browser_token.rs"]
mod browser_token;

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
    /// ブラウザのリモートデバッグポート (0 = ブラウザに空きポートを選ばせる)
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// サイト定義の SDK でブラウザ確認トークンを準備する
    #[arg(long, conflicts_with = "cookie")]
    browser_token: bool,
    /// Private credential-helper protocol (stdout contains the token).
    #[arg(long, hide = true, requires = "browser_token", conflicts_with_all = ["cookie", "export", "passphrase", "clear_text", "list", "clear"])]
    token_json: bool,
    /// ログイン完了を待つ秒数
    #[arg(long, default_value_t = 300)]
    timeout: u64,
    /// 保存済みの Cookie を表示する
    #[arg(long)]
    list: bool,
    /// 保存済みの Cookie を削除する
    #[arg(long)]
    clear: bool,
    /// ブラウザプロファイルを使い回す（既定は毎回まっさらな状態で開く）
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,
    /// 取得した Cookie を書き出しファイル (YAML) に出力する
    #[arg(long, value_name = "FILE")]
    export: Option<PathBuf>,
    /// 書き出しファイルを暗号化するパスフレーズ
    #[arg(long, value_name = "PASS")]
    passphrase: Option<String>,
    /// パスフレーズ指定時でも平文で書き出す
    #[arg(long = "clear-text")]
    clear_text: bool,
    /// この取得に付ける名前（`narou login list` や Web UI に表示される）
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
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
    let store = if args.token_json { None } else { InventoryCookieStore::for_current_root().ok() };

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

    // Collect the cookies to store or export. A throwaway browser profile (the
    // default) is remembered here and removed once the cookies are safe.
    let user_agent = if args.token_json { None } else { narou_rs::compat::load_local_setting_string("user-agent") };
    let mut token_profile = if args.token_json && args.profile.is_none() {
        Some(narou_rs::native::browser_profile::BrowserProfile::new()
            .map_err(|_| "ブラウザ確認用の一時プロファイルを作成できません".to_string())?)
    } else {
        None
    };
    let mut throwaway_profile: Option<PathBuf> = None;
    let captured: BTreeMap<String, String> = if let Some(cookie) = args.cookie.as_deref() {
        let host = hosts
            .first()
            .ok_or_else(|| "Cookie を保存するホストを特定できませんでした".to_string())?
            .clone();
        BTreeMap::from([(host, cookie.to_string())])
    } else {
        if args.browser_token && site.browser_token.is_none() {
            return Err("このサイトにはブラウザトークン取得が定義されていません".to_string());
        }
        let login_url = if args.browser_token
            && (site.matches_url(&target) || site.matches_author_url(&target))
        {
            Cow::Borrowed(target.as_str())
        } else {
            browser_start_url(&site, &target)
        };
        if !args.token_json {
            println!("{login_url} をブラウザで開きます。確認・ログインが終わったら Enter を押してください。");
        }
        // Port 0 advertises WebDriver automation in Chromium. This explicit
        // token mode uses a free nonzero port, authenticated by the child's
        // own endpoint announcement before any cookies are read.
        let port = if args.browser_token && args.port == 0 {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                .map_err(|_| "ブラウザの起動ポートを確保できません".to_string())?;
            listener.local_addr()
                .map_err(|_| "ブラウザの起動ポートを確認できません".to_string())?.port()
        } else {
            args.port
        };

        let browser = args.browser.clone().or_else(find_browser).ok_or_else(|| {
            "Chromium 系ブラウザが見つかりませんでした。--browser でパスを指定するか、--cookie で Cookie 文字列を渡してください"
                .to_string()
        })?;
        let profile = token_profile.as_ref().map_or_else(
            || profile_dir(args.profile.as_deref()), |profile| profile.path().to_path_buf(),
        );
        std::fs::create_dir_all(&profile)
            .map_err(|error| format!("{} を作成できません: {error}", profile.display()))?;
        let mut launched = launch_browser(&browser, &login_url, port, &profile)?;
        let captured = match capture_cookies(
            &mut launched,
            &profile,
            args.timeout,
            &site,
            &hosts,
            &login_url,
            if args.token_json { CaptureMode::TokenJson } else if args.browser_token { CaptureMode::BrowserToken } else { CaptureMode::Interactive },
        ) {
            Ok(captured) => captured,
            Err(error) => {
                stop_browser(&mut launched.child);
                if !args.token_json {
                    println!(
                        "ブラウザプロファイルを残しました: {} (再試行時に使えます)",
                        profile.display()
                    );
                }
                return Err(error);
            }
        };
        stop_browser(&mut launched.child);
        if let Some(profile) = token_profile.take() {
            profile.close().map_err(|_| "ブラウザ確認用の一時プロファイルを削除できません".to_string())?;
        }
        if args.profile.is_none() && !args.token_json {
            throwaway_profile = Some(profile);
        }
        captured.into_iter().collect()
    };

    if captured.is_empty() {
        return Err("Cookie を取得できませんでした".to_string());
    }

    if args.token_json {
        let config = site.browser_token
            .ok_or_else(|| "ブラウザトークン取得の定義がありません".to_string())?;
        let response = browser_token_response(site.domain, config.cookie_name, &target, captured)?;
        return serde_json::to_writer(std::io::stdout().lock(), &response)
            .map_err(|_| "確認トークンを呼び出し元へ渡せません".to_string());
    }

    // Outside a library the export is the only output; inside one it is
    // optional and the inventory keeps the cookies as before.
    let export_path = args
        .export
        .clone()
        .or_else(|| store.is_none().then(|| PathBuf::from(DEFAULT_EXPORT_FILE)));
    if let Some(path) = export_path {
        write_export(&path, &site, &captured, store.as_ref(), &args)?;
    }
    if let Some(store) = &store {
        save_group(store, &site.domain, &captured, args.name.as_deref())?;
    }

    // 保存は済んでいるので、その Cookie で実際に取得できるかだけ確かめる。
    if site.matches_url(&target) && cookie_host_for_url(&target).is_some() {
        verify_capture(&target, site, captured, user_agent);
    }
    // Cookie を保存し終えたので、使い捨てのプロファイルを片付ける。
    if let Some(profile) = throwaway_profile {
        cleanup_profile(&profile);
    }
    Ok(())
}

fn browser_token_response(
    site: String,
    cookie_name: String,
    target: &str,
    mut cookies: BTreeMap<String, String>,
) -> std::result::Result<narou_rs::platform::cookie_store::BrowserTokenResponse, String> {
    let host = cookie_host_for_url(target)
        .ok_or_else(|| "ブラウザトークン取得には対象 URL が必要です".to_string())?;
    let allowed_hosts = narou_rs::platform::cookie_store::cookie_lookup_hosts(&host);
    cookies.retain(|host, header| {
        allowed_hosts.iter().any(|allowed| allowed == host)
            && retain_token_cookie(header, &cookie_name)
    });
    if cookies.is_empty() {
        return Err("対象 URL の確認トークンを取得できませんでした".to_string());
    }
    Ok(narou_rs::platform::cookie_store::BrowserTokenResponse {
        version: narou_rs::platform::cookie_store::BROWSER_TOKEN_RESPONSE_VERSION,
        site,
        cookie_name,
        cookies,
    })
}

fn retain_token_cookie(header: &mut String, wanted: &str) -> bool {
    let mut offset = 0;
    let mut selected = None;
    for part in header.split(';') {
        let trimmed = part.trim();
        if trimmed.split_once('=').is_some_and(|(name, value)| name == wanted && !value.is_empty()) {
            let start = offset + part.len() - part.trim_start().len();
            selected = Some((start, start + trimmed.len()));
            break;
        }
        offset += part.len() + 1;
    }
    if let Some((start, end)) = selected {
        header.truncate(end);
        drop(header.drain(..start));
        true
    } else {
        false
    }
}

fn browser_start_url<'a>(site: &SiteSetting, target: &'a str) -> Cow<'a, str> {
    if matches!(site.login_wall_error, narou_rs::downloader::site_setting::LoginWallError::Login)
        && (site.matches_url(target) || site.matches_author_url(target))
    {
        Cow::Borrowed(target)
    } else {
        Cow::Owned(site.login_url().unwrap_or_else(|| site.top_url()))
    }
}

fn stop_browser(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = narou_rs::compat::terminate_process(child.id());
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Browser profile directory for this run.
///
/// The default is a fresh directory: the browser must start signed out, or
/// signing in as somebody else means logging out first (and that detour is
/// where a session often dies). Pass `--profile <DIR>` to reuse one instead —
/// for the browser's own conveniences (saved logins, extensions) — knowing the
/// session is then carried over.
fn profile_dir(override_dir: Option<&Path>) -> PathBuf {
    match override_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::temp_dir().join(format!("narou-rs-login-{}", std::process::id())),
    }
}

/// Remove a throwaway browser profile once the cookies are safely stored.
fn cleanup_profile(profile: &Path) {
    if narou_rs::native::browser_profile::remove(profile).is_err() {
        eprintln!("WARN: ブラウザの一時プロファイルを削除できませんでした");
    }
}

/// One request with the captured cookies, mirroring the downloader's login-wall
/// detection: a work that needs the session keeps answering with the site's
/// error message or login marker while the captured cookies are anonymous.
fn verify_capture(target: &str, mut site: SiteSetting, cookies: BTreeMap<String, String>, user_agent: Option<String>) {
    if cookie_host_for_url(target).is_none() {
        return;
    }
    let cookie = LoginGroup::new(
        &site.domain,
        cookies.into_iter().map(|(host, cookie)| HostCookie { host, cookie }).collect(),
    ).merged_cookie();
    if cookie.is_empty() {
        return;
    }
    site.cookie = match site.cookie.take() {
        Some(base) => narou_rs::platform::cookie_store::merge_cookie_headers(Some(&base), Some(&cookie)),
        None => Some(cookie),
    };
    let user_agent = narou_rs::downloader::resolve_user_agent(None, user_agent);
    let Ok(client) = narou_rs::native::http::NativeHttpClient::new(&user_agent) else {
        return;
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("注意: Cookie の検証を実行できません: {error}");
            return;
        }
    };
    let url = site.toc_url_with_url_captures(target).unwrap_or_else(|| target.to_string());
    let limiter = narou_rs::downloader::rate_limit::RateLimiter::with_settings(0.0, 0);
    let result = runtime.block_on(narou_rs::downloader::toc::fetch_toc(
        &client, &limiter, &site, &url,
        &mut narou_rs::downloader::preprocess::PreprocessJobs::new(),
    ));
    match result {
        Ok(source) if site.compiled_login_pattern().is_some_and(|pattern| pattern.is_match(&source))
            || site.is_partial_login_view(&source) => {
            println!("注意: {target} は Cookie を付けても閲覧制限があります。ログイン状態や閲覧権限をサイトで確認してください");
        }
        Ok(_) => println!("{target} を保存した Cookie で取得できることを確認しました"),
        Err(error) => println!("注意: Cookie の検証で取得に失敗しました: {error}"),
    }
}

/// Write the captured cookies (plus the library's existing ones) to a portable
/// export file the download host reads back with `narou login import`.
fn write_export(
    path: &Path,
    site: &SiteSetting,
    captured: &BTreeMap<String, String>,
    store: Option<&InventoryCookieStore>,
    args: &Args,
) -> std::result::Result<(), String> {
    let mut sites = match store {
        Some(store) => store.groups_by_site().map_err(|error| error.to_string())?,
        None => BTreeMap::new(),
    };
    sites.insert(
        site.domain.clone(),
        vec![
            LoginGroup::new(
                &site.domain,
                captured
                    .iter()
                    .map(|(host, cookie)| HostCookie {
                        host: host.clone(),
                        cookie: cookie.clone(),
                    })
                    .collect(),
            )
            .with_label(args.name.as_deref().map(str::to_string))
            .with_added_at(Some(chrono::Local::now().to_rfc3339())),
        ],
    );

    let passphrase = if args.clear_text {
        None
    } else {
        args.passphrase.as_deref()
    };
    let library = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned()));
    let text = build_export(
        &sites,
        passphrase,
        &chrono::Local::now().to_rfc3339(),
        library.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    narou_rs::login::transfer::write_export_file(path, &text)
        .map_err(|error| format!("{} に書き込めません: {error}", path.display()))?;

    let logins: usize = sites.values().map(Vec::len).sum();
    println!(
        "{} に {} 件のログイン情報を書き出しました",
        path.display(),
        logins
    );
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
    let stored = store.groups_by_site().map_err(|error| error.to_string())?;
    if stored.is_empty() {
        println!("保存済みのログイン Cookie はありません");
        return Ok(());
    }
    for (site, groups) in stored {
        println!("{site}:");
        for (index, group) in groups.iter().enumerate() {
            let hosts: Vec<&str> = group.cookies.iter().map(|entry| entry.host.as_str()).collect();
            println!(
                "  {}. {} ({} ホスト: {})",
                index + 1,
                group.display_name(),
                group.cookies.len(),
                hosts.join(", ")
            );
        }
    }
    Ok(())
}

fn save_group(
    store: &InventoryCookieStore,
    site: &str,
    captured: &BTreeMap<String, String>,
    name: Option<&str>,
) -> std::result::Result<(), String> {
    let group = LoginGroup::new(
        site,
        captured
            .iter()
            .map(|(host, cookie)| HostCookie {
                host: host.clone(),
                cookie: cookie.clone(),
            })
            .collect(),
    )
    .with_label(name.map(str::to_string))
    .with_added_at(Some(chrono::Local::now().to_rfc3339()));
    let mut map = BTreeMap::new();
    map.insert(site.to_string(), vec![group.clone()]);
    store
        .merge_groups(&map)
        .map_err(|error| error.to_string())?;
    println!(
        "{site} のログインを保存しました ({} ホスト)",
        group.cookies.len()
    );
    Ok(())
}

/// Site definition matching a URL, domain, or site name.
fn resolve_site(target: &str) -> std::result::Result<SiteSetting, String> {
    let settings = narou_rs::downloader::site_setting::effective_site_settings();
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
        .or_else(|| {
            let host = cookie_host_for_url(target);
            let key = narou_rs::platform::site_for_host_with(&settings, host.as_deref().unwrap_or(&lower));
            settings.iter().find(|setting| setting.domain.eq_ignore_ascii_case(&key))
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    Interactive,
    BrowserToken,
    TokenJson,
}

struct BrowserProcess {
    child: Child,
    endpoint: Option<Receiver<String>>,
    port: u16,
}

/// Launch the browser with remote debugging enabled and the login page open.
///
/// `port == 0` lets the browser pick a free port; it reports the choice in
/// `<profile>/DevToolsActivePort` (see [`wait_for_devtools`]). A reused
/// `--profile` can carry that file from an earlier run, so it is removed
/// before the browser starts and only the new browser's value is read.
fn launch_browser(
    browser: &Path,
    login_url: &str,
    port: u16,
    profile: &Path,
) -> std::result::Result<BrowserProcess, String> {
    if port == 0 {
        let _ = std::fs::remove_file(profile.join(DEVTOOLS_ACTIVE_PORT_FILE));
    }
    let mut child = Command::new(browser)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(login_url)
        .stdout(Stdio::null())
        .stderr(if port == 0 { Stdio::null() } else { Stdio::piped() })
        .spawn()
        .map_err(|error| format!("ブラウザの起動に失敗しました: {error}"))?;
    if port == 0 {
        return Ok(BrowserProcess { child, endpoint: None, port });
    }
    let Some(mut stderr) = child.stderr.take() else {
        stop_browser(&mut child);
        return Err("ブラウザの DevTools 起動通知を取得できません".to_string());
    };
    let (reported, endpoint) = mpsc::sync_channel(1);
    if std::thread::Builder::new().name("narou-browser-endpoint".to_string()).spawn(move || {
        if let Some(websocket) = read_browser_endpoint(&mut stderr) {
            let _ = reported.send(websocket);
        }
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    }).is_err() {
        stop_browser(&mut child);
        return Err("ブラウザの DevTools 起動通知を待機できません".to_string());
    }
    Ok(BrowserProcess { child, endpoint: Some(endpoint), port })
}

/// Poll the browser's cookies until the user finishes signing in.
fn capture_cookies(
    browser: &mut BrowserProcess,
    profile: &Path,
    timeout: u64,
    site: &SiteSetting,
    hosts: &[String],
    target_url: &str,
    mode: CaptureMode,
) -> std::result::Result<Vec<(String, String)>, String> {
    let websocket_url = wait_for_devtools(
        &mut browser.child, browser.endpoint.as_ref(), browser.port, profile, Duration::from_secs(30),
    )?;
    if mode != CaptureMode::Interactive {
        let config = site.browser_token.as_ref()
            .ok_or_else(|| "このサイトにはブラウザトークン取得が定義されていません".to_string())?;
        browser_token::prepare_token(&websocket_url, config, target_url)?;
        if mode != CaptureMode::TokenJson {
            println!("ブラウザの確認トークンを準備しました。確認が終わったら Enter で保存します。");
        }
    }
    let mut socket = connect_devtools(&websocket_url, Duration::from_secs(5))?;

    if mode == CaptureMode::TokenJson {
        return read_site_cookies(&mut socket, 1, site, hosts);
    }
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
        let captured = read_site_cookies(&mut socket, request_id, site, hosts)?;
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
        if browser.child.try_wait().ok().flatten().is_some() {
            println!("ブラウザが終了しました。取得できた Cookie を保存します。");
            return Ok(captured);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn read_site_cookies(
    socket: &mut DevToolsSocket,
    request_id: u64,
    site: &SiteSetting,
    hosts: &[String],
) -> std::result::Result<Vec<(String, String)>, String> {
    let request = serde_json::json!({ "id": request_id, "method": "Storage.getCookies" });
    socket.send(tungstenite::Message::text(request.to_string()))
        .map_err(|_| "DevTools へ Cookie 取得要求を送信できません".to_string())?;
    let cookies = read_cookies(socket, request_id)?;
    Ok(cookies_for_site(&cookies, site, hosts))
}

/// ブラウザが自動選択した DevTools ポートを書くファイル名 (`<profile>` 直下)。
const DEVTOOLS_ACTIVE_PORT_FILE: &str = "DevToolsActivePort";

/// Read only the endpoint announcement emitted by the child we started.
/// Long or invalid diagnostic lines are discarded without growing a buffer.
fn read_browser_endpoint(reader: &mut impl Read) -> Option<String> {
    let mut line = [0u8; 4096];
    let mut used = 0usize;
    let mut oversized = false;
    let mut chunk = [0u8; 1024];
    loop {
        let count = reader.read(&mut chunk).ok()?;
        if count == 0 {
            return (!oversized).then(|| parse_browser_endpoint_line(&line[..used])).flatten();
        }
        for &byte in &chunk[..count] {
            if byte == b'\n' {
                if !oversized
                    && let Some(endpoint) = parse_browser_endpoint_line(&line[..used])
                {
                    return Some(endpoint);
                }
                used = 0;
                oversized = false;
            } else if used < line.len() {
                line[used] = byte;
                used += 1;
            } else {
                oversized = true;
            }
        }
    }
}

fn parse_browser_endpoint_line(line: &[u8]) -> Option<String> {
    std::str::from_utf8(line).ok()?.trim()
        .strip_prefix("DevTools listening on ")
        .filter(|endpoint| !endpoint.is_empty())
        .map(str::to_owned)
}

fn reported_browser_endpoint(websocket: &str, requested_port: u16) -> std::result::Result<(u16, String), String> {
    let parsed = url::Url::parse(websocket)
        .map_err(|_| "ブラウザの DevTools 起動通知が不正です".to_string())?;
    let port = parsed.port().filter(|port| *port != 0)
        .ok_or_else(|| "ブラウザの DevTools 起動通知にポートがありません".to_string())?;
    if requested_port != 0 && port != requested_port {
        return Err("ブラウザの DevTools 起動通知が指定ポートと一致しません".to_string());
    }
    verify_devtools_websocket(websocket, port, None)?;
    Ok((port, parsed.path().to_owned()))
}
/// ブラウザ用 WebSocket のパス接頭辞。ページ用 (`/devtools/page/...`) ではなく、
/// ブラウザ全体の Cookie を読む Browser エンドポイントに接続する。
const DEVTOOLS_BROWSER_PATH: &str = "/devtools/browser/";

/// `<profile>/DevToolsActivePort` をパースする。
///
/// ブラウザは 1 行目に実際に選んだポート、2 行目にブラウザ用 WebSocket の
/// パス (`/devtools/browser/...`) を書く。書きかけ・形式違い・ポート 0 などは
/// `None` を返し、呼び出し側がポーリングを続ける。
fn parse_devtools_active_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines().map(str::trim);
    let port = lines
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    let path = lines.next()?;
    path.starts_with(DEVTOOLS_BROWSER_PATH)
        .then(|| (port, path.to_string()))
}

/// `<profile>/DevToolsActivePort` が示すポート (まだ無ければ None)。
fn read_devtools_active_port(profile: &Path) -> Option<(u16, String)> {
    parse_devtools_active_port(
        &std::fs::read_to_string(profile.join(DEVTOOLS_ACTIVE_PORT_FILE)).ok()?,
    )
}

/// `/json/version` の `webSocketDebuggerUrl` を取得する (まだ公開前なら None)。
fn fetch_devtools_websocket(client: &reqwest::blocking::Client, port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let response = client.get(&url).send().ok()?;
    let body = response.text().ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value
        .get("webSocketDebuggerUrl")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// DevTools の WebSocket URL が、起動したブラウザの Browser エンドポイントを
/// 指しているか検証する。
///
/// - ループバックの `ws://` であること (外部ホストや `wss` は掴まない)。
/// - ポートが期待値 (自動検出したポート / 利用者が指定したポート) と一致する
///   こと。固定ポートを先に使っている別プロセスが返す URL を掴まないため。
/// - パスが `/devtools/browser/...` であること (ページ用ではなく Browser 用)。
/// - 起動したブラウザの通知または `DevToolsActivePort` のパスとも一致すること。
fn verify_devtools_websocket(
    websocket: &str,
    expected_port: u16,
    expected_path: Option<&str>,
) -> std::result::Result<String, String> {
    let parsed = url::Url::parse(websocket)
        .map_err(|error| format!("DevTools の WebSocket URL が不正です: {error}"))?;
    if parsed.scheme() != "ws" {
        return Err("DevTools の WebSocket URL が ws ではありません".to_string());
    }
    let loopback = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !loopback {
        return Err("DevTools の WebSocket がループバックを指していません".to_string());
    }
    if parsed.port() != Some(expected_port) {
        return Err(format!(
            "DevTools の WebSocket ポートが {expected_port} ではありません"
        ));
    }
    if !parsed.path().starts_with(DEVTOOLS_BROWSER_PATH) {
        return Err("DevTools の WebSocket パスがブラウザ用ではありません".to_string());
    }
    if let Some(expected_path) = expected_path
        && parsed.path() != expected_path
    {
        return Err("DevTools の WebSocket パスが起動したブラウザと一致しません".to_string());
    }
    Ok(websocket.to_string())
}

/// Wait for the DevTools endpoint and return its browser websocket URL.
///
/// `port == 0` means the browser picked the port itself: it is read from
/// `<profile>/DevToolsActivePort` (polled with `timeout`), and the endpoint
/// reported by `/json/version` must have that same loopback port and path.
/// An explicit port additionally requires the endpoint announced on the
/// launched child's stderr, so another local server cannot supply a session.
fn wait_for_devtools(
    child: &mut Child,
    endpoint: Option<&Receiver<String>>,
    port: u16,
    profile: &Path,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| error.to_string())?;
    // 自動検出時は DevToolsActivePort の (ポート, パス)。
    let mut discovered: Option<(u16, String)> = None;
    let mut reported: Option<(u16, String)> = None;
    loop {
        // ブラウザが DevTools を公開する前に終了したら、待ち続けない。
        if child.try_wait().ok().flatten().is_some() {
            return Err("ブラウザが DevTools を公開する前に終了しました".to_string());
        }
        if reported.is_none()
            && let Some(endpoint) = endpoint
            && let Ok(websocket) = endpoint.try_recv()
        {
            reported = Some(reported_browser_endpoint(&websocket, port)?);
        }
        let endpoint = if port == 0 {
            if discovered.is_none() {
                discovered = read_devtools_active_port(profile);
            }
            discovered.clone()
        } else {
            reported.clone()
        };
        if let Some((effective, active_path)) = endpoint
            && let Some(websocket) = fetch_devtools_websocket(&client, effective)
        {
            let expected_path = Some(active_path.as_str());
            return verify_devtools_websocket(&websocket, effective, expected_path);
        }
        if Instant::now() >= deadline {
            return Err(if port == 0 {
                format!(
                    "ブラウザの DevTools エンドポイントを特定できませんでした ({} がありません)",
                    profile.join(DEVTOOLS_ACTIVE_PORT_FILE).display()
                )
            } else {
                format!("ブラウザの DevTools ポート {port} に接続できませんでした")
            });
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

type DevToolsSocket = tungstenite::WebSocket<std::net::TcpStream>;

fn connect_devtools(url: &str, timeout: Duration) -> std::result::Result<DevToolsSocket, String> {
    let endpoint = url::Url::parse(url)
        .map_err(|error| format!("DevTools の WebSocket URL が不正です: {error}"))?;
    if endpoint.scheme() != "ws" {
        return Err("DevTools はループバックの ws 接続を要求します".to_string());
    }
    let addresses = match endpoint.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => [Some(ip.into()), None],
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => [Some(ip.into()), None],
        Some(url::Host::Domain("localhost")) => [
            Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Some(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
        ],
        _ => return Err("DevTools がループバックを指していません".to_string()),
    };
    let port = endpoint.port_or_known_default()
        .ok_or_else(|| "DevTools のポートが不正です".to_string())?;
    let mut connection = Err(std::io::Error::from(std::io::ErrorKind::NotConnected));
    for address in addresses.into_iter().flatten() {
        connection = std::net::TcpStream::connect_timeout(&std::net::SocketAddr::new(address, port), timeout);
        if connection.is_ok() {
            break;
        }
    }
    let stream = connection.map_err(|error| format!("DevTools に接続できませんでした: {error}"))?;
    stream.set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| format!("DevTools の通信期限を設定できません: {error}"))?;
    let (socket, _) = tungstenite::client(url, stream)
        .map_err(|error| format!("DevTools の接続を開始できませんでした: {error}"))?;
    Ok(socket)
}

fn read_cookies(
    socket: &mut DevToolsSocket,
    request_id: u64,
) -> std::result::Result<Vec<serde_json::Value>, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() >= deadline {
            return Err("DevTools の応答待ちが上限に達しました".to_string());
        }
        let message = socket
            .read()
            .map_err(|error| format!("DevTools からの受信に失敗しました: {error}"))?;
        let tungstenite::Message::Text(text) = message else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
            continue;
        };
        if value.get("id").and_then(|id| id.as_u64()) != Some(request_id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("DevTools がエラーを返しました: {error}"));
        }
        return Ok(value
            .get_mut("result")
            .and_then(|result| result.get_mut("cookies"))
            .and_then(|cookies| cookies.as_array_mut())
            .map(std::mem::take)
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
        // Cookies for the parent domain (`.pixiv.net` for `www.pixiv.net`) are
        // sent to the site, so they are part of the session. Sessions usually
        // live there, not on the `www` host.
        || same_registrable_domain(domain, &site_domain)
}

/// Whether two hosts belong to the same public-suffix-aware domain family.
fn same_registrable_domain(left: &str, right: &str) -> bool {
    match (psl::domain_str(left), psl::domain_str(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn site(yaml: &str) -> SiteSetting {
        serde_yaml::from_str(yaml).expect("site setting")
    }

    #[test]
    fn capture_keeps_the_domain_family_of_the_site() {
        let site = site("name: pixiv\nsitename: Pixiv\ndomain: www.pixiv.net\ntop_url: https://www.pixiv.net/\ntoc_url: \\k<url>\nnovel_info_url: https://www.pixiv.net/novel/show.php?id=\\k<id>\n");
        let hosts = vec!["www.pixiv.net".to_string()];
        // Session cookies live on `.pixiv.net`, not on the `www` host, so both
        // the parent domain and sibling hosts belong to the capture.
        assert!(belongs_to_site("pixiv.net", &site, &hosts));
        assert!(belongs_to_site("accounts.pixiv.net", &site, &hosts));
        assert!(belongs_to_site("www.pixiv.net", &site, &hosts));
        // Unrelated domains stay out of the stored session.
        assert!(!belongs_to_site("i.pximg.net", &site, &hosts));
        assert!(!belongs_to_site("example.com", &site, &hosts));
        assert!(!belongs_to_site("net", &site, &hosts));
    }

    #[test]
    fn capture_isolates_alphapolis_from_other_co_jp_accounts() {
        let site = SiteSetting::load_bundled(&[
            include_str!("../../webnovel/www.alphapolis.co.jp.yaml"),
        ]).unwrap().remove(0);
        let cookies = serde_json::json!([
            {"domain":".alphapolis.co.jp","name":"session","value":"alpha"},
            {"domain":"www.alphapolis.co.jp","name":"reader","value":"vertical"},
            {"domain":"accounts.alphapolis.co.jp","name":"auth","value":"alpha-auth"},
            {"domain":".unrelated.co.jp","name":"session","value":"must-not-export"},
            {"domain":".co.jp","name":"suffix","value":"must-not-export"}
        ]);
        let actual: BTreeMap<_, _> = cookies_for_site(
            cookies.as_array().unwrap(), &site, &site_hosts(&site, "www.alphapolis.co.jp"),
        ).into_iter().collect();
        assert_eq!(actual, BTreeMap::from([
            ("alphapolis.co.jp".into(), "session=alpha".into()),
            ("www.alphapolis.co.jp".into(), "reader=vertical".into()),
            ("accounts.alphapolis.co.jp".into(), "auth=alpha-auth".into()),
        ]));
    }

    #[test]
    fn devtools_active_port_is_parsed_from_the_browser_file() {
        assert_eq!(
            parse_devtools_active_port("52222\n/devtools/browser/1a2b-3c4d\n"),
            Some((52222, "/devtools/browser/1a2b-3c4d".to_string()))
        );
        // Windows の CRLF と末尾の空行も読める。
        assert_eq!(
            parse_devtools_active_port("9222\r\n/devtools/browser/abc\r\n"),
            Some((9222, "/devtools/browser/abc".to_string()))
        );
        // 書きかけ・壊れた内容・ポート 0・ブラウザ用でないパスは None。
        for text in [
            "",
            "\n",
            "52222",
            "52222\n",
            "abc\n/devtools/browser/x",
            "0\n/devtools/browser/x",
            "70000\n/devtools/browser/x",
            "52222\n/devtools/page/x",
            "52222\n\n",
        ] {
            assert_eq!(parse_devtools_active_port(text), None, "{text:?}");
        }
    }

    #[test]
    fn devtools_websocket_is_verified_against_the_discovered_endpoint() {
        let valid = "ws://127.0.0.1:52222/devtools/browser/1a2b";
        assert_eq!(
            verify_devtools_websocket(valid, 52222, Some("/devtools/browser/1a2b")).unwrap(),
            valid
        );
        // ポート・パスが自動検出した値と違えば掴まない。
        assert!(verify_devtools_websocket(valid, 52223, None).is_err());
        assert!(
            verify_devtools_websocket(valid, 52222, Some("/devtools/browser/other")).is_err()
        );
        for bad in [
            // ループバック以外 (固定ポートを先に使っている別プロセス対策)。
            "ws://example.com:52222/devtools/browser/x",
            "ws://127.0.0.1:52223/devtools/browser/x",
            // ページ用エンドポイントでは Storage.getCookies を読めない。
            "ws://127.0.0.1:52222/devtools/page/x",
            "wss://127.0.0.1:52222/devtools/browser/x",
            "not a url",
        ] {
            assert!(
                verify_devtools_websocket(bad, 52222, None).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn login_target_accepts_parent_domains_and_author_urls_without_crossing_sites() {
        for target in [
            "alphapolis.co.jp",
            "https://www.alphapolis.co.jp/author/detail/166360898",
        ] {
            assert_eq!(resolve_site(target).unwrap().domain, "www.alphapolis.co.jp");
        }
        assert!(resolve_site("https://unrelated.co.jp/").is_err());
        assert!(resolve_site("https://co.jp/").is_err());
    }

    #[test]
    fn browser_check_opens_requested_works_and_authors_without_changing_other_login_pages() {
        let mut site = SiteSetting::load_bundled(&["name: example\nsitename: Example\ndomain: example.com\ntop_url: https://example.com\ntoc_url: \\k<url>\nurl: '^https://example\\.com/story/\\d+$'\nauthor_url: '^https://example\\.com/user/\\d+/(?:profile|story)(?:\\?[^#]*)?$'\nlogin_url: https://example.com/sign-in\n"]).unwrap().remove(0);
        let targets = [
            "https://example.com/story/42",
            "https://example.com/user/7/profile",
            "https://example.com/user/7/story?p=2",
        ];
        for target in targets {
            assert_eq!(browser_start_url(&site, target), "https://example.com/sign-in");
        }
        site.login_wall_error = narou_rs::downloader::site_setting::LoginWallError::Login;
        for target in targets {
            assert_eq!(browser_start_url(&site, target), target);
        }
        assert_eq!(browser_start_url(&site, "example.com"), "https://example.com/sign-in");
        assert_eq!(browser_start_url(&site, "https://unrelated.com/story/42"), "https://example.com/sign-in");
    }

    #[test]
    fn stalled_devtools_does_not_hold_cookie_capture_open() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/devtools/browser/test", listener.local_addr().unwrap());
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket.read().unwrap();
            let _ = stopped.recv();
        });
        let mut socket = connect_devtools(&url, Duration::from_millis(50)).unwrap();
        socket.send(tungstenite::Message::text(
            r#"{"id":1,"method":"Storage.getCookies"}"#,
        )).unwrap();
        let (result, received) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = result.send(read_cookies(&mut socket, 1));
        });
        let outcome = received.recv_timeout(Duration::from_secs(2));
        drop(stop);
        server.join().unwrap();
        reader.join().unwrap();
        assert!(matches!(outcome, Ok(Err(_))), "{outcome:?}");
    }

    #[test]
    fn browser_endpoint_uses_the_owned_announcement_after_oversized_diagnostics() {
        let expected = "ws://127.0.0.1:52222/devtools/browser/owned";
        let mut diagnostics = b"DevTools listening on ws://127.0.0.1:52222/devtools/browser/truncated".to_vec();
        diagnostics.extend(std::iter::repeat_n(b'x', 5000));
        diagnostics.extend_from_slice(b"\n\xffinvalid diagnostic\nDevTools listening on ");
        diagnostics.extend_from_slice(expected.as_bytes());
        diagnostics.extend_from_slice(b"\r\n");
        let endpoint = read_browser_endpoint(&mut std::io::Cursor::new(diagnostics)).unwrap();
        assert_eq!(endpoint, expected);
        let (port, path) = reported_browser_endpoint(&endpoint, 52222).unwrap();
        assert!(verify_devtools_websocket(
            "ws://127.0.0.1:52222/devtools/browser/other-process", port, Some(&path),
        ).is_err(), "a server on the same port must not replace the launched browser");
    }

    #[test]
    fn token_protocol_excludes_accounts_siblings_and_prefix_matches() {
        let cookies = BTreeMap::from([
            ("example.test".to_string(), "account=do-not-export; example-token=opaque==; analytics=tracking".to_string()),
            ("api.example.test".to_string(), "example-token=other-host-secret".to_string()),
            ("www.example.test".to_string(), "example-token-extra=prefix-secret; account=private".to_string()),
        ]);
        let response = browser_token_response(
            "example.test".to_string(), "example-token".to_string(),
            "https://www.example.test/read/1", cookies,
        ).unwrap();
        assert_eq!(response.cookies, BTreeMap::from([
            ("example.test".to_string(), "example-token=opaque==".to_string()),
        ]));
    }
}
