//! Request parameters shared by the native and Worker transfer endpoints.
//!
//! `GET /api/transfer/export?ids=1,2` and `POST /api/transfer/import`
//! exist in both front ends. The parsing rules and the response shape are
//! part of the API contract, so they live here (the portable core) instead of
//! being written twice in the HTTP layers.

use serde_json::Value;

/// Parses `ids=1,2,3`.
///
/// An omitted or empty value is rejected: exporting the whole library by
/// accident is not a useful default for a download endpoint. Duplicate ids are
/// exported once.
pub fn parse_ids(value: Option<&str>) -> std::result::Result<Vec<i64>, String> {
    let Some(value) = value else {
        return Err("ids を指定してください (例: /api/transfer/export?ids=1,2)".to_string());
    };
    let mut ids = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id = part
            .parse::<i64>()
            .map_err(|_| format!("不正な ID です: {part:?}"))?;
        if id < 0 {
            return Err(format!("不正な ID です: {part:?}"));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("ids を指定してください (例: /api/transfer/export?ids=1,2)".to_string());
    }
    Ok(ids)
}

/// Reads a boolean query flag (`true` / `1` / `yes` / `on`).
pub fn flag_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim),
        Some("1" | "true" | "TRUE" | "True" | "yes" | "on")
    )
}

/// Whether a `Content-Type` header value is an accepted ZIP media type.
///
/// The parameter is the raw header value; the media type is the part before
/// the first `;`.
pub fn is_zip_content_type(value: &str) -> bool {
    let value = value.split(';').next().unwrap_or("").trim();
    value.eq_ignore_ascii_case("application/zip")
        || value.eq_ignore_ascii_case("application/x-zip-compressed")
        || value.eq_ignore_ascii_case("application/octet-stream")
}

/// The JSON summary of an import run (the response body of
/// `POST /api/transfer/import`).
pub fn import_payload(
    success: bool,
    created: usize,
    overwritten: usize,
    skipped: usize,
    errors: Vec<String>,
) -> Value {
    serde_json::json!({
        "success": success,
        "created": created,
        "overwritten": overwritten,
        "skipped": skipped,
        "errors": errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_parsed_and_deduplicated() {
        assert_eq!(parse_ids(Some("1,2, 3")).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_ids(Some("2,2")).unwrap(), vec![2]);
        assert!(parse_ids(None).is_err());
        assert!(parse_ids(Some("")).is_err());
        assert!(parse_ids(Some("1,x")).is_err());
        assert!(parse_ids(Some("-1")).is_err());
    }

    #[test]
    fn flags_accept_true_like_values() {
        assert!(flag_value(Some("true")));
        assert!(flag_value(Some("1")));
        assert!(flag_value(Some("on")));
        assert!(!flag_value(Some("false")));
        assert!(!flag_value(None));
    }

    #[test]
    fn zip_media_types_are_accepted_with_parameters() {
        assert!(is_zip_content_type("application/zip"));
        assert!(is_zip_content_type("application/zip; charset=binary"));
        assert!(is_zip_content_type("application/octet-stream"));
        assert!(!is_zip_content_type("application/json"));
        assert!(!is_zip_content_type(""));
    }

}
