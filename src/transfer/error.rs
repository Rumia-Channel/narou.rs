use thiserror::Error;

/// Result type used by the transfer module.
pub type Result<T> = std::result::Result<T, TransferError>;

/// Errors produced while building, parsing or validating transfer archives.
#[derive(Debug, Error)]
pub enum TransferError {
    /// The archive or its manifest is structurally invalid.
    #[error("invalid transfer archive: {0}")]
    InvalidArchive(String),

    /// The archive, one of its entries or its manifest exceeds a size cap.
    #[error("transfer archive is too large: {0}")]
    TooLarge(String),

    /// A path would escape the destination directory or collide on Windows.
    #[error("unsafe path in transfer archive: {0}")]
    UnsafePath(String),

    /// The manifest declares a format version this build cannot read or write.
    #[error("unsupported transfer archive format version {0}")]
    UnsupportedVersion(u32),

    /// A file does not match the SHA-256 digest stored in the manifest.
    #[error("sha256 mismatch for {path}: manifest has {expected}, archive has {actual}")]
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    /// The ZIP container could not be read or written.
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),

    /// The manifest is not valid JSON or does not match the schema.
    #[error("manifest JSON error: {0}")]
    Serde(#[from] serde_json::Error),
}

impl From<TransferError> for crate::error::NarouError {
    fn from(error: TransferError) -> Self {
        crate::error::NarouError::Platform(format!("transfer: {error}"))
    }
}
