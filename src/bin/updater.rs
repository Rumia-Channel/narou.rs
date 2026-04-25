//! `narou_rs_updater` — sub-binary that performs the file replacement and
//! restart for the WEB UI self-update feature.
//!
//! 親 (本体) プロセスから spawn されることを想定し、引数で受け取った
//! 新バージョンの zip を install ディレクトリに展開して本体を再起動する。
//! 引数仕様は将来互換性のため固定する (新バージョン本体が古い updater を
//! 引き続き使えるよう、引数を後方互換に保つ)。
//!
//! Usage:
//! ```text
//! narou_rs_updater \
//!   --pid <PARENT_PID> \
//!   --zip <ZIP_PATH> \
//!   --install-dir <DIR> \
//!   --log <LOG_PATH> \
//!   --restart <PROG> [args...]
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

#[derive(Debug, Default)]
struct Args {
    pid: Option<u32>,
    zip: Option<PathBuf>,
    install_dir: Option<PathBuf>,
    log: Option<PathBuf>,
    restart: Vec<String>,
}

fn parse_args_from(raw: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut i = 0;
    while i < raw.len() {
        let key = &raw[i];
        match key.as_str() {
            "--pid" => {
                let value = raw.get(i + 1).ok_or("--pid requires a value")?;
                args.pid = Some(value.parse::<u32>().map_err(|e| format!("--pid: {e}"))?);
                i += 2;
            }
            "--zip" => {
                let value = raw.get(i + 1).ok_or("--zip requires a value")?;
                args.zip = Some(PathBuf::from(value));
                i += 2;
            }
            "--install-dir" => {
                let value = raw.get(i + 1).ok_or("--install-dir requires a value")?;
                args.install_dir = Some(PathBuf::from(value));
                i += 2;
            }
            "--log" => {
                let value = raw.get(i + 1).ok_or("--log requires a value")?;
                args.log = Some(PathBuf::from(value));
                i += 2;
            }
            "--restart" => {
                args.restart = raw[i + 1..].to_vec();
                i = raw.len();
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }
    Ok(args)
}

struct Logger {
    file: Option<File>,
}

impl Logger {
    fn open(path: Option<&Path>) -> Self {
        let file = path.and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        Self { file }
    }

    fn log(&mut self, msg: impl AsRef<str>) {
        let line = format!("[{}] {}\n", timestamp_now(), msg.as_ref());
        eprint!("{line}");
        if let Some(file) = self.file.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

/// 依存を増やさず単純な epoch ベースのタイムスタンプを返す。
fn timestamp_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    format!("{secs}.{millis:03}")
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args_from(&raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("narou_rs_updater: {e}");
            std::process::exit(2);
        }
    };

    let install_dir = match args.install_dir.as_ref() {
        Some(d) => d.clone(),
        None => {
            eprintln!("narou_rs_updater: --install-dir is required");
            std::process::exit(2);
        }
    };
    let zip_path = match args.zip.as_ref() {
        Some(z) => z.clone(),
        None => {
            eprintln!("narou_rs_updater: --zip is required");
            std::process::exit(2);
        }
    };
    let log_path = args
        .log
        .clone()
        .unwrap_or_else(|| install_dir.join("update.log"));
    let mut logger = Logger::open(Some(&log_path));

    logger.log(format!(
        "updater start: pid={:?} zip={:?} install_dir={:?} restart={:?}",
        args.pid, zip_path, install_dir, args.restart
    ));

    if let Some(pid) = args.pid {
        wait_for_process_exit(pid, Duration::from_secs(30), &mut logger);
    }

    let backups = match apply_update(&zip_path, &install_dir, &mut logger) {
        Ok(backups) => backups,
        Err(e) => {
            logger.log(format!("update failed: {e}"));
            std::process::exit(1);
        }
    };

    let _ = fs::remove_file(&zip_path);

    if !args.restart.is_empty() {
        if let Err(e) = spawn_restart(&args.restart, &install_dir, &mut logger) {
            logger.log(format!("restart failed: {e}"));
            rollback(&backups, &mut logger);
            std::process::exit(1);
        }
    } else {
        logger.log("no --restart specified, skipping relaunch");
    }

    cleanup_backups(&backups, &mut logger);
    logger.log("updater done");
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32, timeout: Duration, logger: &mut Logger) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            logger.log(format!(
                "OpenProcess({pid}) returned null; assuming process already exited"
            ));
            return;
        }
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let res = WaitForSingleObject(handle, millis);
        let _ = CloseHandle(handle);
        if res != WAIT_OBJECT_0 {
            logger.log(format!(
                "WaitForSingleObject returned {res}; proceeding regardless"
            ));
        } else {
            logger.log(format!("parent pid {pid} exited"));
        }
    }
}

#[cfg(not(windows))]
fn wait_for_process_exit(pid: u32, timeout: Duration, logger: &mut Logger) {
    let deadline = Instant::now() + timeout;
    loop {
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        if !alive {
            logger.log(format!("parent pid {pid} no longer alive"));
            return;
        }
        if Instant::now() >= deadline {
            logger.log(format!(
                "parent pid {pid} still alive after {timeout:?}; proceeding regardless"
            ));
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[derive(Debug)]
struct BackupEntry {
    target: PathBuf,
    backup: PathBuf,
}

fn apply_update(
    zip_path: &Path,
    install_dir: &Path,
    logger: &mut Logger,
) -> Result<Vec<BackupEntry>, String> {
    let temp_root = install_dir.join("update_extract.tmp");
    if temp_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    fs::create_dir_all(&temp_root).map_err(|e| format!("create temp dir: {e}"))?;

    let extract_root = extract_zip(zip_path, &temp_root, logger)?;
    logger.log(format!("extracted to {extract_root:?}"));

    let mut backups: Vec<BackupEntry> = Vec::new();
    let result = copy_tree_overwrite(&extract_root, install_dir, &mut backups, logger);

    let _ = fs::remove_dir_all(&temp_root);

    match result {
        Ok(()) => Ok(backups),
        Err(e) => {
            logger.log(format!("rollback after error: {e}"));
            rollback(&backups, logger);
            Err(e)
        }
    }
}

/// zip を展開し、`narou/` ディレクトリ (またはトップレベル) を返す。
fn extract_zip(zip_path: &Path, dest: &Path, logger: &mut Logger) -> Result<PathBuf, String> {
    let file = File::open(zip_path).map_err(|e| format!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("read zip: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;
        let entry_name = entry.name().to_string();
        let Some(rel) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            logger.log(format!("skip suspicious zip entry: {entry_name}"));
            continue;
        };
        let out_path = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("mkdir {out_path:?}: {e}"))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
        }
        let mut out = File::create(&out_path).map_err(|e| format!("create {out_path:?}: {e}"))?;
        io::copy(&mut entry, &mut out).map_err(|e| format!("write {out_path:?}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }

    let narou_dir = dest.join("narou");
    if narou_dir.is_dir() {
        Ok(narou_dir)
    } else {
        Ok(dest.to_path_buf())
    }
}

fn copy_tree_overwrite(
    src: &Path,
    dst: &Path,
    backups: &mut Vec<BackupEntry>,
    logger: &mut Logger,
) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("ensure dir {dst:?}: {e}"))?;
    let entries = fs::read_dir(src).map_err(|e| format!("read_dir {src:?}: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("file_type {:?}: {e}", entry.path()))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree_overwrite(&src_path, &dst_path, backups, logger)?;
        } else if file_type.is_file() {
            replace_file_with_backup(&src_path, &dst_path, backups, logger)?;
        }
    }
    Ok(())
}

fn replace_file_with_backup(
    src: &Path,
    dst: &Path,
    backups: &mut Vec<BackupEntry>,
    logger: &mut Logger,
) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
    }

    if dst.exists() {
        let backup_path = backup_path_for(dst);
        let _ = fs::remove_file(&backup_path);
        // Rename existing file to backup. Works on Windows even for the
        // running updater binary itself, because rename != delete.
        if let Err(e) = fs::rename(dst, &backup_path) {
            logger.log(format!("rename {dst:?} -> {backup_path:?} failed: {e}"));
            return Err(format!("rename to backup: {e}"));
        }
        backups.push(BackupEntry {
            target: dst.to_path_buf(),
            backup: backup_path,
        });
    }

    if let Err(e) = fs::copy(src, dst) {
        return Err(format!("copy {src:?} -> {dst:?}: {e}"));
    }
    Ok(())
}

fn backup_path_for(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".old");
    target.with_file_name(name)
}

fn rollback(backups: &[BackupEntry], logger: &mut Logger) {
    for entry in backups.iter().rev() {
        let _ = fs::remove_file(&entry.target);
        if let Err(e) = fs::rename(&entry.backup, &entry.target) {
            logger.log(format!(
                "rollback failed for {:?}: {} (backup left at {:?})",
                entry.target, e, entry.backup
            ));
        }
    }
}

fn cleanup_backups(backups: &[BackupEntry], logger: &mut Logger) {
    for entry in backups {
        if entry.backup.exists() {
            if let Err(e) = fs::remove_file(&entry.backup) {
                logger.log(format!(
                    "cleanup: cannot remove {:?}: {} (will retry on next update)",
                    entry.backup, e
                ));
            }
        }
    }
}

fn spawn_restart(
    restart: &[String],
    install_dir: &Path,
    logger: &mut Logger,
) -> Result<(), String> {
    let (program, rest) = restart.split_first().ok_or("--restart is empty")?;
    let program_path = resolve_program(program, install_dir);
    let mut command = Command::new(&program_path);
    command
        .args(rest)
        .current_dir(install_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach_command(&mut command);
    logger.log(format!("spawning restart: {program_path:?} {rest:?}"));
    let child = command
        .spawn()
        .map_err(|e| format!("spawn {program_path:?}: {e}"))?;
    drop(child);
    Ok(())
}

fn resolve_program(program: &str, install_dir: &Path) -> PathBuf {
    let candidate = install_dir.join(program);
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from(program)
}

#[cfg(windows)]
fn detach_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(windows))]
fn detach_command(_command: &mut Command) {
    // Best-effort: rely on the OS to keep the child alive after we exit.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_args() {
        let raw = vec![
            "--pid".to_string(),
            "1234".to_string(),
            "--zip".to_string(),
            "/tmp/x.zip".to_string(),
            "--install-dir".to_string(),
            "/opt/narou".to_string(),
            "--log".to_string(),
            "/tmp/u.log".to_string(),
            "--restart".to_string(),
            "narou_rs".to_string(),
            "web".to_string(),
            "--no-browser".to_string(),
        ];
        let args = parse_args_from(&raw).unwrap();
        assert_eq!(args.pid, Some(1234));
        assert_eq!(args.zip, Some(PathBuf::from("/tmp/x.zip")));
        assert_eq!(args.install_dir, Some(PathBuf::from("/opt/narou")));
        assert_eq!(args.log, Some(PathBuf::from("/tmp/u.log")));
        assert_eq!(
            args.restart,
            vec![
                "narou_rs".to_string(),
                "web".to_string(),
                "--no-browser".to_string()
            ]
        );
    }

    #[test]
    fn parse_unknown_argument_returns_error() {
        let raw = vec!["--bogus".to_string()];
        assert!(parse_args_from(&raw).is_err());
    }

    #[test]
    fn backup_path_appends_old_suffix() {
        assert_eq!(
            backup_path_for(Path::new("/foo/bar/narou_rs.exe")),
            PathBuf::from("/foo/bar/narou_rs.exe.old")
        );
    }

    #[test]
    fn copy_tree_overwrite_replaces_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), b"new-a").unwrap();
        fs::write(src.join("sub/b.txt"), b"new-b").unwrap();
        fs::write(dst.join("a.txt"), b"old-a").unwrap();

        let mut backups = Vec::new();
        let mut logger = Logger { file: None };
        copy_tree_overwrite(&src, &dst, &mut backups, &mut logger).unwrap();

        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"new-a");
        assert_eq!(fs::read(dst.join("sub/b.txt")).unwrap(), b"new-b");
        assert_eq!(backups.len(), 1);
        assert!(backups[0].backup.ends_with("a.txt.old"));
    }

    #[test]
    fn rollback_restores_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("file.txt");
        let backup = tmp.path().join("file.txt.old");
        fs::write(&target, b"new").unwrap();
        fs::write(&backup, b"original").unwrap();
        let backups = vec![BackupEntry {
            target: target.clone(),
            backup: backup.clone(),
        }];
        let mut logger = Logger { file: None };
        rollback(&backups, &mut logger);
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(!backup.exists());
    }
}
