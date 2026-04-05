use std::time::{Duration, Instant};

use parking_lot::Mutex;

const DEFAULT_INTERVAL: Duration = Duration::from_millis(700);
const STEPS_WAIT_TIME: Duration = Duration::from_secs(5);
const DEFAULT_WAIT_STEPS: u32 = 10;
const MAX_STEPS_WAIT_TIME: Duration = Duration::from_secs(30);

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
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            wait_steps: DEFAULT_WAIT_STEPS,
        }
    }

    pub fn with_interval(interval_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(interval_ms),
            wait_steps: DEFAULT_WAIT_STEPS,
        }
    }

    pub fn wait(&self) {
        let mut state = STATE.lock();
        let now = Instant::now();

        if let Some(last) = state.last_download {
            let elapsed = now.duration_since(last);
            if elapsed > MAX_STEPS_WAIT_TIME {
                state.counter = 0;
            }
        }

        if state.counter > 0 {
            if state.counter % self.wait_steps == 0 && state.counter >= self.wait_steps {
                std::thread::sleep(STEPS_WAIT_TIME);
            } else {
                std::thread::sleep(self.interval);
            }
        }

        state.counter += 1;
        state.last_download = Some(Instant::now());
    }
}
