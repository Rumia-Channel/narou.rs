//! ユーザーに見えるメッセージの集約層。
//!
//! native の CLI コマンド (`src/commands/**`) とライブラリ実行経路
//! (`src/downloader/**` の進捗・警告) が出す文言を、値を受け取って完成済み
//! 文字列を返す純関数としてここに集約する。送り先は [`MessageSink`] port で
//! 差し替え、文言の所有者を実行環境から分離する:
//!
//! - native … `src/progress.rs` のコンソール sink (現行の `println!` /
//!   `eprintln!` / `safe_println` と等価な stdout / stderr 書き込み)。
//! - Worker … PushHub へ `echo` イベントとして送る sink
//!   (`worker_entry::push_hub`)。native の Web UI は子プロセスの出力をその
//!   まま echo するので、ここの文言が両環境で一致する。
//!
pub mod convert;
pub mod author;
pub mod download;
pub mod flush;
pub mod jobs;
pub mod mail;
pub mod send;
pub mod update;

use std::fmt::Display;

/// メッセージの送り先ストリーム。native の `println!` (= stdout) と
/// `eprintln!` (= stderr) の区別に対応する。
///
/// Worker では [`Self::target_console`] が PushHub の `echo` イベントの
/// `target_console` に直結し、native の Web UI で stdout 行が "stdout"、
/// stderr 行が別コンソール ("stdout2") へ流れるのと同じ分離になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    /// Web UI (`main.js` の `appendConsole`) が解釈する宛先コンソール名。
    pub fn target_console(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stdout2",
        }
    }

    /// `stdout` 行の宛先を上書きして解決する。
    ///
    /// native の `web::worker::console_target_for_job` は convert/send/backup/mail
    /// (外部通信を持たないジョブ) の行を、`concurrency` 有効時に `stdout2` へ
    /// 寄せる。Worker の `PushHubSink` も同じ上書きを使うので、規則をここ 1 箇所に
    /// 置く (`stderr` 行は元から `stdout2` なので上書きしない)。
    pub fn target_console_with(self, override_console: Option<&'static str>) -> &'static str {
        match (override_console, self) {
            (_, Stream::Stderr) => "stdout2",
            (Some(console), Stream::Stdout) => console,
            (None, Stream::Stdout) => "stdout",
        }
    }
}

/// メッセージの送出先 port。
///
/// `emit` は `println!` / `eprintln!` 相当の「行」送出で、sink が改行を
/// 付ける (Worker では 1 行ごとに `echo` イベントへ分ける)。
/// `emit_fragment` は `print!` 相当の改行なし断片 (対話プロンプト等)。
/// 部分行を扱えない環境向けに既定では行として送る。
pub trait MessageSink: Send + Sync {
    fn emit(&self, stream: Stream, text: &str);

    fn emit_fragment(&self, stream: Stream, text: &str) {
        self.emit(stream, text);
    }
}

/// Message routing and debug visibility for one invocation, not one process.
/// A scope is installed only while its future is being polled; suspension,
/// cancellation and unwinding cannot leak it into another job.
#[derive(Clone)]
pub struct MessageContext {
    sink: std::sync::Arc<dyn MessageSink>,
    debug_enabled: bool,
}

thread_local! {
    static ACTIVE_CONTEXT: std::cell::RefCell<Option<MessageContext>> =
        const { std::cell::RefCell::new(None) };
}

struct ContextScope<'a> {
    previous: Option<MessageContext>,
    owner: &'a mut Option<MessageContext>,
}

impl<'a> ContextScope<'a> {
    fn enter(owner: &'a mut Option<MessageContext>) -> Self {
        let previous = ACTIVE_CONTEXT.with(|slot| slot.replace(owner.take()));
        Self { previous, owner }
    }
}

impl Drop for ContextScope<'_> {
    fn drop(&mut self) {
        *self.owner = ACTIVE_CONTEXT.with(|slot| slot.replace(self.previous.take()));
    }
}

impl MessageContext {
    pub fn new(sink: std::sync::Arc<dyn MessageSink>, debug_enabled: bool) -> Self {
        Self { sink, debug_enabled }
    }

    /// Explicitly capture context when spawning onto another task or thread.
    pub fn current() -> Option<Self> {
        ACTIVE_CONTEXT.with(|slot| slot.borrow().clone())
    }

    pub fn run<T>(self, operation: impl FnOnce() -> T) -> T {
        let mut owner = Some(self);
        let _scope = ContextScope::enter(&mut owner);
        operation()
    }

    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        let mut owner = Some(self);
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| {
            let _scope = ContextScope::enter(&mut owner);
            future.as_mut().poll(cx)
        }).await
    }
}

pub(super) fn scoped_debug_enabled() -> Option<bool> {
    ACTIVE_CONTEXT.with(|slot| slot.borrow().as_ref().map(|context| context.debug_enabled))
}

/// Deep warnings use the current invocation's sink. Native CLI output and
/// out-of-invocation Worker diagnostics retain their console fallback.
pub fn emit_default(stream: Stream, text: &str) {
    let sink = ACTIVE_CONTEXT.with(|slot| {
        slot.borrow().as_ref().map(|context| context.sink.clone())
    });
    match sink {
        Some(sink) => sink.emit(stream, text),
        None => match stream {
            Stream::Stdout => println!("{text}"),
            Stream::Stderr => eprintln!("{text}"),
        },
    }
}

// ---------------------------------------------------------------------------
// 複数コマンドで使い回す共通文言。command 固有のものは各サブモジュールへ。
// ---------------------------------------------------------------------------

/// 対象ごとの区切り線 (`"\u{2015}".repeat(35)`、download / update / convert で
/// 共通。update の hotentry 開始時にも同じ線を引く)。
pub fn separator() -> String {
    "\u{2015}".repeat(35)
}

/// `eprintln!("Error: {}", e)` 系。
pub fn error_line(error: impl Display) -> String {
    format!("Error: {error}")
}

/// `println!("  Error: {}", e)` 系 (download の小説ごとの失敗など、
/// エラーを字下げして出す箇所)。
pub fn indented_error(error: impl Display) -> String {
    format!("  Error: {error}")
}

/// データベース初期化失敗 (`eprintln!("Error initializing database: {}", e)`)。
pub fn init_db_error(error: impl Display) -> String {
    format!("Error initializing database: {error}")
}

/// Downloader 構築失敗 (`eprintln!("Error creating downloader: {}", e)`)。
pub fn downloader_create_error(error: impl Display) -> String {
    format!("Error creating downloader: {error}")
}

/// 「指定された小説が存在しない」汎用文言 (convert / send / mail で共用)。
pub fn target_missing(target: impl Display) -> String {
    format!("{target} は存在しません")
}

/// 更新無し (download / update で共用)。
pub fn no_update(title: impl Display) -> String {
    format!("{title} に更新はありません")
}

/// 「◯◯ へコピーしました」(update の hotentry / convert / send で共用)。
pub fn copied_to(path: impl Display) -> String {
    format!("{path} へコピーしました")
}

/// コピー先フォルダが無い / 消えたときの文言 (update の hotentry / send で
/// 共用。convert の ZIP 版は末尾が違うので `convert::zip_copy_dest_not_dir`)。
pub fn copy_dest_not_dir(dir: impl Display) -> String {
    format!("{dir} はフォルダではないかすでに削除されています。コピー出来ませんでした")
}

/// 新規ダウンロード完了 (download / update で同一文言)。
pub fn dl_completed_new(
    title: impl Display,
    id: impl Display,
    total_count: impl Display,
) -> String {
    format!("{title} のDL完了 (ID:{id}, {total_count}セクション)")
}

/// 「{}へ送信しています」(update の hotentry / send で共用)。
pub fn sending_to(device: impl Display) -> String {
    format!("{device}へ送信しています")
}

/// コピー先の端末が見つからなかった (update / send で共用)。
pub fn copy_device_missing(device: impl Display) -> String {
    format!("{device}が見つからなかったためコピー出来ませんでした")
}

#[cfg(test)]
mod tests {
    use super::{MessageContext, MessageSink, Stream};

    #[test]
    fn stream_maps_to_web_console_names() {
        assert_eq!(Stream::Stdout.target_console(), "stdout");
        assert_eq!(Stream::Stderr.target_console(), "stdout2");
    }

    /// `concurrency` 有効時の上書き (native `console_target_for_job` 相当)。
    #[test]
    fn stream_target_console_can_be_overridden_for_local_jobs() {
        assert_eq!(Stream::Stdout.target_console_with(None), "stdout");
        assert_eq!(Stream::Stdout.target_console_with(Some("stdout2")), "stdout2");
        // stderr は元から別コンソールなので上書きしない。
        assert_eq!(Stream::Stderr.target_console_with(None), "stdout2");
        assert_eq!(Stream::Stderr.target_console_with(Some("stdout2")), "stdout2");
    }

    struct Collect(parking_lot::Mutex<Vec<(Stream, String)>>);

    impl MessageSink for Collect {
        fn emit(&self, stream: Stream, text: &str) {
            self.0.lock().push((stream, text.to_string()));
        }
    }

    #[test]
    fn interleaved_invocations_keep_their_output_and_debug_visibility() {
        use std::future::poll_fn;
        use std::task::{Context, Poll, Waker};
        let first = std::sync::Arc::new(Collect(parking_lot::Mutex::new(Vec::new())));
        let second = std::sync::Arc::new(Collect(parking_lot::Mutex::new(Vec::new())));
        let outer = std::sync::Arc::new(Collect(parking_lot::Mutex::new(Vec::new())));
        let operation = |name| async move {
            super::emit_default(Stream::Stdout, name);
            let mut suspended = false;
            poll_fn(|_| {
                if std::mem::replace(&mut suspended, true) { Poll::Ready(()) }
                else { Poll::Pending }
            }).await;
            crate::application::debug::emit(name);
        };
        let mut a = std::pin::pin!(MessageContext::new(first.clone(), true).scope(operation("first")));
        let mut b = std::pin::pin!(MessageContext::new(second.clone(), false).scope(operation("second")));
        MessageContext::new(outer.clone(), true).run(|| {
            let mut cx = Context::from_waker(Waker::noop());
            assert!(a.as_mut().poll(&mut cx).is_pending());
            assert!(b.as_mut().poll(&mut cx).is_pending());
            assert!(crate::application::debug::enabled());
            assert!(b.as_mut().poll(&mut cx).is_ready());
            assert!(a.as_mut().poll(&mut cx).is_ready());
            super::emit_default(Stream::Stderr, "outer");
        });
        assert_eq!(*first.0.lock(), vec![
            (Stream::Stdout, "first".into()), (Stream::Stdout, "[debug] first".into()),
        ]);
        assert_eq!(*second.0.lock(), vec![(Stream::Stdout, "second".into())]);
        assert_eq!(*outer.0.lock(), vec![(Stream::Stderr, "outer".into())]);
    }

    #[test]
    fn unwinding_an_inner_invocation_restores_the_outer_sink() {
        let outer = std::sync::Arc::new(Collect(parking_lot::Mutex::new(Vec::new())));
        let inner = std::sync::Arc::new(Collect(parking_lot::Mutex::new(Vec::new())));
        MessageContext::new(outer.clone(), true).run(|| {
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                MessageContext::new(inner.clone(), false).run(|| panic!("request failed"));
            }));
            assert!(failed.is_err());
            crate::application::debug::emit("outer survived");
        });
        assert_eq!(*outer.0.lock(), vec![(Stream::Stdout, "[debug] outer survived".into())]);
        assert!(inner.0.lock().is_empty());
    }
}
