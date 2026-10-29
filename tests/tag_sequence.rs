//! タグの登録順 (どのタグが先にライブラリへ入ったか) の保存。
//!
//! タグ一覧の「登録順」並び替えは、タグがライブラリへ入った順を保つ。
//! 小説 ID 順では再現できない (古い小説へ後からタグを付けても ID 順は
//! 変わらない) ので、在庫の `tag_sequence` キーに連番表を保存する。
//!
//! 既存ライブラリには連番が無いので、最初の書き込みでバックフィルする
//! (最小小説 ID → タグ名の順)。以後の新しいタグは登録順の末尾に足す。

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{TimeZone, Utc};
use narou_rs::application::tag_sequence::{library_tags, registered_ranks, TagSequence};
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::db::{Database, NovelRecord};

fn record(id: i64, tags: &[&str]) -> NovelRecord {
    NovelRecord {
        id,
        author: "作者".to_string(),
        title: format!("title {id}"),
        file_title: format!("title {id}"),
        toc_url: format!("https://example.com/{id}/"),
        sitename: "Example".to_string(),
        novel_type: 1,
        end: false,
        last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
        new_arrivals_date: None,
        use_subdirectory: false,
        general_firstup: None,
        novelupdated_at: None,
        general_lastup: None,
        last_mail_date: None,
        tags: tags.iter().map(|tag| tag.to_string()).collect(),
        ncode: None,
        domain: None,
        general_all_no: None,
        length: None,
        suspend: false,
        is_narou: true,
        last_check_date: None,
        convert_failure: false,
        requires_login: false,
        login_session: None,
        extra_fields: Default::default(),
    }
}

/// 小説を書き込む (毎回 `Database` を作り直す = 再起動と同じ状態)。
fn write(root: &Path, records: Vec<NovelRecord>) {
    let mut db = Database::with_root(root.to_path_buf()).unwrap();
    db.update_records(|mut current| {
        for record in records {
            current.insert(record.id, record);
        }
        Ok((current, ()))
    })
    .unwrap();
}

/// 在庫からタグの登録順を読む。
fn sequence(root: &Path) -> TagSequence {
    let inventory = Inventory::new(root.to_path_buf());
    let raw = inventory
        .load_raw("tag_sequence", InventoryScope::Local)
        .unwrap();
    if raw.trim().is_empty() {
        return TagSequence::default();
    }
    serde_yaml::from_str(&raw).unwrap()
}

/// 登録順は「タグがライブラリへ入った順」で、小説 ID の順ではない。
#[test]
fn tags_keep_the_order_they_entered_the_library() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    // 新しい小説 (ID 5) へ先にタグを付ける。
    write(root, vec![record(5, &["ab"])]);
    // 続けて古い小説 (ID 1) へ別のタグを付ける。
    write(root, vec![record(1, &["mm"])]);

    let sequence = sequence(root);
    assert_eq!(
        sequence.order(),
        ["ab".to_string(), "mm".to_string()],
        "登録順は小説 ID 順ではなく、タグがライブラリへ入った順"
    );

    // 再起動後も同じ順序が読める。
    let db = Database::with_root(root.to_path_buf()).unwrap();
    let records: Vec<NovelRecord> = (1..=5)
        .filter_map(|id| db.get(id).cloned())
        .collect();
    let library = library_tags(
        records
            .iter()
            .map(|record| (record.id, record.tags.as_slice())),
        |_| true,
    );
    let ranks = registered_ranks(&sequence, &library);
    assert_eq!(ranks.get("ab"), Some(&0));
    assert_eq!(ranks.get("mm"), Some(&1));
}

/// 既存ライブラリ (登録順が無い) は、最初の書き込みでバックフィルされる。
#[test]
fn existing_libraries_are_backfilled_in_a_stable_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let narou_dir = root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    // 登録順の無いライブラリを直接作る (旧バージョンのライブラリ相当)。
    let mut existing: BTreeMap<i64, NovelRecord> = BTreeMap::new();
    existing.insert(1, record(1, &["b"]));
    existing.insert(2, record(2, &["a", "c"]));
    std::fs::write(
        narou_dir.join("database.yaml"),
        serde_yaml::to_string(&existing).unwrap(),
    )
    .unwrap();

    // 最初の書き込みで既存タグが登録され、新しいタグはその後ろに足される。
    write(root, vec![record(3, &["d"])]);

    let sequence = sequence(root);
    assert_eq!(
        sequence.order(),
        vec![
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
            "d".to_string()
        ],
        "既存タグは最小小説 ID → タグ名の順、新しいタグは後ろ"
    );
}

/// タグを付け替えても登録順は変わらない (最初に登録された順を保つ)。
#[test]
fn retagging_keeps_the_registration_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    write(root, vec![record(1, &["first"])]);
    write(root, vec![record(2, &["second"])]);
    // 1 番の小説のタグを別のものへ変え、3 番の小説へ新しいタグを付ける。
    write(root, vec![record(1, &["changed"]), record(3, &["third"])]);

    let sequence = sequence(root);
    assert_eq!(
        sequence.order(),
        [
            "first".to_string(),
            "second".to_string(),
            "changed".to_string(),
            "third".to_string()
        ]
    );
}

/// 連番表の保存に失敗したら、作品データだけを確定させてはならない。
///
/// 連番表の更新は作品と同じ直列化境界の中で行う。別々にすると、連番表の
/// 書き込みに失敗したときに作品だけが残ってしまう。
#[test]
fn a_failed_sequence_write_does_not_commit_the_records() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let narou_dir = root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    // 連番表のファイルを壊して、連番表の読み書きが失敗する状態にする。
    std::fs::create_dir(narou_dir.join("tag_sequence.yaml")).unwrap();

    let mut db = Database::with_root(root.to_path_buf()).unwrap();
    let error = db
        .update_records(|mut current| {
            current.insert(1, record(1, &["written"]));
            Ok((current, ()))
        })
        .expect_err("連番表の保存に失敗したときは書き込みも失敗する");
    assert!(
        !error.to_string().is_empty(),
        "失敗の理由が分かること"
    );

    // 作品データは確定していない。
    let raw = std::fs::read_to_string(narou_dir.join("database.yaml")).unwrap_or_default();
    assert!(
        !raw.contains("written"),
        "作品だけが確定してよいのは連番表の更新と同じ境界の中だけ"
    );
}

/// `Database::save()` の直接経路でも、連番表の保存に失敗したら作品を残さない。
///
/// `save()` は `update_records()` を通らない別経路なので、両方 individually
/// 確認する。
#[test]
fn a_failed_sequence_write_does_not_commit_the_records_on_save() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let narou_dir = root.join(".narou");
    std::fs::create_dir_all(&narou_dir).unwrap();
    // 連番表のファイルを壊して、連番表の読み書きが失敗する状態にする。
    std::fs::create_dir(narou_dir.join("tag_sequence.yaml")).unwrap();

    let mut db = Database::with_root(root.to_path_buf()).unwrap();
    db.insert(record(1, &["written"]));
    db.save()
        .expect_err("連番表の保存に失敗したときは save() も失敗する");

    // 作品データは確定していない。
    let raw = std::fs::read_to_string(narou_dir.join("database.yaml")).unwrap_or_default();
    assert!(
        !raw.contains("written"),
        "作品だけが確定してよいのは連番表の更新と同じ境界の中だけ"
    );
}
