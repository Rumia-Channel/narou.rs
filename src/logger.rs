use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

use chrono::Local;
use regex::Regex;

const DEFAULT_LOG_FORMAT_FILENAME: &str = "%Y%m%d.txt";
const DEFAULT_LOG_FORMAT_TIMESTAMP: &str = "[%H:%M:%S]";

static STATE: OnceLock<Mutex<LoggerState>> = OnceLock::new();
static SUPPRESS_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone)]
struct LoggerState {
    enabled: bool,
    log_dir: Option<PathBuf>,
    format_filename: String,
    format_timestamp: String,
    timestamp_disabled: bool,
    log_postfix: Option<String>,
    before_head_ln_by_path: HashMap<PathBuf, bool>,
}

impl LoggerState {
    fn disabled() -> Self {
        Self {
            enabled: false,
            log_dir: None,
            format_filename: DEFAULT_LOG_FORMAT_FILENAME.to_string(),
            format_timestamp: DEFAULT_LOG_FORMAT_TIMESTAMP.to_string(),
            timestamp_disabled: false,
            log_postfix: None,
            before_head_ln_by_path: HashMap::new(),
        }
    }

    fn load() -> Self {
        let Some(root_dir) = find_narou_root() else {
            return Self::disabled();
        };

        let local_setting_path = root_dir.join(".narou").join("local_setting.yaml");
        let settings = read_yaml_map(&local_setting_path);
        let logging_enabled = yaml_bool(settings.get("logging"));
        let logging_enabled =
            logging_enabled && std::env::var("NAROU_ENV").ok().as_deref() != Some("test");

        let log_dir = root_dir.join("log");
        if logging_enabled {
            let _ = fs::create_dir_all(&log_dir);
        }

        let format_filename = yaml_string(settings.get("logging.format-filename"))
            .unwrap_or_else(|| DEFAULT_LOG_FORMAT_FILENAME.to_string());
        let format_timestamp = yaml_string(settings.get("logging.format-timestamp"))
            .unwrap_or_else(|| DEFAULT_LOG_FORMAT_TIMESTAMP.to_string());
        let timestamp_disabled =
            format_timestamp.trim().is_empty() || format_timestamp.trim() == "$none";

        Self {
            enabled: logging_enabled,
            log_dir: Some(log_dir),
            format_filename,
            format_timestamp,
            timestamp_disabled,
            log_postfix: None,
            before_head_ln_by_path: HashMap::new(),
        }
    }

    fn current_log_path(&self) -> Option<PathBuf> {
        let dir = self.log_dir.as_ref()?;
        let filename = Local::now().format(&self.format_filename).to_string();
        let filename = apply_log_postfix(&filename, self.log_postfix.as_deref());
        Some(dir.join(filename))
    }
}

pub fn init() {
    let mut state = state().lock().expect("logger state lock poisoned");
    *state = LoggerState::load();
}

pub fn init_tracing(no_color: bool) {
    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(!no_color)
        .without_time()
        .with_writer(|| TracingWriter)
        .try_init();
}

pub fn without_logging<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    SUPPRESS_COUNT.fetch_add(1, AtomicOrdering::SeqCst);
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SUPPRESS_COUNT.fetch_sub(1, AtomicOrdering::SeqCst);
        }
    }
    let _guard = Reset;
    f()
}

pub fn emit_stdout(text: &str, newline: bool) {
    emit(text, newline, false);
}

pub fn emit_stderr(text: &str, newline: bool) {
    emit(text, newline, true);
}

pub fn log_dir() -> Option<PathBuf> {
    let state = state().lock().ok()?;
    state.log_dir.clone()
}

pub fn latest_log_path(source_convert: bool) -> Option<PathBuf> {
    let dir = log_dir()?;
    latest_log_path_in(&dir, source_convert)
}

pub(crate) fn latest_log_path_in(dir: &Path, source_convert: bool) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .map(|name| !name.to_string_lossy().starts_with('.'))
                .unwrap_or(true)
        })
        .collect();

    entries.sort_by(|a, b| {
        let time_cmp = modified_time(b)
            .partial_cmp(&modified_time(a))
            .unwrap_or(Ordering::Equal);
        if time_cmp == Ordering::Equal {
            a.to_string_lossy().cmp(&b.to_string_lossy())
        } else {
            time_cmp
        }
    });

    entries
        .into_iter()
        .find(|path| path_matches_source_convert(path, source_convert))
}

pub struct TracingWriter;

impl Write for TracingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        emit_stderr(&text, false);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn state() -> &'static Mutex<LoggerState> {
    STATE.get_or_init(|| Mutex::new(LoggerState::disabled()))
}

fn emit(text: &str, newline: bool, stderr: bool) {
    write_console(text, newline, stderr);

    if SUPPRESS_COUNT.load(AtomicOrdering::SeqCst) > 0 {
        return;
    }

    let mut state = match state().lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    if !state.enabled {
        return;
    }

    let Some(path) = state.current_log_path() else {
        return;
    };

    let mut chunk = text.to_string();
    if newline {
        chunk.push('\n');
    }

    let before_head_ln = *state.before_head_ln_by_path.get(&path).unwrap_or(&false);
    let (logged, before_head_ln) = embed_timestamp(
        &chunk,
        before_head_ln,
        &state.format_timestamp,
        state.timestamp_disabled,
    );
    state
        .before_head_ln_by_path
        .insert(path.clone(), before_head_ln);

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let logged = strip_ansi_codes(&logged);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(logged.as_bytes());
    }
}

fn write_console(text: &str, newline: bool, stderr: bool) {
    if stderr {
        let mut handle = io::stderr().lock();
        let _ = handle.write_all(text.as_bytes());
        if newline {
            let _ = handle.write_all(b"\n");
        }
        let _ = handle.flush();
    } else {
        let mut handle = io::stdout().lock();
        let _ = handle.write_all(text.as_bytes());
        if newline {
            let _ = handle.write_all(b"\n");
        }
        let _ = handle.flush();
    }
}

fn embed_timestamp(
    text: &str,
    before_head_ln: bool,
    format_timestamp: &str,
    timestamp_disabled: bool,
) -> (String, bool) {
    let mut output = text.to_string();
    let mut before_head_ln = before_head_ln;

    if !before_head_ln {
        output.insert(0, '\n');
        before_head_ln = true;
    }

    if output.ends_with('\n') {
        output.pop();
        if output.ends_with('\r') {
            output.pop();
        }
        before_head_ln = false;
    }

    if timestamp_disabled {
        return (output, before_head_ln);
    }

    let stamp = Local::now().format(format_timestamp).to_string();
    output = output.replace('\n', &format!("\n{} ", stamp));
    (output, before_head_ln)
}

fn strip_ansi_codes(text: &str) -> String {
    static ANSI_RE: OnceLock<Regex> = OnceLock::new();
    let re = ANSI_RE.get_or_init(|| Regex::new(r"\x1B\[[0-9;]*[A-Za-z]").expect("valid regex"));
    re.replace_all(text, "").to_string()
}

fn apply_log_postfix(filename: &str, postfix: Option<&str>) -> String {
    let Some(postfix) = postfix else {
        return filename.to_string();
    };
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let ext = path.extension().and_then(|s| s.to_str());
    match ext {
        Some(ext) if !ext.is_empty() => format!("{}{}.{}", stem, postfix, ext),
        _ => format!("{}{}", stem, postfix),
    }
}

fn path_matches_source_convert(path: &Path, source_convert: bool) -> bool {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ended_with_convert = stem.ends_with("_convert");
    if source_convert {
        ended_with_convert
    } else {
        !ended_with_convert
    }
}

fn modified_time(path: &Path) -> std::time::SystemTime {
    path.metadata()
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

pub(crate) fn find_narou_root() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        if current.join(".narou").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn read_yaml_map(path: &Path) -> HashMap<String, serde_yaml::Value> {
    let Ok(raw) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_yaml::from_str(&raw).unwrap_or_default()
}

fn yaml_bool(value: Option<&serde_yaml::Value>) -> bool {
    match value {
        Some(serde_yaml::Value::Bool(v)) => *v,
        Some(serde_yaml::Value::Number(n)) => n.as_i64().map(|v| v != 0).unwrap_or(false),
        Some(serde_yaml::Value::String(v)) => matches!(v.as_str(), "true" | "yes" | "on" | "1"),
        _ => false,
    }
}

fn yaml_string(value: Option<&serde_yaml::Value>) -> Option<String> {
    match value {
        Some(serde_yaml::Value::String(v)) => Some(v.clone()),
        Some(serde_yaml::Value::Number(v)) => Some(v.to_string()),
        Some(serde_yaml::Value::Bool(v)) => Some(v.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("narou_rs_{}_{}", prefix, nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn latest_log_path_filters_convert_logs() {
        let dir = temp_dir("logger_latest");
        let log_dir = dir.join("log");
        fs::create_dir_all(&log_dir).unwrap();

        let regular = log_dir.join("20260414.txt");
        let convert = log_dir.join("20260415_convert.txt");
        fs::write(&regular, "regular").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&convert, "convert").unwrap();

        assert_eq!(
            latest_log_path_in(&log_dir, false)
                .unwrap()
                .file_name()
                .unwrap(),
            regular.file_name().unwrap()
        );
        assert_eq!(
            latest_log_path_in(&log_dir, true)
                .unwrap()
                .file_name()
                .unwrap(),
            convert.file_name().unwrap()
        );

        let _ = fs::remove_dir_all(dir);
    }
}
