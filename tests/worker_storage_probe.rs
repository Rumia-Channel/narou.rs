//! Run the Worker's actual storage-probe response logic without a Worker host.

#[path = "../worker_entry/src/storage_probe.rs"]
mod storage_probe;

use std::sync::Mutex;

use futures::executor::block_on;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectStore, PlatformFuture,
};
use serde_json::{Value, json};

struct ProbeStore {
    requests: Mutex<Vec<ObjectListRequest>>,
    result: Mutex<Option<Result<ObjectListPage>>>,
}

impl ProbeStore {
    fn new(result: Result<ObjectListPage>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            result: Mutex::new(Some(result)),
        }
    }

    fn assert_one_root_list(&self) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the probe must perform exactly one LIST");
        assert_eq!(requests[0].prefix.as_ref(), "");
        assert_eq!(requests[0].limit.get(), 1);
        assert_eq!(requests[0].cursor, None);
    }
}

impl ObjectStore for ProbeStore {
    fn stat<'a>(&'a self, _: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        panic!("the probe must not inspect individual objects")
    }

    fn read_small<'a>(&'a self, _: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        panic!("the probe must not read object contents")
    }

    fn write_small<'a>(&'a self, _: &'a ObjectKey, _: Vec<u8>) -> PlatformFuture<'a, Result<()>> {
        panic!("the probe must not write objects")
    }

    fn delete<'a>(&'a self, _: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        panic!("the probe must not delete objects")
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        self.requests.lock().unwrap().push(request.clone());
        let result = self
            .result
            .lock()
            .unwrap()
            .take()
            .expect("unexpected second LIST");
        Box::pin(async move { result })
    }
}

fn empty_page() -> ObjectListPage {
    ObjectListPage {
        objects: Vec::new(),
        next_cursor: None,
    }
}

fn expected_mode(backend: &str, required: bool) -> Value {
    json!({
        "success": true,
        "mode": "sqlite",
        "locked_by_env": true,
        "metadata_backend": "d1",
        "illustration_backend": backend,
        "s3_required": required,
    })
}

#[test]
fn reporting_selected_s3_without_a_probe_does_not_contact_the_store() {
    let store = ProbeStore::new(Err(NarouError::Platform("must not be queried".into())));
    let response = block_on(storage_probe::storage_mode_response(
        Some(&store),
        true,
        false,
    ));

    assert_eq!(response, (200, expected_mode("s3", true)));
    assert!(store.requests.lock().unwrap().is_empty());
}

#[test]
fn reporting_d1_without_a_probe_succeeds() {
    let response = block_on(storage_probe::storage_mode_response(None, false, false));

    assert_eq!(response, (200, expected_mode("d1", false)));
}

#[test]
fn selected_s3_with_an_empty_bucket_passes_one_bounded_root_list() {
    let store = ProbeStore::new(Ok(empty_page()));
    let response = block_on(storage_probe::storage_mode_response(
        Some(&store),
        true,
        true,
    ));
    let mut expected = expected_mode("s3", true);
    expected["s3_list"] = "ok".into();

    assert_eq!(response, (200, expected));
    store.assert_one_root_list();
}

#[test]
fn selected_s3_does_not_return_metadata_or_follow_the_next_page() {
    let store = ProbeStore::new(Ok(ObjectListPage {
        objects: vec![ObjectMetadata {
            key: ObjectKey::try_new("private-library/secret-object.jpg").unwrap(),
            size: 1234,
            etag: Some("private-etag".into()),
            content_type: Some("private-content-type".into()),
            last_modified: None,
        }],
        next_cursor: Some("private-next-page-token".into()),
    }));
    let response = block_on(storage_probe::storage_mode_response(
        Some(&store),
        false,
        true,
    ));
    let mut expected = expected_mode("s3", false);
    expected["s3_list"] = "ok".into();

    assert_eq!(
        response,
        (200, expected),
        "only the fixed probe status may be returned"
    );
    store.assert_one_root_list();
}

#[test]
fn s3_errors_are_sanitized_and_do_not_retry_or_fall_back() {
    let provider_error = concat!(
        "https://private-endpoint.invalid/private-bucket/private-object?credential=fake-secret ",
        "<Error><Code>AccessDenied</Code><Message>private-provider-message</Message>",
        "<AWSAccessKeyId>fake-access-key</AWSAccessKeyId>",
        "<SignatureProvided>fake-signature</SignatureProvided></Error>"
    );
    let store = ProbeStore::new(Err(NarouError::Platform(provider_error.into())));
    let response = block_on(storage_probe::storage_mode_response(
        Some(&store),
        true,
        true,
    ));
    let mut expected = expected_mode("s3", true);
    expected["success"] = false.into();
    expected["s3_list"] = "failed".into();

    assert_eq!(
        response,
        (503, expected),
        "provider data must not enter the response"
    );
    store.assert_one_root_list();
}

#[test]
fn a_probe_without_selected_s3_reports_not_selected() {
    let response = block_on(storage_probe::storage_mode_response(None, false, true));
    let mut expected = expected_mode("d1", false);
    expected["success"] = false.into();
    expected["s3_list"] = "not_selected".into();

    assert_eq!(response, (503, expected));
}
