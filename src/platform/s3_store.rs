//! S3 互換ストレージ backed の [`ObjectStore`] / [`AssetStore`]。
//!
//! native (`NativeHttpClient`) と Worker (`WorkerHttpClient`) が同じ実装を
//! 使う。署名は [`super::s3_sigv4`]、URL の組み立てと一覧 XML の解釈は
//! [`super::s3_request`] にあり、ここには HTTP の実行と保存契約だけを置く。
//!
//! URL は path-style (`{endpoint}/{bucket}/{key}`)。バケット名の DNS 互換性に
//! 依存せず、MinIO のようなローカル実装でも同じ経路で動く。
//!
//! 大きいオブジェクトは [`MAX_OBJECT_BYTES`] まで一括で読み書きする
//! (multipart は未実装)。上限を超えるものは黙って切り捨てず失敗させる。

use std::sync::Arc;

use super::clock::Clock;
use super::http::{HttpClient, HttpMethod, HttpRequest, RedirectMode};
use super::object_store::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, content_type_for_key,
};
use super::s3_request::{S3Location, S3ObjectSummary, parse_list_objects_v2};
use super::s3_sigv4::{self, Credentials, RequestToSign};
use crate::error::{NarouError, Result};
use crate::platform::PlatformFuture;

/// `read_small` / `write_small` の上限。
pub const SMALL_CAP: u64 = 16 * 1024 * 1024;
/// 1 オブジェクトの上限。これを超えるものは streaming 対応まで扱わない。
pub const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// S3 への接続情報。値は呼び出し側 (設定 / 環境変数 / Secret) が解決する。
#[derive(Debug, Clone)]
pub struct S3StoreConfig {
    /// `https://s3.ap-northeast-1.wasabisys.com` のような endpoint
    /// (末尾スラッシュ無し)。path-style で使うのでバケット名は含めない。
    pub endpoint: String,
    pub bucket: String,
    /// 署名に使うリージョン。S3 互換サービスでも必須。
    pub region: String,
    /// バケット内の接頭辞 (`narou/develop` など)。空なら直下。
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl S3StoreConfig {
    /// 必須値の欠けを早期に落とす。片肺の設定でローカル保存へ黙って
    /// フォールバックさせない (fail-closed)。
    pub fn validate(&self) -> Result<()> {
        if self.access_key_id.is_empty() || self.secret_access_key.is_empty() {
            return Err(NarouError::Platform(
                "S3 credentials are not configured (access key id / secret access key)".to_string(),
            ));
        }
        if self.region.is_empty() {
            return Err(NarouError::Platform(
                "S3 region is not configured (signing requires it)".to_string(),
            ));
        }
        // endpoint と bucket の形式は S3Location が検証する。
        S3Location::new(
            self.endpoint.as_str(),
            self.bucket.as_str(),
            self.prefix.as_str(),
        )?;
        Ok(())
    }
}

struct CredentialsOwned {
    access_key_id: String,
    secret_access_key: String,
}

impl CredentialsOwned {
    fn borrowed(&self) -> Credentials<'_> {
        Credentials {
            access_key_id: &self.access_key_id,
            secret_access_key: &self.secret_access_key,
        }
    }
}

/// S3 バケットを [`ObjectStore`] / [`AssetStore`] として使う。
#[derive(Clone)]
pub struct S3Store {
    location: S3Location,
    region: String,
    host: String,
    scheme: String,
    credentials: Arc<CredentialsOwned>,
    http: Arc<dyn HttpClient>,
    clock: Arc<dyn Clock>,
}

impl S3Store {
    pub fn new(
        config: S3StoreConfig,
        http: Arc<dyn HttpClient>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        config.validate()?;
        let scheme = config
            .endpoint
            .split_once("://")
            .map(|(scheme, _)| scheme.to_string())
            .unwrap_or_else(|| "https".to_string());
        let location = S3Location::new(config.endpoint, config.bucket, config.prefix)?;
        let host = location.host()?;
        Ok(Self {
            location,
            region: config.region,
            host,
            scheme,
            credentials: Arc::new(CredentialsOwned {
                access_key_id: config.access_key_id,
                secret_access_key: config.secret_access_key,
            }),
            http,
            clock,
        })
    }

    /// 保存先 (バケット / prefix) と host を覗く。ログや presign 用。
    pub fn location(&self) -> &S3Location {
        &self.location
    }

    /// ダウンロード用の presigned URL を作る (クエリ認証)。
    ///
    /// 挿絵のような大きなオブジェクトを中継せず、クライアントに S3 から
    /// 直接取らせるために使う。署名が覆うのは host / path / query だけなので、
    /// scheme は設定された endpoint に合わせる (ローカル MinIO は http)。
    pub fn presign_get_url(&self, key: &ObjectKey, expires_secs: u64) -> String {
        let credentials = Credentials {
            access_key_id: &self.credentials.access_key_id,
            secret_access_key: &self.credentials.secret_access_key,
        };
        let url = s3_sigv4::presign_get(
            &self.host,
            &self.location.object_path(key),
            &credentials,
            &self.region,
            expires_secs,
            self.clock.now_utc(),
        );
        if self.scheme == "https" {
            url
        } else {
            url.replacen("https://", &format!("{}://", self.scheme), 1)
        }
    }

    /// 署名済みリクエストを送る。`path` と `query` は署名対象と一致させる。
    async fn send(
        &self,
        method: HttpMethod,
        url: &str,
        path: &str,
        query: &str,
        extra_headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<super::http::HttpResponse> {
        let payload_sha256 = s3_sigv4::payload_sha256(body.as_deref().unwrap_or(&[]));
        let amz_date = s3_sigv4::amz_date(self.clock.now_utc());
        let signed = s3_sigv4::sign(
            &RequestToSign {
                method: method_name(method),
                host: &self.host,
                path,
                query,
                headers: extra_headers,
                payload_sha256: &payload_sha256,
                amz_date: &amz_date,
            },
            &self.credentials.borrowed(),
            &self.region,
            "s3",
        );

        let mut request = match (method, body) {
            (HttpMethod::Put, Some(body)) => HttpRequest::put(url, body),
            (HttpMethod::Put, None) => HttpRequest::put(url, Vec::new()),
            (HttpMethod::Delete, _) => HttpRequest::delete(url),
            (HttpMethod::Get, _) => HttpRequest::get(url),
            (HttpMethod::Post, Some(body)) => HttpRequest::post(url, body),
            (HttpMethod::Post, None) => HttpRequest::post(url, Vec::new()),
        }
        // 保存先は利用者自身の設定値なので、公開アドレス判定は掛けない
        // (ローカル MinIO を許す)。リダイレクトは署名が壊れるので追わない。
        .with_trusted_endpoint()
        .with_redirect(RedirectMode::Manual);
        for (name, value) in extra_headers {
            request = request.with_header(*name, *value);
        }
        request = request
            .with_header("x-amz-date", signed.x_amz_date)
            .with_header("x-amz-content-sha256", signed.x_amz_content_sha256)
            .with_header("authorization", signed.authorization);
        self.http.send(request).await
    }

    async fn stat_object(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        // `HEAD` はエッジで 403 になる S3 互換サービスがあるため、
        // `GET` + `Range: bytes=0-0` でメタデータだけ読む。
        let response = self
            .send(
                HttpMethod::Get,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[("range", "bytes=0-0")],
                None,
            )
            .await?;
        let status = response.status;
        if status == 404 {
            return Ok(None);
        }
        if !matches!(status, 200 | 206) {
            return Err(NarouError::Platform(format!(
                "S3 GET {} failed with status {status}",
                key.as_ref()
            )));
        }
        let content_range = response.header("content-range").map(str::to_string);
        let content_length = response.header("content-length").map(str::to_string);
        let size = super::object_size(status, content_range.as_deref(), content_length.as_deref())
            .ok_or_else(|| {
                NarouError::Platform(format!(
                    "S3 GET {} returned no size (content-range={content_range:?})",
                    key.as_ref()
                ))
            })?;
        let etag = response
            .header("etag")
            .map(|value| value.trim_matches('"').to_string());
        let content_type = response.header("content-type").map(str::to_string);
        let last_modified = response
            .header("last-modified")
            .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc));
        Ok(Some(ObjectMetadata {
            key: key.clone(),
            size,
            etag,
            content_type,
            last_modified,
        }))
    }

    async fn read_object(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        let response = self
            .send(
                HttpMethod::Get,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[],
                None,
            )
            .await?;
        let (status, body) = status_and_body(response)?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            status => Err(NarouError::Platform(format!(
                "S3 GET {} failed with status {status}",
                key.as_ref()
            ))),
        }
    }

    async fn write_object(&self, key: &ObjectKey, data: Vec<u8>) -> Result<()> {
        if data.len() as u64 > MAX_OBJECT_BYTES {
            return Err(NarouError::Platform(format!(
                "S3 PUT {} exceeds the {} byte limit",
                key.as_ref(),
                MAX_OBJECT_BYTES
            )));
        }
        let content_type = content_type_for_key(key).unwrap_or("application/octet-stream");
        let response = self
            .send(
                HttpMethod::Put,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[("content-type", content_type)],
                Some(data),
            )
            .await?;
        let status = response.status;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(NarouError::Platform(format!(
                "S3 PUT {} failed with status {status}",
                key.as_ref()
            )))
        }
    }

    async fn delete_object(&self, key: &ObjectKey) -> Result<()> {
        let response = self
            .send(
                HttpMethod::Delete,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[],
                None,
            )
            .await?;
        let status = response.status;
        // S3 の DELETE は存在しなくても 204 を返す。404 も成功として扱う。
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(NarouError::Platform(format!(
                "S3 DELETE {} failed with status {status}",
                key.as_ref()
            )))
        }
    }

    async fn list_objects(&self, request: &ObjectListRequest) -> Result<ObjectListPage> {
        let (url, query) = self.location.list_url(
            request.prefix.as_ref(),
            request.limit.get(),
            request.cursor.as_deref(),
        );
        let response = self
            .send(
                HttpMethod::Get,
                &url,
                &self.location.bucket_path(),
                &query,
                &[],
                None,
            )
            .await?;
        let (status, body) = status_and_body(response)?;
        if status != 200 {
            return Err(NarouError::Platform(format!(
                "S3 LIST {} failed with status {status}",
                request.prefix.as_ref()
            )));
        }
        let xml = String::from_utf8(body)
            .map_err(|_| NarouError::Platform("S3 LIST response is not UTF-8".to_string()))?;
        let page = parse_list_objects_v2(&xml)?;

        let mut objects = Vec::new();
        for summary in page.objects {
            let Some(key) = self.logical_key(&summary) else {
                continue;
            };
            if !request.prefix.matches(&key) {
                continue;
            }
            objects.push(ObjectMetadata {
                key: key.clone(),
                size: summary.size,
                etag: summary.etag.clone(),
                content_type: content_type_for_key(&key).map(str::to_string),
                last_modified: summary.last_modified,
            });
        }
        Ok(ObjectListPage {
            objects,
            next_cursor: page.next_token,
        })
    }

    /// S3 のキーを論理キーへ戻す (prefix 不一致・フォルダ風キーは `None`)。
    fn logical_key(&self, summary: &S3ObjectSummary) -> Option<ObjectKey> {
        let storage_prefix = self.location.prefix();
        let rest = if storage_prefix.is_empty() {
            summary.key.as_str()
        } else {
            summary
                .key
                .strip_prefix(storage_prefix)?
                .strip_prefix('/')?
        };
        if rest.is_empty() || rest.ends_with('/') {
            return None;
        }
        ObjectKey::try_new(rest).ok()
    }

    async fn copy_object(&self, source: &ObjectKey, destination: &ObjectKey) -> Result<()> {
        let copy_source = self.location.copy_source(source);
        let response = self
            .send(
                HttpMethod::Put,
                &self.location.object_url(destination),
                &self.location.object_path(destination),
                "",
                &[("x-amz-copy-source", copy_source.as_str())],
                Some(Vec::new()),
            )
            .await?;
        let status = response.status;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(NarouError::Platform(format!(
                "S3 COPY {} -> {} failed with status {status}",
                source.as_ref(),
                destination.as_ref()
            )))
        }
    }
}

/// SigV4 に渡すメソッド名。
fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
    }
}

/// 応答のステータスとボディ。エラー応答のボディは捨てる。
fn status_and_body(response: super::http::HttpResponse) -> Result<(u16, Vec<u8>)> {
    let status = response.status;
    if status != 200 {
        return Ok((status, Vec::new()));
    }
    if response.body.len() as u64 > MAX_OBJECT_BYTES {
        return Err(NarouError::Platform(format!(
            "S3 object exceeds the {} byte limit",
            MAX_OBJECT_BYTES
        )));
    }
    Ok((status, response.body))
}

impl ObjectStore for S3Store {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(self.stat_object(key))
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let Some(data) = self.read_object(key).await? else {
                return Ok(None);
            };
            if data.len() as u64 > SMALL_CAP {
                return Err(NarouError::Platform(format!(
                    "Object {} exceeds small-read cap ({} bytes)",
                    key.as_ref(),
                    SMALL_CAP
                )));
            }
            Ok(Some(data))
        })
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if data.len() as u64 > SMALL_CAP {
                return Err(NarouError::Platform(format!(
                    "Object {} exceeds small-write cap ({} bytes)",
                    key.as_ref(),
                    SMALL_CAP
                )));
            }
            self.write_object(key, data).await
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.delete_object(key))
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(self.list_objects(request))
    }
}

impl AssetStore for S3Store {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(self.stat_object(key))
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move {
            let Some(data) = self.read_object(key).await? else {
                return Ok(None);
            };
            let stream = futures::stream::once(async move { Ok(data) });
            Ok(Some(Box::pin(stream) as AssetStream))
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use futures::StreamExt;
            let mut stream = stream;
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                data.extend_from_slice(&chunk);
                if data.len() as u64 > MAX_OBJECT_BYTES {
                    return Err(NarouError::Platform(format!(
                        "S3 PUT {} exceeds the {} byte limit",
                        key.as_ref(),
                        MAX_OBJECT_BYTES
                    )));
                }
            }
            self.write_object(key, data).await
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.delete_object(key))
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.copy_object(source, destination))
    }

    /// S3 に rename は無いので server-side copy + delete で移動する。
    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.copy_object(source, destination).await?;
            self.delete_object(source).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::http::HttpResponse;
    use crate::platform::mocks::MockHttpClient;
    use crate::platform::{ObjectPrefix, SystemClock};

    const ENDPOINT: &str = "https://s3.example.com";

    fn store(http: Arc<MockHttpClient>) -> S3Store {
        S3Store::new(
            S3StoreConfig {
                endpoint: ENDPOINT.to_string(),
                bucket: "bucket".to_string(),
                region: "ap-northeast-1".to_string(),
                prefix: "narou/test".to_string(),
                access_key_id: "AKIAEXAMPLE".to_string(),
                secret_access_key: "secret".to_string(),
            },
            http,
            Arc::new(SystemClock),
        )
        .unwrap()
    }

    fn key(name: &str) -> ObjectKey {
        ObjectKey::try_new(name).unwrap()
    }

    fn ok(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/yaml".to_string())],
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn write_small_signs_a_put_with_the_content_type() {
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            "https://s3.example.com/bucket/narou/test/novels/site/title/toc.yaml",
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        let store = store(http.clone());
        futures::executor::block_on(ObjectStore::write_small(
            &store,
            &key("novels/site/title/toc.yaml"),
            b"body".to_vec(),
        ))
        .unwrap();

        assert_eq!(
            http.requested_urls(),
            vec!["PUT https://s3.example.com/bucket/narou/test/novels/site/title/toc.yaml"]
        );
        let (_, headers) = http.sent_requests().into_iter().next().unwrap();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(header("content-type").as_deref(), Some("application/yaml"));
        assert!(
            header("authorization")
                .unwrap()
                .starts_with("AWS4-HMAC-SHA256 ")
        );
        assert!(
            header("authorization")
                .unwrap()
                .contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date")
        );
        assert_eq!(http.sent_bodies(), vec![Some(b"body".to_vec())]);
    }

    #[test]
    fn read_small_returns_none_for_missing_objects() {
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            "https://s3.example.com/bucket/narou/test/novels/site/title/toc.yaml",
            HttpResponse {
                status: 404,
                headers: Vec::new(),
                body: b"<Error/>".to_vec(),
            },
        );
        let store = store(http.clone());
        let data = futures::executor::block_on(ObjectStore::read_small(
            &store,
            &key("novels/site/title/toc.yaml"),
        ))
        .unwrap();
        assert!(data.is_none());
    }

    #[test]
    fn read_small_returns_the_stored_bytes() {
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            "https://s3.example.com/bucket/narou/test/novels/site/title/toc.yaml",
            ok("toc: 1"),
        );
        let store = store(http);
        let data = futures::executor::block_on(ObjectStore::read_small(
            &store,
            &key("novels/site/title/toc.yaml"),
        ))
        .unwrap();
        assert_eq!(data.as_deref(), Some(&b"toc: 1"[..]));
    }

    #[test]
    fn stat_reads_the_size_from_a_range_response() {
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            "https://s3.example.com/bucket/narou/test/novels/site/title/illustration_cache.yaml",
            HttpResponse {
                status: 206,
                headers: vec![
                    ("content-range".to_string(), "bytes 0-0/4096".to_string()),
                    ("etag".to_string(), "\"abc123\"".to_string()),
                    (
                        "last-modified".to_string(),
                        "Mon, 01 Sep 2025 10:00:00 GMT".to_string(),
                    ),
                ],
                body: b"x".to_vec(),
            },
        );
        let store = store(http.clone());
        let metadata = futures::executor::block_on(ObjectStore::stat(
            &store,
            &key("novels/site/title/illustration_cache.yaml"),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(metadata.size, 4096);
        assert_eq!(metadata.etag.as_deref(), Some("abc123"));
        assert!(metadata.last_modified.is_some());
        let (_, headers) = http.sent_requests().into_iter().next().unwrap();
        assert!(
            headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("range") && value == "bytes=0-0")
        );
    }

    #[test]
    fn list_page_maps_storage_keys_back_to_logical_keys() {
        let http = Arc::new(MockHttpClient::new());
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>next-token</NextContinuationToken>
  <Contents>
    <Key>narou/test/novels/site/title/挿絵/0001.jpg</Key>
    <Size>2048</Size><ETag>&quot;e1&quot;</ETag>
  </Contents>
  <Contents>
    <Key>narou/test/novels/site/</Key>
    <Size>0</Size>
  </Contents>
</ListBucketResult>"#;
        http.add_response(
            "https://s3.example.com/bucket?list-type=2&max-keys=10&prefix=narou%2Ftest%2Fnovels%2Fsite%2Ftitle%2F%E6%8C%BF%E7%B5%B5",
            HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/xml".to_string())],
                body: body.as_bytes().to_vec(),
            },
        );
        let store = store(http);
        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site/title/挿絵").unwrap(),
            std::num::NonZeroUsize::new(10).unwrap(),
        );
        let page = futures::executor::block_on(ObjectStore::list_page(&store, &request)).unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(
            page.objects[0].key.as_ref(),
            "novels/site/title/挿絵/0001.jpg"
        );
        assert_eq!(page.objects[0].size, 2048);
        assert_eq!(page.next_cursor.as_deref(), Some("next-token"));
    }

    #[test]
    fn delete_accepts_a_missing_object() {
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            "https://s3.example.com/bucket/narou/test/novels/site/title/%E6%8C%BF%E7%B5%B5/0001.jpg",
            HttpResponse {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        let store = store(http.clone());
        futures::executor::block_on(ObjectStore::delete(
            &store,
            &key("novels/site/title/挿絵/0001.jpg"),
        ))
        .unwrap();
        assert_eq!(
            http.requested_urls(),
            vec![
                "DELETE https://s3.example.com/bucket/narou/test/novels/site/title/%E6%8C%BF%E7%B5%B5/0001.jpg"
            ]
        );
    }

    #[test]
    fn presign_get_url_carries_the_signature_and_keeps_the_endpoint_scheme() {
        let http = Arc::new(MockHttpClient::new());
        let store = store(http);
        let url = store.presign_get_url(&key("novels/site/title/挿絵/0001.jpg"), 3600);
        assert!(
            url.starts_with("https://s3.example.com/bucket/narou/test/"),
            "{url}"
        );
        assert!(url.contains("X-Amz-Signature="), "{url}");
        assert!(url.contains("X-Amz-Expires=3600"), "{url}");

        let local = S3Store::new(
            S3StoreConfig {
                endpoint: "http://127.0.0.1:9000".to_string(),
                bucket: "bucket".to_string(),
                region: "us-east-1".to_string(),
                prefix: String::new(),
                access_key_id: "minio".to_string(),
                secret_access_key: "minio123".to_string(),
            },
            Arc::new(MockHttpClient::new()),
            Arc::new(SystemClock),
        )
        .unwrap();
        let url = local.presign_get_url(&key("novels/site/title/挿絵/0001.jpg"), 60);
        assert!(url.starts_with("http://127.0.0.1:9000/bucket/"), "{url}");
    }

    #[test]
    fn a_half_configured_store_fails_loudly() {
        let http = Arc::new(MockHttpClient::new());
        let error = match S3Store::new(
            S3StoreConfig {
                endpoint: ENDPOINT.to_string(),
                bucket: "bucket".to_string(),
                region: "ap-northeast-1".to_string(),
                prefix: String::new(),
                access_key_id: String::new(),
                secret_access_key: "secret".to_string(),
            },
            http.clone(),
            Arc::new(SystemClock),
        ) {
            Ok(_) => panic!("a store without credentials must not build"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("credentials"), "{error}");
        assert_eq!(http.request_count(), 0);
    }
}
