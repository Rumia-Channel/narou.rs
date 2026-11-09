use super::*;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use parking_lot::Mutex;
use crate::platform::http::HttpResponse;

#[derive(Default)]
struct Transport {
    uploaded: AtomicU64,
    parts: Mutex<Vec<usize>>,
    journal: Mutex<std::collections::BTreeMap<String, (Vec<u8>, u64)>>,
    hold_part: AtomicBool,
    ignore_range: AtomicBool,
    body_polls: Arc<AtomicUsize>,
}

impl HttpClient for Transport {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let mut response = HttpResponse { status: 200, headers: Vec::new(), body: Vec::new() };
            if request.url.ends_with(".narou-stage-scan-cursor") {
                if request.method == HttpMethod::Get { response.status = 404; }
                return Ok(response);
            }
            if request.url.contains("list-type=2") {
                response.body = b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".to_vec();
                return Ok(response);
            }
            if request.url.contains(JOURNAL_PREFIX) {
                let mut journal = self.journal.lock();
                match request.method {
                    HttpMethod::Get => match journal.get(&request.url) {
                        Some((bytes, version)) => {
                            response.body = bytes.clone();
                            response.headers.push(("etag".into(), format!("\"{version}\"")));
                        }
                        None => response.status = 404,
                    },
                    HttpMethod::Put => {
                        let current = journal.get(&request.url).map(|(_, version)| format!("\"{version}\""));
                        if request.header("if-none-match") == Some("*") && current.is_some()
                            || request.header("if-match").is_some_and(|expected| current.as_deref() != Some(expected)) {
                            response.status = 412;
                        } else {
                            let version = journal.get(&request.url).map_or(1, |(_, version)| version + 1);
                            journal.insert(request.url.clone(), (request.body.as_deref().unwrap().clone(), version));
                        }
                    }
                    HttpMethod::Delete => { journal.remove(&request.url); }
                    _ => unreachable!(),
                }
                return Ok(response);
            }
            if request.url.contains("partNumber=") {
                let length = request.body.as_ref().unwrap().len();
                self.parts.lock().push(length);
                if self.hold_part.load(Ordering::SeqCst) { futures::future::pending::<()>().await; }
                self.uploaded.fetch_add(length as u64, Ordering::SeqCst);
                response.headers.push(("etag".into(), "\"part\"".into()));
            } else if request.method == HttpMethod::Post && request.url.contains("uploads=") {
                response.body = b"<InitiateMultipartUploadResult><UploadId>test-upload</UploadId></InitiateMultipartUploadResult>".to_vec();
            } else if request.method == HttpMethod::Post && request.url.contains("uploadId=") {
                response.body = b"<CompleteMultipartUploadResult><ETag>complete</ETag></CompleteMultipartUploadResult>".to_vec();
            } else if request.method == HttpMethod::Get && request.url.contains("uploads=") {
                response.body = b"<ListMultipartUploadsResult><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>".to_vec();
            } else if request.method == HttpMethod::Put {
                self.uploaded.store(request.body.as_ref().map_or(0, |body| body.len()) as u64, Ordering::SeqCst);
            }
            Ok(response)
        })
    }

    fn send_stream<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpStreamResponse>> {
        Box::pin(async move {
            let status = if self.ignore_range.load(Ordering::SeqCst) { 200 } else { 206 };
            let total = self.uploaded.load(Ordering::SeqCst).max(1);
            let polls = self.body_polls.clone();
            Ok(HttpStreamResponse { status, url: request.url, headers: vec![
                ("content-range".into(), format!("bytes 0-0/{total}")),
                ("content-length".into(), "1".into()),
            ], body: Box::pin(futures::stream::once(async move {
                polls.fetch_add(1, Ordering::SeqCst); Ok(vec![1])
            })) })
        })
    }
}

fn store(http: Arc<Transport>) -> S3Store {
    S3Store::new(S3StoreConfig { endpoint: "https://s3.example.com".into(), bucket: "bucket".into(),
        region: "test".into(), prefix: String::new(), illustration_dedup: false,
        access_key_id: "test".into(), secret_access_key: "test".into(),
    }, http, Arc::new(crate::platform::SystemClock)).unwrap()
}

#[test]
fn ignored_range_is_rejected_without_reading_any_body() {
    futures::executor::block_on(async {
        let transport = Arc::new(Transport::default());
        transport.ignore_range.store(true, Ordering::SeqCst);
        let store = store(transport.clone());
        assert!(store.read_range(&ObjectKey::try_new("test.bin").unwrap(), 0..1).await.is_err());
        assert_eq!(transport.body_polls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn slow_upload_stops_pulling_at_one_part_and_cancellation_keeps_journal() {
    futures::executor::block_on(async {
        let transport = Arc::new(Transport::default());
        transport.hold_part.store(true, Ordering::SeqCst);
        let store = store(transport.clone());
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let stream: AssetStream = Box::pin(futures::stream::unfold((), move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Some((Ok(vec![0; 64 * 1024]), ())) }
        }));
        let mut upload = store.stage_stream(stream);
        assert!(futures::poll!(upload.as_mut()).is_pending());
        assert_eq!(reads.load(Ordering::SeqCst), PART_BYTES / (64 * 1024));
        assert_eq!(*transport.parts.lock(), vec![PART_BYTES]);
        drop(upload);
        assert_eq!(transport.journal.lock().len(), 1, "cancelled upload must remain recoverable");
    });
}

#[test]
fn stage_exceeds_old_sixty_four_megabyte_limit_without_collecting_archive() {
    futures::executor::block_on(async {
        let transport = Arc::new(Transport::default());
        let store = store(transport.clone());
        let stream: AssetStream = Box::pin(futures::stream::iter((0..1050).map(|index| Ok(vec![index as u8; 64 * 1024]))));
        let stage = store.stage_stream(stream).await.unwrap();
        assert_eq!(stage.size, 1050 * 64 * 1024);
        assert_eq!(transport.uploaded.load(Ordering::SeqCst), stage.size);
        assert!(transport.parts.lock().iter().all(|&length| length <= PART_BYTES));
        assert_eq!(transport.journal.lock().len(), 1, "successful private stage needs expiry until publication");
        assert_eq!(stage.sha256.len(), 64);
        AssetStore::delete(&store, &stage.key).await.unwrap();
        assert!(transport.journal.lock().is_empty());
    });
}

#[test]
fn progress_survives_creation_ttl_and_claimed_or_deleted_leases_never_revive() {
    struct TestClock(AtomicI64);
    impl Clock for TestClock {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
        }
    }
    futures::executor::block_on(async {
        let transport = Arc::new(Transport::default());
        let clock = Arc::new(TestClock(AtomicI64::new(1_800_000_000)));
        let mut store = store(transport.clone());
        store.clock = clock.clone();
        let stage = store.stage_stream(Box::pin(futures::stream::once(async { Ok(vec![1; 1024]) }))).await.unwrap();
        for _ in 0..4 {
            clock.0.fetch_add(STAGE_TTL_SECONDS / 2, Ordering::SeqCst);
            assert_eq!(store.read_range(&stage.key, 0..1).await.unwrap().unwrap().len(), 1);
            assert!(!store.claim_stage(&stage.key, true).await.unwrap());
        }
        clock.0.fetch_add(STAGE_TTL_SECONDS + 1, Ordering::SeqCst);
        assert!(store.claim_stage(&stage.key, true).await.unwrap());
        assert!(store.renew_stage_progress(&stage.key).await.is_err());
        store.delete_stage_record(&stage.key).await.unwrap();
        assert!(store.renew_stage_progress(&stage.key).await.is_err());
        assert!(transport.journal.lock().is_empty());
    });
}
