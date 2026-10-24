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

/// Collapse related hosts into one download family.
///
/// なろう / なろうR18 / 作者ページ / API は同じ運営なので、Worker の
/// per-site admission でも同一キーにまとめる。Pixiv の `www` / `accounts`
/// のような別ホストも同じ。それ以外は登録可能ドメイン相当
/// (最後の 2〜3 ラベル) でまとめる。
pub fn site_family(site: &str) -> String {
    let domain = site.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return String::new();
    }
    const TWO_LEVEL_SUFFIXES: &[&str] = &[
        "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "ed.jp", "gr.jp", "lg.jp",
        "com.au", "co.uk", "org.uk", "co.kr", "com.cn", "com.tw", "co.in",
    ];
    let labels: Vec<&str> = domain.split('.').filter(|label| !label.is_empty()).collect();
    if labels.len() <= 2 {
        return domain;
    }
    let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    if TWO_LEVEL_SUFFIXES.contains(&last_two.as_str()) && labels.len() >= 3 {
        return format!(
            "{}.{}.{}",
            labels[labels.len() - 3],
            labels[labels.len() - 2],
            labels[labels.len() - 1]
        );
    }
    last_two
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

/// `update.interval` の既定値かつ下限 (秒)。作品開始の間隔を数える
/// native `commands::update` と Worker の `update-start` バケットで共有する。
pub const UPDATE_INTERVAL_MIN_SECS: f64 = 2.5;

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

    pub fn from_settings(values: &std::collections::HashMap<String, serde_yaml::Value>) -> Self {
        Self::new(
            setting_interval_secs(values.get("download.interval")),
            setting_wait_steps(values.get("download.wait-steps")),
        )
    }

    /// スコープへ適用するペーシング。サイト定義が `min_interval` を宣言して
    /// いれば全体設定より優先し (native `reserve_wait_duration_for_scope`)、
    /// `wait_steps` は `narou` フラグで正規化する。`max_steps_wait_time` は
    /// `max(基準, min_interval)` で、リセット窓だけは native と同じく設定値の
    /// まま (`reset_window`) — サイト下限は長い静寂の判定には足さない。
    pub fn for_scope(&self, scope: &RateLimitScope) -> ScopedPacing {
        self.for_site(scope.narou, scope.min_interval)
    }

    /// Apply the scope's policy flags without requiring its transport identity.
    pub fn for_site(&self, narou: bool, min_interval: Option<std::time::Duration>) -> ScopedPacing {
        let min_interval = min_interval.unwrap_or(std::time::Duration::ZERO);
        ScopedPacing {
            interval: self.interval.max(min_interval),
            wait_steps: normalize_wait_steps(self.wait_steps, narou),
            max_steps_wait_time: self.max_steps_wait_time.max(min_interval),
            reset_window: self.max_steps_wait_time,
        }
    }
}

/// `DownloadPacing` をスコープへ適用した結果 (DO / 各実装がそのまま使う形)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedPacing {
    /// リクエスト間の最小間隔 (サイト下限で引き上げた後の値)。
    pub interval: std::time::Duration,
    /// なろう系の正規化済み wait-steps (0 = max-step 休止なし)。
    pub wait_steps: u32,
    /// `wait_steps` ごとの休止 (`max(STEPS_WAIT_TIME, interval, min_interval)`)。
    pub max_steps_wait_time: std::time::Duration,
    /// 長い静寂後のバーストカウンタのリセット窓。native と同じく設定値だけで
    /// 決まり (`max(STEPS_WAIT_TIME, interval)`)、サイト下限は足さない。
    pub reset_window: std::time::Duration,
}

/// Effective request spacing in milliseconds. Permit state records only
/// actual grants, so a changed schedule cannot invalidate another waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PacingSchedule {
    pub interval_ms: u64,
    pub wait_steps: u32,
    pub max_steps_wait_time_ms: u64,
    pub reset_window_ms: u64,
}

impl PacingSchedule {
    /// Round up to milliseconds so a positive site floor is never shortened.
    pub fn from_scoped(pacing: &ScopedPacing) -> Self {
        Self {
            interval_ms: duration_millis_ceil(pacing.interval),
            wait_steps: pacing.wait_steps,
            max_steps_wait_time_ms: duration_millis_ceil(pacing.max_steps_wait_time),
            reset_window_ms: duration_millis_ceil(pacing.reset_window),
        }
    }
}

/// Per-host grant state, shared by the native adapter and Durable Object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PacingState {
    pub counter: u64,
    /// Time of the last actual grant, never a future reservation.
    pub last_permit_ms: Option<u64>,
    /// Earliest next grant; also preserves an imported legacy reservation fence.
    pub next_permit_ms: Option<u64>,
    pub schedule: Option<PacingSchedule>,
}

/// A denied acquisition does not reserve a future slot. Callers wait and
/// recheck, which prevents delayed or concurrent waiters from bursting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PacingDecision {
    Granted,
    Wait { wait_ms: u64 },
}

pub fn try_acquire(
    state: &mut PacingState,
    now_ms: u64,
    schedule: PacingSchedule,
) -> PacingDecision {
    rebase_on_schedule_change(state, schedule);
    reset_idle_counter(state, now_ms, schedule.reset_window_ms);
    if let Some(next) = state.next_permit_ms && next > now_ms {
        return PacingDecision::Wait { wait_ms: next - now_ms };
    }
    state.counter = state.counter.saturating_add(1);
    state.last_permit_ms = Some(now_ms);
    state.next_permit_ms = Some(now_ms.saturating_add(delay_after_grant(state.counter, schedule)));
    PacingDecision::Granted
}

fn delay_after_grant(counter: u64, schedule: PacingSchedule) -> u64 {
    if schedule.wait_steps > 0 && counter.is_multiple_of(u64::from(schedule.wait_steps)) {
        schedule.max_steps_wait_time_ms.max(schedule.interval_ms)
    } else {
        schedule.interval_ms
    }
}

fn rebase_on_schedule_change(state: &mut PacingState, schedule: PacingSchedule) {
    if state.schedule.is_some_and(|previous| previous != schedule)
        && let Some(last) = state.last_permit_ms
    {
        state.next_permit_ms = Some(last.saturating_add(delay_after_grant(state.counter, schedule)));
    }
    state.schedule = Some(schedule);
}

/// native の idle reset: pending slot が無く、最後の permit から
/// `reset_window_ms` を超えて静かなホストはバーストカウンタを 0 に戻す。
fn reset_idle_counter(state: &mut PacingState, now_ms: u64, reset_window_ms: u64) {
    let Some(last_permit_ms) = state.last_permit_ms else {
        return;
    };
    let no_pending_slot = state.next_permit_ms.is_none_or(|next| now_ms >= next);
    if now_ms.saturating_sub(last_permit_ms) > reset_window_ms && no_pending_slot {
        state.counter = 0;
        state.last_permit_ms = None;
        state.next_permit_ms = None;
    }
}

pub fn duration_millis_ceil(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

/// `download.interval` の設定値を秒として解釈する (Number / 数値文字列、
/// 前後の空白は許容)。解釈できない値は `None` = 未設定。
pub fn setting_interval_secs(value: Option<&serde_yaml::Value>) -> Option<f64> {
    let seconds = match value? {
        serde_yaml::Value::Number(number) => number.as_f64(),
        serde_yaml::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    seconds.filter(|seconds| seconds.is_finite())
}

/// `download.wait-steps` の設定値を解釈する。解釈できない値は `None` = 未設定。
pub fn setting_wait_steps(value: Option<&serde_yaml::Value>) -> Option<i64> {
    match value? {
        serde_yaml::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok())),
        serde_yaml::Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `update.interval` shares the CLI's 2.5-second default and lower bound.
pub fn setting_update_interval_secs(value: Option<&serde_yaml::Value>) -> f64 {
    setting_interval_secs(value).unwrap_or(UPDATE_INTERVAL_MIN_SECS).max(UPDATE_INTERVAL_MIN_SECS)
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
    fn site_family_groups_narou_and_sibling_hosts() {
        assert_eq!(site_family("ncode.syosetu.com"), "syosetu.com");
        assert_eq!(site_family("novel18.syosetu.com"), "syosetu.com");
        assert_eq!(site_family("xmypage.syosetu.com"), "syosetu.com");
        assert_eq!(site_family("www.pixiv.net"), "pixiv.net");
        assert_eq!(site_family("kakuyomu.jp"), "kakuyomu.jp");
        assert_eq!(site_family("www.alphapolis.co.jp"), "alphapolis.co.jp");
        assert_eq!(site_family(""), "");
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
    fn pacing_uses_configured_values() {
        let pacing = DownloadPacing::new(Some(2.5), Some(4));
        let scoped = pacing.for_scope(&RateLimitScope::narou("n"));
        assert_eq!(scoped.interval, std::time::Duration::from_millis(2_500));
        assert_eq!(scoped.wait_steps, 4);
        // `interval = 0` は「間隔なし」で、負値は 0 へクランプする。
        assert_eq!(
            DownloadPacing::new(Some(0.0), None)
                .for_scope(&RateLimitScope::site("x"))
                .interval,
            std::time::Duration::ZERO
        );
        assert_eq!(
            DownloadPacing::new(Some(-1.0), None).interval,
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
        assert_eq!(scoped.reset_window, std::time::Duration::from_secs(5));

        // リセット窓は設定値だけで決まる (サイト下限で引き上げない)。
        let paced = DownloadPacing::new(Some(8.0), None);
        assert_eq!(
            paced.for_scope(&pixiv).reset_window,
            std::time::Duration::from_secs(8)
        );

        // 下限が設定値を下回るなら設定値が効く。
        let mild = RateLimitScope::site("www.pixiv.net").with_min_interval(Some(0.1));
        assert_eq!(
            pacing.for_scope(&mild).interval,
            std::time::Duration::from_millis(700)
        );
    }

    fn schedule(interval_ms: u64, wait_steps: u32) -> PacingSchedule {
        PacingSchedule {
            interval_ms,
            wait_steps,
            max_steps_wait_time_ms: 5_000.max(interval_ms),
            reset_window_ms: 5_000.max(interval_ms),
        }
    }

    #[test]
    fn waiters_recheck_without_reserving_or_bursting() {
        let mut state = PacingState::default();
        let pace = schedule(700, 0);
        assert_eq!(try_acquire(&mut state, 1_000, pace), PacingDecision::Granted);
        for _ in 0..3 {
            assert_eq!(try_acquire(&mut state, 1_000, pace), PacingDecision::Wait { wait_ms: 700 });
        }
        assert_eq!(try_acquire(&mut state, 1_700, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 1_700, pace), PacingDecision::Wait { wait_ms: 700 });
        // Waking much later still grants only one request, not every past slot.
        assert_eq!(try_acquire(&mut state, 100_000, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 100_001, pace), PacingDecision::Wait { wait_ms: 699 });
    }

    #[test]
    fn periodic_pause_follows_the_nth_actual_request() {
        let mut state = PacingState::default();
        let pace = schedule(700, 3);
        let grants = [1_000, 1_700, 2_400, 7_400, 8_100, 8_800, 13_800];
        for (index, now) in grants.into_iter().enumerate() {
            if index > 0 {
                assert_eq!(try_acquire(&mut state, now - 1, pace), PacingDecision::Wait { wait_ms: 1 });
            }
            assert_eq!(try_acquire(&mut state, now, pace), PacingDecision::Granted);
        }
    }

    #[test]
    fn changed_interval_rebases_from_the_last_actual_grant() {
        let mut state = PacingState::default();
        assert_eq!(try_acquire(&mut state, 1_000, schedule(60_000, 0)), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 2_000, schedule(60_000, 0)), PacingDecision::Wait { wait_ms: 59_000 });
        assert_eq!(try_acquire(&mut state, 2_000, schedule(5_000, 0)), PacingDecision::Wait { wait_ms: 4_000 });
        assert_eq!(try_acquire(&mut state, 6_000, schedule(5_000, 0)), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 6_000, schedule(8_000, 0)), PacingDecision::Wait { wait_ms: 8_000 });
        assert_eq!(try_acquire(&mut state, 14_000, schedule(8_000, 0)), PacingDecision::Granted);
    }

    #[test]
    fn changed_step_pause_and_site_floor_apply_to_pending_waiters() {
        let scope = RateLimitScope::site("floor.example").with_min_interval(Some(2.0001));
        let paced = DownloadPacing::new(Some(0.1), Some(2));
        let pace = PacingSchedule::from_scoped(&paced.for_scope(&scope));
        let mut state = PacingState::default();
        assert_eq!(try_acquire(&mut state, 0, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 2_000, pace), PacingDecision::Wait { wait_ms: 1 });
        assert_eq!(try_acquire(&mut state, 2_001, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 4_002, pace), PacingDecision::Wait { wait_ms: 2_999 });
        let no_steps = PacingSchedule::from_scoped(&DownloadPacing::new(Some(0.0), Some(0)).for_scope(&scope));
        assert_eq!(try_acquire(&mut state, 4_002, no_steps), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 4_002, no_steps), PacingDecision::Wait { wait_ms: 2_001 });
    }

    #[test]
    fn idle_reset_restores_a_full_request_cycle_but_keeps_imported_fences() {
        let mut state = PacingState::default();
        let pace = schedule(700, 2);
        assert_eq!(try_acquire(&mut state, 0, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 5_001, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 5_701, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut state, 6_401, pace), PacingDecision::Wait { wait_ms: 4_300 });

        let mut legacy = PacingState { next_permit_ms: Some(9_000), ..Default::default() };
        assert_eq!(try_acquire(&mut legacy, 2_000, pace), PacingDecision::Wait { wait_ms: 7_000 });
        assert_eq!(try_acquire(&mut legacy, 3_000, schedule(100, 0)), PacingDecision::Wait { wait_ms: 6_000 });
        assert_eq!(try_acquire(&mut legacy, 9_000, pace), PacingDecision::Granted);
        assert_eq!(try_acquire(&mut legacy, 9_000, pace), PacingDecision::Wait { wait_ms: 700 });
    }

    #[test]
    fn setting_values_are_parsed_for_both_runtimes() {
        let padded = serde_yaml::Value::String(" 0.5 ".to_string());
        assert_eq!(setting_interval_secs(Some(&padded)), Some(0.5));
        let number = serde_yaml::Value::Number(serde_yaml::Number::from(2));
        assert_eq!(setting_interval_secs(Some(&number)), Some(2.0));
        let invalid = serde_yaml::Value::String("abc".to_string());
        assert_eq!(setting_interval_secs(Some(&invalid)), None);
        assert_eq!(setting_interval_secs(None), None);

        let padded = serde_yaml::Value::String(" 12 ".to_string());
        assert_eq!(setting_wait_steps(Some(&padded)), Some(12));
        let number = serde_yaml::Value::Number(serde_yaml::Number::from(3));
        assert_eq!(setting_wait_steps(Some(&number)), Some(3));
        let invalid = serde_yaml::Value::Bool(true);
        assert_eq!(setting_wait_steps(Some(&invalid)), None);
    }
}
