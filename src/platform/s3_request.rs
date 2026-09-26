//! S3 リクエストの組み立てと `ListObjectsV2` 応答の解釈。
//!
//! 署名 ([`super::s3_sigv4`]) と同じく transport を持たない純関数群で、
//! URL の組み立て・キーの写像・XML の読み取りだけを行う。実際の HTTP は
//! 各プラットフォームのアダプタが担う。
//!
//! URL は path-style (`{endpoint}/{bucket}/{key}`) を使う。S3 互換の
//! 実装 (Wasabi / MinIO / R2 など) は path-style を受け付けるため、
//! バケット名の DNS 互換性に依存しない。

use chrono::{DateTime, Utc};

use crate::error::{NarouError, Result};

use super::object_store::ObjectKey;
use super::s3_sigv4::{canonical_path, canonical_query};

/// S3 の接続先とキー名前空間。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Location {
    endpoint: String,
    bucket: String,
    prefix: String,
}

impl S3Location {
    /// `endpoint` は `https://host[:port]` (末尾スラッシュ無し)、`prefix` は
    /// 空か `narou/develop` のようなスラッシュ区切り。
    pub fn new(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Result<Self> {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(NarouError::Platform(format!(
                "S3 endpoint must be an http(s) URL: {endpoint:?}"
            )));
        }
        let bucket = bucket.into();
        if bucket.is_empty() || bucket.contains('/') {
            return Err(NarouError::Platform(format!(
                "invalid S3 bucket name: {bucket:?}"
            )));
        }
        let prefix = prefix.into().trim_matches('/').to_string();
        Ok(Self {
            endpoint,
            bucket,
            prefix,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// `Host` ヘッダに載せる `host[:port]`。
    pub fn host(&self) -> Result<String> {
        let rest = self
            .endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .ok_or_else(|| NarouError::Platform("S3 endpoint has no scheme".to_string()))?;
        let host = rest.split('/').next().unwrap_or_default();
        if host.is_empty() {
            return Err(NarouError::Platform(format!(
                "S3 endpoint has no host: {:?}",
                self.endpoint
            )));
        }
        Ok(host.to_string())
    }

    /// 論理キーを S3 のオブジェクトキーへ写像する (prefix を前置)。
    pub fn storage_key(&self, key: &ObjectKey) -> String {
        if self.prefix.is_empty() {
            key.as_ref().to_string()
        } else {
            format!("{}/{}", self.prefix, key.as_ref())
        }
    }

    /// オブジェクトの URL。
    pub fn object_url(&self, key: &ObjectKey) -> String {
        format!("{}{}", self.endpoint, self.object_path(key))
    }

    /// 署名対象にもなるオブジェクトのパス (`/{bucket}/{encoded key}`)。
    pub fn object_path(&self, key: &ObjectKey) -> String {
        format!(
            "/{}/{}",
            self.bucket,
            canonical_path(&self.storage_key(key)).trim_start_matches('/')
        )
    }

    /// バケット直下 (一覧・バケット操作) のパス。
    pub fn bucket_path(&self) -> String {
        format!("/{}", self.bucket)
    }

    /// 一覧 (ListObjectsV2) の URL と、署名に使う canonical query。
    pub fn list_url(
        &self,
        prefix: &str,
        limit: usize,
        continuation_token: Option<&str>,
    ) -> (String, String) {
        let storage_prefix = if self.prefix.is_empty() {
            prefix.to_string()
        } else if prefix.is_empty() {
            format!("{}/", self.prefix)
        } else {
            format!("{}/{}", self.prefix, prefix.trim_start_matches('/'))
        };
        let mut params = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), limit.to_string()),
            ("prefix".to_string(), storage_prefix),
        ];
        if let Some(token) = continuation_token {
            params.push(("continuation-token".to_string(), token.to_string()));
        }
        let query = canonical_query(&params);
        (
            format!("{}{}?{}", self.endpoint, self.bucket_path(), query),
            query,
        )
    }

    /// サーバサイドコピー (`x-amz-copy-source`) に渡す値。
    pub fn copy_source(&self, key: &ObjectKey) -> String {
        format!(
            "/{}/{}",
            self.bucket,
            canonical_path(&self.storage_key(key)).trim_start_matches('/')
        )
    }
}

/// `ListObjectsV2` が返す 1 オブジェクト。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ObjectSummary {
    /// S3 のキー (prefix を含む生の値)。
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
}

/// `ListObjectsV2` の 1 ページ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ListPage {
    pub objects: Vec<S3ObjectSummary>,
    pub next_token: Option<String>,
}

/// `ListObjectsV2` の XML を読む。
///
/// 必要な要素だけを拾う小さな走査で、外部の XML クレートを持ち込まない。
/// キーは自分たちの論理キーなので、要素の入れ子より実体参照の復元を重視する。
pub fn parse_list_objects_v2(xml: &str) -> Result<S3ListPage> {
    let mut objects = Vec::new();
    let mut next_token = None;
    let mut cursor = 0usize;

    while let Some(start) = find_tag(&xml[cursor..], "Contents") {
        let content_start = cursor + start;
        let Some(end) = find_close(&xml[content_start..], "Contents") else {
            return Err(NarouError::Platform(
                "ListObjectsV2 response has an unterminated <Contents>".to_string(),
            ));
        };
        let body = &xml[content_start..content_start + end];
        let key = element_text(body, "Key").ok_or_else(|| {
            NarouError::Platform("ListObjectsV2 <Contents> has no <Key>".to_string())
        })?;
        let size = element_text(body, "Size")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let etag = element_text(body, "ETag").map(|value| value.trim_matches('"').to_string());
        let last_modified = element_text(body, "LastModified")
            .and_then(|value| DateTime::parse_from_rfc3339(value.trim()).ok())
            .map(|value| value.with_timezone(&Utc));
        objects.push(S3ObjectSummary {
            key,
            size,
            etag,
            last_modified,
        });
        cursor = content_start + end + "</Contents>".len();
    }

    if let Some(value) = element_text(xml, "NextContinuationToken") {
        if !value.is_empty() {
            next_token = Some(value);
        }
    }
    if element_text(xml, "IsTruncated").is_some_and(|value| value.trim() == "true") && next_token.is_none() {
        return Err(NarouError::Platform(
            "ListObjectsV2 response is truncated without a continuation token".to_string(),
        ));
    }

    Ok(S3ListPage {
        objects,
        next_token,
    })
}

/// `<Contents>` 開始タグの位置を返す (名前空間接頭辞つきも許容)。
fn find_tag(haystack: &str, tag: &str) -> Option<usize> {
    let mut search_from = 0usize;
    while let Some(offset) = haystack[search_from..].find('<') {
        let start = search_from + offset;
        let rest = &haystack[start + 1..];
        let name_end = rest
            .find(|ch: char| ch == '>' || ch.is_whitespace() || ch == '/')
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tag {
            return Some(start);
        }
        search_from = start + 1;
    }
    None
}

/// `<tag>` の開始位置から `</tag>` の直前までの長さを返す (入れ子は 1 段だけ数える)。
fn find_close(haystack: &str, tag: &str) -> Option<usize> {
    let open_end = haystack.find('>')?;
    let mut depth = 1usize;
    let mut cursor = open_end + 1;
    while depth > 0 {
        let rest = &haystack[cursor..];
        let next_open = find_tag(rest, tag);
        let close_offset = rest
            .find(&format!("</{tag}>"))
            .or_else(|| find_prefixed_close(rest, tag));
        match (next_open, close_offset) {
            (Some(open), Some(close)) if open < close => {
                depth += 1;
                cursor += open + 1;
            }
            (_, Some(close)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor + close);
                }
                cursor += close + format!("</{tag}>").len();
            }
            // 閉じタグが無い (壊れた XML) か、ネストの対応が取れない。
            _ => return None,
        }
    }
    None
}

fn find_prefixed_close(haystack: &str, tag: &str) -> Option<usize> {
    let needle = format!(":{tag}>");
    let mut search_from = 0usize;
    while let Some(offset) = haystack[search_from..].find("</") {
        let start = search_from + offset;
        let rest = &haystack[start + 2..];
        if rest.starts_with(&needle) {
            return Some(start);
        }
        search_from = start + 2;
    }
    None
}

/// `<tag>...</tag>` の中身を実体参照込みで返す。
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let start = find_tag(xml, tag)?;
    let after_open = xml[start..].find('>')? + start + 1;
    let rest = &xml[after_open..];
    let close = rest
        .find(&format!("</{tag}>"))
        .or_else(|| find_prefixed_close(rest, tag))?;
    Some(unescape_xml(&rest[..close]))
}

fn unescape_xml(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let entity_end = tail.find(';').map(|end| end + 1);
        match entity_end {
            Some(end) => {
                let entity = &tail[..end];
                let replacement = match entity {
                    "&amp;" => Some('&'),
                    "&lt;" => Some('<'),
                    "&gt;" => Some('>'),
                    "&quot;" => Some('"'),
                    "&apos;" => Some('\''),
                    _ => None,
                };
                match replacement {
                    Some(ch) => out.push(ch),
                    None => out.push_str(entity),
                }
                rest = &tail[end..];
            }
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `Content-Range` ヘッダから全体サイズを取り出す (`bytes 0-0/12345`)。
///
/// S3 互換ストレージでは `HEAD` がエッジで 403 になることがあるため、
/// `GET` + `Range: bytes=0-0` の応答からメタデータを読む経路で使う。
pub fn parse_content_range_size(value: &str) -> Option<u64> {
    let (_, total) = value.trim().rsplit_once('/')?;
    total.trim().parse::<u64>().ok()
}

/// `Content-Length` からサイズを取り出す (Range を無視するサーバー向け)。
pub fn parse_content_length(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

/// Range 応答 (`206`) か通常応答 (`200`) かに関わらず全体サイズを求める。
pub fn object_size(status: u16, content_range: Option<&str>, content_length: Option<&str>) -> Option<u64> {
    match status {
        206 => content_range.and_then(parse_content_range_size),
        _ => content_length
            .and_then(parse_content_length)
            .or_else(|| content_range.and_then(parse_content_range_size)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location() -> S3Location {
        S3Location::new(
            "https://s3.example.com/",
            "narou-library",
            "narou/develop/",
        )
        .unwrap()
    }

    fn key(value: &str) -> ObjectKey {
        ObjectKey::try_new(value).unwrap()
    }

    #[test]
    fn object_url_is_path_style_and_percent_encoded() {
        let location = location();
        assert_eq!(location.host().unwrap(), "s3.example.com");
        assert_eq!(
            location.object_url(&key("novels/カクヨム/[作者] タイトル/toc.yaml")),
            "https://s3.example.com/narou-library/narou/develop/novels/%E3%82%AB%E3%82%AF%E3%83%A8%E3%83%A0/%5B%E4%BD%9C%E8%80%85%5D%20%E3%82%BF%E3%82%A4%E3%83%88%E3%83%AB/toc.yaml"
        );
        assert_eq!(
            location.copy_source(&key("novels/site/title/toc.yaml")),
            "/narou-library/narou/develop/novels/site/title/toc.yaml"
        );
    }

    #[test]
    fn prefix_is_optional_and_normalized() {
        let bare = S3Location::new("http://127.0.0.1:9000", "narou-local", "").unwrap();
        assert_eq!(bare.storage_key(&key("novels/a/toc.yaml")), "novels/a/toc.yaml");
        assert_eq!(bare.host().unwrap(), "127.0.0.1:9000");
        assert_eq!(
            bare.object_url(&key("novels/a/toc.yaml")),
            "http://127.0.0.1:9000/narou-local/novels/a/toc.yaml"
        );
    }

    #[test]
    fn invalid_location_is_rejected() {
        assert!(S3Location::new("ftp://example.com", "b", "").is_err());
        assert!(S3Location::new("https://example.com", "", "").is_err());
        assert!(S3Location::new("https://example.com", "a/b", "").is_err());
    }

    #[test]
    fn list_url_carries_the_canonical_query() {
        let location = location();
        let (url, query) = location.list_url("novels/", 100, Some("abc/def="));
        assert_eq!(
            query,
            "continuation-token=abc%2Fdef%3D&list-type=2&max-keys=100&prefix=narou%2Fdevelop%2Fnovels%2F"
        );
        assert_eq!(
            url,
            "https://s3.example.com/narou-library?continuation-token=abc%2Fdef%3D&list-type=2&max-keys=100&prefix=narou%2Fdevelop%2Fnovels%2F"
        );

        let (_, query) = location.list_url("", 10, None);
        assert_eq!(query, "list-type=2&max-keys=10&prefix=narou%2Fdevelop%2F");
    }

    #[test]
    fn list_response_is_parsed_with_entities_and_paging() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>narou-library</Name>
  <Prefix>narou/develop/novels/</Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>100</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
  <Contents>
    <Key>narou/develop/novels/site/A &amp; B/toc.yaml</Key>
    <LastModified>2026-09-26T02:39:12.000Z</LastModified>
    <ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag>
    <Size>1024</Size>
  </Contents>
  <Contents>
    <Key>narou/develop/novels/site/タイトル/本文/0001 第一話.yaml</Key>
    <LastModified>2026-09-26T02:40:00.000Z</LastModified>
    <Size>2048</Size>
  </Contents>
</ListBucketResult>"#;

        let page = parse_list_objects_v2(xml).unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "narou/develop/novels/site/A & B/toc.yaml");
        assert_eq!(page.objects[0].size, 1024);
        assert_eq!(
            page.objects[0].etag.as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(
            page.objects[0].last_modified.unwrap().to_rfc3339(),
            "2026-09-26T02:39:12+00:00"
        );
        assert_eq!(
            page.objects[1].key,
            "narou/develop/novels/site/タイトル/本文/0001 第一話.yaml"
        );
        assert_eq!(page.objects[1].size, 2048);
        assert_eq!(
            page.next_token.as_deref(),
            Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
        );
    }

    #[test]
    fn empty_listing_has_no_objects_or_token() {
        let xml = r#"<ListBucketResult><Name>b</Name><KeyCount>0</KeyCount><MaxKeys>100</MaxKeys><IsTruncated>false</IsTruncated></ListBucketResult>"#;
        let page = parse_list_objects_v2(xml).unwrap();
        assert!(page.objects.is_empty());
        assert_eq!(page.next_token, None);
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn reads_the_total_size_from_a_range_response() {
        assert_eq!(parse_content_range_size("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_size("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_size("garbage"), None);
        assert_eq!(object_size(206, Some("bytes 0-0/555"), Some("1")), Some(555));
        // Range を無視して 200 を返すサーバーは Content-Length を使う。
        assert_eq!(object_size(200, None, Some("777")), Some(777));
    }
}
