// Exercise the production policy on the host: the Worker crate itself is
// wasm-only, and a native `cargo test -p narou_worker` skips its modules.
#[path = "../worker_entry/src/asset_backend.rs"]
mod asset_backend;

use asset_backend::{AssetBackend, AssetBackendRow, BackendError, required_s3};

fn parse_asset_backend(value: Result<Option<&str>, ()>) -> Result<AssetBackend, BackendError> {
    asset_backend::parse_asset_backend(value.map(|value| {
        value.map(|value| AssetBackendRow {
            value_json: Some(value.to_string()),
        })
    }))
}

#[test]
fn persisted_json_text_is_decoded_before_backend_selection() {
    // D1 first::<Value>(Some("value_json")) deserializes the JS string cell;
    // it does not parse the JSON inside that string. Reproduce that boundary.
    let cell = serde_json::Value::String(serde_json::to_string("s3").unwrap());
    assert_ne!(cell.as_str(), Some("s3"));
    assert_eq!(parse_asset_backend(Ok(cell.as_str())), Ok(AssetBackend::S3));
    assert_eq!(
        parse_asset_backend(Ok(Some(r#""d1""#))),
        Ok(AssetBackend::D1)
    );
    assert_eq!(
        parse_asset_backend(Ok(Some(" \n\"s3\"\t"))),
        Ok(AssetBackend::S3)
    );
}

#[test]
fn only_a_missing_row_keeps_the_legacy_d1_default() {
    assert_eq!(parse_asset_backend(Ok(None)), Ok(AssetBackend::D1));
    assert_eq!(parse_asset_backend(Err(())), Err(BackendError::ReadFailed));
}

#[test]
fn sql_null_is_a_present_invalid_row_not_the_legacy_default() {
    let row: AssetBackendRow =
        serde_json::from_value(serde_json::json!({"value_json": null})).unwrap();
    assert_eq!(
        asset_backend::parse_asset_backend(Ok(Some(row))),
        Err(BackendError::InvalidJsonString)
    );
    assert!(
        serde_json::from_value::<AssetBackendRow>(serde_json::json!({"value_json": 1})).is_err()
    );
}

#[test]
fn exact_legacy_raw_markers_preserve_existing_backend_selection() {
    assert_eq!(parse_asset_backend(Ok(Some("d1"))), Ok(AssetBackend::D1));
    assert_eq!(parse_asset_backend(Ok(Some("s3"))), Ok(AssetBackend::S3));
}

#[test]
fn malformed_and_non_string_json_never_fall_back_to_d1() {
    for cell in [
        "",
        " s3",
        "d1 ",
        "S3",
        "r2",
        "null",
        "true",
        "3",
        "{}",
        "[]",
        "\"s3",
        "\"s3\" trailing",
    ] {
        assert_eq!(
            parse_asset_backend(Ok(Some(cell))),
            Err(BackendError::InvalidJsonString),
            "unexpected backend for {cell:?}"
        );
    }
}

#[test]
fn unknown_json_strings_never_fall_back_to_d1() {
    for cell in [r#""""#, r#""S3""#, r#""s3 ""#, r#""r2""#, r#""\"s3\"""#] {
        assert_eq!(
            parse_asset_backend(Ok(Some(cell))),
            Err(BackendError::UnknownBackend),
            "unexpected backend for {cell:?}"
        );
    }
}

#[test]
fn s3_requirement_selects_s3_for_valid_d1_and_missing_markers() {
    for cell in [None, Some("d1"), Some(r#""d1""#)] {
        let selected = parse_asset_backend(Ok(cell)).unwrap();
        assert_eq!(selected, AssetBackend::D1);
        assert_eq!(selected.resolve(true), AssetBackend::S3);
        assert_eq!(selected.resolve(false), AssetBackend::D1);
    }
    for cell in ["s3", r#""s3""#] {
        let selected = parse_asset_backend(Ok(Some(cell))).unwrap();
        assert_eq!(selected.resolve(true), AssetBackend::S3);
        assert_eq!(selected.resolve(false), AssetBackend::S3);
    }
}

#[test]
fn s3_requirement_does_not_mask_invalid_markers_or_read_failures() {
    for value in [
        Err(()),
        Ok(Some("null")),
        Ok(Some(r#""unknown""#)),
        Ok(Some("broken-json")),
    ] {
        let parsed = parse_asset_backend(value);
        assert!(parsed.is_err());
        assert_eq!(parsed.map(|backend| backend.resolve(true)), parsed);
    }
    let parsed = asset_backend::parse_asset_backend(Ok(Some(AssetBackendRow { value_json: None })));
    assert_eq!(
        parsed.map(|backend| backend.resolve(true)),
        Err(BackendError::InvalidJsonString)
    );
}

#[test]
fn requirement_flag_is_strict_and_defaults_only_when_absent() {
    assert_eq!(required_s3(None), Ok(false));
    assert_eq!(required_s3(Some("false")), Ok(false));
    assert_eq!(required_s3(Some("true")), Ok(true));
    for value in ["", "TRUE", "1", "yes", " true", "true ", "\"true\""] {
        assert_eq!(
            required_s3(Some(value)),
            Err(BackendError::InvalidRequirement)
        );
    }
}

#[test]
fn error_messages_do_not_echo_configuration_contents() {
    let arbitrary_value = "private-value-must-not-be-echoed";
    let json = serde_json::to_string(arbitrary_value).unwrap();
    let error = parse_asset_backend(Ok(Some(&json))).unwrap_err();
    assert!(!error.to_string().contains(arbitrary_value));
    let error = required_s3(Some(arbitrary_value)).unwrap_err();
    assert!(!error.to_string().contains(arbitrary_value));
}
