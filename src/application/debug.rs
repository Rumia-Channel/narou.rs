//! `webui.debug-mode` のときだけ流す詳細ログ。
//!
//! 挿絵の取り込みや EPUB への挿入のように「失敗しても処理は続く」箇所は、
//! 既定では黙って結果だけが変わる (挿絵が入らない EPUB ができる等) ため、
//! 原因を追えない。`webui.debug-mode` が ON のときは、そうした内部判断を
//! ユーザーに見える行として流す。
//!
//! 送り先は [`messages::emit_default`] — つまり native はコンソール (Web UI
//! 経由なら Web コンソール)、Worker は PushHub の `echo` なので、CLI と
//! Cloudflare 版のどちらでも同じ行が同じ場所に出る。行頭の [`PREFIX`] で
//! 通常の出力と区別できるようにし、Web UI はこの印を見てブラウザの
//! 開発者コンソールへも転送する (`main.js` の `appendConsole`)。
//!
//! native CLI の既定値はプロセスのフラグ、並行ジョブ・EPUB 要求の値は
//! [`messages::MessageContext`] のスコープで保持する。設定の解釈は
//! [`enabled_in_settings`]。

use std::sync::atomic::{AtomicBool, Ordering};

use super::messages::{self, Stream};

/// デバッグ行の目印。Web UI はこの接頭辞でブラウザの開発者コンソールへ
/// 転送し、native のユーザーも通常の出力と見分けられる。
pub const PREFIX: &str = "[debug]";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// 詳細ログの ON/OFF を切り替える (`webui.debug-mode`)。
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// 詳細ログが有効か。文字列を組み立てる前に確認するために使う。
pub fn enabled() -> bool {
    messages::scoped_debug_enabled().unwrap_or_else(|| ENABLED.load(Ordering::Relaxed))
}

/// 詳細ログを 1 行流す (無効なら何もしない)。`stdout` 側へ出す。
pub fn emit(message: impl std::fmt::Display) {
    if !enabled() {
        return;
    }
    messages::emit_default(Stream::Stdout, &format!("{PREFIX} {message}"));
}

/// 失敗の詳細を 1 行流す (無効なら何もしない)。
pub fn emit_error(message: impl std::fmt::Display) {
    emit(format_args!("Error: {message}"));
}

/// local 設定マップから `webui.debug-mode` を読んで反映する。
///
/// native の設定ストアと Worker の D1 設定はどちらも「設定マップ」を渡せる
/// ので、解釈は [`enabled_in_settings`] に寄せる。真偽は共有の
/// [`crate::setting_core::setting_bool_value`] と同じ規則
/// (`true`/`yes`/`on`/`1` と真偽値・数値 (0 以外)、未設定は OFF)。
pub fn apply_settings(map: &std::collections::HashMap<String, serde_yaml::Value>) {
    set_enabled(enabled_in_settings(map));
}

/// `webui.debug-mode` の解釈 (未設定は OFF)。真偽の解釈は共有の
/// [`crate::setting_core::setting_bool_value`] に一本化してある。
pub fn enabled_in_settings(
    map: &std::collections::HashMap<String, serde_yaml::Value>,
) -> bool {
    crate::setting_core::bool_or(map, "webui.debug-mode", false)
}

