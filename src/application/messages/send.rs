//! `narou send` が出す文言 (端末へのコピー・栞バックアップ)。

use std::fmt::Display;

/// 直接送信に未対応の端末。
pub fn direct_send_unsupported(device: impl Display) -> String {
    format!("{device} への直接送信は対応していません")
}

pub fn invalid_device_setting() -> &'static str {
    "送信に使う端末設定が不正です"
}

pub fn device_not_connected(device: impl Display) -> String {
    format!("{device} が接続されていません")
}

/// デバイス名未指定・不正 (`format!` の `\` 継続行をそのまま再現)。
pub fn device_name_unspecified(device_names: impl Display) -> String {
    format!(
        "デバイス名が指定されていないか、間違っています。\n\
narou setting device=デバイス名 で指定出来ます。\n\
指定出来るデバイス名：{device_names}"
    )
}

/// 変換前のファイルがまだ無い。
pub fn file_not_yet(filename: impl Display) -> String {
    format!("まだファイル({filename})が無いようです")
}

pub fn send_destination_invalid() -> &'static str {
    "送信先端末が不正です"
}

/// コピーの進捗ドット (`print!` の断片)。
pub fn progress_dot() -> &'static str {
    "."
}

pub fn send_interrupted() -> &'static str {
    "送信を中断しました"
}

/// 栞バックアップ非対応の端末 (stderr)。
pub fn bookmark_backup_unsupported() -> &'static str {
    "ご利用の端末での栞データのバックアップは対応していません"
}

pub fn bookmark_backed_up() -> &'static str {
    "端末の栞データをバックアップしました"
}

pub fn bookmark_restored() -> &'static str {
    "栞データを端末にコピーしました"
}

pub fn bookmark_absent() -> &'static str {
    "栞データが無いようです"
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_literals_match_native() {
        assert_eq!(super::invalid_device_setting(), "送信に使う端末設定が不正です");
        assert_eq!(super::send_destination_invalid(), "送信先端末が不正です");
        assert_eq!(super::progress_dot(), ".");
        assert_eq!(super::send_interrupted(), "送信を中断しました");
        assert_eq!(
            super::bookmark_backup_unsupported(),
            "ご利用の端末での栞データのバックアップは対応していません"
        );
        assert_eq!(super::bookmark_backed_up(), "端末の栞データをバックアップしました");
        assert_eq!(super::bookmark_restored(), "栞データを端末にコピーしました");
        assert_eq!(super::bookmark_absent(), "栞データが無いようです");
    }

    #[test]
    fn lines_match_native() {
        assert_eq!(
            super::direct_send_unsupported("kindle"),
            "kindle への直接送信は対応していません"
        );
        assert_eq!(
            super::device_not_connected("kindle"),
            "kindle が接続されていません"
        );
        assert_eq!(
            super::file_not_yet("a.epub"),
            "まだファイル(a.epub)が無いようです"
        );
    }
}
