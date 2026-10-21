//! 旧 `tests/relay-policy.test.mjs` の production ポリシーを、起動済みリレーで
//! end-to-end に確認する。
//!
//! 許可ホストの正規化・DNS 全応答の検査・非公開アドレスの網羅は
//! `relay/src/security.rs` の単体テストが担う。ここで固定するのは「production
//! の `DnsResolver` を使ったバイナリ経路に localhost / 私設 IP の例外が無いこと」。

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use narou_relay::config::{Availability, Config};
use narou_relay::security::DnsResolver;
use serde_json::json;

fn must_not_arrive() -> impl Fn(&UpstreamRequest) -> Action + Send + Sync + 'static {
    |_| Action::Response {
        status: 200,
        headers: Vec::new(),
        body: b"must not arrive".to_vec(),
    }
}

#[tokio::test]
async fn the_production_resolver_rejects_a_loopback_target() {
    let upstream = Upstream::start(must_not_arrive()).await;
    let resolver = Arc::new(DnsResolver::new("127.0.0.1,relay.invalid").expect("resolver"));
    let relay = TestRelay::start_with(test_config(), resolver).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.loopback_target("/"), &json!({})).await;
    response.expect_status(400);
    assert_eq!(response.json()["error"], "target address is not public");
    assert_eq!(
        upstream.seen.requests(),
        0,
        "a loopback target must be rejected before any connection"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn the_production_resolver_rejects_a_host_outside_the_allowlist() {
    let upstream = Upstream::start(must_not_arrive()).await;
    let resolver = Arc::new(DnsResolver::new("127.0.0.1").expect("resolver"));
    let relay = TestRelay::start_with(test_config(), resolver).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/"), &json!({})).await;
    response.expect_status(400);
    assert_eq!(response.json()["error"], "target host is not allowed");
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn an_empty_allowlist_rejects_every_target() {
    let upstream = Upstream::start(must_not_arrive()).await;
    let resolver = Arc::new(DnsResolver::new("").expect("resolver"));
    let relay = TestRelay::start_with(test_config(), resolver).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/"), &json!({})).await;
    response.expect_status(400);
    assert_eq!(response.json()["error"], "target host is not allowed");
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn production_startup_config_has_no_localhost_or_allow_private_exception() {
    let upstream = Upstream::start(must_not_arrive()).await;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("NAROU_RELAY_KEY".to_string(), TOKEN.to_string());
    env.insert(
        "NAROU_RELAY_ALLOWED_HOSTS".to_string(),
        "localhost,127.0.0.1".to_string(),
    );
    // 旧 Node 版にも現行 Rust 版にも存在しない「私設 IP 許可」設定。
    // 設定しても本番経路の公開アドレス検査は緩まない。
    env.insert("NAROU_RELAY_ALLOW_PRIVATE".to_string(), "1".to_string());
    let config = Config::from_env_map(&env, Availability { wget: true }).expect("startup config");
    assert_eq!(
        config.allowed_hosts,
        vec!["localhost".to_string(), "127.0.0.1".to_string()]
    );
    // `main.rs` と同じく、検証済みの許可ホストから production resolver を作る。
    let resolver = Arc::new(DnsResolver::from_normalized(config.allowed_hosts.clone()));
    let relay = TestRelay::start_with(config, resolver).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.loopback_target("/"), &json!({})).await;
    response.expect_status(400);
    assert_eq!(response.json()["error"], "target address is not public");
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}
