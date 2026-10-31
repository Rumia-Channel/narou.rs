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
        .env("USERPROFILE", root.join("home"))
        .env("HOME", root.join("home"))
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

    // The feature defaults to OFF; migration still stores each classification.
    let tags = run(root, &["tag"]);
    assert!(tags.status.success(), "{}", String::from_utf8_lossy(&tags.stderr));
    assert!(!text(&tags).contains("漫画"));
    assert!(!text(&tags).contains("小説"));
    assert!(text(&run(root, &["list", "--tag", "漫画"])).trim().is_empty());
    if sqlite {
        let connection = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
        let (attribute, tags): (String, String) = connection.query_row(
            "SELECT attribute_tag, tags_json FROM novels WHERE id=1", [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(attribute, "漫画");
        assert!(!tags.contains("漫画"));
        let indexed: i64 = connection.query_row(
            "SELECT COUNT(*) FROM novel_tags WHERE tag IN ('小説', '漫画')", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(indexed, 0);
    } else {
        let database: serde_yaml::Value = serde_yaml::from_str(
            &std::fs::read_to_string(root.join(".narou/database.yaml")).unwrap(),
        ).unwrap();
        let record = database.as_mapping().unwrap().get(serde_yaml::Value::Number(1.into())).unwrap();
        assert_eq!(record["attribute_tag"].as_str(), Some("漫画"));
        assert!(!record["tags"].as_sequence().unwrap().iter().any(|tag| tag.as_str() == Some("漫画")));
    }
    assert!(run(root, &["setting", "tag.attributes-enabled=true"]).status.success());
    let tags = run(root, &["tag"]);
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
    assert!(run(root, &["setting", "tag.attributes-enabled=false"]).status.success());
    assert!(text(&run(root, &["list", "--tag", "漫画"])).trim().is_empty());
    assert!(run(root, &["setting", "tag.attributes-enabled=true"]).status.success());
    assert_eq!(text(&run(root, &["list", "--tag", "漫画"])).trim(), "1");
}

#[test]
fn yaml_library_attributes_and_cli_display() { exercise(false); }

#[test]
fn sqlite_library_attributes_and_cli_display() { exercise(true); }

#[test]
fn existing_attributes_survive_initialization_without_the_site_definition() {
    let mut existing = record(3, "作品");
    existing.set_attribute_tag("漫画");
    assert!(!narou_rs::application::attribute_tags::initialize_record(&mut existing, &[]));
    assert_eq!(existing.attribute_tag(), Some("漫画"));

    let mut legacy = record(4, "作品");
    legacy.tags.push("漫画".into());
    assert!(narou_rs::application::attribute_tags::initialize_record(&mut legacy, &[]));
    assert_eq!(legacy.attribute_tag(), Some("漫画"));
    assert_eq!(legacy.tags, vec!["手動"]);
}
