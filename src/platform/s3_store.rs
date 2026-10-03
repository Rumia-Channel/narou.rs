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
    /// SQLite+S3 モードで、挿絵を `illustrations/<base64url(sha256)>.<ext>`
    /// のグローバルプールへ寄せて重複を除去する。
    pub illustration_dedup: bool,
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
    credential_sources: Option<(&'static str, &'static str)>,
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
        let mut location = S3Location::new(config.endpoint, config.bucket, config.prefix)?;
        if config.illustration_dedup {
            location = location.with_illustration_dedup();
        }
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
            credential_sources: None,
        })
    }

    #[cfg(feature = "native-runtime")]
    pub(crate) fn with_credential_sources(mut self, sources: (&'static str, &'static str)) -> Self {
        self.credential_sources = Some(sources);
        self
    }

    fn diagnostic_location(&self) -> String {
        // URL parsing drops userinfo/path/query. Never format the raw endpoint.
        let host = url::Url::parse(&format!("{}://{}", self.scheme, self.host))
            .ok()
            .and_then(|url| {
                let host = url.host_str()?;
                Some(match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_string(),
                })
            })
            .unwrap_or_else(|| "<redacted>".to_string());
        let region = diagnostic_region(&self.region);
        let (access, secret) = self
            .credential_sources
            .unwrap_or(("unspecified", "unspecified"));
        format!(
            " [narou-config: endpoint-host={host}; region={region}; \
             access-key-source={access}; secret-source={secret}]"
        )
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
        // `presign_get` は生パスを期待して自分で percent-encode する。
        // encode 済みの `object_path()` を渡すと `%` が再エンコードされて
        // 非 ASCII キーの URL が壊れるため、ここでは生キーから組み立てる。
        // endpoint にパス成分がある構成では、そのパスも署名対象に含める。
        let path = format!(
            "{}/{}/{}",
            self.location.endpoint_path(),
            self.location.bucket(),
            self.location.storage_key(key)
        );
        let url = s3_sigv4::presign_get(
            &self.host,
            &path,
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

    /// 署名済みリクエストを送る。
    ///
    /// 署名対象の path/query は `url` そのものから切り出す。呼び出し側が
    /// 別途 canonical 値を渡す形だと、endpoint にパス成分が混ざる構成や
    /// エンコード差で「送ったもの」と「署名したもの」が乖離する余地が残るため、
    /// ここで wire bytes と署名対象を構造的に一致させる。
    ///
    /// 戻り値の [`SignedHeaders`] は署名に使った canonical request を持つ。
    /// SignatureDoesNotMatch を切り分けるとき、サーバーが返した
    /// `StringToSign` の digest と比較するのに使う。
    async fn send(
        &self,
        method: HttpMethod,
        url: &str,
        extra_headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(super::http::HttpResponse, s3_sigv4::SignedHeaders)> {
        let (path, query) = url_target(url);
        let payload_sha256 = s3_sigv4::payload_sha256(body.as_deref().unwrap_or(&[]));
        let amz_date = s3_sigv4::amz_date(self.clock.now_utc());
        let signed = s3_sigv4::sign(
            &RequestToSign {
                method: method_name(method),
                host: &self.host,
                path: &path,
                query: &query,
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
            .with_header("x-amz-date", &signed.x_amz_date)
            .with_header("x-amz-content-sha256", &signed.x_amz_content_sha256)
            .with_header("authorization", &signed.authorization);
        Ok((self.http.send(request).await?, signed))
    }

    /// dedup プール (`illustrations/<b64>`) にだけオブジェクトがあるか。
    /// legacy `挿絵/` fallback は付けない (移行判定に使う)。
    async fn pool_stat(&self, key: &ObjectKey) -> Result<Option<u64>> {
        if !self.location.has_pool_alternate(key) {
            return Ok(None);
        }
        let (response, _) = self
            .send(
                HttpMethod::Get,
                &self.location.object_url(key),
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
        Ok(super::object_size(
            status,
            response.header("content-range"),
            response.header("content-length"),
        ))
    }

    /// dedup プールにそのキーが存在するか (移行判定用)。
    pub async fn pool_exists(&self, key: &ObjectKey) -> Result<bool> {
        Ok(self.pool_stat(key).await?.is_some())
    }

    async fn stat_object(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        // `HEAD` はエッジで 403 になる S3 互換サービスがあるため、
        // `GET` + `Range: bytes=0-0` でメタデータだけ読む。
        let (response, _) = self
            .send(
                HttpMethod::Get,
                &self.location.object_url(key),
                &[("range", "bytes=0-0")],
                None,
            )
            .await?;
        // dedup 移行期: pool に無くても旧 `挿絵/` 位置に存在する挿絵を拾う。
        let response = if response.status == 404 && self.location.has_pool_alternate(key) {
            self.send(
                HttpMethod::Get,
                &self.location.legacy_object_url(key),
                &[("range", "bytes=0-0")],
                None,
            )
            .await?
            .0
        } else {
            response
        };
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
        let (response, _) = self
            .send(
                HttpMethod::Get,
                &self.location.object_url(key),
                &[],
                None,
            )
            .await?;
        let (status, body) = status_and_body(response)?;
        match status {
            200 => Ok(Some(body)),
            404 if self.location.has_pool_alternate(key) => {
                // dedup 移行期: pool に無ければ旧 `挿絵/` 位置から読む。
                let (response, _) = self
                    .send(
                        HttpMethod::Get,
                        &self.location.legacy_object_url(key),
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
        let (response, _) = self
            .send(
                HttpMethod::Put,
                &self.location.object_url(key),
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
        let (response, _) = self
            .send(
                HttpMethod::Delete,
                // dedup プールは共有なので小説削除では消さない。
                // 消すのは移行前の旧 `挿絵/` 位置の物理オブジェクトだけ。
                &self.location.legacy_object_url(key),
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
        let url = self.location.list_url(
            request.prefix.as_ref(),
            request.limit.get(),
            request.cursor.as_deref(),
        );
        let (response, signed) = self.send(HttpMethod::Get, &url, &[], None).await?;
        let (status, body) = status_and_body(response)?;
        if status != 200 {
            // Response XML can contain credential identifiers and signatures.
            // Keep only allowlisted codes and categorical comparisons.
            let detail = String::from_utf8_lossy(&body);
            let code = super::s3_diagnostics::error_code(&detail);
            let hint = signature_mismatch_hint(&detail, &signed);
            let evidence = super::s3_diagnostics::signature_context(&detail, &signed);
            let location = self.diagnostic_location();
            return Err(NarouError::Platform(format!(
                "S3 LIST {} failed with status {status}: {code}{hint}{evidence}{location}",
                request.prefix.as_ref()
            )));
        }
        let page = parse_list_objects_v2(&String::from_utf8_lossy(&body))?;

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
        // dedup プール (`illustrations/<b64>`) は論理キーではなく S3 内部の
        // 物理名。論理キーとして出すと読み戻しが `挿絵` 判定に合わず
        // primary へ回るため、一覧では隠す。
        if rest.starts_with("illustrations/") {
            return None;
        }
        ObjectKey::try_new(rest).ok()
    }

    async fn copy_object(&self, source: &ObjectKey, destination: &ObjectKey) -> Result<()> {
        let copy_source = self.location.copy_source(source);
        let (response, _) = self
            .send(
                HttpMethod::Put,
                &self.location.object_url(destination),
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

// Keep an explicit diagnostic allowlist rather than reflecting an arbitrary
// configuration string. Other/custom regions still sign exactly as configured.
fn diagnostic_region(region: &str) -> &str {
    match region {
        "auto" | "default" | "us-east-1" | "us-east-2" | "us-west-1" | "us-west-2"
        | "us-gov-east-1" | "us-gov-west-1" | "ca-central-1" | "ca-west-1" | "sa-east-1"
        | "eu-central-1" | "eu-central-2" | "eu-west-1" | "eu-west-2" | "eu-west-3"
        | "eu-north-1" | "eu-south-1" | "eu-south-2" | "ap-east-1" | "ap-south-1"
        | "ap-south-2" | "ap-northeast-1" | "ap-northeast-2" | "ap-northeast-3"
        | "ap-southeast-1" | "ap-southeast-2" | "ap-southeast-3" | "ap-southeast-4"
        | "af-south-1" | "me-south-1" | "me-central-1" | "il-central-1" | "cn-north-1"
        | "cn-northwest-1" => region,
        _ => "<redacted>",
    }
}

/// URL 文字列から署名対象の `path` と `query` を切り出す。
///
/// `url` クレートで再パースすると percent-encoding が正規化されて署名対象と
/// wire のバイト列がずれる余地があるため、単純な文字列分割で済ませる。
/// `endpoint` 構築時に `?`/`#` を拒否しているので、ここで出る `?` は常に
/// クエリの開始。
fn url_target(url: &str) -> (String, String) {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    let target = match after_scheme.find('/') {
        Some(index) => &after_scheme[index..],
        None => "/",
    };
    match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
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

/// 応答のステータスとボディ。エラー応答でもボディは残す
/// (S3 の XML エラーが AccessDenied か SignatureDoesNotMatch かで原因が変わる)。
fn status_and_body(response: super::http::HttpResponse) -> Result<(u16, Vec<u8>)> {
    let status = response.status;
    if status != 200 {
        return Ok((status, response.body));
    }
    if response.body.len() as u64 > MAX_OBJECT_BYTES {
        return Err(NarouError::Platform(format!(
            "S3 object exceeds the {} byte limit",
            MAX_OBJECT_BYTES
        )));
    }
    Ok((status, response.body))
}

/// `SignatureDoesNotMatch` の切り分けヒント。
///
/// S3 のエラー応答に含まれる `<StringToSign>` の末尾行は、サーバー側が
/// 再構成した canonical request の SHA-256 digest。これと自分側の digest を
/// 比較して、canonical request 自体が違うのか (エンコード/ヘッダ差異)、
/// digest と StringToSign 全体の一致を分ける。資格情報の誤りとは断定しない。
fn signature_mismatch_hint(body: &str, signed: &s3_sigv4::SignedHeaders) -> String {
    if super::s3_diagnostics::error_code(body) != "SignatureDoesNotMatch" {
        return String::new();
    }
    let Some(server_to_sign) = super::s3_diagnostics::unique_text(body, "StringToSign") else {
        return String::new();
    };
    // Some S3-compatible services use literal \n / \r\n in the XML value.
    // Normalize delimiters before comparing, never echo an unparsed value as a digest.
    let server_to_sign = server_to_sign
        .replace("\\r\\n", "\n")
        .replace("\\n", "\n")
        .replace("&#13;", "\r")
        .replace("&#xD;", "\r")
        .replace("&#xd;", "\r")
        .replace("&#10;", "\n")
        .replace("&#xA;", "\n")
        .replace("&#xa;", "\n");
    let server_lines: Vec<&str> = server_to_sign.trim().lines().collect();
    let our_lines: Vec<&str> = signed.string_to_sign.trim().lines().collect();
    if server_lines.len() != 4 || our_lines.len() != 4 {
        return String::new();
    }
    let server_digest = server_lines[3];
    let our_digest = our_lines[3];
    let is_digest =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !is_digest(server_digest) || !is_digest(our_digest) {
        return String::new();
    }
    if !server_digest.eq_ignore_ascii_case(our_digest) {
        " [narou: canonical-request digest differs; compare canonical URI, query, and signed headers]".to_string()
    } else if server_lines != our_lines {
        " [narou: canonical-request digest matches, but string-to-sign differs; \
         check algorithm, timestamp, region/service scope, and digest representation]"
            .to_string()
    } else {
        " [narou: canonical-request digest matches and normalized string-to-sign matches; \
         use the server echo and runtime self-test results to distinguish the remaining causes]"
            .to_string()
    }
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
                illustration_dedup: false,
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
            "https://s3.example.com/bucket?encoding-type=url&list-type=2&max-keys=10&prefix=narou%2Ftest%2Fnovels%2Fsite%2Ftitle%2F%E6%8C%BF%E7%B5%B5",
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
    fn list_page_redacts_s3_error_body() {
        let http = Arc::new(MockHttpClient::new());
        http.add_response(
            "https://s3.example.com/bucket?encoding-type=url&list-type=2&max-keys=10&prefix=narou%2Ftest%2Fnovels%2Fsite%2Ftitle%2F%E6%8C%BF%E7%B5%B5",
            HttpResponse {
                status: 403,
                headers: vec![("content-type".to_string(), "application/xml".to_string())],
                body: br#"<?xml version="1.0"?><Error><Code>AccessDenied</Code><Message>PRIVATE_BODY_SENTINEL</Message><AWSAccessKeyId>PRIVATE_KEY_IDENTIFIER</AWSAccessKeyId><SignatureProvided>PRIVATE_SIGNATURE_SENTINEL</SignatureProvided></Error>"#
                    .to_vec(),
            },
        );
        let store = store(http);
        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site/title/挿絵").unwrap(),
            std::num::NonZeroUsize::new(10).unwrap(),
        );
        let error = match futures::executor::block_on(ObjectStore::list_page(&store, &request)) {
            Ok(_) => panic!("a 403 LIST response must fail"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("403"), "{message}");
        assert!(message.contains("AccessDenied"), "{message}");
        assert!(!message.contains("PRIVATE_"), "{message}");
        assert!(!message.contains("<Error>"), "{message}");
        assert!(
            message.contains("endpoint-host=s3.example.com"),
            "{message}"
        );
    }

    #[test]
    fn logical_key_hides_the_dedup_illustration_pool() {
        let http = Arc::new(MockHttpClient::new());
        let body = r#"<?xml version="1.0"?><ListBucketResult>
  <Contents><Key>narou/test/illustrations/EYcyfG0PCwsZszqyEaVJAjqppB81nG0Kgn172Z-NWZQ.png</Key><Size>10</Size></Contents>
  <Contents><Key>narou/test/novels/site/title/挿絵/0001.jpg</Key><Size>20</Size></Contents>
</ListBucketResult>"#;
        http.add_response(
            "https://s3.example.com/bucket?encoding-type=url&list-type=2&max-keys=10&prefix=narou%2Ftest%2F",
            HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/xml".to_string())],
                body: body.as_bytes().to_vec(),
            },
        );
        let store = store(http);
        let request = ObjectListRequest::new(
            ObjectPrefix::new("").unwrap(),
            std::num::NonZeroUsize::new(10).unwrap(),
        );
        let page = futures::executor::block_on(ObjectStore::list_page(&store, &request)).unwrap();
        let keys: Vec<&str> = page.objects.iter().map(|m| m.key.as_ref()).collect();
        // プール (`illustrations/`) は論理キーに現れず、通常キーだけが残る。
        assert!(!keys.iter().any(|k| k.starts_with("illustrations/")), "{keys:?}");
        assert!(keys.contains(&"novels/site/title/挿絵/0001.jpg"), "{keys:?}");
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
                illustration_dedup: false,
                access_key_id: "minio".to_string(),
                secret_access_key: "minio123".to_string(),
            },
            Arc::new(MockHttpClient::new()),
            Arc::new(SystemClock),
        )
        .unwrap();
        let url = local.presign_get_url(&key("novels/site/title/挿絵/0001.jpg"), 60);
        assert!(url.starts_with("http://127.0.0.1:9000/bucket/"), "{url}");
        // 非 ASCII キーは一度だけ encode される (`%25E3` は二重エンコードの兆候)。
        assert!(url.contains("%E6%8C%BF%E7%B5%B5"), "{url}");
        assert!(!url.contains("%25"), "{url}");
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
                illustration_dedup: false,
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

    #[test]
    fn list_failure_compares_real_signed_request_without_echoing_it() {
        struct FixedClock;
        impl Clock for FixedClock {
            fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
                chrono::DateTime::from_timestamp(1_770_000_000, 0).unwrap()
            }
        }
        let http = Arc::new(MockHttpClient::new());
        let mut store = store(http.clone());
        store.clock = Arc::new(FixedClock);
        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site/title/挿絵").unwrap(),
            std::num::NonZeroUsize::new(10).unwrap(),
        );
        let url = store.location.list_url(request.prefix.as_ref(), 10, None);
        http.add_text(&url, 403, "<Error><Code>AccessDenied</Code></Error>");
        let (_, signed) =
            futures::executor::block_on(store.send(HttpMethod::Get, &url, &[], None)).unwrap();
        let signature = signed.authorization.rsplit_once("Signature=").unwrap().1;
        let bytes = signed
            .string_to_sign
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        http.add_text(&url, 403, format!(
            "<Error><Code>SignatureDoesNotMatch</Code><Message>PRIVATE_BODY_SENTINEL</Message>\
             <AWSAccessKeyId>AKIAEXAMPLE</AWSAccessKeyId><SignatureProvided>{signature}</SignatureProvided>\
             <StringToSign>{}</StringToSign><StringToSignBytes>{bytes}</StringToSignBytes></Error>",
            signed.string_to_sign.replace('\n', "\\n"),
        ));
        let error = match futures::executor::block_on(store.list_objects(&request)) {
            Ok(_) => panic!("a 403 LIST response must fail"),
            Err(error) => error.to_string(),
        };
        for expected in [
            "normalized string-to-sign matches",
            "SignatureProvided=match",
            "AWSAccessKeyId=match",
            "StringToSignBytes=match",
            "self-test=pass",
        ] {
            assert!(error.contains(expected), "{error}");
        }
        for private in [
            "PRIVATE_BODY_SENTINEL",
            "AKIAEXAMPLE",
            signature,
            &signed.authorization,
            &bytes,
        ] {
            assert!(!error.contains(private));
        }
        assert!(!error.contains(signed.string_to_sign.rsplit_once('\n').unwrap().1));
    }

    #[test]
    fn diagnostic_location_redacts_userinfo_paths_and_nonstandard_regions() {
        let mut store = store(Arc::new(MockHttpClient::new()));
        store.host =
            "private-user:PRIVATE_PASSWORD@example.invalid:9443/private/path?token=PRIVATE_QUERY"
                .to_string();
        store.region = "PRIVATE_REGION_SENTINEL".to_string();
        store.credential_sources = Some(("environment", "local-setting"));
        let diagnostic = store.diagnostic_location();
        assert!(diagnostic.contains("endpoint-host=example.invalid:9443"));
        assert!(diagnostic.contains("region=<redacted>"));
        assert!(diagnostic.contains("access-key-source=environment; secret-source=local-setting"));
        assert!(!diagnostic.contains("PRIVATE"));
        assert!(!diagnostic.contains("private"));
        for region in ["ap-northeast-2", "us-east-1", "us-gov-west-1", "auto"] {
            assert_eq!(diagnostic_region(region), region);
        }
        for region in [
            "",
            "secret",
            "ap-PRIVATE-2",
            "ap-northeast-2\nSECRET",
            "us-secret-123",
            "us-secret-1",
        ] {
            assert_eq!(diagnostic_region(region), "<redacted>");
        }
    }

    #[test]
    fn signature_hint_reports_matching_digests() {
        let signed = s3_sigv4::sign(
            &s3_sigv4::RequestToSign {
                method: "GET",
                host: "s3.example.com",
                path: "/bucket",
                query: "list-type=2",
                headers: &[],
                payload_sha256: &s3_sigv4::payload_sha256(&[]),
                amz_date: "20261001T000000Z",
            },
            &s3_sigv4::Credentials {
                access_key_id: "AK",
                secret_access_key: "SK",
            },
            "us-east-1",
            "s3",
        );
        for server_value in [
            signed.string_to_sign.replace('\n', "\\n"),
            signed.string_to_sign.clone(),
            signed.string_to_sign.replace('\n', "\\r\\n"),
            signed.string_to_sign.replace('\n', "\r\n"),
            signed.string_to_sign.replace('\n', "&#10;"),
            signed.string_to_sign.replace('\n', "&#xA;"),
            signed.string_to_sign.replace('\n', "&#13;&#10;"),
        ] {
            let body = format!(
                "<Error><Code>SignatureDoesNotMatch</Code>\
                 <StringToSign>{server_value}</StringToSign></Error>",
            );
            let hint = signature_mismatch_hint(&body, &signed);
            assert!(hint.contains("digest matches"), "{hint}");
            assert!(hint.contains("string-to-sign matches"), "{hint}");
            assert!(!hint.contains("digest differs"), "{hint}");
        }
        let different_scope = signed
            .string_to_sign
            .replace("/us-east-1/", "/ap-northeast-2/");
        let body = format!(
            "<Error><Code>SignatureDoesNotMatch</Code>\
             <StringToSign>{different_scope}</StringToSign></Error>",
        );
        let hint = signature_mismatch_hint(&body, &signed);
        assert!(hint.contains("string-to-sign differs"), "{hint}");
        assert!(!hint.contains("string-to-sign matches"), "{hint}");

        let (prefix, digest) = signed.string_to_sign.rsplit_once('\n').unwrap();
        let uppercase_digest = format!("{prefix}\n{}", digest.to_ascii_uppercase());
        let body = format!(
            "<Error><Code>SignatureDoesNotMatch</Code>\
             <StringToSign>{uppercase_digest}</StringToSign></Error>",
        );
        let hint = signature_mismatch_hint(&body, &signed);
        assert!(hint.contains("digest matches"), "{hint}");
        assert!(hint.contains("string-to-sign differs"), "{hint}");

        for malformed in [
            "unparsed response",
            "AWS4-HMAC-SHA256\\nshort\\nscope\\nnot-a-digest",
        ] {
            let body = format!(
                "<Error><Code>SignatureDoesNotMatch</Code>\
                 <StringToSign>{malformed}</StringToSign></Error>",
            );
            assert!(signature_mismatch_hint(&body, &signed).is_empty());
        }
    }

    #[test]
    fn signature_hint_reports_different_digests() {
        let signed = s3_sigv4::sign(
            &s3_sigv4::RequestToSign {
                method: "GET",
                host: "s3.example.com",
                path: "/bucket",
                query: "list-type=2",
                headers: &[],
                payload_sha256: &s3_sigv4::payload_sha256(&[]),
                amz_date: "20261001T000000Z",
            },
            &s3_sigv4::Credentials {
                access_key_id: "AK",
                secret_access_key: "SK",
            },
            "us-east-1",
            "s3",
        );
        let body = "<Error><Code>SignatureDoesNotMatch</Code>\
            <StringToSign>AWS4-HMAC-SHA256\n20261001T000000Z\nscope\n\
            0000000000000000000000000000000000000000000000000000000000000000\
            </StringToSign></Error>";
        let hint = signature_mismatch_hint(body, &signed);
        assert!(hint.contains("digest differs"), "{hint}");
    }

    #[test]
    fn signature_hint_is_silent_for_other_errors() {
        let signed = s3_sigv4::sign(
            &s3_sigv4::RequestToSign {
                method: "GET",
                host: "s3.example.com",
                path: "/bucket",
                query: "",
                headers: &[],
                payload_sha256: &s3_sigv4::payload_sha256(&[]),
                amz_date: "20261001T000000Z",
            },
            &s3_sigv4::Credentials {
                access_key_id: "AK",
                secret_access_key: "SK",
            },
            "us-east-1",
            "s3",
        );
        let body = "<Error><Code>AccessDenied</Code></Error>";
        assert!(signature_mismatch_hint(body, &signed).is_empty());
    }
}
