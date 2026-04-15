use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::logger;
use narou_rs::termcolor::bold_colored;

pub fn cmd_log(
    path: Option<&str>,
    num: usize,
    tail: bool,
    source_convert: bool,
) -> Result<(), String> {
    logger::without_logging(|| cmd_log_inner(path, num, tail, source_convert))
}

pub fn report_error(message: &str) {
    logger::without_logging(|| {
        if narou_rs::progress::is_web_mode() {
            eprintln!("{} {}", bold_colored("[ERROR]", "red"), message);
        } else if std::env::var_os("NO_COLOR").is_some() {
            eprintln!("[ERROR] {}", message);
        } else {
            eprintln!("\x1b[1;31m[ERROR]\x1b[0m {}", message);
        }
    });
}

fn cmd_log_inner(
    path: Option<&str>,
    num: usize,
    tail: bool,
    source_convert: bool,
) -> Result<(), String> {
    let log_path = match path.filter(|path| !path.trim().is_empty()) {
        Some(path) => resolve_path(path)?,
        None => logger::latest_log_path(source_convert)
            .ok_or_else(|| "表示できるログファイルが一つも見つかりませんでした".to_string())?,
    };

    println!("{}", colorize_path(&log_path.display().to_string()));

    let reader = TailReader::new(log_path, num);
    if tail {
        reader.stream()
    } else {
        reader.display()
    }
}

fn resolve_path(path: &str) -> Result<PathBuf, String> {
    let expanded = expand_tilde(path);
    let fullpath = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(expanded)
    };

    if !fullpath.exists() {
        return Err(format!("{} が存在しません", path));
    }

    Ok(fullpath.canonicalize().unwrap_or(fullpath))
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn colorize_path(text: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        text.to_string()
    } else {
        format!("\x1b[36m{}\x1b[0m", text)
    }
}

struct TailReader {
    path: PathBuf,
    num: usize,
}

impl TailReader {
    fn new(path: PathBuf, num: usize) -> Self {
        Self { path, num }
    }

    fn display(&self) -> Result<(), String> {
        self.ensure_num()?;
        let mut file = File::open(&self.path).map_err(|e| e.to_string())?;
        let offset = tail_offset(&mut file, self.num).map_err(|e| e.to_string())?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(offset)))
            .map_err(|e| e.to_string())?;

        let mut buf = String::new();
        file.read_to_string(&mut buf).map_err(|e| e.to_string())?;
        print!("{}", buf);
        println!();
        Ok(())
    }

    fn stream(&self) -> Result<(), String> {
        self.ensure_num()?;
        let mut file = File::open(&self.path).map_err(|e| e.to_string())?;
        let offset = tail_offset(&mut file, self.num).map_err(|e| e.to_string())?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(offset)))
            .map_err(|e| e.to_string())?;

        loop {
            let mut buf = String::new();
            let bytes = file.read_to_string(&mut buf).map_err(|e| e.to_string())?;
            if bytes > 0 {
                print!("{}", buf);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn ensure_num(&self) -> Result<(), String> {
        if self.num == 0 {
            return Err("表示する行数は1以上にしてください".to_string());
        }
        Ok(())
    }
}

fn tail_offset(file: &mut File, count: usize) -> std::io::Result<u64> {
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(0);
    }

    let chunk_size = 16 * 1024u64;
    let mut n = size / chunk_size;
    if size == n * chunk_size && n > 0 {
        n -= 1;
    }

    let mut len = size - n * chunk_size;
    let mut offset = 0u64;
    let mut remaining = count as isize;

    loop {
        file.seek(SeekFrom::Start(n * chunk_size))?;
        let mut buf = vec![0u8; len as usize];
        let read = file.read(&mut buf)?;
        let chunk = &buf[..read];

        for i in 0..chunk.len() {
            let idx = chunk.len() - i - 1;
            let chr = chunk[idx];
            if chr == b'\n' || (offset == 0 && i == 0 && chr != b'\n') {
                remaining -= 1;
                if remaining < 0 {
                    offset += i as u64;
                    return Ok(offset);
                }
            }
        }

        offset += chunk.len() as u64;
        if n == 0 {
            break;
        }
        n -= 1;
        len = chunk_size;
    }

    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::latest_log_path_in;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("narou_rs_{}_{}", name, nanos));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.txt");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn tail_offset_returns_last_two_lines() {
        let path = temp_file("tail_offset", "a\nb\nc\n");
        let mut file = File::open(&path).unwrap();
        let offset = tail_offset(&mut file, 2).unwrap();
        let size = file.metadata().unwrap().len();
        file.seek(SeekFrom::Start(size - offset)).unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "b\nc\n");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn latest_log_path_ignores_hidden_files() {
        let dir = temp_file("hidden_filter", "regular");
        let log_dir = dir.parent().unwrap().to_path_buf();
        let _ = fs::remove_file(&dir);
        let regular = log_dir.join("20260414.txt");
        let hidden = log_dir.join(".20260415.txt");
        fs::write(&regular, "regular").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&hidden, "hidden").unwrap();

        assert_eq!(
            latest_log_path_in(&log_dir, false)
                .unwrap()
                .file_name()
                .unwrap(),
            regular.file_name().unwrap()
        );

        let _ = fs::remove_dir_all(log_dir);
    }
}
