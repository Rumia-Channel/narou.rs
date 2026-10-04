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
    /// SQLite+S3 モードで、挿絵だけを `illustrations/<base64url(sha256)>.<ext>`
    /// のグローバルプールへ寄せる変換を有効にする。
    dedup: bool,
}

impl S3Location {
    /// `endpoint` は `https://host[:port][/base]` (末尾スラッシュ無し)、`prefix` は
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
        if endpoint.contains('?') || endpoint.contains('#') {
            return Err(NarouError::Platform(format!(
                "S3 endpoint must not contain a query or fragment: {endpoint:?}"
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
            dedup: false,
        })
    }

    /// SQLite+S3 モードで挿絵をコンテンツハッシュ名のグローバルプールへ
    /// 寄せる変換を有効にする。
    pub fn with_illustration_dedup(mut self) -> Self {
        self.dedup = true;
        self
    }

    /// `illustrations/` プールに入る `挿絵/<sha256-hex>.<ext>` か。
    /// hex 名でない挿絵は小説ごとの配置を保つ (衝突を避ける)。
    fn illustration_pool_name(filename: &str) -> Option<String> {
        let (stem, ext) = filename.rsplit_once('.')?;
        if stem.len() != 64 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(format!("{}.{}", sha256_hex_to_base64url(stem), ext))
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

    /// endpoint URL のパス成分 (`https://host/base` → `/base`)。無ければ空。
    /// リバースプロキシ型の S3 互換ではリクエストパスの先頭に付くため、
    /// presign の署名対象パスにも含める必要がある。
    pub fn endpoint_path(&self) -> &str {
        let Some((_, rest)) = self.endpoint.split_once("://") else {
            return "";
        };
        match rest.find('/') {
            Some(index) => &rest[index..],
            None => "",
        }
    }

    /// 論理キーを S3 のオブジェクトキーへ写像する (prefix を前置)。
    ///
    /// dedup 有効時は `…/挿絵/<sha256-hex>.<ext>` を
    /// `illustrations/<base64url(sha256)>.<ext>` へ寄せ、作品を跨いで
    /// 同一内容を 1 オブジェクトにする。hex 名でない挿絵は小説ごとの
    /// 配置のまま残す (移行前・非ハッシュ名の名残)。
    pub fn storage_key(&self, key: &ObjectKey) -> String {
        self.storage_key_dedup(key, self.dedup)
    }

    /// dedup 変換を無視した、従来どおりの配置 (`挿絵/`) のキー。
    /// 移行前に置かれた物理オブジェクトを指す。
    pub fn legacy_storage_key(&self, key: &ObjectKey) -> String {
        self.storage_key_dedup(key, false)
    }

    fn storage_key_dedup(&self, key: &ObjectKey, dedup: bool) -> String {
        let key = key.as_ref();
        let mapped = if dedup {
            match Self::illustration_pool_key(key) {
                Some(pool) => pool,
                None => key.to_string(),
            }
        } else {
            key.to_string()
        };
        if self.prefix.is_empty() {
            mapped
        } else {
            format!("{}/{}", self.prefix, mapped)
        }
    }

    /// `novels/…/挿絵/<file>` を `illustrations/<file>` へ寄せる。
    /// hex 名だけがプールへ出る。
    fn illustration_pool_key(key: &str) -> Option<String> {
        let (_, file) = key.rsplit_once("挿絵/")?;
        Self::illustration_pool_name(file).map(|name| format!("illustrations/{name}"))
    }

    /// dedup でこの論理キーがプール (`illustrations/`) へ寄るか。
    pub fn has_pool_alternate(&self, key: &ObjectKey) -> bool {
        self.dedup && Self::illustration_pool_key(key.as_ref()).is_some()
    }

    /// オブジェクトの URL (dedup 変換込みの正位置)。
    pub fn object_url(&self, key: &ObjectKey) -> String {
        format!("{}{}", self.endpoint, self.object_path(key))
    }

    /// 署名対象にもなるオブジェクトのパス (`/{bucket}/{encoded key}`)。
    pub fn object_path(&self, key: &ObjectKey) -> String {
        self.object_path_of(&self.storage_key(key))
    }

    /// 移行前の `挿絵/` 配置を指す URL/パス。dedup の read fallback と
    /// 移行処理で使う。
    pub fn legacy_object_url(&self, key: &ObjectKey) -> String {
        format!("{}{}", self.endpoint, self.legacy_object_path(key))
    }

    pub fn legacy_object_path(&self, key: &ObjectKey) -> String {
        self.object_path_of(&self.legacy_storage_key(key))
    }

    /// 任意の物理オブジェクトキー (prefix 込みの生キー) のパス。
    /// 移行が `illustrations/` や `挿絵/` の生キーを直接触るために使う。
    pub fn raw_object_path(&self, storage_key: &str) -> String {
        self.object_path_of(storage_key)
    }

    pub fn raw_object_url(&self, storage_key: &str) -> String {
        format!("{}{}", self.endpoint, self.raw_object_path(storage_key))
    }

    fn object_path_of(&self, storage_key: &str) -> String {
        format!(
            "/{}/{}",
            self.bucket,
            canonical_path(storage_key).trim_start_matches('/')
        )
    }

    /// バケット直下 (一覧・バケット操作) のパス。
    pub fn bucket_path(&self) -> String {
        format!("/{}", self.bucket)
    }

    /// 一覧 (ListObjectsV2) の URL。クエリは署名対象と一致する canonical
    /// form で組み立てる (呼び出し側はこの URL をそのまま送ればよい)。
    pub fn list_url(
        &self,
        prefix: &str,
        limit: usize,
        continuation_token: Option<&str>,
    ) -> String {
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
            // 非 ASCII のキーを持つバケットで AccessDenied/署名不一致を避けるため、
            // SDK と同じく応答キーの URL エンコードを要求する。
            ("encoding-type".to_string(), "url".to_string()),
        ];
        if let Some(token) = continuation_token {
            params.push(("continuation-token".to_string(), token.to_string()));
        }
        let query = canonical_query(&params);
        format!("{}{}?{}", self.endpoint, self.bucket_path(), query)
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
    let xml = list_bucket_result_body(xml).ok_or_else(|| {
        NarouError::Platform("ListObjectsV2 response has an invalid ListBucketResult envelope".to_string())
    })?;
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
        // encoding-type=url を要求しているので、キーは percent-encoded。
        // デコードしないと論理キーと一致せず、保存物が見えなくなる。
        let key = percent_decode(&key);
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
        let close_start = content_start + end;
        cursor = close_start + xml[close_start..].find('>').unwrap() + 1;
    }

    if let Some(value) = element_text(xml, "NextContinuationToken")
        && !value.is_empty() {
            next_token = Some(percent_decode(&value));
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

/// HTTP 200 だけでは一覧の成功としない。外側の要素と閉じタグを確認し、
/// 本文以外 (HTML / Error / 途中で切れた応答 / 後続の別文書) を拒否する。
/// 要素の値や属性の意味は既存の小さなパーサーに任せる。
fn list_bucket_result_body(xml: &str) -> Option<&str> {
    let mut xml = xml.strip_prefix('\u{feff}').unwrap_or(xml).trim();
    if let Some(declaration) = xml.strip_prefix("<?xml") {
        if !declaration.starts_with(char::is_whitespace) {
            return None;
        }
        xml = declaration.split_once("?>")?.1.trim_start();
    }
    let open_end = xml_tag_end(xml)?;
    let open = xml.get(1..open_end)?;
    let root_name = open.split_ascii_whitespace().next()?.trim_end_matches('/');
    let local_name = match root_name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() => local,
        _ => root_name,
    };
    if !xml.starts_with('<') || local_name != "ListBucketResult" {
        return None;
    }
    if open.ends_with('/') {
        return xml[open_end + 1..].trim().is_empty().then_some("");
    }
    let body_start = open_end + 1;
    let mut cursor = body_start;
    let mut elements = vec![root_name];
    while let Some(offset) = xml[cursor..].find('<') {
        let start = cursor + offset;
        let rest = &xml[start..];
        // タグ風の文字列をコメントや CDATA 内の閉じタグと取り違えない。
        if rest.starts_with("<!--") {
            cursor = start + rest.find("-->")? + 3;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            cursor = start + rest.find("]]>")? + 3;
            continue;
        }
        let end = start + xml_tag_end(rest)?;
        let tag = &xml[start + 1..end];
        if let Some(close_name) = tag.strip_prefix('/') {
            if elements.pop()? != close_name.trim_end() {
                return None;
            }
            if elements.is_empty() {
                return xml[end + 1..].trim().is_empty().then_some(&xml[body_start..start]);
            }
        } else {
            let name = tag.split_ascii_whitespace().next()?.trim_end_matches('/');
            if name.is_empty() || name.starts_with(['!', '?']) {
                return None;
            }
            if !tag.ends_with('/') {
                elements.push(name);
            }
        }
        cursor = end + 1;
    }
    None
}

/// 引用符内の `>` をタグ終端として扱わない。
fn xml_tag_end(xml: &str) -> Option<usize> {
    let mut quote = None;
    for (index, ch) in xml.char_indices() {
        match (quote, ch) {
            (Some(expected), actual) if actual == expected => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
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
        let close_offset = find_element_close(rest, tag);
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
                cursor += close + rest[close..].find('>')? + 1;
            }
            // 閉じタグが無い (壊れた XML) か、ネストの対応が取れない。
            _ => return None,
        }
    }
    None
}

fn find_element_close(haystack: &str, tag: &str) -> Option<usize> {
    let mut search_from = 0usize;
    while let Some(offset) = haystack[search_from..].find("</") {
        let start = search_from + offset;
        let rest = &haystack[start + 2..];
        let end = rest.find('>')?;
        if rest[..end].trim_end().rsplit(':').next() == Some(tag) {
            return Some(start);
        }
        search_from = start + 2;
    }
    None
}

/// `<tag>...</tag>` の中身を実体参照込みで返す。
pub(crate) fn element_text(xml: &str, tag: &str) -> Option<String> {
    let start = find_tag(xml, tag)?;
    let after_open = xml[start..].find('>')? + start + 1;
    let rest = &xml[after_open..];
    let close = find_element_close(rest, tag)?;
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

/// `encoding-type=url` 応答の percent-encoding を戻す。UTF-8 として読めない
/// バイトはそのまま残す (キーは UTF-8 前提のため実害は無い)。
fn percent_decode(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push((h << 4) | l);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// SHA-256 の 64 文字 hex を Base64URL (padding 無し) へ変換する。
/// dedup プールのオブジェクト名に使う。hex でない入力は `None`。
fn sha256_hex_to_base64url(hex: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 32];
    for i in 0..32 {
        let hi = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
        bytes[i] = hi;
    }
    let mut out = String::with_capacity(43);
    let mut i = 0usize;
    while i + 3 <= 32 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        out.push(ALPHABET[n as usize & 63] as char);
        i += 3;
    }
    // 32 = 10*3 + 2 → 末尾 2 バイトを 3 文字で閉じる (padding 無し)。
    let n = ((bytes[30] as u32) << 16) | ((bytes[31] as u32) << 8);
    out.push(ALPHABET[(n >> 18) as usize & 63] as char);
    out.push(ALPHABET[(n >> 12) as usize & 63] as char);
    out.push(ALPHABET[(n >> 6) as usize & 63] as char);
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
    fn dedup_storage_key_pools_hash_named_illustrations() {
        let location = location().with_illustration_dedup();
        // sha256("test image") の hex → base64url
        let hex = "1187327c6d0f0b0b19b33ab211a549023aa9a41f359c6d0a827d7bd99f8d5994";
        assert_eq!(
            location.storage_key(&key(&format!("novels/site/title/挿絵/{hex}.png"))),
            "narou/develop/illustrations/EYcyfG0PCwsZszqyEaVJAjqppB81nG0Kgn172Z-NWZQ.png"
        );
        // 別作品でも同じ内容なら同じオブジェクトへ集約される。
        assert_eq!(
            location.storage_key(&key(&format!("novels/other/another/挿絵/{hex}.png"))),
            "narou/develop/illustrations/EYcyfG0PCwsZszqyEaVJAjqppB81nG0Kgn172Z-NWZQ.png"
        );
        // hex でない挿絵名は小説ごとの配置を保つ (衝突回避)。
        assert_eq!(
            location.storage_key(&key("novels/site/title/挿絵/i422674.jpg")),
            "narou/develop/novels/site/title/挿絵/i422674.jpg"
        );
        // 非挿絵キーは従来どおり。
        assert_eq!(
            location.storage_key(&key("novels/site/title/toc.yaml")),
            "narou/develop/novels/site/title/toc.yaml"
        );
    }

    #[test]
    fn dedup_is_off_by_default() {
        let location = location();
        let hex = "1187327c6d0f0b0b19b33ab211a549023aa9a41f359c6d0a827d7bd99f8d5994";
        assert_eq!(
            location.storage_key(&key(&format!("novels/site/title/挿絵/{hex}.png"))),
            format!("narou/develop/novels/site/title/挿絵/{hex}.png")
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
        let url = location.list_url("novels/", 100, Some("abc/def="));
        assert_eq!(
            url,
            "https://s3.example.com/narou-library?continuation-token=abc%2Fdef%3D&encoding-type=url&list-type=2&max-keys=100&prefix=narou%2Fdevelop%2Fnovels%2F"
        );

        let url = location.list_url("", 10, None);
        assert_eq!(
            url,
            "https://s3.example.com/narou-library?encoding-type=url&list-type=2&max-keys=10&prefix=narou%2Fdevelop%2F"
        );
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
