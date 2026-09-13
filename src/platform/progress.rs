//! Portable progress reporting capability.
//!
//! The trait is defined here so the shared downloader (and any other core
//! module) can report progress without depending on the native terminal
//! implementations (`indicatif` progress bars, WebSocket event emission)
//! which live in `crate::progress` under the `native-runtime` feature.

/// Progress reporting sink injected into long-running core operations.
///
/// Implementations are platform-specific: native builds render terminal
/// progress bars or WebSocket events; Worker builds supply a no-op or
/// trace-backed reporter.
pub trait ProgressReporter: Send + Sync {
    fn set_length(&self, len: u64);
    fn set_position(&self, pos: u64);
    fn inc(&self, delta: u64);
    fn set_message(&self, msg: &str);
    fn finish_with_message(&self, msg: &str);
    fn println(&self, msg: &str);
}

/// Progress reporter that discards all output.
pub struct NoProgress;

impl ProgressReporter for NoProgress {
    fn set_length(&self, _len: u64) {}
    fn set_position(&self, _pos: u64) {}
    fn inc(&self, _delta: u64) {}
    fn set_message(&self, _msg: &str) {}
    fn finish_with_message(&self, _msg: &str) {}
    fn println(&self, msg: &str) {
        eprintln!("{}", msg);
    }
}
