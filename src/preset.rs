//! 配布物の `preset/` ディレクトリ (AozoraEpub3 の設定資産) の共有定義。
//!
//! # 利用者設定と同梱の既定値は別のファイル
//!
//! `preset/` には「利用者が編集する設定」と「配布物が持つ既定値」の 2 役目が
//! 混在していた。v0.5.2 までは既定値を `preset/AozoraEpub3.ini` という名前で
//! 同梱していたため、セルフアップデートが配布物のファイルを置き換えると
//! 利用者の編集内容が無言で消えていた (issue #45)。
//!
//! そこで役割を次の 2 ファイルに分けた。
//!
//! - `preset/AozoraEpub3.ini` … 利用者が作る設定ファイル。**配布物には
//!   同梱しない**ので、どのバージョンの更新でも書き換わらない。
//! - `preset/AozoraEpub3.ini.default` … 配布物が持つ既定値 (テンプレート)。
//!   利用者はこれをコピーして `AozoraEpub3.ini` を作れば編集できる。
//!
//! 実行時の解決順は「利用者設定 → 同梱の既定値 → 埋め込みの既定値」。
//! 同じ仕組みを narou のカスタム注記 (`custom_chuki_tag.txt`) にも適用する。

use std::path::{Path, PathBuf};

/// 利用者が編集する AozoraEpub3 設定のファイル名。
///
/// 配布物には同梱しないため、セルフアップデートでも上書きされない。
pub const USER_INI_FILE_NAME: &str = "AozoraEpub3.ini";

/// 同梱する AozoraEpub3 設定の既定値 (テンプレート) のファイル名。
pub const DEFAULT_INI_FILE_NAME: &str = "AozoraEpub3.ini.default";

/// 同梱の既定値の内容。ファイルが無い環境 (wasm / 未設定) の既定になる。
pub const DEFAULT_INI: &str = include_str!("../preset/AozoraEpub3.ini.default");

/// 利用者が編集する narou カスタム注記のファイル名。
pub const USER_CHUKI_FILE_NAME: &str = "custom_chuki_tag.txt";

/// 同梱する narou カスタム注記の既定値 (テンプレート) のファイル名。
pub const DEFAULT_CHUKI_FILE_NAME: &str = "custom_chuki_tag.txt.default";

/// 同梱のデフォルトのカスタム注記。
pub const DEFAULT_CHUKI_TAG: &str = include_str!("../preset/custom_chuki_tag.txt.default");

/// 利用者設定 → 同梱の既定値 の順で、存在するものだけを候補として返す。
fn candidates(dir: &Path, user_name: &str, default_name: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for name in [user_name, default_name] {
        let path = dir.join(name);
        if path.is_file() {
            paths.push(path);
        }
    }
    paths
}

/// AozoraEpub3 設定 INI の候補 (利用者設定 → 同梱の既定値)。
pub fn ini_candidates(dir: &Path) -> Vec<PathBuf> {
    candidates(dir, USER_INI_FILE_NAME, DEFAULT_INI_FILE_NAME)
}

/// narou カスタム注記の候補 (利用者設定 → 同梱の既定値)。
pub fn chuki_tag_candidates(dir: &Path) -> Vec<PathBuf> {
    candidates(dir, USER_CHUKI_FILE_NAME, DEFAULT_CHUKI_FILE_NAME)
}

/// 実行時に読む narou カスタム注記 (利用者設定 → 同梱の既定値)。
///
/// どちらも無い場合は `None` (呼び出し側は埋め込みの既定値を使う)。
pub fn read_chuki_tag(dir: &Path) -> Option<String> {
    for path in chuki_tag_candidates(dir) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_ini_wins_over_the_bundled_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_INI_FILE_NAME), "TitlePage=2\n").unwrap();
        std::fs::write(dir.path().join(USER_INI_FILE_NAME), "TitlePage=0\n").unwrap();

        let candidates = ini_candidates(dir.path());
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], dir.path().join(USER_INI_FILE_NAME));
        assert_eq!(candidates[1], dir.path().join(DEFAULT_INI_FILE_NAME));
    }

    #[test]
    fn bundled_default_is_used_when_the_user_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_INI_FILE_NAME), "TitlePage=2\n").unwrap();

        let candidates = ini_candidates(dir.path());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], dir.path().join(DEFAULT_INI_FILE_NAME));
    }

    #[test]
    fn missing_files_report_no_candidate() {
        let dir = tempfile::tempdir().unwrap();

        assert!(ini_candidates(dir.path()).is_empty());
        assert!(chuki_tag_candidates(dir.path()).is_empty());
        assert!(read_chuki_tag(dir.path()).is_none());
    }

    #[test]
    fn custom_chuki_tag_follows_the_same_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_CHUKI_FILE_NAME), "default\n").unwrap();
        assert_eq!(read_chuki_tag(dir.path()).as_deref(), Some("default\n"));

        std::fs::write(dir.path().join(USER_CHUKI_FILE_NAME), "user\n").unwrap();
        assert_eq!(read_chuki_tag(dir.path()).as_deref(), Some("user\n"));
    }

    #[test]
    fn embedded_defaults_match_the_bundled_files() {
        // 埋め込みの既定値と同梱ファイルの内容がずれると、ファイルの有無で
        // 変換結果が変わるため、同じ内容であることを固定する。
        assert_eq!(
            DEFAULT_INI,
            std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/preset/AozoraEpub3.ini.default"
            ))
            .unwrap()
        );
        assert_eq!(
            DEFAULT_CHUKI_TAG,
            std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/preset/custom_chuki_tag.txt.default"
            ))
            .unwrap()
        );
    }
}
