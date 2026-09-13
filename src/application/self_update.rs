//! Application boundary for the native self-update workflow.
//!
//! The web layer owns request/response and presentation concerns only.  The
//! platform adapter owns release discovery, temporary files, archive
//! validation, updater process spawning, and restart arguments.

use std::sync::Arc;

use crate::application::events::EventSink;
use crate::error::Result;
use crate::platform::PlatformFuture;

/// Release asset variant selected for a self-update download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelfUpdateVariant {
    /// Standard build (BSD-2-Clause); EPUB 変換には外部 AozoraEpub3 が必要。
    Standard,
    /// GPL build with AozoraEpub3_Lite embedded (`narou_rs_*-GPL.zip`).
    Gpl,
}

impl SelfUpdateVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Gpl => "gpl",
        }
    }

    pub fn from_str_lossy(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gpl" => Some(Self::Gpl),
            "standard" => Some(Self::Standard),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SelfUpdateRequest {
    pub asset_url: Option<String>,
    /// Explicit variant choice from the UI. `None` falls back to the saved
    /// `self-update.variant` preference, then to the running build's variant.
    pub variant: Option<SelfUpdateVariant>,
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
