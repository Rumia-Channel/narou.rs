use std::net::{IpAddr, ToSocketAddrs};

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

pub fn is_safe_public_url(url: &str) -> bool {
    validate_public_url(url).is_ok()
}

pub fn validate_public_url(url: &str) -> std::result::Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("unsupported URL scheme: {}", parsed.scheme()));
    }

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

    for ip in &resolved {
        if !is_safe_public_ip(*ip) {
            return Err(format!("resolved to non-public address {ip}"));
        }
    }

    Ok(())
}

fn is_safe_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_safe_public_ipv4(ipv4.octets()),
        IpAddr::V6(ipv6) => ipv6
            .to_ipv4_mapped()
            .map(|mapped| is_safe_public_ipv4(mapped.octets()))
            .unwrap_or_else(|| is_safe_public_ipv6(ipv6.segments())),
    }
}

fn is_safe_public_ipv4(octets: [u8; 4]) -> bool {
    let [a, b, c, d] = octets;
    if a == 0
        || a == 10
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (224..=239).contains(&a)
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 18)
        || (a == 198 && b == 19)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (a == 255 && b == 255 && c == 255 && d == 255)
    {
        return false;
    }
    true
}

fn is_safe_public_ipv6(segments: [u16; 8]) -> bool {
    if segments == [0, 0, 0, 0, 0, 0, 0, 1] {
        return false;
    }

    let first = segments[0];
    if (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xff00) == 0xff00
        || (first == 0x2001 && segments[1] == 0x0db8)
        || segments == [0; 8]
    {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::{is_safe_header_value, is_safe_public_url};

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
    fn safe_header_value_rejects_control_characters() {
        assert!(is_safe_header_value("session=abc123"));
        assert!(!is_safe_header_value("session=abc\r\nX-Test: 1"));
        assert!(!is_safe_header_value("session=\0"));
    }
}
