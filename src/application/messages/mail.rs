//! `narou mail` と download/update の --mail 後処理で共有する文言。

use std::path::Path;

/// 設定ファイルの新規作成通知 (path 付き)。
pub fn mail_setting_created(path: &Path) -> String {
    format!("created {}", path.display())
}

/// 設定ファイルを作った直後の案内文。
pub fn mail_setting_file_notice() -> &'static str {
    "メールの設定用ファイルを作成しました。設定ファイルを書き換えることで mail コマンドが有効になります。"
}

/// 設定ファイル新規作成時だけ出す注意書き (mail コマンドと download の
/// after_process で使う。update の hotentry 経路はこの行を出さない)。
pub fn mail_setting_next_update_notice() -> &'static str {
    "注意：次回以降のupdateで新着があった場合に送信可能フラグが立ちます"
}

/// 設定ファイルの中身が未編集 (path 表示版 — download の after_process)。
pub fn mail_setting_incomplete(path: &Path) -> String {
    format!("設定ファイルの書き換えが終了していないようです。\n設定ファイルは {} にあります", path.display())
}

/// 同上、固定ファイル名版 (update の hotentry 経路は mail_setting.yaml 決め
/// 打ちだったのでそれを保持)。
pub fn mail_setting_incomplete_fixed() -> &'static str {
    "設定ファイルの書き換えが終了していないようです。\n設定ファイルは mail_setting.yaml にあります"
}

/// 中断時に本文として出す文言 (= `MAIL_INTERRUPTED_MESSAGE` の内容と一致)。
/// 実際の出力は `send_target_with_setting_interruptible` が Err に乗せた
/// 文字列なので、ここでは判定に使う定数として置く。
pub const MAIL_INTERRUPTED: &str = "メール送信を中断しました";

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn fixed_literals_match_native() {
        assert_eq!(
            super::mail_setting_file_notice(),
            "メールの設定用ファイルを作成しました。設定ファイルを書き換えることで mail コマンドが有効になります。"
        );
        assert_eq!(
            super::mail_setting_next_update_notice(),
            "注意：次回以降のupdateで新着があった場合に送信可能フラグが立ちます"
        );
        assert_eq!(
            super::mail_setting_incomplete_fixed(),
            "設定ファイルの書き換えが終了していないようです。\n設定ファイルは mail_setting.yaml にあります"
        );
    }

    #[test]
    fn lines_match_native() {
        assert_eq!(
            super::mail_setting_created(Path::new("/tmp/mail_setting.yaml")),
            "created /tmp/mail_setting.yaml"
        );
        assert_eq!(
            super::mail_setting_incomplete(Path::new("/tmp/mail_setting.yaml")),
            "設定ファイルの書き換えが終了していないようです。\n設定ファイルは /tmp/mail_setting.yaml にあります"
        );
        assert_eq!(
            super::MAIL_INTERRUPTED,
            narou_rs::mail::MAIL_INTERRUPTED_MESSAGE
        );
    }
}
