use std::sync::Arc;

use narou_rs::error::Result;
use narou_rs::platform::s3_sigv4::{self, RequestToSign};
use narou_rs::platform::{
    HttpClient, HttpRequest, HttpResponse, ObjectKey, ObjectStore, PlatformFuture, S3Store,
    S3StoreConfig, SystemClock,
};

fn key(value: &str) -> ObjectKey {
    ObjectKey::try_new(value).unwrap()
}

fn url_target(value: &str) -> (String, String) {
    let url = url::Url::parse(value).unwrap();
    (
        url.path().to_string(),
        url.query().unwrap_or_default().to_string(),
    )
}

fn store(http: Arc<dyn HttpClient>) -> S3Store {
    S3Store::new(
        S3StoreConfig {
            endpoint: "https://s3.example.com".to_string(),
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

#[test]
fn stat_and_read_survive_range_header_changes_in_transit() {
    struct RangeChangingTransport(Option<&'static str>);

    impl HttpClient for RangeChangingTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let authorization = request.header("authorization").unwrap();
                let signed_names = authorization
                    .split("SignedHeaders=")
                    .nth(1)
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap();
                // Verify the signature against the headers reaching the origin,
                // after the cache has removed or changed the optional Range.
                let extra: Vec<_> = signed_names
                    .split(';')
                    .filter(|name| !matches!(*name, "host" | "x-amz-date" | "x-amz-content-sha256"))
                    .map(|name| {
                        let value = if name == "range" {
                            self.0
                        } else {
                            request.header(name)
                        };
                        (name, value.unwrap_or(""))
                    })
                    .collect();
                let (path, query) = url_target(&request.url);
                let expected = s3_sigv4::sign(
                    &RequestToSign {
                        method: "GET",
                        host: "s3.example.com",
                        path: &path,
                        query: &query,
                        headers: &extra,
                        payload_sha256: request.header("x-amz-content-sha256").unwrap(),
                        amz_date: request.header("x-amz-date").unwrap(),
                    },
                    &s3_sigv4::Credentials {
                        access_key_id: "AKIAEXAMPLE",
                        secret_access_key: "secret",
                    },
                    "ap-northeast-1",
                    "s3",
                );
                if expected.authorization != authorization {
                    return Ok(HttpResponse {
                        status: 403,
                        headers: Vec::new(),
                        body: b"<Error><Code>SignatureDoesNotMatch</Code></Error>".to_vec(),
                    });
                }
                let body = b"stored image";
                let range = request.header("range").and(self.0);
                let (status, headers, bytes) = match range {
                    Some("bytes=0-0") => (
                        206,
                        vec![("content-range".into(), "bytes 0-0/12".into())],
                        &body[..1],
                    ),
                    Some("bytes=0-1") => (
                        206,
                        vec![("content-range".into(), "bytes 0-1/12".into())],
                        &body[..2],
                    ),
                    _ => (
                        200,
                        vec![("content-length".into(), body.len().to_string())],
                        &body[..],
                    ),
                };
                Ok(HttpResponse {
                    status,
                    headers,
                    body: bytes.to_vec(),
                })
            })
        }

        fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, narou_rs::error::Result<narou_rs::platform::HttpStreamResponse>> {
            let url = request.url.clone();
            Box::pin(async move {
                let response = self.send(request).await?;
                Ok(narou_rs::platform::HttpStreamResponse {
                    status: response.status, url, headers: response.headers,
                    body: Box::pin(futures::stream::once(async { panic!("stat must not read the response body, including a full 200 response") })),
                })
            })
        }
    }

    let image = key("novels/小説家になろう/n9364bq 設定集/挿絵/i83860.jpg");
    futures::executor::block_on(async {
        for range in [None, Some("bytes=0-1"), Some("bytes=0-0")] {
            let store = store(Arc::new(RangeChangingTransport(range)));
            let metadata = ObjectStore::stat(&store, &image).await.unwrap().unwrap();
            assert_eq!(metadata.key, image);
            assert_eq!(metadata.size, 12);
            assert_eq!(
                ObjectStore::read_small(&store, &image)
                    .await
                    .unwrap()
                    .unwrap(),
                b"stored image"
            );
        }
    });
}

#[cfg(feature = "native-runtime")]
#[test]
fn illustration_migration_cli_preserves_bytes_and_dry_run_boundaries() {
    use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
    use axum::response::IntoResponse;
    use std::process::Command;
    use std::sync::Mutex;
    use std::collections::BTreeMap;

    fn decoded_path(value: &str) -> String {
        let mut input = value.bytes();
        let mut output = Vec::with_capacity(value.len());
        while let Some(byte) = input.next() {
            if byte == b'%' {
                let hex = [input.next().unwrap(), input.next().unwrap()];
                output.push(u8::from_str_radix(std::str::from_utf8(&hex).unwrap(), 16).unwrap());
            } else { output.push(byte); }
        }
        String::from_utf8(output).unwrap()
    }
    fn etag(bytes: &[u8]) -> String { format!("\"{:08x}\"", crc32fast::hash(bytes)) }

    struct Server {
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
            if let Some(thread) = self.thread.take() { let _ = thread.join(); }
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stored = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
    let server_stored = stored.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let _server = Server {
        stop: Some(stop),
        thread: Some(std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let app = axum::Router::new().fallback(axum::routing::any(
                    move |method: Method, uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
                        let stored = server_stored.clone();
                        async move {
                            let path = decoded_path(uri.path());
                            let query: BTreeMap<_, _> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect();
                            let mut stored = stored.lock().unwrap();
                            if method == Method::GET && path == "/bucket" {
                                let prefix = format!("/bucket/{}", query.get("prefix").map(String::as_str).unwrap_or(""));
                                let contents = stored.iter().filter(|(key, _)| key.starts_with(&prefix))
                                    .map(|(key, bytes)| format!("<Contents><Key>{}</Key><Size>{}</Size></Contents>", &key["/bucket/".len()..], bytes.len()))
                                    .collect::<String>();
                                return ([(header::CONTENT_TYPE, "application/xml")], format!(
                                    "<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>",
                                )).into_response();
                            }
                            let current = stored.get(&path);
                            if headers.get(header::IF_MATCH).is_some_and(|value| current.is_none_or(|bytes| value.as_bytes() != etag(bytes).as_bytes()))
                                || (headers.get(header::IF_NONE_MATCH).is_some_and(|value| value == "*") && current.is_some()) {
                                return StatusCode::PRECONDITION_FAILED.into_response();
                            }
                            if method == Method::PUT {
                                let bytes = if let Some(source) = headers.get("x-amz-copy-source") {
                                    let Some(bytes) = stored.get(&decoded_path(source.to_str().unwrap())).cloned() else {
                                        return StatusCode::NOT_FOUND.into_response();
                                    };
                                    bytes
                                } else { body.to_vec() };
                                let digest = etag(&bytes);
                                stored.insert(path, bytes);
                                if headers.contains_key("x-amz-copy-source") {
                                    return ([(header::CONTENT_TYPE, "application/xml")],
                                        format!("<CopyObjectResult><LastModified>2026-10-15T00:00:00Z</LastModified><ETag>{digest}</ETag></CopyObjectResult>")).into_response();
                                }
                                return ([(header::ETAG, digest)], "").into_response();
                            }
                            if method == Method::DELETE {
                                stored.remove(&path);
                                return StatusCode::NO_CONTENT.into_response();
                            }
                            if method == Method::GET {
                                let Some(bytes) = stored.get(&path) else { return StatusCode::NOT_FOUND.into_response(); };
                                if let Some(range) = headers.get(header::RANGE) {
                                    let (start, end) = range.to_str().unwrap().strip_prefix("bytes=").unwrap().split_once('-').unwrap();
                                    let start: usize = start.parse().unwrap();
                                    let end: usize = end.parse().unwrap();
                                    if start > end || end >= bytes.len() {
                                        return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{}", bytes.len()))]).into_response();
                                    }
                                    return (StatusCode::PARTIAL_CONTENT, [
                                        (header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", bytes.len())),
                                        (header::ETAG, etag(bytes)),
                                    ], bytes[start..=end].to_vec()).into_response();
                                }
                                return ([(header::ETAG, etag(bytes))], bytes.clone()).into_response();
                            }
                            StatusCode::METHOD_NOT_ALLOWED.into_response()
                        }
                    },
                ));
                axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app)
                    .with_graceful_shutdown(async { let _ = stopped.await; })
                    .await.unwrap();
            });
        })),
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join(".narou")).unwrap();
    std::fs::write(root.join(".narou/database.yaml"), "{}\n").unwrap();
    std::fs::write(root.join(".narou/local_setting.yaml"), format!(
        "s3.asset-backend: s3\ns3.endpoint: http://{address}\ns3.bucket: bucket\ns3.prefix: fixture\ns3.region: us-east-1\ns3.access-key-id: fixture\ns3.secret-access-key: fixture-secret\n",
    )).unwrap();
    let image = root.join("小説データ/site/title/挿絵/001.png");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    let expected = b"stored illustration bytes";
    std::fs::write(&image, expected).unwrap();
    let run = |arguments: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_narou_rs"))
            .current_dir(root)
            .env_clear()
            .envs(std::env::vars().filter(|(name, _)| [
                "PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP", "COMSPEC", "PATHEXT", "SYSTEMDRIVE",
            ].iter().any(|allowed| name.eq_ignore_ascii_case(allowed))))
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("NAROU_RS_LEGACY_YAML", "1")
            .arg("illust").args(arguments).output().unwrap();
        assert!(output.status.success(), "{arguments:?}: {}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    };
    run(&["s3-push"]);
    assert!(stored.lock().unwrap().is_empty(), "dry run must not publish an object");
    run(&["s3-push", "-f"]);
    let remote_image = "/bucket/fixture/novels/site/title/挿絵/001.png";
    assert_eq!(stored.lock().unwrap().get(remote_image).map(Vec::as_slice), Some(expected.as_slice()));
    run(&["s3-verify"]);
    run(&["s3-dedup"]);
    run(&["s3-dedup", "-f"]);
    assert_eq!(stored.lock().unwrap().get(remote_image).map(Vec::as_slice), Some(expected.as_slice()), "non-hash image names must not be deleted by dedup");
    assert_eq!(std::fs::read(image).unwrap(), expected);
}
