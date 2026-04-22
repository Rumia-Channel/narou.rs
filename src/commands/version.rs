use std::process::Command;

use narou_rs::compat::{
    configure_hidden_console_command, resolve_java_command_path, sanitize_java_command,
};
use narou_rs::version;

pub fn cmd_version(more: bool) {
    println!("{}", version::create_version_string());
    if more {
        version_more();
    }
}

fn version_more() {
    println!("  on {}", version::runtime_description());
    println!();

    let aozoraepub3_jar = version::aozoraepub3_jar_path().and_then(|path| std::fs::canonicalize(path).ok());
    let working_dir = aozoraepub3_jar
        .as_ref()
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()));

    let java_output = match run_java_command(working_dir.as_deref()) {
        Ok(output) => output,
        Err(_) => {
            println!("Java実行時にエラーが発生しました");
            return;
        }
    };

    print_process_output(&java_output);
    if !java_output.status.success() {
        println!("{}", java_output.status);
        println!("Java実行時にエラーが発生しました");
        return;
    }

    let Some(aozoraepub3_jar) = aozoraepub3_jar else {
        println!();
        println!("AozoraEpub3が見つかりません");
        return;
    };

    println!();

    let classpath = aozoraepub3_jar
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .unwrap_or_else(|| aozoraepub3_jar.to_string_lossy().to_string());

    let aozora_output = match run_aozora_command(working_dir.as_deref(), &classpath) {
        Ok(output) => output,
        Err(_) => {
            println!("AozoraEpub3実行時にエラーが発生しました");
            return;
        }
    };

    let stdout_text = String::from_utf8_lossy(&aozora_output.stdout);
    let stderr_text = String::from_utf8_lossy(&aozora_output.stderr);
    let output_text = format!("{}{}", stdout_text, stderr_text);
    let lines: Vec<&str> = output_text.lines().collect();

    if aozora_output.status.success()
        && lines.get(2).is_some_and(|line| line.starts_with(" -c,"))
        && lines.last().is_some_and(|line| line.starts_with(" -tf"))
    {
        if let Some(version_line) = lines.get(1) {
            println!("AozoraEpub3 {}", version_line.trim());
            return;
        }
    }

    print_stream(&stdout_text);
    print_stream(&stderr_text);
    println!("{}", aozora_output.status);
    if !aozora_output.status.success() {
        println!("AozoraEpub3実行時にエラーが発生しました");
    }
}

fn print_process_output(output: &std::process::Output) {
    print_stream(&String::from_utf8_lossy(&output.stdout));
    print_stream(&String::from_utf8_lossy(&output.stderr));
}

fn print_stream(text: &str) {
    print!("{}", text);
    if !text.ends_with('\n') {
        println!();
    }
}

fn run_java_command(
    current_dir: Option<&std::path::Path>,
) -> std::io::Result<std::process::Output> {
    let mut command = java_command()?;
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    command
        .arg("-Dfile.encoding=UTF-8")
        .arg("-Dstdout.encoding=UTF-8")
        .arg("-Dstderr.encoding=UTF-8")
        .arg("-Dsun.stdout.encoding=UTF-8")
        .arg("-Dsun.stderr.encoding=UTF-8")
        .arg("-version")
        .output()
}

fn run_aozora_command(
    current_dir: Option<&std::path::Path>,
    classpath: &str,
) -> std::io::Result<std::process::Output> {
    let mut command = java_command()?;
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    command
        .arg("-Dfile.encoding=UTF-8")
        .arg("-Dstdout.encoding=UTF-8")
        .arg("-Dstderr.encoding=UTF-8")
        .arg("-Dsun.stdout.encoding=UTF-8")
        .arg("-Dsun.stderr.encoding=UTF-8")
        .arg("-cp")
        .arg(classpath)
        .arg("AozoraEpub3")
        .arg("--help")
        .output()
}

fn java_command() -> std::io::Result<Command> {
    let java_path = resolve_java_command_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "java not found"))?;
    let mut command = Command::new(java_path);
    configure_hidden_console_command(&mut command);
    sanitize_java_command(&mut command);
    Ok(command)
}
