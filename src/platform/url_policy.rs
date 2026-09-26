//! URL の安全性検証のうち、DNS を見なくても決まる部分。
//!
//! DNS 解決を持つプラットフォーム (native) は、この構文検証に加えて解決先
//! アドレスの判定まで行う (`downloader::security::validate_public_url` と
//! `NativeHttpClient`)。
//!
//! 解決手段を持たない wasm (Cloudflare Workers) では構文検証だけで判断し、
//! 実到達性はプラットフォームの egress 側に委ねる。判定基準を 1 箇所に置く
//! ため、`HttpClient::validate_url` の既定実装もここを使う。

use std::net::IpAddr;

/// 構文と、ホストがアドレスリテラルの場合の判定だけを行う。
///
/// ホスト名 (ドメイン) は解決しないと公開アドレスかどうか決まらないため、
/// ここでは通す。呼び出し側は解決できるなら
/// [`super::super::downloader::security::validate_resolved_addresses`] で
/// 解決先を確認する。
pub fn validate_url_syntax(url: &str) -> std::result::Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("unsupported URL scheme: {}", parsed.scheme()));
    }

    match parsed.host() {
        Some(url::Host::Ipv4(address)) => check_address(IpAddr::V4(address)),
        Some(url::Host::Ipv6(address)) => check_address(IpAddr::V6(address)),
        Some(url::Host::Domain(name)) => {
            if name.is_empty() {
                return Err("URL host is empty".to_string());
            }
            if parsed.port_or_known_default().is_none() {
                return Err("URL port is missing".to_string());
            }
            Ok(())
        }
        None => Err("URL host is missing".to_string()),
    }
}

fn check_address(address: IpAddr) -> std::result::Result<(), String> {
    if is_safe_public_ip(address) {
        Ok(())
    } else {
        Err(format!("URL host is not a public address: {address}"))
    }
}

/// 公開アドレスとして扱ってよいか (ループバック・私有・リンクローカル・
/// 予約域を除く)。
pub fn is_safe_public_ip(ip: IpAddr) -> bool {
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
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn accepts_public_http_urls() {
        assert!(validate_url_syntax("https://example.com/novel/1").is_ok());
        assert!(validate_url_syntax("http://8.8.8.8/path").is_ok());
        assert!(validate_url_syntax("https://1.1.1.1/image.jpg").is_ok());
        assert!(validate_url_syntax("https://[2606:4700:4700::1111]/").is_ok());
        // ホスト名は解決しないと判定できないので通す。
        assert!(validate_url_syntax("https://internal.example/").is_ok());
    }

    #[test]
    fn rejects_unsupported_schemes_and_shapes() {
        assert!(validate_url_syntax("ftp://1.1.1.1/file").is_err());
        assert!(validate_url_syntax("file:///etc/passwd").is_err());
        assert!(validate_url_syntax("https://").is_err());
        assert!(validate_url_syntax("not a url").is_err());
    }

    #[test]
    fn rejects_non_public_address_literals() {
        for url in [
            "http://127.0.0.1/test",
            "http://10.0.0.1/test",
            "http://192.168.0.1/test",
            "http://169.254.1.10/test",
            "http://172.16.5.4/test",
            "http://100.64.0.1/test",
            "http://[::1]/test",
            "http://[fe80::1]/test",
            "http://[::ffff:192.168.0.1]/test",
        ] {
            assert!(
                validate_url_syntax(url).is_err(),
                "should be rejected: {url}"
            );
        }
    }

    #[test]
    fn classifies_addresses() {
        assert!(is_safe_public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_safe_public_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_safe_public_ip(IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111
        ))));
        assert!(!is_safe_public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
}
