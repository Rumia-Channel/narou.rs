//! Pure policy for the persisted Worker illustration backend.
//!
//! D1 returns `value_json` as TEXT, not as the decoded JSON value. The exact
//! legacy raw markers `d1` / `s3` also remain valid for manually selected stores.
//! Only a missing row keeps the standalone legacy D1 default. Invalid rows
//! and read failures never select a backend. `NAROU_REQUIRE_S3=true` resolves
//! every valid selection to S3 without modifying the persisted marker or rows.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetBackend {
    D1,
    S3,
}

/// Fetch the complete row to distinguish SQL NULL from a missing row. A
/// column-only `first("value_json")` returns null for both cases.
#[derive(Debug, serde::Deserialize)]
pub struct AssetBackendRow {
    pub value_json: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendError {
    ReadFailed,
    InvalidJsonString,
    UnknownBackend,
    InvalidRequirement,
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadFailed => "failed to read app_state('inv', 'asset_backend')",
            Self::InvalidJsonString => "asset_backend must contain a JSON string",
            Self::UnknownBackend => "asset_backend must be d1 or s3",
            Self::InvalidRequirement => "NAROU_REQUIRE_S3 must be true or false when configured",
        })
    }
}

/// Parse the actual D1 row. A database/decoding error or SQL NULL is distinct
/// from a genuinely missing row, the only implicit legacy D1 selection.
pub fn parse_asset_backend(
    row: Result<Option<AssetBackendRow>, ()>,
) -> Result<AssetBackend, BackendError> {
    let Some(row) = row.map_err(|()| BackendError::ReadFailed)? else {
        return Ok(AssetBackend::D1);
    };
    let value_json = row
        .value_json
        .as_deref()
        .ok_or(BackendError::InvalidJsonString)?;
    // Older manual selections used raw TEXT instead of JSON. Preserve only
    // these two exact markers so existing S3 selections keep working.
    match value_json {
        "d1" => return Ok(AssetBackend::D1),
        "s3" => return Ok(AssetBackend::S3),
        _ => {}
    }
    let value: String =
        serde_json::from_str(value_json).map_err(|_| BackendError::InvalidJsonString)?;
    match value.as_str() {
        "d1" => Ok(AssetBackend::D1),
        "s3" => Ok(AssetBackend::S3),
        _ => Err(BackendError::UnknownBackend),
    }
}

pub fn required_s3(value: Option<&str>) -> Result<bool, BackendError> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(BackendError::InvalidRequirement),
    }
}

impl AssetBackend {
    /// Apply deployment policy on every use, including cached selections.
    /// This runs only after the persisted value has been validated.
    pub fn resolve(self, required_s3: bool) -> Self {
        if required_s3 { Self::S3 } else { self }
    }
}
