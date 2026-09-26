use std::net::{IpAddr, ToSocketAddrs};

use crate::platform::url_policy::{is_safe_public_ip, validate_url_syntax};

pub const CONNECT_TIMEOUT_SECS: u64 = 30;
pub const READ_TIMEOUT_SECS: u64 = 60;
pub const TOTAL_TIMEOUT_SECS: u64 = 120;
pub const MAX_REDIRECTS: usize = 10;
pub const MAX_RESPONSE_BYTES: usize = 50_000_000;
pub const MAX_YAML_REGEX_PATTERN_LEN: usize = 4 * 1024;
pub const MAX_REGEX_INPUT_LEN: usize = 8 * 1024 * 1024;

pub fn is_safe_header_value(value: &str) -> bool {
    !value.bytes().any(|byte| byte.is_ascii_control())
}

/// DNS まで見る検証。解決手段を持つ呼び出し側 (native トランスポート) 用。
pub fn is_safe_public_url(url: &str) -> bool {
    validate_public_url(url).is_ok()
}

/// 構文とアドレスリテラルだけの検証 (同期文脈の足切り用)。
///
/// ホスト名の解決先は見ない。実際の取得時には `HttpClient::validate_url` が
/// 解決先まで確認する。
pub fn is_safe_public_url_syntax(url: &str) -> bool {
    validate_url_syntax(url).is_ok()
}

/// 構文検証に加えてホストを解決し、解決先が公開アドレスであることを確認する。
pub fn validate_public_url(url: &str) -> std::result::Result<(), String> {
    validate_url_syntax(url)?;
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL host is missing".to_string())?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "URL port is missing".to_string())?;

    let resolved: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolve failed for {host}: {e}"))?
        .map(|addr| addr.ip())
        .collect();
    if resolved.is_empty() {
        return Err(format!("DNS resolve returned no addresses for {host}"));
    }
    validate_resolved_addresses(&resolved)
}

/// 解決済みアドレスの検証 (自前で解決した呼び出し側・テスト用)。
pub fn validate_resolved_addresses(addresses: &[IpAddr]) -> std::result::Result<(), String> {
    for ip in addresses {
        if !is_safe_public_ip(*ip) {
            return Err(format!("resolved to non-public address {ip}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_safe_header_value, is_safe_public_url, is_safe_public_url_syntax,
        validate_resolved_addresses,
    };

    #[test]
    fn safe_public_url_accepts_public_http_and_https() {
        assert!(is_safe_public_url("https://1.1.1.1/image.jpg"));
        assert!(is_safe_public_url("http://8.8.8.8/path"));
    }

    #[test]
    fn safe_public_url_rejects_unsupported_schemes() {
        assert!(!is_safe_public_url("ftp://1.1.1.1/file"));
    }

    #[test]
    fn safe_public_url_rejects_private_and_loopback_addresses() {
        assert!(!is_safe_public_url("http://127.0.0.1/test"));
        assert!(!is_safe_public_url("http://10.0.0.1/test"));
        assert!(!is_safe_public_url("http://192.168.0.1/test"));
        assert!(!is_safe_public_url("http://169.254.1.10/test"));
        assert!(!is_safe_public_url("http://[::1]/test"));
        assert!(!is_safe_public_url("http://[fe80::1]/test"));
        assert!(!is_safe_public_url("http://[::ffff:192.168.0.1]/test"));
    }

    #[test]
    fn syntax_check_accepts_domains_and_rejects_non_public_literals() {
        // ホスト名は解決しないと判定できないので通す (取得時に transport が確認)。
        assert!(is_safe_public_url_syntax("https://example.com/novel"));
        assert!(is_safe_public_url_syntax("http://8.8.8.8/path"));
        assert!(!is_safe_public_url_syntax("http://127.0.0.1/test"));
        assert!(!is_safe_public_url_syntax("ftp://example.com/"));
    }

    #[test]
    fn resolved_addresses_must_be_public() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert!(validate_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]).is_ok());
        assert!(
            validate_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), IpAddr::V6(Ipv6Addr::LOCALHOST)])
                .is_err()
        );
    }

    #[test]
    fn safe_header_value_rejects_control_characters() {
        assert!(is_safe_header_value("session=abc123"));
        assert!(!is_safe_header_value("session=abc\r\nX-Test: 1"));
        assert!(!is_safe_header_value("session=\0"));
    }
}
