//! 起動時の一括バックアップ提案と、ライブラリ全体バックアップの実装。
//!
//! 0.4.0 未満から 0.4.0 以上へアップデートした後の初回起動時に、
//! 小説データ (`小説データ/`) と管理データ (`.narou/`) をまとめた zip を
//! narou_rs 実行ファイルと同じフォルダの `backup/` へ作成するか確認する。
//! Web UI では `GET /api/library_backup` が同じ判定を返し、モーダルで提案する。
//!
//! 前回起動バージョンは `.narou/last-run-version` に記録する。
//! プロンプトを出した・出さなかったに関わらず、判定を行った起動では
//! 常に現在バージョンへ更新する。
//! 実際の zip 作成は `narou_rs_backup` 実行ファイルとしても独立しており、
//! Web UI からはサブプロセスとして起動される。

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::error::{NarouError, Result};
use crate::version;

/// このバージョンを境にバックアップ提案を出す。
/// prev < BOUNDARY <= current のときだけ提案する。
const BACKUP_PROMPT_BOUNDARY: &str = "0.4.0";

/// 前回起動バージョンを記録する `.narou` 内のファイル名。
const LAST_RUN_VERSION_FILE: &str = "last-run-version";

/// バックアップ zip の出力先 (exe と同じフォルダの下)。
const BACKUP_DIR_NAME: &str = "backup";

/// バックアップ専用実行ファイル名 (拡張子なし)。
pub const BACKUP_EXE_STEM: &str = "narou_rs_backup";

/// プロンプトを出さないコマンド。
/// `web` はサーバー起動をブロックしないため除外 (Web UI 側で提案する)。
/// `help`/`version` は情報表示のみなので除外。
const SKIP_COMMANDS: &[&str] = &["web", "help", "version"];

/// バックアップ提案に必要な容量情報。
#[derive(Debug, Clone, Copy)]
pub struct PendingOffer {
    /// バックアップ対象の非圧縮合計サイズ。
    pub total_bytes: u64,
    /// 保存先ドライブの空き容量。取得不能なら None。
    pub free_bytes: Option<u64>,
    /// 空き容量が合計サイズ以上 (または不明) なら true。
    pub enough_space: bool,
}

/// コマンド実行前に呼ばれる入口。失敗しても本体の動作を妨げない。
pub fn maybe_offer(command: &str) {
    if let Err(err) = offer(command) {
        eprintln!("[WARN] 起動時バックアップ処理でエラー: {}", err);
    }
}

fn offer(command: &str) -> Result<()> {
    if SKIP_COMMANDS.contains(&command) {
        return Ok(());
    }
    if std::env::var_os("NAROU_ENV").is_some_and(|v| v == "test") {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Ok(());
    }
    let Some(root) = crate::logger::find_narou_root() else {
        return Ok(());
    };

    if let Some(pending) = pending_offer(&root) {
        prompt_and_backup(&root, &pending);
    }

    // 判定を行った起動では常に現在バージョンを記録する。
    // バックアップ失敗時も記録して再プロンプトの嵐を防ぐ。
    record_run(&root);
    Ok(())
}

/// バックアップ提案が必要なら容量情報を返す。
/// CLI と Web API で共通の判定。提案不要・データなしなら None。
pub fn pending_offer(root: &Path) -> Option<PendingOffer> {
    let previous = read_last_run_version(root);
    if !should_offer(previous.as_deref(), version::VERSION) {
        return None;
    }
    if !has_library_data(root) {
        return None;
    }
    let total_bytes = backup_size(root).ok()?;
    let free_bytes = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .and_then(|dir| fs2::available_space(dir).ok());
    Some(PendingOffer {
        total_bytes,
        free_bytes,
        enough_space: free_bytes.is_none_or(|bytes| bytes >= total_bytes),
    })
}

/// 前回起動バージョンを `.narou/last-run-version` へ記録する。
/// 提案への回答 (作成/スキップ) があった時点で呼ぶ。
pub fn record_run(root: &Path) {
    let marker_path = root.join(".narou").join(LAST_RUN_VERSION_FILE);
    let _ = fs::write(marker_path, format!("{}\n", version::VERSION));
}

fn read_last_run_version(root: &Path) -> Option<String> {
    fs::read_to_string(root.join(".narou").join(LAST_RUN_VERSION_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
/// prev < BOUNDARY <= current のとき true。
/// prev が読めない/パース不能なら旧バージョン扱いで提案する。
fn should_offer(previous: Option<&str>, current: &str) -> bool {
    let current_ok = matches!(
        version::version_compare(&version::version_core(current), BACKUP_PROMPT_BOUNDARY),
        Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
    );
    if !current_ok {
        return false;
    }
    match previous {
        Some(prev) => matches!(
            version::version_compare(&version::version_core(prev), BACKUP_PROMPT_BOUNDARY),
            Some(std::cmp::Ordering::Less) | None
        ),
        None => true,
    }
}

/// バックアップ対象となる実データがあるか。
/// `.narou` だけの空ライブラリでは提案しない。
fn has_library_data(root: &Path) -> bool {
    let archive = root.join(crate::downloader::ARCHIVE_ROOT_DIR);
    fs::read_dir(&archive)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}


fn prompt_and_backup(root: &Path, pending: &PendingOffer) {
    let default_dir = default_backup_dir();

    println!();
    println!("0.4.0 では管理データの保存形式に破壊的な変更が加わっています。データ消失に備え、小説データのバックアップを推奨します。");
    println!(
        "対象: {} と .narou (計 {})",
        crate::downloader::ARCHIVE_ROOT_DIR,
        format_size(pending.total_bytes)
    );
    match pending.free_bytes {
        Some(bytes) => println!("保存先ドライブの空き容量: {}", format_size(bytes)),
        None => println!("保存先ドライブの空き容量: 不明"),
    }
    if !pending.enough_space {
        println!("[WARN] 空き容量が不足している可能性があります。");
    }

    if !crate::compat::confirm("バックアップを作成しますか", pending.enough_space, false) {
        return;
    }

    // 保存先の指定を受け付ける。未入力なら exe 横の backup/。
    let dest_dir = match default_dir.as_ref() {
        Some(dir) => match ask_backup_destination(dir) {
            Ok(dir) => dir,
            Err(err) => {
                eprintln!("[WARN] 保存先の確認に失敗しました: {}", err);
                return;
            }
        },
        None => {
            eprintln!("[WARN] 実行ファイルの場所を特定できないためバックアップを作成できません");
            return;
        }
    };

    if let Err(err) = check_free_space(&dest_dir, pending.total_bytes) {
        eprintln!("[WARN] {}", err);
        return;
    }

    match create_library_backup(root, &dest_dir) {
        Ok(path) => println!("バックアップを作成しました: {}", path.display()),
        Err(err) => eprintln!("[WARN] バックアップの作成に失敗しました: {}", err),
    }
}

/// 保存先フォルダを対話で尋ねる。未入力は default_dir。
fn ask_backup_destination(default_dir: &Path) -> Result<PathBuf> {
    println!("保存先フォルダ (未入力で {}):", default_dir.display());
    print!(">");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(default_dir.to_path_buf());
    }
    let input = input.trim();
    if input.is_empty() {
        return Ok(default_dir.to_path_buf());
    }
    Ok(PathBuf::from(input))
}

/// 既定のバックアップ出力先: narou_rs と同じフォルダの `backup/`。
pub fn default_backup_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .map(|dir| dir.join(BACKUP_DIR_NAME))
}

/// 保存先ドライブの空き容量が合計サイズに足りるか確認する。
/// 空き容量を取得できない場合は Ok (不明扱い)。
pub fn check_free_space(dest_dir: &Path, total_bytes: u64) -> Result<()> {
    // 存在しないパスでも最も近い既存の親で statvfs 相当を取るため、
    // 親を辿って既存ディレクトリを探す。
    let mut probe = dest_dir;
    let mut free = None;
    loop {
        if probe.is_dir() {
            free = fs2::available_space(probe).ok();
            break;
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    match free {
        Some(bytes) if bytes < total_bytes => Err(NarouError::Conversion(format!(
            "保存先ドライブの空き容量が不足しています (必要: 約{}, 空き: {})",
            format_size(total_bytes),
            format_size(bytes)
        ))),
        _ => Ok(()),
    }
}

/// `narou_rs_backup` 実行ファイルのパス (本体と同じフォルダ)。
pub fn backup_exe_path() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))?;
    let name = if cfg!(windows) {
        format!("{}.exe", BACKUP_EXE_STEM)
    } else {
        BACKUP_EXE_STEM.to_string()
    };
    let path = exe_dir.join(name);
    path.is_file().then_some(path)
}

/// `小説データ/` と `.narou/` を `dest_dir` に zip 化する。
/// `dest_dir` は zip を置くフォルダ (なければ作成する)。
/// 失敗時は中途半端な zip を残さない。
pub fn create_library_backup(root: &Path, dest_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;
    let backup_name = format!(
        "narou-backup-{}.zip",
        chrono::Local::now().format("%Y%m%d%H%M%S")
    );
    let backup_path = dest_dir.join(&backup_name);

    let result = (|| -> Result<()> {
        let file = fs::File::create(&backup_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for dir in [
            root.join(crate::downloader::ARCHIVE_ROOT_DIR),
            root.join(".narou"),
        ] {
            if dir.is_dir() {
                add_directory_to_zip(&mut zip, root, &dir, options)?;
            }
        }
        zip.finish()
            .map(|_| ())
            .map_err(|e| NarouError::Conversion(e.to_string()))
    })();

    if result.is_err() {
        let _ = fs::remove_file(&backup_path);
    }
    result.map(|_| backup_path)
}

/// バックアップ対象ファイルの合計サイズ (非圧縮)。
pub fn backup_size(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    for dir in [
        root.join(crate::downloader::ARCHIVE_ROOT_DIR),
        root.join(".narou"),
    ] {
        if dir.is_dir() {
            let mut files = Vec::new();
            collect_backup_files(root, &dir, &mut files)?;
            for path in files {
                total += fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    Ok(total)
}

fn add_directory_to_zip(
    zip: &mut zip::ZipWriter<fs::File>,
    base_dir: &Path,
    current_dir: &Path,
    options: zip::write::SimpleFileOptions,
) -> Result<()> {
    let mut files = Vec::new();
    collect_backup_files(base_dir, current_dir, &mut files)?;

    let mut entries: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(base_dir)
                .map_err(|e| NarouError::Conversion(e.to_string()))?;
            Ok((rel.to_string_lossy().replace('\\', "/"), path))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (rel_name, path) in entries {
        let mut file = fs::File::open(&path)?;
        zip.start_file(rel_name, options)
            .map_err(|e| NarouError::Conversion(e.to_string()))?;
        io::copy(&mut file, zip)?;
    }
    Ok(())
}

/// バックアップ対象を再帰列挙する。シンボリックリンクは辿らない。
/// 除外ルール:
/// - 任意の階層の `backup/` ディレクトリ (小説ごとのバックアップ)
/// - `.narou/section_convert_cache/` (再生成可能なキャッシュ)
/// - `.narou/*.lock`, `.narou/server.pid` (実行時の一時ファイル)
fn collect_backup_files(base_dir: &Path, current_dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }
        if is_backup_excluded(base_dir, &path) {
            continue;
        }
        if path.is_dir() {
            collect_backup_files(base_dir, &path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn is_backup_excluded(base_dir: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(base_dir) else {
        return false;
    };
    let mut components = rel.components().peekable();
    if components
        .peek()
        .is_some_and(|c| c.as_os_str() == ".narou")
    {
        components.next();
        if let Some(first) = components.next() {
            let name = first.as_os_str();
            if name == "section_convert_cache" || name == "server.pid" {
                return true;
            }
            // .narou 直下の *.lock のみ除外 (ネストした .lock は残す)
            if components.next().is_none()
                && name
                    .to_string_lossy()
                    .to_lowercase()
                    .ends_with(".lock")
            {
                return true;
            }
        }
        return false;
    }
    rel.components().any(|c| c.as_os_str() == "backup")
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_offer_only_when_crossing_boundary() {
        // prev < 0.4.0 <= current → 提案
        assert!(should_offer(Some("0.3.6"), "0.4.0"));
        assert!(should_offer(Some("0.3.6"), "0.4.1"));
        // 初回起動 (marker なし) → 提案
        assert!(should_offer(None, "0.4.0"));
        // marker が壊れていても旧バージョン扱い → 提案
        assert!(should_offer(Some("garbage"), "0.4.0"));
        // 既に 0.4.0 以上で起動済み → 出さない
        assert!(!should_offer(Some("0.4.0"), "0.4.1"));
        assert!(!should_offer(Some("0.4.0"), "0.4.0"));
        // 現在が 0.4.0 未満 → 出さない
        assert!(!should_offer(Some("0.3.6"), "0.3.9"));
        assert!(!should_offer(None, "0.3.9"));
        // サフィックス付き表記も core で比較
        assert!(should_offer(Some("0.3.6 (develop)"), "0.4.0 (local-build)"));
    }

    #[test]
    fn backup_exclusion_rules() {
        let base = Path::new("/lib");
        // 小説ごとの backup/ は除外
        assert!(is_backup_excluded(
            base,
            Path::new("/lib/小説データ/site/1 title/backup/x.zip")
        ));
        // .narou のキャッシュ・ロック・pid は除外
        assert!(is_backup_excluded(
            base,
            Path::new("/lib/.narou/section_convert_cache/a.bin")
        ));
        assert!(is_backup_excluded(
            base,
            Path::new("/lib/.narou/database.yaml.lock")
        ));
        assert!(is_backup_excluded(
            base,
            Path::new("/lib/.narou/server.pid")
        ));
        // .narou の通常ファイルは残す
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/.narou/database.yaml")
        ));
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/.narou/last-run-version")
        ));
        // 小説データ本体は残す
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/小説データ/site/1 title/toc.yaml")
        ));
        // タイトルに backup を含むだけの小説は残す (完全一致のみ除外)
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/小説データ/site/1 backup-story/toc.yaml")
        ));
    }

    #[test]
    fn format_size_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
