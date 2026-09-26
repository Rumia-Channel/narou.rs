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
//! **文言を変更するなら必ず対応する関数のテストを更新すること。** 関数の
//! 戻り値は移行前の `format!` / リテラルと 1 バイトも違わないのが契約。

pub mod convert;
pub mod download;
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

// ---------------------------------------------------------------------------
// 既定 sink (Downloader の深い呼び出しなど、sink を引き回せない箇所向け)。
// ---------------------------------------------------------------------------

static DEFAULT_SINK: std::sync::LazyLock<
    std::sync::RwLock<Option<std::sync::Arc<dyn MessageSink>>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

/// プロセス共通の既定 sink を差し替える。Worker のジョブ実行が PushHub への
/// `echo` をインストールするために使う。native は呼ばなくてよい
/// (フォールバックが `println!`/`eprintln!` で従来動作と一致するため)。
pub fn set_default_sink(sink: std::sync::Arc<dyn MessageSink>) {
    if let Ok(mut slot) = DEFAULT_SINK.write() {
        *slot = Some(sink);
    }
}

/// 既定 sink 経由で 1 行出す。未設定の場合は `println!`/`eprintln!`
/// (native では logger 経由の従来動作、Worker ではコンソールログ) に
/// フォールバックする。
pub fn emit_default(stream: Stream, text: &str) {
    let sink = DEFAULT_SINK
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
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
    use super::{MessageSink, Stream};

    #[test]
    fn stream_maps_to_web_console_names() {
        assert_eq!(Stream::Stdout.target_console(), "stdout");
        assert_eq!(Stream::Stderr.target_console(), "stdout2");
    }

    #[test]
    fn separator_matches_ruby_rule() {
        assert_eq!(super::separator(), "―".repeat(35));
        assert_eq!(super::separator().chars().count(), 35);
    }

    #[test]
    fn shared_lines_match_native_formats() {
        assert_eq!(super::error_line("boom"), "Error: boom");
        assert_eq!(super::indented_error("boom"), "  Error: boom");
        assert_eq!(
            super::init_db_error("io"),
            "Error initializing database: io"
        );
        assert_eq!(
            super::downloader_create_error("net"),
            "Error creating downloader: net"
        );
        assert_eq!(super::target_missing("n1234"), "n1234 は存在しません");
        assert_eq!(super::no_update("タイトル"), "タイトル に更新はありません");
        assert_eq!(super::copied_to("/tmp/x.txt"), "/tmp/x.txt へコピーしました");
        assert_eq!(
            super::copy_dest_not_dir("/out"),
            "/out はフォルダではないかすでに削除されています。コピー出来ませんでした"
        );
        assert_eq!(
            super::dl_completed_new("タイトル", 42, 12),
            "タイトル のDL完了 (ID:42, 12セクション)"
        );
        assert_eq!(super::sending_to("kindle"), "kindleへ送信しています");
        assert_eq!(
            super::copy_device_missing("kobo"),
            "koboが見つからなかったためコピー出来ませんでした"
        );
    }

    struct Collect(parking_lot::Mutex<Vec<(Stream, String)>>);

    impl MessageSink for Collect {
        fn emit(&self, stream: Stream, text: &str) {
            self.0.lock().push((stream, text.to_string()));
        }
    }

    #[test]
    fn default_emit_fragment_delegates_to_emit() {
        let sink = Collect(parking_lot::Mutex::new(Vec::new()));
        sink.emit_fragment(Stream::Stderr, "fragment");
        let lines = sink.0.lock();
        assert_eq!(
            lines.as_slice(),
            &[(Stream::Stderr, "fragment".to_string())]
        );
    }
}
