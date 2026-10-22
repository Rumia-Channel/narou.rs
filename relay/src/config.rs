//! 起動設定の解決 (環境変数と `.env`)。
//!
//! 旧 Node 版 `scripts/relay/env.mjs` + `server.mjs` の起動部分の移植。
//! ホストごとの差 (クラウドの `K_SERVICE` / `PORT` と、SORAHOST 等の
//! `SERVER_PORT` / `.env`) はここだけが吸収し、サーバー本体にホスト固有の
//! 分岐を足さない。
//!
//! 環境変数:
//! - `NAROU_RELAY_KEY` (必須): `X-Proxy-Token` と照合する合言葉。
//! - `NAROU_RELAY_ALLOWED_HOSTS`: 許可ホスト (カンマ区切りの完全一致)。
//! - `PORT` → `SERVER_PORT` → 3000。
//! - `NAROU_RELAY_HOST`: 待受アドレス。未指定は `K_SERVICE` があれば
//!   `0.0.0.0`、無ければ `127.0.0.1`。
//! - `NAROU_RELAY_FETCH`: 取得順 (`curl` / `wget` のカンマ区切り、既定
//!   `curl,wget`)。使えない wget は起動時に外す。
//! - `NAROU_RELAY_CURL` / `NAROU_RELAY_WGET`: 実行ファイルの明示指定。
//! - `NAROU_RELAY_CACERT`: 全クライアントに渡す CA バンドル (起動時に検証)。
//! - `NAROU_RELAY_ENV_FILE`: 読み込む `.env` の明示指定。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::security::{normalize_host, TargetError};
use crate::transport::TransportKind;

/// 1 リクエスト (取得先への接続を含む) の期限。Node 版と同じ 75 秒。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(75);
/// リクエストヘッダ受信の期限の上限。Node 版 `headersTimeout` と同じ 10 秒。
pub const DEFAULT_HEADERS_TIMEOUT: Duration = Duration::from_secs(10);
/// curl / wget 1 回あたりの期限。Node 版と同じ 30 秒。
pub const DEFAULT_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// 取得トランスポートの実行ファイルと、その由来 (`env` / `path`)。
#[derive(Debug, Clone)]
pub struct TransportBin {
    pub bin: OsString,
    /// 起動ログの `curl:path` などの表示に使う。
    pub source: &'static str,
}

/// 起動時のトランスポート可用性 (テストで差し替える)。
#[derive(Debug, Clone, Copy)]
pub struct Availability {
    pub wget: bool,
}

impl Default for Availability {
    fn default() -> Self {
        Self { wget: true }
    }
}

/// 読み込んだ `.env` の記録 (値は保持しない)。
#[derive(Debug, Clone)]
pub struct LoadedEnvFile {
    pub path: PathBuf,
    /// `.env` から実際に適用したキー。
    pub applied: Vec<String>,
    /// 読み取りに失敗したか。
    pub unreadable: bool,
}

/// 起動設定。
#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    /// 正規化済みの許可ホスト (空なら全取得拒否)。
    pub allowed_hosts: Vec<String>,
    /// 実際に使う取得順 (使えない wget は含まない)。
    pub fetch_order: Vec<TransportKind>,
    pub curl: TransportBin,
    pub wget: TransportBin,
    /// `NAROU_RELAY_CACERT` (curl / wget に渡す)。
    pub cacert: Option<PathBuf>,
    pub host: String,
    pub port: u16,
    pub request_timeout: Duration,
    pub headers_timeout: Duration,
    pub curl_timeout: Duration,
    pub wget_timeout: Duration,
    /// 要求された取得順から wget を外したか (起動ログ用)。
    pub wget_dropped: bool,
    pub env_loaded: Option<LoadedEnvFile>,
}

impl Config {
    /// `/health` の `fetch` フィールド (`curl,wget` 形式)。
    pub fn fetch_names(&self) -> String {
        self.fetch_order
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 起動ログの `(fetch: ...)` 表示 (`curl:path, wget:path` 形式)。
    pub fn fetch_summary(&self) -> String {
        self.fetch_order
            .iter()
            .map(|kind| {
                let bin = match kind {
                    TransportKind::Curl => &self.curl,
                    TransportKind::Wget => &self.wget,
                };
                format!("{}:{}", kind.name(), bin.source)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 環境変数マップから設定を作る (`.env` 読み込みと wget の疎通確認は
    /// 呼び出し側)。
    pub fn from_env_map(
        env: &BTreeMap<String, String>,
        availability: Availability,
    ) -> Result<Self, ConfigError> {
        let token = env.get("NAROU_RELAY_KEY").cloned().unwrap_or_default();
        if token.is_empty() {
            return Err(ConfigError::MissingToken);
        }
        let mut allowed_hosts = Vec::new();
        for host in value(env, "NAROU_RELAY_ALLOWED_HOSTS")
            .split(',')
            .map(str::trim)
            .filter(|host| !host.is_empty())
        {
            allowed_hosts.push(normalize_host(host).map_err(ConfigError::InvalidHost)?);
        }
        let requested = parse_fetch_names(env)?;
        let (fetch_order, wget_dropped) = resolve_fetch_order(&requested, availability)?;
        let port = resolve_port(env)?;
        let host = resolve_host(env);
        let cacert = {
            let value = value(env, "NAROU_RELAY_CACERT");
            if value.is_empty() {
                None
            } else {
                Some(PathBuf::from(value))
            }
        };
        let request_timeout = DEFAULT_REQUEST_TIMEOUT;
        Ok(Self {
            token,
            allowed_hosts,
            fetch_order,
            curl: resolve_bin(env, "NAROU_RELAY_CURL", "curl"),
            wget: resolve_bin(env, "NAROU_RELAY_WGET", "wget"),
            cacert,
            host,
            port,
            request_timeout,
            headers_timeout: DEFAULT_HEADERS_TIMEOUT.min(request_timeout),
            curl_timeout: DEFAULT_TRANSPORT_TIMEOUT,
            wget_timeout: DEFAULT_TRANSPORT_TIMEOUT,
            wget_dropped,
            env_loaded: None,
        })
    }

    /// プロセスの環境変数と `.env` から設定を作る。wget が要求されていれば
    /// `--version` で実行可能か確認し、だめなら取得順から外す。
    pub async fn from_process_env() -> Result<Self, ConfigError> {
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        let env_loaded = load_env_files(&mut env);
        let requested = parse_fetch_names(&env)?;
        let wget_bin = resolve_bin(&env, "NAROU_RELAY_WGET", "wget");
        let mut availability = Availability { wget: true };
        if requested.iter().any(|name| name == "wget") && !probe_runnable(&wget_bin.bin).await {
            availability.wget = false;
        }
        let mut config = Self::from_env_map(&env, availability)?;
        config.env_loaded = env_loaded;
        Ok(config)
    }
}

/// 起動設定のエラー。表示は旧 Node 版の `narou relay startup failed: ...` の
/// 後ろにそのまま続く。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("NAROU_RELAY_KEY is required")]
    MissingToken,
    #[error("invalid host")]
    InvalidHost(#[source] TargetError),
    #[error("invalid PORT")]
    InvalidPort,
    #[error("invalid NAROU_RELAY_FETCH: {0}")]
    InvalidFetch(String),
    #[error("使用可能な取得トランスポートがありません (NAROU_RELAY_FETCH / NAROU_RELAY_CURL / NAROU_RELAY_WGET または PATH を確認してください)")]
    NoTransport,
}

/// 環境変数の値を取り出す (`String(env.X ?? "")` 相当)。
fn value<'a>(env: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    env.get(name).map(String::as_str).unwrap_or_default()
}

fn trimmed_value<'a>(env: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    value(env, name).trim()
}

/// 取得順の生の名前をパースする。値が空なら既定 `curl,wget`。
pub fn parse_fetch_names(env: &BTreeMap<String, String>) -> Result<Vec<String>, ConfigError> {
    let text = trimmed_value(env, "NAROU_RELAY_FETCH").to_lowercase();
    let mut names = Vec::new();
    if text.is_empty() {
        names.push("curl".to_string());
        names.push("wget".to_string());
    } else {
        for name in text.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            if !matches!(name, "curl" | "wget") {
                return Err(ConfigError::InvalidFetch(name.to_string()));
            }
            if !names.iter().any(|seen| seen == name) {
                names.push(name.to_string());
            }
        }
        if names.is_empty() {
            names.push("curl".to_string());
            names.push("wget".to_string());
        }
    }
    Ok(names)
}

/// 可用性を反映した取得順を作る。1 つも残らなければ起動失敗。
pub fn resolve_fetch_order(
    requested: &[String],
    availability: Availability,
) -> Result<(Vec<TransportKind>, bool), ConfigError> {
    let mut order = Vec::new();
    let mut wget_dropped = false;
    for name in requested {
        match name.as_str() {
            "curl" => order.push(TransportKind::Curl),
            "wget" => {
                if availability.wget {
                    order.push(TransportKind::Wget);
                } else {
                    wget_dropped = true;
                }
            }
            other => return Err(ConfigError::InvalidFetch(other.to_string())),
        }
    }
    if order.is_empty() {
        return Err(ConfigError::NoTransport);
    }
    Ok((order, wget_dropped))
}

fn resolve_bin(env: &BTreeMap<String, String>, name: &str, fallback: &str) -> TransportBin {
    let configured = trimmed_value(env, name);
    if configured.is_empty() {
        TransportBin {
            bin: OsString::from(fallback),
            source: "path",
        }
    } else {
        TransportBin {
            bin: OsString::from(configured),
            source: "env",
        }
    }
}

fn resolve_port(env: &BTreeMap<String, String>) -> Result<u16, ConfigError> {
    for name in ["PORT", "SERVER_PORT"] {
        let value = trimmed_value(env, name);
        if !value.is_empty() {
            return value.parse::<u16>().map_err(|_| ConfigError::InvalidPort);
        }
    }
    Ok(3000)
}

fn resolve_host(env: &BTreeMap<String, String>) -> String {
    let explicit = trimmed_value(env, "NAROU_RELAY_HOST");
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    if !value(env, "K_SERVICE").is_empty() {
        "0.0.0.0".to_string()
    } else {
        "127.0.0.1".to_string()
    }
}

async fn probe_runnable(bin: &OsString) -> bool {
    let mut command = tokio::process::Command::new(bin);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    matches!(
        tokio::time::timeout(Duration::from_secs(5), command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

/// `.env` を読み込む。適用先は既存の環境変数で、空文字は未設定として上書きする。
/// 最初に見つかった 1 ファイルだけを読む。
pub fn load_env_files(env: &mut BTreeMap<String, String>) -> Option<LoadedEnvFile> {
    let explicit = env
        .get("NAROU_RELAY_ENV_FILE")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    let candidates: Vec<PathBuf> = if explicit.is_empty() {
        let mut candidates = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(".env"));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join(".env"));
        }
        if let Some(home) = home_dir() {
            candidates.push(home.join(".env"));
        }
        candidates
    } else {
        vec![PathBuf::from(explicit)]
    };
    for path in candidates {
        if !path.exists() {
            continue;
        }
        return Some(apply_env_file(env, &path));
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn apply_env_file(env: &mut BTreeMap<String, String>, path: &std::path::Path) -> LoadedEnvFile {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => {
            return LoadedEnvFile {
                path: path.to_path_buf(),
                applied: Vec::new(),
                unreadable: true,
            };
        }
    };
    let mut applied = Vec::new();
    for (key, value) in parse_env_file(&text) {
        let already_set = env.get(&key).is_some_and(|existing| !existing.is_empty());
        if already_set {
            continue;
        }
        env.insert(key.clone(), value);
        applied.push(key);
    }
    LoadedEnvFile {
        path: path.to_path_buf(),
        applied,
        unreadable: false,
    }
}

/// `KEY=VALUE` の行だけを読む。値は前後の空白をそのまま保持する。
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut values = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(separator) = line.find('=') else {
            continue;
        };
        if separator == 0 {
            continue;
        }
        let key = &line[..separator];
        if !is_env_key(key) {
            continue;
        }
        values.push((key.to_string(), line[separator + 1..].to_string()));
    }
    values
}

fn is_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    match bytes.next() {
        Some(byte) if byte.is_ascii_alphabetic() || byte == b'_' => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_env() -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("NAROU_RELAY_KEY".to_string(), "secret".to_string());
        env
    }

    fn env_with(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut env = base_env();
        for (key, value) in pairs {
            env.insert((*key).to_string(), (*value).to_string());
        }
        env
    }

    #[test]
    fn key_is_required_and_not_trimmed() {
        assert!(matches!(
            Config::from_env_map(&BTreeMap::new(), Availability::default()),
            Err(ConfigError::MissingToken)
        ));
        let env = env_with(&[("NAROU_RELAY_KEY", "  spaced  ")]);
        let config = Config::from_env_map(&env, Availability::default()).unwrap();
        assert_eq!(config.token, "  spaced  ");
    }

    #[test]
    fn port_prefers_port_then_server_port_then_default() {
        assert_eq!(
            Config::from_env_map(&env_with(&[("PORT", "0")]), Availability::default())
                .unwrap()
                .port,
            0
        );
        assert_eq!(
            Config::from_env_map(
                &env_with(&[("PORT", ""), ("SERVER_PORT", "3100")]),
                Availability::default()
            )
            .unwrap()
            .port,
            3100
        );
        assert_eq!(
            Config::from_env_map(&base_env(), Availability::default())
                .unwrap()
                .port,
            3000
        );
        for bad in ["abc", "-1", "65536", "1.5"] {
            assert!(matches!(
                Config::from_env_map(&env_with(&[("PORT", bad)]), Availability::default()),
                Err(ConfigError::InvalidPort)
            ));
        }
    }

    #[test]
    fn listener_host_defaults_to_loopback_unless_k_service_is_set() {
        assert_eq!(
            Config::from_env_map(&base_env(), Availability::default())
                .unwrap()
                .host,
            "127.0.0.1"
        );
        assert_eq!(
            Config::from_env_map(&env_with(&[("K_SERVICE", "narou-relay")]), Availability::default())
                .unwrap()
                .host,
            "0.0.0.0"
        );
        assert_eq!(
            Config::from_env_map(
                &env_with(&[("K_SERVICE", "narou-relay"), ("NAROU_RELAY_HOST", "192.0.2.10")]),
                Availability::default()
            )
            .unwrap()
            .host,
            "192.0.2.10"
        );
    }

    #[test]
    fn fetch_order_parsing_dedupes_and_rejects_unknown_names() {
        assert_eq!(
            parse_fetch_names(&base_env()).unwrap(),
            vec!["curl", "wget"]
        );
        assert_eq!(
            parse_fetch_names(&env_with(&[("NAROU_RELAY_FETCH", " WGET, curl ,wget")])).unwrap(),
            vec!["wget", "curl"]
        );
        assert_eq!(
            parse_fetch_names(&env_with(&[("NAROU_RELAY_FETCH", " , ")])).unwrap(),
            vec!["curl", "wget"]
        );
        assert!(matches!(
            parse_fetch_names(&env_with(&[("NAROU_RELAY_FETCH", "curl,node")])),
            Err(ConfigError::InvalidFetch(name)) if name == "node"
        ));
    }

    #[test]
    fn unavailable_wget_is_dropped_from_the_order() {
        let requested = vec!["wget".to_string(), "curl".to_string()];
        let (order, dropped) =
            resolve_fetch_order(&requested, Availability { wget: true }).unwrap();
        assert_eq!(order, vec![TransportKind::Wget, TransportKind::Curl]);
        assert!(!dropped);
        let (order, dropped) = resolve_fetch_order(&requested, Availability { wget: false }).unwrap();
        assert_eq!(order, vec![TransportKind::Curl]);
        assert!(dropped);
        assert!(matches!(
            resolve_fetch_order(&["wget".to_string()], Availability { wget: false }),
            Err(ConfigError::NoTransport)
        ));
    }

    #[test]
    fn binaries_and_cacert_come_from_env_or_path() {
        let config = Config::from_env_map(&base_env(), Availability::default()).unwrap();
        assert_eq!(config.curl.bin, OsString::from("curl"));
        assert_eq!(config.curl.source, "path");
        assert_eq!(config.wget.bin, OsString::from("wget"));
        let config = Config::from_env_map(
            &env_with(&[
                ("NAROU_RELAY_CURL", "/opt/bin/curl"),
                ("NAROU_RELAY_WGET", "/opt/bin/wget"),
                ("NAROU_RELAY_CACERT", "/etc/ssl/certs/ca-certificates.crt"),
            ]),
            Availability::default(),
        )
        .unwrap();
        assert_eq!(config.curl.bin, OsString::from("/opt/bin/curl"));
        assert_eq!(config.curl.source, "env");
        assert_eq!(config.wget.source, "env");
        assert_eq!(
            config.cacert.as_deref(),
            Some(std::path::Path::new("/etc/ssl/certs/ca-certificates.crt"))
        );
        assert_eq!(config.fetch_summary(), "curl:env, wget:env");
        assert_eq!(config.fetch_names(), "curl,wget");
    }

    #[test]
    fn allowed_hosts_are_normalized_and_invalid_entries_fail_startup() {
        let config = Config::from_env_map(
            &env_with(&[("NAROU_RELAY_ALLOWED_HOSTS", " BÜCHER.example.,syosetu.org ")]),
            Availability::default(),
        )
        .unwrap();
        assert_eq!(
            config.allowed_hosts,
            vec!["xn--bcher-kva.example", "syosetu.org"]
        );
        for bad in ["*.example.com", "example.com:443", "https://example.com", "example..com"] {
            assert!(matches!(
                Config::from_env_map(&env_with(&[("NAROU_RELAY_ALLOWED_HOSTS", bad)]), Availability::default()),
                Err(ConfigError::InvalidHost(_))
            ));
        }
        let config = Config::from_env_map(&base_env(), Availability::default()).unwrap();
        assert!(config.allowed_hosts.is_empty());
    }

    #[test]
    fn env_file_parser_matches_key_value_rules() {
        let parsed = parse_env_file(
            "# comment\nNAROU_RELAY_KEY= secret \n\nBAD KEY=1\n1BAD=x\nEMPTY=\nEQ=a=b\n",
        );
        assert_eq!(
            parsed,
            vec![
                ("NAROU_RELAY_KEY".to_string(), " secret ".to_string()),
                ("EMPTY".to_string(), String::new()),
                ("EQ".to_string(), "a=b".to_string()),
            ]
        );
    }

    /// テスト用の一時ファイル置き場 (`target/` 配下。gitignore 済み)。
    fn unique_test_file(name: &str) -> PathBuf {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("target")
            .join("narou-relay-tests");
        std::fs::create_dir_all(&directory).expect("test temp dir");
        directory.join(format!(
            "{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn env_file_only_fills_unset_or_empty_values() {
        let mut env = BTreeMap::new();
        env.insert("A".to_string(), "kept".to_string());
        env.insert("B".to_string(), String::new());
        env.insert("C".to_string(), "platform".to_string());
        let path = unique_test_file("env-file");
        std::fs::write(&path, "A=file\nB=file\nC=file\n").unwrap();
        let loaded = apply_env_file(&mut env, &path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(env["A"], "kept");
        assert_eq!(env["B"], "file");
        assert_eq!(env["C"], "platform");
        assert_eq!(loaded.applied, vec!["B".to_string()]);
        assert!(!loaded.unreadable);
    }

    #[tokio::test]
    async fn from_process_env_reads_an_explicit_env_file() {
        let path = unique_test_file("env-explicit").with_extension("env");
        std::fs::write(&path, "NAROU_RELAY_KEY=from-file\nNAROU_RELAY_ALLOWED_HOSTS=example.com\n").unwrap();
        // 安全のため、このテストだけ子プロセスではなく読み込み関数を直接検査する。
        let mut env = BTreeMap::new();
        env.insert(
            "NAROU_RELAY_ENV_FILE".to_string(),
            path.display().to_string(),
        );
        let loaded = load_env_files(&mut env);
        let _ = std::fs::remove_file(&path);
        let loaded = loaded.expect("env file");
        assert_eq!(env["NAROU_RELAY_KEY"], "from-file");
        assert_eq!(env["NAROU_RELAY_ALLOWED_HOSTS"], "example.com");
        assert_eq!(
            loaded.applied,
            vec!["NAROU_RELAY_KEY".to_string(), "NAROU_RELAY_ALLOWED_HOSTS".to_string()]
        );
    }
}
