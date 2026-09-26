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
    /// Minimum spacing for this site, from the definition's `min_interval`
    /// (seconds). Sites that answer a burst with 429 (Pixiv) declare a floor
    /// here instead of slowing every other site down. `None` keeps the global
    /// `download.interval`.
    pub min_interval: Option<std::time::Duration>,
}

impl RateLimitScope {
    pub fn site(site: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            narou: false,
            min_interval: None,
        }
    }

    pub fn narou(site: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            narou: true,
            min_interval: None,
        }
    }

    /// Apply a site definition's `min_interval`, ignoring absent or
    /// non-positive values.
    pub fn with_min_interval(mut self, seconds: Option<f64>) -> Self {
        self.min_interval = seconds
            .filter(|seconds| *seconds > 0.0)
            .map(std::time::Duration::from_secs_f64);
        self
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

/// `download.wait-steps` の正規化。なろう系スコープは 0・10 超を 10 に
/// 丸める (API 制約)、それ以外のサイトは設定値をそのまま使う
/// (未設定 = 0 = wait-steps 無し)。native (`src/downloader/rate_limit.rs`)
/// と Worker (`worker_entry/src/rate_limiter.rs`) の共通規則。
pub fn normalize_wait_steps(raw: i64, narou: bool) -> u32 {
    let wait_steps = if raw > 0 { raw.min(u32::MAX as i64) as u32 } else { 0 };
    if narou && (wait_steps == 0 || wait_steps > 10) { 10 } else { wait_steps }
}

/// `download.interval` / `download.wait-steps` の設定値を展開した
/// ペーシング設定。`RateLimiter` 実装 (native / Worker) が共有する。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownloadPacing {
    /// リクエスト間の最小間隔 (`download.interval`、0 許容 = 間隔なし)。
    pub interval: std::time::Duration,
    /// `download.wait-steps` の生設定値。スコープの `narou` フラグで
    /// `normalize_wait_steps` に掛けてから使う。
    pub wait_steps: i64,
    /// `wait_steps` ごとの休止の基準 (`STEPS_WAIT_TIME.max(interval)`)。
    /// native と同じく設定値だけで決め、サイト下限は `for_scope` で畳む。
    pub max_steps_wait_time: std::time::Duration,
}

/// `download.interval` が未設定のときの既定値 (native `DEFAULT_INTERVAL_SECS`)。
pub const DEFAULT_DOWNLOAD_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(700);
/// `wait_steps` ごとの休止の既定 (native `STEPS_WAIT_TIME`)。
pub const STEPS_WAIT_TIME: std::time::Duration = std::time::Duration::from_secs(5);

impl DownloadPacing {
    /// `interval_secs` は `download.interval` の秒 (`None` で 0.7s)、
    /// `wait_steps` は `download.wait-steps` の生値 (`None` で 0)。
    pub fn new(interval_secs: Option<f64>, wait_steps: Option<i64>) -> Self {
        let interval = interval_secs
            .map(|secs| std::time::Duration::from_secs_f64(secs.max(0.0)))
            .unwrap_or(DEFAULT_DOWNLOAD_INTERVAL);
        Self {
            interval,
            wait_steps: wait_steps.unwrap_or(0),
            max_steps_wait_time: STEPS_WAIT_TIME.max(interval),
        }
    }

    /// スコープへ適用するペーシング。サイト定義が `min_interval` を宣言して
    /// いれば全体設定より優先し (native `reserve_wait_duration_for_scope`)、
    /// `wait_steps` は `narou` フラグで正規化する。`max_steps_wait_time` は
    /// `max(基準, min_interval)` — DO 側はこの単一値を「`wait_steps` ごとの
    /// 休止」と「長い静寂後のバーストカウンタのリセット窓」の両方に使う。
    pub fn for_scope(&self, scope: &RateLimitScope) -> ScopedPacing {
        let min_interval = scope.min_interval.unwrap_or(std::time::Duration::ZERO);
        ScopedPacing {
            interval: self.interval.max(min_interval),
            wait_steps: normalize_wait_steps(self.wait_steps, scope.narou),
            max_steps_wait_time: self.max_steps_wait_time.max(min_interval),
        }
    }
}

/// `DownloadPacing` をスコープへ適用した結果 (DO / 各実装がそのまま使う形)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedPacing {
    pub interval: std::time::Duration,
    pub wait_steps: u32,
    pub max_steps_wait_time: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_carries_a_site_interval() {
        let scope = RateLimitScope::site("www.pixiv.net").with_min_interval(Some(5.0));
        assert_eq!(scope.min_interval, Some(std::time::Duration::from_secs(5)));
        assert_eq!(RateLimitScope::site("x").min_interval, None);
        // 0 や負値、未指定は「指定なし」として扱う。
        assert_eq!(RateLimitScope::site("x").with_min_interval(Some(0.0)).min_interval, None);
        assert_eq!(RateLimitScope::site("x").with_min_interval(Some(-1.0)).min_interval, None);
        assert_eq!(RateLimitScope::site("x").with_min_interval(None).min_interval, None);
    }

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

    #[test]
    fn wait_steps_normalization_mirrors_native() {
        // なろう: 0・10 超は 10、それ以外は設定値。
        assert_eq!(normalize_wait_steps(0, true), 10);
        assert_eq!(normalize_wait_steps(11, true), 10);
        assert_eq!(normalize_wait_steps(4, true), 4);
        assert_eq!(normalize_wait_steps(-1, true), 10);
        // 非なろう: 0/負は 0、正はそのまま。
        assert_eq!(normalize_wait_steps(0, false), 0);
        assert_eq!(normalize_wait_steps(20, false), 20);
        assert_eq!(normalize_wait_steps(-3, false), 0);
    }

    #[test]
    fn pacing_defaults_match_native_constants() {
        let pacing = DownloadPacing::new(None, None);
        assert_eq!(pacing.interval, std::time::Duration::from_millis(700));
        assert_eq!(pacing.wait_steps, 0);
        assert_eq!(pacing.max_steps_wait_time, std::time::Duration::from_secs(5));

        // なろうスコープ: 既定の wait-steps 10 が効く。
        let scoped = pacing.for_scope(&RateLimitScope::narou("ncode.syosetu.com"));
        assert_eq!(scoped.wait_steps, 10);
        assert_eq!(scoped.interval, std::time::Duration::from_millis(700));
    }

    #[test]
    fn pacing_uses_configured_values() {
        let pacing = DownloadPacing::new(Some(2.5), Some(4));
        let scoped = pacing.for_scope(&RateLimitScope::narou("n"));
        assert_eq!(scoped.interval, std::time::Duration::from_millis(2_500));
        assert_eq!(scoped.wait_steps, 4);
        // `interval = 0` は「間隔なし」。
        assert_eq!(
            DownloadPacing::new(Some(0.0), None)
                .for_scope(&RateLimitScope::site("x"))
                .interval,
            std::time::Duration::ZERO
        );
        // 設定値が 5s を超えると wait-steps の休止もそれに引き上げられる。
        assert_eq!(
            DownloadPacing::new(Some(8.0), None).max_steps_wait_time,
            std::time::Duration::from_secs(8)
        );
    }

    #[test]
    fn site_min_interval_overrides_global_interval() {
        let pacing = DownloadPacing::new(Some(0.7), None);
        let pixiv = RateLimitScope::site("www.pixiv.net").with_min_interval(Some(5.0));
        let scoped = pacing.for_scope(&pixiv);
        assert_eq!(scoped.interval, std::time::Duration::from_secs(5));
        assert_eq!(scoped.max_steps_wait_time, std::time::Duration::from_secs(5));

        // 下限が設定値を下回るなら設定値が効く。
        let mild = RateLimitScope::site("www.pixiv.net").with_min_interval(Some(0.1));
        assert_eq!(
            pacing.for_scope(&mild).interval,
            std::time::Duration::from_millis(700)
        );
    }
}
