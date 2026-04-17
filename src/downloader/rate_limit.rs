use std::time::{Duration, Instant};

use parking_lot::Mutex;

const DEFAULT_INTERVAL_SECS: f64 = 0.7;
const STEPS_WAIT_TIME: Duration = Duration::from_secs(5);

static STATE: Mutex<RateLimitState> = parking_lot::const_mutex(RateLimitState {
    counter: 0,
    last_download: None,
});

struct RateLimitState {
    counter: u32,
    last_download: Option<Instant>,
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

    pub fn wait(&self) {
        let mut state = STATE.lock();
        let now = Instant::now();

        if let Some(last) = state.last_download {
            let elapsed = now.duration_since(last);
            if elapsed > self.max_steps_wait_time {
                state.counter = 0;
            }
        }

        if self.wait_steps > 0
            && state.counter % self.wait_steps == 0
            && state.counter >= self.wait_steps
        {
            std::thread::sleep(self.max_steps_wait_time);
        } else if state.counter > 0 {
            std::thread::sleep(self.interval);
        }

        state.counter += 1;
        state.last_download = Some(Instant::now());
    }
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
    use super::{DEFAULT_INTERVAL_SECS, RateLimiter, normalize_wait_steps};

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
}
