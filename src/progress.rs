use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Prefix for structured WebSocket messages in stdout.
/// Lines starting with this prefix are intercepted by the web worker
/// and sent as WebSocket events instead of being echoed to the console.
pub const WS_LINE_PREFIX: &str = "__NAROU_WS__:";

/// Check if running under the web server (subprocess mode)
pub fn is_web_mode() -> bool {
    std::env::var("NAROU_RS_WEB_MODE").is_ok()
}

pub trait ProgressReporter: Send + Sync {
    fn set_length(&self, len: u64);
    fn set_position(&self, pos: u64);
    fn inc(&self, delta: u64);
    fn set_message(&self, msg: &str);
    fn finish_with_message(&self, msg: &str);
    fn println(&self, msg: &str);
}

pub struct NoProgress;

impl ProgressReporter for NoProgress {
    fn set_length(&self, _len: u64) {}
    fn set_position(&self, _pos: u64) {}
    fn inc(&self, _delta: u64) {}
    fn set_message(&self, _msg: &str) {}
    fn finish_with_message(&self, _msg: &str) {}
    fn println(&self, msg: &str) {
        eprintln!("{}", msg);
    }
}

pub struct CliProgress {
    pb: ProgressBar,
    multi: Option<Arc<MultiProgress>>,
}

impl CliProgress {
    pub fn new(msg: &str) -> Self {
        let pb = ProgressBar::new(0);
        pb.set_style(
            ProgressStyle::with_template(
                "{msg} {spinner:.green} [{wide_bar:.cyan/blue}] {pos}/{len}",
            )
            .unwrap()
            .progress_chars("█▓░"),
        );
        pb.set_message(msg.to_string());
        Self { pb, multi: None }
    }

    pub fn with_multi(msg: &str, multi: Arc<MultiProgress>) -> Self {
        let pb = multi.add(ProgressBar::new(0));
        pb.set_style(
            ProgressStyle::with_template(
                "{msg} {spinner:.green} [{wide_bar:.cyan/blue}] {pos}/{len}",
            )
            .unwrap()
            .progress_chars("█▓░"),
        );
        pb.set_message(msg.to_string());
        Self {
            pb,
            multi: Some(multi),
        }
    }

    pub fn new_spinner(msg: &str) -> Self {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{msg} {spinner:.green} {pos}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.set_message(msg.to_string());
        Self { pb, multi: None }
    }

    pub fn with_multi_spinner(msg: &str, multi: Arc<MultiProgress>) -> Self {
        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{msg} {spinner:.green} {pos}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.set_message(msg.to_string());
        Self {
            pb,
            multi: Some(multi),
        }
    }

    pub fn multi() -> Arc<MultiProgress> {
        Arc::new(MultiProgress::new())
    }
}

impl ProgressReporter for CliProgress {
    fn set_length(&self, len: u64) {
        self.pb.set_length(len);
        self.pb
            .enable_steady_tick(std::time::Duration::from_millis(100));
    }

    fn set_position(&self, pos: u64) {
        self.pb.set_position(pos);
    }

    fn inc(&self, delta: u64) {
        self.pb.inc(delta);
    }

    fn set_message(&self, msg: &str) {
        self.pb.set_message(msg.to_string());
    }

    fn finish_with_message(&self, msg: &str) {
        self.pb.finish_with_message(msg.to_string());
    }

    fn println(&self, msg: &str) {
        if let Some(ref multi) = self.multi {
            let _ = multi.println(msg);
        } else {
            self.pb.println(msg);
        }
    }
}

impl Drop for CliProgress {
    fn drop(&mut self) {
        self.pb.finish_and_clear();
    }
}

/// Progress reporter for web mode — outputs structured lines to stdout
/// that the web worker intercepts and converts to WebSocket events.
pub struct WebProgress {
    topic: String,
    length: AtomicU64,
    position: AtomicU64,
}

impl WebProgress {
    pub fn new(topic: &str) -> Self {
        let wp = Self {
            topic: topic.to_string(),
            length: AtomicU64::new(0),
            position: AtomicU64::new(0),
        };
        wp.send("progressbar.init", serde_json::json!({ "topic": topic }));
        wp
    }

    fn send(&self, event_type: &str, data: serde_json::Value) {
        let msg = serde_json::json!({ "type": event_type, "data": data });
        println!("{}{}", WS_LINE_PREFIX, msg);
    }

    fn emit_step(&self) {
        let len = self.length.load(Ordering::Relaxed);
        let pos = self.position.load(Ordering::Relaxed);
        if len > 0 {
            let percent = (pos as f64 / len as f64) * 100.0;
            self.send(
                "progressbar.step",
                serde_json::json!({ "percent": percent, "topic": self.topic }),
            );
        }
    }
}

impl ProgressReporter for WebProgress {
    fn set_length(&self, len: u64) {
        self.length.store(len, Ordering::Relaxed);
    }

    fn set_position(&self, pos: u64) {
        self.position.store(pos, Ordering::Relaxed);
        self.emit_step();
    }

    fn inc(&self, delta: u64) {
        self.position.fetch_add(delta, Ordering::Relaxed);
        self.emit_step();
    }

    fn set_message(&self, _msg: &str) {
        // Web mode doesn't display message updates (progress bar only)
    }

    fn finish_with_message(&self, _msg: &str) {
        self.send(
            "progressbar.clear",
            serde_json::json!({ "topic": self.topic }),
        );
    }

    fn println(&self, msg: &str) {
        println!("{}", msg);
    }
}

impl Drop for WebProgress {
    fn drop(&mut self) {
        self.send(
            "progressbar.clear",
            serde_json::json!({ "topic": self.topic }),
        );
    }
}
