//! ロガーは bin と lib で 1 つの状態を共有しなければならない。
//!
//! `src/logger.rs` を bin (`main.rs` の `mod logger`) と lib
//! (`narou_rs::logger`) の両方にコンパイルすると `LoggerState` が 2 つになり、
//! `logger::init()` が bin 側しか初期化しない。その状態では lib 側の logger を
//! 通る行 — `MessageSink` (`progress::console_sink`) や Downloader のレポート行
//! — がコンソールに出るのにログへ残らない (issue #36)。
//!
//! 実際のバイナリを動かして、sink 経由の行と bin 側の行が同じログファイルに
//! 並ぶことを固定する。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn narou_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_narou_rs"))
}

/// stdout / stderr は使わない (確認するのはログファイルだけ) ので pipe にしない。
fn run(cwd: &Path, home: &Path, args: &[&str]) {
    let status = Command::new(narou_binary())
        .args(args)
        .current_dir(cwd)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run narou_rs");
    assert!(
        status.code().is_some(),
        "narou_rs {args:?} did not exit normally"
    );
}

#[test]
fn sink_and_bin_lines_share_one_log_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    std::fs::write(root.join(".narou").join("local_setting.yaml"), "logging: true\n").unwrap();

    // sink 経由 (`MessageSink`) の行と、bin 側の `println!` の行を 1 回ずつ出す。
    // `convert` は対象が存在しない時点で sink へ 1 行出して終わる (通信不要)。
    run(root, root, &["convert", "zzz-not-exist"]);
    run(root, root, &["version"]);

    let log = std::fs::read_dir(root.join("log"))
        .expect("logging: true must create log/")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .next()
        .expect("a log file must exist");
    let text = std::fs::read_to_string(&log).unwrap();

    assert!(
        text.contains("zzz-not-exist は存在しません"),
        "MessageSink 経由の行がログに無い: {text}"
    );
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "bin 側の行がログに無い: {text}"
    );
}
