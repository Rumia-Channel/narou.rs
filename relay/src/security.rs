//! 取得先の検証 (SSRF 防止)。
//!
//! 旧 Node 版 `scripts/relay/security.mjs` の移植。許可ホストの完全一致、
//! HTTP(S) 以外・資格情報つき URL の拒否、DNS の全応答の公開アドレス検査、
//! 検証済みアドレス 1 個への接続固定、という順序を変えないこと。
//! 解決結果の [`Target`] は必ず検証済みの [`Target::address`] を持ち、
//! トランスポートはそのアドレスへしか接続しない。

use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::sync::Arc;

use url::Url;

/// 検証済みの取得先。`url` は正規化済み、`address` は公開アドレス検査を通った
/// 接続先 (DNS 解決結果の 1 個)。
#[derive(Debug, Clone)]
pub struct Target {
    pub url: Url,
    pub address: IpAddr,
}

/// 取得先の拒否理由。`status` は踏み台が返す HTTP ステータス。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct TargetError {
    pub status: u16,
    pub message: &'static str,
}

impl TargetError {
    pub fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }
}

type ResolveFuture<'a> = Pin<Box<dyn Future<Output = Result<Target, TargetError>> + Send + 'a>>;

/// 取得先 URL を検証・解決する。テストは localhost を許可する実装を注入する。
pub trait TargetResolver: Send + Sync {
    fn resolve<'a>(&'a self, raw_url: &'a str) -> ResolveFuture<'a>;
}

type LookupFuture<'a> = Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// DNS 参照の抽象。テストで差し替える。
pub trait DnsLookup: Send + Sync {
    fn lookup<'a>(&'a self, host: &'a str) -> LookupFuture<'a>;
}

/// システムリゾルバ (tokio)。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemDnsLookup;

impl DnsLookup for SystemDnsLookup {
    fn lookup<'a>(&'a self, host: &'a str) -> LookupFuture<'a> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host, 0)).await?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        })
    }
}

/// 許可ホストと DNS 参照を持つ既定の解決器。
pub struct DnsResolver {
    allowed: HashSet<String>,
    lookup: Arc<dyn DnsLookup>,
}

impl DnsResolver {
    /// `allowed_hosts` はカンマ区切りの完全一致リスト (前後空白は無視)。
    /// ワイルドカード・ポート・URL・パスは許可しない。
    pub fn new(allowed_hosts: &str) -> Result<Self, TargetError> {
        Self::with_lookup(allowed_hosts, Arc::new(SystemDnsLookup))
    }

    /// 正規化済みの許可ホストから作る (起動時に検証済みの一覧)。
    pub fn from_normalized(allowed_hosts: Vec<String>) -> Self {
        Self {
            allowed: allowed_hosts.into_iter().collect(),
            lookup: Arc::new(SystemDnsLookup),
        }
    }

    /// DNS 参照を差し替えて作る (テスト用)。
    pub fn with_lookup(allowed_hosts: &str, lookup: Arc<dyn DnsLookup>) -> Result<Self, TargetError> {
        let mut allowed = HashSet::new();
        for host in allowed_hosts.split(',').map(str::trim).filter(|host| !host.is_empty()) {
            allowed.insert(normalize_host(host)?);
        }
        Ok(Self { allowed, lookup })
    }

    /// DNS 参照を差し替えて作る (正規化済み一覧)。
    pub fn with_lookup_normalized(
        allowed_hosts: Vec<String>,
        lookup: Arc<dyn DnsLookup>,
    ) -> Self {
        Self {
            allowed: allowed_hosts.into_iter().collect(),
            lookup,
        }
    }

    pub fn is_allowed(&self, host: &str) -> bool {
        self.allowed.contains(host)
    }
}

impl TargetResolver for DnsResolver {
    fn resolve<'a>(&'a self, raw_url: &'a str) -> ResolveFuture<'a> {
        Box::pin(async move { self.resolve_inner(raw_url).await })
    }
}

impl DnsResolver {
    async fn resolve_inner(&self, raw_url: &str) -> Result<Target, TargetError> {
        let invalid_url = || TargetError::new(400, "invalid target URL");
        if raw_url.chars().any(|c| c.is_control()) {
            return Err(invalid_url());
        }
        let mut url = Url::parse(raw_url).map_err(|_| invalid_url())?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            return Err(TargetError::new(
                400,
                "target must be HTTP(S) without user information",
            ));
        }
        let hostname = url.host_str().unwrap_or_default().to_string();
        let host = normalize_host(&hostname)?;
        if !self.allowed.contains(&host) {
            return Err(TargetError::new(400, "target host is not allowed"));
        }
        set_normalized_host(&mut url, &host)?;

        let literal = host.parse::<IpAddr>().ok();
        let records: Vec<IpAddr> = match literal {
            Some(address) => vec![address],
            None => {
                let records = self
                    .lookup
                    .lookup(&host)
                    .await
                    .map_err(|_| TargetError::new(502, "target DNS lookup failed"))?;
                if records.is_empty() {
                    return Err(TargetError::new(502, "target DNS returned no addresses"));
                }
                records
            }
        };
        // 全応答を検査してから 1 個だけ選ぶ。1 つでも非公開なら拒否する。
        if records.iter().any(|address| !is_public_address(*address)) {
            return Err(TargetError::new(400, "target address is not public"));
        }
        let address = records
            .iter()
            .find(|address| address.is_ipv4())
            .copied()
            .unwrap_or(records[0]);
        Ok(Target { url, address })
    }
}

fn set_normalized_host(url: &mut Url, host: &str) -> Result<(), TargetError> {
    let serialized = if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    url.set_host(Some(&serialized))
        .map_err(|_| TargetError::new(400, "invalid host"))
}

/// 許可リストと URL に同じ正規化を使う。
///
/// - 前後空白・パス・ポート・資格情報・ワイルドカードを含む値は拒否する。
/// - 末尾の `.` と IPv6 の `[]` を外し、IDN は punycode、IPv6 は WHATWG 表記へ
///   正規化する。WHATWG の省略形 IPv4 (`127.1` など) も URL パーサ経由で
///   `127.0.0.1` に揃える。
pub fn normalize_host(value: &str) -> Result<String, TargetError> {
    let invalid = || TargetError::new(400, "invalid host");
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | '?' | '#' | '\\' | '*'))
    {
        return Err(invalid());
    }
    let mut host = value.trim_end_matches('.').to_string();
    if host.starts_with('[') && host.ends_with(']') {
        host = host[1..host.len() - 1].to_string();
    }
    if host.is_empty() {
        return Err(invalid());
    }
    if host.contains(':') {
        let address: Ipv6Addr = host.parse().map_err(|_| invalid())?;
        return Ok(address.to_string());
    }
    let parsed = Url::parse(&format!("http://{host}/")).map_err(|_| invalid())?;
    if parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "/" | "")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid());
    }
    let normalized = parsed.host_str().ok_or_else(invalid)?.trim_end_matches('.').to_string();
    if normalized.is_empty()
        || normalized.len() > 253
        || !normalized.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
    {
        return Err(invalid());
    }
    Ok(normalized)
}

/// 公開 unicast アドレスか。ループバック・私設・文書用・予約・IPv4 埋込み
/// 移行用アドレスを拒否する (旧 Node 版 `security.mjs` と同じ規則)。
pub fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    // 公開 unicast 以外と、文書用・廃止済み・IPv4 埋込みの移行用アドレスは
    // 拒否する。mapped/NAT64/compatible は 2000::/3 外。Teredo/ORCHID 等は
    // 2001::/23 内。
    segments[0] >= 0x2000
        && segments[0] <= 0x3fff
        && !(segments[0] == 0x2001 && (segments[1] < 0x0200 || segments[1] == 0x0db8))
        && segments[0] != 0x2002
        && segments[0] != 0x3ffe
        && !(segments[0] == 0x3fff && segments[1] < 0x1000)
        && !(segments[5] == 0x5efe && (segments[4] == 0 || segments[4] == 0x0200))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type FakeRespond = Arc<dyn Fn(&str, usize) -> std::io::Result<Vec<IpAddr>> + Send + Sync>;

    struct FakeLookup {
        seen: Arc<Mutex<Vec<String>>>,
        calls: Arc<AtomicUsize>,
        respond: FakeRespond,
    }

    impl DnsLookup for FakeLookup {
        fn lookup<'a>(&'a self, host: &'a str) -> LookupFuture<'a> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.seen.lock().unwrap().push(host.to_string());
                (self.respond)(host, call)
            })
        }
    }

    fn lookup(seen: Arc<Mutex<Vec<String>>>, calls: Arc<AtomicUsize>, respond: impl Fn(&str, usize) -> std::io::Result<Vec<IpAddr>> + Send + Sync + 'static) -> Arc<dyn DnsLookup> {
        Arc::new(FakeLookup {
            seen,
            calls,
            respond: Arc::new(respond),
        })
    }

    fn public() -> Vec<IpAddr> {
        vec!["8.8.8.8".parse().unwrap()]
    }

    fn resolver(hosts: &str, lookup: Arc<dyn DnsLookup>) -> DnsResolver {
        DnsResolver::with_lookup(hosts, lookup).expect("resolver")
    }

    #[tokio::test]
    async fn allowed_hosts_use_exact_case_insensitive_idn_and_trailing_dot_normalization() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = resolver(
            " BÜCHER.example. , example.com ",
            lookup(seen.clone(), calls, |_, _| Ok(public())),
        );
        let target = resolver
            .resolve("https://BÜCHER.example./chapter?part=1")
            .await
            .expect("target");
        assert_eq!(target.url.host_str(), Some("xn--bcher-kva.example"));
        assert_eq!(target.url.path(), "/chapter");
        assert_eq!(target.url.query(), Some("part=1"));
        resolver.resolve("https://example.com./chapter").await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["xn--bcher-kva.example", "example.com"]
        );
    }

    #[tokio::test]
    async fn empty_or_nonmatching_host_list_rejects_before_dns() {
        let calls = Arc::new(AtomicUsize::new(0));
        for hosts in ["", "example.com"] {
            let resolver = resolver(hosts, lookup(Arc::new(Mutex::new(Vec::new())), calls.clone(), |_, _| Ok(public())));
            for url in ["https://sub.example.com/", "https://notexample.com/", "https://example.com.evil/"] {
                let error = resolver.resolve(url).await.unwrap_err();
                assert_eq!(error.status, 400, "{hosts} {url}");
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn wildcards_ports_and_urls_cannot_expand_the_host_allowlist() {
        for entry in [
            "*.example.com",
            "https://example.com",
            "example.com:443",
            "user@example.com",
            "example.com/",
            "example..com",
        ] {
            let error = DnsResolver::new(entry).err().expect(entry);
            assert_eq!(error.status, 400, "{entry}");
        }
    }

    #[tokio::test]
    async fn unsupported_schemes_and_user_information_are_rejected_without_dns() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = resolver("example.com", lookup(Arc::new(Mutex::new(Vec::new())), calls.clone(), |_, _| {
            Err(std::io::Error::other("unexpected lookup"))
        }));
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/a",
            "http://user@example.com/a",
            "https://u:p@example.com/?private-query",
            "not a URL",
        ] {
            let error = resolver.resolve(url).await.unwrap_err();
            assert_eq!(error.status, 400, "{url}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn public_literal_addresses_keep_their_validated_destination_without_dns() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = resolver(
            "8.8.8.8,[2606:4700:4700::1111]",
            lookup(Arc::new(Mutex::new(Vec::new())), calls.clone(), |_, _| {
                Err(std::io::Error::other("literal must not be looked up"))
            }),
        );
        let v4 = resolver.resolve("https://8.8.8.8/a").await.unwrap();
        let v6 = resolver.resolve("https://[2606:4700:4700::1111]/b").await.unwrap();
        assert_eq!(v4.address, "8.8.8.8".parse::<IpAddr>().unwrap());
        assert_eq!(v6.address, "2606:4700:4700::1111".parse::<IpAddr>().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn nonpublic_and_embedded_address_spellings_cannot_bypass_the_policy() {
        let destinations = [
            "0.0.0.0",
            "10.0.0.1",
            "127.0.0.1",
            "127.1",
            "2130706433",
            "0x7f000001",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "[::]",
            "[::1]",
            "[fc00::1]",
            "[fe80::1]",
            "[ff02::1]",
            "[2001:db8::1]",
            "[::ffff:127.0.0.1]",
            "[::ffff:10.0.0.1]",
            "[::127.0.0.1]",
            "[64:ff9b::7f00:1]",
            "[2002:7f00:1::]",
            "[2001::1]",
            "[2001:4860::5efe:7f00:1]",
        ];
        for destination in destinations {
            let url = Url::parse(&format!("http://{destination}/")).expect(destination);
            let hostname = url.host_str().expect(destination).to_string();
            let resolver = DnsResolver::new(&hostname).expect(destination);
            let error = resolver.resolve(url.as_str()).await.unwrap_err();
            assert_eq!(error.status, 400, "{destination}");
        }
    }

    #[tokio::test]
    async fn every_dns_answer_is_checked_not_just_a_first_public_answer() {
        let cases: Vec<Vec<IpAddr>> = vec![
            vec!["8.8.8.8".parse().unwrap(), "169.254.169.254".parse().unwrap()],
            vec!["::1".parse().unwrap(), "8.8.8.8".parse().unwrap()],
        ];
        for records in cases {
            let resolver = resolver("example.com", lookup(Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)), move |_, _| Ok(records.clone())));
            let error = resolver.resolve("https://example.com/").await.unwrap_err();
            assert_eq!(error.status, 400);
        }
    }

    #[tokio::test]
    async fn a_later_dns_rebind_is_not_authorized_by_an_earlier_successful_lookup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = resolver(
            "example.com",
            lookup(Arc::new(Mutex::new(Vec::new())), calls.clone(), |_, call| {
                if call == 0 {
                    Ok(public())
                } else {
                    Ok(vec!["127.0.0.1".parse().unwrap()])
                }
            }),
        );
        let first = resolver.resolve("https://example.com/one").await.unwrap();
        assert_eq!(first.address, "8.8.8.8".parse::<IpAddr>().unwrap());
        let second = resolver.resolve("https://example.com/two").await;
        assert_eq!(second.unwrap_err().status, 400);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lookup_failures_do_not_expose_urls_query_values_or_raw_diagnostics() {
        let secret = "secret-query-cookie-authorization";
        let resolver = resolver(
            "example.com",
            lookup(Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)), move |_, _| {
                Err(std::io::Error::other(secret))
            }),
        );
        let error = resolver
            .resolve(&format!("https://example.com/?token={secret}"))
            .await
            .unwrap_err();
        assert_eq!(error.status, 502);
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn normalize_host_rejects_ports_paths_and_credentials() {
        assert_eq!(normalize_host("Example.COM").unwrap(), "example.com");
        assert_eq!(normalize_host("example.com.").unwrap(), "example.com");
        assert_eq!(normalize_host("BÜCHER.example").unwrap(), "xn--bcher-kva.example");
        assert_eq!(normalize_host("[2606:4700:4700::1111]").unwrap(), "2606:4700:4700::1111");
        assert_eq!(normalize_host("2606:4700:4700:0:0:0:0:1111").unwrap(), "2606:4700:4700::1111");
        assert_eq!(normalize_host("127.1").unwrap(), "127.0.0.1");
        for value in ["", " ", "example.com:443", "example.com/", "user@example.com", "*.example.com", "example..com", "-example.com", "example-.com"] {
            assert!(normalize_host(value).is_err(), "{value}");
        }
    }

    #[test]
    fn public_address_rules_cover_ipv4_and_ipv6_boundaries() {
        for address in ["8.8.8.8", "1.1.1.1", "203.0.114.1", "172.32.0.1", "198.20.0.1"] {
            assert!(is_public_address(address.parse().unwrap()), "{address}");
        }
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "169.254.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "2001:db8::1",
            "2002::1",
            "::ffff:8.8.8.8",
            "64:ff9b::8.8.8.8",
        ] {
            assert!(!is_public_address(address.parse().unwrap()), "{address}");
        }
        assert!(is_public_address("2606:4700:4700::1111".parse().unwrap()));
    }
}
