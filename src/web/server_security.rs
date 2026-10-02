//! `server-*` 系グローバル設定 (`server-add-accepted-hosts` /
//! `server-ws-add-accepted-domains` / `server-reverse-proxy.enable` /
//! `server-basic-auth.*`) から導出されるランタイム値。
//!
//! これらの値は従来サーバ起動時に一度だけ計算され `AppState` / `PushServer`
//! に固定されていた。`AppState::server_security` が `RwLock` で保持する
//! ようになったため、`ServerSecurity::load` で現在の `global_setting` から
//! 再計算して `AppState::apply_server_security` に渡せば稼働中にも反映できる。
//! 保存 API (`POST /api/global_setting`) は `SettingsEffect::ServerSecurityChanged`
//! を受けて即時再適用し、CLI / 手編集による変更は commands 側の
//! 設定ウォッチャー (30 秒ポーリング) が拾う。

use std::collections::HashMap;

use serde_yaml::Value;

use crate::db::inventory::Inventory;
use crate::db::settings as settings_store;
use crate::setting_core::SettingScope;

use super::AppState;

/// `global_setting` から導出したサーバセキュリティ設定のスナップショット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSecurity {
    /// `Authorization` ヘッダと照合する `Basic ...` 文字列。未設定/不完全なら `None`。
    pub basic_auth_header: Option<String>,
    /// HTTP `Host` / `Origin` ヘッダに許可するホスト一覧。
    /// `reverse_proxy_mode` 時は空 (リクエスト側で forwarded host 判定に切替)。
    pub allowed_request_hosts: Vec<String>,
    /// WebSocket の Origin 判定で許可するドメイン一覧。`reverse_proxy_mode` 時は空。
    pub accepted_ws_domains: Vec<String>,
    /// `server-reverse-proxy.enable`。
    pub reverse_proxy_mode: bool,
    /// `server-basic-auth.require-for-external-bind`。起動時ガード専用で、
    /// 稼働中のリクエスト評価には使われない (再適用しても挙動は変わらない)。
    pub require_basic_auth_for_external_bind: bool,
}

impl ServerSecurity {
    /// 現在の `global_setting` から値を再計算する。
    pub fn load(bind_host: &str) -> Result<Self, String> {
        let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
        let global_setting: HashMap<String, Value> =
            settings_store::load_with_inventory(&inventory, SettingScope::Global)
                .unwrap_or_default();
        Ok(Self::from_settings(&global_setting, bind_host))
    }

    /// 与えられた `global_setting` マップから値を導出する。
    pub fn from_settings(
        global_setting: &HashMap<String, Value>,
        bind_host: &str,
    ) -> Self {
        let reverse_proxy_mode = yaml_bool(global_setting.get("server-reverse-proxy.enable"))
            .unwrap_or(false);
        Self {
            basic_auth_header: basic_auth_header_from_settings(global_setting),
            allowed_request_hosts: http_allowed_request_hosts(
                global_setting,
                bind_host,
                reverse_proxy_mode,
            ),
            accepted_ws_domains: ws_accepted_domains(
                global_setting,
                bind_host,
                reverse_proxy_mode,
            ),
            reverse_proxy_mode,
            require_basic_auth_for_external_bind: yaml_bool(
                global_setting.get("server-basic-auth.require-for-external-bind"),
            )
            .unwrap_or(true),
        }
    }
}

impl AppState {
    /// 計算済みのセキュリティ設定を稼働中の `AppState` / `PushServer` に
    /// 適用する。以後のリクエストから新しい値が使われる。
    pub fn apply_server_security(&self, security: &ServerSecurity) {
        self.push_server
            .set_accepted_domains(security.accepted_ws_domains.clone());
        *self.server_security.write() = security.clone();
    }

    /// `global_setting` を読み直してセキュリティ設定を再適用する。
    ///
    /// `POST /api/global_setting` の保存フック (`ServerSecurityChanged`) と、
    /// 外部編集を拾う設定ウォッチャーから呼ばれる。
    pub fn reload_server_security(&self) -> Result<ServerSecurity, String> {
        let security = ServerSecurity::load(&self.bind_host)?;
        self.apply_server_security(&security);
        Ok(security)
    }
}

fn basic_auth_header_from_settings(global_setting: &HashMap<String, Value>) -> Option<String> {
    use base64::Engine as _;

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
    let token = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", user, password).as_bytes());
    Some(format!("Basic {}", token))
}

/// `server-ws-add-accepted-domains` を既定の許可ドメインに追加した一覧。
/// reverse proxy モードでは WS 側も forwarded 判定に任せるため空を返す。
fn ws_accepted_domains(
    global_setting: &HashMap<String, Value>,
    host: &str,
    reverse_proxy_mode: bool,
) -> Vec<String> {
    if reverse_proxy_mode {
        return Vec::new();
    }
    let mut accepted_domains = super::default_allowed_request_hosts(host);
    if let Some(extra) = yaml_string(global_setting.get("server-ws-add-accepted-domains")) {
        accepted_domains.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|domain| !domain.is_empty())
                .map(ToString::to_string),
        );
    }
    accepted_domains
}

/// HTTP `Host` ヘッダ許可リスト: `default_allowed_request_hosts` に
/// `server-add-accepted-hosts` (カンマ区切り) を足したもの。
/// `*.example.com` 形式のワイルドカードはここではそのまま保持し、
/// リクエスト時に [`super::is_safe_wildcard_pattern`] で検証する。
/// 安全でないパターンは警告を出して除外する (サーバ自体は起動させる)。
fn http_allowed_request_hosts(
    global_setting: &HashMap<String, Value>,
    host: &str,
    reverse_proxy_mode: bool,
) -> Vec<String> {
    if reverse_proxy_mode {
        return Vec::new();
    }
    let mut allowed = super::default_allowed_request_hosts(host);
    if let Some(extra) = yaml_string(global_setting.get("server-add-accepted-hosts")) {
        allowed.extend(parse_extra_allowed_hosts(&extra));
    }
    allowed.sort();
    allowed.dedup();
    allowed
}

/// `server-add-accepted-hosts` の値 (カンマ区切り) をパースする。
/// 裸の `*` や `*.com`、末尾ワイルドカードなどの危険なパターンは除外される。
fn parse_extra_allowed_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| {
            if s.contains('*') && !super::is_safe_wildcard_pattern(s) {
                tracing::warn!(
                    "server-add-accepted-hosts: '{}' は安全なワイルドカードパターンではないため無視します",
                    s
                );
                false
            } else {
                true
            }
        })
        .map(ToString::to_string)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn external_bind_auth_guard_defaults_to_enabled() {
        let security = ServerSecurity::from_settings(&HashMap::new(), "127.0.0.1");
        assert!(security.require_basic_auth_for_external_bind);
    }

    #[test]
    fn external_bind_auth_guard_can_be_disabled() {
        let settings = settings(&[(
            "server-basic-auth.require-for-external-bind",
            Value::Bool(false),
        )]);
        let security = ServerSecurity::from_settings(&settings, "127.0.0.1");
        assert!(!security.require_basic_auth_for_external_bind);
    }

    #[test]
    fn reverse_proxy_mode_clears_host_lists() {
        let settings = settings(&[
            ("server-reverse-proxy.enable", Value::Bool(true)),
            (
                "server-add-accepted-hosts",
                Value::String("extra.example.com".to_string()),
            ),
        ]);
        let security = ServerSecurity::from_settings(&settings, "127.0.0.1");
        assert!(security.reverse_proxy_mode);
        assert!(security.allowed_request_hosts.is_empty());
        assert!(security.accepted_ws_domains.is_empty());
    }

    #[test]
    fn basic_auth_header_uses_setting_values() {
        let settings = settings(&[
            ("server-basic-auth.enable", Value::Bool(true)),
            ("server-basic-auth.user", Value::String("user".to_string())),
            (
                "server-basic-auth.password",
                Value::String("pass".to_string()),
            ),
        ]);
        let security = ServerSecurity::from_settings(&settings, "127.0.0.1");
        assert_eq!(
            security.basic_auth_header.as_deref(),
            Some("Basic dXNlcjpwYXNz")
        );
    }

    #[test]
    fn basic_auth_header_is_none_when_incomplete() {
        let settings = settings(&[
            ("server-basic-auth.enable", Value::Bool(true)),
            ("server-basic-auth.user", Value::String("user".to_string())),
        ]);
        let security = ServerSecurity::from_settings(&settings, "127.0.0.1");
        assert!(security.basic_auth_header.is_none());
    }

    #[test]
    fn wildcard_bind_defaults_do_not_accept_arbitrary_ws_domains() {
        let security = ServerSecurity::from_settings(&HashMap::new(), "0.0.0.0");
        assert!(
            security
                .accepted_ws_domains
                .contains(&"127.0.0.1".to_string())
        );
        assert!(!security.accepted_ws_domains.iter().any(|d| d == "*"));
    }

    #[test]
    fn extra_allowed_hosts_split_trim_and_dedup() {
        let settings = settings(&[(
            "server-add-accepted-hosts",
            Value::String("narou.example.com, *.lan.example ,,  , foo".to_string()),
        )]);
        let security = ServerSecurity::from_settings(&settings, "127.0.0.1");
        for expected in ["narou.example.com", "*.lan.example", "foo"] {
            assert!(
                security
                    .allowed_request_hosts
                    .iter()
                    .any(|h| h == expected),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn parse_extra_allowed_hosts_drops_unsafe_wildcards() {
        // Unsafe patterns: bare *, *.com (too few labels), trailing wildcard, mid wildcard
        let parsed =
            parse_extra_allowed_hosts("*, *.com, *.example.com, example.com*, sub*.example.com");
        assert_eq!(parsed, vec!["*.example.com".to_string()]);
    }

    #[test]
    fn parse_extra_allowed_hosts_drops_empty_input() {
        assert!(parse_extra_allowed_hosts("").is_empty());
        assert!(parse_extra_allowed_hosts("  , , ").is_empty());
    }
}
