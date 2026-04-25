//! `narou_rs_updater(.exe).new` を起動時に検証して通常名へ昇格する。
//!
//! セルフアップデートの新フローでは、updater は zip に
//! `narou_rs_updater(.exe).new` 名で同梱され、updater 自身は実行中の自分を
//! 上書きしない。新 narou_rs 起動時にコンパイル時埋込みハッシュ
//! (`NAROU_RS_UPDATER_SHA256`) と一致を確認してから rename する。
//!
//! 失敗は致命的にしない: ログだけ出して起動を続ける。

use std::fs;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

/// updater のリリース zip 内エントリ名 (拡張子は OS で決まる)。
pub const PENDING_UPDATER_FILENAME: &str = if cfg!(windows) {
    "narou_rs_updater.exe.new"
} else {
    "narou_rs_updater.new"
};

/// 通常時の updater ファイル名 (リネーム後)。
pub const ACTIVE_UPDATER_FILENAME: &str = if cfg!(windows) {
    "narou_rs_updater.exe"
} else {
    "narou_rs_updater"
};

/// build.rs 経由で渡される、リリースビルドに埋め込まれた updater の SHA-256
/// (16 進小文字)。develop ビルドでは空文字列。
pub const EMBEDDED_UPDATER_SHA256: &str = match option_env!("NAROU_RS_UPDATER_SHA256") {
    Some(v) => v,
    None => "",
};

/// `current_exe` のあるディレクトリで `.new` updater を検出して昇格を試みる。
/// 結果は呼び出し側に返さず、`eprintln!` で軽くログするだけ。
pub fn try_promote_pending_updater() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(install_dir) = exe.parent() else {
        return;
    };
    let pending = install_dir.join(PENDING_UPDATER_FILENAME);
    if !pending.is_file() {
        return;
    }
    if let Err(e) = promote_pending_updater_in(install_dir) {
        eprintln!("[updater promote] skipped: {e}");
    }
}

/// テスト可能な実装本体。指定ディレクトリで pending → active へ昇格する。
pub fn promote_pending_updater_in(install_dir: &Path) -> Result<(), String> {
    promote_with_expected_hash(install_dir, EMBEDDED_UPDATER_SHA256)
}

fn promote_with_expected_hash(install_dir: &Path, expected_hex: &str) -> Result<(), String> {
    let pending = install_dir.join(PENDING_UPDATER_FILENAME);
    let active = install_dir.join(ACTIVE_UPDATER_FILENAME);

    if !pending.is_file() {
        return Ok(());
    }

    if expected_hex.is_empty() {
        // develop ビルド等、ハッシュが埋め込まれていない。安全側に倒し、
        // 偽物を昇格しないように何もしない。pending は次回 release バイナリが
        // 起動した時に処理される。
        return Err("no embedded updater hash (develop build)".to_string());
    }

    let actual_hex = sha256_hex_of_file(&pending)
        .map_err(|e| format!("hash {pending:?}: {e}"))?;
    if !hex_eq_ci(&actual_hex, expected_hex) {
        return Err(format!(
            "hash mismatch (expected {expected_hex}, actual {actual_hex})"
        ));
    }

    // 既存 updater がいれば削除を試みる。失敗したら .bak へ退避。
    if active.exists() {
        if let Err(e) = fs::remove_file(&active) {
            let bak = active.with_extension({
                let base = active
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if base.is_empty() {
                    "old".to_string()
                } else {
                    format!("{base}.old")
                }
            });
            let _ = fs::remove_file(&bak);
            fs::rename(&active, &bak).map_err(|re| {
                format!(
                    "remove_file {active:?}: {e}; rename to backup {bak:?}: {re}"
                )
            })?;
        }
    }

    fs::rename(&pending, &active)
        .map_err(|e| format!("rename {pending:?} -> {active:?}: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(mut perm) = fs::metadata(&active).map(|m| m.permissions()) {
            let mode = perm.mode() | 0o755;
            perm.set_mode(mode);
            let _ = fs::set_permissions(&active, perm);
        }
    }

    eprintln!("[updater promote] activated {ACTIVE_UPDATER_FILENAME}");
    Ok(())
}

fn sha256_hex_of_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hex_eq_ci(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    #[test]
    fn no_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        // active がいてもいなくても OK
        assert!(promote_with_expected_hash(tmp.path(), "deadbeef").is_ok());
    }

    #[test]
    fn pending_with_correct_hash_replaces_active() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = tmp.path().join(PENDING_UPDATER_FILENAME);
        let active = tmp.path().join(ACTIVE_UPDATER_FILENAME);
        write_file(&pending, b"new-updater-binary");
        write_file(&active, b"old-updater-binary");
        let expected = sha256_hex(b"new-updater-binary");
        promote_with_expected_hash(tmp.path(), &expected).unwrap();
        assert!(!pending.exists(), "pending should be consumed");
        assert!(active.exists(), "active should exist");
        assert_eq!(fs::read(&active).unwrap(), b"new-updater-binary");
    }

    #[test]
    fn pending_with_wrong_hash_is_left_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = tmp.path().join(PENDING_UPDATER_FILENAME);
        let active = tmp.path().join(ACTIVE_UPDATER_FILENAME);
        write_file(&pending, b"new-bytes");
        write_file(&active, b"old-bytes");
        let err = promote_with_expected_hash(tmp.path(), "00").unwrap_err();
        assert!(err.contains("hash mismatch"));
        assert!(pending.exists());
        assert_eq!(fs::read(&active).unwrap(), b"old-bytes");
    }

    #[test]
    fn empty_expected_hash_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = tmp.path().join(PENDING_UPDATER_FILENAME);
        write_file(&pending, b"x");
        let err = promote_with_expected_hash(tmp.path(), "").unwrap_err();
        assert!(err.contains("no embedded"));
        assert!(pending.exists());
    }

    #[test]
    fn pending_without_existing_active_is_promoted() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = tmp.path().join(PENDING_UPDATER_FILENAME);
        let active = tmp.path().join(ACTIVE_UPDATER_FILENAME);
        write_file(&pending, b"data");
        let expected = sha256_hex(b"data");
        promote_with_expected_hash(tmp.path(), &expected).unwrap();
        assert!(!pending.exists());
        assert_eq!(fs::read(&active).unwrap(), b"data");
    }

    #[test]
    fn hex_eq_ignores_case() {
        assert!(hex_eq_ci("AbCdEf", "abcdef"));
        assert!(!hex_eq_ci("ab", "abc"));
    }
}
