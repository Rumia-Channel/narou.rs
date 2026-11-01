#![cfg(feature = "native-runtime")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;

use narou_rs::error::{NarouError, Result};
use narou_rs::native::http::NativeHttpClient;
use narou_rs::platform::http::HttpPacing;
use narou_rs::platform::{HttpClient, HttpRequest, PlatformFuture, RateLimitScope, RateLimiter};

struct AdmissionGate {
    permits: tokio::sync::Semaphore,
    waiting: tokio::sync::mpsc::UnboundedSender<()>,
}

impl RateLimiter for AdmissionGate {
    fn acquire<'a>(&'a self, _scope: &'a RateLimitScope) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.waiting.send(()).map_err(|error| NarouError::Platform(error.to_string()))?;
            let permit = self.permits.acquire().await
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            permit.forget();
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_cannot_send_its_next_request_using_the_previous_permit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(AtomicUsize::new(0));
    let seen = received.clone();
    let server = std::thread::spawn(move || {
        for hop in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0 && request.len() + read <= 8192);
                request.extend_from_slice(&chunk[..read]);
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let response: &[u8] = if hop == 0 {
                b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nfinished"
            };
            stream.write_all(response).unwrap();
        }
    });
    let client = Arc::new(NativeHttpClient::new("narou-pacing-test").unwrap());
    let (waiting, mut attempts) = tokio::sync::mpsc::unbounded_channel();
    let limiter = Arc::new(AdmissionGate { permits: tokio::sync::Semaphore::new(1), waiting });
    let gate = limiter.clone();
    let task = tokio::spawn(async move {
        let mut request = HttpRequest::get(format!("http://127.0.0.1:{port}/start"));
        request.trusted_endpoint = true;
        let scope = RateLimitScope::site("127.0.0.1");
        client.send_paced(request, HttpPacing { limiter: gate.as_ref(), scope: &scope }).await
    });
    tokio::time::timeout(Duration::from_secs(5), attempts.recv()).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), attempts.recv()).await.unwrap().unwrap();
    assert_eq!(received.load(Ordering::SeqCst), 1, "redirected I/O must remain behind admission");
    limiter.permits.add_permits(1);
    let response = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
    assert_eq!(response.body, b"finished");
    assert_eq!(received.load(Ordering::SeqCst), 2);
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_redirect_body_is_followed_by_a_full_download_interval() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        first.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut request = [0u8; 8192];
        assert!(first.read(&mut request).unwrap() > 0);
        first.write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").unwrap();
        first.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        first.write_all(b"slow").unwrap();
        first.flush().unwrap();
        let completed = std::time::Instant::now();
        let (mut second, _) = listener.accept().unwrap();
        let gap = completed.elapsed();
        second.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert!(second.read(&mut request).unwrap() > 0);
        second.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nfinished").unwrap();
        gap
    });
    let client = NativeHttpClient::new("narou-completion-pacing-test").unwrap();
    let limiter = narou_rs::downloader::rate_limit::RateLimiter::with_settings(0.2, 0);
    let scope = RateLimitScope::site("127.0.0.1");
    let mut request = HttpRequest::get(format!("http://127.0.0.1:{port}/slow"));
    request.trusted_endpoint = true;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.send_paced(request, HttpPacing { limiter: &limiter, scope: &scope }),
    ).await.unwrap().unwrap();
    assert_eq!(response.body, b"finished");
    let gap = server.join().unwrap();
    assert!(gap >= Duration::from_millis(195), "completion-to-next-start gap was {gap:?}");
    println!("response completion -> redirect: {gap:?} (configured 200 ms)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_body_read_is_followed_by_a_full_download_interval() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        first.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut request = [0u8; 8192];
        assert!(first.read(&mut request).unwrap() > 0);
        first.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n").unwrap();
        first.flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        first.write_all(b"half").unwrap();
        drop(first);
        let completed = std::time::Instant::now();
        let (mut second, _) = listener.accept().unwrap();
        let gap = completed.elapsed();
        second.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert!(second.read(&mut request).unwrap() > 0);
        second.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nfinished").unwrap();
        gap
    });
    let client = NativeHttpClient::new("narou-error-completion-pacing-test").unwrap();
    let limiter = narou_rs::downloader::rate_limit::RateLimiter::with_settings(0.2, 0);
    let scope = RateLimitScope::site(format!("error-completion-{port}.example"));
    for index in 0..2 {
        let mut request = HttpRequest::get(format!("http://127.0.0.1:{port}/{index}"));
        request.trusted_endpoint = true;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            client.send_paced(request, HttpPacing { limiter: &limiter, scope: &scope }),
        ).await.unwrap();
        if index == 0 {
            assert!(result.is_err(), "a truncated body must fail");
        } else {
            assert_eq!(result.unwrap().body, b"finished");
        }
    }
    let gap = server.join().unwrap();
    assert!(gap >= Duration::from_millis(195), "error-to-next-start gap was {gap:?}");
    println!("body failure -> next request: {gap:?} (configured 200 ms)");
}
