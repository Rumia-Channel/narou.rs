//! Storage-mode response shared by the Worker route and native contract tests.

use std::num::NonZeroUsize;

use narou_rs::platform::{ObjectListRequest, ObjectPrefix, ObjectStore};
use serde_json::{Value, json};

/// Probe only the selected S3 store, and disclose only a fixed result label.
/// Neither object metadata nor provider errors may enter the response.
pub async fn storage_mode_response(
    s3: Option<&dyn ObjectStore>,
    s3_required: bool,
    probe_s3: bool,
) -> (u16, Value) {
    let mut payload = json!({
        "success": true,
        "mode": "sqlite",
        "locked_by_env": true,
        "metadata_backend": "d1",
        "illustration_backend": if s3.is_some() { "s3" } else { "d1" },
        "s3_required": s3_required,
    });
    let mut status = 200;
    if probe_s3 {
        let result = match s3 {
            Some(store) => {
                let request = ObjectListRequest::new(
                    ObjectPrefix::new("").expect("empty prefix is valid"),
                    NonZeroUsize::new(1).expect("one is nonzero"),
                );
                if store.list_page(&request).await.is_ok() {
                    "ok"
                } else {
                    "failed"
                }
            }
            None => "not_selected",
        };
        payload["s3_list"] = result.into();
        if result != "ok" {
            payload["success"] = false.into();
            status = 503;
        }
    }
    (status, payload)
}
