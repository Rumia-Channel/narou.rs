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
//! ON/OFF はプロセス/isolate 全体のフラグ。native は設定読み込みの入口
//! (`commands::*` / Web ジョブ開始) で、Worker はジョブ開始と EPUB 生成の
//! 入口で [`set_enabled`] を呼ぶ。設定の解釈は [`enabled_in_settings`]。

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
    ENABLED.load(Ordering::Relaxed)
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
/// ので、解釈はここ 1 箇所に寄せる。真偽は narou.rb の設定と同じ扱いで、
/// `true`/`yes`/`on`/`1` と真偽値・数値 (0 以外) を受け付ける。
pub fn apply_settings(map: &std::collections::HashMap<String, serde_yaml::Value>) {
    set_enabled(enabled_in_settings(map));
}

/// `webui.debug-mode` の解釈 (未設定は OFF)。
pub fn enabled_in_settings(
    map: &std::collections::HashMap<String, serde_yaml::Value>,
) -> bool {
    match map.get("webui.debug-mode") {
        Some(serde_yaml::Value::Bool(value)) => *value,
        Some(serde_yaml::Value::String(value)) => truthy(value),
        Some(serde_yaml::Value::Number(value)) => value.as_i64().is_some_and(|value| value != 0),
        _ => false,
    }
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings(value: serde_yaml::Value) -> HashMap<String, serde_yaml::Value> {
        HashMap::from([("webui.debug-mode".to_string(), value)])
    }

    #[test]
    fn settings_accept_ruby_style_booleans() {
        for value in [
            serde_yaml::Value::Bool(true),
            serde_yaml::Value::String("true".into()),
            serde_yaml::Value::String("YES".into()),
            serde_yaml::Value::String(" on ".into()),
            serde_yaml::Value::Number(1.into()),
        ] {
            assert!(
                enabled_in_settings(&settings(value.clone())),
                "{value:?} は ON"
            );
        }
    }

    #[test]
    fn settings_treat_missing_and_falsey_values_as_off() {
        assert!(!enabled_in_settings(&HashMap::new()));
        for value in [
            serde_yaml::Value::Bool(false),
            serde_yaml::Value::String("false".into()),
            serde_yaml::Value::String("0".into()),
            serde_yaml::Value::Number(0.into()),
            serde_yaml::Value::Null,
        ] {
            assert!(
                !enabled_in_settings(&settings(value.clone())),
                "{value:?} は OFF"
            );
        }
    }

    #[test]
    fn emit_is_silent_until_enabled() {
        // 既定 sink とフラグはプロセス共通なので、他のテストと直列化する。
        let _global = crate::test_support::global_state_guard();
        // 既定 sink を差し替えて、行が流れるかどうかを観測する。
        struct Lines(std::sync::Mutex<Vec<String>>);
        impl crate::application::messages::MessageSink for Lines {
            fn emit(&self, _stream: Stream, text: &str) {
                self.0.lock().unwrap().push(text.to_string());
            }
        }
        let lines = std::sync::Arc::new(Lines(std::sync::Mutex::new(Vec::new())));
        crate::application::messages::set_default_sink(lines.clone());

        set_enabled(false);
        emit("hidden");
        assert!(lines.0.lock().unwrap().is_empty());

        set_enabled(true);
        emit("visible");
        emit_error("boom");
        let lines = lines.0.lock().unwrap().clone();
        assert_eq!(lines, vec!["[debug] visible", "[debug] Error: boom"]);

        // 後続テストへ状態を持ち越さない。
        set_enabled(false);
        crate::application::messages::take_default_sink();
    }
}
