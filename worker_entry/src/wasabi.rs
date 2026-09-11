use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest,
    ObjectMetadata, ObjectStore, PlatformFuture,
};
use sha2::{Digest, Sha256};
use url::Url;
use wasm_bindgen::JsValue;
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Response};

type HmacSha256 = Hmac<Sha256>;

pub const SMALL_OBJECT_LIMIT: usize = 16 * 1024 * 1024;
pub const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct WasabiConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    access_key: String,
    secret_key: String,
}

impl WasabiConfig {
    pub fn from_env(env: &Env) -> Result<Self> {
        let endpoint = env
            .var("WASABI_ENDPOINT")
            .map_err(worker_error)?
            .to_string();
        let bucket = env.var("WASABI_BUCKET").map_err(worker_error)?.to_string();
        let region = env
            .var("WASABI_REGION")
            .map_err(worker_error)?
            .to_string();
        let prefix = env
            .var("WASABI_PREFIX")
            .map_err(worker_error)
            .map(|value| value.to_string())
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        let access_key = env
            .secret("WASABI_ACCESS_KEY")
            .map_err(worker_error)?
            .to_string();
        let secret_key = env
            .secret("WASABI_SECRET_KEY")
            .map_err(worker_error)?
            .to_string();
        if endpoint.is_empty() || bucket.is_empty() || region.is_empty() || access_key.is_empty() || secret_key.is_empty() {
            return Err(NarouError::Platform("Wasabi storage configuration is incomplete".to_string()));
        }
        Ok(Self { endpoint, bucket, region, prefix, access_key, secret_key })
    }
}

#[derive(Debug, Clone)]
pub struct WasabiObjectStore {
    config: Arc<WasabiConfig>,
    subrequests: crate::budget::SubrequestBudget,
}

impl WasabiObjectStore {
    pub fn new(config: WasabiConfig, subrequests: crate::budget::SubrequestBudget) -> Self {
        Self { config: Arc::new(config), subrequests }
    }

    fn object_key(&self, key: &ObjectKey) -> String {
        if self.config.prefix.is_empty() {
            key.as_ref().to_string()
        } else {
            format!("{}/{}", self.config.prefix, key.as_ref())
        }
    }

    fn object_url(&self, key: &str) -> Result<Url> {
        let base = self.config.endpoint.trim_end_matches('/');
        let path = format!("{base}/{}/{}", self.config.bucket, key);
        Url::parse(&path).map_err(|error| NarouError::Platform(format!("invalid Wasabi endpoint: {error}")))
    }

    async fn request(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        body: Option<Vec<u8>>,
        extra_headers: &[(String, String)],
        redirect: RequestRedirect,
    ) -> Result<Response> {
        let mut url = self.object_url(key)?;
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in query {
                pairs.append_pair(name, value);
            }
        }
        let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let date = timestamp[..8].to_string();
        let payload_hash = body.as_deref().map(sha256_hex).unwrap_or_else(|| sha256_hex(b""));
        let host = url
            .host_str()
            .ok_or_else(|| NarouError::Platform("Wasabi endpoint has no host".to_string()))?;
        let host = url
            .port()
            .map(|port| format!("{host}:{port}"))
            .unwrap_or_else(|| host.to_string());
        let mut signed_headers = BTreeMap::new();
        signed_headers.insert("host".to_string(), host.to_string());
        signed_headers.insert("x-amz-content-sha256".to_string(), payload_hash.clone());
        signed_headers.insert("x-amz-date".to_string(), timestamp.clone());
        for (name, value) in extra_headers {
            signed_headers.insert(name.to_ascii_lowercase(), normalize_header(value));
        }
        let authorization = authorization_header(
            method.as_ref(),
            &url,
            query,
            &signed_headers,
            &payload_hash,
            &timestamp,
            &date,
            &self.config.region,
            &self.config.access_key,
            &self.config.secret_key,
        )?;

        let headers = Headers::new();
        for (name, value) in signed_headers.iter() {
            headers.set(name, value).map_err(worker_error)?;
        }
        headers.set("authorization", &authorization).map_err(worker_error)?;
        let mut init = RequestInit::new();
        init.with_method(method);
        init.with_redirect(redirect);
        init.with_headers(headers);
        if let Some(body) = body {
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(body.as_slice()))));
        }
        let request = Request::new_with_init(url.as_str(), &init).map_err(worker_error)?;
        self.subrequests.record();
        Fetch::Request(request).send().await.map_err(worker_error)
    }

    async fn check_response(response: Response) -> Result<Response> {
        let status = response.status_code();
        if (200..300).contains(&status) {
            Ok(response)
        } else if status == 404 {
            Err(NarouError::Platform("not found".to_string()))
        } else {
            Err(NarouError::Platform(format!("Wasabi request failed with status {status}")))
        }
    }

    async fn request_bytes(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        body: Option<Vec<u8>>,
        headers: &[(String, String)],
    ) -> Result<Response> {
        Self::check_response(self.request(method, key, query, body, headers, RequestRedirect::Follow).await?).await
    }
}

impl ObjectStore for WasabiObjectStore {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        let logical_key = key.clone();
        let storage_key = self.object_key(key);
        Box::pin(async move {
            let response = self
                .request(Method::Head, &storage_key, &[], None, &[], RequestRedirect::Follow)
                .await?;
            if response.status_code() == 404 {
                return Ok(None);
            }
            let response = Self::check_response(response).await?;
            let size = response
                .headers()
                .get("content-length")
                .map_err(worker_error)?
                .and_then(|value| value.parse().ok())
                .unwrap_or_default();
            let etag = response.headers().get("etag").map_err(worker_error)?;
            let content_type = response.headers().get("content-type").map_err(worker_error)?;
            Ok(Some(ObjectMetadata {
                key: logical_key,
                size,
                etag,
                content_type,
                last_modified: None,
            }))
        })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        let key = self.object_key(key);
        Box::pin(async move {
            let mut response = match self.request(Method::Get, &key, &[], None, &[], RequestRedirect::Follow).await {
                Ok(response) if response.status_code() == 404 => return Ok(None),
                Ok(response) => Self::check_response(response).await?,
                Err(error) => return Err(error),
            };
            if response
                .headers()
                .get("content-length")
                .map_err(worker_error)?
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|size| size > SMALL_OBJECT_LIMIT)
            {
                return Err(NarouError::Platform("small object exceeds configured limit".to_string()));
            }
            let bytes = response.bytes().await.map_err(worker_error)?;
            if bytes.len() > SMALL_OBJECT_LIMIT {
                return Err(NarouError::Platform("small object exceeds configured limit".to_string()));
            }
            Ok(Some(bytes))
        })
    }

    fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        let key = self.object_key(key);
        Box::pin(async move {
            if data.len() > SMALL_OBJECT_LIMIT {
                return Err(NarouError::Platform("small object exceeds configured limit".to_string()));
            }
            self.request_bytes(
                Method::Put,
                &key,
                &[],
                Some(data),
                &[("content-type".to_string(), "application/octet-stream".to_string())],
            )
            .await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let key = self.object_key(key);
        Box::pin(async move {
            let response = self.request(Method::Delete, &key, &[], None, &[], RequestRedirect::Follow).await?;
            if response.status_code() == 404 {
                return Ok(());
            }
            Self::check_response(response).await?;
            Ok(())
        })
    }

    fn list_page<'a>(&'a self, request: &'a ObjectListRequest) -> PlatformFuture<'a, Result<ObjectListPage>> {
        let prefix = if self.config.prefix.is_empty() {
            request.prefix.as_ref().to_string()
        } else if request.prefix.as_ref().is_empty() {
            format!("{}/", self.config.prefix)
        } else {
            format!("{}/{}", self.config.prefix, request.prefix.as_ref())
        };
        let storage_prefix = self.config.prefix.clone();
        let mut query = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), request.limit.get().to_string()),
            ("prefix".to_string(), prefix),
        ];
        if let Some(cursor) = &request.cursor {
            query.push(("continuation-token".to_string(), cursor.clone()));
        }
        Box::pin(async move {
            let response = self.request(Method::Get, "", &query, None, &[], RequestRedirect::Follow).await?;
            let mut response = Self::check_response(response).await?;
            let body = response.bytes().await.map_err(worker_error)?;
            parse_list_page(&body, &storage_prefix, &request.prefix)
        })
    }
}

impl AssetStore for WasabiObjectStore {
    fn stat<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        ObjectStore::stat(self, key)
    }

    fn read_stream<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        let key = self.object_key(key);
        Box::pin(async move {
            let response = match self.request(Method::Get, &key, &[], None, &[], RequestRedirect::Follow).await {
                Ok(response) if response.status_code() == 404 => return Ok(None),
                Ok(response) => Self::check_response(response).await?,
                Err(error) => return Err(error),
            };
            let mut response = response;
            let stream = response
                .stream()
                .map_err(worker_error)?
                .map(|chunk| chunk.map_err(worker_error));
            Ok(Some(Box::pin(stream) as AssetStream))
        })
    }

    fn write_stream<'a>(&'a self, key: &'a ObjectKey, mut stream: AssetStream) -> PlatformFuture<'a, Result<()>> {
        let key = self.object_key(key);
        Box::pin(async move {
            let mut upload_id = None;
            let result = async {
                let first = loop {
                    match stream.next().await {
                        None => {
                            self.request_bytes(Method::Put, &key, &[], Some(Vec::new()), &[]).await?;
                            return Ok(());
                        }
                        Some(chunk) => {
                            let chunk = chunk?;
                            if !chunk.is_empty() {
                                break chunk;
                            }
                        }
                    }
                };
                let response = self
                    .request(Method::Post, &key, &[("uploads".to_string(), String::new())], None, &[], RequestRedirect::Follow)
                    .await?;
                let mut response = Self::check_response(response).await?;
                let body = response.bytes().await.map_err(worker_error)?;
                let id = xml_text(&body, "UploadId")?
                    .ok_or_else(|| NarouError::Platform("Wasabi multipart response omitted UploadId".to_string()))?;
                upload_id = Some(id.clone());
                let mut parts = Vec::new();
                let mut part = Vec::with_capacity(MULTIPART_PART_SIZE);
                let mut part_number = 1u32;
                let mut pending = Some(first);
                loop {
                    let chunk = match pending.take() {
                        Some(chunk) => chunk,
                        None => match stream.next().await {
                            Some(chunk) => chunk?,
                            None => break,
                        },
                    };
                    let mut offset = 0;
                    while offset < chunk.len() {
                        let take = (MULTIPART_PART_SIZE - part.len()).min(chunk.len() - offset);
                        part.extend_from_slice(&chunk[offset..offset + take]);
                        offset += take;
                        if part.len() == MULTIPART_PART_SIZE {
                            let etag = self.upload_part(&key, &id, part_number, std::mem::take(&mut part)).await?;
                            parts.push((part_number, etag));
                            part_number += 1;
                            part = Vec::with_capacity(MULTIPART_PART_SIZE);
                        }
                    }
                }
                if !part.is_empty() {
                    let etag = self.upload_part(&key, &id, part_number, part).await?;
                    parts.push((part_number, etag));
                }
                self.complete_multipart(&key, &id, &parts).await
            }.await;
            if result.is_err()
                && let Some(id) = upload_id
            {
                let _ = self
                    .request(
                        Method::Delete,
                        &key,
                        &[("uploadId".to_string(), id)],
                        None,
                        &[],
                        RequestRedirect::Follow,
                    )
                    .await;
            }
            result
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }

    fn copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let source = self.object_key(source);
        let destination = self.object_key(destination);
        Box::pin(async move {
            self.request_bytes(
                Method::Put,
                &destination,
                &[],
                None,
                &[("x-amz-copy-source".to_string(), canonical_uri(&format!("/{}/{}", self.config.bucket, source)))],
            ).await?;
            Ok(())
        })
    }

    fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.copy(source, destination).await?;
            ObjectStore::delete(self, source).await
        })
    }
}

impl WasabiObjectStore {
    async fn upload_part(&self, key: &str, upload_id: &str, number: u32, body: Vec<u8>) -> Result<String> {
        // The writer fills every non-final part to MULTIPART_PART_SIZE; S3 permits
        // the final part to be smaller than its minimum multipart size.
        let response = self.request_bytes(
            Method::Put,
            key,
            &[("partNumber".to_string(), number.to_string()), ("uploadId".to_string(), upload_id.to_string())],
            Some(body),
            &[("content-type".to_string(), "application/octet-stream".to_string())],
        ).await?;
        response.headers().get("etag").map_err(worker_error)?.ok_or_else(|| NarouError::Platform("multipart part omitted ETag".to_string()))
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)]) -> Result<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (number, etag) in parts {
            body.push_str(&format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"));
        }
        body.push_str("</CompleteMultipartUpload>");
        self.request_bytes(
            Method::Post,
            key,
            &[("uploadId".to_string(), upload_id.to_string())],
            Some(body.into_bytes()),
            &[("content-type".to_string(), "application/xml".to_string())],
        ).await?;
        Ok(())
    }
}

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Wasabi request error: {error}"))
}

fn normalize_header(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn canonical_uri(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'/' {
            output.push('/');
            index += 1;
        } else if byte == b'%' && index + 2 < bytes.len() && is_hex(bytes[index + 1]) && is_hex(bytes[index + 2]) {
            output.push('%');
            output.push((bytes[index + 1] as char).to_ascii_uppercase());
            output.push((bytes[index + 2] as char).to_ascii_uppercase());
            index += 3;
        } else if is_unreserved(byte) {
            output.push(byte as char);
            index += 1;
        } else {
            output.push_str(&format!("%{byte:02X}"));
            index += 1;
        }
    }
    output
}

pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut encoded = query
        .iter()
        .map(|(key, value)| (percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>();
    encoded.sort();
    encoded.into_iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("&")
}

#[allow(clippy::too_many_arguments)]
pub fn authorization_header(
    method: &str,
    url: &Url,
    query: &[(String, String)],
    headers: &BTreeMap<String, String>,
    payload_hash: &str,
    timestamp: &str,
    date: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<String> {
    let canonical_headers = headers.iter().map(|(key, value)| format!("{key}:{}\n", normalize_header(value))).collect::<String>();
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let path = canonical_uri(url.path());
    let canonical_request = format!("{method}\n{path}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}", canonical_query(query));
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
    let signing_key = signing_key(secret_key, date, region, "s3");
    let signature = hmac_hex(&signing_key, string_to_sign.as_bytes());
    Ok(format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"))
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let date_key = hmac_bytes(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let region_key = hmac_bytes(&date_key, region.as_bytes());
    let service_key = hmac_bytes(&region_key, service.as_bytes());
    hmac_bytes(&service_key, b"aws4_request")
}

fn hmac_bytes(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts all key sizes");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}
fn hmac_hex(key: &[u8], value: &[u8]) -> String { hex::encode(hmac_bytes(key, value)) }
fn is_unreserved(byte: u8) -> bool { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') }
fn is_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}
fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if is_unreserved(*byte) { output.push(*byte as char); } else { output.push_str(&format!("%{byte:02X}")); }
    }
    output
}

fn xml_text(body: &[u8], tag: &str) -> Result<Option<String>> {
    let text = std::str::from_utf8(body)
        .map_err(|error| NarouError::Platform(format!("invalid Wasabi XML response: {error}")))?;
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let Some(start) = text.find(&start_tag) else {
        return Ok(None);
    };
    let value_start = start + start_tag.len();
    let end = text[value_start..]
        .find(&end_tag)
        .map(|offset| value_start + offset)
        .ok_or_else(|| NarouError::Platform(format!("Wasabi XML tag <{tag}> is not closed")))?;
    let value = &text[value_start..end];
    if value.contains('<') {
        return Err(NarouError::Platform(format!("Wasabi XML tag <{tag}> contains nested markup")));
    }
    Ok(Some(xml_unescape(value)?))
}

fn xml_unescape(value: &str) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut remainder = value;
    while let Some(ampersand) = remainder.find('&') {
        output.push_str(&remainder[..ampersand]);
        let entity = remainder
            .get(ampersand + 1..)
            .and_then(|tail| tail.find(';').map(|end| &tail[..end]))
            .ok_or_else(|| NarouError::Platform("unterminated XML entity in Wasabi response".to_string()))?;
        let decoded = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            entity if let Some(value) = entity.strip_prefix("#x") => char::from_u32(
                u32::from_str_radix(value, 16)
                    .map_err(|_| NarouError::Platform("invalid hexadecimal XML entity".to_string()))?,
            )
            .ok_or_else(|| NarouError::Platform("invalid hexadecimal XML entity".to_string()))?,
            entity if let Some(value) = entity.strip_prefix('#') => char::from_u32(
                value
                    .parse::<u32>()
                    .map_err(|_| NarouError::Platform("invalid decimal XML entity".to_string()))?,
            )
            .ok_or_else(|| NarouError::Platform("invalid decimal XML entity".to_string()))?,
            _ => return Err(NarouError::Platform("unknown XML entity in Wasabi response".to_string())),
        };
        output.push(decoded);
        remainder = &remainder[ampersand + entity.len() + 2..];
    }
    output.push_str(remainder);
    Ok(output)
}

fn parse_list_page(
    body: &[u8],
    storage_prefix: &str,
    prefix: &narou_rs::platform::ObjectPrefix,
) -> Result<ObjectListPage> {
    let text = std::str::from_utf8(body)
        .map_err(|error| NarouError::Platform(format!("invalid Wasabi list response: {error}")))?;
    let mut objects = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find("<Contents>") {
        let start = cursor + offset;
        let value_start = start + "<Contents>".len();
        let end = text[value_start..]
            .find("</Contents>")
            .map(|offset| value_start + offset)
            .ok_or_else(|| NarouError::Platform("Wasabi list entry is not closed".to_string()))?;
        let entry = &text[start..end + "</Contents>".len()];
        let Some(key) = xml_text(entry.as_bytes(), "Key")? else {
            return Err(NarouError::Platform("Wasabi list entry omitted Key".to_string()));
        };
        let key = key
            .strip_prefix(storage_prefix)
            .unwrap_or(&key)
            .trim_start_matches('/');
        if let Ok(key) = ObjectKey::try_new(key)
            && prefix.matches(&key)
        {
            let size = xml_text(entry.as_bytes(), "Size")?
                .ok_or_else(|| NarouError::Platform("Wasabi list entry omitted Size".to_string()))?
                .parse()
                .map_err(|_| NarouError::Platform("Wasabi list entry has invalid Size".to_string()))?;
            let etag = xml_text(entry.as_bytes(), "ETag")?;
            objects.push(ObjectMetadata {
                key,
                size,
                etag,
                content_type: None,
                last_modified: None,
            });
        }
        cursor = end + "</Contents>".len();
    }
    Ok(ObjectListPage {
        objects,
        next_cursor: xml_text(body, "NextContinuationToken")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_path_keeps_separators_and_encodes_utf8() {
        assert_eq!(canonical_uri("/a b/日本語+%"), "/a%20b/%E6%97%A5%E8%AA%9E%2B%25");
        assert_eq!(canonical_uri("/a%20b/%E6%97%A5%E6%9C%AC%E8%AA%9E"), "/a%20b/%E6%97%A5%E6%9C%AC%E8%AA%9E");
    }

    #[test]
    fn query_sorting_is_encoded_and_stable() {
        assert_eq!(canonical_query(&[("prefix".into(), "a b".into()), ("x".into(), "+".into())]), "prefix=a%20b&x=%2B");
    }

    #[test]
    fn list_page_strips_storage_prefix_and_decodes_xml() {
        let body = br#"<ListBucketResult><Contents><Key>tenant/novels/a&amp;b/toc.yaml</Key><Size>12</Size><ETag>&quot;etag&quot;</ETag></Contents><NextContinuationToken>next&amp;cursor</NextContinuationToken></ListBucketResult>"#;
        let prefix = narou_rs::platform::ObjectPrefix::new("novels").unwrap();
        let page = parse_list_page(body, "tenant", &prefix).unwrap();
        assert_eq!(page.objects[0].key.as_ref(), "novels/a&b/toc.yaml");
        assert_eq!(page.objects[0].size, 12);
        assert_eq!(page.objects[0].etag.as_deref(), Some("\"etag\""));
        assert_eq!(page.next_cursor.as_deref(), Some("next&cursor"));
    }

    #[test]
    fn list_page_rejects_unclosed_contents() {
        let body = br#"<ListBucketResult><Contents><Key>novels/a</Key><Size>1</Size></ListBucketResult>"#;
        let prefix = narou_rs::platform::ObjectPrefix::new("novels").unwrap();
        assert!(parse_list_page(body, "", &prefix).is_err());
    }

    #[test]
    fn xml_unescape_does_not_decode_nested_entities_twice() {
        assert_eq!(xml_unescape("&amp;lt;").unwrap(), "&lt;");
        assert_eq!(xml_unescape("&#x65;&#101;").unwrap(), "ee");
    }
    #[test]
    fn authorization_header_matches_aws_get_vector() {
        let mut headers = BTreeMap::new();
        headers.insert("host".to_string(), "examplebucket.s3.amazonaws.com".to_string());
        headers.insert("range".to_string(), "bytes=0-9".to_string());
        headers.insert(
            "x-amz-content-sha256".to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        );
        headers.insert("x-amz-date".to_string(), "20130524T000000Z".to_string());
        let header = authorization_header(
            "GET",
            &Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap(),
            &[],
            &headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "20130524T000000Z",
            "20130524",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        )
        .unwrap();
        assert_eq!(
            header,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }
}
