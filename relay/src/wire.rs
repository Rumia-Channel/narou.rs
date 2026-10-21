//! リレー wire の JSON・base64・ヘッダ・サイズ境界。
//!
//! 旧 Node 版 (`scripts/relay/worker/protocol.mjs` と `server.mjs`) と Worker
//! 側クライアント (`src/platform/relay.rs`) が共有していた契約を移植したもの。
//! 上限値・エラーの解釈を変える場合は Worker 側の受理上限も合わせて確認する
//! こと。

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde_json::{Map, Value, json};

/// 上流レスポンス本文の上限 (base64 デコード後)。
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// 踏み台へのリクエスト JSON (または GET の `headers` クエリ) の上限。
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// HTTP ヘッダ (受信リクエスト・上流応答のどちらも) の上限。
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// 応答 JSON のうち base64 本文を除いた部分の上限。
///
/// 上流ヘッダは JSON エスケープで最大 6 倍に膨らむため、ヘッダ上限の 6 倍に
/// 封筒のぶんを足した予算を Worker 側と共有する。
pub const MAX_METADATA_BYTES: usize = 6 * MAX_HEAD_BYTES + 64 * 1024;
/// curl / wget の子プロセスから受け取る出力 (`-i` / `--save-headers` の
/// ヘッダ + 本文) の上限。
pub const MAX_TRANSPORT_OUTPUT: usize = MAX_HEAD_BYTES + MAX_BODY_BYTES;

/// Worker 側 `platform::relay::response_wire_limit` と同じ、base64 本文を含む
/// 応答全体の上限。
pub fn response_wire_limit(decoded_body_limit: usize) -> usize {
    decoded_body_limit
        .div_ceil(3)
        .saturating_mul(4)
        .saturating_add(MAX_METADATA_BYTES)
}

/// wire 境界の検証エラー。HTTP ステータスへの対応は [`crate::server`] が決める。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    /// base64 として不正、または JSON オブジェクトでない。
    #[error("headers must be base64 JSON")]
    HeadersNotBase64,
    /// 上限を超えている。
    #[error("request too large")]
    RequestTooLarge,
    /// JSON オブジェクトではない。
    #[error("expected JSON object")]
    ExpectedJsonObject,
    /// リクエスト全体として不正。
    #[error("invalid request")]
    InvalidRequest,
    /// 転送ヘッダの形・値が不正。
    #[error("invalid request headers")]
    InvalidHeaders,
    /// `url` フィールドが文字列でない・欠落。
    #[error("invalid target URL")]
    InvalidTargetUrl,
    /// 応答メタデータが上限を超えている。
    #[error("response too large")]
    ResponseTooLarge,
}

/// POST 本文から取り出した未検証のペイロード。
#[derive(Debug, Clone)]
pub struct Payload {
    /// `url` フィールド。文字列以外・欠落もそのまま保持し、解決時に拒否する。
    pub url: Option<Value>,
    /// `headers` フィールド。オブジェクト以外は解決時に拒否する。
    pub headers: Option<Value>,
}

/// POST の JSON 本文をパースする。オブジェクト以外は拒否する。
pub fn parse_post_payload(bytes: &[u8]) -> Result<Payload, WireError> {
    let text = std::str::from_utf8(bytes).map_err(|_| WireError::InvalidRequest)?;
    let value: Value = serde_json::from_str(text).map_err(|_| WireError::ExpectedJsonObject)?;
    let object = value.as_object().ok_or(WireError::ExpectedJsonObject)?;
    Ok(Payload {
        url: object.get("url").cloned(),
        headers: object.get("headers").cloned(),
    })
}

/// `url` フィールドを文字列として取り出す。欠落・非文字列は拒否する。
pub fn payload_url(payload: &Payload) -> Result<&str, WireError> {
    match payload.url.as_ref() {
        Some(Value::String(url)) => Ok(url),
        _ => Err(WireError::InvalidTargetUrl),
    }
}

/// GET の `headers` クエリ (base64(UTF-8 JSON オブジェクト)) をデコードする。
///
/// base64 の検証は Node 版と同じく「標準アルファベット + 末尾パディングのみ +
/// 再エンコード一致」で行う (非正規のパディングや余剰ビットを拒否する)。
pub fn decode_header_json(raw: &str) -> Result<Value, WireError> {
    // Node: raw.length > 4 * ceil(MAX_REQUEST_BYTES / 3) を先に弾く。
    if raw.len() > 4 * MAX_REQUEST_BYTES.div_ceil(3) {
        return Err(WireError::RequestTooLarge);
    }
    let decoded = strict_base64(raw)?;
    if decoded.len() > MAX_REQUEST_BYTES {
        return Err(WireError::RequestTooLarge);
    }
    let text = std::str::from_utf8(&decoded).map_err(|_| WireError::ExpectedJsonObject)?;
    let value: Value = serde_json::from_str(text).map_err(|_| WireError::ExpectedJsonObject)?;
    if !value.is_object() {
        return Err(WireError::ExpectedJsonObject);
    }
    Ok(value)
}

/// 標準 base64 (パディング任意) を厳密にデコードする。
fn strict_base64(raw: &str) -> Result<Vec<u8>, WireError> {
    let bytes = raw.as_bytes();
    let data_len = bytes
        .iter()
        .position(|&byte| byte == b'=')
        .unwrap_or(bytes.len());
    if bytes.len() - data_len > 2 || bytes[data_len..].iter().any(|&byte| byte != b'=') {
        return Err(WireError::HeadersNotBase64);
    }
    let data = &raw[..data_len];
    if !data
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
        return Err(WireError::HeadersNotBase64);
    }
    if data.len() % 4 == 1 {
        return Err(WireError::HeadersNotBase64);
    }
    let decoded = STANDARD_NO_PAD
        .decode(data)
        .map_err(|_| WireError::HeadersNotBase64)?;
    if STANDARD_NO_PAD.encode(&decoded) != data {
        return Err(WireError::HeadersNotBase64);
    }
    Ok(decoded)
}

/// RFC 7230 の token を満たすヘッダ名か。
pub fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
        })
}

/// HTTP ヘッダ値として安全か。
///
/// Node 版と同じく、値は UTF-8 バイト列をそのまま HTTP の値として送る
/// (curl / wget は与えられたバイト列を送る)。制御文字と DEL を拒否する。
pub fn is_valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte) || byte >= 0x80)
}

/// `headers` フィールドを順序つきの `(名前, 値)` に開く。
///
/// JSON オブジェクトのキー順は `serde_json` の既定 (BTreeMap) で決定的になり、
/// Worker 側クライアントが作る JSON (キー昇順) と一致する。同名ヘッダは配列で
/// 受けた順に展開する。名前の大文字小文字はそのまま残す (HTTP/1.1 の wget /
/// curl は与えられた綴りを送るため)。
pub fn header_pairs(raw: Option<&Value>) -> Result<Vec<(String, String)>, WireError> {
    let Some(value) = raw else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let object = value.as_object().ok_or(WireError::InvalidHeaders)?;
    let mut pairs = Vec::new();
    for (name, value) in object {
        if !is_valid_header_name(name) {
            return Err(WireError::InvalidHeaders);
        }
        match value {
            Value::String(text) => {
                if !is_valid_header_value(text) {
                    return Err(WireError::InvalidHeaders);
                }
                pairs.push((name.clone(), text.clone()));
            }
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::String(text) if is_valid_header_value(text) => {
                            pairs.push((name.clone(), text.clone()));
                        }
                        _ => return Err(WireError::InvalidHeaders),
                    }
                }
            }
            _ => return Err(WireError::InvalidHeaders),
        }
    }
    Ok(pairs)
}

/// 上流レスポンスのうちリレーが運ぶメタデータ。
#[derive(Debug, Clone)]
pub struct ResponseMetadata {
    /// 上流の HTTP ステータス (踏み台自身の応答ではない)。
    pub status: u16,
    /// `Content-Type`。空文字もありうる。
    pub content_type: String,
    /// `Location` (3xx を辿らずに返す)。
    pub location: Option<String>,
    /// 上流が返した `Set-Cookie` (受信順、全件)。
    pub set_cookie: Vec<String>,
    /// 取得に使ったクライアント (`curl` / `wget`)。
    pub via: &'static str,
}

impl ResponseMetadata {
    /// 本文を空にした応答 JSON (Worker 側のメタデータ上限計算と同じ形)。
    pub fn envelope(&self) -> Value {
        let mut map = Map::new();
        map.insert("status".to_string(), json!(self.status));
        map.insert("contentType".to_string(), json!(self.content_type));
        if let Some(location) = &self.location {
            map.insert("location".to_string(), json!(location));
        }
        map.insert("setCookie".to_string(), json!(self.set_cookie));
        map.insert("via".to_string(), json!(self.via));
        map.insert("body".to_string(), json!(""));
        Value::Object(map)
    }
}

/// base64 本文を除いた応答 JSON の長さ (UTF-8 バイト数)。
pub fn metadata_wire_len(metadata: &ResponseMetadata) -> usize {
    serde_json::to_vec(&metadata.envelope()).map_or(usize::MAX, |bytes| bytes.len())
}

/// 最終応答 JSON (base64 本文つき) を組み立てる。
///
/// メタデータが上限を超える場合と、応答全体が Worker 側の受理上限を超える
/// 場合は [`WireError::ResponseTooLarge`]。
pub fn encode_response(metadata: &ResponseMetadata, body: &[u8]) -> Result<Vec<u8>, WireError> {
    let metadata_len = metadata_wire_len(metadata);
    if metadata_len > MAX_METADATA_BYTES {
        return Err(WireError::ResponseTooLarge);
    }
    let encoded = STANDARD.encode(body);
    let mut value = metadata.envelope();
    if let Some(object) = value.as_object_mut() {
        object.insert("body".to_string(), Value::String(encoded));
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| WireError::ResponseTooLarge)?;
    if bytes.len() > response_wire_limit(MAX_BODY_BYTES) {
        return Err(WireError::ResponseTooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(input: &[u8]) -> String {
        STANDARD.encode(input)
    }

    fn object(value: Value) -> Value {
        assert!(value.is_object());
        value
    }

    #[test]
    fn post_payload_requires_a_json_object() {
        assert!(parse_post_payload(b"null").is_err());
        assert!(parse_post_payload(b"[]").is_err());
        assert!(parse_post_payload(b"\"text\"").is_err());
        assert!(parse_post_payload(b"42").is_err());
        assert!(parse_post_payload(b"{").is_err());
        assert!(parse_post_payload(&[0xc3, 0x28]).is_err());
        assert!(parse_post_payload(b"").is_err());
        let payload = parse_post_payload(br#"{"url":"https://example.com/","headers":{"a":["1","2"]}}"#)
            .expect("object payload");
        assert_eq!(payload_url(&payload).unwrap(), "https://example.com/");
        assert!(payload.headers.is_some());
    }

    #[test]
    fn payload_url_must_be_a_string() {
        for raw in [
            &b"{}"[..],
            br#"{"url":42}"#,
            br#"{"url":null}"#,
            br#"{"url":[]}"#,
        ] {
            assert_eq!(
                payload_url(&parse_post_payload(raw).unwrap()).unwrap_err(),
                WireError::InvalidTargetUrl
            );
        }
    }

    #[test]
    fn get_headers_decode_is_strict() {
        let raw = encode(r#"{"x-label":"café"}"#.as_bytes());
        let value = object(decode_header_json(&raw).unwrap());
        assert_eq!(value["x-label"], "café");
        // パディング無しでも正規形なら受け付ける。
        assert!(decode_header_json(encode(b"{}").trim_end_matches('=')).is_ok());

        for garbage in ["", "%", "a", "A===", "AAAA=AAA", "!!!!", "aGVs bG8=", "aGVsbG8", "AB=="] {
            assert!(
                decode_header_json(garbage).is_err(),
                "{garbage:?} should be rejected"
            );
        }
        for value in ["null", "[]", "\"text\"", "42"] {
            assert!(decode_header_json(&encode(value.as_bytes())).is_err());
        }
        let invalid_utf8 = encode(&[0xc3, 0x28]);
        assert!(decode_header_json(&invalid_utf8).is_err());

        let too_long = "A".repeat(4 * MAX_REQUEST_BYTES.div_ceil(3) + 1);
        assert_eq!(
            decode_header_json(&too_long).unwrap_err(),
            WireError::RequestTooLarge
        );
        // 64 KiB を超える JSON も拒否する。
        let big = encode(format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_REQUEST_BYTES)).as_bytes());
        assert_eq!(
            decode_header_json(&big).unwrap_err(),
            WireError::RequestTooLarge
        );
    }

    #[test]
    fn header_pairs_validate_shape_and_preserve_order_and_case() {
        let value = object(json!({
            "x-first": ["日本語", "café"],
            "x-second": "last",
        }));
        assert_eq!(
            header_pairs(Some(&value)).unwrap(),
            vec![
                ("x-first".to_string(), "日本語".to_string()),
                ("x-first".to_string(), "café".to_string()),
                ("x-second".to_string(), "last".to_string()),
            ]
        );
        assert!(header_pairs(None).unwrap().is_empty());
        assert!(header_pairs(Some(&Value::Null)).unwrap().is_empty());
        for bad in [
            json!([]),
            json!("text"),
            json!(42),
            json!({"x": 1}),
            json!({"x": [null]}),
            json!({"x": ["ok", 2]}),
            json!({"bad name": "x"}),
            json!({"bad\nname": "x"}),
            json!({"x": "a\r\nInjected: y"}),
            json!({"x": "a\u{0}b"}),
        ] {
            assert_eq!(
                header_pairs(Some(&bad)).unwrap_err(),
                WireError::InvalidHeaders,
                "{bad}"
            );
        }
    }

    #[test]
    fn header_value_rules_match_http_header_values() {
        assert!(is_valid_header_value("plain"));
        assert!(is_valid_header_value("café"));
        assert!(is_valid_header_value("\tvalue"));
        assert!(!is_valid_header_value("bad\r\nvalue"));
        assert!(!is_valid_header_value("bad\u{0}value"));
        assert!(!is_valid_header_value("bad\u{7f}value"));
    }

    #[test]
    fn response_encoding_round_trips_binary_and_limit_checks() {
        let metadata = ResponseMetadata {
            status: 200,
            content_type: "text/html; charset=UTF-8".to_string(),
            location: None,
            set_cookie: vec!["a=1; Path=/".to_string(), "b=2".to_string()],
            via: "curl",
        };
        let body: Vec<u8> = (0u8..=255).collect();
        let wire = encode_response(&metadata, &body).unwrap();
        let value: Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(value["status"], 200);
        assert_eq!(value["contentType"], "text/html; charset=UTF-8");
        assert!(value.get("location").is_none());
        assert_eq!(value["setCookie"], json!(["a=1; Path=/", "b=2"]));
        assert_eq!(value["via"], "curl");
        assert_eq!(
            STANDARD.decode(value["body"].as_str().unwrap()).unwrap(),
            body
        );
        assert_eq!(
            metadata_wire_len(&metadata),
            serde_json::to_vec(&metadata.envelope()).unwrap().len()
        );
        assert!(metadata_wire_len(&metadata) < MAX_METADATA_BYTES);
    }

    #[test]
    fn metadata_allowance_includes_escaping_utf8_and_the_empty_body_envelope() {
        let base = ResponseMetadata {
            status: 200,
            content_type: String::new(),
            location: None,
            set_cookie: Vec::new(),
            via: "curl",
        };
        let base_len = metadata_wire_len(&base);
        let location_envelope = metadata_wire_len(&ResponseMetadata {
            location: Some(String::new()),
            ..base.clone()
        });
        let padding_budget = MAX_METADATA_BYTES - location_envelope;
        for (character, cost) in [
            ("x", 1usize),
            ("é", 2usize),
            ("\u{1}", 6usize),
        ] {
            let padding = character.repeat(padding_budget / cost)
                + &"x".repeat(padding_budget % cost);
            let metadata = ResponseMetadata {
                location: Some(padding.clone()),
                ..base.clone()
            };
            assert_eq!(metadata_wire_len(&metadata), MAX_METADATA_BYTES);
            assert!(encode_response(&metadata, b"").is_ok());
            let too_large = ResponseMetadata {
                location: Some(format!("{padding}x")),
                ..base.clone()
            };
            assert!(metadata_wire_len(&too_large) > MAX_METADATA_BYTES);
            assert_eq!(
                encode_response(&too_large, b"").unwrap_err(),
                WireError::ResponseTooLarge
            );
        }
        assert!(base_len < MAX_METADATA_BYTES);
    }

    #[test]
    fn exactly_sixteen_mib_of_decoded_bytes_is_accepted_on_the_wire() {
        let limit = MAX_BODY_BYTES;
        let metadata = ResponseMetadata {
            status: 200,
            content_type: String::new(),
            location: None,
            set_cookie: vec!["sid=fresh".to_string()],
            via: "curl",
        };
        let body = vec![b'x'; limit];
        let wire = encode_response(&metadata, &body).unwrap();
        assert!(wire.len() <= response_wire_limit(limit));
        let value: Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(STANDARD.decode(value["body"].as_str().unwrap()).unwrap(), body);
        assert_eq!(value["setCookie"], json!(["sid=fresh"]));
    }

    #[test]
    fn oversized_metadata_is_rejected_even_with_a_small_body() {
        let metadata = ResponseMetadata {
            status: 200,
            content_type: String::new(),
            location: Some("x".repeat(MAX_METADATA_BYTES)),
            set_cookie: Vec::new(),
            via: "wget",
        };
        assert!(metadata_wire_len(&metadata) > MAX_METADATA_BYTES);
        assert_eq!(
            encode_response(&metadata, b"").unwrap_err(),
            WireError::ResponseTooLarge
        );
    }
}
