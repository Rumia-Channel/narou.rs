use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Parser;

const COMMAND_NAMES: &[&str] = &[
    "download", "update", "list", "convert", "diff", "setting", "alias", "inspect", "send",
    "folder", "browser", "remove", "freeze", "tag", "web", "mail", "backup", "csv", "clean", "log",
    "trace", "help", "version", "init",
];

fn build_shortcuts() -> HashMap<String, &'static str> {
    let mut map = HashMap::new();
    for &name in COMMAND_NAMES.iter().rev() {
        if let Some(c) = name.chars().next() {
            map.insert(c.to_string(), name);
        }
        if name.len() >= 2 {
            map.insert(name[..2].to_string(), name);
        }
    }
    map
}

pub struct GlobalFlags {
    pub no_color: bool,
    pub multiple: bool,
    pub show_time: bool,
    pub backtrace: bool,
    pub user_agent: Option<String>,
}

pub fn preprocess_args(args: &mut Vec<String>) -> GlobalFlags {
    let mut flags = GlobalFlags {
        no_color: false,
        multiple: false,
        show_time: false,
        backtrace: false,
        user_agent: None,
    };

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].clone();
        if arg == "--no-color" {
            flags.no_color = true;
            args.remove(i);
        } else if arg == "--multiple" {
            flags.multiple = true;
            args.remove(i);
        } else if arg == "--time" {
            flags.show_time = true;
            args.remove(i);
        } else if arg == "--backtrace" {
            flags.backtrace = true;
            args.remove(i);
        } else if arg == "--user-agent" {
            if i + 1 < args.len() {
                flags.user_agent = Some(args[i + 1].clone());
                args.remove(i);
                args.remove(i);
            } else {
                i += 1;
            }
        } else if arg.starts_with("--user-agent=") {
            flags.user_agent = Some(arg["--user-agent=".len()..].to_string());
            args.remove(i);
        } else if arg == "-h" || arg == "--help" {
            if i > 0 {
                resolve_command_shortcut(args, i);
                let cmd_name = args[i].clone();
                if crate::commands::help::display_command_help(&cmd_name) {
                    std::process::exit(0);
                }
            }
            args.clear();
            args.push("help".to_string());
            break;
        } else if arg == "-v" || arg == "--version" {
            args[i] = "version".to_string();
            break;
        } else if !arg.starts_with('-') {
            resolve_command_shortcut(args, i);
            break;
        } else {
            i += 1;
        }
    }

    if args.is_empty() {
        args.push("help".to_string());
    }

    if args.len() > 1 && args[1..].iter().any(|a| a == "-h" || a == "--help") {
        let cmd_name = args[0].clone();
        if crate::commands::help::display_command_help(&cmd_name) {
            std::process::exit(0);
        }
    }

    if flags.multiple {
        apply_multiple(args);
    }

    if args.first().is_some_and(|arg| arg == "diff") {
        expand_diff_short_number_options(args);
    }

    if !flags.no_color {
        flags.no_color = load_global_no_color();
    }

    flags
}

fn resolve_command_shortcut(args: &mut Vec<String>, cmd_index: usize) {
    let shortcuts = build_shortcuts();
    if let Some(resolved) = shortcuts.get(&args[cmd_index].to_lowercase()) {
        args[cmd_index] = resolved.to_string();
    }
}

fn apply_multiple(args: &mut Vec<String>) {
    let delimiter = load_multiple_delimiter();
    let cmd_index = match args.iter().position(|a| !a.starts_with('-')) {
        Some(idx) => idx,
        None => return,
    };
    let rest: Vec<String> = args.drain((cmd_index + 1)..).collect();
    for arg in &rest {
        for part in arg.split(&delimiter) {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                args.push(trimmed.to_string());
            }
        }
    }
}

fn expand_diff_short_number_options(args: &mut Vec<String>) {
    let mut expanded = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        if index > 0
            && arg.starts_with('-')
            && arg.len() > 1
            && arg[1..].chars().all(|ch| ch.is_ascii_digit())
        {
            expanded.push("-n".to_string());
            expanded.push(arg[1..].to_string());
        } else {
            expanded.push(arg.clone());
        }
    }
    *args = expanded;
}

fn load_multiple_delimiter() -> String {
    load_local_setting_value("multiple-delimiter").unwrap_or_else(|| ",".to_string())
}

fn load_global_no_color() -> bool {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").ok()
    } else {
        std::env::var("HOME").ok()
    };
    let Some(home) = home else { return false };
    let path = PathBuf::from(home).join(".narousetting/global_setting.yaml");
    if !path.exists() {
        return false;
    }
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(settings): Result<HashMap<String, serde_yaml::Value>, _> = serde_yaml::from_str(&raw)
    else {
        return false;
    };
    settings
        .get("no-color")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn inject_default_args(args: &mut Vec<String>) {
    if args.len() < 2 {
        return;
    }
    let cmd_name = &args[0];
    if cmd_name.starts_with('-') {
        return;
    }
    let has_positional_args = args[1..].iter().any(|a| !a.starts_with('-'));
    if has_positional_args {
        return;
    }
    if !is_terminal_stdin() {
        return;
    }

    let default_args = load_default_args(cmd_name);
    if !default_args.is_empty() {
        args.extend(default_args);
    }
}

pub fn inject_command_defaults(args: &mut Vec<String>) {
    if args.is_empty() {
        return;
    }

    match args[0].as_str() {
        "convert" => inject_convert_defaults(args),
        "log" => inject_log_defaults(args),
        _ => {}
    }
}

fn load_default_args(cmd_name: &str) -> Vec<String> {
    let key = format!("default_args.{}", cmd_name);
    load_local_setting_value(&key)
        .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
        .unwrap_or_default()
}

fn load_local_setting_value(key: &str) -> Option<String> {
    load_local_setting_raw_value(key).and_then(|value| match value {
        serde_yaml::Value::String(s) => Some(s),
        serde_yaml::Value::Number(v) => Some(v.to_string()),
        serde_yaml::Value::Bool(v) => Some(v.to_string()),
        _ => None,
    })
}

fn load_local_setting_bool(key: &str) -> Option<bool> {
    match load_local_setting_raw_value(key)? {
        serde_yaml::Value::Bool(v) => Some(v),
        serde_yaml::Value::Number(v) => v.as_i64().map(|n| n != 0),
        serde_yaml::Value::String(v) => parse_bool(&v),
        _ => None,
    }
}

fn load_local_setting_raw_value(key: &str) -> Option<serde_yaml::Value> {
    let dir = std::env::current_dir().ok()?;
    if !dir.join(".narou").exists() {
        return None;
    }
    let inv = narou_rs::db::inventory::Inventory::new(dir);
    let settings: HashMap<String, serde_yaml::Value> = inv
        .load(
            "local_setting",
            narou_rs::db::inventory::InventoryScope::Local,
        )
        .ok()?;
    settings.get(key).cloned()
}

fn inject_log_defaults(args: &mut Vec<String>) {
    let mut defaults = Vec::new();

    if !has_option(args, "-n", "--num") {
        if let Some(value) = load_local_setting_value("log.num") {
            defaults.push("--num".to_string());
            defaults.push(value);
        }
    }
    if !has_option(args, "-t", "--tail") && load_local_setting_bool("log.tail").unwrap_or(false) {
        defaults.push("--tail".to_string());
    }
    if !has_option(args, "-c", "--source-convert")
        && load_local_setting_bool("log.source-convert").unwrap_or(false)
    {
        defaults.push("--source-convert".to_string());
    }

    for token in defaults.into_iter().rev() {
        args.insert(1, token);
    }
}

fn inject_convert_defaults(args: &mut Vec<String>) {
    if !has_option(args, "-i", "--inspect")
        && load_local_setting_bool("convert.inspect").unwrap_or(false)
    {
        args.insert(1, "--inspect".to_string());
    }
    if !has_option(args, "", "--no-open")
        && load_local_setting_bool("convert.no-open").unwrap_or(false)
    {
        args.insert(1, "--no-open".to_string());
    }
}

fn has_option(args: &[String], short: &str, long: &str) -> bool {
    let long_prefix = format!("{}=", long);
    args.iter()
        .skip(1)
        .any(|arg| arg == short || arg == long || arg.starts_with(&long_prefix))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn is_terminal_stdin() -> bool {
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn version_flag_becomes_version_command() {
        let mut args = vec!["--version".to_string()];
        let _flags = preprocess_args(&mut args);
        assert_eq!(args, vec!["version".to_string()]);
    }

    #[test]
    fn version_command_keeps_more_option() {
        let mut args = vec!["--version".to_string(), "--more".to_string()];
        let _flags = preprocess_args(&mut args);
        assert_eq!(args[0], "version");
        assert!(args.iter().any(|arg| arg == "--more"));
    }

    #[test]
    fn diff_short_number_option_is_expanded() {
        let mut args = vec![
            "diff".to_string(),
            "1".to_string(),
            "-2".to_string(),
            "--no-tool".to_string(),
        ];
        expand_diff_short_number_options(&mut args);
        assert_eq!(
            args,
            vec![
                "diff".to_string(),
                "1".to_string(),
                "-n".to_string(),
                "2".to_string(),
                "--no-tool".to_string(),
            ]
        );
    }

    #[test]
    fn convert_no_open_is_injected_from_local_setting() {
        let root = std::env::temp_dir().join(format!(
            "narou-rs-cli-no-open-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".narou")).unwrap();
        let inventory = narou_rs::db::inventory::Inventory::new(root.clone());
        let mut settings = HashMap::new();
        settings.insert("convert.no-open".to_string(), serde_yaml::Value::Bool(true));
        inventory
            .save(
                "local_setting",
                narou_rs::db::inventory::InventoryScope::Local,
                &settings,
            )
            .unwrap();

        let current = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        let mut args = vec!["convert".to_string(), "1".to_string()];
        inject_command_defaults(&mut args);

        std::env::set_current_dir(current).unwrap();
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(args, vec!["convert", "--no-open", "1"]);
    }
}

#[derive(Parser, Debug)]
#[command(name = "narou", about = "narou.rs - A Rust port of narou.rb")]
pub struct Cli {
    #[arg(long, global = true)]
    pub user_agent: Option<String>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Subcommand, Debug)]
pub enum Commands {
    Init {
        #[arg(short = 'p', long = "path")]
        aozora_path: Option<String>,
        #[arg(short = 'l', long = "line-height")]
        line_height: Option<f64>,
    },
    Web {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
        #[arg(short, long, default_value_t = false)]
        no_browser: bool,
    },
    Download {
        targets: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'n', long)]
        no_convert: bool,
        #[arg(short = 'z', long)]
        freeze: bool,
        #[arg(short = 'r', long)]
        remove: bool,
        #[arg(short = 'm', long)]
        mail: bool,
    },
    Mail {
        targets: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
    },
    Send {
        args: Vec<String>,
        #[arg(short = 'w', long = "without-freeze")]
        without_freeze: bool,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'b', long = "backup-bookmark")]
        backup_bookmark: bool,
        #[arg(short = 'r', long = "restore-bookmark")]
        restore_bookmark: bool,
    },
    Backup {
        targets: Vec<String>,
    },
    Update {
        ids: Option<Vec<String>>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'n', long)]
        no_convert: bool,
        #[arg(short = 'a', long)]
        convert_only_new_arrival: bool,
        #[arg(long)]
        gl: Option<Option<String>>,
        #[arg(short = 's', long = "sort-by")]
        sort_by: Option<String>,
        #[arg(short = 'i', long)]
        ignore_all: bool,
    },
    Convert {
        #[arg(short = 'i', long)]
        inspect: bool,
        #[arg(long = "no-open")]
        no_open: bool,
        targets: Vec<String>,
    },
    Diff {
        target: Option<String>,
        view_diff_version: Option<String>,
        #[arg(short = 'n', long = "number")]
        number: Option<usize>,
        #[arg(short = 'l', long)]
        list: bool,
        #[arg(short = 'c', long = "clean")]
        clean: bool,
        #[arg(long = "all-clean")]
        all_clean: bool,
        #[arg(long)]
        no_tool: bool,
    },
    List {
        limit: Option<usize>,
        #[arg(short = 'l', long)]
        latest: bool,
        #[arg(long = "gl")]
        general_lastup: bool,
        #[arg(short = 'r', long)]
        reverse: bool,
        #[arg(short = 'u', long)]
        url: bool,
        #[arg(short = 'k', long)]
        kind: bool,
        #[arg(short = 's', long)]
        site: bool,
        #[arg(short = 'a', long)]
        author: bool,
        #[arg(short = 'f', long = "filter")]
        filter: Option<String>,
        #[arg(short = 'g', long = "grep")]
        grep: Option<String>,
        #[arg(short = 't', long = "tag", num_args = 0..=1)]
        tag: Option<Option<String>>,
        #[arg(short = 'e', long)]
        echo: bool,
        #[arg(long, hide = true)]
        frozen: bool,
    },
    Tag {
        #[arg(short = 'a', long = "add")]
        add: Option<String>,
        #[arg(short = 'd', long = "delete", alias = "remove")]
        delete: Option<String>,
        #[arg(short = 'c', long = "color")]
        color: Option<String>,
        #[arg(long = "clear")]
        clear: bool,
        #[arg(long = "no-overwrite-color", hide = true)]
        no_overwrite_color: bool,
        targets: Vec<String>,
    },
    Freeze {
        targets: Vec<String>,
        #[arg(short, long)]
        list: bool,
        #[arg(long)]
        on: bool,
        #[arg(long)]
        off: bool,
    },
    Remove {
        targets: Vec<String>,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short = 'w', long = "with-file")]
        with_file: bool,
        #[arg(long = "all-ss")]
        all_ss: bool,
    },
    Setting {
        args: Vec<String>,
        #[arg(short, long)]
        list: bool,
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        burn: bool,
    },
    Alias {
        args: Vec<String>,
        #[arg(short = 'l', long)]
        list: bool,
    },
    Folder {
        targets: Vec<String>,
        #[arg(short = 'n', long = "no-open")]
        no_open: bool,
    },
    Browser {
        targets: Vec<String>,
        #[arg(short = 'v', long)]
        vote: bool,
    },
    Clean {
        targets: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
        #[arg(short = 'a', long)]
        all: bool,
    },
    Inspect {
        targets: Vec<String>,
    },
    Csv {
        #[arg(short = 'o', long = "output")]
        output: Option<String>,
        #[arg(short = 'i', long = "import")]
        import: Option<String>,
    },
    Log {
        path: Option<String>,
        #[arg(short = 'n', long = "num", default_value_t = 20)]
        num: usize,
        #[arg(short = 't', long)]
        tail: bool,
        #[arg(short = 'c', long = "source-convert")]
        source_convert: bool,
    },
    Trace,
    Version {
        #[arg(short = 'm', long)]
        more: bool,
    },
}
