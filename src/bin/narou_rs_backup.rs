//! `narou_rs_backup` — ライブラリ全体バックアップ用のサブ実行ファイル。
//!
//! `小説データ/` と `.narou/` をまとめた zip を作成する。
//! narou_rs 本体の起動時バックアップ提案や Web UI からサブプロセスとして
//! 起動されるほか、単体で実行して任意のタイミングでバックアップできる。
//!
//! Usage:
//! ```text
//! narou_rs_backup [--root <LIBRARY_DIR>] [--output <DEST_DIR>]
//! ```
//! - `--root`: `.narou` を持つライブラリフォルダ。省略時はカレント
//!   ディレクトリから親方向へ `.narou` を探す。
//! - `--output`: zip の出力先フォルダ。省略時はこの exe と同じ
//!   フォルダの `backup/`。

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use narou_rs::startup_backup;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("Error: {}", message);
            wait_for_enter();
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;

    let root = match args.root {
        Some(dir) => dir,
        None => find_library_root()
            .ok_or_else(|| ".narou を持つライブラリフォルダが見つかりません".to_string())?,
    }.canonicalize().map_err(|error| error.to_string())?;
    if !root.join(".narou").is_dir() {
        return Err(format!(".narou が見つかりません: {}", root.display()));
    }

    let output = match args.output {
        Some(dir) => dir,
        None => startup_backup::default_backup_dir()
            .ok_or_else(|| "実行ファイルの場所を特定できません".to_string())?,
    };
    let output = if output.is_absolute() { output } else { std::env::current_dir().map_err(|error| error.to_string())?.join(output) };
    std::env::set_current_dir(&root).map_err(|error| error.to_string())?;
    narou_rs::db::init_database().map_err(|error| error.to_string())?;

    println!("対象: ライブラリフォルダ全体 (整合したスナップショットを作成します)");
    println!("保存先: {}", output.display());

    println!("バックアップを作成しています...");
    let path = startup_backup::create_library_backup(&root, &output).map_err(|e| e.to_string())?;
    println!("作成しました: {}", path.display());
    if !io::stdout().is_terminal() {
        println!("{}{}", startup_backup::BACKUP_RESULT_PREFIX,
            serde_json::to_string(&path).map_err(|error| error.to_string())?);
    }

    wait_for_enter();
    Ok(())
}

fn find_library_root() -> Option<PathBuf> {
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

/// ダブルクリック実行でウィンドウが即閉じないよう、対話端末では
/// Enter 待ちにする。パイプ/サブプロセスからは待たない。
fn wait_for_enter() {
    if !io::stdin().is_terminal() {
        return;
    }
    print!("\nEnter キーで終了します");
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
}

struct Args {
    root: Option<PathBuf>,
    output: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        root: None,
        output: None,
    };
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--root" => {
                let value = raw.get(i + 1).ok_or("--root requires a value")?;
                args.root = Some(PathBuf::from(value));
                i += 2;
            }
            "--output" | "-o" => {
                let value = raw.get(i + 1).ok_or("--output requires a value")?;
                args.output = Some(PathBuf::from(value));
                i += 2;
            }
            "-h" | "--help" => {
                println!("Usage: narou_rs_backup [--root <LIBRARY_DIR>] [--output <DEST_DIR>]");
                std::process::exit(0);
            }
            other => return Err(format!("不明な引数: {}", other)),
        }
    }
    Ok(args)
}

