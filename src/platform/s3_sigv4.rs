//! S3 互換ストレージ向けの AWS Signature Version 4 署名。
//!
//! HTTP クライアントから独立した純関数群だけを置く。transport (Workers の
//! `fetch`、native の reqwest 等) は呼び出し側が持ち、ここでは
//! 「どのヘッダを足せばよいか」だけを計算する。
//!
//! 対応する範囲は S3 の通常リクエスト (PUT/GET/HEAD/DELETE/LIST) で、
//! ストリーミングアップロード (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`) は
//! 扱わない。ペイロードのハッシュは呼び出し側が [`payload_sha256`] で
//! 求めて渡す (空ボディは空文字列の SHA-256)。

use chrono::{DateTime, SecondsFormat, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const TERMINATOR: &str = "aws4_request";

/// 署名に使う資格情報。値はログに出さない前提で扱う。
pub struct Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
}

/// 署名対象のリクエスト。`path` と `query` は既に正規化 (percent-encode) 済みの
/// 形で渡す。`headers` は小文字名で、`host` / `x-amz-*` は含めない
/// (この関数が付与する)。
pub struct RequestToSign<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256: &'a str,
    /// `20260926T120000Z` 形式 (UTC)。
    pub amz_date: &'a str,
}

/// リクエストへ追加するヘッダ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    pub authorization: String,
    pub x_amz_date: String,
    pub x_amz_content_sha256: String,
}

/// SigV4 の署名ヘッダを計算する。
pub fn sign(
    request: &RequestToSign<'_>,
    credentials: &Credentials<'_>,
    region: &str,
    service: &str,
) -> SignedHeaders {
    let date = request
        .amz_date
        .get(..8)
        .unwrap_or(request.amz_date)
        .to_string();
    let scope = format!("{date}/{region}/{service}/{TERMINATOR}");

    let mut headers: Vec<(&str, &str)> = vec![
        ("host", request.host),
        ("x-amz-content-sha256", request.payload_sha256),
        ("x-amz-date", request.amz_date),
    ];
    headers.extend(request.headers.iter().map(|(name, value)| (*name, *value)));
    headers.sort_by(|left, right| left.0.cmp(right.0));

    let signed_headers = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical_headers = String::new();
    for (name, value) in &headers {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
    }

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method, request.path, request.query, canonical_headers, signed_headers,
        request.payload_sha256
    );
    let request_digest = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!("{ALGORITHM}\n{}\n{scope}\n{request_digest}", request.amz_date);

    let signing_key = signing_key(credentials.secret_access_key, &date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    SignedHeaders {
        authorization: format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
        x_amz_date: request.amz_date.to_string(),
        x_amz_content_sha256: request.payload_sha256.to_string(),
    }
}

/// ペイロードの SHA-256 (小文字 hex)。空ボディは空文字列のハッシュになる。
pub fn payload_sha256(payload: &[u8]) -> String {
    sha256_hex(payload)
}

/// SigV4 用のタイムスタンプ (`20260926T120000Z`)。
pub fn amz_date(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
        .replace(['-', ':'], "")
}

/// クエリ文字列を canonical form (key/value を encode して sort) にする。
///
/// 値を持たないパラメータは `key=` として扱う。
pub fn canonical_query(params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(key, value)| (uri_encode(key, false), uri_encode(value, false)))
        .collect();
    encoded.sort();
    encoded
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// パスを canonical form (`/` は残し、各セグメントを encode) にする。
pub fn canonical_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let encoded = uri_encode(path, true);
    if encoded.starts_with('/') {
        encoded
    } else {
        format!("/{encoded}")
    }
}

/// RFC 3986 の unreserved (`A-Za-z0-9-._~`) 以外を percent-encode する。
/// `keep_slash` が真なら `/` をそのまま残す (パス用)。
fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && *byte == b'/');
        if unreserved {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, TERMINATOR.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct SigCase {
        name: &'static str,
        method: &'static str,
        host: &'static str,
        path: &'static str,
        query: &'static str,
        headers: &'static [(&'static str, &'static str)],
        body: Option<&'static str>,
        amz_date: &'static str,
        expected_authorization: &'static str,
    }

    /// 期待値は botocore の `S3SigV4Auth` で生成した (独立実装との突き合わせ)。
    const CASES: &[SigCase] = &[
                SigCase {
            name: "get_toc_unsigned",
            method: "GET",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/ncode.syosetu.com/n1234ab/toc.yaml",
            query: "",
            headers: &[],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=bdb7c80e99fb9a0fb12deebc70bd225864ef566efc70cb9e0a5b3fddbf37dd41",
        },
        SigCase {
            name: "put_japanese_unsigned",
            method: "PUT",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/%E3%82%AB%E3%82%AF%E3%83%A8%E3%83%A0/%5B%E4%BD%9C%E8%80%85%5D%20%E3%82%BF%E3%82%A4%E3%83%88%E3%83%AB/%E6%9C%AC%E6%96%87/0001%20%E7%AC%AC%E4%B8%80%E8%A9%B1.yaml",
            query: "",
            headers: &[("content-type", "application/octet-stream")],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=0c2c88408701a9547f443401e9bfe61876e3d1121d7b3b7be2dff62f050ea5d7",
        },
        SigCase {
            name: "list_query_unsigned",
            method: "GET",
            host: "s3.example.com",
            path: "/narou-library/",
            query: "continuation-token=abc%2Fdef%3D&list-type=2&max-keys=100&prefix=narou%2Fdevelop%2Fnovels%2F",
            headers: &[],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=e6194dae683aa20131af9ce96a5196ed0395edadc9d6d1ce0e7c9f4d85cede0b",
        },
        SigCase {
            name: "delete_unsigned",
            method: "DELETE",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/site/title/%E6%8C%BF%E7%B5%B5/foo.jpg",
            query: "",
            headers: &[],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=301bddc50eaa76fb2b051493373bcb727ebe73d580d77e0b64b8c2c9662affc5",
        },
        SigCase {
            name: "head_unsigned",
            method: "HEAD",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/site/title/novel.txt",
            query: "",
            headers: &[],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=246aad5b30e953df413a7f3a6a3c7e5a0fb65b7d15d6e05b74ce5bc76958fc20",
        },
        SigCase {
            name: "get_signed_payload",
            method: "GET",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/site/title/toc.yaml",
            query: "",
            headers: &[],
            body: Some(""),
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=b9d3751a20cd8382f9aa6d213c0950169030fe8fd24f2cbd8c154a22aa634769",
        },
        SigCase {
            name: "put_signed_payload",
            method: "PUT",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/site/title/body.yaml",
            query: "",
            headers: &[("content-type", "application/yaml")],
            body: Some("本文のテスト"),
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=825d85739c20dd3bf1bf7bbf89a4fa1365335398a841d0bbd3e8ff62fd394ca1",
        },
        SigCase {
            name: "put_unsigned_payload",
            method: "PUT",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/site/title/body.yaml",
            query: "",
            headers: &[("content-type", "application/yaml")],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=c14c17042f2df3c0758db0c74fa016474744b7605ce40ce8cf86c8faf7c5d417",
        },
        SigCase {
            name: "get_unsigned_payload",
            method: "GET",
            host: "s3.example.com",
            path: "/narou-library/narou/develop/novels/site/title/insert/%E7%94%BB%E5%83%8F%201.jpg",
            query: "",
            headers: &[],
            body: None,
            amz_date: "20260926T023912Z",
            expected_authorization: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260926/ap-northeast-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=be4e4066f813d23ded2bfa9423c51de00a5fb01820976e4d3e43f85e2f8a1b28",
        },
    ];

    #[test]
    fn signatures_match_the_reference_implementation() {
        let credentials = Credentials {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        };
        for case in CASES {
            let payload = payload_sha256(case.body.unwrap_or("").as_bytes());
            let request = RequestToSign {
                method: case.method,
                host: case.host,
                path: case.path,
                query: case.query,
                headers: case.headers,
                payload_sha256: &payload,
                amz_date: case.amz_date,
            };
            let signed = sign(&request, &credentials, "ap-northeast-1", "s3");
            assert_eq!(
                signed.authorization, case.expected_authorization,
                "case {}",
                case.name
            );
        }
    }

    #[test]
    fn canonical_path_encodes_each_segment() {
        assert_eq!(
            canonical_path("/narou/カクヨム/[作者] タイトル/本文/0001 第一話.yaml"),
            "/narou/%E3%82%AB%E3%82%AF%E3%83%A8%E3%83%A0/%5B%E4%BD%9C%E8%80%85%5D%20%E3%82%BF%E3%82%A4%E3%83%88%E3%83%AB/%E6%9C%AC%E6%96%87/0001%20%E7%AC%AC%E4%B8%80%E8%A9%B1.yaml"
        );
        assert_eq!(canonical_path(""), "/");
        assert_eq!(canonical_path("key"), "/key");
        // `~` は unreserved なので encode しない。
        assert_eq!(canonical_path("/a~b"), "/a~b");
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        let params = vec![
            ("prefix".to_string(), "narou/develop/".to_string()),
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), "100".to_string()),
            ("continuation-token".to_string(), "abc/def=".to_string()),
        ];
        assert_eq!(
            canonical_query(&params),
            "continuation-token=abc%2Fdef%3D&list-type=2&max-keys=100&prefix=narou%2Fdevelop%2F"
        );
        assert_eq!(canonical_query(&[]), "");
        assert_eq!(
            canonical_query(&[("delete".to_string(), String::new())]),
            "delete="
        );
    }

    #[test]
    fn payload_hash_of_empty_body_is_the_known_digest() {
        assert_eq!(
            payload_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn amz_date_is_utc_without_punctuation() {
        let at = Utc.with_ymd_and_hms(2026, 9, 26, 12, 0, 0).unwrap();
        assert_eq!(amz_date(at), "20260926T120000Z");
    }
}
