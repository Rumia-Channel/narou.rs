//! `narou download` と Downloader 本体が出す文言。
//!
//! セクション進捗やログイン再試行といった Downloader 内部の出力は native /
//! Worker の両方から同じ文字列が出るようにここへ集約する。`print!` の
//! プロンプト断片も送り先が出せるよう関数に切り出す。

use std::fmt::Display;
use std::path::Path;

/// 中断時のメッセージ (exit code 126)。
pub fn download_interrupted() -> &'static str {
    "ダウンロードを中断しました"
}

/// 凍結中小説を対象にしたときの中止通知 (複数行そのまま出す)。
pub fn frozen_abort(title: impl Display) -> String {
    format!("{title} は凍結中です\nダウンロードを中止しました")
}

/// 既にダウンロード済みの小説。
pub fn already_downloaded(
    target: impl Display,
    id: impl Display,
    title: impl Display,
) -> String {
    format!("{target} はダウンロード済みです。\nID: {id}\ntitle: {title}")
}

/// 保存フォルダが消えていて DB インデックスを消した旨 (stderr)。
pub fn missing_dir_index_removed(path: &Path) -> String {
    format!(
        "{} が見つかりません。\n保存フォルダが消去されていたため、データベースのインデックスを削除しました。",
        path.display()
    )
}

/// 消えたインデックス掃除自体の失敗 (stderr の Warning 行)。
pub fn stale_index_cleanup_warn(error: impl Display) -> String {
    format!("Warning: stale database index cleanup failed: {error}")
}

/// (y/n) プロンプト。改行なしで出す断片。
pub fn confirm_yes_no(message: impl Display) -> String {
    format!("{message} (y/n)?: ")
}

/// auto_convert の失敗を括って出す行。
pub fn convert_error_line(error: impl Display) -> String {
    format!("  Convert error: {error}")
}

/// シリーズ指定を個別 URL 群へ展開した件数。
pub fn series_expanded(target: impl Display, count: usize) -> String {
    format!("{target} を {count} 件の小説URLに展開しました")
}

/// 対話モード冒頭の案内文 (6 行。最後の空行は対話モード表示と入力待ちの
/// 視覚的な区切り)。
pub fn interactive_banner() -> [&'static str; 6] {
    [
        "【対話モード】",
        "ダウンロードしたい小説のNコードもしくはURLを入力して下さい。(1行に1つ)",
        "連続して複数の小説を入力していきます。",
        "対応サイトは小説家になろう(小説を読もう)、ノクターンノベルズ、ムーンライトノベルズ、Arcadia、ハーメルン、暁、カクヨムです。",
        "入力を終了してダウンロードを開始するには未入力のままエンターを押して下さい。",
        "",
    ]
}

/// 対話モードの入力待ちプロンプト (改行なし)。
pub fn interactive_prompt(count: usize) -> String {
    format!("{count}件をダウンロードしますか？ [Y/n]> ")
}

pub fn already_entered() -> &'static str {
    "入力済みです"
}

pub fn unsupported_novel() -> &'static str {
    "対応外の小説です"
}

/// DL 結果の「更新あり」通知 (更新 0 件のときは件数なし)。
pub fn update_completed(
    title: impl Display,
    id: impl Display,
    updated_count: impl Display,
    total_count: impl Display,
) -> String {
    format!("{title} の更新完了 (ID:{id}, {updated_count}/{total_count}話更新)")
}

/// タイトル変更の通知 (download 側は半角空白区切り — update 側の全角空白と
/// 文言が違うので別関数)。
pub fn title_changed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id} {title} のタイトルが更新されています")
}

pub fn story_changed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id} {title} のあらすじが更新されています")
}

pub fn author_changed(id: impl Display, title: impl Display) -> String {
    format!("ID:{id} {title} の作者名が更新されています")
}

pub fn update_canceled(id: impl Display, title: impl Display) -> String {
    format!("ID:{id} {title} の更新はキャンセルされました")
}

/// `--freeze` / `--remove` の後処理通知。
pub fn freeze_target(target: impl Display) -> String {
    format!("凍結: {target}")
}

pub fn remove_target(target: impl Display) -> String {
    format!("削除: {target}")
}

// ---------------------------------------------------------------------------
// Downloader 本体 (src/downloader/**) が出す進捗・警告
// ---------------------------------------------------------------------------

/// DL 開始行 (native は色付きで出すので文字列は呼び出し側で装飾する)。
pub fn download_started(id: impl Display, title: impl Display) -> String {
    format!("ID:{id}　{title} のDL開始")
}

/// セクション 1 件分の進捗行。`series` = 連載 (話数が 4 桁まで第◯部分)、
/// `decorate` は "(新着)" マーカーの装飾用 (native は色付け、Worker は素通し)。
pub fn section_progress_line(
    series: bool,
    index: &str,
    subtitle: &str,
    downloaded: usize,
    total: usize,
) -> String {
    let mut line = String::new();
    if series {
        if index.len() <= 4 {
            line.push_str(&format!("第{index}部分　"));
        }
    } else {
        line.push_str("短編　");
    }
    line.push_str(&format!("{subtitle} ({downloaded}/{total})"));
    line
}

/// 新規DL / force 時の新着話マーカー (native は magenta)。
pub fn new_arrival_marker() -> &'static str {
    " (新着)"
}

/// force 再DLで更新があった話のマーカー。
pub fn updated_marker() -> &'static str {
    " (更新あり)"
}

/// 進捗バーの完了メッセージ (ProgressReporter::finish_with_message 経由)。
pub fn dl_done_message(title: impl Display, updated: usize, total: usize) -> String {
    format!("DL {title} done ({updated}/{total})")
}

/// 保存済みログイン情報での再試行通知。
pub fn login_retry() -> &'static str {
    "ログインが必要な可能性があります。保存済みのログイン情報で再試行します"
}

/// 小説が取得できなかった (削除/非公開)。
pub fn novel_unavailable() -> &'static str {
    "小説が削除されているか非公開な可能性があります"
}

/// ダイジェスト化検知プロンプトの冒頭 (改行込みの複数行)。
pub fn digest_detected_prompt(old_count: usize, latest_count: usize) -> String {
    format!(
        "更新後の話数が保存されている話数より減少していることを検知しました。\nダイジェスト化されている可能性があるので、更新に関しての処理を選択して下さい。\n\n保存済み話数: {old_count}\n更新後の話数: {latest_count}\n\n"
    )
}

/// ダイジェスト選択「バックアップ」の完了通知。
pub fn backup_created(name: impl Display) -> String {
    format!("{name} を作成しました")
}

/// ダイジェスト選択「あらすじを表示」の見出し。
pub fn story_label() -> &'static str {
    "あらすじ"
}

// --- 挿絵・アニメーションの WARN 行 (stderr) ---

pub fn warn_unsafe_illustration_url(url: impl Display) -> String {
    format!("WARN: skipping unsafe illustration URL: {url}")
}

pub fn warn_animation_assemble(url: impl Display, error: impl Display) -> String {
    format!("WARN: failed to assemble animation {url}: {error}")
}

pub fn warn_illustration_save(url: impl Display, error: impl Display) -> String {
    format!("WARN: failed to save illustration {url}: {error}")
}

pub fn warn_illustration_download(url: impl Display, error: impl Display) -> String {
    format!("WARN: failed to download illustration {url}: {error}")
}

// --- サイト定義の fancy-regex ガード (info_extraction.rs, stderr) ---

pub fn warn_fancy_pattern_large(key: impl Display) -> String {
    format!("WARN: skipping fancy-regex for {key}: pattern is too large")
}

pub fn warn_fancy_input_large(key: impl Display) -> String {
    format!("WARN: skipping fancy-regex for {key}: input is too large")
}

// --- auto_convert の子プロセスリレー由来のエラー文言 ---

pub fn convert_stdout_unavailable() -> &'static str {
    "convert stdout を取得できません"
}

pub fn convert_stderr_unavailable() -> &'static str {
    "convert stderr を取得できません"
}

pub fn convert_stdout_relay_panicked() -> &'static str {
    "convert stdout relay thread が panic しました"
}

pub fn convert_stderr_relay_panicked() -> &'static str {
    "convert stderr relay thread が panic しました"
}

pub fn convert_exit_code_failed(code: impl Display) -> String {
    format!("convert が終了コード {code} で失敗しました")
}

pub fn convert_abnormal_exit() -> &'static str {
    "convert が異常終了しました"
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn fixed_literals_match_native() {
        assert_eq!(super::download_interrupted(), "ダウンロードを中断しました");
        assert_eq!(super::already_entered(), "入力済みです");
        assert_eq!(super::unsupported_novel(), "対応外の小説です");
        assert_eq!(
            super::login_retry(),
            "ログインが必要な可能性があります。保存済みのログイン情報で再試行します"
        );
        assert_eq!(
            super::novel_unavailable(),
            "小説が削除されているか非公開な可能性があります"
        );
        assert_eq!(super::story_label(), "あらすじ");
        assert_eq!(super::new_arrival_marker(), " (新着)");
        assert_eq!(super::updated_marker(), " (更新あり)");
    }

    #[test]
    fn status_lines_match_native() {
        assert_eq!(
            super::frozen_abort("タイトル"),
            "タイトル は凍結中です\nダウンロードを中止しました"
        );
        assert_eq!(
            super::already_downloaded("n1234", 7, "タイトル"),
            "n1234 はダウンロード済みです。\nID: 7\ntitle: タイトル"
        );
        assert_eq!(
            super::missing_dir_index_removed(Path::new("/tmp/novel")),
            "/tmp/novel が見つかりません。\n保存フォルダが消去されていたため、データベースのインデックスを削除しました。"
        );
        assert_eq!(
            super::stale_index_cleanup_warn("io"),
            "Warning: stale database index cleanup failed: io"
        );
        assert_eq!(super::confirm_yes_no("再ダウンロードしますか"), "再ダウンロードしますか (y/n)?: ");
        assert_eq!(super::convert_error_line("x"), "  Convert error: x");
        assert_eq!(
            super::series_expanded("s1234", 3),
            "s1234 を 3 件の小説URLに展開しました"
        );
        assert_eq!(
            super::interactive_prompt(2),
            "2件をダウンロードしますか？ [Y/n]> "
        );
        assert_eq!(
            super::update_completed("タイトル", 7, 2, 10),
            "タイトル の更新完了 (ID:7, 2/10話更新)"
        );
        assert_eq!(
            super::title_changed(7, "タイトル"),
            "ID:7 タイトル のタイトルが更新されています"
        );
        assert_eq!(
            super::story_changed(7, "タイトル"),
            "ID:7 タイトル のあらすじが更新されています"
        );
        assert_eq!(
            super::author_changed(7, "タイトル"),
            "ID:7 タイトル の作者名が更新されています"
        );
        assert_eq!(
            super::update_canceled(7, "タイトル"),
            "ID:7 タイトル の更新はキャンセルされました"
        );
        assert_eq!(super::freeze_target("n1"), "凍結: n1");
        assert_eq!(super::remove_target("n1"), "削除: n1");
    }

    #[test]
    fn interactive_banner_has_six_lines_and_blank_tail() {
        let banner = super::interactive_banner();
        assert_eq!(banner.len(), 6);
        assert_eq!(banner[0], "【対話モード】");
        assert_eq!(banner[5], "");
    }

    #[test]
    fn downloader_lines_match_native() {
        assert_eq!(
            super::download_started(7, "タイトル"),
            "ID:7　タイトル のDL開始"
        );
        assert_eq!(
            super::section_progress_line(true, "12", "サブタイトル", 3, 10),
            "第12部分　サブタイトル (3/10)"
        );
        assert_eq!(
            super::section_progress_line(true, "12345", "サブタイトル", 3, 10),
            "サブタイトル (3/10)"
        );
        assert_eq!(
            super::section_progress_line(false, "1", "短編タイトル", 1, 1),
            "短編　短編タイトル (1/1)"
        );
        assert_eq!(super::dl_done_message("タイトル", 2, 5), "DL タイトル done (2/5)");
        assert_eq!(
            super::digest_detected_prompt(12, 10),
            "更新後の話数が保存されている話数より減少していることを検知しました。\nダイジェスト化されている可能性があるので、更新に関しての処理を選択して下さい。\n\n保存済み話数: 12\n更新後の話数: 10\n\n"
        );
        assert_eq!(super::backup_created("backup.zip"), "backup.zip を作成しました");
    }

    #[test]
    fn warn_lines_match_native() {
        assert_eq!(
            super::warn_unsafe_illustration_url("http://x"),
            "WARN: skipping unsafe illustration URL: http://x"
        );
        assert_eq!(
            super::warn_animation_assemble("u", "e"),
            "WARN: failed to assemble animation u: e"
        );
        assert_eq!(
            super::warn_illustration_save("u", "e"),
            "WARN: failed to save illustration u: e"
        );
        assert_eq!(
            super::warn_illustration_download("u", "e"),
            "WARN: failed to download illustration u: e"
        );
        assert_eq!(
            super::warn_fancy_pattern_large("t"),
            "WARN: skipping fancy-regex for t: pattern is too large"
        );
        assert_eq!(
            super::warn_fancy_input_large("t"),
            "WARN: skipping fancy-regex for t: input is too large"
        );
    }

    #[test]
    fn convert_relay_errors_match_native() {
        assert_eq!(super::convert_stdout_unavailable(), "convert stdout を取得できません");
        assert_eq!(super::convert_stderr_unavailable(), "convert stderr を取得できません");
        assert_eq!(
            super::convert_stdout_relay_panicked(),
            "convert stdout relay thread が panic しました"
        );
        assert_eq!(
            super::convert_stderr_relay_panicked(),
            "convert stderr relay thread が panic しました"
        );
        assert_eq!(
            super::convert_exit_code_failed(3),
            "convert が終了コード 3 で失敗しました"
        );
        assert_eq!(super::convert_abnormal_exit(), "convert が異常終了しました");
    }
}
