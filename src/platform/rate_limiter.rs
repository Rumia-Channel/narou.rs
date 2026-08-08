//! Rate limiting abstraction.
//!
//! The domain layer (downloader, crawler) must not call `thread::sleep` or
//! `tokio::time::sleep` directly to space out site requests. Instead it asks a
//! [`RateLimiter`] to acquire a slot for a scope (usually a site/host). The
//! native implementation keeps the current per-host state machine; a Worker
//! implementation could serialize per-site access via a Durable Object.

use std::fmt;

/// Which site/scope a request belongs to. Rate limiting is per site so that
/// parallel work against different domains does not share one global slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimitScope {
    pub site: String,
}

impl RateLimitScope {
    pub fn site(site: impl Into<String>) -> Self {
        Self { site: site.into() }
    }
}

impl fmt::Display for RateLimitScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.site)
    }
}

/// Async rate limiter. `acquire` blocks (asynchronously) until the next slot
/// for the scope is available, then returns.
pub trait RateLimiter: Send + Sync {
    fn acquire(&self, scope: &RateLimitScope) -> crate::error::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_display_and_equality() {
        let a = RateLimitScope::site("syosetu.com");
        let b = RateLimitScope::site("syosetu.com");
        let c = RateLimitScope::site("kakuyomu.jp");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.to_string(), "syosetu.com");
    }
}
