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
