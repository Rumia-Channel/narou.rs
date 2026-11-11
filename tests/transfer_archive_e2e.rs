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
    let source_dir = source.join("小説データ/ExampleSite/example-title");
    let extra_files = [
        ("raw/1.html", "<p>元の HTML</p>"),
        ("本文/cache/revision/old.yaml", "旧版本文"),
        ("book.epub", "既存の生成物"),
        ("notes.txt", "利用者の追加ファイル"),
    ];
    for (relative, content) in extra_files {
        let path = source_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

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
    for (relative, _) in extra_files {
        std::fs::remove_file(novel_dir.join(relative)).unwrap();
    }
    assert_success(
        &run(&target, &target, &["setting", "sqlite.mirror-files=false"]),
        "disable filesystem mirrors",
    );
    let id: i64 = rusqlite::Connection::open(target.join(".narou/db.sqlite")).unwrap()
        .query_row("SELECT id FROM novels WHERE toc_url=?1", [TOC_URL], |row| row.get(0)).unwrap();
    let complete_zip = temp.path().join("complete.zip");
    assert_success(
        &run(&target, &target, &["export", &id.to_string(), "-o", complete_zip.to_str().unwrap()]),
        "export SQLite-only files",
    );
    let mut exported = zip::ZipArchive::new(std::fs::File::open(&complete_zip).unwrap()).unwrap();
    for (relative, content) in extra_files {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut exported.by_name(&format!("novels/0/{relative}")).unwrap(), &mut bytes,
        ).unwrap();
        assert_eq!(bytes, content.as_bytes(), "{relative}");
        assert!(!novel_dir.join(relative).exists(), "export must not restore live outputs: {relative}");
    }
    assert_success(&run(&target, &target, &["backup", &id.to_string()]), "backup SQLite-only files");
    let backup = std::fs::read_dir(novel_dir.join("backup")).unwrap().next().unwrap().unwrap().path();
    let mut backup = zip::ZipArchive::new(std::fs::File::open(backup).unwrap()).unwrap();
    for (relative, content) in extra_files {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut backup.by_name(relative).unwrap(), &mut bytes).unwrap();
        assert_eq!(bytes, content.as_bytes(), "{relative}");
        assert!(!novel_dir.join(relative).exists(), "backup must not restore live outputs: {relative}");
    }

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

// --- virtual novel transfer --------------------------------------------------

const MEMBER_ONE_URL: &str = "https://example.test/novel/1/";
const MEMBER_TWO_URL: &str = "https://example.test/novel/2/";

/// Builds a YAML library with two ordinary works and one virtual parent that
/// keeps them in the reverse order. The parent owns no body: only its
/// per-novel inputs (settings and cover) live in its own folder.
fn create_virtual_source_library(root: &Path) {
    let narou_dir = root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    std::fs::write(
        narou_dir.join("database.yaml"),
        r#"---
1:
  id: 1
  author: 作者一
  title: 作品一
  file_title: n1111aa
  toc_url: https://example.test/novel/1/
  sitename: ExampleSite
  novel_type: 1
  end: false
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  use_subdirectory: false
  tags:
    - タグ
  ncode: n1111aa
  domain: example.test
  general_all_no: 1
  is_narou: false
2:
  id: 2
  author: 作者二
  title: 作品二
  file_title: n2222bb
  toc_url: https://example.test/novel/2/
  sitename: ExampleSite
  novel_type: 1
  end: false
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  use_subdirectory: false
  tags:
    - タグ
  ncode: n2222bb
  domain: example.test
  general_all_no: 1
  is_narou: false
3:
  id: 3
  author: 著者
  title: 結合作品
  file_title: 結合作品
  toc_url: https://narou.rs/virtual/vs_3
  sitename: ExampleSite
  novel_type: 0
  end: true
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  use_subdirectory: false
  tags:
    - タグ
    - vs_3
  ncode: vs_3
  domain: example.test
  attribute_tag: 小説
  is_narou: false
"#,
    )
    .unwrap();
    // The group holds its members in the reverse order.
    std::fs::write(
        narou_dir.join("virtual_novel_members.yaml"),
        "---\n3:\n  - 2\n  - 1\n",
    )
    .unwrap();

    for (folder, title, url) in [
        ("n1111aa", "作品一", MEMBER_ONE_URL),
        ("n2222bb", "作品二", MEMBER_TWO_URL),
    ] {
        let dir = root.join("小説データ").join("ExampleSite").join(folder);
        std::fs::create_dir_all(dir.join("本文")).unwrap();
        std::fs::write(
            dir.join("toc.yaml"),
            format!(
                "---\ntitle: {title}\nauthor: 作者\ntoc_url: {url}\nstory: あらすじ\nsubtitles:\n  - index: '1'\n    href: '1'\n    chapter: ''\n    subchapter: ''\n    subtitle: 第一話\n    file_subtitle: 第一話\n    subdate: '2026-04-20'\nnovel_type: 1\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join(SECTION_FILE),
            "---\nsubtitle: 第一話\nbody: |\n  本文です\n",
        )
        .unwrap();
    }

    let parent = root.join("小説データ").join("ExampleSite").join("結合作品");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(parent.join("setting.ini"), "[global]\n").unwrap();
    std::fs::write(parent.join("cover.jpg"), b"cover-bytes").unwrap();
}

fn read_records(root: &Path) -> std::collections::BTreeMap<i64, narou_rs::db::NovelRecord> {
    let raw = std::fs::read_to_string(root.join(".narou").join("database.yaml")).unwrap();
    serde_yaml::from_str(&raw).unwrap()
}

fn read_members(root: &Path) -> std::collections::BTreeMap<i64, Vec<i64>> {
    let raw =
        std::fs::read_to_string(root.join(".narou").join("virtual_novel_members.yaml")).unwrap();
    serde_yaml::from_str(&raw).unwrap()
}

/// The member URLs of a group, in the stored order.
fn group_urls(
    records: &std::collections::BTreeMap<i64, narou_rs::db::NovelRecord>,
    members: &std::collections::BTreeMap<i64, Vec<i64>>,
) -> Vec<String> {
    let group = members.values().next().expect("the group must exist");
    group
        .iter()
        .map(|id| records.get(id).expect("the member must exist").toc_url.clone())
        .collect()
}

/// Exporting a virtual parent expands its members and keeps the group's own
/// inputs; importing it into another library rebuilds the group with the
/// destination IDs and the archived member order.
#[test]
fn virtual_parent_export_expands_members_and_import_restores_the_group() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    create_virtual_source_library(&source);
    std::fs::create_dir_all(target.join(".narou")).unwrap();

    let zip = temp.path().join("virtual.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    let exported = run(&source, &source, &["export", "3", "-o", zip_arg.as_str()]);
    assert_success(&exported, "narou export (virtual parent)");
    assert!(
        stdout(&exported).contains("3 件の小説"),
        "the parent and its two members are written: {}",
        stdout(&exported)
    );

    let bytes = std::fs::read(&zip).unwrap();
    let parsed = narou_rs::transfer::parse_archive(&bytes).unwrap();
    assert_eq!(parsed.manifest.novels.len(), 3, "two members and one parent");
    // Ordinary members come before the parent that references them.
    assert!(parsed.manifest.novels[0].virtual_members.is_none());
    assert!(parsed.manifest.novels[1].virtual_members.is_none());
    let parent = &parsed.manifest.novels[2];
    assert_eq!(
        parent.virtual_members.as_deref(),
        Some([MEMBER_TWO_URL.to_string(), MEMBER_ONE_URL.to_string()].as_slice()),
        "the membership travels as member URLs in the stored order"
    );
    assert!(parent.ncode.is_none(), "a parent carries no library-local ncode");
    assert!(
        !parent.tags.iter().any(|tag| narou_rs::virtual_novel::is_internal_tag(tag)),
        "a parent carries no internal tag: {:?}",
        parent.tags
    );
    assert!(
        narou_rs::virtual_novel::is_portable_toc_url(&parent.toc_url),
        "the parent identity is portable: {}",
        parent.toc_url
    );
    let parent_files: Vec<&str> = parent.files.iter().map(|file| file.path.as_str()).collect();
    assert!(parent_files.contains(&"setting.ini"), "{parent_files:?}");
    assert!(parent_files.contains(&"cover.jpg"), "{parent_files:?}");
    assert_eq!(parent_files.len(), 2, "only the per-novel inputs travel");

    // The source record was migrated to the portable identity.
    let source_database = std::fs::read_to_string(source.join(".narou").join("database.yaml")).unwrap();
    assert!(source_database.contains(&parent.toc_url), "{source_database}");

    // The group is rebuilt in another library.
    let imported = run(&target, &target, &["import", zip_arg.as_str()]);
    assert_success(&imported, "narou import (virtual parent)");
    assert!(stdout(&imported).contains("新規: 3 件"), "{}", stdout(&imported));

    let records = read_records(&target);
    let members = read_members(&target);
    assert_eq!(records.len(), 3, "{records:?}");
    assert_eq!(members.len(), 1, "one group: {members:?}");
    assert_eq!(
        group_urls(&records, &members),
        vec![MEMBER_TWO_URL.to_string(), MEMBER_ONE_URL.to_string()],
        "the reversed order survives the reallocated destination IDs"
    );

    let parent_record = records
        .values()
        .find(|record| record.toc_url == parent.toc_url)
        .expect("the parent keeps the archived identity");
    assert!(
        narou_rs::virtual_novel::is_portable_toc_url(&parent_record.toc_url),
        "the destination identity is portable: {}",
        parent_record.toc_url
    );
    assert_ne!(
        parent_record.toc_url, "https://narou.rs/virtual/vs_3",
        "a same-number local parent has a different identity"
    );
    assert_eq!(
        parent_record.ncode.as_deref(),
        Some(format!("vs_{}", parent_record.id).as_str()),
        "the local ncode follows the destination ID"
    );
    assert_eq!(parent_record.attribute_tag(), Some("小説"));
    assert!(
        parent_record
            .tags
            .iter()
            .any(|tag| tag == &format!("vs_{}", parent_record.id)),
        "the group is hidden locally by its own internal tag: {:?}",
        parent_record.tags
    );

    // The parent's own inputs are restored next to the group.
    let cover = find_file(&target.join("小説データ"), "cover.jpg").expect("the cover is restored");
    let parent_dir = cover.parent().unwrap();
    assert!(
        parent_dir.join("setting.ini").is_file(),
        "{}",
        parent_dir.display()
    );
    assert_eq!(std::fs::read(&cover).unwrap(), b"cover-bytes");
}

/// A member that is selected directly and expanded through a parent is stored
/// once, and the shared member is not written twice.
#[test]
fn virtual_parent_export_deduplicates_an_expanded_member() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    create_virtual_source_library(&source);

    let zip = temp.path().join("dedup.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    assert_success(
        &run(&source, &source, &["export", "3", "1", "-o", zip_arg.as_str()]),
        "narou export (parent and member)",
    );
    let bytes = std::fs::read(&zip).unwrap();
    let parsed = narou_rs::transfer::parse_archive(&bytes).unwrap();
    assert_eq!(parsed.manifest.novels.len(), 3, "two members and one parent");
    let urls: Vec<&str> = parsed
        .manifest
        .novels
        .iter()
        .map(|novel| novel.toc_url.as_str())
        .collect();
    assert_eq!(urls.iter().filter(|url| **url == MEMBER_ONE_URL).count(), 1);
}

/// An existing member is skipped but still maps to its destination ID, a
/// repeated import skips everything, and an overwrite restores the archived
/// member order.
#[test]
fn virtual_parent_import_skips_existing_members_and_overwrite_restores_the_order() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    create_virtual_source_library(&source);

    let zip = temp.path().join("skip.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    assert_success(
        &run(&source, &source, &["export", "3", "-o", zip_arg.as_str()]),
        "narou export (virtual parent)",
    );

    // The target already owns one member of the group.
    std::fs::create_dir_all(target.join(".narou")).unwrap();
    std::fs::write(
        target.join(".narou").join("database.yaml"),
        r#"---
1:
  id: 1
  author: 作者一
  title: 作品一
  file_title: n1111aa
  toc_url: https://example.test/novel/1/
  sitename: ExampleSite
  novel_type: 1
  end: false
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  use_subdirectory: false
  ncode: n1111aa
  domain: example.test
  is_narou: false
"#,
    )
    .unwrap();
    let existing = target
        .join("小説データ")
        .join("ExampleSite")
        .join("n1111aa");
    std::fs::create_dir_all(existing.join("本文")).unwrap();
    std::fs::write(
        existing.join("toc.yaml"),
        "---\ntitle: 作品一\nauthor: 作者一\ntoc_url: https://example.test/novel/1/\nsubtitles:\n  - index: '1'\n    href: '1'\n    subtitle: 第一話\n    file_subtitle: 第一話\nnovel_type: 1\n",
    )
    .unwrap();

    let imported = run(&target, &target, &["import", zip_arg.as_str()]);
    assert_success(&imported, "narou import (existing member)");
    let summary = stdout(&imported);
    assert!(summary.contains("新規: 2 件"), "{summary}");
    assert!(summary.contains("スキップ: 1"), "{summary}");

    let records = read_records(&target);
    let members = read_members(&target);
    assert_eq!(
        group_urls(&records, &members),
        vec![MEMBER_TWO_URL.to_string(), MEMBER_ONE_URL.to_string()],
        "the skipped member keeps its own destination ID"
    );
    let group = members.values().next().unwrap();
    assert!(
        group.contains(&1),
        "the existing member is referenced by its own ID: {group:?}"
    );
    let group_id = *members.keys().next().unwrap();

    // A repeated import changes nothing.
    let repeated = run(&target, &target, &["import", zip_arg.as_str()]);
    assert_success(&repeated, "narou import (repeat)");
    assert!(stdout(&repeated).contains("スキップ: 3"), "{}", stdout(&repeated));

    // A local reorder is replaced by the archived order on overwrite.
    let mut reordered = members[&group_id].clone();
    reordered.reverse();
    std::fs::write(
        target.join(".narou").join("virtual_novel_members.yaml"),
        serde_yaml::to_string(&std::collections::BTreeMap::from([(group_id, reordered)])).unwrap(),
    )
    .unwrap();
    let overwritten = run(
        &target,
        &target,
        &["import", zip_arg.as_str(), "--overwrite"],
    );
    assert_success(&overwritten, "narou import --overwrite (virtual parent)");
    assert!(stdout(&overwritten).contains("上書き: 3"), "{}", stdout(&overwritten));
    let records = read_records(&target);
    let members = read_members(&target);
    assert_eq!(
        group_urls(&records, &members),
        vec![MEMBER_TWO_URL.to_string(), MEMBER_ONE_URL.to_string()],
        "the overwrite restores the archived member order"
    );
}

/// A member that cannot be imported blocks the group: no parent record and no
/// membership are published.
#[test]
fn virtual_parent_import_does_not_publish_a_group_with_a_broken_member() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    std::fs::create_dir_all(target.join(".narou")).unwrap();

    let manifest = narou_rs::transfer::ArchiveManifest {
        format_version: narou_rs::transfer::FORMAT_VERSION,
        novels: vec![
            narou_rs::transfer::ArchiveNovel {
                toc_url: MEMBER_ONE_URL.to_string(),
                title: "作品一".to_string(),
                // A member without a site name cannot be stored.
                sitename: String::new(),
                files: vec![narou_rs::transfer::ArchiveFile::new(
                    "toc.yaml",
                    "title: 作品一\n".as_bytes().to_vec(),
                )],
                ..Default::default()
            },
            narou_rs::transfer::ArchiveNovel {
                toc_url: "https://narou.rs/virtual/0123456789abcdef0123456789abcdef".to_string(),
                title: "結合作品".to_string(),
                sitename: "ExampleSite".to_string(),
                virtual_members: Some(vec![MEMBER_ONE_URL.to_string()]),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let bytes = narou_rs::transfer::build_archive(&manifest).unwrap();
    let zip = temp.path().join("broken.zip");
    std::fs::write(&zip, bytes).unwrap();
    let zip_arg = zip.to_string_lossy().to_string();

    let imported = run(&target, &target, &["import", zip_arg.as_str()]);
    assert!(!imported.status.success(), "the broken member fails the import");
    let database = target.join(".narou").join("database.yaml");
    let stored = std::fs::read_to_string(&database).unwrap_or_default();
    assert!(
        !stored.contains("narou.rs/virtual"),
        "the group must not be published: {stored}"
    );
    assert!(
        !target.join(".narou").join("virtual_novel_members.yaml").exists(),
        "the membership must not be published"
    );
}

/// A parent that references a member the archive does not carry is refused
/// before anything is written.
#[test]
fn virtual_parent_import_refuses_a_missing_member_before_any_change() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    std::fs::create_dir_all(target.join(".narou")).unwrap();

    let mut buffer = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        writer
            .start_file("manifest.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(
            &mut writer,
            r#"{"format_version":2,"novels":[{"toc_url":"https://narou.rs/virtual/0123456789abcdef0123456789abcdef","title":"結合作品","author":"作者","sitename":"ExampleSite","novel_type":0,"virtual_members":["https://example.test/novel/9/"]}]}"#.as_bytes(),
        )
        .unwrap();
        writer.finish().unwrap();
    }
    let zip = temp.path().join("missing-member.zip");
    std::fs::write(&zip, buffer).unwrap();
    let zip_arg = zip.to_string_lossy().to_string();

    let imported = run(&target, &target, &["import", zip_arg.as_str()]);
    assert!(!imported.status.success(), "the archive is refused");
    let stderr = String::from_utf8_lossy(&imported.stderr);
    assert!(
        stderr.contains("virtual member is missing from the archive"),
        "{stderr}"
    );
    let stored = std::fs::read_to_string(target.join(".narou").join("database.yaml"))
        .unwrap_or_default();
    assert!(stored.trim().is_empty(), "nothing was written: {stored}");
}

/// The SQLite ("Lite") library keeps the authoritative rows in `db.sqlite`, so
/// the rebuilt group must live in the stored state, not only in the mirror.
#[test]
fn sqlite_library_import_restores_a_virtual_group() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("lite");
    create_virtual_source_library(&source);
    std::fs::create_dir_all(target.join(".narou")).unwrap();
    std::fs::write(target.join(".narou").join("storage-backend"), "sqlite\n").unwrap();

    let zip = temp.path().join("virtual.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    assert_success(
        &run(&source, &source, &["export", "3", "-o", zip_arg.as_str()]),
        "narou export (virtual parent)",
    );
    let bytes = std::fs::read(&zip).unwrap();
    let parsed = narou_rs::transfer::parse_archive(&bytes).unwrap();
    let parent_url = parsed.manifest.novels[2].toc_url.clone();

    let imported = run(&target, &target, &["import", zip_arg.as_str()]);
    assert_success(&imported, "sqlite import (virtual parent)");
    assert!(stdout(&imported).contains("新規: 3 件"), "{}", stdout(&imported));

    let conn = rusqlite::Connection::open(target.join(".narou/db.sqlite")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM novels WHERE toc_url=?1", [&parent_url], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1, "the parent is stored in SQLite");
    let members: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM novels WHERE toc_url IN (?1, ?2)",
            [MEMBER_ONE_URL, MEMBER_TWO_URL],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(members, 2, "both members are stored in SQLite");

    // The group is visible through the CLI, so the stored membership is used.
    let listed = run(&target, &target, &["virtual", "list"]);
    assert_success(&listed, "narou virtual list");
    let listing = stdout(&listed);
    assert!(listing.contains("結合作品"), "{listing}");
    assert!(listing.contains("2 作品"), "{listing}");

    // The parent's inputs are mirrored next to the group.
    let cover = find_file(&target.join("小説データ"), "cover.jpg").expect("the cover is mirrored");
    assert_eq!(std::fs::read(&cover).unwrap(), b"cover-bytes");
}

/// A parent's cover travels as a stream: an input larger than the
/// small-object limit must not be buffered, and it must survive the round trip
/// byte for byte.
#[test]
fn virtual_parent_export_streams_a_cover_larger_than_the_small_object_limit() {
    use std::io::{Read, Write};
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    create_virtual_source_library(&source);
    std::fs::create_dir_all(target.join(".narou")).unwrap();

    // A cover above the 16 MiB small-object limit, written in chunks.
    let cover = source
        .join("小説データ")
        .join("ExampleSite")
        .join("結合作品")
        .join("cover.jpg");
    let mut expected = Sha256::new();
    let mut file = std::fs::File::create(&cover).unwrap();
    let mut random = 0x12345678u32;
    let mut chunk = [0u8; 64 * 1024];
    for _ in 0..(17 * 1024 / 64) {
        for byte in &mut chunk {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            *byte = random as u8;
        }
        expected.update(chunk);
        file.write_all(&chunk).unwrap();
    }
    file.sync_all().unwrap();
    drop(file);

    let zip = temp.path().join("large-cover.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    assert_success(
        &run(&source, &source, &["export", "3", "-o", zip_arg.as_str()]),
        "narou export (large cover)",
    );
    assert_success(
        &run(&target, &target, &["import", zip_arg.as_str()]),
        "narou import (large cover)",
    );

    let restored = find_file(&target.join("小説データ"), "cover.jpg").expect("the cover is restored");
    let mut actual = Sha256::new();
    let mut restored = std::fs::File::open(restored).unwrap();
    let mut chunk = [0u8; 64 * 1024];
    let mut size = 0usize;
    loop {
        let count = restored.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        actual.update(&chunk[..count]);
        size += count;
    }
    assert_eq!(size, 17 * 1024 * 1024);
    assert_eq!(actual.finalize(), expected.finalize());
}

#[test]
fn large_binary_entry_roundtrips_through_file_streaming_cli() {
    use std::io::{Read, Write};
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    create_source_library(&source);
    std::fs::create_dir_all(target.join(".narou")).unwrap();
    let images = source.join("小説データ/ExampleSite/example-title/挿絵");
    std::fs::create_dir_all(&images).unwrap();
    let mut file = std::fs::File::create(images.join("large.bin")).unwrap();
    let mut expected = Sha256::new();
    let mut random = 0x12345678u32;
    let mut chunk = [0u8; 64 * 1024];
    for _ in 0..(17 * 1024 / 64) {
        for byte in &mut chunk {
            random ^= random << 13; random ^= random >> 17; random ^= random << 5;
            *byte = random as u8;
        }
        expected.update(chunk);
        file.write_all(&chunk).unwrap();
    }
    drop(file);
    let zip = temp.path().join("large.zip");
    let zip_arg = zip.to_string_lossy().to_string();
    assert_success(&run(&source, &source, &["export", "3", "-o", &zip_arg]), "large export");
    assert!(std::fs::metadata(&zip).unwrap().len() > 16 * 1024 * 1024);
    assert_success(&run(&target, &target, &["import", &zip_arg]), "large import");
    let restored = find_file(&target.join("小説データ"), "large.bin").unwrap();
    let mut restored = std::fs::File::open(restored).unwrap();
    let mut actual = Sha256::new();
    let mut size = 0usize;
    loop {
        let count = restored.read(&mut chunk).unwrap();
        if count == 0 { break; }
        actual.update(&chunk[..count]);
        size += count;
    }
    assert_eq!(size, 17 * 1024 * 1024);
    assert_eq!(actual.finalize(), expected.finalize());
}
