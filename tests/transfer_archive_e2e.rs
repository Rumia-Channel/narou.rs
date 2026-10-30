//! End-to-end CLI test for `narou export` / `narou import`.
//!
//! A small YAML-mode library is created in a `tempfile::TempDir`, exported to a
//! ZIP, imported into a fresh library, and finally re-imported with `--overwrite`
//! to prove the file replacement path. `%TEMP%` is never used directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TOC_URL: &str = "https://example.test/novel/1/";
const TITLE: &str = "作品名";
const SECTION_FILE: &str = "本文/1 第一話.yaml";

fn narou_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_narou_rs"))
}

/// Runs the real binary with an isolated global-setting home.
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

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn create_source_library(root: &Path) {
    let narou_dir = root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    std::fs::write(
        narou_dir.join("database.yaml"),
        r#"---
3:
  id: 3
  author: 作者
  title: 作品名
  file_title: example-title
  toc_url: https://example.test/novel/1/
  sitename: ExampleSite
  novel_type: 1
  end: false
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  use_subdirectory: false
  tags:
    - タグ
  ncode:
  domain: example.test
  general_all_no: 1
  is_narou: false
"#,
    )
    .unwrap();

    let novel_dir = root
        .join("小説データ")
        .join("ExampleSite")
        .join("example-title");
    std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
    std::fs::create_dir_all(novel_dir.join("backup")).unwrap();
    std::fs::write(
        novel_dir.join("toc.yaml"),
        r#"---
title: 作品名
author: 作者
toc_url: https://example.test/novel/1/
story: あらすじ
subtitles:
  - index: '1'
    href: '1'
    chapter: ''
    subchapter: ''
    subtitle: 第一話
    file_subtitle: 第一話
    subdate: '2026-04-20'
novel_type: 1
"#,
    )
    .unwrap();
    std::fs::write(
        novel_dir.join(SECTION_FILE),
        "---\nsubtitle: 第一話\nbody: |\n  本文です\n",
    )
    .unwrap();
    std::fs::write(novel_dir.join("setting.ini"), "[global]\n").unwrap();
    // `backup/` must never travel in a transfer archive.
    std::fs::write(novel_dir.join("backup").join("old.zip"), b"old backup").unwrap();
    std::fs::write(novel_dir.join("toc.yaml.lock"), b"lock").unwrap();
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|value| value.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

#[test]
fn export_then_import_roundtrips_a_novel_into_a_fresh_library() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    create_source_library(&source);
    std::fs::create_dir_all(target.join(".narou")).unwrap();

    let zip = temp.path().join("roundtrip.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    let exported = run(&source, &source, &["export", "3", "-o", zip_arg.as_str()]);
    assert_success(&exported, "narou export");
    assert!(
        stdout(&exported).contains("書き出しました"),
        "{}",
        stdout(&exported)
    );
    assert!(zip.is_file(), "{} was not written", zip.display());

    // The archive core accepts the file and contains exactly the novel data.
    let bytes = std::fs::read(&zip).unwrap();
    let parsed = narou_rs::transfer::parse_archive(&bytes).unwrap();
    assert_eq!(parsed.novels.len(), 1);
    assert_eq!(parsed.novels[0].toc_url, TOC_URL);
    assert_eq!(parsed.novels[0].title, TITLE);
    let archived_paths: Vec<&str> = parsed.novels[0]
        .files
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(archived_paths.contains(&"toc.yaml"), "{archived_paths:?}");
    assert!(archived_paths.contains(&SECTION_FILE), "{archived_paths:?}");
    assert!(
        archived_paths
            .iter()
            .all(|path| !path.starts_with("backup/")),
        "{archived_paths:?}"
    );
    assert!(
        archived_paths.iter().all(|path| !path.ends_with(".lock")),
        "{archived_paths:?}"
    );
    let metadata = &parsed.manifest.novels[0];
    assert_eq!(metadata.author, "作者");
    assert_eq!(metadata.sitename, "ExampleSite");
    assert_eq!(metadata.domain.as_deref(), Some("example.test"));
    assert_eq!(metadata.tags, vec!["タグ"]);
    assert_eq!(metadata.attribute_tag.as_deref(), Some("小説"));
    assert_eq!(metadata.subtitle.as_deref(), Some("あらすじ"));

    // Import into the pristine library.
    let imported = run(&target, &target, &["import", zip_arg.as_str()]);
    assert_success(&imported, "narou import");
    let summary = stdout(&imported);
    assert!(summary.contains("新規: 1 件"), "{summary}");

    let database = std::fs::read_to_string(target.join(".narou").join("database.yaml")).unwrap();
    assert!(database.contains(TOC_URL), "{database}");
    assert!(database.contains(TITLE), "{database}");

    let toc = find_file(&target.join("小説データ"), "toc.yaml").expect("imported toc.yaml");
    let novel_dir = toc.parent().unwrap();
    assert_eq!(
        novel_dir.file_name().and_then(|value| value.to_str()),
        Some(TITLE)
    );
    assert_eq!(
        novel_dir
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str()),
        Some("ExampleSite")
    );
    assert!(novel_dir.join(SECTION_FILE).is_file());
    assert!(
        !novel_dir.join("backup").exists(),
        "backup/ must not be recreated from the archive"
    );

    // A second import without --overwrite leaves the library alone.
    let skipped = run(&target, &target, &["import", zip_arg.as_str()]);
    assert_success(&skipped, "narou import (skip)");
    assert!(
        stdout(&skipped).contains("スキップ: 1"),
        "{}",
        stdout(&skipped)
    );

    // --overwrite restores the archived body over a local edit.
    let section = novel_dir.join(SECTION_FILE);
    std::fs::write(
        &section,
        "---\nsubtitle: 第一話\nbody: |\n  ローカルで編集\n",
    )
    .unwrap();
    let overwritten = run(
        &target,
        &target,
        &["import", zip_arg.as_str(), "--overwrite"],
    );
    assert_success(&overwritten, "narou import --overwrite");
    assert!(
        stdout(&overwritten).contains("上書き: 1"),
        "{}",
        stdout(&overwritten)
    );
    let restored = std::fs::read_to_string(&section).unwrap();
    assert!(restored.contains("本文です"), "{restored}");
    assert!(!restored.contains("ローカルで編集"), "{restored}");

    // Overwrite kept the same record (no duplicate was created).
    let database = std::fs::read_to_string(target.join(".narou").join("database.yaml")).unwrap();
    assert_eq!(
        database.matches(TOC_URL).count(),
        1,
        "overwrite must keep one record: {database}"
    );
}

#[test]
fn export_without_targets_exits_non_zero_with_a_clear_message() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".narou")).unwrap();

    let output = run(root, root, &["export"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("書き出す小説を指定してください"),
        "{stderr}"
    );
}

/// SQLite ("Lite") libraries keep the authoritative bytes in `db.sqlite` and
/// mirror them below `小説データ/`. An import must update the stored rows, not
/// only the mirror: a stored row wins over the mirror, so an overwrite that
/// only touched files would stay invisible to the rest of the application.
///
/// The final `db export-yaml` renders the rollback bundle *from the store*
/// with the mirror body deleted, so it observes exactly what the import wrote
/// into SQLite.
#[test]
fn sqlite_library_import_updates_stored_rows() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    create_source_library(&source);

    let zip_first = temp.path().join("first.zip");
    let zip_first_arg = zip_first.to_string_lossy().to_string();
    let exported = run(
        &source,
        &source,
        &["export", "3", "-o", zip_first_arg.as_str()],
    );
    assert_success(&exported, "narou export (first)");

    // A second archive carries a different body for the same novel.
    let source_section = source
        .join("小説データ")
        .join("ExampleSite")
        .join("example-title")
        .join(SECTION_FILE);
    std::fs::write(
        &source_section,
        "---\nsubtitle: 第一話\nbody: |\n  更新版の本文です\n",
    )
    .unwrap();
    let zip_second = temp.path().join("second.zip");
    let zip_second_arg = zip_second.to_string_lossy().to_string();
    let exported = run(
        &source,
        &source,
        &["export", "3", "-o", zip_second_arg.as_str()],
    );
    assert_success(&exported, "narou export (second)");

    // Fresh SQLite-mode library (mirror enabled, the common "Lite" layout).
    let target = temp.path().join("lite");
    std::fs::create_dir_all(target.join(".narou")).unwrap();
    std::fs::write(target.join(".narou").join("storage-backend"), "sqlite\n").unwrap();

    let first = run(&target, &target, &["import", zip_first_arg.as_str()]);
    assert_success(&first, "sqlite import (create)");
    assert!(stdout(&first).contains("新規: 1 件"), "{}", stdout(&first));

    let toc = find_file(&target.join("小説データ"), "toc.yaml").expect("mirrored toc.yaml");
    let novel_dir = toc.parent().unwrap().to_path_buf();
    assert!(novel_dir.join(SECTION_FILE).is_file());

    let second = run(
        &target,
        &target,
        &["import", zip_second_arg.as_str(), "--overwrite"],
    );
    assert_success(&second, "sqlite import (overwrite)");
    assert!(stdout(&second).contains("上書き: 1"), "{}", stdout(&second));

    // Delete the mirror body so the rollback bundle can only come from the
    // stored row. A stale row would render the first archive's body.
    std::fs::remove_file(novel_dir.join(SECTION_FILE)).unwrap();

    let bundle = temp.path().join("bundle");
    let bundle_arg = bundle.to_string_lossy().to_string();
    let rolled_back = run(
        &target,
        &target,
        &["db", "export-yaml", "--out", bundle_arg.as_str()],
    );
    assert_success(&rolled_back, "db export-yaml");

    let rendered = find_file(&bundle.join("小説データ"), "1 第一話.yaml")
        .expect("rollback rendered the stored body");
    let text = std::fs::read_to_string(rendered).unwrap();
    assert!(text.contains("更新版の本文です"), "{text}");

    let database = find_file(&bundle, "database.yaml").expect("rollback database.yaml");
    let database = std::fs::read_to_string(database).unwrap();
    assert!(database.contains(TOC_URL), "{database}");
    assert!(database.contains(TITLE), "{database}");
}
