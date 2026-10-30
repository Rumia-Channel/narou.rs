//! End-to-end classification of virtual novels by their members' work types.
//!
//! The classification (小説 / 漫画) lives in the `attribute_tag` field of the
//! virtual novel row, separately from the ordinary tags, and stays available
//! regardless of the `tag.attributes-enabled` display setting.

use std::path::Path;
use std::process::{Command, Output};

/// Write a library with one novel per requested work type.
///
/// `members` is a list of `(id, title, attribute_tag)`. The attribute tag is
/// written as the `attribute_tag` field of the row, exactly like the real
/// library stores it.
fn write_fixture(root: &Path, members: &[(i64, &str, Option<&str>)]) {
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    let mut database = String::new();
    for (id, title, attribute) in members {
        database.push_str(&format!(
            "{id}:\n  id: {id}\n  title: \"{title}\"\n  author: \"Author\"\n  file_title: \"[Author] {title}\"\n  toc_url: \"https://mock.test/novel/a{id}/\"\n  sitename: \"mock.test\"\n  novel_type: 1\n  end: false\n  last_update: \"2026-01-02T00:00:00Z\"\n  general_lastup: \"2026-01-01T00:00:00Z\"\n  general_all_no: 0\n  length: 10\n  tags: [\"共通\"]\n"
        ));
        if let Some(attribute) = attribute {
            database.push_str(&format!("  attribute_tag: \"{attribute}\"\n"));
        }
    }
    std::fs::write(root.join(".narou/database.yaml"), database).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("run narou_rs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Read the `attribute_tag` field of a row from the YAML database.
fn stored_attribute(root: &Path, id: i64) -> Option<String> {
    let database: serde_yaml::Value = serde_yaml::from_str(
        &std::fs::read_to_string(root.join(".narou/database.yaml")).unwrap(),
    )
    .unwrap();
    database
        .as_mapping()
        .and_then(|mapping| {
            mapping
                .iter()
                .find(|(key, _)| key.as_i64() == Some(id))
                .map(|(_, value)| value)
        })
        .and_then(|value| value["attribute_tag"].as_str().map(str::to_owned))
}

#[test]
fn novel_only_members_are_classified_as_novels() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("小説"))]);

    let added = run(
        root,
        &[
            "virtual", "add", "--title", "XXX", "--author", "Author", "--site", "mock.test", "1",
            "2",
        ],
    );
    assert!(
        added.status.success(),
        "virtual add failed: {}{}",
        stdout(&added),
        stderr(&added)
    );
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("小説"),
        "a novel-only virtual novel must be classified as a novel"
    );
}

#[test]
fn manga_only_members_are_classified_as_manga() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("漫画")), (2, "A その2", Some("漫画"))]);

    let added = run(
        root,
        &[
            "virtual", "add", "--title", "XXX", "--author", "Author", "--site", "mock.test", "1",
            "2",
        ],
    );
    assert!(
        added.status.success(),
        "virtual add failed: {}{}",
        stdout(&added),
        stderr(&added)
    );
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("漫画"),
        "a manga-only virtual novel must be classified as a manga"
    );
}

#[test]
fn mixed_members_default_to_novel_and_can_be_changed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("漫画"))]);

    let added = run(
        root,
        &[
            "virtual", "add", "--title", "XXX", "--author", "Author", "--site", "mock.test", "1",
            "2",
        ],
    );
    assert!(
        added.status.success(),
        "virtual add failed: {}{}",
        stdout(&added),
        stderr(&added)
    );
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("小説"),
        "a mixed virtual novel must default to the novel classification"
    );

    // 混在しているときは利用者が属性を選べる。
    let changed = run(root, &["virtual", "attribute", "vs_3", "漫画"]);
    assert!(
        changed.status.success(),
        "virtual attribute failed: {}{}",
        stdout(&changed),
        stderr(&changed)
    );
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("漫画"),
        "the chosen classification must be stored"
    );

    // 一覧にも現在の分類が出る。
    let listed = stdout(&run(root, &["virtual", "list"]));
    assert!(
        listed.contains("漫画"),
        "the list must show the current classification: {listed}"
    );
}

#[test]
fn mixed_members_accept_the_attribute_at_creation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("漫画"))]);

    let added = run(
        root,
        &[
            "virtual",
            "add",
            "--title",
            "XXX",
            "--author",
            "Author",
            "--site",
            "mock.test",
            "--attribute",
            "漫画",
            "1",
            "2",
        ],
    );
    assert!(
        added.status.success(),
        "virtual add failed: {}{}",
        stdout(&added),
        stderr(&added)
    );
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("漫画"),
        "the requested classification must be stored"
    );
}

#[test]
fn a_single_kind_virtual_novel_rejects_a_contradicting_attribute() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("小説"))]);

    let added = run(
        root,
        &[
            "virtual",
            "add",
            "--title",
            "XXX",
            "--author",
            "Author",
            "--site",
            "mock.test",
            "--attribute",
            "漫画",
            "1",
            "2",
        ],
    );
    assert!(
        !added.status.success(),
        "a contradicting attribute must be rejected"
    );
    assert!(
        stderr(&added).contains("一致"),
        "unexpected error: {}",
        stderr(&added)
    );
}

#[test]
fn a_single_kind_virtual_novel_cannot_change_its_attribute() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("小説"))]);

    let added = run(
        root,
        &[
            "virtual", "add", "--title", "XXX", "--author", "Author", "--site", "mock.test", "1",
            "2",
        ],
    );
    assert!(added.status.success());

    let changed = run(root, &["virtual", "attribute", "vs_3", "漫画"]);
    assert!(
        !changed.status.success(),
        "a single-kind virtual novel must not change its attribute"
    );
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("小説"),
        "the stored classification must not change"
    );
}

#[test]
fn an_invalid_attribute_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("漫画"))]);

    let added = run(
        root,
        &[
            "virtual",
            "add",
            "--title",
            "XXX",
            "--author",
            "Author",
            "--site",
            "mock.test",
            "--attribute",
            "映画",
            "1",
            "2",
        ],
    );
    assert!(!added.status.success(), "an invalid attribute must be rejected");
    assert!(
        stderr(&added).contains("属性タグ"),
        "unexpected error: {}",
        stderr(&added)
    );

    let changed = run(root, &["virtual", "attribute", "vs_3", "映画"]);
    assert!(
        !changed.status.success(),
        "an invalid attribute must be rejected"
    );
}

#[test]
fn appending_a_manga_member_keeps_the_existing_classification() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(
        root,
        &[
            (1, "A その1", Some("小説")),
            (2, "A その2", Some("漫画")),
            (3, "A その3", Some("小説")),
        ],
    );

    let added = run(
        root,
        &[
            "virtual", "add", "--title", "XXX", "--author", "Author", "--site", "mock.test", "1",
            "2",
        ],
    );
    assert!(added.status.success());
    // メンバーを 2 件しか使っていないので、仮想小説は次の ID (4) になる。
    assert_eq!(stored_attribute(root, 4).as_deref(), Some("小説"));

    // 小説のメンバーを足しても混在したままなので、既存の分類を保持する。
    let appended = run(root, &["virtual", "append", "vs_4", "3"]);
    assert!(
        appended.status.success(),
        "virtual append failed: {}{}",
        stdout(&appended),
        stderr(&appended)
    );
    assert_eq!(
        stored_attribute(root, 4).as_deref(),
        Some("小説"),
        "a mixed virtual novel must keep its classification after an append"
    );
}

#[test]
fn a_stored_classification_is_corrected_when_the_members_become_single_kind() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // 混在した仮想小説が 保存済みで 「漫画」 になっている状態を作る。
    write_fixture(root, &[(1, "A その1", Some("小説")), (2, "A その2", Some("漫画"))]);
    let added = run(
        root,
        &[
            "virtual", "add", "--title", "XXX", "--author", "Author", "--site", "mock.test", "1",
            "2",
        ],
    );
    assert!(added.status.success());
    let changed = run(root, &["virtual", "attribute", "vs_3", "漫画"]);
    assert!(changed.status.success());
    assert_eq!(stored_attribute(root, 3).as_deref(), Some("漫画"));

    // メンバーの属性を後から小説へ変えると、単一種別なので自動分類へ直す。
    let database_path = root.join(".narou/database.yaml");
    let mut database: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&database_path).unwrap()).unwrap();
    {
        let mapping = database.as_mapping_mut().unwrap();
        let member = mapping
            .get_mut(serde_yaml::Value::Number(2.into()))
            .and_then(serde_yaml::Value::as_mapping_mut)
            .unwrap();
        member.insert(
            serde_yaml::Value::String("attribute_tag".into()),
            serde_yaml::Value::String("小説".into()),
        );
    }
    std::fs::write(&database_path, serde_yaml::to_string(&database).unwrap()).unwrap();

    // 起動時の整合で単一種別の値は自動分類へ直る。
    let listed = run(root, &["list"]);
    assert!(listed.status.success());
    assert_eq!(
        stored_attribute(root, 3).as_deref(),
        Some("小説"),
        "a single-kind virtual novel must be auto-corrected"
    );
}

#[test]
fn sqlite_keeps_a_mixed_selection_separate_from_tags_across_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root, &[(1, "Text", Some("小説")), (2, "Pictures", Some("漫画"))]);
    std::fs::write(root.join(".narou/storage-backend"), "sqlite").unwrap();
    let output = run(root, &["virtual", "add", "--title", "Mixed", "--attribute", "漫画", "1", "2"]);
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    let output = run(root, &["virtual", "attribute", "vs_3", "小説"]);
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    assert!(run(root, &["virtual", "list"]).status.success());
    let db = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
    let (attribute, tags): (String, String) = db.query_row(
        "SELECT attribute_tag,tags_json FROM novels WHERE id=3", [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(attribute, "小説");
    let tags: Vec<String> = serde_json::from_str(&tags).unwrap();
    assert_eq!(tags, vec!["vs_3", "共通"]);
}
