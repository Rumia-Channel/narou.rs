//! お気に入りタグ。
//!
//! 自動付与されるタグは数百個まで増えるため、「タグの編集」画面から
//! 検索しないとよく使うタグを選べない。お気に入りタグを登録し、その一覧を
//! タグ編集画面の先頭へ表示する。
//!
//! 保存は在庫 (inventory) の `favorite_tags` キー: Native は
//! `.narou/favorite_tags.yaml` / SQLite `app_state('inv','favorite_tags')`、
//! Worker は D1 の `app_state('inv','favorite_tags')`。
//!
//! 登録順は保持する (利用者が登録した順序をそのまま見せる)。タグ名は Web UI
//! のタグ名検証と同じ規則 (`application::webui::validate_web_tag_name`) を
//! 通す。タグ色とは独立した設定で、片方の色分けは他方に影響しない。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// お気に入りタグの集合。順序は登録順。
///
/// 保存形式は `tags:` を持つマッピング (native の YAML / Worker の JSON)。
/// 読み込みは配列形式も受け付ける (旧データ互換)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteTags {
    #[serde(default)]
    pub tags: Vec<String>,
}

impl FavoriteTags {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.tags.iter().any(|value| value == tag)
    }

    /// タグを登録する。すでにあれば位置を保つ (先頭には移動しない)。
    pub fn add(&mut self, tag: &str) -> bool {
        let tag = tag.trim();
        if tag.is_empty() || self.contains(tag) {
            return false;
        }
        self.tags.push(tag.to_string());
        true
    }

    /// タグの登録を外す。
    pub fn remove(&mut self, tag: &str) -> bool {
        let before = self.tags.len();
        self.tags.retain(|value| value != tag);
        before != self.tags.len()
    }

    /// 全解除。
    pub fn clear(&mut self) -> bool {
        let changed = !self.tags.is_empty();
        self.tags.clear();
        changed
    }

    /// 存在しなくなったタグをお気に入りから外す。
    ///
    /// automatic タグが消えた (作品削除・タグ整理) 場合に、お気に入りに
    /// 残ったタグを一覧に出さないため。
    pub fn retain_existing(&mut self, existing: &BTreeSet<String>) -> bool {
        let before = self.tags.len();
        self.tags.retain(|tag| existing.contains(tag));
        before != self.tags.len()
    }

    /// 保存された YAML 値 (マッピング形式) を読む。
    ///
    /// 旧形式 (`tags:` を持つマッピング) も新しい形式 (配列) も読む。
    pub fn from_yaml_value(value: &serde_yaml::Value) -> Self {
        let mut favorites = Self::default();
        match value {
            serde_yaml::Value::Sequence(items) => {
                for item in items {
                    if let Some(tag) = item.as_str() {
                        favorites.add(tag);
                    }
                }
            }
            serde_yaml::Value::Mapping(mapping) => {
                if let Some(items) = mapping.get(serde_yaml::Value::String("tags".to_string()))
                    && let Some(items) = items.as_sequence()
                {
                    for item in items {
                        if let Some(tag) = item.as_str() {
                            favorites.add(tag);
                        }
                    }
                }
            }
            _ => {}
        }
        favorites
    }

    /// 保存用の YAML 値 (`tags:` を持つマッピング)。
    pub fn to_yaml_value(&self) -> serde_yaml::Value {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            serde_yaml::Value::String("tags".to_string()),
            serde_yaml::Value::Sequence(
                self.tags
                    .iter()
                    .map(|tag| serde_yaml::Value::String(tag.clone()))
                    .collect(),
            ),
        );
        serde_yaml::Value::Mapping(mapping)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_and_does_not_duplicate() {
        let mut favorites = FavoriteTags::default();
        assert!(favorites.add("小説"));
        assert!(favorites.add("ゲーム"));
        assert!(!favorites.add("小説"));
        assert_eq!(favorites.tags, vec!["小説".to_string(), "ゲーム".to_string()]);
        assert!(favorites.contains("ゲーム"));
        assert!(!favorites.contains("漫画"));
    }

    #[test]
    fn remove_and_clear_report_whether_they_changed() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        assert!(favorites.remove("A"));
        assert!(!favorites.remove("A"));
        assert_eq!(favorites.tags, vec!["B".to_string()]);
        assert!(favorites.clear());
        assert!(!favorites.clear());
        assert!(favorites.is_empty());
    }

    #[test]
    fn drops_tags_that_no_longer_exist() {
        let mut favorites = FavoriteTags::default();
        favorites.add("残す");
        favorites.add("消える");
        let existing = BTreeSet::from(["残す".to_string()]);
        assert!(favorites.retain_existing(&existing));
        assert_eq!(favorites.tags, vec!["残す".to_string()]);
    }

    #[test]
    fn round_trips_through_yaml() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        let payload = favorites.to_yaml_value();
        assert_eq!(FavoriteTags::from_yaml_value(&payload), favorites);
    }

    #[test]
    fn reads_the_legacy_mapping_shape() {
        let value: serde_yaml::Value =
            serde_yaml::from_str("tags:\n  - A\n  - B\n").unwrap();
        let favorites = FavoriteTags::from_yaml_value(&value);
        assert_eq!(favorites.tags, vec!["A".to_string(), "B".to_string()]);
    }
}
