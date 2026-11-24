use std::sync::Arc;

use thiserror::Error;

fn object_io_detail(source: &std::io::Error) -> std::io::Error {
    // Some filesystem helpers embed absolute paths in their error text.
    // Show only the OS error or kind; retain the original as the source.
    match source.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::from(source.kind()),
    }
}

#[derive(Error, Debug)]
pub enum NarouError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("IO error: {operation} for object {key:?}: {}", object_io_detail(.source))]
    ObjectIo {
        operation: &'static str,
        key: String,
        #[source]
        source: std::io::Error,
    },

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Platform error: {0}")]
    Platform(String),

    #[error("novel_recovering: creation cleanup pending")]
    CreationCleanupPending,

    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Download suspended: {0}")]
    SuspendDownload(String),

    #[error("Novel not found: {0}")]
    NotFound(String),

    #[error("Invalid target: {0}")]
    InvalidTarget(String),

    #[error("Conversion error: {0}")]
    Conversion(String),

    #[error("Site setting error: {0}")]
    SiteSetting(String),

    #[error("download budget expired before section {next_section_index}")]
    DownloadBudgetExpired { next_section_index: usize },

    #[error("download resume checkpoint is corrupt: {0}")]
    DownloadResumeCorrupt(String),

    #[error("Unsupported on this platform: {0}")]
    Unsupported(String),

    #[error("Login error: {0}")]
    Login(String),

    #[error(transparent)]
    Shared(Arc<NarouError>),
}

pub type Result<T> = std::result::Result<T, NarouError>;
