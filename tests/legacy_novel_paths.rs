//! Regression test for legacy narou.rb-era directory names.
//!
//! Some existing archives store the novel directory under a `‛` (U+201B)
//! marker encoding (`‛：` for `：`, `‛‛％` for `％`, `‛．` for a trailing
//! `．`, ...) instead of the canonical name the record's `file_title`
//! produces. `narou export` must read such directories without renaming
//! anything or rewriting `database.yaml`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn narou_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_narou_rs"))
}

fn run(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(narou_binary())
        .args(args)
        .current_dir(cwd)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env_remove("NAROU_RS_LEGACY_YAML")
        .env_remove("NAROU_RS_RESTART_CWD")
        .env_remove("NAROU_RS_MIRROR_FILES")
        .stdin(Stdio::null())
        .output()
        .expect("run narou_rs")
}

/// Builds a minimal YAML-mode library whose novel directory lives under an
/// escaped folder name that differs from the record's `file_title`.
fn create_library(root: &Path, file_title: &str, disk_leaf: &str, subdirectory: Option<&str>) {
    let narou_dir = root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    let yaml_sub = if subdirectory.is_some() { "true" } else { "false" };
    std::fs::write(
        narou_dir.join("database.yaml"),
        format!(
            "---\n\
             3:\n\
             \x20 id: 3\n\
             \x20 author: 作者\n\
             \x20 title: 作品名\n\
             \x20 file_title: {file_title}\n\
             \x20 toc_url: https://example.test/novel/1/\n\
             \x20 sitename: ExampleSite\n\
             \x20 novel_type: 1\n\
             \x20 end: false\n\
             \x20 last_update: 2026-04-20 00:00:00.000000000 +09:00\n\
             \x20 use_subdirectory: {yaml_sub}\n\
             \x20 general_all_no: 1\n\
             \x20 is_narou: false\n",
        ),
    )
    .unwrap();
    let mut novel_dir = root.join("小説データ").join("ExampleSite");
    if let Some(subdirectory) = subdirectory {
        novel_dir = novel_dir.join(subdirectory);
    }
    novel_dir = novel_dir.join(disk_leaf);
    std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
    std::fs::write(
        novel_dir.join("toc.yaml"),
        "---\ntitle: 作品名\nauthor: 作者\ntoc_url: https://example.test/novel/1/\n\
         subtitles:\n  - index: '1'\n    subtitle: 第一話\n    file_subtitle: 第一話\n",
    )
    .unwrap();
    std::fs::write(
        novel_dir.join("本文").join("1 第一話.yaml"),
        "---\nsubtitle: 第一話\nbody: |\n  本文です\n",
    )
    .unwrap();
    std::fs::write(novel_dir.join("setting.ini"), "[global]\n").unwrap();
}

/// Exports record 3 and returns the parsed archive.
fn export_archive(root: &Path, temp: &Path) -> narou_rs::transfer::ParsedArchive {
    let zip = temp.join("export.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    let output = run(root, root, &["export", "3", "-o", zip_arg.as_str()]);
    assert!(
        output.status.success(),
        "narou export failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&zip).unwrap();
    narou_rs::transfer::parse_archive(&bytes).unwrap()
}

#[test]
fn export_reads_novel_dir_escaped_with_fullwidth_marker() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    // file_title carries `：`; the directory on disk is `‛：`-escaped.
    let file_title = "n6247ex 天才詐欺師（旧題：異世界（インチキ）ヒーロー）";
    create_library(
        &source,
        file_title,
        "n6247ex 天才詐欺師（旧題\u{201b}：異世界（インチキ）ヒーロー）",
        None,
    );

    let archive = export_archive(&source, temp.path());
    assert_eq!(archive.novels.len(), 1);
    let paths: Vec<&str> = archive.novels[0]
        .files
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(paths.contains(&"toc.yaml"), "{paths:?}");
    assert!(
        paths.contains(&"本文/1 第一話.yaml"),
        "section file must be read from the escaped directory: {paths:?}"
    );
}

#[test]
fn export_reads_escaped_novel_dir_inside_subdirectory_layout() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    // use_subdirectory: the leaf sits under `<site>/12/<escaped leaf>`.
    let file_title = "n1234ab Re：Creatorの物語";
    create_library(
        &source,
        file_title,
        "n1234ab Re\u{201b}：Creatorの物語",
        Some("12"),
    );

    let archive = export_archive(&source, temp.path());
    assert_eq!(archive.novels.len(), 1);
    let paths: Vec<&str> = archive.novels[0]
        .files
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(paths.contains(&"toc.yaml"), "{paths:?}");
    assert!(paths.contains(&"本文/1 第一話.yaml"), "{paths:?}");
}

#[test]
fn export_prefers_canonical_dir_but_decodes_percent_and_trailing_dot() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    // `％` is written `‛‛％`; a name-final `．` is written `‛．`.
    let file_title = "n5150 成功率100％の物語．";
    create_library(
        &source,
        file_title,
        "n5150 成功率100\u{201b}\u{201b}％の物語\u{201b}．",
        None,
    );

    let archive = export_archive(&source, temp.path());
    assert_eq!(archive.novels.len(), 1);
    let paths: Vec<&str> = archive.novels[0]
        .files
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(paths.contains(&"toc.yaml"), "{paths:?}");
}
