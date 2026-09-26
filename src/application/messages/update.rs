//! `narou update` が出す文言。download と共通のものは `messages` 直下の
//! 共有関数 (`separator`, `indented_error`, `no_update`, …) を使い、
//! update 固有 (hotentry / general lastup / 並列ワーカーのドメイン行) だけ
//! をここに置く。

use std::fmt::Display;

/// 中断時 (exit 126)。
pub fn update_interrupted() -> &'static str {
    "アップデートを中断しました"
}

/// hotentry 後処理の失敗 (2 行目は字下げ)。
pub fn hotentry_failed(error: impl Display) -> String {
    format!("hotentry の処理に失敗しました\n  {error}")
}

/// 更新終了時のエラー件数まとめ (先頭に空行を置く)。
pub fn errors_occurred(count: usize) -> String {
    format!("\n{count} 件のエラーが発生しました")
}

/// 対象が管理小説に無い (先頭の [ERROR] は呼び出し側が赤くする)。
pub fn unmanaged_target(error_tag: impl Display, target: impl Display) -> String {
    format!("{error_tag} {target} は管理小説の中に存在しません")
}

/// sort-by に不正なキーを渡したときの一覧付きエラー。
pub fn invalid_sort_key(key: impl Display, summaries: impl Display) -> String {
    format!("{key} は正しいキーではありません。次の中から選択して下さい\n{summaries}")
}

/// sort-by のキー候補 1 行 (`"  {key:>20}   {label}"`)。
pub fn sort_key_summary_entry(key: impl Display, label: impl Display) -> String {
    format!("  {key:>20}   {label}")
}

/// 一部の話がサイト側で消えた通知 (全角空白区切り — download 側の半角と
/// 文言が違うので download のものとは別関数)。
pub fn sections_deleted(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} は一部の話が削除されています")
}

/// 差分更新の完了 (件数を出さない update 側の文言)。
pub fn update_completed(title: impl Display) -> String {
    format!("{title} の更新が完了しました")
}

pub fn title_changed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} のタイトルが更新されています")
}

pub fn story_changed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} のあらすじが更新されています")
}

pub fn author_changed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} の作者名が更新されています")
}

pub fn hotentry_convert_started() -> &'static str {
    "hotentry の変換を開始"
}

pub fn hotentry_generated(path: impl Display) -> String {
    format!("hotentry を生成しました: {path}")
}

/// `--gl` の不正値エラー。
pub fn gl_option_invalid() -> &'static str {
    "--gl で指定可能なオプションではありません。詳細は narou u -h を参照"
}

pub fn checking_lastup() -> &'static str {
    "最新話掲載日を確認しています..."
}

pub fn lastup_check_failed() -> &'static str {
    "最新話掲載日の確認に失敗しました"
}

pub fn check_completed() -> &'static str {
    "確認が完了しました"
}

// --- 並列ドメインワーカーの行 (safe_println 経由) ---

/// ワーカーの Downloader 構築失敗。
pub fn parallel_worker_downloader_error(worker_idx: usize, error: impl Display) -> String {
    format!("[worker {worker_idx}] Error creating downloader: {error}")
}

pub fn domain_starting(domain: impl Display, count: usize) -> String {
    format!("[{domain}] starting ({count} novel(s))")
}

pub fn domain_done(domain: impl Display) -> String {
    format!("[{domain}] done")
}

/// 凍結中小説を単発指定したときの通知 (全角空白区切り)。
pub fn frozen_id(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} は凍結中です")
}

pub fn update_canceled(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} の更新はキャンセルされました")
}

/// 前回の変換失敗がある小説の再変換予告 (呼び出し側で黄色にする)。
pub fn reconvert_after_failure() -> &'static str {
    "前回変換できなかったので再変換します"
}

pub fn update_failed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} の更新は失敗しました")
}

/// webui.debug-mode 時だけ出す失敗の詳細。
pub fn error_detail(error: impl Display) -> String {
    format!("  Error detail: {error}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_literals_match_native() {
        assert_eq!(super::update_interrupted(), "アップデートを中断しました");
        assert_eq!(super::hotentry_convert_started(), "hotentry の変換を開始");
        assert_eq!(
            super::gl_option_invalid(),
            "--gl で指定可能なオプションではありません。詳細は narou u -h を参照"
        );
        assert_eq!(super::checking_lastup(), "最新話掲載日を確認しています...");
        assert_eq!(super::lastup_check_failed(), "最新話掲載日の確認に失敗しました");
        assert_eq!(super::check_completed(), "確認が完了しました");
        assert_eq!(super::reconvert_after_failure(), "前回変換できなかったので再変換します");
    }

    #[test]
    fn status_lines_match_native() {
        assert_eq!(
            super::hotentry_failed("io"),
            "hotentry の処理に失敗しました\n  io"
        );
        assert_eq!(super::errors_occurred(3), "\n3 件のエラーが発生しました");
        assert_eq!(
            super::unmanaged_target("[ERROR]", "n1"),
            "[ERROR] n1 は管理小説の中に存在しません"
        );
        assert_eq!(
            super::invalid_sort_key("bogus", "  list"),
            "bogus は正しいキーではありません。次の中から選択して下さい\n  list"
        );
        assert_eq!(
            super::sort_key_summary_entry("title", "タイトル"),
            "                 title   タイトル"
        );
        assert_eq!(
            super::sections_deleted(7, "タイトル"),
            "ID:7　タイトル は一部の話が削除されています"
        );
        assert_eq!(super::update_completed("タイトル"), "タイトル の更新が完了しました");
        assert_eq!(
            super::title_changed(7, "タイトル"),
            "ID:7　タイトル のタイトルが更新されています"
        );
        assert_eq!(
            super::story_changed(7, "タイトル"),
            "ID:7　タイトル のあらすじが更新されています"
        );
        assert_eq!(
            super::author_changed(7, "タイトル"),
            "ID:7　タイトル の作者名が更新されています"
        );
        assert_eq!(
            super::hotentry_generated("/tmp/h.txt"),
            "hotentry を生成しました: /tmp/h.txt"
        );
    }

    #[test]
    fn parallel_worker_lines_match_native() {
        assert_eq!(
            super::parallel_worker_downloader_error(2, "net"),
            "[worker 2] Error creating downloader: net"
        );
        assert_eq!(
            super::domain_starting("syosetu.com", 4),
            "[syosetu.com] starting (4 novel(s))"
        );
        assert_eq!(super::domain_done("syosetu.com"), "[syosetu.com] done");
        assert_eq!(
            super::frozen_id(7, "タイトル"),
            "ID:7　タイトル は凍結中です"
        );
        assert_eq!(
            super::update_canceled(7, "タイトル"),
            "ID:7　タイトル の更新はキャンセルされました"
        );
        assert_eq!(
            super::update_failed(7, "タイトル"),
            "ID:7　タイトル の更新は失敗しました"
        );
        assert_eq!(super::error_detail("boom"), "  Error detail: boom");
    }
}
