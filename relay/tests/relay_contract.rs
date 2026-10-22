//! 旧 `tests/relay.test.mjs` の HTTP 契約を Rust リレーへ移植する。
//!
//! 取得自体は実 HTTP の上流サーバーで検証し、localhost の例外はテスト専用
//! resolver (`common::LocalResolver`) にだけ置く。

mod common;

use std::time::Duration;

use common::*;
use narou_relay::config::TransportBin;
use narou_relay::transport::TransportKind;
use narou_relay::wire::{MAX_BODY_BYTES, MAX_METADATA_BYTES, MAX_REQUEST_BYTES};
use serde_json::json;

/// 上流が受け取ったヘッダ値を UTF-8 として読む (HTTP の生バイトが UTF-8 か)。
fn upstream_header_utf8(seen: &Seen, name: &str) -> String {
    let bytes = seen
        .last_header_bytes(name)
        .unwrap_or_else(|| panic!("upstream did not receive header {name}; head={:?}", seen.last_head_latin1()));
    String::from_utf8(bytes).expect("upstream header value must be UTF-8 bytes")
}

/// 2 つの Set-Cookie を返す上流。
fn dup_upstream() -> impl Fn(&UpstreamRequest) -> Action + Send + Sync + 'static {
    |request| match request.path_only() {
        "/dup" => Action::Response {
            status: 200,
            headers: vec![
                (
                    "Content-Type".to_string(),
                    "text/html; charset=UTF-8".to_string(),
                ),
                ("Set-Cookie".to_string(), "sid=1; Path=/".to_string()),
                ("Set-Cookie".to_string(), "over18=yes".to_string()),
            ],
            body: b"body".to_vec(),
        },
        _ => Action::Response {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: b"ok".to_vec(),
        },
    }
}

#[tokio::test]
async fn missing_or_incorrect_token_is_rejected_before_the_upstream_is_contacted() {
    let upstream = Upstream::start(|_| Action::Response {
        status: 200,
        headers: Vec::new(),
        body: b"body".to_vec(),
    })
    .await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/dup");
    let body = json!({ "url": target }).to_string().into_bytes();
    for token in [None, Some("relay-private-test-key-wrong")] {
        let response = post_proxy_once(relay.addr, token, &body).await;
        response.expect_status(403);
        assert_eq!(response.json()["error"], "forbidden");
    }
    assert_eq!(
        upstream.seen.requests(),
        0,
        "authentication must be checked before any upstream request"
    );
    assert_eq!(upstream.seen.connections(), 0);
    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn cookies_and_repeated_header_order_survive_the_round_trip() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/dup");

    let response = call_proxy(
        relay.addr,
        TOKEN,
        &target,
        &json!({ "x-multi": ["first", "second"] }),
    )
    .await;
    response.expect_status(200);
    let result = response.json();
    assert_eq!(result["status"], 200);
    assert_eq!(
        result["setCookie"],
        json!(["sid=1; Path=/", "over18=yes"]),
        "all Set-Cookie headers must be returned in order"
    );
    assert_eq!(
        upstream.seen.last_header("x-multi").as_deref(),
        Some("first, second"),
        "repeated header values must keep their order"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn cookies_from_set_cookie_can_authenticate_the_next_request() {
    let upstream = Upstream::start(|request| {
        if request.path_only() == "/account" {
            let logged_in =
                request.header_latin1("cookie").as_deref() == Some("sid=1; over18=yes");
            Action::Response {
                status: if logged_in { 200 } else { 401 },
                headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                body: if logged_in {
                    b"signed in".to_vec()
                } else {
                    b"sign in required".to_vec()
                },
            }
        } else {
            Action::Response {
                status: 200,
                headers: vec![
                    ("Set-Cookie".to_string(), "sid=1; Path=/".to_string()),
                    ("Set-Cookie".to_string(), "over18=yes".to_string()),
                ],
                body: b"body".to_vec(),
            }
        }
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/dup"), &json!({})).await;
    response.expect_status(200);
    let result = response.json();
    let cookies = result["setCookie"]
        .as_array()
        .expect("setCookie array")
        .iter()
        .map(|value| value.as_str().expect("setCookie string").split(';').next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("; ");
    assert_eq!(cookies, "sid=1; over18=yes");

    let anonymous = call_proxy(relay.addr, TOKEN, &upstream.target("/account"), &json!({})).await;
    anonymous.expect_status(200);
    assert_eq!(anonymous.json()["status"], 401);

    let account = call_proxy(
        relay.addr,
        TOKEN,
        &upstream.target("/account"),
        &json!({ "cookie": cookies }),
    )
    .await;
    account.expect_status(200);
    let logged_in = account.json();
    assert_eq!(logged_in["status"], 200);
    assert_eq!(body_text(&logged_in), "signed in");

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn redirects_are_returned_without_a_second_upstream_request() {
    let upstream = Upstream::start(|request| {
        if request.path_only() == "/redirect" {
            Action::Response {
                status: 302,
                headers: vec![("Location".to_string(), "/followed".to_string())],
                body: Vec::new(),
            }
        } else {
            Action::Response {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                body: b"followed".to_vec(),
            }
        }
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/redirect"), &json!({})).await;
    response.expect_status(200);
    let result = response.json();
    assert_eq!(result["status"], 302);
    assert_eq!(result["location"], "/followed");
    assert_eq!(
        upstream.seen.count_path("/followed"),
        0,
        "the relay must not follow redirects itself"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn an_informational_response_cannot_masquerade_as_a_completed_fetch() {
    let upstream = Upstream::start(|_| {
        Action::Raw(
            b"HTTP/1.1 100 Continue\r\nX-Interim: 1\r\n\r\n\
              HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
              Set-Cookie: after-100=1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nafter"
                .to_vec(),
        )
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/interim"), &json!({})).await;
    // Rust の curl トランスポートは 1xx を読み飛ばすので、最終応答だけを返す。
    response.expect_status(200);
    let result = response.json();
    assert_eq!(result["status"], 200);
    assert_eq!(result["setCookie"], json!(["after-100=1"]));
    assert_eq!(body_text(&result), "after");

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn get_decodes_utf8_json_before_sending_headers() {
    let upstream = Upstream::start(|_| Action::Response {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: b"UTF-8 header accepted".to_vec(),
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let raw = b64_encode(
        json!({
            "x-label": "café",
            "x-multi": ["première", "deuxième"],
        })
        .to_string()
        .as_bytes(),
    );
    let response = get_proxy_once(
        relay.addr,
        TOKEN,
        &upstream.target("/utf8"),
        Some(raw.as_str()),
    )
    .await;
    response.expect_status(200);
    assert_eq!(response.json()["status"], 200);
    assert_eq!(upstream_header_utf8(&upstream.seen, "x-label"), "café");
    assert_eq!(
        upstream_header_utf8(&upstream.seen, "x-multi"),
        "première, deuxième"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn malformed_get_header_payloads_return_400() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/dup");

    let invalid_utf8 = b64_encode(b"{\"x-label\":\"\xc3\x28\"}");
    let mut malformed: Vec<String> = ["", "%%%", "a", "e30=!", invalid_utf8.as_str()]
        .into_iter()
        .map(str::to_string)
        .collect();
    for value in ["null", "[]", "\"text\"", "42", "{"] {
        malformed.push(b64_encode(value.as_bytes()));
    }
    for raw in malformed {
        let response = get_proxy_once(relay.addr, TOKEN, &target, Some(raw.as_str())).await;
        assert_eq!(response.status, 400, "headers={raw:?} must be rejected");
    }
    assert_eq!(
        upstream.seen.requests(),
        0,
        "malformed header payloads must be rejected before the upstream is contacted"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_missing_get_headers_parameter_still_fetches() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;

    let response = get_proxy_once(relay.addr, TOKEN, &upstream.target("/dup"), None).await;
    response.expect_status(200);
    assert_eq!(
        response.json()["setCookie"],
        json!(["sid=1; Path=/", "over18=yes"])
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn request_json_at_the_64_kib_boundary_is_accepted() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;

    let boundary = padded_json(&upstream.target("/dup"), MAX_REQUEST_BYTES);
    assert_eq!(boundary.len(), MAX_REQUEST_BYTES);
    let response = post_proxy_once(relay.addr, Some(TOKEN), &boundary).await;
    response.expect_status(200);
    assert_eq!(response.json()["status"], 200);
    assert_eq!(upstream.seen.count_path("/dup"), 1);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn request_json_over_the_64_kib_boundary_is_rejected() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;

    // 宣言 Content-Length が上限を超えたら、本文を読まずに 400 を返す
    // (Node 版 readBoundedBytes も宣言長だけで拒否する)。
    let declared = (MAX_REQUEST_BYTES + 1).to_string();
    let response = request_once(
        relay.addr,
        "POST",
        "/proxy",
        &[
            ("X-Proxy-Token", TOKEN),
            ("Content-Length", declared.as_str()),
        ],
        None,
    )
    .await;
    response.expect_status(400);

    // 旧 Node 試験と同じく、上限 +1 バイトの本文を実際に送っても、未読本文を
    // 残した RST ではなくきれいな 400 応答が返らなければならない。
    let over_boundary = padded_json(&upstream.target("/dup"), MAX_REQUEST_BYTES + 1);
    let mut client = RawClient::connect(relay.addr).await;
    client
        .send_request(
            "POST",
            "/proxy",
            &[
                ("X-Proxy-Token", TOKEN),
                ("Content-Type", "application/json"),
            ],
            Some(&over_boundary),
        )
        .await;
    let response = client.read_response().await;
    response.expect_status(400);

    // 文字数では 64 KiB 未満でも、UTF-8 のバイト数が上限を超える JSON は 400。
    let multibyte = multibyte_padded_json(&upstream.target("/dup"));
    assert!(multibyte.len() > MAX_REQUEST_BYTES);
    assert!(
        String::from_utf8_lossy(&multibyte).chars().count() < MAX_REQUEST_BYTES,
        "the multibyte body must stay below the limit in characters"
    );
    let mut client = RawClient::connect(relay.addr).await;
    client
        .send_request(
            "POST",
            "/proxy",
            &[
                ("X-Proxy-Token", TOKEN),
                ("Content-Type", "application/json"),
            ],
            Some(&multibyte),
        )
        .await;
    let response = client.read_response().await;
    response.expect_status(400);

    assert_eq!(
        upstream.seen.requests(),
        0,
        "oversized requests must be rejected before the upstream is contacted"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_chunked_request_body_over_the_limit_is_rejected() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;

    // 1 個目で上限ちょうど、2 個目で超える。宣言長ではなく累積で判定させる。
    // 未読の chunked 本文を残した RST にならず、きれいな 400 が返ること。
    let first = vec![b'x'; MAX_REQUEST_BYTES];
    let second = vec![b'y'; 1];
    let body = chunked_body(&[&first, &second]);
    let mut client = RawClient::connect(relay.addr).await;
    client
        .send_request(
            "POST",
            "/proxy",
            &[
                ("X-Proxy-Token", TOKEN),
                ("Transfer-Encoding", "chunked"),
            ],
            Some(&body),
        )
        .await;
    let response = client.read_response().await;
    response.expect_status(400);

    assert_eq!(upstream.seen.requests(), 0);
    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn malformed_or_non_object_post_json_returns_400() {
    let upstream = Upstream::start(dup_upstream()).await;
    let relay = TestRelay::start(test_config()).await;
    let target = upstream.target("/dup");

    let invalid_utf8 = {
        let mut body = format!("{{\"url\":{},\"padding\":\"", serde_json::to_string(&target).unwrap())
            .into_bytes();
        body.extend_from_slice(b"\xc3\x28");
        body.extend_from_slice(b"\"}");
        body
    };
    for body in [
        b"null".to_vec(),
        b"[]".to_vec(),
        b"\"text\"".to_vec(),
        b"42".to_vec(),
        b"{".to_vec(),
        invalid_utf8,
    ] {
        let response = post_proxy_once(relay.addr, Some(TOKEN), &body).await;
        response.expect_status(400);
    }
    assert_eq!(upstream.seen.requests(), 0);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn credentials_never_leak_when_the_configured_curl_cannot_run() {
    let secret = "private-query-cookie-authorization";
    let authorization = format!("Bearer {secret}");
    let cookie = format!("sid={secret}");
    let upstream = Upstream::start(dup_upstream()).await;

    let mut config = test_config();
    config.fetch_order = vec![TransportKind::Curl];
    config.curl = TransportBin {
        bin: format!("missing-curl-{secret}").into(),
        source: "test",
    };
    let relay = TestRelay::start(config).await;

    let target = format!("{}?token={secret}", upstream.target("/dup"));
    let response = call_proxy(
        relay.addr,
        TOKEN,
        &target,
        &json!({ "authorization": authorization, "cookie": cookie }),
    )
    .await;
    response.expect_status(502);
    assert!(
        response.json().get("body").is_none(),
        "a failed fetch must not return a body"
    );
    let response_text = response.text();
    let log_text = relay.log_text();
    for leaked in [
        secret,
        authorization.as_str(),
        cookie.as_str(),
        target.as_str(),
        TOKEN,
    ] {
        assert!(
            !response_text.contains(leaked),
            "relay response leaked {leaked:?}: {response_text:?}"
        );
        assert!(
            !log_text.contains(leaked),
            "relay log leaked {leaked:?}: {log_text:?}"
        );
    }
    assert_eq!(
        upstream.seen.requests(),
        0,
        "a transport that cannot run must not reach the upstream"
    );

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_full_16_mib_binary_body_round_trips() {
    let expected = max_binary();
    let upstream = Upstream::start(|request| match request.path_only() {
        "/binary" => Action::Response {
            status: 200,
            headers: vec![
                (
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                ),
                ("X-Boundary".to_string(), "h".repeat(2048)),
            ],
            body: max_binary(),
        },
        _ => Action::Response {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        },
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/binary"), &json!({})).await;
    response.expect_status(200);
    let result = response.json();
    assert_eq!(result["status"], 200);
    assert_eq!(result["via"], "curl");
    assert_eq!(
        decode_body(&result),
        expected,
        "16 MiB binary body must round-trip exactly"
    );
    let mut envelope = result.clone();
    envelope["body"] = json!("");
    let envelope_len = serde_json::to_vec(&envelope).expect("envelope json").len();
    assert!(
        envelope_len <= MAX_METADATA_BYTES,
        "the JSON envelope excluding base64 body must fit the shared wire budget: {envelope_len} > {MAX_METADATA_BYTES}"
    );
    assert_eq!(upstream.seen.count_path("/binary"), 1);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_compressed_upstream_body_is_returned_decoded() {
    let upstream = Upstream::start(|_| Action::Gzip {
        body: compressed_body(),
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/compressed"), &json!({})).await;
    response.expect_status(200);
    let result = response.json();
    assert_eq!(result["status"], 200);
    assert_eq!(decode_body(&result), compressed_body());

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_declared_oversized_upstream_response_is_rejected() {
    let upstream = Upstream::start(|_| Action::Truncated {
        declared: MAX_BODY_BYTES + 1,
        sent: b"no".to_vec(),
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(
        relay.addr,
        TOKEN,
        &upstream.target("/oversize-declared"),
        &json!({}),
    )
    .await;
    response.expect_status(502);
    assert!(
        response.json().get("body").is_none(),
        "declared oversized responses must not return a body"
    );
    assert!(upstream.seen.requests() >= 1);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_streamed_oversized_chunked_response_is_rejected() {
    let upstream = Upstream::start(|_| Action::Chunked {
        chunks: vec![max_binary(), vec![0u8]],
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = tokio::time::timeout(
        Duration::from_secs(20),
        call_proxy(
            relay.addr,
            TOKEN,
            &upstream.target("/oversize-chunked"),
            &json!({}),
        ),
    )
    .await
    .expect("streamed oversize response must be rejected promptly");
    response.expect_status(502);
    assert!(
        response.json().get("body").is_none(),
        "streamed oversized responses must not return a body"
    );
    assert!(upstream.seen.requests() >= 1);

    relay.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn a_truncated_content_length_response_is_never_partial_success() {
    let upstream = Upstream::start(|_| Action::Truncated {
        declared: 64,
        sent: b"partial".to_vec(),
    })
    .await;
    let relay = TestRelay::start(test_config()).await;

    let response = call_proxy(relay.addr, TOKEN, &upstream.target("/truncated"), &json!({})).await;
    response.expect_status(502);
    assert!(
        response.json().get("body").is_none(),
        "a truncated response must never return partial success"
    );
    assert!(upstream.seen.requests() >= 1);

    relay.stop().await;
    upstream.stop().await;
}
