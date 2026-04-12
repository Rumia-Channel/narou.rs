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

fn load_default_args(cmd_name: &str) -> Vec<String> {
    let key = format!("default_args.{}", cmd_name);
    load_local_setting_value(&key)
        .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
        .unwrap_or_default()
}

fn load_local_setting_value(key: &str) -> Option<String> {
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
    settings
        .get(key)
        .and_then(|v: &serde_yaml::Value| v.as_str())
        .map(|s: &str| s.to_string())
}

fn is_terminal_stdin() -> bool {
    std::io::stdin().is_terminal()
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
    },
    Update {
        ids: Option<Vec<String>>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        no_convert: bool,
        #[arg(long)]
        sort_by: Option<String>,
    },
    Convert {
        targets: Vec<String>,
    },
    List {
        #[arg(short, long)]
        tag: Option<String>,
        #[arg(long)]
        frozen: bool,
    },
    Tag {
        #[arg(short, long)]
        add: Option<String>,
        #[arg(short, long)]
        remove: Option<String>,
        targets: Vec<String>,
    },
    Freeze {
        targets: Vec<String>,
        #[arg(long)]
        off: bool,
    },
    Remove {
        targets: Vec<String>,
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
}
