use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const DEFAULT_INTERVAL_SECS: f64 = 0.7;
const STEPS_WAIT_TIME: Duration = Duration::from_secs(5);
const GLOBAL_HOST_KEY: &str = "__global__";

static STATE: LazyLock<Mutex<RateLimitState>> =
    LazyLock::new(|| Mutex::new(RateLimitState::default()));

#[derive(Default)]
struct RateLimitState {
    hosts: HashMap<String, HostRateLimitState>,
}

#[derive(Default)]
struct HostRateLimitState {
    counter: u32,
    last_download: Option<Instant>,
    next_allowed: Option<Instant>,
}

pub struct RateLimiter {
    interval: Duration,
    wait_steps: u32,
    max_steps_wait_time: Duration,
}

impl RateLimiter {
    pub fn new(is_narou: bool) -> Self {
        let interval_secs = load_interval_secs();
        let wait_steps = load_wait_steps(is_narou);
        Self::from_values(interval_secs, wait_steps)
    }

    fn from_values(interval_secs: f64, wait_steps: u32) -> Self {
        let interval = Duration::from_secs_f64(interval_secs.max(0.0));
        Self {
            interval,
            wait_steps,
            max_steps_wait_time: STEPS_WAIT_TIME.max(interval),
        }
    }

    /// Build a rate limiter with explicit interval/wait-steps, bypassing the
    /// default `download.interval` / `download.wait-steps` settings.
    /// Used by code paths (e.g. なろうAPI) that have their own dedicated
    /// rate-limit configuration.
    pub fn with_settings(interval_secs: f64, wait_steps: u32) -> Self {
        Self::from_values(interval_secs, wait_steps)
    }

    pub fn wait(&self) {
        self.wait_for_host(GLOBAL_HOST_KEY);
    }

    pub fn wait_for_url(&self, url: &str) {
        self.wait_for_host(&host_key_from_url(url));
    }

    pub async fn wait_async_for_url(&self, url: &str) {
        let duration = self.reserve_wait_duration(&host_key_from_url(url));
        if !duration.is_zero() {
            tokio::time::sleep(duration).await;
        }
    }

    fn wait_for_host(&self, host: &str) {
        let duration = self.reserve_wait_duration(host);
        if !duration.is_zero() {
            std::thread::sleep(duration);
        }
    }

    fn reserve_wait_duration(&self, host: &str) -> Duration {
        let now = Instant::now();
        let mut state = STATE.lock();
        let host_state = state.hosts.entry(host.to_string()).or_default();

        let no_pending_slot = host_state
            .next_allowed
            .map(|next_allowed| now >= next_allowed)
            .unwrap_or(true);
        if let Some(last_download) = host_state.last_download {
            let elapsed = now.checked_duration_since(last_download).unwrap_or_default();
            if elapsed > self.max_steps_wait_time && no_pending_slot {
                host_state.counter = 0;
                host_state.last_download = None;
                host_state.next_allowed = None;
            }
        }

        let allowed_at = host_state
            .next_allowed
            .map(|next_allowed| next_allowed.max(now))
            .unwrap_or(now);

        host_state.counter += 1;
        host_state.last_download = Some(allowed_at);
        host_state.next_allowed = Some(allowed_at + self.delay_after_request(host_state.counter));

        allowed_at.checked_duration_since(now).unwrap_or_default()
    }

    fn delay_after_request(&self, counter: u32) -> Duration {
        if self.wait_steps > 0 && counter % self.wait_steps == 0 && counter >= self.wait_steps {
            self.max_steps_wait_time
        } else if counter > 0 {
            self.interval
        } else {
            Duration::ZERO
        }
    }
}

fn host_key_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| GLOBAL_HOST_KEY.to_string())
}

fn load_interval_secs() -> f64 {
    crate::compat::load_local_setting_value("download.interval")
        .and_then(|value| match value {
            serde_yaml::Value::Number(number) => number.as_f64(),
            serde_yaml::Value::String(raw) => raw.parse::<f64>().ok(),
            _ => None,
        })
        .unwrap_or(DEFAULT_INTERVAL_SECS)
        .max(0.0)
}

fn load_wait_steps(is_narou: bool) -> u32 {
    let raw_wait_steps = crate::compat::load_local_setting_value("download.wait-steps")
        .and_then(|value| match value {
            serde_yaml::Value::Number(number) => number.as_i64(),
            serde_yaml::Value::String(raw) => raw.parse::<i64>().ok(),
            _ => None,
        })
        .unwrap_or(0);
    normalize_wait_steps(raw_wait_steps, is_narou)
}

fn normalize_wait_steps(raw_wait_steps: i64, is_narou: bool) -> u32 {
    let wait_steps = if raw_wait_steps > 0 {
        raw_wait_steps as u32
    } else {
        0
    };
    if is_narou && (wait_steps == 0 || wait_steps > 10) {
        10
    } else {
        wait_steps
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;

    use super::{DEFAULT_INTERVAL_SECS, RateLimiter, STATE, normalize_wait_steps};

    static TEST_MUTEX: Mutex<()> = parking_lot::const_mutex(());

    fn reset_state() {
        STATE.lock().hosts.clear();
    }

    #[test]
    fn narou_wait_steps_defaults_to_ten() {
        assert_eq!(normalize_wait_steps(0, true), 10);
    }

    #[test]
    fn non_narou_wait_steps_defaults_to_zero() {
        assert_eq!(normalize_wait_steps(0, false), 0);
    }

    #[test]
    fn narou_wait_steps_are_capped_to_ten() {
        assert_eq!(normalize_wait_steps(50, true), 10);
    }

    #[test]
    fn non_narou_wait_steps_are_preserved() {
        assert_eq!(normalize_wait_steps(50, false), 50);
    }

    #[test]
    fn interval_lower_than_zero_is_clamped() {
        let limiter = RateLimiter::from_values(-1.0, 0);
        assert_eq!(limiter.interval, std::time::Duration::from_secs(0));
        assert_eq!(limiter.max_steps_wait_time, std::time::Duration::from_secs(5));
    }

    #[test]
    fn default_interval_uses_ruby_compatible_value() {
        let limiter = RateLimiter::from_values(DEFAULT_INTERVAL_SECS, 0);
        assert_eq!(limiter.interval, std::time::Duration::from_millis(700));
        assert_eq!(limiter.max_steps_wait_time, std::time::Duration::from_secs(5));
    }

    #[test]
    fn same_host_requests_keep_their_reserved_order() {
        let _guard = TEST_MUTEX.lock();
        reset_state();

        let limiter = Arc::new(RateLimiter::from_values(0.08, 0));
        limiter.wait_for_host("same.example");

        let started = Instant::now();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let limiter = Arc::clone(&limiter);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                limiter.wait_for_host("same.example");
                started.elapsed()
            }));
        }

        barrier.wait();

        let mut elapsed = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker should finish"))
            .collect::<Vec<_>>();
        elapsed.sort();

        assert!(elapsed[0] >= Duration::from_millis(60), "first wait was {:?}", elapsed[0]);
        assert!(elapsed[1] >= Duration::from_millis(140), "second wait was {:?}", elapsed[1]);

        reset_state();
    }

    #[test]
    fn different_hosts_do_not_block_each_other() {
        let _guard = TEST_MUTEX.lock();
        reset_state();

        let limiter = Arc::new(RateLimiter::from_values(0.08, 0));
        limiter.wait_for_host("alpha.example");

        let barrier = Arc::new(Barrier::new(2));
        let sleeping_worker = {
            let limiter = Arc::clone(&limiter);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                limiter.wait_for_host("alpha.example");
            })
        };

        barrier.wait();
        thread::sleep(Duration::from_millis(10));

        let started = Instant::now();
        limiter.wait_for_host("beta.example");
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_millis(40), "different host waited {:?}", elapsed);

        sleeping_worker.join().expect("worker should finish");
        reset_state();
    }
}
