//! Application-layer error type.
//!
//! The web layer maps these to HTTP responses; the application layer itself
//! never deals with status codes. Errors are grouped by the kind of failure
//! so the web layer can choose an appropriate status without string matching.

use std::fmt;

/// Error returned by application services.
///
/// Deliberately free of HTTP concepts: no status codes, no JSON types. The
/// web layer converts [`ApplicationError`] into an HTTP response at the
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationError {
    /// The underlying repository/port failed (database, inventory, ...).
    /// The message is the platform error's display text.
    Platform(String),
    /// The requested record does not exist.
    NotFound(String),
    /// The request was malformed (bad sort column, invalid pagination, ...).
    InvalidRequest(String),
}

impl ApplicationError {
    /// Wrap a platform error's display text.
    pub fn platform(error: impl fmt::Display) -> Self {
        Self::Platform(error.to_string())
    }
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(message) => write!(f, "platform error: {message}"),
            Self::NotFound(message) => write!(f, "not found: {message}"),
            Self::InvalidRequest(message) => write!(f, "invalid request: {message}"),
        }
    }
}

impl std::error::Error for ApplicationError {}
