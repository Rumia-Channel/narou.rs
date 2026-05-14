use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cargo local-build: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let options = Options::parse(env::args_os().skip(1))?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = root.join("target");
    let target_release_dir = match &options.target {
        Some(target) => target_dir.join(target).join("release"),
        None => target_dir.join("release"),
    };
    let exe_suffix = env::consts::EXE_SUFFIX;
    let app_binary = target_release_dir.join(format!("narou_rs{exe_suffix}"));
    let updater_binary = target_release_dir.join(format!("narou_rs_updater{exe_suffix}"));

    build_release_binary(&options, "narou_rs_updater")?;
    let updater_hash = sha256_file(&updater_binary)
        .map_err(|err| format!("failed to hash {}: {err}", updater_binary.display()))?;
    build_app_binary(&options, &updater_hash)?;
    create_local_package(&root, &app_binary, &updater_binary)?;

    println!("Created {}", root.join("narou").display());
    Ok(())
}

struct Options {
    locked: bool,
    target: Option<String>,
}

impl Options {
    fn parse(args: impl Iterator<Item = OsString>) -> Result<Self, String> {
        let mut locked = false;
        let mut target = None;
        let mut iter = args.peekable();
        while let Some(arg) = iter.next() {
            let arg = arg
                .into_string()
                .map_err(|_| "arguments must be valid UTF-8".to_string())?;
            match arg.as_str() {
                "--locked" => locked = true,
                "--release" => {}
                "--target" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--target requires a value".to_string())?
                        .into_string()
                        .map_err(|_| "--target value must be valid UTF-8".to_string())?;
                    target = Some(value);
                }
                "-h" | "--help" => return Err(help_text()),
                other if other.starts_with("--target=") => {
                    target = Some(other["--target=".len()..].to_string());
                }
                other => return Err(format!("unsupported argument: {other}\n\n{}", help_text())),
            }
        }
        Ok(Self { locked, target })
    }
}

fn help_text() -> String {
    "usage: cargo local-build [--locked] [--target <triple>]\n\
     Builds release binaries and creates a local narou/ package directory."
        .to_string()
}

fn build_release_binary(options: &Options, bin: &str) -> Result<(), String> {
    let mut command = cargo_build_command(options);
    command.args(["--bin", bin]);
    run_command(&mut command, &format!("failed to build {bin}"))
}

fn build_app_binary(options: &Options, updater_hash: &str) -> Result<(), String> {
    let mut command = cargo_build_command(options);
    command.args(["--bin", "narou_rs"]);
    command.env("NAROU_RS_RELEASE_BUILD", "1");
    command.env("NAROU_RS_UPDATER_SHA256", updater_hash);
    run_command(&mut command, "failed to build narou_rs")
}

fn cargo_build_command(options: &Options) -> Command {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")));
    command.args(["build", "--release"]);
    if options.locked {
        command.arg("--locked");
    }
    if let Some(target) = &options.target {
        command.args(["--target", target]);
    }
    command
}

fn run_command(command: &mut Command, context: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|err| format!("{context}: failed to spawn command: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{context}: command exited with {status}"))
    }
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

fn create_local_package(
    root: &Path,
    app_binary: &Path,
    updater_binary: &Path,
) -> Result<(), String> {
    let package_root = root.join("narou");
    if !app_binary.is_file() {
        return Err(format!("binary not found: {}", app_binary.display()));
    }
    if !updater_binary.is_file() {
        return Err(format!(
            "updater binary not found: {}",
            updater_binary.display()
        ));
    }

    if package_root.exists() {
        fs::remove_dir_all(&package_root)
            .map_err(|err| format!("failed to remove {}: {err}", package_root.display()))?;
    }
    fs::create_dir_all(&package_root)
        .map_err(|err| format!("failed to create {}: {err}", package_root.display()))?;

    copy_file(app_binary, &package_root.join(file_name(app_binary)?))?;
    let updater_name = format!("{}.new", file_name(updater_binary)?);
    copy_file(updater_binary, &package_root.join(updater_name))?;

    for dir in ["webnovel", "preset"] {
        copy_dir_recursive(&root.join(dir), &package_root.join(dir))?;
    }
    for file in ["LICENSE", "README.md", "Third-Party-License.md"] {
        let source = root.join(file);
        if source.is_file() {
            copy_file(&source, &package_root.join(file))?;
        }
    }
    fs::write(
        package_root.join("commitversion"),
        resolve_commit_version(root),
    )
    .map_err(|err| format!("failed to write commitversion: {err}"))?;
    Ok(())
}

fn file_name(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("path has no valid file name: {}", path.display()))
}

fn copy_file(source: &Path, dest: &Path) -> Result<(), String> {
    fs::copy(source, dest).map(|_| ()).map_err(|err| {
        format!(
            "failed to copy {} to {}: {err}",
            source.display(),
            dest.display()
        )
    })
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<(), String> {
    if !source.is_dir() {
        return Err(format!(
            "required resource directory not found: {}",
            source.display()
        ));
    }
    fs::create_dir_all(dest)
        .map_err(|err| format!("failed to create {}: {err}", dest.display()))?;
    for entry in
        fs::read_dir(source).map_err(|err| format!("failed to read {}: {err}", source.display()))?
    {
        let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if source_path.is_file() {
            copy_file(&source_path, &dest_path)?;
        }
    }
    Ok(())
}

fn resolve_commit_version(root: &Path) -> String {
    let output = Command::new("git")
        .args(["describe", "--always"])
        .current_dir(root)
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    env!("CARGO_PKG_VERSION").to_string()
}
