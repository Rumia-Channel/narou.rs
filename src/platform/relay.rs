//! SORAHOST 取得リレー (`scripts/sorahost-proxy/`) のプロトコル・コーデック。
//!
//! Cloudflare Workers の Fetch API が弾かれるサイト (ハーメルン等) を、
//! SORAHOST (PteWorker) 上の踏み台から中継取得するためのリクエスト組み立てと
//! 応答パースだけを提供する。トランスポート (Fetch / `connect()` 生ソケット)
//! は呼び出し側が持つ。
//!
//! プロトコル (`worker/worker.mjs` と `server.mjs` が受け付ける形):
//!
//! ```text
//! GET|POST /proxy
//!   GET : ?url=<URL エンコード>&headers=<base64(JSON オブジェクト)>&redirect=follow|manual
//!   POST: {"url": "...", "headers": {...}, "redirect": "follow"|"manual"}
//! 認証: X-Proxy-Token: <PROXY_TOKEN>
//! 応答: {"status": 200, "contentType": "...", "location": "...",
//!        "via": "curl", "body": "<base64>"}
//! 失敗: {"status": <上流>, "error": "..."} /
//!       HTTP 403 {"error":"forbidden"} / 500 {"error":"PROXY_TOKEN is not set"}
//! ```
//!
//! `headers` はサイト定義 (`FetchPolicy::for_site`) が組み立てた
//! `Accept` / `Accept-Language` / `Sec-Fetch-*` / `Cookie` 等をそのまま転送する
//! ためのもの。生ソケット経路では URL 長を抑えたいのでクエリ側は base64 に
//! 畳み、キーはソートして安定化させる。
//!
//! バイト列と文字列だけを扱う純粋ロジックのためホストでユニットテストできる
//! (Worker クレートのテストは wasm 専用で実行されない)。外部クレートは
//! 非 optional の `serde_json` と `url` のみに依存する。

use crate::platform::http::RedirectMode;

/// 踏み台への認証ヘッダ名。
pub const TOKEN_HEADER: &str = "x-proxy-token";

/// リレー呼び出しの失敗理由。
///
/// 踏み台が上流の取得に失敗した場合 (`Rejected`) と、応答そのものが
/// プロトコルに合わなかった場合 (`Malformed`) を区別する。呼び出し側は
/// `Malformed` / 送信失敗なら元の fetch 経路の結果をそのまま使い回せる。
#[derive(Debug)]
pub enum RelayError {
    /// リクエストを組み立てられない (URL・トークン・ヘッダが不正)。
    InvalidRequest(String),
    /// 踏み台が呼び出しを拒否したか、踏み台上での上流取得が失敗した。
    Rejected {
        /// 踏み台自身の HTTP ステータス (403, 502, …)。
        http_status: u16,
        /// エラーオブジェクトが `status` を内包していた場合の上流ステータス。
        upstream_status: Option<u16>,
        /// 踏み台が返した `error` メッセージ。
        message: String,
    },
    /// 応答がプロトコルに合わない (JSON でない、必須フィールド欠落など)。
    Malformed(String),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::InvalidRequest(message) => write!(f, "invalid relay request: {message}"),
            RelayError::Rejected {
                http_status,
                upstream_status,
                message,
            } => match upstream_status {
                Some(upstream) => write!(
                    f,
                    "relay request rejected (relay HTTP {http_status}, upstream {upstream}): {message}"
                ),
                None => write!(f, "relay request rejected (relay HTTP {http_status}): {message}"),
            },
            RelayError::Malformed(message) => write!(f, "malformed relay response: {message}"),
        }
    }
}

impl std::error::Error for RelayError {}

impl From<RelayError> for crate::error::NarouError {
    fn from(error: RelayError) -> Self {
        crate::error::NarouError::Http(error.to_string())
    }
}

/// 組み立て済みのリレー呼び出し。トランスポートは GET または POST のどちらを
/// 使ってもよい (踏み台は同一ペイロードを両方で受け付ける)。
#[derive(Debug, Clone)]
pub struct RelayRequest {
    /// `GET /proxy?...` のフル URL (`base_url` のオリジン + `/proxy`)。
    url: String,
    /// `X-Proxy-Token` に入れるトークン。
    token: String,
    /// `POST /proxy` 用の JSON ボディ。
    post_body: String,
}

impl RelayRequest {
    /// リレー呼び出しを組み立てる。
    ///
    /// - `base_url`: 踏み台のベース URL (`http://<IP>:<port>/`)。オリジン
    ///   (scheme://host:port) だけが使われ、パスがあっても `/proxy` に固定される。
    /// - `token`: `PROXY_TOKEN`。空・制御文字入りは拒否する。
    /// - `target_url`: 中継してほしい取得先。`http(s)` 以外は踏み台が 400 を
    ///   返すため、ここで弾く。
    /// - `headers`: 取得先に転送するヘッダ (例: `FetchPolicy::headers()`)。
    ///   CR/LF 等の制御文字を含む値・トークンでない名前は拒否する。
    /// - `redirect`: `Follow` → `redirect=follow`、`Manual` → `redirect=manual`。
    pub fn new(
        base_url: &str,
        token: &str,
        target_url: &str,
        headers: &[(String, String)],
        redirect: RedirectMode,
    ) -> std::result::Result<Self, RelayError> {
        let invalid = |message: String| RelayError::InvalidRequest(message);

        let base = url::Url::parse(base_url)
            .map_err(|e| invalid(format!("invalid relay base URL: {e}")))?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(invalid(format!(
                "relay base URL must be http(s) with a host: {base_url}"
            )));
        }
        let origin = base.origin().ascii_serialization();
        if origin == "null" {
            return Err(invalid(format!(
                "relay base URL must be http(s) with a host: {base_url}"
            )));
        }

        // 踏み台は `^https?://` でしか受け付けないので先に弾く。
        let target = url::Url::parse(target_url)
            .map_err(|e| invalid(format!("invalid target URL: {e}")))?;
        if !matches!(target.scheme(), "http" | "https") || target.host_str().is_none() {
            return Err(invalid(format!(
                "target URL must be http(s) with a host: {target_url}"
            )));
        }

        if token.is_empty() || !is_safe_header_value(token) {
            return Err(invalid("proxy token is empty or unsafe".to_string()));
        }

        // JSON オブジェクトに畳む。serde_json::Map は BTreeMap なのでキー順が
        // 安定し、同じ入力から常に同じ URL が生成される。
        let mut header_map = serde_json::Map::new();
        for (name, value) in headers {
            if !is_safe_header_name(name) || !is_safe_header_value(value) {
                return Err(invalid(format!("unsafe header: {name:?}")));
            }
            header_map.insert(name.clone(), serde_json::Value::String(value.clone()));
        }
        let headers_value = serde_json::Value::Object(header_map);
        let headers_json = headers_value.to_string();

        let redirect_mode = match redirect {
            RedirectMode::Follow => "follow",
            RedirectMode::Manual => "manual",
        };

        let post_body = serde_json::json!({
            "url": target_url,
            "headers": headers_value,
            "redirect": redirect_mode,
        })
        .to_string();

        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("url", target_url);
        // 空オブジェクトを送っても踏み台側の解釈は同じなので省略して URL を
        // 短く保つ (生ソケット経路では URL 長が効く)。
        if !headers.is_empty() {
            query.append_pair("headers", &base64_encode(headers_json.as_bytes()));
        }
        query.append_pair("redirect", redirect_mode);

        Ok(Self {
            url: format!("{origin}/proxy?{}", query.finish()),
            token: token.to_string(),
            post_body,
        })
    }

    /// `GET /proxy?url=...&headers=...&redirect=...` のフル URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// `X-Proxy-Token` ヘッダに入れる値。
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `POST /proxy` に送る JSON ボディ (GET と同じペイロード)。
    pub fn post_body(&self) -> &str {
        &self.post_body
    }
}

/// 踏み台の JSON 応答をデコードしたもの。
#[derive(Debug, Clone)]
pub struct RelayResponse {
    /// 上流サイトの HTTP ステータス (踏み台自身の応答ではない)。
    pub status: u16,
    /// `contentType`。空文字・欠落は `None`。
    pub content_type: Option<String>,
    /// `location` (redirect=manual 時の 3xx 応答など)。
    pub location: Option<String>,
    /// 踏み台がどのクライアントで取ったか (`curl` / `worker-fetch` / 診断情報)。
    pub via: Option<String>,
    /// base64 をデコード済みのレスポンスボディ。
    pub body: Vec<u8>,
}

impl RelayResponse {
    /// 踏み台の応答ボディをパースする。`http_status` はトランスポートが観測
    /// した踏み台自身の HTTP ステータスで、エラーの報告にだけ使う。
    ///
    /// `error` フィールドを含む応答は `Rejected`、JSON でない・必須フィールド
    /// が欠けている応答は `Malformed` を返す。
    pub fn parse(http_status: u16, bytes: &[u8]) -> std::result::Result<Self, RelayError> {
        let malformed = |message: String| RelayError::Malformed(message);

        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|e| malformed(format!("response is not JSON: {e}")))?;

        if let Some(message) = value.get("error").and_then(|v| v.as_str()) {
            return Err(RelayError::Rejected {
                http_status,
                upstream_status: value
                    .get("status")
                    .and_then(|v| v.as_u64())
                    .and_then(|s| u16::try_from(s).ok()),
                message: message.to_string(),
            });
        }

        let status = value
            .get("status")
            .and_then(|v| v.as_u64())
            .and_then(|s| u16::try_from(s).ok())
            .ok_or_else(|| malformed("response has no numeric `status`".to_string()))?;
        let body64 = value
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| malformed("response has no base64 `body`".to_string()))?;

        let nonempty_string = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        Ok(Self {
            status,
            content_type: nonempty_string("contentType"),
            location: nonempty_string("location"),
            via: nonempty_string("via"),
            body: base64_decode(body64)?,
        })
    }
}

/// `http_policy` と同じ規則: ヘッダ名は RFC 9110 のトークン相当
/// (`[A-Za-z0-9_-]`、64 文字まで)。`downloader` は feature-gated で platform
/// から参照できないため、ここで同じ規則を守る。
fn is_safe_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// `is_safe_header_value` と同じ規則: 値に ASCII 制御文字 (CR/LF 含む) を
/// 許さない。
fn is_safe_header_value(value: &str) -> bool {
    !value.bytes().any(|byte| byte.is_ascii_control())
}

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 標準 base64 (padding あり)。踏み台は `btoa` / `Buffer.toString("base64")`
/// の出力を返すので、URL セーフ変種ではなくこちらを使う。
fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let acc = b0 << 16 | b1 << 8 | b2;
        out.push(B64_ALPHABET[(acc >> 18) as usize & 0x3F] as char);
        out.push(B64_ALPHABET[(acc >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(acc >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[acc as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

/// `base64_encode` の逆。`=` は末尾のチャンクの位置 2/3 にだけ許す
/// (`btoa` 出力の形)。空白・不正文字・壊れたパディングは `Malformed`。
fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, RelayError> {
    let malformed = |message: String| RelayError::Malformed(message);

    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a' + 26)),
            b'0'..=b'9' => Some(u32::from(byte - b'0' + 52)),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(malformed(format!(
            "base64 length {} is not a multiple of 4",
            bytes.len()
        )));
    }
    let chunks: Vec<&[u8]> = bytes.chunks(4).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in chunks.iter().enumerate() {
        let data_len = chunk.iter().take_while(|&&b| b != b'=').count();
        if data_len < 2
            || !chunk[data_len..].iter().all(|&b| b == b'=')
            || (data_len < 4 && index + 1 != chunks.len())
        {
            return Err(malformed("invalid base64 padding".to_string()));
        }
        let mut acc = 0u32;
        for (i, &byte) in chunk.iter().enumerate() {
            let sextet = if i < data_len {
                sextet(byte).ok_or_else(|| {
                    malformed(format!("invalid base64 byte 0x{byte:02X}"))
                })?
            } else {
                0
            };
            acc = acc << 6 | sextet;
        }
        out.push((acc >> 16) as u8);
        if data_len >= 3 {
            out.push((acc >> 8) as u8);
        }
        if data_len == 4 {
            out.push(acc as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const BASE: &str = "http://172.233.91.23:50071/";

    fn query(url: &str) -> BTreeMap<String, String> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn site_headers() -> Vec<(String, String)> {
        vec![
            ("Accept".into(), "text/html,application/xhtml+xml".into()),
            ("Accept-Language".into(), "ja-JP".into()),
            ("Sec-Fetch-Mode".into(), "navigate".into()),
            (
                "Cookie".into(),
                "over18=yes; session=a b=%2F".into(),
            ),
        ]
    }

    #[test]
    fn request_url_round_trips_target_and_headers() {
        let target = "https://syosetu.org/novel/426898/1.html?x=1&y=%2F";
        let request =
            RelayRequest::new(BASE, "secret-token", target, &site_headers(), RedirectMode::Manual)
                .unwrap();

        assert!(request.url().starts_with("http://172.233.91.23:50071/proxy?"));
        assert_eq!(request.token(), "secret-token");

        let params = query(request.url());
        assert_eq!(params["url"], target);
        assert_eq!(params["redirect"], "manual");

        // 踏み台側の復号 (atob → JSON.parse) と同じ手順で往復させる。
        let decoded: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&base64_decode(&params["headers"]).unwrap()).unwrap();
        assert_eq!(decoded["Accept"], "text/html,application/xhtml+xml");
        assert_eq!(decoded["Cookie"], "over18=yes; session=a b=%2F");
        assert_eq!(decoded.len(), 4);
    }

    #[test]
    fn request_url_is_stable_regardless_of_header_order() {
        let mut reversed = site_headers();
        reversed.reverse();
        let a = RelayRequest::new(BASE, "t", "https://example.com/", &site_headers(),
            RedirectMode::Follow)
            .unwrap();
        let b = RelayRequest::new(BASE, "t", "https://example.com/", &reversed,
            RedirectMode::Follow)
            .unwrap();
        assert_eq!(a.url(), b.url());
        assert_eq!(query(a.url())["redirect"], "follow");
    }

    #[test]
    fn request_without_headers_omits_the_param() {
        let request = RelayRequest::new(
            BASE,
            "t",
            "https://example.com/",
            &[],
            RedirectMode::Follow,
        )
        .unwrap();
        let params = query(request.url());
        assert!(!params.contains_key("headers"));
        // POST 側も同じペイロードを持つ (空オブジェクト)。
        let body: serde_json::Value = serde_json::from_str(request.post_body()).unwrap();
        assert_eq!(body["url"], "https://example.com/");
        assert_eq!(body["headers"], serde_json::json!({}));
        assert_eq!(body["redirect"], "follow");
    }

    #[test]
    fn request_rejects_unsafe_inputs() {
        let bad = [
            vec![("Bad\nName".to_string(), "v".to_string())],
            vec![("bad name".to_string(), "v".to_string())],
            vec![("Ok".to_string(), "v\r\nInjected: x".to_string())],
        ];
        for headers in bad {
            assert!(matches!(
                RelayRequest::new(BASE, "t", "https://example.com/", &headers, RedirectMode::Follow),
                Err(RelayError::InvalidRequest(_))
            ));
        }
        assert!(matches!(
            RelayRequest::new(
                BASE,
                "to\nken",
                "https://example.com/",
                &[],
                RedirectMode::Follow
            ),
            Err(RelayError::InvalidRequest(_))
        ));
        assert!(matches!(
            RelayRequest::new(
                BASE,
                "t",
                "ftp://example.com/",
                &[],
                RedirectMode::Follow
            ),
            Err(RelayError::InvalidRequest(_))
        ));
        assert!(matches!(
            RelayRequest::new(
                "file:///etc/passwd",
                "t",
                "https://example.com/",
                &[],
                RedirectMode::Follow
            ),
            Err(RelayError::InvalidRequest(_))
        ));
    }

    #[test]
    fn response_decodes_utf8_and_binary_bodies() {
        let utf8 = "こんにちは、世界".as_bytes();
        let response = format!(
            r#"{{"status":200,"contentType":"text/html; charset=UTF-8","via":"curl","body":"{}"}}"#,
            base64_encode(utf8)
        );
        let parsed = RelayResponse::parse(200, response.as_bytes()).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.content_type.as_deref(), Some("text/html; charset=UTF-8"));
        assert_eq!(parsed.via.as_deref(), Some("curl"));
        assert_eq!(parsed.body, utf8);

        // 非 UTF-8 (Shift_JIS など) もバイト列として復元できる。
        let binary: Vec<u8> = (0u8..=255).collect();
        let response = format!(
            r#"{{"status":404,"location":"/other","body":"{}"}}"#,
            base64_encode(&binary)
        );
        let parsed = RelayResponse::parse(200, response.as_bytes()).unwrap();
        assert_eq!(parsed.status, 404);
        assert_eq!(parsed.location.as_deref(), Some("/other"));
        assert_eq!(parsed.body, binary);
    }

    #[test]
    fn response_errors_are_reported() {
        // 踏み台の拒否。
        match RelayResponse::parse(403, br#"{"error":"forbidden"}"#) {
            Err(RelayError::Rejected {
                http_status: 403,
                upstream_status: None,
                message,
            }) => assert_eq!(message, "forbidden"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        // 上流ステータスを内包するエラー。
        match RelayResponse::parse(
            502,
            br#"{"status":503,"error":"fetch failed: boom"}"#,
        ) {
            Err(RelayError::Rejected {
                http_status: 502,
                upstream_status: Some(503),
                ..
            }) => {}
            other => panic!("expected Rejected, got {other:?}"),
        }
        // JSON でない応答 (Cloudflare のチャレンジ HTML 等)。
        assert!(matches!(
            RelayResponse::parse(200, b"<html>challenge</html>"),
            Err(RelayError::Malformed(_))
        ));
        // status 欠落・非数値・body 欠落・body 非文字列。
        for bad in [
            br#"{"contentType":"text/html","body":"QQ=="}"#.as_slice(),
            br#"{"status":"200","body":"QQ=="}"#.as_slice(),
            br#"{"status":200}"#.as_slice(),
            br#"{"status":200,"body":123}"#.as_slice(),
        ] {
            assert!(
                matches!(RelayResponse::parse(200, bad), Err(RelayError::Malformed(_))),
                "{bad:?} should be malformed"
            );
        }
    }

    #[test]
    fn base64_round_trip_and_rejects_garbage() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"he"), "aGU=");
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGU=").unwrap(), b"he");
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());

        for garbage in ["AAA", "A===", "AAAA=AAA", "!!!!", "aGVs bG8=", "aGVsbG8"] {
            assert!(
                matches!(base64_decode(garbage), Err(RelayError::Malformed(_))),
                "{garbage:?} should be malformed"
            );
        }
        // デコード失敗は parse でも Malformed になる。
        assert!(matches!(
            RelayResponse::parse(200, br#"{"status":200,"body":"!!!"}"#),
            Err(RelayError::Malformed(_))
        ));
    }
}
