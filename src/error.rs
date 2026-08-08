use thiserror::Error;

#[derive(Error, Debug)]
pub enum NarouError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Platform error: {0}")]
    Platform(String),

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
}

pub type Result<T> = std::result::Result<T, NarouError>;
