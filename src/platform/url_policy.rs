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
///
/// IPv6 は IPv4 を埋め込む移行機構の形式 (IPv4-mapped/compatible/translated,
/// NAT64, 6to4, Teredo, ISATAP) を先に展開し、埋め込み IPv4 に IPv4 と同じ
/// 判定を適用する。`::127.0.0.1` や `64:ff9b::7f00:1` のような表記違いの
/// ループバック・私有アドレスを素通りさせないため。
pub fn is_safe_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_safe_public_ipv4(ipv4.octets()),
        IpAddr::V6(ipv6) => is_safe_public_ipv6(ipv6.segments()),
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

/// IPv4 を埋め込む移行機構の形式から埋め込み IPv4 を取り出す。
///
/// - IPv4-compatible `::a.b.c.d` (`::/96`)
/// - IPv4-mapped `::ffff:a.b.c.d` (`::ffff:0.0.0.0/96`)
/// - IPv4-translated `::ffff:0:a.b.c.d` (`::ffff:0:0/96`)
/// - NAT64 `64:ff9b::a.b.c.d` (`64:ff9b::/96`)
/// - 6to4 `2002:a.b.c.d::` (`2002::/16`、上位 32bit に埋め込む)
/// - ISATAP `…::{0,200}:5efe:a.b.c.d` (インターフェース ID に埋め込む)
fn embedded_ipv4(segments: [u16; 8]) -> Option<[u8; 4]> {
    let [s0, s1, s2, s3, s4, s5, s6, s7] = segments;
    if s0 == 0 && s1 == 0 && s2 == 0 && s3 == 0 {
        let compatible = s4 == 0 && s5 == 0;
        let mapped = s4 == 0 && s5 == 0xffff;
        let translated = s4 == 0xffff && s5 == 0;
        if compatible || mapped || translated {
            return Some([
                (s6 >> 8) as u8,
                s6 as u8,
                (s7 >> 8) as u8,
                s7 as u8,
            ]);
        }
    }
    if s0 == 0x0064 && s1 == 0xff9b && s2 == 0 && s3 == 0 && s4 == 0 && s5 == 0 {
        return Some([(s6 >> 8) as u8, s6 as u8, (s7 >> 8) as u8, s7 as u8]);
    }
    if s0 == 0x2002 {
        return Some([(s1 >> 8) as u8, s1 as u8, (s2 >> 8) as u8, s2 as u8]);
    }
    if s5 == 0x5efe && (s4 == 0x0000 || s4 == 0x0200) {
        return Some([(s6 >> 8) as u8, s6 as u8, (s7 >> 8) as u8, s7 as u8]);
    }
    None
}

fn is_safe_public_ipv6(segments: [u16; 8]) -> bool {
    let [s0, s1, _, _, _, _, s6, s7] = segments;
    // Teredo (`2001::/32`) はサーバ IPv4 (seg2-3) とクライアント IPv4
    // (seg6-7 を ~0xffff で XOR) を埋め込む。どちらかが非公開なら拒否する。
    // ISATAP の seg5=0x5efe 判定より先に評価する (Teredo のポートフィールドが
    // 偶然一致しうるため)。
    if s0 == 0x2001 && s1 == 0 {
        let server_v4 = [
            (segments[2] >> 8) as u8,
            segments[2] as u8,
            (segments[3] >> 8) as u8,
            segments[3] as u8,
        ];
        let client_v4 = [
            ((s6 ^ 0xffff) >> 8) as u8,
            (s6 ^ 0xffff) as u8,
            ((s7 ^ 0xffff) >> 8) as u8,
            (s7 ^ 0xffff) as u8,
        ];
        return is_safe_public_ipv4(server_v4) && is_safe_public_ipv4(client_v4);
    }

    if let Some(v4) = embedded_ipv4(segments) {
        return is_safe_public_ipv4(v4);
    }

    if segments == [0, 0, 0, 0, 0, 0, 0, 1] {
        return false;
    }

    let first = segments[0];
    if (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xff00) == 0xff00
        || (first == 0x2001 && segments[1] == 0x0db8)
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
            // IPv4 を埋め込む移行機構の表記違いも拒否する。
            "http://[::127.0.0.1]/test",       // IPv4-compatible
            "http://[::ffff:0:127.0.0.1]/test", // IPv4-translated
            "http://[64:ff9b::7f00:1]/test",   // NAT64
            "http://[2002:7f00:1::]/test",     // 6to4
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
        assert!(is_safe_public_ip(
            "2001:4860:4860::8888".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(!is_safe_public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        // IPv4 を埋め込む移行機構: 埋め込み先が非公開なら拒否する。
        for text in [
            "::127.0.0.1",          // IPv4-compatible
            "::ffff:0:127.0.0.1",   // IPv4-translated
            "64:ff9b::7f00:1",      // NAT64 (well-known prefix)
            "64:ff9b::ac10:1",      // NAT64 → 172.16.0.1
            "2002:7f00:1::",        // 6to4 → 127.0.0.1
            "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredo → ~client IPv4
            "2001:db8::5efe:7f00:1", // ISATAP → 127.0.0.1
        ] {
            let ip: Ipv6Addr = text.parse().unwrap();
            assert!(
                !is_safe_public_ip(IpAddr::V6(ip)),
                "should be rejected: {text}"
            );
        }
        // 公開 IPv4 を埋め込む形は許可する。
        for text in [
            "::ffff:8.8.8.8",
            "::8.8.8.8",
            "64:ff9b::8.8.8.8",
            "2002:808:808::",
        ] {
            let ip: Ipv6Addr = text.parse().unwrap();
            assert!(
                is_safe_public_ip(IpAddr::V6(ip)),
                "should be accepted: {text}"
            );
        }
    }
}
