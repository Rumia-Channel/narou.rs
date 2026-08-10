//! Application boundary for the native self-update workflow.
//!
//! The web layer owns request/response and presentation concerns only.  The
//! platform adapter owns release discovery, temporary files, archive
//! validation, updater process spawning, and restart arguments.

use std::sync::Arc;

use crate::application::events::EventSink;
use crate::error::Result;
use crate::platform::PlatformFuture;

#[derive(Debug, Clone, Default)]
pub struct SelfUpdateRequest {
    pub asset_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateResult {
    pub asset_name: String,
}

pub trait SelfUpdateService: Send + Sync {
    fn start<'a>(
        &'a self,
        request: SelfUpdateRequest,
        events: Arc<dyn EventSink>,
    ) -> PlatformFuture<'a, Result<SelfUpdateResult>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSelfUpdateService;

impl SelfUpdateService for NoopSelfUpdateService {
    fn start<'a>(
        &'a self,
        _request: SelfUpdateRequest,
        _events: Arc<dyn EventSink>,
    ) -> PlatformFuture<'a, Result<SelfUpdateResult>> {
        Box::pin(async {
            Err(crate::error::NarouError::Platform(
                "self-update is unavailable on this platform".to_string(),
            ))
        })
    }
}
