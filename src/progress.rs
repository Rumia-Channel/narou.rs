use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub trait ProgressReporter: Send + Sync {
    fn set_length(&self, len: u64);
    fn inc(&self, delta: u64);
    fn set_message(&self, msg: &str);
    fn finish_with_message(&self, msg: &str);
    fn println(&self, msg: &str);
}

pub struct NoProgress;

impl ProgressReporter for NoProgress {
    fn set_length(&self, _len: u64) {}
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
