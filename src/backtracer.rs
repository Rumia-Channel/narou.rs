use std::any::Any;
use std::backtrace::Backtrace;
use std::fs;
use std::path::PathBuf;

use chrono::Local;

use crate::logger;

const LOG_NAME: &str = "trace_dump.txt";
const LOG_FORMAT_TIMESTAMP: &str = "%Y/%m/%d %H:%M:%S";

pub fn log_path() -> PathBuf {
    if let Some(root_dir) = logger::find_narou_root() {
        root_dir.join(LOG_NAME)
    } else {
        PathBuf::from(LOG_NAME)
    }
}

pub fn save_log(argv: &[String], panic_info: &(dyn Any + Send)) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, build_log(argv, panic_info));
}

fn build_log(argv: &[String], panic_info: &(dyn Any + Send)) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "--- {} ---\n",
        Local::now().format(LOG_FORMAT_TIMESTAMP)
    ));
    output.push_str(&build_command(argv));
    output.push_str("\n\n");
    output.push_str(&format!("panic: {}\n", panic_message(panic_info)));
    output.push_str(&format!("{}", Backtrace::force_capture()));
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn build_command(argv: &[String]) -> String {
    let mut command = std::env::args()
        .next()
        .unwrap_or_else(|| "narou".to_string());
    for arg in argv {
        command.push(' ');
        command.push_str(arg);
    }
    command
}

fn panic_message(panic_info: &(dyn Any + Send)) -> String {
    if let Some(s) = panic_info.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic_info.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("narou_rs_{}_{}", prefix, nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_current_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = CWD_LOCK.lock().unwrap();
        let current = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let result = f();
        std::env::set_current_dir(current).unwrap();
        result
    }

    #[test]
    fn log_path_uses_root_dir_when_initialized() {
        let dir = temp_dir("backtracer_root");
        fs::create_dir_all(dir.join(".narou")).unwrap();
        let path = with_current_dir(&dir, log_path);
        assert_eq!(path, dir.join("trace_dump.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn log_path_falls_back_to_cwd_without_root() {
        let dir = temp_dir("backtracer_cwd");
        let path = with_current_dir(&dir, log_path);
        assert_eq!(path, PathBuf::from("trace_dump.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_log_writes_trace_file() {
        let dir = temp_dir("backtracer_save");
        fs::create_dir_all(dir.join(".narou")).unwrap();
        with_current_dir(&dir, || {
            let panic = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
            save_log(&["trace".to_string(), "foo".to_string()], panic.as_ref());
        });

        let content = fs::read_to_string(dir.join("trace_dump.txt")).unwrap();
        assert!(content.contains("trace foo"));
        assert!(content.contains("panic: boom"));
        let _ = fs::remove_dir_all(dir);
    }
}
