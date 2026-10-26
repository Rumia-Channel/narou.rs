use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::platform::rate_limiter::{
    DownloadPacing, PacingDecision, PacingSchedule, PacingState, try_acquire,
};
use crate::platform::RateLimitScope;

/// ホスト別の許可状態。プロセス内の全 limiter で共有する。
static STATE: LazyLock<Mutex<HashMap<String, PacingState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// native の時計 ([`Instant`]) を許可エンジンの ms へ写す基準。
static MONOTONIC_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

pub struct RateLimiter {
    pacing: DownloadPacing,
}

impl RateLimiter {
    /// `download.interval` / `download.wait-steps` の保存値を読む。
    pub fn new() -> Self {
        Self::from_pacing(load_download_pacing())
    }

    /// Build a rate limiter with explicit interval/wait-steps, bypassing the
    /// default `download.interval` / `download.wait-steps` settings.
    /// Used by code paths (e.g. なろうAPI) that have their own dedicated
    /// rate-limit configuration.
    pub fn with_settings(interval_secs: f64, wait_steps: u32) -> Self {
        Self::from_pacing(DownloadPacing::new(
            Some(interval_secs),
            Some(i64::from(wait_steps)),
        ))
    }

    fn from_pacing(pacing: DownloadPacing) -> Self {
        Self { pacing }
    }

    /// Grant at the current instant, or return a delay for rechecking.
    fn try_acquire(&self, scope: &RateLimitScope) -> PacingDecision {
        let now_ms = monotonic_ms();
        let schedule = PacingSchedule::from_scoped(&self.pacing.for_scope(scope));
        let mut hosts = STATE.lock();
        if let Some(state) = hosts.get_mut(&scope.site) {
            return try_acquire(state, now_ms, schedule);
        }
        let mut state = PacingState::default();
        let decision = try_acquire(&mut state, now_ms, schedule);
        hosts.insert(scope.site.clone(), state);
        decision
    }
}

impl crate::platform::RateLimiter for RateLimiter {
    fn acquire<'a>(
        &'a self,
        scope: &'a RateLimitScope,
    ) -> futures::future::BoxFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            loop {
                match self.try_acquire(scope) {
                    PacingDecision::Granted => return Ok(()),
                    PacingDecision::Wait { wait_ms } => {
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                }
            }
        })
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// `download.interval` / `download.wait-steps` の保存値を共有規則で展開する。
fn load_download_pacing() -> DownloadPacing {
    let values = crate::db::settings::load(crate::setting_core::SettingScope::Local)
        .unwrap_or_default();
    DownloadPacing::from_settings(&values)
}

/// 許可エンジンの ms (プロセス開始からの経過)。
fn monotonic_ms() -> u64 {
    u64::try_from(MONOTONIC_BASE.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;

    use super::{RateLimiter, STATE};
    use crate::platform::{DownloadPacing, RateLimitScope};

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn reset_state() {
        STATE.lock().clear();
    }

    fn limiter_with_interval(interval_secs: f64) -> RateLimiter {
        RateLimiter::from_pacing(DownloadPacing::new(Some(interval_secs), Some(0)))
    }

    /// Exercise the same grant/recheck loop without a Tokio runtime.
    fn acquire_and_wait(limiter: &RateLimiter, scope: &RateLimitScope) {
        loop {
            match limiter.try_acquire(scope) {
                super::PacingDecision::Granted => return,
                super::PacingDecision::Wait { wait_ms } => {
                    thread::sleep(Duration::from_millis(wait_ms));
                }
            }
        }
    }

    #[test]
    fn site_interval_floor_applies_to_the_scope_only() {
        let _guard = TEST_MUTEX.lock();
        reset_state();
        let limiter = limiter_with_interval(0.7);
        let pixiv = RateLimitScope::site("interval-floor.example").with_min_interval(Some(5.0));
        let other = RateLimitScope::site("interval-floor-other.example");

        assert_eq!(limiter.try_acquire(&pixiv), super::PacingDecision::Granted);
        let super::PacingDecision::Wait { wait_ms } = limiter.try_acquire(&pixiv) else {
            panic!("site floor did not delay the second request");
        };
        assert!((4_950..=5_000).contains(&wait_ms), "got {wait_ms}");
        assert_eq!(limiter.try_acquire(&other), super::PacingDecision::Granted);
        let super::PacingDecision::Wait { wait_ms } = limiter.try_acquire(&other) else {
            panic!("global interval did not delay the second request");
        };
        assert!((600..=700).contains(&wait_ms), "got {wait_ms}");
    }


    #[test]
    fn same_host_requests_keep_their_reserved_order() {
        let _guard = TEST_MUTEX.lock();
        reset_state();

        let limiter = Arc::new(limiter_with_interval(0.08));
        acquire_and_wait(&limiter, &RateLimitScope::site("same.example"));

        let started = Instant::now();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let limiter = Arc::clone(&limiter);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                acquire_and_wait(&limiter, &RateLimitScope::site("same.example"));
                started.elapsed()
            }));
        }

        barrier.wait();

        let mut elapsed = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker should finish"))
            .collect::<Vec<_>>();
        elapsed.sort();

        assert!(
            elapsed[0] >= Duration::from_millis(60),
            "first wait was {:?}",
            elapsed[0]
        );
        assert!(
            elapsed[1] >= Duration::from_millis(140),
            "second wait was {:?}",
            elapsed[1]
        );

        reset_state();
    }

    #[test]
    fn different_hosts_do_not_block_each_other() {
        let _guard = TEST_MUTEX.lock();
        reset_state();

        let limiter = Arc::new(limiter_with_interval(0.08));
        acquire_and_wait(&limiter, &RateLimitScope::site("alpha.example"));

        let barrier = Arc::new(Barrier::new(2));
        let sleeping_worker = {
            let limiter = Arc::clone(&limiter);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                acquire_and_wait(&limiter, &RateLimitScope::site("alpha.example"));
            })
        };

        barrier.wait();
        thread::sleep(Duration::from_millis(10));

        let started = Instant::now();
        acquire_and_wait(&limiter, &RateLimitScope::site("beta.example"));
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(40),
            "different host waited {:?}",
            elapsed
        );

        sleeping_worker.join().expect("worker should finish");
        reset_state();
    }

    /// Mirrors the parallel-update scenario: several workers process
    /// the same host while other workers process unrelated hosts. We
    /// assert that the unrelated-host workers do not have to wait
    /// while a same-host burst is being drained. This is the key
    /// invariant that lets `update.max-parallel-domains > 1` honour
    /// the per-domain etiquette guarantee from the task spec.
    #[test]
    fn parallel_workers_on_different_domains_do_not_block() {
        let _guard = TEST_MUTEX.lock();
        reset_state();

        // Slow down same-host requests so the parallel worker has
        // a meaningful opportunity to outlive the wait it ignored.
        let limiter = Arc::new(limiter_with_interval(0.12));
        let started = Instant::now();
        acquire_and_wait(&limiter, &RateLimitScope::site("alpha.example"));
        let (alpha_waited, beta_waited, gamma_waited) = {
            let limiter_alpha = Arc::clone(&limiter);
            let limiter_beta = Arc::clone(&limiter);
            let limiter_gamma = Arc::clone(&limiter);
            let barrier = Arc::new(Barrier::new(4));
            let barrier_a = Arc::clone(&barrier);
            let barrier_b = Arc::clone(&barrier);
            let barrier_c = Arc::clone(&barrier);
            let barrier_main = Arc::clone(&barrier);

            let alpha_handle = thread::spawn(move || {
                barrier_a.wait();
                acquire_and_wait(&limiter_alpha, &RateLimitScope::site("alpha.example"));
                // Include thread/barrier startup since the priming request.
                started.elapsed()
            });
            let beta_handle = thread::spawn(move || {
                barrier_b.wait();
                let local = Instant::now();
                acquire_and_wait(&limiter_beta, &RateLimitScope::site("beta.example"));
                local.elapsed()
            });
            let gamma_handle = thread::spawn(move || {
                barrier_c.wait();
                let local = Instant::now();
                acquire_and_wait(&limiter_gamma, &RateLimitScope::site("gamma.example"));
                local.elapsed()
            });

            barrier_main.wait();
            // Give the other threads a moment to settle.
            thread::sleep(Duration::from_millis(5));
            // Take the priming-host slot again from the main thread
            // so another same-host caller has to wait for it.
            acquire_and_wait(&limiter, &RateLimitScope::site("delta.example"));

            (
                alpha_handle.join().expect("alpha"),
                beta_handle.join().expect("beta"),
                gamma_handle.join().expect("gamma"),
            )
        };

        // Same-host (alpha) worker had to wait for the slot that was
        // already taken by the priming call at the top of the test.
        assert!(
            alpha_waited + Duration::from_millis(20) >= Duration::from_millis(100),
            "alpha (same host) should wait at least ~120ms, got {:?}",
            alpha_waited
        );
        // Different-host (beta/gamma) workers must proceed without waiting on
        // alpha's slot. Compare against alpha's own wait instead of absolute
        // milliseconds: both are measured in this run, so a loaded machine
        // inflates them together and the discrimination survives.
        assert!(
            beta_waited * 2 < alpha_waited,
            "beta (different host) blocked unexpectedly: beta={beta_waited:?} alpha={alpha_waited:?}"
        );
        assert!(
            gamma_waited * 2 < alpha_waited,
            "gamma (different host) blocked unexpectedly: gamma={gamma_waited:?} alpha={alpha_waited:?}"
        );

        // Total runtime should be dominated by alpha's queued wait, not by
        // stacking all the cross-domain traffic behind it.
        let total = started.elapsed();
        assert!(
            total < alpha_waited * 3,
            "overall took too long: total={total:?} alpha={alpha_waited:?}"
        );
        reset_state();
    }
}
