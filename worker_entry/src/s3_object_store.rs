//! S3 互換ストレージ backed の ObjectStore / AssetStore (Worker ランタイム)。
//!
//! 論理キー (`NovelObjectKeys`) をそのまま S3 のキーに写像し、`S3_PREFIX`
//! 配下へ格納する。署名は core の [`narou_rs::platform::s3_sigv4`]、URL と
//! 一覧 XML の解釈は [`narou_rs::platform::s3_request`] を使い、ここには
//! HTTP の実行だけを置く。
//!
//! URL は path-style (`{endpoint}/{bucket}/{key}`)。バケット名の DNS 互換性に
//! 依存せず、MinIO などのローカル実装でも同じ経路で動く。
//!
//! 大きいオブジェクトの streaming (multipart) は P4 の範囲なので、ここでは
//! 上限 [`MAX_OBJECT_BYTES`] までを一括で読み書きする。

use std::sync::Arc;

use chrono::Utc;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::s3_request::{parse_list_objects_v2, S3Location, S3ObjectSummary};
use narou_rs::platform::s3_sigv4::{self, Credentials, RequestToSign};
use narou_rs::platform::{
    content_type_for_key, AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest,
    ObjectMetadata, ObjectStore, PlatformFuture,
};
use wasm_bindgen::JsValue;
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

/// `read_small` / `write_small` の上限 (D1 アダプタと同じ契約)。
const SMALL_CAP: u64 = 16 * 1024 * 1024;
/// 1 オブジェクトの上限。これを超えるものは streaming 対応まで扱わない。
const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("S3 storage error: {error}"))
}

fn required_var(env: &Env, name: &str) -> worker::Result<String> {
    match env.var(name) {
        Ok(value) => {
            let value = value.to_string();
            if value.trim().is_empty() {
                Err(worker::Error::RustError(format!("{name} is empty")))
            } else {
                Ok(value)
            }
        }
        Err(error) => Err(worker::Error::RustError(format!(
            "{name} is not configured: {error}"
        ))),
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

/// S3 バケットを ObjectStore / AssetStore として使う。
#[derive(Clone)]
pub struct S3ObjectStore {
    location: S3Location,
    region: String,
    host: String,
    credentials: Arc<CredentialsOwned>,
}

impl S3ObjectStore {
    /// バインディングから構成する。値が欠けていれば失敗させる (fail-closed)。
    /// Secrets Store のバインディング (`<name>_STORE`) があればそこから読む。
    ///
    /// Cloudflare Secrets Store を使うと、資格情報を Worker 個別の secret ではなく
    /// アカウント共有のストアに置ける。値はランタイムが isolate ごとにキャッシュする
    /// ので、リクエストごとに `get` しても実害は小さい。
    async fn secret_or_store(env: &Env, name: &str) -> worker::Result<String> {
        let binding = format!("{name}_STORE");
        if let Ok(store) = env.secret_store(&binding)
            && let Ok(Some(value)) = store.get().await
            && !value.is_empty()
        {
            return Ok(value);
        }
        env.secret(name).map(|secret| secret.to_string())
    }

    /// 設定値の解決順: Secrets Store (`<name>_STORE`) → vars → secrets。
    ///
    /// Dantalian と同じく、endpoint / region / bucket も Secrets Store に置ける
    /// (リポジトリと CI には名前しか残らない)。
    async fn config_value(env: &Env, name: &str) -> Option<String> {
        let binding = format!("{name}_STORE");
        if let Ok(store) = env.secret_store(&binding)
            && let Ok(Some(value)) = store.get().await
            && !value.is_empty()
        {
            return Some(value);
        }
        if let Ok(value) = env.var(name) {
            let value = value.to_string();
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
        env.secret(name)
            .ok()
            .map(|secret| secret.to_string())
            .filter(|value| !value.trim().is_empty())
    }

    async fn require_config(env: &Env, name: &str) -> worker::Result<String> {
        Self::config_value(env, name)
            .await
            .ok_or_else(|| worker::Error::RustError(format!("{name} is not configured")))
    }

    pub async fn from_env(env: &Env) -> worker::Result<Self> {
        let endpoint = Self::require_config(env, "S3_ENDPOINT").await?;
        let bucket = Self::require_config(env, "S3_BUCKET").await?;
        let region = Self::require_config(env, "S3_REGION").await?;
        let prefix = Self::config_value(env, "S3_PREFIX").await.unwrap_or_default();
        let access_key_id = Self::secret_or_store(env, "S3_ACCESS_KEY_ID").await?;
        let secret_access_key = Self::secret_or_store(env, "S3_SECRET_ACCESS_KEY").await?;
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(worker::Error::RustError(
                "S3 credentials are not configured (S3_ACCESS_KEY_ID / S3_SECRET_ACCESS_KEY)".to_string(),
            ));
        }
        let credentials = CredentialsOwned {
            access_key_id,
            secret_access_key,
        };
        let location = S3Location::new(endpoint, bucket, prefix)
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let host = location
            .host()
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        Ok(Self {
            location,
            region,
            host,
            credentials: Arc::new(credentials),
        })
    }

    /// ダウンロード用の presigned URL を作る（クエリ認証）。
    ///
    /// 挿絵のような大きなオブジェクトは Worker で中継せず、クライアントに
    /// S3 から直接取らせる（native の Web UI が静的配信するのと同じ発想）。
    pub fn presign_get_url(&self, key: &ObjectKey, expires_secs: u64) -> String {
        let credentials = Credentials {
            access_key_id: &self.credentials.access_key_id,
            secret_access_key: &self.credentials.secret_access_key,
        };
        s3_sigv4::presign_get(
            &self.host,
            &self.location.object_path(key),
            &credentials,
            &self.region,
            expires_secs,
            chrono::Utc::now(),
        )
    }

    /// 署名済みリクエストを送る。`path` と `query` は署名対象と一致させる。
    async fn send(
        &self,
        method: Method,
        url: &str,
        path: &str,
        query: &str,
        extra_headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<worker::Response> {
        let payload_sha256 = s3_sigv4::payload_sha256(body.as_deref().unwrap_or(&[]));
        let amz_date = s3_sigv4::amz_date(Utc::now());
        let signed = s3_sigv4::sign(
            &RequestToSign {
                method: method.as_ref(),
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

        let headers = Headers::new();
        for (name, value) in extra_headers {
            headers
                .append(name, value)
                .map_err(|error| worker_error(format!("invalid header {name}: {error}")))?;
        }
        headers
            .append("x-amz-date", &signed.x_amz_date)
            .map_err(worker_error)?;
        headers
            .append("x-amz-content-sha256", &signed.x_amz_content_sha256)
            .map_err(worker_error)?;
        headers
            .append("authorization", &signed.authorization)
            .map_err(worker_error)?;

        let mut init = RequestInit::new();
        init.with_method(method);
        init.with_redirect(RequestRedirect::Manual);
        init.with_headers(headers);
        if let Some(body) = body {
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(body.as_slice()))));
        }
        let request = Request::new_with_init(url, &init)
            .map_err(|error| worker_error(format!("invalid S3 request: {error}")))?;
        Fetch::Request(request)
            .send()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))
    }

    async fn status_and_body(&self, mut response: worker::Response) -> Result<(u16, Vec<u8>)> {
        let status = response.status_code();
        let body = if status == 200 {
            let bytes = response
                .bytes()
                .await
                .map_err(|error| NarouError::Http(error.to_string()))?;
            if bytes.len() as u64 > MAX_OBJECT_BYTES {
                return Err(NarouError::Platform(format!(
                    "S3 object exceeds the {} byte limit",
                    MAX_OBJECT_BYTES
                )));
            }
            bytes
        } else {
            Vec::new()
        };
        Ok((status, body))
    }

    async fn stat_object(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        // `HEAD` は Workers のエッジで 403 になる S3 互換サービスがあるため、
        // `GET` + `Range: bytes=0-0` でメタデータだけ読む (本体は受け取らない)。
        let response = self
            .send(
                Method::Get,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[("range", "bytes=0-0")],
                None,
            )
            .await?;
        let status = response.status_code();
        if status == 404 {
            return Ok(None);
        }
        if !matches!(status, 200 | 206) {
            return Err(NarouError::Platform(format!(
                "S3 GET {} failed with status {status}",
                key.as_ref()
            )));
        }
        {
            {
                let headers = response.headers();
                let content_range = headers.get("content-range").ok().flatten();
                let content_length = headers.get("content-length").ok().flatten();
                let size = narou_rs::platform::object_size(
                    status,
                    content_range.as_deref(),
                    content_length.as_deref(),
                )
                .ok_or_else(|| {
                    NarouError::Platform(format!(
                        "S3 GET {} returned no size (content-range={content_range:?})",
                        key.as_ref()
                    ))
                })?;
                let etag = headers
                    .get("etag")
                    .ok()
                    .flatten()
                    .map(|value| value.trim_matches('"').to_string());
                let content_type = headers.get("content-type").ok().flatten();
                let last_modified = headers
                    .get("last-modified")
                    .ok()
                    .flatten()
                    .and_then(|value| chrono::DateTime::parse_from_rfc2822(&value).ok())
                    .map(|value| value.with_timezone(&Utc));
                Ok(Some(ObjectMetadata {
                    key: key.clone(),
                    size,
                    etag,
                    content_type,
                    last_modified,
                }))
            }
        }
    }

    async fn read_object(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        let response = self
            .send(
                Method::Get,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[],
                None,
            )
            .await?;
        match self.status_and_body(response).await? {
            (200, body) => Ok(Some(body)),
            (404, _) => Ok(None),
            (status, _) => Err(NarouError::Platform(format!(
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
                Method::Put,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[("content-type", content_type)],
                Some(data),
            )
            .await?;
        let status = response.status_code();
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
                Method::Delete,
                &self.location.object_url(key),
                &self.location.object_path(key),
                "",
                &[],
                None,
            )
            .await?;
        let status = response.status_code();
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
                Method::Get,
                &url,
                &self.location.bucket_path(),
                &query,
                &[],
                None,
            )
            .await?;
        let (status, body) = self.status_and_body(response).await?;
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
        let response = self
            .send(
                Method::Put,
                &self.location.object_url(destination),
                &self.location.object_path(destination),
                "",
                &[("x-amz-copy-source", &self.location.copy_source(source))],
                Some(Vec::new()),
            )
            .await?;
        let status = response.status_code();
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

impl ObjectStore for S3ObjectStore {
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

impl AssetStore for S3ObjectStore {
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
