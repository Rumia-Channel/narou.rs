//! ブラウザが発行するサイトトークンを初期化する (サイト定義の `browser_token` 用)。
//!
//! ページが読み込むスクリプトがサイト自身にトークンを発行し、そのトークンを
//! Cookie として保存する。ここで行うのは「ブラウザに発行させる」ことだけで、
//! トークンの内容を解読したり自作したりはしない。CAPTCHA を解いたり、
//! `navigator.webdriver` などの自動実行痕跡を消したりすることもしない。
//!
//! 手順は次の通り。
//!
//! 1. 親が検証済みの Browser エンドポイントへ短期の接続を開く (受信サイズの上限
//!    は接続時に決めておく)。
//! 2. 対象オリジンのページを選び、flatten セッションで接続する。
//! 3. 1 個の IIFE で、DOM の準備を待ち → 関数が無ければ設定されたスクリプトを
//!    読み込み → 関数を `this` を保ったまま呼び、返り値は捨てる。
//! 4. 設定された Cookie がブラウザに存在するか確かめる (値は見ない)。
//!
//! トークンの値は外へ出さない (ログにも載せない)。発行できたことは「サーバが
//! トークンを受け付ける」証明ではないため、既存の取得・検証の流れはそのまま残す。

use std::time::{Duration, Instant};

use narou_rs::downloader::site_setting::BrowserTokenSetting;

use super::DevToolsSocket;

/// Browser エンドポイントへ接続する際の待ち時間。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 待ち時間の上限 (全体の期限)。
const OVERALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 受け入れる DevTools メッセージの上限 (受信前に設定する)。
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// 対象オリジンのページを選んで、ブラウザにトークンを発行させる。
///
/// - `websocket_url`: 親が検証済みの Browser エンドポイント (`ws://` のループバック)。
/// - `config`: サイト定義の `browser_token` (スクリプト URL・関数名・Cookie 名)。
/// - `target_url`: 対象ページの絶対 URL (このオリジンのページが使われる)。
///   遷移先が別ホストになる場合はその URL を渡す。
///
/// トークンの値は返さず、Cookie が存在したかどうかだけを返す。エラー文にも
/// トークンや CDP の例外内容は載せない。
pub(super) fn prepare_token(
    websocket_url: &str,
    config: &BrowserTokenSetting,
    target_url: &str,
) -> Result<(), String> {
    prepare_token_until(websocket_url, config, target_url, OVERALL_TIMEOUT)
}

/// 期限を区切った版 (テストでは短い期限を渡せる)。
fn prepare_token_until(
    websocket_url: &str,
    config: &BrowserTokenSetting,
    target_url: &str,
    timeout: Duration,
) -> Result<(), String> {
    let target = url::Url::parse(target_url).map_err(|_| "対象 URL が不正です".to_string())?;
    let deadline = Instant::now() + timeout;
    let mut socket = super::connect_devtools(websocket_url, CONNECT_TIMEOUT.min(timeout))?;
    socket.set_config(|config| {
        config.max_message_size = Some(MAX_MESSAGE_BYTES);
        config.max_frame_size = Some(MAX_MESSAGE_BYTES);
    });
    let mut cdp = Cdp::new(socket, deadline);
    let session = attach_page(&mut cdp, &target)?;
    issue_token(&mut cdp, &session, &config.function, &config.script_url, &target.origin().ascii_serialization())?;
    verify_cookie(&mut cdp, &config.cookie_name, &target)?;
    Ok(())
}


enum CdpError {
    Navigated,
    Failed(String),
}

impl From<String> for CdpError {
    fn from(message: String) -> Self { Self::Failed(message) }
}

impl From<CdpError> for String {
    fn from(error: CdpError) -> Self {
        match error {
            CdpError::Navigated => "ブラウザのページ遷移で実行コンテキストが切り替わりました".to_string(),
            CdpError::Failed(message) => message,
        }
    }
}

#[derive(serde::Serialize)]
struct CdpRequest<'a> {
    id: u64,
    method: &'a str,
    params: &'a serde_json::Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

/// DevTools プロトコルの最小クライアント (応答 id で取り違えない)。
struct Cdp {
    socket: DevToolsSocket,
    next_id: u64,
    deadline: Instant,
}

impl Cdp {
    fn new(socket: DevToolsSocket, deadline: Instant) -> Self {
        Self {
            socket,
            next_id: 1,
            deadline,
        }
    }

    /// コマンドを 1 つ送って応答を待つ。
    fn call(
        &mut self,
        method: &str,
        params: &serde_json::Value,
        session: Option<&str>,
    ) -> Result<serde_json::Value, CdpError> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CdpError::Failed("DevTools の応答待ちが上限に達しました".to_string()));
        }
        self.socket.get_mut().set_write_timeout(Some(remaining))
            .map_err(|_| "DevTools の通信期限を設定できません".to_string())?;
        let id = self.next_id;
        self.next_id += 1;
        let text = serde_json::to_string(&CdpRequest { id, method, params, session_id: session })
            .map_err(|_| "DevTools の要求を準備できません".to_string())?;
        self.socket
            .send(tungstenite::Message::text(text))
            .map_err(|error| format!("DevTools への送信に失敗しました: {error}"))?;
        self.wait_for(id, method)
    }

    /// 応答が返るまで読む (イベントは読み飛ばす)。
    fn wait_for(&mut self, id: u64, method: &str) -> Result<serde_json::Value, CdpError> {
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CdpError::Failed("DevTools の応答待ちが上限に達しました".to_string()));
            }
            self.socket
                .get_mut()
                .set_read_timeout(Some(remaining))
                .map_err(|error| format!("DevTools の通信期限を設定できません: {error}"))?;
            let message = self.socket.read().map_err(|error| match error {
                tungstenite::Error::Io(io_error)
                    if matches!(
                        io_error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    "DevTools の応答待ちが上限に達しました".to_string()
                }
                other => format!("DevTools からの受信に失敗しました: {other}"),
            })?;
            let tungstenite::Message::Text(text) = message else {
                continue;
            };
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                // エラーの内容は載せない (トークンを含むおそれがある)。
                let code = error
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default();
                if code == -32000 && error.get("message").and_then(serde_json::Value::as_str)
                    == Some("Execution context was destroyed.") {
                    return Err(CdpError::Navigated);
                }
                return Err(CdpError::Failed(format!(
                    "DevTools がエラーを返しました ({method}, code {code})"
                )));
            }
            return Ok(value.get_mut("result").map(serde_json::Value::take).unwrap_or(serde_json::Value::Null));
        }
    }
}

/// 対象オリジンのページを選び、flatten セッションで接続する。
fn attach_page(cdp: &mut Cdp, target: &url::Url) -> Result<String, String> {
    let targets = cdp.call("Target.getTargets", &serde_json::json!({}), None)?;
    let infos = targets
        .get("targetInfos")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "DevTools のページ一覧を取得できませんでした".to_string())?;
    for info in infos {
        if info.get("type").and_then(serde_json::Value::as_str) != Some("page") {
            continue;
        }
        let Some(page_url) = info.get("url").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(page) = url::Url::parse(page_url) else {
            continue;
        };
        if page.scheme() != target.scheme() || page.host_str() != target.host_str()
            || page.port_or_known_default() != target.port_or_known_default() {
            continue;
        }
        let Some(target_id) = info.get("targetId").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let attached = cdp.call(
            "Target.attachToTarget",
            &serde_json::json!({ "targetId": target_id, "flatten": true }),
            None,
        )?;
        return attached
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "対象ページへ接続できませんでした".to_string());
    }
    Err("対象のオリジンにページがありません".to_string())
}

/// ページの中でトークンを発行させる (1 個の IIFE で済ませる)。
///
/// 関数が既にあればスクリプトは読み込まない。関数は親のオブジェクトを `this` に
/// して呼び、返り値は捨てる (トークンが応答に混ざらないようにする)。
fn issue_token(
    cdp: &mut Cdp,
    session: &str,
    function: &str,
    script_url: &str,
    expected_origin: &str,
) -> Result<(), String> {
    let expression = format!(
        "({})({},{},{})",
        include_str!("browser_token.js"),
        serde_json::to_string(function).map_err(|_| "取得関数を準備できません".to_string())?,
        serde_json::to_string(script_url).map_err(|_| "取得スクリプトを準備できません".to_string())?,
        serde_json::to_string(expected_origin).map_err(|_| "対象オリジンを準備できません".to_string())?,
    );
    let params = serde_json::Value::Object(serde_json::Map::from_iter([
        ("expression".to_string(), serde_json::Value::String(expression)),
        ("awaitPromise".to_string(), serde_json::Value::Bool(true)),
        ("returnByValue".to_string(), serde_json::Value::Bool(true)),
    ]));
    let result = loop {
        match cdp.call("Runtime.evaluate", &params, Some(session)) {
            Ok(result) if result.pointer("/result/value/stage").and_then(serde_json::Value::as_str) != Some("context_pending") => break result,
            Ok(_) | Err(CdpError::Navigated) => {}
            Err(error) => return Err(error.into()),
        }
        let remaining = cdp.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("ブラウザのページ遷移待ちが上限に達しました".to_string());
        }
        std::thread::sleep(Duration::from_millis(25).min(remaining));
    };
    if result.get("exceptionDetails").is_some() {
        return Err("トークンの取得に失敗しました (スクリプトが例外を投げました)".to_string());
    }
    let stage = result
        .get("result")
        .and_then(|value| value.get("value"))
        .and_then(|value| value.get("stage"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match stage {
        "ok" => Ok(()),
        "script_failed" => Err("トークン取得スクリプトの読み込みに失敗しました".to_string()),
        "missing" => Err("トークン取得スクリプトが対象の関数を公開しませんでした".to_string()),
        "wrong_origin" => Err("対象と異なるオリジンではトークンを取得しません".to_string()),
        _ => Err("トークンの取得に失敗しました".to_string()),
    }
}

/// 設定された Cookie がブラウザに存在するか確かめる (値には触れない)。
fn verify_cookie(cdp: &mut Cdp, cookie_name: &str, target: &url::Url) -> Result<(), String> {
    let cookies = cdp.call("Storage.getCookies", &serde_json::json!({}), None)?;
    let list = cookies
        .get("cookies")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cookie の一覧を取得できませんでした".to_string())?;
    let host = target.host_str().unwrap_or_default();
    let found = list.iter().any(|cookie| {
        cookie.get("name").and_then(serde_json::Value::as_str) == Some(cookie_name)
            && cookie
                .get("domain")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|domain| domain_matches(domain, host))
    });
    if found {
        return Ok(());
    }
    Err(format!(
        "トークンの Cookie ({cookie_name}) がブラウザに保存されませんでした"
    ))
}

/// Cookie のドメインが対象ホストの配下か。
fn domain_matches(domain: &str, host: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    if domain.is_empty() {
        return false;
    }
    if domain.eq_ignore_ascii_case(host) {
        return true;
    }
    let Some(offset) = host.len().checked_sub(domain.len() + 1) else {
        return false;
    };
    host.as_bytes()[offset] == b'.'
        && host.get(offset + 1..).is_some_and(|suffix| suffix.eq_ignore_ascii_case(domain))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const SCRIPT_URL: &str = "https://cdn.example.test/challenge.js";
    const FUNCTION: &str = "ExampleIntegration.getToken";
    const COOKIE_NAME: &str = "example-token";
    const TARGET_URL: &str = "https://novels.example.test/read/1";
    const PAGE_URL: &str = "https://novels.example.test/read/1";

    /// 要求を記録しつつ応答を返す DevTools の代役 (実 WebSocket で受ける)。
    struct Fixture {
        websocket_url: String,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl Fixture {
        /// 応答を 1 つずつ返す関数を渡す。`None` を返すと応答しない (待ち続ける)。
        fn start(
            handler: impl Fn(&str, &serde_json::Value) -> Option<serde_json::Value>
            + Send
            + 'static,
        ) -> Self {
            Self::start_with_errors(move |method, request| handler(method, request).map(Ok))
        }

        fn start_with_errors(
            handler: impl Fn(&str, &serde_json::Value) -> Option<Result<serde_json::Value, serde_json::Value>>
                + Send + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let port = listener.local_addr().expect("addr").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&requests);
            std::thread::spawn(move || {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let Ok(mut socket) = tungstenite::accept(stream) else {
                    return;
                };
                loop {
                    let Ok(tungstenite::Message::Text(text)) = socket.read() else {
                        break;
                    };
                    let request: serde_json::Value = match serde_json::from_str(text.as_str()) {
                        Ok(request) => request,
                        Err(_) => continue,
                    };
                    let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let method = request
                        .get("method")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Ok(mut entries) = log.lock() {
                        entries.push(request.clone());
                    }
                    match handler(&method, &request) {
                        Some(result) => {
                            let response = match result {
                                Ok(result) => serde_json::json!({ "id": id, "result": result }),
                                Err(error) => serde_json::json!({ "id": id, "error": error }),
                            };
                            if socket
                                .send(tungstenite::Message::text(response.to_string()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        None => {
                            std::thread::sleep(Duration::from_secs(30));
                            break;
                        }
                    }
                }
            });
            Self {
                websocket_url: format!("ws://127.0.0.1:{port}/devtools/browser/test"),
                requests,
            }
        }

        /// 受け取ったメソッド名。
        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .map(|requests| {
                    requests
                        .iter()
                        .filter_map(|request| {
                            request
                                .get("method")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    fn setting() -> BrowserTokenSetting {
        serde_json::from_value(serde_json::json!({
            "script_url": SCRIPT_URL,
            "function": FUNCTION,
            "cookie_name": COOKIE_NAME,
        }))
        .expect("setting")
    }

    /// 正常な DevTools の応答 (ページは対象オリジンにある)。
    fn browser(
        page_url: &str,
    ) -> impl Fn(&str, &serde_json::Value) -> Option<serde_json::Value> + Send + 'static {
        let page_url = page_url.to_string();
        move |method, _request| {
            Some(match method {
                "Target.getTargets" => serde_json::json!({
                    "targetInfos": [
                        { "targetId": "page-1", "type": "page", "url": page_url },
                    ]
                }),
                "Target.attachToTarget" => serde_json::json!({ "sessionId": "session-1" }),
                "Runtime.evaluate" => serde_json::json!({
                    "result": { "type": "object", "value": { "stage": "ok" } }
                }),
                "Storage.getCookies" => serde_json::json!({
                    "cookies": [
                        { "name": COOKIE_NAME, "domain": "novels.example.test", "value": "issued" },
                    ]
                }),
                _ => serde_json::json!({}),
            })
        }
    }


    #[test]
    fn a_page_on_another_origin_is_refused() {
        let fixture = Fixture::start(browser("https://other.example.test/read/1"));
        let result = prepare_token(&fixture.websocket_url, &setting(), TARGET_URL);
        let message = result.expect_err("別オリジンのページを使ってはいけない");
        assert!(message.contains("ページがありません"), "{message}");
        assert!(
            !fixture
                .methods()
                .iter()
                .any(|method| method == "Runtime.evaluate"),
            "別オリジンのページには触れない: {:?}",
            fixture.methods()
        );
    }

    #[test]
    fn a_missing_cookie_is_reported() {
        let fixture = Fixture::start(|method, _request| {
            Some(match method {
                "Target.getTargets" => serde_json::json!({
                    "targetInfos": [
                        { "targetId": "page-1", "type": "page", "url": PAGE_URL },
                    ]
                }),
                "Target.attachToTarget" => serde_json::json!({ "sessionId": "session-1" }),
                "Runtime.evaluate" => serde_json::json!({
                    "result": { "type": "object", "value": { "stage": "ok" } }
                }),
                "Storage.getCookies" => serde_json::json!({
                    "cookies": [
                        { "name": "other-cookie", "domain": "novels.example.test", "value": "x" },
                    ]
                }),
                _ => serde_json::json!({}),
            })
        });
        let result = prepare_token(&fixture.websocket_url, &setting(), TARGET_URL);
        let message = result.expect_err("Cookie が無ければ失敗する");
        assert!(message.contains(COOKIE_NAME), "{message}");
    }

    #[test]
    fn a_cookie_of_another_site_is_not_counted() {
        let fixture = Fixture::start(|method, _request| {
            Some(match method {
                "Target.getTargets" => serde_json::json!({
                    "targetInfos": [
                        { "targetId": "page-1", "type": "page", "url": PAGE_URL },
                    ]
                }),
                "Target.attachToTarget" => serde_json::json!({ "sessionId": "session-1" }),
                "Runtime.evaluate" => serde_json::json!({
                    "result": { "type": "object", "value": { "stage": "ok" } }
                }),
                "Storage.getCookies" => serde_json::json!({
                    "cookies": [
                        {
                            "name": COOKIE_NAME,
                            "domain": "unrelated.example.test",
                            "value": "issued",
                        },
                    ]
                }),
                _ => serde_json::json!({}),
            })
        });
        let result = prepare_token(&fixture.websocket_url, &setting(), TARGET_URL);
        let message = result.expect_err("別サイトの Cookie は数えない");
        assert!(message.contains(COOKIE_NAME), "{message}");
    }

    #[test]
    fn an_sdk_exception_does_not_leak_the_token() {
        let fixture = Fixture::start(|method, _request| {
            Some(match method {
                "Target.getTargets" => serde_json::json!({
                    "targetInfos": [
                        { "targetId": "page-1", "type": "page", "url": PAGE_URL },
                    ]
                }),
                "Target.attachToTarget" => serde_json::json!({ "sessionId": "session-1" }),
                "Runtime.evaluate" => serde_json::json!({
                    "result": { "type": "object" },
                    "exceptionDetails": { "text": "rejected: super-secret-token" },
                }),
                _ => serde_json::json!({}),
            })
        });
        let result = prepare_token(&fixture.websocket_url, &setting(), TARGET_URL);
        let message = result.expect_err("SDK の例外は失敗として扱う");
        assert!(message.contains("トークンの取得に失敗しました"), "{message}");
        assert!(
            !message.contains("super-secret-token"),
            "トークンがエラー文に混ざっている: {message}"
        );
    }

    #[test]
    fn a_script_failure_is_reported() {
        let fixture = Fixture::start(|method, _request| {
            Some(match method {
                "Target.getTargets" => serde_json::json!({
                    "targetInfos": [
                        { "targetId": "page-1", "type": "page", "url": PAGE_URL },
                    ]
                }),
                "Target.attachToTarget" => serde_json::json!({ "sessionId": "session-1" }),
                "Runtime.evaluate" => serde_json::json!({
                    "result": { "type": "object", "value": { "stage": "script_failed" } }
                }),
                _ => serde_json::json!({}),
            })
        });
        let result = prepare_token(&fixture.websocket_url, &setting(), TARGET_URL);
        let message = result.expect_err("スクリプトの読み込み失敗は失敗として扱う");
        assert!(
            message.contains("スクリプトの読み込みに失敗しました"),
            "{message}"
        );
    }

    #[test]
    fn a_stalled_devtools_is_bounded() {
        let fixture = Fixture::start(|_method, _request| None);
        let started = Instant::now();
        let result = prepare_token_until(
            &fixture.websocket_url,
            &setting(),
            TARGET_URL,
            Duration::from_secs(2),
        );
        let elapsed = started.elapsed();
        let message = result.expect_err("応答が途切れたら失敗する");
        assert!(message.contains("上限"), "{message}");
        assert!(
            elapsed < Duration::from_secs(20),
            "期限内に打ち切ること: {elapsed:?}"
        );
    }

    #[test]
    fn navigation_does_not_hide_a_later_terminal_protocol_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let normal = browser(PAGE_URL);
        let fixture = Fixture::start_with_errors(move |method, request| {
            if method == "Runtime.evaluate" {
                return Some(Err(if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    serde_json::json!({ "code": -32000, "message": "Execution context was destroyed." })
                } else {
                    serde_json::json!({ "code": -32601, "message": "unsupported: private-token" })
                }));
            }
            normal(method, request).map(Ok)
        });
        let error = prepare_token(&fixture.websocket_url, &setting(), TARGET_URL).unwrap_err();
        assert!(error.contains("code -32601"), "{error}");
        assert!(!error.contains("private-token"), "{error}");
    }

    #[test]
    fn continuing_navigation_still_obeys_the_original_deadline() {
        let normal = browser(PAGE_URL);
        let fixture = Fixture::start_with_errors(move |method, request| {
            if method == "Runtime.evaluate" {
                return Some(Err(serde_json::json!({
                    "code": -32000, "message": "Execution context was destroyed."
                })));
            }
            normal(method, request).map(Ok)
        });
        let started = Instant::now();
        let error = prepare_token_until(&fixture.websocket_url, &setting(), TARGET_URL, Duration::from_millis(150)).unwrap_err();
        assert!(error.contains("上限"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "navigation must not reset the deadline");
    }
}
