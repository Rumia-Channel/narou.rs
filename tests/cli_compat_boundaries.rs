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

/// Like [`run`] but with the SQLite backend selected by the storage marker —
/// the mode `sqlite.mirror-files=false` libraries run in.
fn run_sqlite(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(args)
        .current_dir(root)
        .env("USERPROFILE", root.join("home"))
        .env("HOME", root.join("home"))
        .env("TMP", root.join("tmp"))
        .env("TEMP", root.join("tmp"))
        .env_remove("NAROU_ENV")
        .env_remove("NAROU_RS_LEGACY_YAML")
        .env_remove("NAROU_RS_MIRROR_FILES")
        .env_remove("NAROU_RS_KEEP_TXT")
        .env_remove("NAROU_RS_SECTION_CACHE")
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

/// DB-only SQLite conversion (`sqlite.mirror-files=false`) must read its
/// `setting.ini` / `replace.txt` out of the object store, keep the converted
/// output on disk, and clean up only the input files it materialized.
///
/// The real binary is driven end to end: the first command imports the minimal
/// narou.rb layout into `.narou/db.sqlite` (the cutover import path) and the
/// novel folder is then removed — the state the mirror-off backend leaves
/// behind. The conversion has to rebuild every input from the store. Before
/// the nested-runtime fix it died inside `#[tokio::main]` with exit code 127
/// instead of producing output, so this test pins the observable contract.
#[test]
fn sqlite_db_only_convert_reads_stored_inputs_and_persists_output() {
    const DATABASE_YAML: &str = "1:\n  id: 1\n  title: \"Mock Novel\"\n  author: \"Test Author\"\n  file_title: \"[Test Author] Mock Novel\"\n  toc_url: \"https://mock.test/novel/n1234ab/\"\n  sitename: \"mock.test\"\n  novel_type: 1\n  end: false\n  last_update: \"2026-01-02T00:00:00Z\"\n  general_all_no: 1\n  length: 57\n  tags: [\"probe\"]\n";
    const TOC_YAML: &str = "title: Mock Novel\nauthor: Test Author\ntoc_url: https://mock.test/novel/n1234ab/\nsubtitles:\n- index: '1'\n  href: /novel/n1234ab/1/\n  chapter: ''\n  subchapter: ''\n  subtitle: 第一話\n  file_subtitle: 第一話\n  subdate: '2026-01-01 00:00:00'\n  download_time: '2026-01-01 00:00:00 +0900'\nnovel_type: 1\n";
    const SECTION_YAML: &str = "index: '1'\nhref: /novel/n1234ab/1/\nchapter: ''\nsubchapter: ''\nsubtitle: 第一話\nfile_subtitle: 第一話\nsubdate: '2026-01-01 00:00:00'\ndownload_time: '2026-01-01 00:00:00 +0900'\nelement:\n  data_type: text\n  introduction: ''\n  postscript: ''\n  body: 'abc xyz 置換前'\n";

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    std::fs::write(root.join(".narou/storage-backend"), "sqlite").unwrap();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "sqlite.mirror-files: false\n",
    )
    .unwrap();
    std::fs::write(root.join(".narou/database.yaml"), DATABASE_YAML).unwrap();

    let novel_dir = root
        .join("小説データ")
        .join("mock.test")
        .join("[Test Author] Mock Novel");
    std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
    std::fs::write(novel_dir.join("toc.yaml"), TOC_YAML).unwrap();
    std::fs::write(novel_dir.join("本文/1 第一話.yaml"), SECTION_YAML).unwrap();
    std::fs::write(
        novel_dir.join("setting.ini"),
        "enable_alphabet_force_zenkaku = true\n",
    )
    .unwrap();
    std::fs::write(novel_dir.join("replace.txt"), "置換前\t置換後\n").unwrap();

    // First open: records + the 小説データ/ tree are imported into db.sqlite,
    // then the legacy files are retired.
    let output = run_sqlite(root, &["list"]);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join(".narou/db.sqlite").is_file(),
        "the first SQLite open must create the database"
    );
    assert!(
        !root.join(".narou/database.yaml").exists(),
        "imported records must be retired"
    );

    // mirror-files=false: the folder is not the authoritative copy.
    std::fs::remove_dir_all(root.join("小説データ").join("mock.test")).unwrap();

    let output = run_sqlite(root, &["convert", "1", "--no-epub", "-o", "expected.txt"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "DB-only convert must produce output: {stdout}{stderr}"
    );

    // The stored replacement and the stored setting were both honored:
    // "abc xyz 置換前" becomes "ａｂｃ ｘｙｚ 置換後".
    let converted = std::fs::read_to_string(novel_dir.join("expected.txt"))
        .expect("the converted output must stay on disk");
    assert!(
        converted.contains("ａｂｃ") && converted.contains("ｘｙｚ"),
        "the stored setting.ini was not applied: {converted}"
    );
    assert!(
        converted.contains("置換後") && !converted.contains("置換前"),
        "the stored replace.txt was not applied: {converted}"
    );
    assert!(
        !converted.contains("abc") && !converted.contains("xyz"),
        "the raw body leaked through untransformed: {converted}"
    );

    // Only the files materialized for this run may be cleaned up; the output
    // is persisted outside the materialization scope.
    for materialized in ["toc.yaml", "setting.ini", "replace.txt", "本文/1 第一話.yaml"] {
        assert!(
            !novel_dir.join(materialized).exists(),
            "materialized input {materialized} must be cleaned up"
        );
    }
    assert!(!novel_dir.join(".illustration_cache.yaml").exists(),
        "DB-only conversion must not create a filesystem illustration index");

    // A corrupt canonical TOC must fail maintenance, not fall back to a stale
    // file or delete sections against an empty/default TOC.
    let connection = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
    let keys_before: Vec<String> = connection.prepare("SELECT object_key FROM objects ORDER BY object_key")
        .unwrap().query_map([], |row| row.get(0)).unwrap()
        .collect::<Result<_, _>>().unwrap();
    assert_eq!(connection.execute(
        "UPDATE objects SET data=?1, encoding='none', size=1, crc32=?2 WHERE object_key=?3",
        rusqlite::params![b"[".as_slice(), i64::from(crc32fast::hash(b"[")),
            "novels/mock.test/[Test Author] Mock Novel/toc.yaml"],
    ).unwrap(), 1);
    let failed_clean = run_sqlite(root, &["clean", "--force", "1"]);
    assert!(!failed_clean.status.success(), "corrupt TOC must make clean fail");
    assert!(String::from_utf8_lossy(&failed_clean.stdout).contains("目次"));
    let keys_after: Vec<String> = connection.prepare("SELECT object_key FROM objects ORDER BY object_key")
        .unwrap().query_map([], |row| row.get(0)).unwrap()
        .collect::<Result<_, _>>().unwrap();
    assert_eq!(keys_after, keys_before, "failed clean must preserve every stored object");
}

#[test]
fn sqlite_clean_all_removes_only_current_orphans_and_skips_frozen_novels() {
    for mirror_files in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for relative in [".narou", "home/.narousetting", "tmp"] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
        }
        std::fs::write(root.join(".narou/storage-backend"), "sqlite\n").unwrap();
        std::fs::write(root.join(".narou/local_setting.yaml"),
            format!("sqlite.mirror-files: {mirror_files}\n")).unwrap();
        std::fs::write(root.join("home/.narousetting/global_setting.yaml"), "{}\n").unwrap();
        std::fs::write(root.join(".narou/database.yaml"),
            "1:\n  id: 1\n  title: 作品\n  author: 著者\n  file_title: 作品\n  toc_url: https://mock.test/1/\n  sitename: mock.test\n  last_update: '2026-10-17T00:00:00Z'\n2:\n  id: 2\n  title: 凍結作品\n  author: 著者\n  file_title: 凍結作品\n  toc_url: https://mock.test/2/\n  sitename: mock.test\n  last_update: '2026-10-17T00:00:00Z'\n").unwrap();
        std::fs::write(root.join(".narou/freeze.yaml"), "2: '2026-10-17 00:00:00 +09:00'\n").unwrap();
        let archive = root.join("小説データ");
        let active = archive.join("mock.test/作品");
        for title in ["作品", "凍結作品"] {
            let novel = archive.join("mock.test").join(title);
            for relative in ["本文/cache/v1", "raw/cache/v1", "挿絵"] {
                std::fs::create_dir_all(novel.join(relative)).unwrap();
            }
            std::fs::write(novel.join("toc.yaml"),
                "title: 作品\nauthor: 著者\ntoc_url: https://mock.test/1/\nsubtitles:\n- index: '1'\n  href: /1/1/\n  subtitle: keep\n  file_subtitle: keep\n  chapter: ''\n  subchapter: ''\n  subdate: ''\n").unwrap();
            for (name, body) in [
                ("本文/1 keep.yaml", "current body"),
                ("本文/9 orphan.yaml", "orphan body"),
                ("raw/1 keep.html", "current raw"),
                ("raw/9 orphan.HTML", "orphan raw"),
                ("raw/cache/v1/9 orphan.txt", "old raw"),
                ("挿絵/9 orphan.png", "illustration"),
            ] {
                std::fs::write(novel.join(name), body).unwrap();
            }
        }
        for index in 0..513 {
            std::fs::write(active.join(format!("本文/cache/v1/{index} old.yaml")), "old body").unwrap();
        }
        let imported = run_sqlite(root, &["list"]);
        assert!(imported.status.success(), "{}", String::from_utf8_lossy(&imported.stderr));
        if mirror_files {
            std::fs::remove_file(active.join("本文/9 orphan.yaml")).unwrap();
            std::fs::remove_file(active.join("raw/9 orphan.HTML")).unwrap();
        } else {
            std::fs::remove_dir_all(&archive).unwrap();
        }
        // Legacy files can appear after import; the database-only orphans above
        // and this filesystem-only orphan must both be found.
        std::fs::create_dir_all(active.join("raw")).unwrap();
        let late = active.join("raw/10 late.txt");
        std::fs::write(&late, "late orphan").unwrap();
        let connection = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
        let snapshot = || {
            connection.prepare("SELECT object_key, data, encoding, size, crc32 FROM objects ORDER BY object_key")
                .unwrap().query_map([], |row| Ok((
                    row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?,
                ))).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        };
        let before = snapshot();
        let preview = run_sqlite(root, &["clean", "-afn"]);
        assert!(preview.status.success(), "{}{}", String::from_utf8_lossy(&preview.stdout), String::from_utf8_lossy(&preview.stderr));
        assert_eq!(snapshot(), before, "dry-run must not change canonical objects");
        assert!(late.is_file());
        let cleaned = run_sqlite(root, &["clean", "-af"]);
        assert!(cleaned.status.success(), "{}{}", String::from_utf8_lossy(&cleaned.stdout), String::from_utf8_lossy(&cleaned.stderr));
        let expected: Vec<_> = before.into_iter().filter(|row| {
            row.0 != "novels/mock.test/作品/本文/9 orphan.yaml"
                && row.0 != "novels/mock.test/作品/raw/9 orphan.HTML"
        }).collect();
        assert_eq!(snapshot(), expected,
            "current sections, cached versions, illustrations and all frozen-novel objects must remain");
        assert!(!late.exists());
        assert!(!active.join("本文/9 orphan.yaml").exists());
        assert!(!active.join("raw/9 orphan.HTML").exists());
    }
}

#[test]
fn sqlite_convert_uses_saved_settings_over_stale_pre_migration_files() {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use narou_rs::application::{NovelSettingsPatch, NovelSettingsService};
    use narou_rs::converter::ini::IniValue;
    use narou_rs::native::object_store::NativeObjectStore;
    use narou_rs::native::sqlite::object_store::SqliteObjectStore;
    use narou_rs::platform::mocks::MemoryNovelRepository;

    const OLD_INI: &str = "novel_title = 旧設定の題名\nnovel_author = 旧設定の著者\n";
    for mirror_files in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for relative in [".narou", "home/.narousetting", "tmp"] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
        }
        std::fs::write(root.join(".narou/storage-backend"), "sqlite\n").unwrap();
        std::fs::write(root.join(".narou/local_setting.yaml"),
            format!("sqlite.mirror-files: {mirror_files}\nconvert.keep-txt: true\n")).unwrap();
        std::fs::write(root.join("home/.narousetting/global_setting.yaml"), "{}\n").unwrap();
        let archive = root.join("小説データ");
        let mut records = BTreeMap::new();
        for (id, folder) in [(1, "old"), (2, "new")] {
            let record: narou_rs::db::NovelRecord = serde_yaml::from_str(&format!(
                "id: {id}\nauthor: 元の著者\ntitle: 元の題名\nfile_title: {folder}\ntoc_url: https://mock.test/{id}/\nsitename: mock.test\nnovel_type: 1\nlast_update: '2026-10-17T00:00:00Z'\ngeneral_all_no: 1\n",
            )).unwrap();
            records.insert(id, record);
            let novel = archive.join("mock.test").join(folder);
            std::fs::create_dir_all(novel.join("本文")).unwrap();
            std::fs::write(novel.join("toc.yaml"),
                "title: 元の題名\nauthor: 元の著者\ntoc_url: https://mock.test/1/\nsubtitles:\n- index: '1'\n  href: /1/1/\n  chapter: ''\n  subchapter: ''\n  subtitle: 第一話\n  file_subtitle: 第一話\n  subdate: '2026-01-01 00:00:00'\nnovel_type: 1\n").unwrap();
            std::fs::write(novel.join("本文/1 第一話.yaml"),
                "index: '1'\nhref: /1/1/\nchapter: ''\nsubchapter: ''\nsubtitle: 第一話\nfile_subtitle: 第一話\nsubdate: '2026-01-01 00:00:00'\ndownload_time: '2026-01-01 00:00:00 +0900'\nelement:\n  data_type: text\n  introduction: ''\n  postscript: ''\n  body: 保存済みの本文\n").unwrap();
            std::fs::write(novel.join("setting.ini"), OLD_INI).unwrap();
        }
        std::fs::write(root.join(".narou/database.yaml"), serde_yaml::to_string(&records).unwrap()).unwrap();
        let imported = run_sqlite(root, &["list"]);
        assert!(imported.status.success(), "{}{}", String::from_utf8_lossy(&imported.stdout), String::from_utf8_lossy(&imported.stderr));
        // Old novels retain their legacy files; new DB-only novels have none.
        if !mirror_files {
            std::fs::remove_dir_all(archive.join("mock.test/new")).unwrap();
        }
        let objects = Arc::new(SqliteObjectStore::with_mirror_files(
            narou_rs::native::sqlite::open(&root.join(".narou/db.sqlite")).unwrap(),
            NativeObjectStore::from_root(archive.clone()).unwrap(), mirror_files,
        ));
        let service = NovelSettingsService::new(
            Arc::new(MemoryNovelRepository::from_records(records.into_values().collect())),
            objects,
        );
        for (title, author) in [
            (Some("更新した題名"), Some("更新した著者")),
            (Some("再編集した題名"), Some("再編集した著者")),
            (None, None),
        ] {
            let patch = NovelSettingsPatch {
                settings: HashMap::from([
                    ("novel_title".into(), title.map(|value| IniValue::String(value.into()))),
                    ("novel_author".into(), author.map(|value| IniValue::String(value.into()))),
                ]),
                replace_patterns: None,
            };
            for (id, folder) in [(1, "old"), (2, "new")] {
                let saved = futures::executor::block_on(service.save(id.into(), &patch)).unwrap();
                assert!(saved.failure.is_none(), "{:?}", saved.failure);
                let output = run_sqlite(root, &["convert", &id.to_string(), "--no-epub", "-o", "proof.txt"]);
                assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                let novel = archive.join("mock.test").join(folder);
                let converted = std::fs::read_to_string(novel.join("proof.txt")).unwrap_or_else(|error| {
                    panic!("{error}: {}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                });
                assert_eq!(converted.lines().take(2).collect::<Vec<_>>(),
                    [title.unwrap_or("元の題名"), author.unwrap_or("元の著者")],
                    "mirror={mirror_files}, novel={folder}");
                assert!(converted.contains("保存済みの本文"));
                if !mirror_files {
                    if folder == "old" {
                        assert_eq!(std::fs::read_to_string(novel.join("setting.ini")).unwrap(), OLD_INI,
                            "reading canonical settings must not overwrite pre-existing files");
                    } else {
                        assert!(!novel.join("setting.ini").exists(),
                            "only newly materialized input files should be removed");
                    }
                }
            }
        }
    }
}
