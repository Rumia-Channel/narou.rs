use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use parking_lot::Mutex;

/// Prefix for structured WebSocket messages in stdout.
/// Lines starting with this prefix are intercepted by the web worker
/// and sent as WebSocket events instead of being echoed to the console.
pub const WS_LINE_PREFIX: &str = "__NAROU_WS__:";
pub const WEB_PROGRESS_SCOPE_ENV: &str = "NAROU_RS_WEB_PROGRESS_SCOPE";

/// Global mutex that serializes direct `println!` calls so parallel
/// workers (e.g. per-domain update workers) don't interleave bytes
/// mid-line. Holding this lock is cheap; the contention window is
/// one `println!` worth of text formatting + write.
///
/// Threads may either wrap their own call sites in
/// `let _g = STDOUT_LOCK.lock();` or use the [`safe_println`]
/// helper from this module to write a pre-formatted line atomically.
pub static STDOUT_LOCK: Mutex<()> = Mutex::new(());

/// Write a pre-formatted line to stdout while holding
/// [`STDOUT_LOCK`]. The argument must already be formatted (callers
/// usually do `&format!(...)`). This bypasses the project's
/// `println!` override (in `output_macros` which writes to the
/// logger) so the bytes reach the real stdout atomically with
/// respect to other parallel callers.
pub fn safe_println(line: &str) {
    use std::io::Write as _;
    let _guard = STDOUT_LOCK.lock();
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
}

/// Helper that serializes a pre-formatted line onto stderr. The
/// pattern uses a closure-flavoured API so callers building the
/// string with format args don't need to repeat the lock manually.
pub fn safe_stderr_println(line: &str) {
    use std::io::Write as _;
    let _guard = STDOUT_LOCK.lock();
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
}

/// Console-backed [`MessageSink`](crate::application::messages::MessageSink) — the default sink for native execution.
///
/// Line output (`emit`) takes [`STDOUT_LOCK`] so parallel workers don't
/// interleave bytes mid-line, then goes through the logger (`println!` /
/// `eprintln!` override) so messages also land in the log file exactly like
/// the pre-sink call sites did. `emit_fragment` is the `print!` equivalent
/// (interactive prompts) and writes without the trailing newline.
pub struct ConsoleSink;

impl crate::application::messages::MessageSink for ConsoleSink {
    fn emit(&self, stream: crate::application::messages::Stream, text: &str) {
        use crate::application::messages::Stream;
        let _guard = STDOUT_LOCK.lock();
        match stream {
            Stream::Stdout => crate::logger::emit_stdout(text, true),
            Stream::Stderr => crate::logger::emit_stderr(text, true),
        }
    }

    fn emit_fragment(&self, stream: crate::application::messages::Stream, text: &str) {
        use crate::application::messages::Stream;
        let _guard = STDOUT_LOCK.lock();
        match stream {
            Stream::Stdout => crate::logger::emit_stdout(text, false),
            Stream::Stderr => crate::logger::emit_stderr(text, false),
        }
    }
}

/// `Arc<dyn MessageSink>` で CLI コマンドへ渡す共有コンソール sink を作る。
///
/// native の送出先はこれ 1 本で、`Downloader` の report 行 (話ごとの進捗や
/// 「小説が削除されているか非公開な可能性があります」) も必ずこれを使う。
/// narou.rb は `$stdout` への write をすべてログにも追記する
/// (`lib/narou_logger.rb` の `append_log`) ため、logger を迂回する sink を
/// 併用すると「コンソールには出るがログに残らない行」ができてしまう。
/// `safe_println` 直結の sink が存在した時期にそれが起きている (issue #36)。
pub fn console_sink() -> Arc<dyn crate::application::messages::MessageSink> {
    Arc::new(ConsoleSink)
}

/// Check if running under the web server (subprocess mode)
pub fn is_web_mode() -> bool {
    std::env::var("NAROU_RS_WEB_MODE").is_ok()
}

/// Emit a structured WebSocket event from a CLI subprocess running under the
/// web server. Lines are intercepted by the parent and forwarded to clients.
/// No-op when not running under the web server.
pub fn emit_web_event(event_type: &str, data: serde_json::Value) {
    if !is_web_mode() {
        return;
    }
    let msg = serde_json::json!({ "type": event_type, "data": data });
    println!("{}{}", WS_LINE_PREFIX, msg);
}

/// Notify the parent web server that a single novel's DB record was just
/// updated (e.g. modified-tag removed, last_check_date refreshed). The parent
/// will reload its in-memory DB and broadcast a `table.reload` event so the
/// UI reflects the change immediately, without waiting for the whole job to
/// finish.
pub fn emit_novel_refresh(id: i64) {
    emit_web_event("novel.refresh", serde_json::json!({ "id": id }));
}

/// Portable progress sink used by the shared downloader/converter core.
/// Implementations live in this native module (terminal bars, WebSocket
/// events); the trait itself is defined in `platform::progress` so core
/// modules never depend on this native-only module.
pub use crate::platform::ProgressReporter;


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

/// Native stdout transport for the shared Web progress lifecycle.
pub struct WebProgress {
    state: crate::platform::progress::ProgressState,
}

static PROCESS_INSTANCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    hex::encode(crate::login::crypto::random_bytes::<16>().expect("progress instance entropy"))
});

impl WebProgress {
    pub fn new(topic: &str) -> Self {
        let progress = Self {
            state: crate::platform::progress::ProgressState::new(
                topic, &current_web_progress_scope(topic), &*PROCESS_INSTANCE,
            ),
        };
        progress.state.init(|phase, identity, snapshot| {
            progress.send(crate::application::push_events::progress_update(phase, identity, snapshot));
        });
        progress
    }

    fn send(&self, event: serde_json::Value) {
        use crate::application::messages::{MessageSink, Stream};
        ConsoleSink.emit(Stream::Stdout, &format!("{WS_LINE_PREFIX}{event}"));
    }
}

impl ProgressReporter for WebProgress {
    fn set_length(&self, len: u64) {
        self.state.set_length(len, |phase, identity, snapshot| {
            self.send(crate::application::push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn set_position(&self, pos: u64) {
        self.state.set_position(pos, |phase, identity, snapshot| {
            self.send(crate::application::push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn inc(&self, delta: u64) {
        self.state.inc(delta, |phase, identity, snapshot| {
            self.send(crate::application::push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn set_message(&self, msg: &str) {
        self.state.set_message(msg, |phase, identity, snapshot| {
            self.send(crate::application::push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn finish_with_message(&self, _msg: &str) {
        self.state.finish(|phase, identity, snapshot| {
            self.send(crate::application::push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn println(&self, msg: &str) {
        use crate::application::messages::{MessageSink, Stream};
        ConsoleSink.emit(Stream::Stdout, msg);
    }
}

impl Drop for WebProgress {
    fn drop(&mut self) {
        self.finish_with_message("");
    }
}

fn current_web_progress_scope(topic: &str) -> String {
    std::env::var(WEB_PROGRESS_SCOPE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| topic.to_string())
}

#[cfg(test)]
mod tests {
    use super::{WEB_PROGRESS_SCOPE_ENV, current_web_progress_scope};
    use std::sync::Mutex;

    /// 環境変数はプロセス全体で共有されるため、書き換えるテストは直列化する
    /// (並列実行だと片方の `remove_var` がもう片方の `set_var` を消してしまう)。
    static SCOPE_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_scope_env() -> std::sync::MutexGuard<'static, ()> {
        SCOPE_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn web_progress_scope_uses_env_override_when_present() {
        let _guard = lock_scope_env();
        unsafe { std::env::set_var(WEB_PROGRESS_SCOPE_ENV, "job-123"); }
        assert_eq!(current_web_progress_scope("convert"), "job-123");
        unsafe { std::env::remove_var(WEB_PROGRESS_SCOPE_ENV); }
    }

    #[test]
    fn web_progress_scope_falls_back_to_topic() {
        let _guard = lock_scope_env();
        unsafe { std::env::remove_var(WEB_PROGRESS_SCOPE_ENV); }
        assert_eq!(current_web_progress_scope("convert"), "convert");
    }

}
