//! Native-only web actions exposed through an application capability.
//!
//! The web layer consumes structured operations and output; it does not spawn
//! the narou executable or depend on CLI argument construction. Native keeps
//! the compatibility implementation in its adapter, while another backend
//! can implement these operations without a process.

use crate::platform::PlatformFuture;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebActionOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait WebActionService: Send + Sync {
    fn inspect<'a>(&'a self, targets: &'a [String]) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>>;
    fn folder<'a>(&'a self, targets: &'a [String]) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>>;
    fn setting_burn<'a>(&'a self, targets: &'a [String]) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>>;
    fn diff<'a>(&'a self, target: &'a str, number: &'a str) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>>;
    fn diff_clean<'a>(&'a self, target: &'a str) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>>;
    /// P4b: copy-forward restore of one stored version (`narou diff --restore`).
    fn diff_restore<'a>(&'a self, target: &'a str, version: i64) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        let _ = (target, version);
        unsupported()
    }
    /// P4b: merge sections of one stored version (`narou diff --merge-from`).
    fn diff_merge<'a>(&'a self, target: &'a str, version: i64, sections: Option<&'a str>) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        let _ = (target, version, sections);
        unsupported()
    }
    fn csv_import<'a>(&'a self, csv: &'a str) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>>;
    fn csv_download(&self) -> PlatformFuture<'_, crate::error::Result<WebActionOutput>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyWebActionService;

impl WebActionService for EmptyWebActionService {
    fn inspect<'a>(&'a self, _targets: &'a [String]) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        unsupported()
    }

    fn folder<'a>(&'a self, _targets: &'a [String]) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        unsupported()
    }

    fn setting_burn<'a>(&'a self, _targets: &'a [String]) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        unsupported()
    }

    fn diff<'a>(&'a self, _target: &'a str, _number: &'a str) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        unsupported()
    }

    fn diff_clean<'a>(&'a self, _target: &'a str) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        unsupported()
    }

    fn csv_import<'a>(&'a self, _csv: &'a str) -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
        unsupported()
    }

    fn csv_download(&self) -> PlatformFuture<'_, crate::error::Result<WebActionOutput>> {
        unsupported()
    }
}

fn unsupported<'a>() -> PlatformFuture<'a, crate::error::Result<WebActionOutput>> {
    Box::pin(async {
        Err(crate::error::NarouError::Platform(
            "web action capability is unavailable".to_string(),
        ))
    })
}
