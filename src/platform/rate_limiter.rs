//! Rate limiting abstraction.
//!
//! The domain layer (downloader, crawler) must not call `thread::sleep` or
//! `tokio::time::sleep` directly to space out site requests. Instead it asks a
//! [`RateLimiter`] to acquire a slot for a scope (usually a site/host). The
//! native implementation keeps the current per-host state machine; a Worker
//! implementation could serialize per-site access via a Durable Object.

use std::fmt;

use super::PlatformFuture;

/// Which site/scope a request belongs to. Rate limiting is per site so that
/// parallel work against different domains does not share one global slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimitScope {
    pub site: String,
    /// True when the site is なろう系 (uses the narou wait-steps default of
    /// 10). The native limiter applies `normalize_wait_steps(.., true)` for
    /// these scopes; other scopes use the configured wait-steps as-is.
    pub narou: bool,
}

impl RateLimitScope {
    pub fn site(site: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            narou: false,
        }
    }

    pub fn narou(site: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            narou: true,
        }
    }
}

impl fmt::Display for RateLimitScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.site)
    }
}

/// Async rate limiter. `acquire` blocks (asynchronously) until the next slot
/// for the scope is available, then returns. The future follows
/// [`PlatformFuture`]: `Send` on native so parallel domain workers can run on
/// a multi-threaded executor.
pub trait RateLimiter: Send + Sync {
    fn acquire<'a>(
        &'a self,
        scope: &'a RateLimitScope,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
}

/// Normalize a site key for rate-limit scoping and Durable Object naming.
///
/// The Durable Object id is derived from this key, so two spellings of the
/// same site must collapse to one key or they would get independent rate
/// buckets (and an attacker-supplied target could fragment the limiter).
/// Returns `None` for keys that have no usable host form.
pub fn normalize_site_key(site: &str) -> Option<String> {
    let key = site.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    // Strip a default port so `syosetu.com:443` and `syosetu.com` share a
    // bucket, while keeping non-default ports (or bare IPv6) distinct.
    for port in [":443", ":80"] {
        if let Some(stripped) = key.strip_suffix(port) {
            if !stripped.contains(':') {
                return Some(stripped.to_string());
            }
            break;
        }
    }
    Some(key)
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

    #[test]
    fn site_key_normalizes_case_and_default_port() {
        assert_eq!(normalize_site_key("syosetu.com").as_deref(), Some("syosetu.com"));
        assert_eq!(normalize_site_key("  Syosetu.COM  ").as_deref(), Some("syosetu.com"));
        assert_eq!(normalize_site_key("syosetu.com:443").as_deref(), Some("syosetu.com"));
        assert_eq!(normalize_site_key("syosetu.com:80").as_deref(), Some("syosetu.com"));
        // Non-default ports stay distinct buckets.
        assert_eq!(normalize_site_key("syosetu.com:8080").as_deref(), Some("syosetu.com:8080"));
        // Bare IPv6 must not be mangled by port stripping.
        assert_eq!(normalize_site_key("[::1]:80").as_deref(), Some("[::1]:80"));
        // Empty keys are unusable.
        assert_eq!(normalize_site_key("   "), None);
        assert_eq!(normalize_site_key(""), None);
    }
}
