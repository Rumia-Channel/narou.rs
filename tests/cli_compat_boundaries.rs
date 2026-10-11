use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(args)
        .current_dir(root)
        .env("USERPROFILE", root.join("home"))
        .env("HOME", root.join("home"))
        .env("NAROU_RS_LEGACY_YAML", "1")
        .env_remove("NAROU_ENV")
        .stdin(Stdio::null())
        .output().unwrap()
}

#[test]
fn invalid_command_and_option_report_to_stdout_with_ruby_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".narou")).unwrap();
    for (args, message) in [
        (vec!["--no-color", "unknown-command"], "[ERROR] 不明なコマンドです。narou help を確認してください"),
        (vec!["--no-color", "list", "--bogus"], "[ERROR] 不明なオプションです(invalid option: --bogus)"),
    ] {
        let output = run(temp.path(), &args);
        assert_eq!(output.status.code(), Some(127));
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), message);
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let output = run(temp.path(), &["freeze", "999999999"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "999999999 は存在しません");
    assert!(output.stderr.is_empty());
}

#[test]
fn legacy_command_default_arguments_remain_readable_and_writable() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".narou")).unwrap();
    for command in ["restore346", "console"] {
        let name = format!("default_args.{command}");
        let assignment = format!("{name}=--example");
        let output = run(temp.path(), &["setting", &assignment]);
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let output = run(temp.path(), &["setting", &name]);
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "--example");
    }
}

#[test]
fn library_global_settings_take_precedence_without_modifying_home_settings() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join(".narou")).unwrap();
    std::fs::create_dir(root.join(".narousetting")).unwrap();
    std::fs::create_dir_all(root.join("home/.narousetting")).unwrap();
    let local = root.join(".narousetting/global_setting.yaml");
    let home = root.join("home/.narousetting/global_setting.yaml");
    std::fs::write(&local, "line-height: 2.5\n").unwrap();
    std::fs::write(&home, "line-height: 1.8\n").unwrap();
    let output = run(root, &["setting", "line-height"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "2.5");
    let output = run(root, &["setting", "line-height=2.6"]);
    assert!(output.status.success());
    let settings: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(local).unwrap()).unwrap();
    assert_eq!(settings["line-height"].as_f64(), Some(2.6));
    assert_eq!(std::fs::read_to_string(home).unwrap(), "line-height: 1.8\n");
}
