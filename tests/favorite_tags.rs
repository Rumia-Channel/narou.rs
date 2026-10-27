//! お気に入りタグの保存・取得の往復。
//!
//! 保存先は在庫の `favorite_tags` キー (Native は `.narou/favorite_tags.yaml` /
//! SQLite `app_state`、Worker は D1)。このテストは native の在庫経由で
//! 登録順の保持と重複の拒否を固定する。

use std::collections::BTreeSet;

use narou_rs::application::favorite_tags::FavoriteTags;

#[test]
fn favorites_keep_the_registration_order_and_drop_duplicates() {
    let mut favorites = FavoriteTags::default();
    assert!(favorites.add("小説"));
    assert!(favorites.add("ゲーム"));
    assert!(!favorites.add("小説"));
    assert_eq!(favorites.tags, vec!["小説".to_string(), "ゲーム".to_string()]);
}

#[test]
fn favorites_are_stored_under_the_inventory_key() {
    let temp = tempfile::tempdir().unwrap();
    let inventory = narou_rs::db::inventory::Inventory::new(temp.path().to_path_buf());

    let mut favorites = FavoriteTags::default();
    favorites.add("小説");
    favorites.add("ゲーム");
    inventory
        .save(
            "favorite_tags",
            narou_rs::db::inventory::InventoryScope::Local,
            &favorites,
        )
        .unwrap();

    let raw = inventory
        .load_raw(
            "favorite_tags",
            narou_rs::db::inventory::InventoryScope::Local,
        )
        .unwrap();
    assert!(!raw.trim().is_empty(), "favorite_tags が保存されていない");
    let restored: FavoriteTags = serde_yaml::from_str(&raw).unwrap();
    assert_eq!(restored.tags, favorites.tags);
}

#[test]
fn favorites_drop_tags_that_no_longer_exist() {
    let mut favorites = FavoriteTags::default();
    favorites.add("残す");
    favorites.add("消える");
    let existing = BTreeSet::from(["残す".to_string()]);
    assert!(favorites.retain_existing(&existing));
    assert_eq!(favorites.tags, vec!["残す".to_string()]);
}
