//! Existing libraries receive classifications and CLI visibility is configurable.
use std::collections::BTreeMap;
use std::process::{Command, Output};

use chrono::Utc;
use narou_rs::db::NovelRecord;

fn record(id: i64, title: &str) -> NovelRecord {
    NovelRecord {
        id, author: "作者".into(), title: title.into(), file_title: format!("work{id}"),
        toc_url: format!("https://example.test/work/{id}"), sitename: "Example".into(),
        novel_type: 1, end: false, last_update: Utc::now(), new_arrivals_date: None,
        use_subdirectory: false, general_firstup: None, novelupdated_at: None,
        general_lastup: None, last_mail_date: None, tags: vec!["手動".into()],
        ncode: None, domain: Some("example.test".into()), general_all_no: Some(1),
        length: None, suspend: false, is_narou: false, last_check_date: None,
        convert_failure: false, requires_login: false, login_session: None,
        extra_fields: Default::default(),
    }
}

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .current_dir(root).env_remove("NAROU_RS_LEGACY_YAML")
        .args(args).output().unwrap()
}

fn text(output: &Output) -> String { String::from_utf8_lossy(&output.stdout).into_owned() }

fn exercise(sqlite: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    let mut ordinary = record(2, "普通の作品");
    ordinary.tags.push("漫画".into()); // 旧バージョンでは手動タグとして追加できた。
    let records = BTreeMap::from([(1, record(1, "漫画作品")), (2, ordinary)]);
    std::fs::create_dir_all(root.join("webnovel")).unwrap();
    if sqlite { std::fs::write(root.join(".narou/storage-backend"), "sqlite").unwrap(); }
    std::fs::write(root.join(".narou/database.yaml"), serde_yaml::to_string(&records).unwrap()).unwrap();
    std::fs::write(root.join("webnovel/example.test.yaml"), r#"name: Example
domain: example.test
top_url: https://example.test
url: 'https://example.test/work/(?<ncode>\d+)'
toc_url: 'https://example.test/work/\k<ncode>'
sitename: Example
version: 1.0
work_type_rules:
  - field: title
    match: '^漫画'
    type: manga
"#).unwrap();

    let tags = run(root, &["tag"]);
    assert!(tags.status.success(), "{}", String::from_utf8_lossy(&tags.stderr));
    assert!(text(&tags).contains("漫画"));
    assert!(!text(&tags).contains("小説"));
    assert!(run(root, &["setting", "tag.show-novel=true"]).status.success());
    assert!(run(root, &["setting", "tag.show-manga=false"]).status.success());
    let tags = run(root, &["tag"]);
    assert!(tags.status.success());
    assert!(text(&tags).contains("小説"));
    assert!(!text(&tags).contains("漫画"));
    let filtered = run(root, &["list", "--tag", "漫画"]);
    assert!(filtered.status.success());
    assert_eq!(text(&filtered).trim(), "1");
    let filtered = run(root, &["list", "--tag", "漫画", "--grep", "漫画"]);
    assert!(filtered.status.success());
    assert_eq!(text(&filtered).trim(), "1");
    assert!(!run(root, &["tag", "--delete", "漫画", "1"]).status.success());
    assert!(run(root, &["tag", "--clear", "1"]).status.success());
    let filtered = run(root, &["list", "--tag", "漫画"]);
    assert_eq!(text(&filtered).trim(), "1");
}

#[test]
fn yaml_library_attributes_and_cli_display() { exercise(false); }

#[test]
fn sqlite_library_attributes_and_cli_display() { exercise(true); }
