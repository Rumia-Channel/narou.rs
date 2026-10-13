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
//!   GET : ?url=<URL エンコード>&headers=<base64(JSON オブジェクト)>
//!   POST: {"url": "...", "headers": {...}}
//! 認証: X-Proxy-Token: <SORAHOST_PROXY_KEY>
//! 応答: {"status": 200, "contentType": "...", "location": "...",
//!        "setCookie": ["..."], "via": "curl", "body": "<base64>"}
//! 失敗: {"status": <上流>, "error": "..."} /
//!       HTTP 403 {"error":"forbidden"} / 500 {"error":"SORAHOST_PROXY_KEY is not set"}
//! ```
//!
//! 踏み台は 1 リクエストにつき上流を 1 回だけ取得し、リダイレクトは辿らない
//! (`Location` を `location` に載せて返す)。チェーンを辿る責務は呼び出し側
//! (Worker) にあり、hop ごとの検証と資格情報の剥がしはそちらの共通規則で
//! 行う。`curl -L` のような無条件追跡は資格情報を私設アドレスへ運びうるため、
//! 踏み台側には置かない。
//!
//! `headers` はサイト定義 (`FetchPolicy::for_site`) が組み立てた
//! `Accept` / `Accept-Language` / `Sec-Fetch-*` / `Cookie` 等をそのまま転送する
//! ためのもの。同じ名前が複数回あるときは値を配列にして順序を保つ
//! (`{"X": ["a", "b"]}`)。呼び出し側は POST の JSON 本文を使い、
//! Cookie 等を URL に載せず、配備先の request-line 上限も避ける。
//!
//! バイト列と文字列だけを扱う純粋ロジックのためホストでユニットテストできる
//! (Worker クレートのテストは wasm 専用で実行されない)。外部クレートは
//! 非 optional の `serde_json` と `url` のみに依存する。

use super::http::{HttpRequest, RedirectMode, is_safe_header_name, is_safe_header_value};
use super::http1::MAX_HTTP1_HEAD;

/// UTF-8 JSON envelope budget, excluding only the base64 body's characters.
pub const MAX_RELAY_METADATA: usize = MAX_HTTP1_HEAD * 7;

/// Maximum JSON response body on the relay wire. Raw headers can expand
/// sixfold under JSON escaping; the remaining block covers the envelope and
/// diagnostics. The outer HTTP response head is bounded independently.
pub fn response_wire_limit(decoded_body_limit: usize) -> usize {
    decoded_body_limit.div_ceil(3).saturating_mul(4)
        .saturating_add(MAX_RELAY_METADATA)
}

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

/// POST の JSON 本文で送る、組み立て済みのリレー呼び出し。
pub struct RelayRequest {
    /// 踏み台のオリジン + `/proxy`。
    url: String,
    /// `X-Proxy-Token` に入れるトークン。
    token: String,
    /// `POST /proxy` 用の JSON ボディ。
    body: Vec<u8>,
}

impl RelayRequest {
    /// リレー呼び出しを組み立てる。
    ///
    /// - `base_url`: 踏み台のベース URL (`http://<IP>:<port>/`)。オリジン
    ///   (scheme://host:port) だけが使われ、パスがあっても `/proxy` に固定される。
    /// - `token`: `SORAHOST_PROXY_KEY`。空・制御文字入りは拒否する。
    /// - `target_url`: 中継してほしい取得先。`http(s)` 以外は踏み台が 400 を
    ///   返すため、ここで弾く。
    /// - `headers`: 取得先に転送するヘッダ (例: `FetchPolicy::headers()`)。
    ///   同名の値は順序を保ったまま運ぶ (JSON では値の配列になる)。CR/LF 等の
    ///   制御文字を含む値・RFC トークンでない名前は拒否する。
    pub fn new(
        base_url: &str,
        token: &str,
        target_url: &str,
        headers: &[(String, String)],
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

        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::io::Write;

        #[derive(serde::Serialize)]
        #[serde(untagged)]
        enum HeaderValues<'a> {
            One(&'a str),
            Many(Vec<&'a str>),
        }
        #[derive(serde::Serialize)]
        struct Payload<'a> {
            url: &'a str,
            headers: BTreeMap<Cow<'a, str>, HeaderValues<'a>>,
        }
        struct JsonBody(Vec<u8>);
        impl Write for JsonBody {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.len().saturating_add(bytes.len()) > MAX_HTTP1_HEAD {
                    return Err(std::io::Error::other("relay request JSON exceeds 64 KiB"));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }

        let mut header_map = BTreeMap::new();
        for (name, value) in headers {
            if !is_safe_header_name(name) || !is_safe_header_value(value) {
                return Err(invalid(format!("unsafe header: {name:?}")));
            }
            let name = if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                Cow::Owned(name.to_ascii_lowercase())
            } else {
                Cow::Borrowed(name.as_str())
            };
            match header_map.entry(name) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(HeaderValues::One(value.as_str()));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    match entry.get_mut() {
                        HeaderValues::One(first) => {
                            let first = *first;
                            *entry.get_mut() = HeaderValues::Many(vec![first, value.as_str()]);
                        }
                        HeaderValues::Many(values) => values.push(value.as_str()),
                    }
                }
            }
        }
        let mut body = JsonBody(Vec::with_capacity(1024));
        serde_json::to_writer(&mut body, &Payload { url: target_url, headers: header_map })
            .map_err(|error| invalid(error.to_string()))?;
        Ok(Self {
            url: format!("{origin}/proxy"),
            token: token.to_string(),
            body: body.0,
        })
    }

    /// 所有している本文とトークンを移し、1 ホップだけの HTTP 要求にする。
    pub fn into_request(self) -> HttpRequest {
        let mut request = HttpRequest::post(self.url, self.body);
        request.redirect = RedirectMode::Manual;
        request.trusted_endpoint = true;
        request.headers = vec![
            (TOKEN_HEADER.to_string(), self.token),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        request
    }
}

/// 踏み台の JSON 応答をデコードしたもの。
#[derive(Debug, Clone)]
pub struct RelayResponse {
    /// 上流サイトの HTTP ステータス (踏み台自身の応答ではない)。
    pub status: u16,
    /// `contentType`。空文字・欠落は `None`。
    pub content_type: Option<String>,
    /// `location` (3xx 応答のリダイレクト先。踏み台は辿らない)。
    pub location: Option<String>,
    /// `setCookie` の値 (上流が複数返したときは受信順)。
    ///
    /// ログインセッションの更新は 1 本も落とせないため、他のヘッダと違って
    /// 名前つきで運ぶ。
    pub set_cookie: Vec<String>,
    /// 踏み台がどのクライアントで取ったか (`curl` / `worker-fetch` / 診断情報)。
    pub via: Option<String>,
    /// base64 をデコード済みのレスポンスボディ。
    pub body: Vec<u8>,
}

/// Strings borrow the wire envelope when JSON needs no unescaping (in
/// particular the large base64 body). Non-string optional metadata retains
/// the protocol's existing ignore semantics.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RelayString<'a> {
    Text(#[serde(borrow)] std::borrow::Cow<'a, str>),
    Other(serde::de::IgnoredAny),
}

impl RelayString<'_> {
    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text.as_ref()),
            Self::Other(_) => None,
        }
    }

    fn into_nonempty_string(self) -> Option<String> {
        match self {
            Self::Text(text) if !text.is_empty() => Some(text.into_owned()),
            _ => None,
        }
    }
}

#[derive(serde::Deserialize)]
struct RelayEnvelope<'a> {
    status: Option<serde_json::Value>,
    #[serde(borrow)]
    body: Option<RelayString<'a>>,
    #[serde(borrow)]
    error: Option<RelayString<'a>>,
    #[serde(rename = "contentType", borrow)]
    content_type: Option<RelayString<'a>>,
    #[serde(borrow)]
    location: Option<RelayString<'a>>,
    #[serde(rename = "setCookie")]
    set_cookie: Option<serde_json::Value>,
    #[serde(borrow)]
    via: Option<RelayString<'a>>,
}

impl RelayResponse {
    /// 踏み台の応答ボディをパースする。`http_status` はトランスポートが観測
    /// した踏み台自身の HTTP ステータスで、エラーの報告にだけ使う。
    ///
    /// `error` フィールドを含む応答は `Rejected`、JSON でない・必須フィールド
    /// が欠けている応答は `Malformed` を返す。`decoded_body_limit` は base64
    /// デコード前の長さ判定と、実際の復号バイト列の両方に適用する。
    pub fn parse(
        http_status: u16,
        bytes: &[u8],
        decoded_body_limit: usize,
    ) -> std::result::Result<Self, RelayError> {
        let malformed = |message: String| RelayError::Malformed(message);
        if bytes.len() > response_wire_limit(decoded_body_limit) {
            return Err(malformed("relay response envelope exceeds limit".into()));
        }
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            return Err(malformed("response is not a JSON object".into()));
        }

        let envelope: RelayEnvelope<'_> = serde_json::from_slice(bytes)
            .map_err(|e| malformed(format!("response is not JSON: {e}")))?;
        let status = envelope.status.as_ref()
            .and_then(serde_json::Value::as_u64)
            .and_then(|status| u16::try_from(status).ok());

        if let Some(message) = envelope.error.as_ref().and_then(RelayString::as_str) {
            if bytes.len() > MAX_RELAY_METADATA {
                return Err(malformed("relay response metadata exceeds limit".into()));
            }
            return Err(RelayError::Rejected {
                http_status,
                upstream_status: status,
                message: message.to_string(),
            });
        }

        let status = status
            .ok_or_else(|| malformed("response has no numeric `status`".to_string()))?;
        let body64 = envelope.body.as_ref()
            .and_then(RelayString::as_str)
            .ok_or_else(|| malformed("response has no base64 `body`".to_string()))?;
        if bytes.len().saturating_sub(body64.len()) > MAX_RELAY_METADATA {
            return Err(malformed("relay response metadata exceeds limit".into()));
        }


        // `setCookie` は文字列の配列。欠落 (古い踏み台) は空、文字列でない
        // 要素は無視する。
        let body = base64_decode(body64, decoded_body_limit)?;
        let set_cookie = match envelope.set_cookie {
            Some(serde_json::Value::Array(items)) => items.into_iter()
                .filter_map(|item| match item {
                    serde_json::Value::String(value) => Some(value),
                    _ => None,
                }).collect(),
            _ => Vec::new(),
        };

        Ok(Self {
            status,
            content_type: envelope.content_type.and_then(RelayString::into_nonempty_string),
            location: envelope.location.and_then(RelayString::into_nonempty_string),
            set_cookie,
            via: envelope.via.and_then(RelayString::into_nonempty_string),
            body,
        })
    }
}

#[cfg(test)]
const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg(test)]
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
fn base64_decode(input: &str, limit: usize) -> std::result::Result<Vec<u8>, RelayError> {
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
    if bytes.len() > limit.div_ceil(3).saturating_mul(4) {
        return Err(malformed("decoded relay body exceeds limit".into()));
    }
    if !bytes.len().is_multiple_of(4) {
        return Err(malformed(format!(
            "base64 length {} is not a multiple of 4",
            bytes.len()
        )));
    }
    let chunks = bytes.len() / 4;
    let padding = bytes.iter().rev().take_while(|&&byte| byte == b'=').count();
    let decoded_len = (chunks * 3).saturating_sub(padding);
    if decoded_len > limit {
        return Err(malformed("decoded relay body exceeds limit".into()));
    }
    let mut out = Vec::with_capacity(decoded_len);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let data_len = chunk.iter().take_while(|&&b| b != b'=').count();
        if data_len < 2
            || !chunk[data_len..].iter().all(|&b| b == b'=')
            || (data_len < 4 && index + 1 != chunks)
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
    if out.len() > limit {
        return Err(malformed("decoded relay body exceeds limit".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "http://172.233.91.23:50071/";


    #[test]
    fn request_rejects_unsafe_inputs() {
        let long_name = "x".repeat(65);
        let bad = [
            vec![("Bad\nName".to_string(), "v".to_string())],
            vec![("bad name".to_string(), "v".to_string())],
            vec![("Ok".to_string(), "v\r\nInjected: x".to_string())],
            vec![("Ok".to_string(), "v\u{0}".to_string())],
            vec![(long_name, "v".to_string())],
        ];
        for headers in bad {
            assert!(matches!(
                RelayRequest::new(BASE, "t", "https://example.com/", &headers),
                Err(RelayError::InvalidRequest(_))
            ));
        }
        assert!(matches!(
            RelayRequest::new(BASE, "to\nken", "https://example.com/", &[]),
            Err(RelayError::InvalidRequest(_))
        ));
        assert!(matches!(
            RelayRequest::new(BASE, "t", "ftp://example.com/", &[]),
            Err(RelayError::InvalidRequest(_))
        ));
        assert!(matches!(
            RelayRequest::new("file:///etc/passwd", "t", "https://example.com/", &[]),
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
        let parsed = RelayResponse::parse(200, response.as_bytes(), 1024).unwrap();
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
        let parsed = RelayResponse::parse(200, response.as_bytes(), 1024).unwrap();
        assert_eq!(parsed.status, 404);
        assert_eq!(parsed.location.as_deref(), Some("/other"));
        assert_eq!(parsed.body, binary);
    }

    #[test]
    fn response_carries_every_set_cookie_and_tolerates_its_absence() {
        let response = br#"{"status":302,"contentType":"text/html","location":"/login","setCookie":["a=1; Path=/","b=2"],"body":"QQ=="}"#;
        let parsed = RelayResponse::parse(200, response, 1024).unwrap();
        assert_eq!(parsed.status, 302);
        assert_eq!(parsed.location.as_deref(), Some("/login"));
        assert_eq!(
            parsed.set_cookie,
            vec!["a=1; Path=/".to_string(), "b=2".to_string()]
        );

        // フィールド欠落 (古い踏み台) と非文字列要素は空/無視。
        let plain = RelayResponse::parse(200, br#"{"status":200,"body":"QQ=="}"#, 1024).unwrap();
        assert!(plain.set_cookie.is_empty());
        let mixed =
            RelayResponse::parse(200, br#"{"status":200,"setCookie":[1,"x=1"],"body":"QQ=="}"#, 1024)
                .unwrap();
        assert_eq!(mixed.set_cookie, vec!["x=1".to_string()]);
    }

    #[test]
    fn escaped_body_strings_and_nonstring_optional_metadata_keep_protocol_semantics() {
        let response = RelayResponse::parse(
            200,
            br#"{"status":200,"body":"\u0051Q==","contentType":17,"location":{},"via":[],"setCookie":[{},"sid=fresh",1]}"#,
            1,
        ).unwrap();
        assert_eq!(response.body, b"A");
        assert!(response.content_type.is_none());
        assert!(response.location.is_none());
        assert!(response.via.is_none());
        assert_eq!(response.set_cookie, vec!["sid=fresh"]);
        assert!(matches!(
            RelayResponse::parse(403, br#"{"status":{},"error":"forbidden","body":123}"#, 1),
            Err(RelayError::Rejected { http_status: 403, upstream_status: None, .. }),
        ));
    }

    #[test]
    fn response_errors_are_reported() {
        // 踏み台の拒否。
        match RelayResponse::parse(403, br#"{"error":"forbidden"}"#, 1024) {
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
            1024,
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
            RelayResponse::parse(200, b"<html>challenge</html>", 1024),
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
                matches!(RelayResponse::parse(200, bad, 1024), Err(RelayError::Malformed(_))),
                "{bad:?} should be malformed"
            );
        }
    }

    #[test]
    fn base64_round_trip_and_rejects_garbage() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"he"), "aGU=");
        assert_eq!(base64_decode("aGVsbG8=", 1024).unwrap(), b"hello");
        assert_eq!(base64_decode("aGU=", 1024).unwrap(), b"he");
        assert_eq!(base64_decode("", 1024).unwrap(), Vec::<u8>::new());

        for garbage in ["AAA", "A===", "AAAA=AAA", "!!!!", "aGVs bG8=", "aGVsbG8"] {
            assert!(
                matches!(base64_decode(garbage, 1024), Err(RelayError::Malformed(_))),
                "{garbage:?} should be malformed"
            );
        }
        // デコード失敗は parse でも Malformed になる。
        assert!(matches!(
            RelayResponse::parse(200, br#"{"status":200,"body":"!!!"}"#, 1024),
            Err(RelayError::Malformed(_))
        ));
    }

    #[test]
    fn decoded_body_limit_checks_length_and_padding_before_allocation() {
        assert!(RelayResponse::parse(200, br#"{"status":200,"body":"QUJDREVG"}"#, 3).is_err());
        // Three bytes fit the base64 wire length of a two-byte limit, but the
        // decoded length must still fail rather than rounding the cap upward.
        assert!(RelayResponse::parse(200, br#"{"status":200,"body":"QUJD"}"#, 2).is_err());
        assert_eq!(
            RelayResponse::parse(200, br#"{"status":200,"body":"QUI="}"#, 2).unwrap().body,
            b"AB",
        );
        assert!(RelayResponse::parse(200, br#"{"status":200,"body":"QQ=="}"#, 0).is_err());
        assert!(RelayResponse::parse(200, br#"{"status":200,"body":""}"#, 0).unwrap().body.is_empty());
    }

    #[test]
    fn relay_accepts_exactly_sixteen_mib_of_decoded_bytes() {
        let limit = 16 * 1024 * 1024;
        for length in [limit, limit + 1] {
            let encoded = base64_encode(&vec![b'x'; length]);
            let wire = format!(r#"{{"status":200,"setCookie":["sid=fresh"],"body":"{encoded}"}}"#);
            drop(encoded);
            assert!(wire.len() <= response_wire_limit(limit));
            let result = RelayResponse::parse(200, wire.as_bytes(), limit);
            if length == limit {
                let response = result.unwrap();
                assert_eq!(response.body.len(), limit);
                assert!(response.body.iter().all(|&byte| byte == b'x'));
                assert_eq!(response.set_cookie, vec!["sid=fresh"]);
            } else {
                assert!(result.unwrap_err().to_string().contains("exceeds limit"));
            }
        }
    }

    #[test]
    fn response_metadata_is_bounded_even_when_the_body_is_small() {
        let envelope_len = r#"{"status":200,"via":"","body":""}"#.len();
        for length in [MAX_RELAY_METADATA, MAX_RELAY_METADATA + 1] {
            let via = "x".repeat(length - envelope_len);
            let wire = format!(r#"{{"status":200,"via":"{via}","body":""}}"#);
            let result = RelayResponse::parse(200, wire.as_bytes(), 16 * 1024 * 1024);
            if length == MAX_RELAY_METADATA {
                assert!(result.unwrap().body.is_empty());
            } else {
                assert!(result.unwrap_err().to_string().contains("metadata exceeds limit"));
            }
        }
    }
}
