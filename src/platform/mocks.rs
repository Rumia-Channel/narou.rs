//! In-memory mock implementations for tests.
//!
//! These let download/update/convert logic run without network, filesystem,
//! or real sleeping. They are compiled into the crate unconditionally (cheap,
//! no extra deps) so both unit tests and future integration tests can use them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::{
    HttpClient, HttpMethod, HttpRequest, HttpResponse, NovelFilter, NovelId, NovelMutation,
    NovelQuery, NovelRepository, ObjectKey, ObjectMetadata, ObjectStore, PlatformFuture,
    RateLimitScope, RateLimiter,
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
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, Result<HttpResponse>> {
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
///
/// Implements the full [`NovelRepository`] contract with no filesystem,
/// network, or sleeping — the basis for downloader/update integration tests.
#[derive(Debug, Default)]
pub struct MemoryNovelRepository {
    records: Mutex<BTreeMap<i64, NovelRecord>>,
    next_id: AtomicU64,
}

impl MemoryNovelRepository {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    pub fn from_records(records: Vec<NovelRecord>) -> Self {
        let repo = Self::new();
        let mut max_id: Option<i64> = None;
        for record in records {
            let id = record.id;
            max_id = Some(max_id.map_or(id, |current| current.max(id)));
            repo.records.lock().insert(id, record);
        }
        // Native semantics: next id is max + 1, or 0 for an empty database.
        let next = max_id.map(|m| m + 1).unwrap_or(0);
        repo.next_id.store(next as u64, Ordering::SeqCst);
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
    fn get<'a>(
        &'a self,
        id: NovelId,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(async move { Ok(self.records.lock().get(&id.0).cloned()) })
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let needle = url.trim().to_lowercase();
        Box::pin(async move {
            Ok(self
                .records
                .lock()
                .values()
                .find(|r| r.toc_url.trim().to_lowercase() == needle)
                .cloned())
        })
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let needle = title.trim().to_lowercase();
        Box::pin(async move {
            let records = self.records.lock();
            // Exact (index) lookup first, then case-insensitive fallback,
            // matching the native `Database::find_by_title` semantics.
            Ok(records
                .values()
                .find(|r| r.title.trim().to_lowercase() == needle)
                .cloned())
        })
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let ncode = ncode.to_lowercase();
        Box::pin(async move {
            Ok(self
                .records
                .lock()
                .values()
                .find(|r| {
                    r.ncode.as_deref().is_some_and(|nc| nc.eq_ignore_ascii_case(&ncode))
                        || r.toc_url
                            .to_lowercase()
                            .trim_end_matches('/')
                            .ends_with(&format!("/{ncode}"))
                })
                .cloned())
        })
    }

    fn count<'a>(
        &'a self,
        filter: &'a NovelFilter,
    ) -> PlatformFuture<'a, Result<u64>> {
        let filter = filter.clone();
        Box::pin(async move {
            Ok(self
                .records
                .lock()
                .values()
                .filter(|r| crate::platform::repository::record_matches_filter(r, &filter))
                .count() as u64)
        })
    }

    fn query<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        let query = query.clone();
        Box::pin(async move {
            let mut records: Vec<NovelRecord> = self
                .records
                .lock()
                .values()
                .filter(|r| crate::platform::repository::record_matches_filter(r, &query.filter))
                .cloned()
                .collect();
            records.sort_by(|a, b| {
                crate::platform::repository::compare_records_by_sort_key(a, b, query.sort.key)
                    .then_with(|| a.id.cmp(&b.id))
            });
            if query.sort.reverse {
                records.reverse();
            }
            let start = query.offset.min(records.len());
            let end = (start + query.limit).min(records.len());
            Ok(records[start..end].to_vec())
        })
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<NovelId>>> {
        let filter = filter.clone();
        let after_id = after_id.map(|id| id.0).unwrap_or(i64::MIN);
        Box::pin(async move {
            let ids: Vec<NovelId> = self
                .records
                .lock()
                .values()
                .filter(|r| {
                    r.id > after_id
                        && crate::platform::repository::record_matches_filter(r, &filter)
                })
                .take(limit)
                .map(|r| NovelId::from(r.id))
                .collect();
            Ok(ids)
        })
    }

    fn allocate_id(&self) -> PlatformFuture<'_, Result<NovelId>> {
        Box::pin(async move {
            // Atomic reservation: unique even under concurrent tasks.
            Ok(NovelId::from(self.next_id.fetch_add(1, Ordering::SeqCst) as i64))
        })
    }

    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut records = self.records.lock();
            for mutation in mutations {
                match mutation {
                    NovelMutation::Upsert(record) => {
                        let id = record.id;
                        records.insert(id, record);
                    }
                    NovelMutation::Remove(id) => {
                        records.remove(&id.0);
                    }
                }
            }
            Ok(())
        })
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
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.acquisitions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{NovelSort, NovelSortKey};
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

        futures::executor::block_on(repo.apply_batch(vec![
            NovelMutation::Upsert(record.clone()),
            NovelMutation::Upsert(record2.clone()),
        ]))
        .unwrap();
        assert_eq!(repo.len(), 2);

        let found = futures::executor::block_on(repo.get(NovelId::from(1)))
            .unwrap()
            .unwrap();
        assert_eq!(found.title, "テスト小説");

        let mut updated = record.clone();
        updated.title = "更新後タイトル".into();
        futures::executor::block_on(repo.apply_batch(vec![NovelMutation::Upsert(updated)]))
            .unwrap();
        assert_eq!(
            futures::executor::block_on(repo.get(NovelId::from(1)))
                .unwrap()
                .unwrap()
                .title,
            "更新後タイトル"
        );

        let query = NovelQuery::page(
            NovelFilter {
                keyword: Some("なろう".into()),
                ..Default::default()
            },
            NovelSort::by(NovelSortKey::Id),
            0,
            10,
        );
        let results = futures::executor::block_on(repo.query(&query)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);

        futures::executor::block_on(repo.apply_batch(vec![NovelMutation::Remove(
            NovelId::from(1),
        )]))
        .unwrap();
        assert_eq!(repo.len(), 1);
    }

    #[test]
    fn memory_repository_allocates_unique_ids() {
        let repo = MemoryNovelRepository::from_records(vec![sample_record(5)]);
        let ids: Vec<i64> = (0..10)
            .map(|_| {
                futures::executor::block_on(repo.allocate_id())
                    .unwrap()
                    .0
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 10, "allocated ids must be unique");
        assert!(ids.iter().all(|id| *id > 5));
    }

    #[test]
    fn memory_repository_scan_ids_uses_keyset_pagination() {
        let repo = MemoryNovelRepository::from_records(vec![
            sample_record(1),
            sample_record(2),
            sample_record(3),
            sample_record(4),
            sample_record(5),
        ]);
        let filter = NovelFilter::all();

        let page1 = futures::executor::block_on(repo.scan_ids(&filter, None, 2))
            .unwrap();
        assert_eq!(page1, vec![NovelId::from(1), NovelId::from(2)]);

        let page2 = futures::executor::block_on(repo.scan_ids(&filter, page1.last().copied(), 2))
            .unwrap();
        assert_eq!(page2, vec![NovelId::from(3), NovelId::from(4)]);

        let page3 = futures::executor::block_on(repo.scan_ids(&filter, page2.last().copied(), 2))
            .unwrap();
        assert_eq!(page3, vec![NovelId::from(5)]);
    }

    #[test]
    fn memory_repository_count_and_find_by_ncode() {
        let mut record = sample_record(1);
        record.ncode = Some("n1234ab".into());
        record.toc_url = "https://ncode.syosetu.com/n1234ab/".into();
        let repo = MemoryNovelRepository::from_records(vec![record]);

        let filter = NovelFilter {
            is_narou: Some(true),
            ..Default::default()
        };
        assert_eq!(futures::executor::block_on(repo.count(&filter)).unwrap(), 0);

        let found = futures::executor::block_on(repo.find_by_ncode("N1234AB"))
            .unwrap()
            .unwrap();
        assert_eq!(found.id, 1);
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
