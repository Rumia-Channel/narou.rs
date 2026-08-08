//! In-memory mock implementations for tests.
//!
//! These let download/update/convert logic run without network, filesystem,
//! or real sleeping. They are compiled into the crate unconditionally (cheap,
//! no extra deps) so both unit tests and future integration tests can use them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::{
    HttpClient, HttpMethod, HttpRequest, HttpResponse, NovelId, NovelQuery, NovelRepository,
    ObjectKey, ObjectMetadata, ObjectStore, RateLimitScope, RateLimiter,
};

/// HTTP client backed by a caller-provided responder closure.
///
/// Register canned responses by URL; any request without a canned response
/// fails with `NarouError::Http`. All requests are recorded so tests can
/// assert on what was fetched.
#[derive(Debug, Default)]
pub struct MockHttpClient {
    responses: Mutex<BTreeMap<String, HttpResponse>>,
    requests: Mutex<Vec<String>>,
}

impl MockHttpClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned response for an exact URL.
    pub fn add_response(&self, url: impl Into<String>, response: HttpResponse) {
        self.responses.lock().insert(url.into(), response);
    }

    /// Register a text response for an exact URL.
    pub fn add_text(&self, url: impl Into<String>, status: u16, body: impl Into<String>) {
        self.add_response(
            url,
            HttpResponse {
                status,
                headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                body: body.into().into_bytes(),
            },
        );
    }

    /// All URLs requested so far, in order.
    pub fn requested_urls(&self) -> Vec<String> {
        self.requests.lock().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().len()
    }
}

impl HttpClient for MockHttpClient {
    fn send<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            self.requests.lock().push(format!("{} {}", method_name(request.method), request.url));
            self.responses
                .lock()
                .get(&request.url)
                .cloned()
                .ok_or_else(|| {
                    NarouError::Http(format!("no canned response for {}", request.url))
                })
        })
    }
}

fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
    }
}

/// In-memory object store. Keys are plain strings.
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.objects.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ObjectStore for MemoryObjectStore {
    fn exists(&self, key: &ObjectKey) -> Result<bool> {
        Ok(self.objects.lock().contains_key(&key.0))
    }

    fn read(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        Ok(self.objects.lock().get(&key.0).cloned())
    }

    fn write(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        self.objects.lock().insert(key.0.clone(), data.to_vec());
        Ok(())
    }

    fn delete(&self, key: &ObjectKey) -> Result<()> {
        self.objects.lock().remove(&key.0);
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>> {
        Ok(self
            .objects
            .lock()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| ObjectMetadata {
                key: ObjectKey::new(k.clone()),
                size: v.len() as u64,
            })
            .collect())
    }
}

/// In-memory novel repository seeded from a map of records.
#[derive(Debug, Default)]
pub struct MemoryNovelRepository {
    records: Mutex<BTreeMap<i64, NovelRecord>>,
}

impl MemoryNovelRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(records: Vec<NovelRecord>) -> Self {
        let repo = Self::new();
        for record in records {
            let id = record.id;
            repo.records.lock().insert(id, record);
        }
        repo
    }

    pub fn len(&self) -> usize {
        self.records.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl NovelRepository for MemoryNovelRepository {
    fn get(&self, id: NovelId) -> Result<Option<NovelRecord>> {
        Ok(self.records.lock().get(&id.0).cloned())
    }

    fn find_by_toc_url(&self, url: &str) -> Result<Option<NovelRecord>> {
        Ok(self
            .records
            .lock()
            .values()
            .find(|r| r.toc_url == url)
            .cloned())
    }

    fn find_by_title(&self, title: &str) -> Result<Option<NovelRecord>> {
        Ok(self
            .records
            .lock()
            .values()
            .find(|r| r.title == title)
            .cloned())
    }

    fn insert(&self, record: &NovelRecord) -> Result<()> {
        self.records.lock().insert(record.id, record.clone());
        Ok(())
    }

    fn update(&self, record: &NovelRecord) -> Result<()> {
        self.records.lock().insert(record.id, record.clone());
        Ok(())
    }

    fn remove(&self, id: NovelId) -> Result<()> {
        self.records.lock().remove(&id.0);
        Ok(())
    }

    fn query(&self, query: &NovelQuery) -> Result<Vec<NovelRecord>> {
        let mut records: Vec<NovelRecord> = self
            .records
            .lock()
            .values()
            .filter(|r| {
                query
                    .keyword
                    .as_ref()
                    .map(|kw| r.title.contains(kw) || r.author.contains(kw))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if let Some(sort_by) = &query.sort_by {
            records.sort_by(|a, b| {
                crate::db::compare_records_by_key(a, b, sort_by)
            });
        }

        let start = query.offset.min(records.len());
        let end = (start + query.limit).min(records.len());
        let limited = if query.limit > 0 {
            records[start..end].to_vec()
        } else {
            records
        };
        Ok(limited)
    }
}

/// A rate limiter that never waits. Used by tests to run downloads without
/// sleeping; request counts can be observed if needed.
#[derive(Debug, Default)]
pub struct FakeRateLimiter {
    acquisitions: AtomicU64,
}

impl FakeRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn acquisition_count(&self) -> u64 {
        self.acquisitions.load(Ordering::SeqCst)
    }
}

impl RateLimiter for FakeRateLimiter {
    fn acquire<'a>(
        &'a self,
        _scope: &'a RateLimitScope,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.acquisitions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::clock::Clock;
    use crate::platform::SystemClock;

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: "作者".into(),
            title: "テスト小説".into(),
            file_title: "test".into(),
            toc_url: format!("https://example.com/novel/{id}"),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn mock_http_serves_canned_and_records() {
        let client = MockHttpClient::new();
        client.add_text("https://example.com/toc", 200, "<html>toc</html>");

        let body = futures::executor::block_on(async {
            let resp = client.send(HttpRequest::get("https://example.com/toc")).await.unwrap();
            String::from_utf8_lossy(&resp.body).into_owned()
        });
        assert_eq!(body, "<html>toc</html>");

        let err = futures::executor::block_on(async {
            client.send(HttpRequest::get("https://example.com/missing")).await.unwrap_err()
        });
        assert!(err.to_string().contains("no canned response"));

        let urls = client.requested_urls();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("GET https://example.com/toc"));
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryObjectStore::new();
        let key = ObjectKey::new("novel/1/toc.yaml");
        assert!(!store.exists(&key).unwrap());
        assert!(store.read(&key).unwrap().is_none());

        store.write(&key, b"data").unwrap();
        assert!(store.exists(&key).unwrap());
        assert_eq!(store.read(&key).unwrap().unwrap(), b"data");
        assert_eq!(store.len(), 1);

        let listed = store.list("novel/1/").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, key);
        assert_eq!(listed[0].size, 4);

        store.delete(&key).unwrap();
        assert!(!store.exists(&key).unwrap());
    }

    #[test]
    fn memory_repository_crud_and_query() {
        let repo = MemoryNovelRepository::new();
        let mut record = sample_record(1);
        record.author = "作者".into();
        record.title = "テスト小説".into();
        let mut record2 = sample_record(2);
        record2.author = "別作者".into();
        record2.title = "なろう作品".into();

        repo.insert(&record).unwrap();
        repo.insert(&record2).unwrap();
        assert_eq!(repo.len(), 2);

        let found = repo.get(NovelId::from(1)).unwrap().unwrap();
        assert_eq!(found.title, "テスト小説");

        record.title = "更新後タイトル".into();
        repo.update(&record).unwrap();
        assert_eq!(repo.get(NovelId::from(1)).unwrap().unwrap().title, "更新後タイトル");

        let query = NovelQuery {
            keyword: Some("なろう".into()),
            sort_by: Some("id".into()),
            offset: 0,
            limit: 10,
        };
        let results = repo.query(&query).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);

        repo.remove(NovelId::from(1)).unwrap();
        assert_eq!(repo.len(), 1);
    }

    #[test]
    fn fake_rate_limiter_counts_acquisitions() {
        let limiter = FakeRateLimiter::new();
        let scope = RateLimitScope::site("example.com");
        futures::executor::block_on(async {
            limiter.acquire(&scope).await.unwrap();
            limiter.acquire(&scope).await.unwrap();
        });
        assert_eq!(limiter.acquisition_count(), 2);
    }

    #[test]
    fn system_clock_is_a_clock() {
        let clock = SystemClock;
        let _ = clock.now_utc();
        let _ = clock.now_unix_secs();
    }
}
