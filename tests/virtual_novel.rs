//! End-to-end virtual novel registration and combined conversion.

use std::path::Path;
use std::process::Command;

fn write_fixture(root: &Path) {
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    let database = r#"1:
  id: 1
  title: "A その1"
  author: "Author"
  file_title: "[Author] A その1"
  toc_url: "https://mock.test/novel/a1/"
  sitename: "mock.test"
  novel_type: 1
  end: false
  last_update: "2026-01-02T00:00:00Z"
  general_lastup: "2026-01-01T00:00:00Z"
  general_all_no: 1
  length: 10
  tags: ["共通", "その1"]
2:
  id: 2
  title: "A その2"
  author: "Author"
  file_title: "[Author] A その2"
  toc_url: "https://mock.test/novel/a2/"
  sitename: "mock.test"
  novel_type: 1
  end: false
  last_update: "2026-01-02T00:00:00Z"
  general_lastup: "2026-03-04T00:00:00Z"
  general_all_no: 1
  length: 10
  tags: ["共通", "その2"]
"#;
    std::fs::write(root.join(".narou/database.yaml"), database).unwrap();

    for (number, body) in [(1, "BODY-1 本文"), (2, "BODY-2 本文")] {
        let dir = root
            .join("小説データ")
            .join("mock.test")
            .join(format!("[Author] A その{number}"));
        std::fs::create_dir_all(dir.join("本文")).unwrap();
        let toc = format!(
            "title: \"A その{number}\"\nauthor: \"Author\"\ntoc_url: \"https://mock.test/novel/a{number}/\"\nsubtitles:\n- index: '1'\n  href: /novel/a{number}/1/\n  chapter: ''\n  subchapter: ''\n  subtitle: 第{number}話\n  file_subtitle: 第{number}話\n  subdate: '2026-01-01 00:00:00'\n  download_time: '2026-01-01 00:00:00 +0900'\nnovel_type: 1\n"
        );
        std::fs::write(dir.join("toc.yaml"), toc).unwrap();
        let section = format!(
            "index: '1'\nhref: /novel/a{number}/1/\nchapter: ''\nsubchapter: ''\nsubtitle: 第{number}話\nfile_subtitle: 第{number}話\nsubdate: '2026-01-01 00:00:00'\ndownload_time: '2026-01-01 00:00:00 +0900'\nelement:\n  data_type: text\n  introduction: ''\n  postscript: ''\n  body: '{body}'\n"
        );
        std::fs::write(dir.join(format!("本文/1 第{number}話.yaml")), section).unwrap();
    }
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("run narou_rs")
}

#[test]
fn virtual_novel_registers_hides_members_and_combines_text_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_fixture(root);

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
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(String::from_utf8_lossy(&added.stdout).contains("vs_3"));

    // Member tags are merged (deduplicated) and the newest member update date
    // becomes the virtual novel's latest-update date.
    let database: serde_yaml::Value = serde_yaml::from_str(
        &std::fs::read_to_string(root.join(".narou/database.yaml")).unwrap(),
    )
    .unwrap();
    let virtual_record = database
        .as_mapping()
        .and_then(|mapping| {
            mapping
                .iter()
                .find(|(key, _)| key.as_i64() == Some(3))
                .map(|(_, value)| value)
        })
        .expect("virtual record 3");
    let tags: Vec<&str> = virtual_record["tags"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|tag| tag.as_str())
        .collect();
    assert_eq!(tags.iter().filter(|tag| **tag == "共通").count(), 1);
    assert!(tags.contains(&"vs_3"));
    assert!(tags.contains(&"その1") && tags.contains(&"その2"));
    assert!(
        virtual_record["general_lastup"]
            .as_str()
            .is_some_and(|date| date.contains("2026-03-04")),
        "virtual general_lastup must be the newest member date: {:?}",
        virtual_record["general_lastup"]
    );

    // Member rows are hidden from the regular list while they belong to the
    // virtual novel; the virtual row itself is a normal row.
    let listed = run(root, &["list"]);
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.lines().any(|line| line.trim() == "3"),
        "virtual row must be listed: {listed}"
    );
    assert!(
        !listed.lines().any(|line| line.trim() == "1" || line.trim() == "2"),
        "member rows must be hidden: {listed}"
    );

    // The virtual novel is hideable through the shared hidden-tag mechanism.
    let hidden = run(root, &["hidden", "add", "--tag", "vs_3"]);
    assert!(hidden.status.success());
    let listed = String::from_utf8_lossy(&run(root, &["list"]).stdout).into_owned();
    assert!(!listed.lines().any(|line| line.trim() == "3"), "hidden virtual row leaked: {listed}");
    let shown = String::from_utf8_lossy(&run(root, &["list", "--show-hidden"]).stdout).into_owned();
    assert!(shown.lines().any(|line| line.trim() == "3"), "--show-hidden must show it: {shown}");

    let converted = run(root, &["convert", "vs_3", "--no-epub"]);
    assert!(
        converted.status.success(),
        "convert failed: {}{}",
        String::from_utf8_lossy(&converted.stdout),
        String::from_utf8_lossy(&converted.stderr)
    );
    let output = root
        .join("小説データ/mock.test/XXX/[Author] XXX.txt");
    let text = std::fs::read_to_string(&output)
        .unwrap_or_else(|error| panic!("missing {}: {error}", output.display()));
    let first = text.find("ＢＯＤＹ－一").expect("first body");
    let second = text.find("ＢＯＤＹ－二").expect("second body");
    assert!(first < second, "members must stay in registration order");
    assert!(text.contains("Ａ その１") && text.contains("Ａ その２"));
}
