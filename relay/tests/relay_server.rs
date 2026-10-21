//! 旧 `tests/relay-server.test.mjs` のサーバー契約 (ルート・メソッド・認証・
//! ヘッダ転送・失敗経路・期限・切断・keep-alive) を Rust リレーへ移植する。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use narou_relay::config::TransportBin;
use narou_relay::transport::TransportKind;
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

/// 何でも 200 "ok" を返す上流。
fn ok_upstream() -> impl Fn(&UpstreamRequest) -> Action + Send + Sync + 'static {
    |_| Action::Response {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: b"ok".to_vec(),
    }
}

#[tokio::test]
async fn health_routes_methods_and_unknown_paths_keep_distinct_statuses() {
    let relay = TestRelay::start(test_config()).await;
    let health = get_health(relay.addr).await;
    health.expect_status(200);
    assert_eq!(health.json(), json!({ "ok": true, "fetch": "curl,wget" }));

    let post_health = request_once(relay.addr, "POST", "/health", &[], None).await;
    post_health.expect_status(405);
    assert_eq!(post_health.json()["error"], "method not allowed");

    let missing = request_once(relay.addr, "GET", "/missing", &[], None).await;
    missing.expect_status(404);
    assert_eq!(missing.json()["error"], "not found");

    let patch_with_token =
        request_once(relay.addr, "PATCH", "/proxy", &[("X-Proxy-Token", TOKEN)], None).await;
    patch_with_token.expect_status(405);

    relay.stop().await;
}

#[tokio::test]
async fn method_dispatch_matches_the_node_server_ordering() {
    // 旧 Node 版 `server.mjs` は /proxy の X-Proxy-Token を検証してから
    // メソッドを分岐する (token 不一致は 403)。PATCH 自体の 405 は token が
    // 正しい場合にだけ返る。この順序を固定する。
    let relay = TestRelay::start(test_config()).await;
    let patch_with_token =
        request_once(relay.addr, "PATCH", "/proxy", &[("X-Proxy-Token", TOKEN)], None).await;
    patch_with_token.expect_status(405);
    let patch_without_token = request_once(relay.addr, "PATCH", "/proxy", &[], None).await;
    patch_without_token.expect_status(403);
    relay.stop().await;
}

#[tokio::test]
async fn authentication_token_variants_are_forbidden() {
    let upstream = Upstream::start(ok_upstream()).await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/dup");
    let without_last = &TOKEN[..TOKEN.len() - 1];
    let extra = format!("{TOKEN}x");
    let duplicated = format!("{TOKEN},{TOKEN}");
    for token in ["", without_last, extra.as_str(), duplicated.as_str()] {
        let response = get_proxy_once(relay.addr, token, &target, None).await;
        response.expect_status(403);
        assert_eq!(response.json()["error"], "forbidden");
    }
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn malformed_post_json_is_400_and_not_echoed() {
    let relay = TestRelay::start(test_config()).await;
    let response =
        post_proxy_once(relay.addr, Some(TOKEN), b"{\"private\":\"do-not-echo\",").await;
    response.expect_status(400);
    assert!(
        !response.text().contains("do-not-echo"),
        "relay must not echo malformed input: {:?}",
        response.text()
    );
    relay.stop().await;
}

#[tokio::test]
async fn invalid_forwarded_headers_and_a_mismatched_host_are_400() {
    let upstream = Upstream::start(ok_upstream()).await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/");

    let invalid_name = call_proxy(
        relay.addr,
        TOKEN,
        &target,
        &json!({ "x-invalid\r\n": "header" }),
    )
    .await;
    invalid_name.expect_status(400);

    let mismatched_host = call_proxy(
        relay.addr,
        TOKEN,
        &target,
        &json!({ "Host": "metadata.google.internal" }),
    )
    .await;
    mismatched_host.expect_status(400);

    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_host_header_matching_the_target_is_forwarded() {
    let upstream = Upstream::start(ok_upstream()).await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/");
    let host = upstream.host_port();

    let response = call_proxy(relay.addr, TOKEN, &target, &json!({ "Host": host })).await;
    response.expect_status(200);
    assert_eq!(
        upstream.seen.last_header("host").as_deref(),
        Some(host.as_str()),
        "the upstream must see the Host header that named the validated target"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_valid_upstream_403_is_returned_as_relay_200_without_a_fallback() {
    let upstream = Upstream::start(|_| Action::Response {
        status: 403,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: b"permission denied".to_vec(),
    })
    .await;
    // 取得順は既定 (curl,wget)。上流の HTTP 403 は取得成功なので wget へ
    // フォールバックしてはならない。
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/"), &json!({})).await;
    response.expect_status(200);
    let result = response.json();
    assert_eq!(result["status"], 403);
    assert_eq!(result["via"], "curl");
    assert_eq!(body_text(&result), "permission denied");
    assert_eq!(
        upstream.seen.requests(),
        1,
        "a valid upstream 403 must not be retried with another transport"
    );
    assert_eq!(upstream.seen.connections(), 1);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn forwarded_header_names_keep_the_callers_casing() {
    let upstream = Upstream::start(ok_upstream()).await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(
        relay.addr,
        TOKEN,
        &upstream.target("/"),
        &json!({
            "Accept": "text/html",
            "Sec-Fetch-Dest": "document",
            "cookie": "over18=off",
        }),
    )
    .await;
    response.expect_status(200);
    let head = upstream.seen.last_head_latin1();
    for expected in [
        "Accept: text/html",
        "Sec-Fetch-Dest: document",
        "cookie: over18=off",
    ] {
        assert!(
            head.contains(expected),
            "upstream did not see the caller's header casing {expected:?}; head={head:?}"
        );
    }

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_response_body_beginning_with_a_status_line_is_preserved() {
    let body = b"HTTP/1.1 403 this is novel text\r\n\r\nbody after that".to_vec();
    let upstream = Upstream::start(move |_| Action::Response {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: body.clone(),
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/"), &json!({})).await;
    response.expect_status(200);
    let result = response.json();
    assert_eq!(
        body_text(&result),
        "HTTP/1.1 403 this is novel text\r\n\r\nbody after that",
        "a body starting with an HTTP status line must be preserved verbatim"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn missing_curl_with_only_curl_configured_returns_502_without_secrets() {
    let secret = "private-query-cookie-authorization";
    let upstream = Upstream::start(ok_upstream()).await;

    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl];
    config.curl = TransportBin {
        bin: format!("missing-curl-{secret}").into(),
        source: "test",
    };
    let relay = TestRelay::start(config).await;

    let target = format!("{}?token={secret}", upstream.target("/"));
    let response = call_proxy(
        relay.addr,
        TOKEN,
        &target,
        &json!({ "cookie": secret, "authorization": secret }),
    )
    .await;
    response.expect_status(502);
    assert!(response.json().get("body").is_none());
    let response_text = response.text();
    let log_text = relay.log_text();
    for leaked in [secret, TOKEN, target.as_str()] {
        assert!(!response_text.contains(leaked), "response leaked {leaked:?}");
        assert!(!log_text.contains(leaked), "logs leaked {leaked:?}");
    }
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_missing_curl_falls_through_to_wget_which_cannot_fetch_http() {
    // wget は HTTPS 専用 (旧 Node 版 transport.mjs も HTTP では Failed)。
    // 取得順が curl,wget でも HTTP ターゲットは 502 にしかならない。
    let secret = "private-wget-cookie";
    let upstream = Upstream::start(ok_upstream()).await;
    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl, TransportKind::Wget];
    config.curl = TransportBin {
        bin: "missing-relay-test-curl".into(),
        source: "test",
    };
    let relay = TestRelay::start(config).await;

    let response = call_proxy(
        relay.addr,
        TOKEN,
        &upstream.target("/"),
        &json!({ "cookie": secret }),
    )
    .await;
    response.expect_status(502);
    assert!(response.json().get("body").is_none());
    let log_text = relay.log_text();
    assert!(!log_text.contains(secret));
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn internal_resolver_failures_return_500_without_leaking_inputs() {
    // 旧 Node 版は resolver の想定外例外を 500 {"error":"internal server error"}
    // にする。Rust の公開 API は TargetError (status 付き) しか返せないため、
    // 同じ公開結果になる status 500 の TargetError で経路を固定する。
    let secret = "private-internal-diagnostic";
    let upstream = Upstream::start(ok_upstream()).await;
    let relay = TestRelay::start_with(
        test_config(),
        Arc::new(FixedErrorResolver {
            status: 500,
            message: "internal server error",
        }),
    )
    .await;

    let target = format!("{}?{secret}", upstream.target("/"));
    let response = call_proxy(
        relay.addr,
        TOKEN,
        &target,
        &json!({ "cookie": secret, "authorization": secret }),
    )
    .await;
    response.expect_status(500);
    assert_eq!(
        response.json(),
        json!({ "error": "internal server error" })
    );
    let response_text = response.text();
    let log_text = relay.log_text();
    assert!(!response_text.contains(secret));
    assert!(!log_text.contains(secret));
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn untrusted_tls_failures_return_502_without_an_http_request() {
    // 平文 HTTP の上流へ https:// で接続すると、TLS 検証は必ず失敗する。
    // 資格情報を含む HTTP リクエストが上流へ届いてはならない。
    let secret = "private-tls-cookie";
    let upstream = Upstream::start(ok_upstream()).await;
    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl];
    config.curl_timeout = Duration::from_secs(5);
    config.request_timeout = Duration::from_secs(10);
    let relay = TestRelay::start(config).await;

    let target = format!("https://relay.invalid:{}/?token={secret}", upstream.addr.port());
    let response = call_proxy(relay.addr, TOKEN, &target, &json!({ "cookie": secret })).await;
    response.expect_status(502);
    assert!(response.json().get("body").is_none());
    assert!(!response.text().contains(secret));
    assert!(!relay.log_text().contains(secret));
    // 上流の接続処理が終わるのを少し待ってから、HTTP リクエストが 0 件なのを確認する。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        upstream.seen.requests(),
        0,
        "no HTTP request may reach an upstream whose TLS could not be verified"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn transport_timeouts_return_502_and_close_the_upstream_socket() {
    let upstream = Upstream::start(|_| Action::Hang).await;
    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl];
    config.curl_timeout = Duration::from_millis(300);
    config.wget_timeout = Duration::from_millis(300);
    config.request_timeout = Duration::from_secs(2);
    let relay = TestRelay::start(config).await;

    let response = tokio::time::timeout(
        Duration::from_secs(6),
        call_proxy(relay.addr, TOKEN, &upstream.target("/hang"), &json!({})),
    )
    .await
    .expect("a transport timeout must return within a few seconds");
    response.expect_status(502);
    assert!(
        response.json().get("body").is_none(),
        "a timed-out fetch must not return a partial body"
    );
    upstream
        .seen
        .wait_closed(1, Duration::from_secs(5))
        .await;
    assert_eq!(
        upstream.seen.requests(),
        1,
        "only the configured curl transport may be attempted"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn the_overall_deadline_kills_an_active_fetch_without_a_fallback() {
    let upstream = Upstream::start(|_| Action::Hang).await;
    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl];
    config.curl_timeout = Duration::from_secs(5);
    config.wget_timeout = Duration::from_secs(5);
    config.request_timeout = Duration::from_millis(500);
    let relay = TestRelay::start(config).await;

    let response = tokio::time::timeout(
        Duration::from_secs(6),
        call_proxy(relay.addr, TOKEN, &upstream.target("/hang"), &json!({})),
    )
    .await
    .expect("the request deadline must return within a few seconds");
    response.expect_status(502);
    upstream
        .seen
        .wait_closed(1, Duration::from_secs(5))
        .await;
    assert_eq!(
        upstream.seen.requests(),
        1,
        "the overall deadline must not start a second transport attempt"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn client_disconnect_cancels_the_active_fetch_and_cannot_retry() {
    let upstream = Upstream::start(|_| Action::Hang).await;
    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl];
    config.curl_timeout = Duration::from_secs(10);
    config.wget_timeout = Duration::from_secs(10);
    config.request_timeout = Duration::from_secs(10);
    let relay = TestRelay::start(config).await;

    let mut client = RawClient::connect(relay.addr).await;
    let body = json!({ "url": upstream.target("/hang") }).to_string().into_bytes();
    let raw = build_request(
        "POST",
        "/proxy",
        &[
            ("X-Proxy-Token", TOKEN),
            ("Content-Type", "application/json"),
        ],
        Some(&body),
    );
    client.send_raw(&raw).await;
    upstream
        .seen
        .wait_requests(1, Duration::from_secs(5))
        .await;
    drop(client);
    upstream
        .seen
        .wait_closed(1, Duration::from_secs(5))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        upstream.seen.requests(),
        1,
        "a client disconnect must cancel the fetch, not retry it"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_dns_result_arriving_after_the_deadline_cannot_start_a_connection() {
    let upstream = Upstream::start(ok_upstream()).await;
    let mut config = test_config();
    config.request_timeout = Duration::from_millis(150);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let resolver = Arc::new(DelayedResolver {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    let relay = TestRelay::start_with(config, resolver).await;

    let addr = relay.addr;
    let body = json!({ "url": upstream.target("/too-late") })
        .to_string()
        .into_bytes();
    let pending = tokio::spawn(async move { post_proxy_once(addr, Some(TOKEN), &body).await });
    started.notified().await;
    let response = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("the request deadline must cancel the pending DNS lookup")
        .expect("relay request task");
    response.expect_status(502);

    release.add_permits(1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        upstream.seen.requests(),
        0,
        "a DNS result arriving after the deadline must not start a connection"
    );
    assert_eq!(upstream.seen.connections(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn two_sequential_proxy_requests_share_one_keep_alive_connection() {
    let upstream = Upstream::start(ok_upstream()).await;
    let relay = TestRelay::start(test_config()).await;

    let mut client = RawClient::connect(relay.addr).await;
    for path in ["/first", "/second"] {
        let body = json!({ "url": upstream.target(path) }).to_string().into_bytes();
        client
            .send_request(
                "POST",
                "/proxy",
                &[
                    ("X-Proxy-Token", TOKEN),
                    ("Content-Type", "application/json"),
                    ("Connection", "keep-alive"),
                ],
                Some(&body),
            )
            .await;
        let response = client.read_response().await;
        response.expect_status(200);
        assert_eq!(response.json()["status"], 200);
    }
    assert_eq!(upstream.seen.requests(), 2);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn an_error_response_closes_the_connection() {
    let relay = TestRelay::start(test_config()).await;
    let mut client = RawClient::connect(relay.addr).await;
    client
        .send_request(
            "POST",
            "/proxy",
            &[("X-Proxy-Token", TOKEN), ("Connection", "keep-alive")],
            Some(b"{"),
        )
        .await;
    let response = client.read_response().await;
    response.expect_status(400);
    assert_eq!(
        response.header("connection").as_deref(),
        Some("close"),
        "4xx responses must close the connection"
    );
    let trailing = client.read_to_eof(Duration::from_secs(2)).await;
    assert!(
        trailing.is_empty(),
        "the relay must close after an error response; trailing={trailing:?}"
    );
    relay.stop().await;
}

#[tokio::test]
async fn health_reports_the_configured_fetch_order() {
    let default_relay = TestRelay::start(test_config()).await;
    assert_eq!(
        get_health(default_relay.addr).await.json()["fetch"],
        "curl,wget"
    );
    default_relay.stop().await;

    let curl_only = TestRelay::start(config_with(&[("NAROU_RELAY_FETCH", "curl")])).await;
    assert_eq!(get_health(curl_only.addr).await.json()["fetch"], "curl");
    curl_only.stop().await;
}
